//! What a *scoped* credential may do, decided in exactly one place (MAIN-602).
//!
//! The only credential automation had was `nook_user_…`, which is its owner
//! entire: it can move cards, read the private notebook and mint more tokens.
//! Handing that to a CI job so it can write one comment is a grant wildly out of
//! proportion to the job. A scoped token narrows it — to a set of verbs, and
//! optionally to one workspace.
//!
//! **One chokepoint, and this is it.** [`authorize`] is called from the `AuthCtx`
//! extractor (every REST route) and from the `/mcp` door, and nothing else
//! decides what a scoped token may reach. Two copies of a permission rule do not
//! stay equal — one gets a new case, the other keeps refusing it, and the API a
//! caller sees stops being one API. That is also why the decision is a pure
//! function of `(method, path)` plus one lookup: it is small enough to read.
//!
//! **Default deny.** [`required_scope`] answers `None` for every path it does not
//! name, and `None` is a refusal. So the failure mode of forgetting to map a new
//! route is a scoped token that cannot use it — never one that can.

use axum::http::request::Parts;
use axum::http::{Method, Uri};
use nook_types::{TaskId, TokenScope, WorkspaceId};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// A set of [`TokenScope`], as a bitset.
///
/// A bitset rather than a `Vec` so the whole grant stays `Copy`, which is what
/// lets it ride beside an `AuthCtx` without turning 130 construction sites in
/// this tree into a refactor. The set is closed and has four members; it will
/// not outgrow a `u32`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScopeSet(u32);

impl ScopeSet {
    fn bit(scope: TokenScope) -> u32 {
        1 << TokenScope::ALL
            .iter()
            .position(|s| *s == scope)
            .expect("every scope is in ALL")
    }

    pub fn contains(self, scope: TokenScope) -> bool {
        self.0 & Self::bit(scope) != 0
    }

