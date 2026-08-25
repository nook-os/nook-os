//! A board belongs to a workspace (MAIN-637).
//!
//! Two callers need the same three things — derive a free key, make a board
//! with the five typed columns, and hold "a workspace has at most one board" —
//! so they live here rather than in `routes/boards.rs` where the create handler
//! grew them. `POST /boards` and `POST /workspaces` both go through this module,
//! which is what makes the two indistinguishable.

use nook_types::*;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Name AND type. A board whose columns have no types is one automation cannot
/// navigate — "move it to started" has nothing to resolve.
pub const DEFAULT_COLUMNS: [(&str, &str); 5] = [
    ("Triage", "backlog"),
    ("Todo", "unstarted"),
    ("In Progress", "started"),
    ("In Review", "review"),
    ("Done", "completed"),
];

/// The first word of a board's name, as a key.
///
/// "NookOS Bootstrap" → `NOOK`. Deliberately not the whole name flattened and
/// cut, which is what produced `NOOKO` — a key nobody would choose, printed on
/// every task forever.
pub fn derive_key(name: &str) -> String {
    let first: String = name
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(4)
        .collect::<String>()
        .to_uppercase();
    if first.is_empty() {
        "BOARD".to_string()
    } else {
        first
    }
}

/// A board key: the `NOOK` in `NOOK-42`.
pub fn validate_key(key: &str) -> ApiResult<String> {
    let k = key.trim().to_uppercase();
    if k.is_empty() || k.len() > 10 {
        return Err(ApiError::BadRequest(
            "a board key must be 1–10 characters".into(),
        ));
    }
    if !k.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::BadRequest(format!(
            "a board key may only contain letters and digits — got {key:?}"
        )));
    }
    Ok(k)
}

/// Derive a key from a board's name, and make it unique in the tenant.
///
/// The FIRST WORD, not the whole name flattened: "NookOS Bootstrap" should be
/// `NOOK`, not `NOOKO` — which is what you get by running the words together
/// and cutting at five characters, and which reads as a typo forever after.
pub async fn unique_key(state: &AppState, tenant: TenantId, name: &str) -> ApiResult<String> {
    let base = derive_key(name);

    for n in 1..100 {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}{n}")
        };
        let taken = state.tasks.board_key_taken(tenant, &candidate).await?;
        if !taken {
            return Ok(candidate);
        }
    }
    Err(ApiError::BadRequest(
        "could not derive a free board key — pass one explicitly".into(),
    ))
}

/// Create a board and its five typed columns.
pub async fn create_with_columns(
    state: &AppState,
    tenant: TenantId,
    workspace: Option<WorkspaceId>,
    name: &str,
    key: &str,
) -> ApiResult<(Board, Vec<BoardColumn>)> {
    let board = state
        .tasks
        .create_board(tenant, workspace.map(|w| w.0), name, key)
        .await?;

    let mut columns = Vec::with_capacity(DEFAULT_COLUMNS.len());
    for (i, (name, kind)) in DEFAULT_COLUMNS.iter().enumerate() {
        columns.push(
            state
                .tasks
                .create_column(board.id, name, i as i32, kind)
                .await?,
        );
    }
    Ok((board, columns))
}

/// The workspace must be one this tenant owns.
///
/// [`ensure_workspace_free`] is tenant-scoped, so without this a board attached
/// to another tenant's workspace would escape the at-most-one rule entirely —
/// and the foreign key, which is not tenant-aware, would happily store it.
pub async fn ensure_workspace_is_ours(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> ApiResult<()> {
    if state.workspaces.get(tenant, workspace).await?.is_none() {
        return Err(ApiError::BadRequest(format!(
            "no such workspace in this tenant — {}",
            workspace.0
        )));
    }
    Ok(())
}

/// Refuse a second board for a workspace that already has one (AC-2/NG-6).
///
/// `moving` is the board being attached, so re-attaching a board to the
/// workspace it is already on is a no-op rather than a conflict with itself.
pub async fn ensure_workspace_free(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    moving: Option<BoardId>,
) -> ApiResult<()> {
    let Some(existing) = state.tasks.board_of_workspace(tenant, workspace).await? else {
        return Ok(());
    };
    if Some(existing.id) == moving {
        return Ok(());
    }
    Err(ApiError::Conflict(format!(
        "workspace already has a board — '{}'{}; a workspace has at most one",
        existing.name,
        existing
            .key
            .as_deref()
            .map(|k| format!(" ({k})"))
            .unwrap_or_default()
    )))
}

/// A card and the board it sits on must name the same workspace (AC-6).
///
/// A board attached to no workspace takes any card, exactly as it does today —
/// which is what keeps an unadopted board (prod's `MAIN`) working while the
/// adoption is still an operator's pending decision.
pub async fn ensure_task_workspace_agrees(
    state: &AppState,
    tenant: TenantId,
    board: BoardId,
    task_workspace: Option<WorkspaceId>,
) -> ApiResult<()> {
    let Some(task_workspace) = task_workspace else {
        return Ok(());
    };
    let Some(board) = state.tasks.get_board(tenant, board).await? else {
        return Err(ApiError::NotFound);
    };
    let Some(board_workspace) = board.workspace_id else {
        return Ok(());
    };
    if board_workspace == task_workspace {
        return Ok(());
    }
    // Both named, because "workspace mismatch" leaves the reader to go and look
    // up which two — and the answer decides whether the card or the board is
    // the thing that is wrong.
    let board_label = describe_workspace(state, tenant, board_workspace).await;
    let task_label = describe_workspace(state, tenant, task_workspace).await;
    Err(ApiError::BadRequest(format!(
        "board '{}' belongs to workspace {board_label}, but the task belongs to \
         workspace {task_label} — they must be the same",
        board.name
    )))
}

/// `name (uuid)` when the workspace is readable, the bare id otherwise — a name
/// alone would be ambiguous, and an id alone unreadable.
async fn describe_workspace(state: &AppState, tenant: TenantId, id: WorkspaceId) -> String {
    match state.workspaces.get(tenant, id).await {
        Ok(Some(w)) => format!("'{}' ({})", w.name, id.0),
        _ => id.0.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "NookOS Bootstrap" must become NOOK. The first implementation flattened
    /// the whole name and cut at five, producing NOOKO — a key nobody would
    /// choose, on every task, permanently.
    #[test]
    fn a_key_comes_from_the_first_word() {
        assert_eq!(derive_key("NookOS Bootstrap"), "NOOK");
        assert_eq!(derive_key("Engineering"), "ENGI");
        assert_eq!(derive_key("web-ui rewrite"), "WEBU");
        // A name with no usable letters still needs a key.
        assert_eq!(derive_key("  "), "BOARD");
        assert_eq!(derive_key("!!! ???"), "BOARD");
        assert_eq!(derive_key(""), "BOARD");
    }

    #[test]
    fn keys_are_uppercased_and_bounded() {
        assert_eq!(validate_key("nook").unwrap(), "NOOK");
        assert_eq!(validate_key(" web ").unwrap(), "WEB");
        assert!(validate_key("").is_err());
        assert!(validate_key("has space").is_err());
        assert!(validate_key("dash-ed").is_err());
        assert!(validate_key("ABCDEFGHIJK").is_err());
    }
}
