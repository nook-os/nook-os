//! The loops master switch (MAIN-239).
//!
//! `job_dispatch`, `job_reaper` and `workspace_reaper` used to spawn and poll
//! unconditionally, so a fresh boot burned cycles standing by even with nothing
//! to loop — and there was no way to say "loops off". This is that switch.
//!
//! **Default OFF.** An absent setting means disabled, so a fresh deployment is
//! quiet until someone asks for loops. That is the safe direction: the failure
//! of "off by default" is a job that waits until you notice, and the failure of
//! "on by default" is a fleet of agents doing work nobody asked for.
//!
//! Stored in the existing `settings` table (tenant scope, key `loops.enabled`),
//! which means no migration and — the part that matters for AC-1/AC-2 — it is
//! **read at runtime, on every poll**. Flipping it takes effect within one poll
//! interval with no restart, because nothing caches it across ticks.
//!
//! Per tenant, with a deployment-wide short-circuit. The consumers are
//! cross-tenant (the queue spans them), so each one asks two questions:
//! [`any_enabled`] — is there any point doing this pass at all? — and then
//! [`enabled`] for the specific tenant whose work it is about to touch. With
//! every tenant off, the first question is a single indexed lookup and the pass
//! ends there, which is what makes "off" genuinely quiet rather than merely
//! ineffective.

use nook_types::TenantId;

use crate::error::ApiResult;

/// The settings key. Tenant-scoped rows only — a `user`-scoped row of the same
/// name is somebody's personal preference and must never gate the fleet.
pub const KEY: &str = "loops.enabled";

/// Is the loop machinery enabled for this tenant? Absent → `false`.
///
/// Fails **closed**: a database error reads as disabled rather than surfacing,
/// because the alternative is a transient blip turning the fleet on when the
/// operator has said off. A caller that needs to distinguish the two should
/// query the setting directly.
pub async fn enabled(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: TenantId,
) -> bool {
    let raw = settings.tenant_value(tenant, KEY).await.unwrap_or(None);
    truthy(raw.as_ref())
}

/// Is ANY tenant running loops? The cheap gate a cross-tenant consumer asks
/// before doing a pass at all.
pub async fn any_enabled(settings: &dyn crate::repo::admin::SettingRepository) -> bool {
    settings
        .tenant_values_everywhere(KEY)
        .await
        .unwrap_or_default()
        .iter()
        .any(|v| truthy(Some(v)))
}

/// Every tenant with loops ON — the second question a cross-tenant consumer
/// asks, when what it does next must be scoped to the tenants it is about to
/// touch rather than merely gated on somebody being on.
///
/// Fails **closed**, like [`enabled`]: a database error yields no tenants.
pub async fn enabled_tenants(
    settings: &dyn crate::repo::admin::SettingRepository,
) -> Vec<TenantId> {
    settings
        .tenants_with_value(KEY)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, v)| truthy(Some(v)))
        .map(|(tenant, _)| tenant)
        .collect()
}

/// Turn loops on or off for a tenant. Returns the value now stored.
pub async fn set(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: TenantId,
    on: bool,
) -> ApiResult<bool> {
    // The generic settings upsert already keys on `(tenant, scope, user, key)`,
    // which is exactly this write — no reason for a second one.
    settings
        .put(crate::repo::admin::SettingWrite {
            tenant,
            scope: "tenant".to_string(),
            user: None,
            key: KEY.to_string(),
            value: serde_json::Value::Bool(on),
        })
        .await?;
    Ok(on)
}

/// A stored value that means "on".
///
/// `pub(crate)` so the review sweep (MAIN-408) reuses this exact definition
/// rather than growing a second one — two switches disagreeing about whether
/// `"true"` counts as on is the kind of drift a shared helper prevents.
///
/// The setting is written as a JSON boolean by both the CLI and the UI, but the
/// settings endpoint takes arbitrary JSON, so a hand-`PUT` string is entirely
/// possible. Accepting `true`/`"true"`/`1` costs nothing; anything else — and
/// crucially anything absent — is off.
pub(crate) fn truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

/// Log the switch only when it CHANGES, so a 2-second poll does not fill the
/// log with "loops disabled" forever. Each consumer owns one of these.
///
/// `None` is the "never reported" start state, which is why the first tick
/// always logs: an operator reading the log needs to see the state they booted
/// into, not infer it from silence.
#[derive(Default)]
pub struct SwitchLog {
    last: Option<bool>,
}

impl SwitchLog {
    /// Note the current state, returning it. Logs on the first observation and
    /// on every flip.
    pub fn observe(&mut self, consumer: &'static str, on: bool) -> bool {
        if self.last != Some(on) {
            if on {
                tracing::info!(consumer, "loops enabled — resuming");
            } else {
                tracing::info!(
                    consumer,
                    "loops disabled — idle. Enable with `nook operator loops on` \
                     or Settings → Loops."
                );
            }
            self.last = Some(on);
        }
        on
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_is_off_and_that_is_the_whole_point() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!(null))));
    }

    #[test]
    fn accepts_the_shapes_a_hand_written_put_might_carry() {
        assert!(truthy(Some(&json!(true))));
        assert!(truthy(Some(&json!("true"))));
        assert!(truthy(Some(&json!("TRUE"))));
        assert!(truthy(Some(&json!(1))));

        assert!(!truthy(Some(&json!(false))));
        assert!(!truthy(Some(&json!("false"))));
        assert!(!truthy(Some(&json!(0))));
        assert!(!truthy(Some(&json!("yes"))), "only an explicit true counts");
    }

    #[test]
    fn the_switch_log_speaks_on_change_and_stays_quiet_otherwise() {
        let mut log = SwitchLog::default();
        // First observation always reports (the operator needs the boot state).
        assert!(!log.observe("t", false));
        assert_eq!(log.last, Some(false));
        // Repeats do not.
        log.observe("t", false);
        assert_eq!(log.last, Some(false));
        // A flip does.
        assert!(log.observe("t", true));
        assert_eq!(log.last, Some(true));
    }
}
