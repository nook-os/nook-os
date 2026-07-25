//! The send guards, as a provider-agnostic decorator.
//!
//! Turning on a real transport must NOT, by itself, start delivering: prod's
//! Postmark plan is 100 emails/month, and verification, invites, and email
//! notifications would all fire. So whatever transport `from_config` builds is
//! wrapped in this [`GuardedMailer`], which enforces three gates before a
//! message reaches the wire (MAIN-52):
//!
//! 1. **Global enable** (`MAIL_SEND_ENABLED`, default off) — off ⇒ everything is
//!    captured (logged "would send"), regardless of provider.
//! 2. **Category** — `notification` mail also needs `MAIL_NOTIFICATIONS_ENABLED`;
//!    `transactional` mail (verification, invites) does not.
//! 3. **Quota** — no more than `MAIL_MAX_PER_MONTH` (and optionally
//!    `MAIL_MAX_PER_DAY`) REAL sends per window. The count is read from the
//!    `mail_sends` table, so it survives restarts and deploys.
//!
//! Because it is itself a [`Mailer`], every call site and the notification
//! channel keep the same `&dyn Mailer` they always had; the gates apply
//! uniformly to capture, smtp, and postmark. With the shipped defaults (sending
//! off) there is no behaviour change — prod stays on capture.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::PgPool;

use super::{Category, Mailer};

pub struct GuardedMailer {
    inner: Arc<dyn Mailer>,
    db: PgPool,
    send_enabled: bool,
    notifications_enabled: bool,
    max_per_month: Option<i64>,
    max_per_day: Option<i64>,
}

impl GuardedMailer {
    pub fn new(inner: Arc<dyn Mailer>, db: PgPool, cfg: &crate::config::Config) -> Self {
        Self {
            inner,
            db,
            send_enabled: cfg.mail_send_enabled,
            notifications_enabled: cfg.mail_notifications_enabled,
            max_per_month: cfg.mail_max_per_month,
            max_per_day: cfg.mail_max_per_day,
        }
    }

    /// Are we at or over any configured cap for the current window? Reads the
    /// persistent `mail_sends` count, so a restart does not reset it (AC-4).
    async fn at_quota(&self) -> bool {
        if let Some(cap) = self.max_per_month {
            if self.count_since("month").await >= cap {
                return true;
            }
        }
        if let Some(cap) = self.max_per_day {
            if self.count_since("day").await >= cap {
                return true;
            }
        }
        false
    }

    /// Recorded real sends since the start of the given `date_trunc` unit
    /// ("month" or "day"). A query failure counts as 0 — the guard must never
    /// fail a request path over its own bookkeeping.
    async fn count_since(&self, unit: &str) -> i64 {
        sqlx::query_as::<_, (i64,)>(
            "SELECT count(*) FROM mail_sends WHERE sent_at >= date_trunc($1, now())",
        )
        .bind(unit)
        .fetch_one(&self.db)
        .await
        .map(|(n,)| n)
        .unwrap_or(0)
    }

    /// Record a real send so it counts toward the quota and can be audited
    /// (AC-5). Best-effort: a failed insert is logged, not surfaced — the mail
    /// already went, and losing a count row is safer than erroring the caller.
    async fn record_sent(&self, to: &str, category: Category) {
        let res = sqlx::query(
            "INSERT INTO mail_sends (id, category, recipient_domain) VALUES ($1, $2, $3)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(category.as_str())
        .bind(recipient_domain(to))
        .execute(&self.db)
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to record a mail.sent row — quota count may drift");
        }
    }
}

/// The domain of a recipient address, for the audit row — so volume is
/// auditable without storing full addresses (AC-5). Handles both `a@b.com` and
/// `Name <a@b.com>`; anything without an `@` becomes `unknown`.
pub fn recipient_domain(to: &str) -> String {
    match to.rsplit_once('@') {
        Some((_, dom)) => {
            let d = dom.trim_end_matches('>').trim();
            if d.is_empty() {
                "unknown".into()
            } else {
                d.to_string()
            }
        }
        None => "unknown".into(),
    }
}

#[async_trait]
impl Mailer for GuardedMailer {
    async fn send(
        &self,
        to: &str,
        subject: &str,
        text_body: &str,
        html_body: Option<&str>,
        category: Category,
    ) -> Result<()> {
        // 1. Global enable. Off ⇒ capture, whatever the provider is (AC-2).
        if !self.send_enabled {
            tracing::info!(
                to,
                subject,
                category = category.as_str(),
                "mail would-send (capture): sending is disabled (MAIL_SEND_ENABLED off)"
            );
            return Ok(());
        }
        // 2. Category gate: notifications need their own flag (AC-3).
        if category == Category::Notification && !self.notifications_enabled {
            tracing::info!(
                to,
                subject,
                "mail would-send (capture): notifications disabled (MAIL_NOTIFICATIONS_ENABLED off)"
            );
            return Ok(());
        }
        // 3. Quota backstop (AC-4).
        if self.at_quota().await {
            tracing::warn!(
                to,
                subject,
                category = category.as_str(),
                "mail quota-blocked (capture): monthly/daily cap reached"
            );
            return Ok(());
        }
        // Real send.
        match self
            .inner
            .send(to, subject, text_body, html_body, category)
            .await
        {
            Ok(()) => {
                self.record_sent(to, category).await;
                tracing::info!(to, subject, category = category.as_str(), "mail sent");
                Ok(())
            }
            Err(e) => {
                tracing::error!(to, subject, error = %e, "mail send failed");
                Err(e)
            }
        }
    }

