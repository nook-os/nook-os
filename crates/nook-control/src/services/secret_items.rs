//! Named secret items: what a name may be, what a `.env` body means, and which
//! items a session or a job is handed (MAIN-625).
//!
//! Everything here that decides something is a pure function, because every
//! one of them is an acceptance criterion: AC-7 is "a node item reaches no
//! environment" and AC-8 is "this file parses to these pairs". Both are
//! assertions about a value, so both are unit tests rather than a stack.

use nook_types::{SecretEnv, SecretImportProblem, SecretScope, WorkspaceId};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Variables nook itself sets in a session or a job, which an item may not
/// take.
///
/// Refused at the door rather than dropped at delivery: a secret that was
/// stored, listed, and then silently not injected is the worst of the three
/// outcomes. Every name here is one the machinery needs to be its own — the
/// agent's credential, the git shim, the run's identity. `GH_TOKEN` is on the
/// list because a session's `gh` is machinery too; a repo that needs its own
/// forge identity pins a workspace credential (MAIN-367), which is a different
/// feature and not this one.
pub const RESERVED_NAMES: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "GH_TOKEN",
    "GIT_SSH_COMMAND",
    "HOME",
    "IS_SANDBOX",
    "LANG",
    "LC_ALL",
    "NOOK_BUILD_TASK",
    "NOOK_JOB_ID",
    "NOOK_JOB_SEED",
    "NOOK_PORT",
    "NOOK_PORTS_UNSATISFIED",
    "NOOK_REVIEW_FORCED",
    "NOOK_REVIEW_PR",
    "NOOK_SANDBOX",
    "NOOK_SERVER",
    "NOOK_SESSION_ID",
    "NOOK_TENANT_ID",
    "NOOK_TOKEN",
    "NOOK_WORKSPACE_ID",
    "PATH",
];

/// Is this a name a shell can actually export, and one we are willing to set?
///
/// The shell's own rule (`[A-Za-z_][A-Za-z0-9_]*`) plus [`RESERVED_NAMES`]. A
/// name outside it is refused at write time, so nothing downstream has to cope
/// with a variable it cannot set.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a secret needs a name".into());
    }
    let mut chars = name.chars();
    let first = chars.next().expect("non-empty");
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(format!(
            "'{name}' is not a usable environment variable name — it must start \
             with a letter or underscore"
        ));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "'{name}' is not a usable environment variable name — letters, digits \
             and underscores only"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!(
            "'{name}' is set by nook itself and cannot be a secret"
        ));
    }
    Ok(())
}

/// One item on its way to a session or a job: what it is attached to, and the
/// value already out of the vault.
#[derive(Debug, Clone)]
pub struct Deliverable {
    pub scope: SecretScope,
    pub scope_id: Uuid,
    pub name: String,
    pub value: String,
}

/// The environment a workspace's session or job is handed (AC-7).
///
/// Two rules, and they are the whole of the scope model: a **node** item goes
/// nowhere (NG-7 — it is a credential for the machine, not for what runs on
/// it), and a **workspace** item beats a tenant item of the same name, since
/// the narrower statement is the more deliberate one.
///
/// Sorted by name so two calls with the same items produce byte-identical
/// output — an environment that varies by hash order is a run that is not
/// reproducible.
pub fn env_for(items: &[Deliverable], workspace: Option<WorkspaceId>) -> Vec<SecretEnv> {
    let mut chosen: std::collections::BTreeMap<&str, (&str, bool)> = Default::default();
    for item in items {
        let from_workspace = match item.scope {
            SecretScope::Tenant => false,
            SecretScope::Workspace if Some(WorkspaceId(item.scope_id)) == workspace => true,
            // A workspace item belonging to another workspace, and every node
            // item.
            _ => continue,
        };
        match chosen.get(item.name.as_str()) {
            // Tenant loses to workspace; a duplicate within one scope cannot
            // happen (the unique index), so nothing else can tie.
            Some((_, existing_from_workspace)) if *existing_from_workspace => continue,
            _ => chosen.insert(&item.name, (&item.value, from_workspace)),
        };
    }
    chosen
        .into_iter()
        .map(|(name, (value, _))| SecretEnv {
            name: name.to_string(),
            value: value.to_string(),
        })
        .collect()
}