    pub fn insert(&mut self, scope: TokenScope) {
        self.0 |= Self::bit(scope);
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Is every scope in `self` also in `other`? The question AC-3 asks at mint
    /// time: a token may never carry a verb its minter's own credential lacks.
    pub fn subset_of(self, other: ScopeSet) -> bool {
        self.0 & !other.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = TokenScope> {
        TokenScope::ALL
            .into_iter()
            .filter(move |s| self.contains(*s))
    }

    /// Parse the stored column: space-separated wire names. An unrecognised name
    /// is dropped rather than fatal — it cannot have been written by this
    /// version (mint refuses unknown scopes by name), so the only way to see one
    /// is a downgrade, and a scope this build cannot enforce must not be
    /// honoured.
    pub fn parse_stored(stored: &str) -> Self {
        let mut set = ScopeSet::default();
        for name in stored.split_whitespace() {
            if let Some(scope) = TokenScope::parse(name) {
                set.insert(scope);
            }
        }
        set
    }

    /// The storage form: canonical names, in `TokenScope::ALL` order, space
    /// separated — so two equal sets are always the same string.
    pub fn to_stored(self) -> String {
        self.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn to_vec(self) -> Vec<TokenScope> {
        self.iter().collect()
    }
}

impl FromIterator<TokenScope> for ScopeSet {
    fn from_iter<I: IntoIterator<Item = TokenScope>>(iter: I) -> Self {
        let mut set = ScopeSet::default();
        for s in iter {
            set.insert(s);
        }
        set
    }
}

/// How much of its owner a credential carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenGrant {
    /// A browser session, a node credential, or an unscoped `nook_user_` token:
    /// exactly what its holder can do, which is what every credential was before
    /// this ticket (NG-1).
    Full,
    /// A narrowed token. `workspace` absent means tenant-wide.
    Scoped {
        scopes: ScopeSet,
        workspace: Option<WorkspaceId>,
    },
}

impl TokenGrant {
    /// Build from what the credential row stored. `scopes = None` is the
    /// unscoped token; see the migration for why that is NULL and not "".
    pub fn from_stored(scopes: Option<&str>, workspace: Option<Uuid>) -> Self {
        match scopes {
            None => TokenGrant::Full,
            Some(raw) => TokenGrant::Scoped {
                scopes: ScopeSet::parse_stored(raw),
                workspace: workspace.map(WorkspaceId),
            },
        }
    }

    pub fn scopes(self) -> Option<ScopeSet> {
        match self {
            TokenGrant::Full => None,
            TokenGrant::Scoped { scopes, .. } => Some(scopes),
        }
    }

    pub fn workspace(self) -> Option<WorkspaceId> {
        match self {
            TokenGrant::Full => None,
            TokenGrant::Scoped { workspace, .. } => workspace,
        }
    }

    /// Refuse a request this credential does not carry `scope` for — **naming
    /// the scope** (AC-7). A bare 403 sends a caller to read source; a 404 would
    /// send them to look for a bug that is not there.
    pub fn require(self, scope: TokenScope) -> Result<(), ApiError> {
        match self {
            TokenGrant::Full => Ok(()),
            TokenGrant::Scoped { scopes, .. } if scopes.contains(scope) => Ok(()),
            TokenGrant::Scoped { scopes, .. } => Err(ApiError::ForbiddenMsg(format!(
                "this token is missing the '{scope}' scope (it has: {})",
                if scopes.is_empty() {
                    "none".to_string()
                } else {
                    scopes.to_stored()
                }
            ))),
        }
    }

    /// Refuse a request against a workspace this credential is not narrowed to.
    ///
    /// `None` for the target means "this request is not about one workspace",
    /// which a narrowed token may not make: a card with no workspace is outside
    /// what the narrowing can vouch for, and answering it would widen the token
    /// by accident.
    pub fn require_workspace(self, target: Option<WorkspaceId>) -> Result<(), ApiError> {
        let Some(narrowed) = self.workspace() else {
            return Ok(());
        };
        match target {
            Some(w) if w == narrowed => Ok(()),
            Some(_) => Err(ApiError::ForbiddenMsg(format!(
                "this token is narrowed to workspace {narrowed} and that is not where this lives"
            ))),
            None => Err(ApiError::ForbiddenMsg(format!(
                "this token is narrowed to workspace {narrowed}, and this request names no workspace"
            ))),
        }
    }
}

/// The path this request was made against, as the CALLER wrote it.
///
/// `parts.uri` is not it. `Router::nest("/api/v1", …)` and
/// `nest_service("/mcp", …)` strip their prefix before the inner service — and
/// the `AuthCtx` extractor and the MCP middleware both run inside one — so
/// `parts.uri.path()` reads `/tasks/MAIN-1` and, for the MCP door, a bare `/`.
/// A scope table written against those would be matching on a fragment whose
/// meaning depends on where its router happens to be mounted. axum keeps the
/// original in an extension for exactly this; the fallback is the stripped path,
/// which maps to nothing and therefore refuses, because a gate that cannot see
/// what it is gating must not open.
fn request_path(parts: &Parts) -> String {
    parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map(|u| u.0.path().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string())
}

/// The scope a request needs, or `None` when no scoped credential may reach it.
///
/// The scoped surface is deliberately small. Everything a narrowed token can
/// address either names a card in its path — so [`authorize`] can check the
/// narrowing against that card's workspace — or is the MCP door.
///
/// Two absences are decisions rather than gaps. **Creating** a card is
/// board-addressed (`POST /boards/{id}/tasks`), and there is no workspace in
/// that path to check a narrowing against. **Deleting** one is off the surface
/// because `tasks:write` is a working grant — edit, move, label, claim — and a
/// destructive verb that cannot be undone should be asked for by name, not
/// arrive folded into the verb a CI job needed for something else.
pub fn required_scope(method: &Method, path: &str) -> Option<TokenScope> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    // The MCP door is one scope for its whole tool surface: MCP is not a REST
    // shape and its tools do not decompose into `resource:verb`.
    if seg.first() == Some(&"mcp") {
        return Some(TokenScope::Mcp);
    }
    let rest = match seg.as_slice() {
        ["api", "v1", rest @ ..] => rest,
        _ => return None,
    };
    let read = method == Method::GET;
    match rest {
        ["tasks"] if read => Some(TokenScope::TasksRead),
        // `bulk` is a task id's position but is not one — and a bulk edit is a
        // broad tool whose targets are in the body, where the narrowing cannot
        // see them.
        ["tasks", "bulk", ..] => None,
        ["tasks", _] if read => Some(TokenScope::TasksRead),
        // DELETE deliberately absent — see the note above.
        ["tasks", _] if method == Method::PATCH => Some(TokenScope::TasksWrite),
        ["tasks", _, "comments" | "attachments"] if read => Some(TokenScope::TasksRead),
        // A run's report on a card is a comment and the files hung off it. This
        // is the grant the ticket opened on: enough for a CI job to say what it
        // found, and nothing else.
        ["tasks", _, "comments" | "attachments"] => Some(TokenScope::ReportsWrite),
        ["tasks", _, "revisions" | "jobs"] if read => Some(TokenScope::TasksRead),
        ["tasks", _, "claim" | "release" | "archive" | "unarchive" | "move"] => {
            Some(TokenScope::TasksWrite)
        }
        ["tasks", _, "labels", _] => Some(TokenScope::TasksWrite),
        _ => None,
    }
}

/// The task identifier a path names, when it names one.
fn task_ident(path: &str) -> Option<&str> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match seg.as_slice() {
        ["api", "v1", "tasks", ident, ..] if *ident != "bulk" => Some(ident),
        _ => None,
    }
}