    fn describe(&self) -> String {
        format!(
            "guarded[{}] send_enabled={} notifications={} cap_month={:?} cap_day={:?}",
            self.inner.describe(),
            self.send_enabled,
            self.notifications_enabled,
            self.max_per_month,
            self.max_per_day,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mailer::capture::CaptureMailer;

    #[test]
    fn recipient_domain_reduces_the_address() {
        assert_eq!(recipient_domain("her@example.com"), "example.com");
        assert_eq!(
            recipient_domain("NookOS <no-reply@hein.network>"),
            "hein.network"
        );
        assert_eq!(recipient_domain("no-at-sign"), "unknown");
        assert_eq!(recipient_domain("trailing@"), "unknown");
    }

    async fn pool() -> Option<PgPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(db)
    }

    fn guard(cap: Arc<CaptureMailer>, db: &PgPool, cfg: &Config) -> GuardedMailer {
        GuardedMailer::new(cap, db.clone(), cfg)
    }

    async fn count(db: &PgPool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM mail_sends")
            .fetch_one(db)
            .await
            .unwrap()
            .0
    }

    /// The whole gate matrix in one test, because the quota count is a single
    /// deployment-wide table — running these as separate parallel tests would
    /// let their `mail_sends` rows collide. Steps clean the table where a fresh
    /// count is needed.
    #[tokio::test]
    async fn the_guards_gate_enable_category_and_quota() {
        let Some(db) = pool().await else {
            eprintln!("skipping the_guards_gate_enable_category_and_quota — no DATABASE_URL");
            return;
        };
        sqlx::query("DELETE FROM mail_sends")
            .execute(&db)
            .await
            .unwrap();

        // Using a CaptureMailer as the inner "transport": cap.sent() is exactly
        // the set of REAL sends that passed the guard (captured/gated/blocked
        // sends never reach the inner).
        let mut cfg = Config::for_test();

        // ── Disabled: everything captured, inner never called (AC-2) ──────
        cfg.mail_send_enabled = false;
        let cap = Arc::new(CaptureMailer::new());
        let g = guard(cap.clone(), &db, &cfg);
        g.send("a@x.test", "s", "b", None, Category::Transactional)
            .await
            .unwrap();
        g.send("a@x.test", "s", "b", None, Category::Notification)
            .await
            .unwrap();
        assert_eq!(
            cap.sent().len(),
            0,
            "disabled ⇒ the transport is not invoked"
        );
        assert_eq!(count(&db).await, 0, "disabled ⇒ nothing recorded");

        // ── Enabled, notifications off: transactional sends, notification is
        //    still captured (AC-3) ───────────────────────────────────────────
        cfg.mail_send_enabled = true;
        cfg.mail_notifications_enabled = false;
        let cap = Arc::new(CaptureMailer::new());
        let g = guard(cap.clone(), &db, &cfg);
        g.send("t@x.test", "s", "b", None, Category::Transactional)
            .await
            .unwrap();
        assert_eq!(cap.sent().len(), 1, "transactional sends when enabled");
        g.send("n@x.test", "s", "b", None, Category::Notification)
            .await
            .unwrap();
        assert_eq!(
            cap.sent().len(),
            1,
            "notification stays captured until its own flag is on"
        );
        assert_eq!(count(&db).await, 1, "only the real send was recorded");

        // ── Notifications on: the notification now sends ──────────────────
        cfg.mail_notifications_enabled = true;
        let cap = Arc::new(CaptureMailer::new());
        let g = guard(cap.clone(), &db, &cfg);
        g.send("n@x.test", "s", "b", None, Category::Notification)
            .await
            .unwrap();
        assert_eq!(cap.sent().len(), 1, "notification sends once enabled");

        // ── Quota: cap at 2, the third is blocked; and it PERSISTS across a
        //    fresh guard (a restart) since the count is table-derived (AC-4) ──
        sqlx::query("DELETE FROM mail_sends")
            .execute(&db)
            .await
            .unwrap();
        cfg.mail_max_per_month = Some(2);
        let cap = Arc::new(CaptureMailer::new());
        let g = guard(cap.clone(), &db, &cfg);
        for _ in 0..3 {
            g.send("q@x.test", "s", "b", None, Category::Transactional)
                .await
                .unwrap();
        }
        assert_eq!(cap.sent().len(), 2, "the third send is quota-blocked");
        assert_eq!(count(&db).await, 2, "only two real sends recorded");

        // A brand-new guard over the same DB still sees the cap — no in-memory
        // counter to reset.
        let cap2 = Arc::new(CaptureMailer::new());
        let g2 = guard(cap2.clone(), &db, &cfg);
        g2.send("q@x.test", "s", "b", None, Category::Transactional)
            .await
            .unwrap();
        assert_eq!(cap2.sent().len(), 0, "the cap survives a restart");

        // The recorded rows carry domain + category for audit (AC-5).
        let row: (String, String) = sqlx::query_as(
            "SELECT category, recipient_domain FROM mail_sends ORDER BY sent_at LIMIT 1",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.0, "transactional");
        assert_eq!(row.1, "x.test");

        sqlx::query("DELETE FROM mail_sends")
            .execute(&db)
            .await
            .unwrap();
    }
}