/// What a session or job in this workspace gets, values and all.
///
/// The list is the whole tenant's and the filtering is [`env_for`]'s, so the
/// scope rules have exactly one definition. That costs decrypting a node item
/// this caller will drop — a handful of AES operations whose plaintext is
/// dropped in the same expression, and a good trade for not spelling the rule
/// twice.
///
/// Never fatal: a value that will not open (a rotated `SECRETS_KEY`, say) is
/// logged and skipped. Refusing to start every session in the tenant because
/// one item is unreadable would turn a secrets problem into an outage.
pub async fn env_for_workspace(
    state: &AppState,
    tenant: nook_types::TenantId,
    workspace: Option<WorkspaceId>,
) -> Vec<SecretEnv> {
    let rows = match state.secret_items.list(tenant).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(%tenant, error = %e, "could not read secret items; delivering none");
            return Vec::new();
        }
    };
    let mut deliverables = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(scope) = SecretScope::parse(&row.scope) else {
            tracing::warn!(name = %row.name, scope = %row.scope, "unknown secret scope; skipped");
            continue;
        };
        match state.vault.open_envelope(&row.dek_wrapped, &row.value_enc) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(value) => deliverables.push(Deliverable {
                    scope,
                    scope_id: row.scope_id,
                    name: row.name,
                    value,
                }),
                // Names only in the log, never the bytes.
                Err(_) => tracing::warn!(name = %row.name, "secret value is not UTF-8; skipped"),
            },
            Err(e) => {
                tracing::warn!(name = %row.name, error = %e, "could not open secret; skipped")
            }
        }
    }
    env_for(&deliverables, workspace)
}

/// Where a `SetSecretItemRequest`'s scope actually points, checked against what
/// exists.
///
/// A tenant item can only ever mean the caller's own tenant, so `scope_id` is
/// optional there and ignored if given. A workspace or node id is required, and
/// it must be one this tenant has — otherwise a secret could be filed against
/// another org's repo, and listed by nobody.
pub async fn resolve_scope_id(
    state: &AppState,
    tenant: nook_types::TenantId,
    scope: SecretScope,
    scope_id: Option<Uuid>,
) -> ApiResult<Uuid> {
    match scope {
        SecretScope::Tenant => Ok(tenant.0),
        SecretScope::Workspace => {
            let id = scope_id.ok_or_else(|| {
                ApiError::BadRequest("a workspace secret needs a workspace id".into())
            })?;
            state
                .workspaces
                .get(tenant, WorkspaceId(id))
                .await?
                .ok_or(ApiError::NotFound)?;
            Ok(id)
        }
        SecretScope::Node => {
            let id = scope_id
                .ok_or_else(|| ApiError::BadRequest("a node secret needs a node id".into()))?;
            state
                .nodes
                .get(tenant, nook_types::NodeId(id))
                .await?
                .ok_or(ApiError::NotFound)?;
            Ok(id)
        }
    }
}

/// What a `.env` body parses to (AC-8): assignments in file order, and the
/// lines that were not one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedEnv {
    pub items: Vec<(String, String)>,
    pub problems: Vec<SecretImportProblem>,
}

/// Read a `.env` body.
///
/// Comments and blank lines are skipped, `export ` is stripped, and single- and
/// double-quoted values are unquoted (only the double-quoted form takes
/// backslash escapes, which is the shell's own rule). An **unquoted** value is
/// taken verbatim after trimming, `#` included: dotenv implementations differ
/// on trailing comments, and truncating a password at a `#` is a far worse
/// failure than importing a comment somebody has to delete.
///
/// A line that is not an assignment becomes a problem, never a silent skip —
/// and the problem carries the line NUMBER, never the line, so a malformed
/// `KEY = value` cannot carry a secret into a log.
pub fn parse_dotenv(body: &str) -> ParsedEnv {
    let mut out = ParsedEnv::default();
    for (index, raw) in body.lines().enumerate() {
        let line_no = index as u32 + 1;
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let assignment = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = assignment.split_once('=') else {
            out.problems.push(SecretImportProblem {
                line: Some(line_no),
                reason: "not an assignment — expected NAME=value".into(),
            });
            continue;
        };
        let name = name.trim();
        if let Err(reason) = check_name(name) {
            out.problems.push(SecretImportProblem {
                line: Some(line_no),
                reason,
            });
            continue;
        }
        match unquote(value.trim()) {
            Ok(value) => out.items.push((name.to_string(), value)),
            Err(reason) => out.problems.push(SecretImportProblem {
                line: Some(line_no),
                reason,
            }),
        }
    }
    out
}