/// **The** gate. Every scoped credential passes through here exactly once, from
/// the `AuthCtx` extractor or from the `/mcp` door.
///
/// `parts` is taken mutably because the narrowing is not always a refusal: a
/// listing is *narrowed* rather than denied, by pinning `workspace=` on the
/// query before the handler ever parses it. A caller that named a different
/// workspace is still refused — silently answering about workspace A a question
/// asked about B is worse than either.
pub async fn authorize(
    state: &AppState,
    tenant: nook_types::TenantId,
    grant: TokenGrant,
    parts: &mut Parts,
) -> Result<(), ApiError> {
    let TokenGrant::Scoped { workspace, .. } = grant else {
        return Ok(());
    };
    let path = request_path(parts);
    let Some(need) = required_scope(&parts.method, &path) else {
        return Err(ApiError::ForbiddenMsg(format!(
            "a scoped token cannot reach {path} — mint an unscoped token for this"
        )));
    };
    grant.require(need)?;

    let Some(narrowed) = workspace else {
        return Ok(());
    };

    // The MCP surface is tenant-wide by construction: its caller identity is
    // `McpCaller` (tenant, user, person) and carries no narrowing (AC-6 pins that
    // shape). Rather than let a narrowed token in and quietly serve it the whole
    // tenant, refuse it here and say why.
    //
    // `routes::tokens::create` refuses the same pair at MINT, which is the kinder
    // half — a caller finds out while they can still fix it. This stays as the
    // enforcing half: mint is not the only way a row is written (seeds and the
    // repo write them directly), and the gate may not depend on who wrote the row.
    if need == TokenScope::Mcp {
        return Err(ApiError::ForbiddenMsg(
            "a workspace-narrowed token cannot use /mcp — the MCP surface is tenant-wide; \
             mint a tenant-wide token with the 'mcp' scope"
                .into(),
        ));
    }

    if let Some(ident) = task_ident(&path) {
        let task = crate::services::tasks::resolve_id(state.tasks.as_ref(), tenant, ident).await?;
        return grant.require_workspace(workspace_of(state, tenant, task).await?);
    }

    // The collection listing: narrow it rather than refuse it.
    //
    // This lands because axum runs a handler's extractors in declaration order
    // and `auth: AuthCtx` is declared before `RawQuery` — so the query the
    // handler parses is the one rewritten here. That is a real dependency and
    // not a comfortable one, so it is guarded by a test rather than by a note:
    // `tests/scoped_tokens.rs` drives the shipped router and asserts a narrowed
    // token never sees the other workspace's card. Reorder those extractors and
    // it fails, which is the point — a narrowing that silently stops applying
    // returns the whole tenant and looks like success.
    parts.uri = pin_workspace_query(&parts.uri, narrowed)?;
    Ok(())
}

async fn workspace_of(
    state: &AppState,
    tenant: nook_types::TenantId,
    task: TaskId,
) -> Result<Option<WorkspaceId>, ApiError> {
    Ok(state
        .tasks
        .get_row(tenant, task)
        .await?
        .ok_or(ApiError::NotFound)?
        .workspace_id)
}

