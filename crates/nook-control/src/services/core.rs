//! The one shared helper left after the MAIN-245 split.
//!
//! This file used to hold 40 query sites spanning workspaces, nodes, sessions,
//! notes, identity, operator pages and the overview — a grab-bag that made every
//! aggregate's future repository card touch the same file. Those queries now live
//! in per-aggregate modules alongside their own kind of data.
//!
//! What is left is deliberately not a query: [`search_filter`] is pure string
//! handling shared by the operator pages (`operator_queries`) and the
//! tenant-members page (`identity`). Duplicating it into both would be a
//! behaviour change waiting to happen the first time one copy is fixed; giving
//! either module ownership would make the other depend on an unrelated
//! aggregate. So it stays here, with nothing else.

/// Normalize a search box value: whitespace-only is "no filter", not "match the
/// empty string". Shared by the operator list queries (MAIN-44).
///
/// `pub(crate)` since MAIN-245 only because its two callers now live in
/// different modules; the function itself is unchanged.
pub(crate) fn search_filter(q: Option<String>) -> Option<String> {
    q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