fn unquote(value: &str) -> Result<String, String> {
    let mut chars = value.chars();
    let quote = match chars.next() {
        Some(q @ ('"' | '\'')) => q,
        // Unquoted: verbatim, per the note on `parse_dotenv`.
        _ => return Ok(value.to_string()),
    };
    let body = &value[quote.len_utf8()..];
    let Some(end) = body.strip_suffix(quote) else {
        return Err(format!("unterminated {quote} quote"));
    };
    if quote == '\'' {
        // POSIX single quotes: no escapes at all, which is what makes them the
        // safe way to write a value full of backslashes.
        return Ok(end.to_string());
    }
    let mut out = String::with_capacity(end.len());
    let mut rest = end.chars();
    while let Some(c) = rest.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match rest.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => return Err("a value ends with a dangling backslash".into()),
        }
    }
    Ok(out)
}

/// Seal a value and store it, recording the write as an event that names the
/// item and never its value (AC-10).
pub async fn set_item(
    state: &AppState,
    tenant: nook_types::TenantId,
    actor: nook_types::UserId,
    scope: SecretScope,
    scope_id: Uuid,
    name: &str,
    value: &str,
) -> ApiResult<nook_types::SecretItem> {
    check_name(name).map_err(ApiError::BadRequest)?;
    let envelope = state
        .vault
        .seal_envelope(value.as_bytes())
        .map_err(ApiError::Internal)?;
    let row = state
        .secret_items
        .put(crate::repo::secret_items::NewSecretItem {
            tenant,
            scope,
            scope_id,
            name: name.to_string(),
            value_enc: envelope.ciphertext,
            dek_wrapped: envelope.wrapped_key,
            updated_by: Some(actor),
        })
        .await?;
    record(state, tenant, actor, "secret.set", scope, scope_id, name).await;
    Ok(row.summary())
}