/// Force `workspace=<narrowed>` onto a listing's query string.
///
/// A caller that named the same workspace is left alone; one that named another
/// is refused, because returning A's cards under a question about B is a wrong
/// answer dressed as a right one. Every other filter is preserved verbatim.
fn pin_workspace_query(uri: &Uri, narrowed: WorkspaceId) -> Result<Uri, ApiError> {
    let mut kept: Vec<(String, String)> = Vec::new();
    for (k, v) in form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
        if k == "workspace" {
            let asked = v.trim();
            if !asked.is_empty() && !asked.eq_ignore_ascii_case(&narrowed.to_string()) {
                return Err(ApiError::ForbiddenMsg(format!(
                    "this token is narrowed to workspace {narrowed}, so it cannot list \
                     workspace {asked}"
                )));
            }
            continue;
        }
        kept.push((k.into_owned(), v.into_owned()));
    }
    let query = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(kept)
        .append_pair("workspace", &narrowed.to_string())
        .finish();
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(
        format!("{}?{query}", uri.path())
            .parse()
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("rebuilding the query: {e}")))?,
    );
    Uri::from_parts(parts).map_err(|e| ApiError::Internal(anyhow::anyhow!("{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scoped(names: &str) -> TokenGrant {
        TokenGrant::from_stored(Some(names), None)
    }

    #[test]
    fn deleting_a_card_is_not_something_tasks_write_folds_in() {
        assert_eq!(
            required_scope(&Method::DELETE, "/api/v1/tasks/MAIN-1"),
            None,
            "a destructive verb is asked for by name or not at all"
        );
    }

    #[test]
    fn an_unmapped_path_is_refused_rather_than_waved_through() {
        // The property that makes forgetting a route safe.
        assert_eq!(required_scope(&Method::GET, "/api/v1/nodes"), None);
        assert_eq!(required_scope(&Method::POST, "/api/v1/tokens"), None);
        assert_eq!(required_scope(&Method::POST, "/api/v1/tasks/bulk"), None);
        assert_eq!(required_scope(&Method::GET, "/healthz"), None);
    }

    #[test]
    fn a_report_is_a_comment_and_its_files_but_reading_is_tasks_read() {
        assert_eq!(
            required_scope(&Method::POST, "/api/v1/tasks/MAIN-1/comments"),
            Some(TokenScope::ReportsWrite)
        );
        assert_eq!(
            required_scope(&Method::POST, "/api/v1/tasks/MAIN-1/attachments"),
            Some(TokenScope::ReportsWrite)
        );
        assert_eq!(
            required_scope(&Method::GET, "/api/v1/tasks/MAIN-1/comments"),
            Some(TokenScope::TasksRead)
        );
        assert_eq!(
            required_scope(&Method::PATCH, "/api/v1/tasks/MAIN-1"),
            Some(TokenScope::TasksWrite)
        );
    }

    #[test]
    fn a_missing_scope_is_named_in_the_refusal() {
        let err = scoped("reports:write")
            .require(TokenScope::TasksWrite)
            .expect_err("reports:write does not carry tasks:write");
        let msg = err.to_string();
        assert!(msg.contains("tasks:write"), "names what is missing: {msg}");
        assert!(msg.contains("reports:write"), "and what it has: {msg}");
    }

    #[test]
    fn an_unknown_stored_scope_is_dropped_not_honoured() {
        let g = scoped("reports:write tasks:teleport");
        assert!(g.require(TokenScope::ReportsWrite).is_ok());
        assert_eq!(g.scopes().expect("scoped").to_stored(), "reports:write");
    }

    #[test]
    fn the_stored_form_is_canonical_so_equal_sets_compare_equal() {
        assert_eq!(
            ScopeSet::parse_stored("tasks:write reports:write"),
            ScopeSet::parse_stored("reports:write  tasks:write"),
        );
        assert_eq!(
            ScopeSet::parse_stored("mcp tasks:read").to_stored(),
            "tasks:read mcp"
        );
    }

    #[test]
    fn a_grant_can_never_widen_past_the_one_that_minted_it() {
        let minter = ScopeSet::parse_stored("reports:write tasks:read");
        assert!(ScopeSet::parse_stored("reports:write").subset_of(minter));
        assert!(!ScopeSet::parse_stored("reports:write mcp").subset_of(minter));
    }

    #[test]
    fn pinning_a_listing_keeps_the_other_filters_and_refuses_another_workspace() {
        let ws = WorkspaceId(Uuid::now_v7());
        let uri: Uri = "/api/v1/tasks?label=agent-ready&done=true".parse().unwrap();
        let pinned = pin_workspace_query(&uri, ws).expect("pins");
        let q = pinned.query().expect("a query");
        assert!(q.contains("label=agent-ready"), "kept the filters: {q}");
        assert!(q.contains(&format!("workspace={ws}")), "pinned: {q}");

        let other = WorkspaceId(Uuid::now_v7());
        let asked: Uri = format!("/api/v1/tasks?workspace={other}").parse().unwrap();
        pin_workspace_query(&asked, ws).expect_err("another workspace is refused, not rewritten");
    }

    #[test]
    fn a_narrowing_refuses_a_card_that_belongs_to_no_workspace() {
        let ws = WorkspaceId(Uuid::now_v7());
        let g = TokenGrant::from_stored(Some("tasks:read"), Some(ws.0));
        assert!(g.require_workspace(Some(ws)).is_ok());
        assert!(g.require_workspace(None).is_err(), "no workspace, no vouch");
    }

    #[test]
    fn an_unscoped_token_is_still_everything_its_owner_is() {
        let full = TokenGrant::from_stored(None, None);
        assert_eq!(full, TokenGrant::Full);
        for scope in TokenScope::ALL {
            assert!(full.require(scope).is_ok(), "NG-1: {scope} still works");
        }
    }
}