/// The one place a secret write becomes an event, so the payload cannot drift
/// into carrying a value on one path and not another (AC-10).
pub async fn record(
    state: &AppState,
    tenant: nook_types::TenantId,
    actor: nook_types::UserId,
    kind: &'static str,
    scope: SecretScope,
    scope_id: Uuid,
    name: &str,
) {
    let mut draft = crate::events::EventDraft::new(kind)
        .actor("user", actor.0)
        .payload(serde_json::json!({
            "name": name,
            "scope": scope.as_str(),
            "scope_id": scope_id,
        }));
    if scope == SecretScope::Workspace {
        draft = draft.workspace(WorkspaceId(scope_id));
    }
    if scope == SecretScope::Node {
        draft = draft.node(nook_types::NodeId(scope_id));
    }
    crate::events::record(state, tenant, draft).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(scope: SecretScope, scope_id: Uuid, name: &str, value: &str) -> Deliverable {
        Deliverable {
            scope,
            scope_id,
            name: name.into(),
            value: value.into(),
        }
    }

    /// AC-7: tenant and workspace items reach the environment; a node item does
    /// not.
    #[test]
    fn a_node_item_is_absent_from_the_built_environment() {
        let workspace = WorkspaceId(Uuid::now_v7());
        let node = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let env = env_for(
            &[
                item(SecretScope::Tenant, tenant, "FLEET_KEY", "fleet"),
                item(SecretScope::Workspace, workspace.0, "REPO_KEY", "repo"),
                item(SecretScope::Node, node, "NODE_KEY", "node"),
            ],
            Some(workspace),
        );
        let names: Vec<&str> = env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["FLEET_KEY", "REPO_KEY"]);
        assert!(
            !env.iter().any(|e| e.value == "node"),
            "a node item must reach no environment: {env:?}"
        );
    }

    #[test]
    fn another_workspaces_item_is_not_delivered_here() {
        let mine = WorkspaceId(Uuid::now_v7());
        let theirs = Uuid::now_v7();
        let env = env_for(
            &[item(SecretScope::Workspace, theirs, "THEIR_KEY", "x")],
            Some(mine),
        );
        assert!(env.is_empty(), "{env:?}");
    }

    #[test]
    fn a_workspace_item_beats_a_tenant_item_of_the_same_name() {
        let workspace = WorkspaceId(Uuid::now_v7());
        let tenant = Uuid::now_v7();
        // Both orders, because a rule that depends on which row came back first
        // is not a rule.
        for items in [
            vec![
                item(SecretScope::Tenant, tenant, "API_KEY", "fleet"),
                item(SecretScope::Workspace, workspace.0, "API_KEY", "repo"),
            ],
            vec![
                item(SecretScope::Workspace, workspace.0, "API_KEY", "repo"),
                item(SecretScope::Tenant, tenant, "API_KEY", "fleet"),
            ],
        ] {
            let env = env_for(&items, Some(workspace));
            assert_eq!(env.len(), 1);
            assert_eq!(env[0].value, "repo");
        }
    }

    /// A session with no workspace — an ad-hoc terminal — still gets the
    /// tenant's items, and no workspace's.
    #[test]
    fn a_workspaceless_session_gets_the_tenant_items_only() {
        let tenant = Uuid::now_v7();
        let env = env_for(
            &[
                item(SecretScope::Tenant, tenant, "FLEET_KEY", "fleet"),
                item(SecretScope::Workspace, Uuid::now_v7(), "REPO_KEY", "repo"),
            ],
            None,
        );
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "FLEET_KEY");
    }

    #[test]
    fn a_name_nook_sets_itself_is_refused() {
        for name in ["NOOK_TOKEN", "GH_TOKEN", "PATH", "HOME"] {
            assert!(check_name(name).is_err(), "{name} must be reserved");
        }
        // …and the one the card's own end-to-end proof uses is not, which is
        // what stops the reserved list quietly growing to cover ordinary names.
        assert!(check_name("NOOK_E2E_SECRET").is_ok());
    }

    #[test]
    fn a_name_a_shell_cannot_export_is_refused() {
        for name in ["", "1KEY", "MY-KEY", "MY KEY", "MY.KEY", "a=b"] {
            assert!(check_name(name).is_err(), "{name:?} must be refused");
        }
        for name in ["_private", "KEY", "key_2", "A1"] {
            assert!(check_name(name).is_ok(), "{name:?} must be allowed");
        }
    }

    /// AC-8, table-driven over the shapes a real `.env` has.
    #[test]
    fn a_dotenv_body_parses_to_its_assignments_in_order() {
        let body = "\
# a comment
\r
export EXPORTED=one
PLAIN=two
QUOTED=\"three four\"
SINGLE='five #six'
EMPTY=
EQUALS=a=b
SPACED = seven
ESCAPED=\"line\\none\"
HASHED=pa#ssword
TRAILING=eight   
";
        let parsed = parse_dotenv(body);
        assert_eq!(
            parsed.items,
            vec![
                ("EXPORTED".into(), "one".into()),
                ("PLAIN".into(), "two".into()),
                ("QUOTED".into(), "three four".into()),
                ("SINGLE".into(), "five #six".into()),
                ("EMPTY".into(), String::new()),
                // Only the FIRST `=` separates; the rest is the value, which is
                // what a base64 or a connection string needs.
                ("EQUALS".into(), "a=b".into()),
                ("SPACED".into(), "seven".into()),
                ("ESCAPED".into(), "line\none".into()),
                // Verbatim: a `#` in an unquoted value is part of the password.
                ("HASHED".into(), "pa#ssword".into()),
                ("TRAILING".into(), "eight".into()),
            ]
        );
        assert!(parsed.problems.is_empty(), "{:?}", parsed.problems);
    }

    #[test]
    fn a_malformed_line_is_reported_by_number_and_never_by_content() {
        let parsed =
            parse_dotenv("GOOD=1\njust some prose\nBAD-NAME=2\nUNTERMINATED=\"x\nFINE=3\n");
        assert_eq!(
            parsed.items,
            vec![("GOOD".into(), "1".into()), ("FINE".into(), "3".into())]
        );
        assert_eq!(
            parsed.problems.iter().map(|p| p.line).collect::<Vec<_>>(),
            vec![Some(2), Some(3), Some(4)]
        );
        for problem in &parsed.problems {
            assert!(
                !problem.reason.contains("just some prose"),
                "a problem must not quote the line: {problem:?}"
            );
        }
    }

    #[test]
    fn an_empty_body_imports_nothing_and_complains_about_nothing() {
        assert_eq!(parse_dotenv(""), ParsedEnv::default());
        assert_eq!(parse_dotenv("\n\n# only comments\n"), ParsedEnv::default());
    }
}
