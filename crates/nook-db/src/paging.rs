//! The pagination contract, server half (QOL sprint 2026-08).
//!
//! Every paginated list in the app speaks one wire shape — `{ rows,
//! next_cursor }` out, `q`/`after`/`limit`/`sort`/`dir` in — and this module is
//! where that shape gets its one implementation: cursor codec, argument
//! validation, the SQL skeleton, and the full-page next-cursor rule. A repo
//! "inherits" pagination by declaring a [`ListSpec`] (its SELECT, its
//! searchable columns, its sortable columns) instead of hand-rolling the
//! WHERE/ORDER/LIMIT dance; an in-memory fake inherits the same semantics from
//! [`page_vec`], so a fake cannot disagree with the SQL about what a page is.
//!
//! The cursor is OPAQUE BY CONTRACT: a client passes `next_cursor` back
//! verbatim as `after` and never parses it. That opacity is load-bearing — it
//! is what lets the server pick the mechanism per request (drift-free keyset on
//! the UUID v7 `id` for the default order, offset under an explicit column
//! sort, where a general keyset would need a per-column composite cursor) and
//! tighten it later without a wire change.

use uuid::Uuid;

use crate::dialect::{ci_match, type_mapping};
use crate::{params, Db, DbValue, Engine, FromDbRow};

/// The bound-search pattern every list shares: `q` arrives as `$2` and matches
/// as a case-insensitive substring. Composed, never spliced — the term itself
/// stays a bind parameter.
const SEARCH_PATTERN: &str = "'%' || $2 || '%'";

/// A decoded continuation token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    /// Keyset on the row's UUID v7 `id` — the default order's cursor.
    Keyset(Uuid),
    /// Row offset — the cursor under an explicit column sort.
    Offset(i64),
}

impl Cursor {
    pub fn encode(&self) -> String {
        match self {
            Cursor::Keyset(id) => format!("k:{id}"),
            Cursor::Offset(n) => format!("o:{n}"),
        }
    }

    pub fn decode(s: &str) -> Result<Self, PageError> {
        if let Some(rest) = s.strip_prefix("k:") {
            return rest
                .parse()
                .map(Cursor::Keyset)
                .map_err(|_| PageError::Cursor);
        }
        if let Some(rest) = s.strip_prefix("o:") {
            return rest
                .parse()
                .ok()
                .filter(|n| *n >= 0)
                .map(Cursor::Offset)
                .ok_or(PageError::Cursor);
        }
        Err(PageError::Cursor)
    }
}

/// What can be wrong with the wire arguments — every variant is the caller's
/// 400, never a 500, because none of them can arise from a well-behaved client
/// that passes cursors back verbatim and picks sorts from the documented set.
#[derive(Debug, PartialEq, Eq)]
pub enum PageError {
    Cursor,
    Sort(String),
    Dir(String),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::Cursor => write!(f, "invalid cursor"),
            PageError::Sort(k) => write!(f, "unknown sort key `{k}`"),
            PageError::Dir(d) => write!(f, "invalid dir `{d}` — use `asc` or `desc`"),
        }
    }
}

impl std::error::Error for PageError {}

/// A resolved sort. `col` is private on purpose: it is only ever one of the
/// list's declared output columns (picked from the allowlist in
/// [`PageArgs::parse`]), which is the invariant that makes splicing it into
/// ORDER BY safe. There is no way to construct one from user input directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sort {
    col: String,
    pub desc: bool,
}

impl Sort {
    pub fn col(&self) -> &str {
        &self.col
    }

    /// The default order — and the keyset order: UUID v7 ids are
    /// creation-ordered, so `id DESC` is newest-first with no tie to break.
    pub fn is_id(&self) -> bool {
        self.col == "id"
    }
}

/// Validated, resolved page arguments — the one way wire values become a page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageArgs {
    pub q: Option<String>,
    pub cursor: Option<Cursor>,
    pub limit: i64,
    pub sort: Sort,
}

impl PageArgs {
    /// Wire values in, validated args out. `sorts` is the list's public
    /// allowlist, `key -> output column` — an unknown key, a bad dir, or a
    /// cursor that does not fit the requested order is a [`PageError`].
    ///
    /// Defaults: no sort is `id DESC` (newest first); an explicit sort is
    /// ascending unless `dir=desc`; `dir` alone flips the default order.
    pub fn parse(
        q: Option<&str>,
        after: Option<&str>,
        limit: Option<i64>,
        sort: Option<&str>,
        dir: Option<&str>,
        sorts: &[(&str, &str)],
    ) -> Result<Self, PageError> {
        // Empty or whitespace-only search is "no filter", not "match the empty
        // string" — the search box clears to that and must show everything.
        let q = q
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let col = match sort {
            None => "id".to_string(),
            Some(key) => sorts
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, col)| col.to_string())
                .ok_or_else(|| PageError::Sort(key.to_string()))?,
        };
        let desc = match dir {
            Some("asc") => false,
            Some("desc") => true,
            None => sort.is_none(), // newest first by default; explicit sorts read ascending
            Some(other) => return Err(PageError::Dir(other.to_string())),
        };
        let sort = Sort { col, desc };

        let cursor = after.map(Cursor::decode).transpose()?;
        // A cursor must fit the order it continues: keyset for the id order,
        // offset for a column sort. A mismatch means the client changed the
        // sort mid-walk but kept the old token — refuse, never guess.
        match (&cursor, sort.is_id()) {
            (Some(Cursor::Keyset(_)), false) | (Some(Cursor::Offset(_)), true) => {
                return Err(PageError::Cursor)
            }
            _ => {}
        }

        Ok(PageArgs {
            q,
            cursor,
            limit: limit.unwrap_or(50).clamp(1, 200),
            sort,
        })
    }
}

/// One page of rows plus the opaque token for the next. `next_cursor` is
/// non-null exactly when the page came back FULL: a short page is the end, and
/// a caller that pages one past the end gets an empty page and a null cursor —
/// a clean end-of-list, not an error.
#[derive(Debug, Clone)]
pub struct DbPage<T> {
    pub rows: Vec<T>,
    pub next_cursor: Option<String>,
}

/// A list endpoint's shape: what it selects, what `q` searches, what `sort`
/// may order by. Declaring one of these IS adopting the pagination contract —
/// the WHERE/ORDER/LIMIT skeleton and both cursor mechanisms come with it.
pub struct ListSpec<'a> {
    /// The inner SELECT. Every searchable/sortable thing must be a named output
    /// column, because the wrapper only sees the subquery's output — which is
    /// also what makes computed columns (counts, COALESCEs) sortable for free.
    /// May reference scope parameters `$4` onward; `$1`–`$3` belong to the
    /// wrapper (limit, q, cursor/offset).
    pub select: &'a str,
    /// The UUID v7 output column the keyset walks (usually `id`).
    pub id: &'a str,
    /// Output columns `q` matches, case-insensitively, OR-ed together.
    pub search: &'a [&'a str],
}

impl ListSpec<'_> {
    /// The wrapper SQL for these args. Pure, so the shape is testable without
    /// a database; [`ListSpec::fetch`] is this plus execution.
    pub fn sql(&self, engine: Engine, args: &PageArgs) -> String {
        let tm = type_mapping(engine);
        let cm = ci_match(engine);
        let term = tm.cast("$2", "text");
        let search = if self.search.is_empty() {
            "1 = 0".to_string()
        } else {
            self.search
                .iter()
                .map(|c| cm.ci_match(&format!("x.{c}"), SEARCH_PATTERN))
                .collect::<Vec<_>>()
                .join(" OR ")
        };
        let dir = if args.sort.desc { "DESC" } else { "ASC" };
        if args.sort.is_id() {
            let op = if args.sort.desc { "<" } else { ">" };
            let cursor = tm.cast("$3", "uuid");
            format!(
                "SELECT x.* FROM ({select}) x \
                 WHERE ({term} IS NULL OR ({search})) \
                   AND ({cursor} IS NULL OR x.{id} {op} $3) \
                 ORDER BY x.{id} {dir} \
                 LIMIT $1",
                select = self.select,
                id = self.id,
            )
        } else {
            // NULLS LAST both ways: a node never seen must sort after every
            // node that has been, whichever way `last_seen` is flipped.
            format!(
                "SELECT x.* FROM ({select}) x \
                 WHERE ({term} IS NULL OR ({search})) \
                 ORDER BY x.{col} {dir} NULLS LAST, x.{id} DESC \
                 LIMIT $1 OFFSET $3",
                select = self.select,
                col = args.sort.col(),
                id = self.id,
            )
        }
    }

    /// Run the page. `scope` params bind from `$4` onward; `id_of` names the
    /// keyset id of a row (the same column [`ListSpec::id`] names in SQL).
    pub async fn fetch<T>(
        &self,
        db: &crate::DbPool,
        args: &PageArgs,
        scope: Vec<DbValue>,
        id_of: impl Fn(&T) -> Uuid,
    ) -> Result<DbPage<T>, crate::DbError>
    where
        T: Send + Unpin + FromDbRow,
    {
        let sql = self.sql(db.engine(), args);
        let third = match args.cursor {
            Some(Cursor::Keyset(id)) => DbValue::Uuid(Some(id)),
            Some(Cursor::Offset(n)) => DbValue::I64(Some(n)),
            None if args.sort.is_id() => DbValue::Uuid(None),
            None => DbValue::I64(Some(0)),
        };
        let mut bound = params![args.limit, args.q.clone()];
        bound.push(third);
        bound.extend(scope);
        let rows: Vec<T> = db.query_all(&sql, bound).await?;
        let next_cursor = next_cursor(&rows, args, id_of);
        Ok(DbPage { rows, next_cursor })
    }
}

/// The full-page rule, shared by SQL and fakes.
fn next_cursor<T>(rows: &[T], args: &PageArgs, id_of: impl Fn(&T) -> Uuid) -> Option<String> {
    if rows.len() as i64 != args.limit {
        return None;
    }
    if args.sort.is_id() {
        rows.last().map(|r| Cursor::Keyset(id_of(r)).encode())
    } else {
        let offset = match args.cursor {
            Some(Cursor::Offset(n)) => n,
            _ => 0,
        };
        Some(Cursor::Offset(offset + args.limit).encode())
    }
}

/// The same page semantics over an in-memory Vec, for repository fakes. The
/// caller has already applied its `q` filter (searchable fields differ per
/// list); `by_col` compares two rows under a named sort column, ascending —
/// direction, tiebreak, cursor and the next-cursor rule are applied here so
/// they cannot drift from the SQL's.
pub fn page_vec<T>(
    mut rows: Vec<T>,
    args: &PageArgs,
    id_of: impl Fn(&T) -> Uuid + Copy,
    by_col: impl Fn(&str, &T, &T) -> std::cmp::Ordering,
) -> DbPage<T> {
    if args.sort.is_id() {
        rows.sort_by_key(|r| std::cmp::Reverse(id_of(r)));
        if !args.sort.desc {
            rows.reverse();
        }
        if let Some(Cursor::Keyset(after)) = args.cursor {
            rows.retain(|r| {
                if args.sort.desc {
                    id_of(r) < after
                } else {
                    id_of(r) > after
                }
            });
        }
    } else {
        rows.sort_by(|a, b| {
            let ord = by_col(args.sort.col(), a, b);
            let ord = if args.sort.desc { ord.reverse() } else { ord };
            ord.then(id_of(b).cmp(&id_of(a)))
        });
        if let Some(Cursor::Offset(n)) = args.cursor {
            rows.drain(..(n as usize).min(rows.len()));
        }
    }
    rows.truncate(args.limit.max(0) as usize);
    let next_cursor = next_cursor(&rows, args, id_of);
    DbPage { rows, next_cursor }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SORTS: &[(&str, &str)] = &[("slug", "slug"), ("created", "id"), ("members", "members")];

    fn parse(
        after: Option<&str>,
        sort: Option<&str>,
        dir: Option<&str>,
    ) -> Result<PageArgs, PageError> {
        PageArgs::parse(None, after, None, sort, dir, SORTS)
    }

    #[test]
    fn cursors_round_trip_and_reject_noise() {
        let id = Uuid::now_v7();
        assert_eq!(
            Cursor::decode(&Cursor::Keyset(id).encode()),
            Ok(Cursor::Keyset(id))
        );
        assert_eq!(
            Cursor::decode(&Cursor::Offset(150).encode()),
            Ok(Cursor::Offset(150))
        );
        for bad in ["", "150", "x:1", "k:nope", "o:-3", "o:1.5"] {
            assert_eq!(Cursor::decode(bad), Err(PageError::Cursor), "{bad}");
        }
    }

    #[test]
    fn parse_resolves_the_allowlist_and_nothing_else() {
        assert_eq!(parse(None, Some("slug"), None).unwrap().sort.col(), "slug");
        // "created" maps to the id column, so it keeps the keyset mechanism.
        let created = parse(None, Some("created"), None).unwrap();
        assert!(created.sort.is_id() && !created.sort.desc);
        assert_eq!(
            parse(None, Some("nope"), None),
            Err(PageError::Sort("nope".into()))
        );
        assert_eq!(
            parse(None, None, Some("sideways")),
            Err(PageError::Dir("sideways".into()))
        );
    }

    #[test]
    fn defaults_newest_first_and_explicit_sorts_ascending() {
        let d = parse(None, None, None).unwrap();
        assert!(d.sort.is_id() && d.sort.desc);
        assert!(!parse(None, Some("slug"), None).unwrap().sort.desc);
        // dir alone flips the default order — how "oldest first" is asked for.
        assert!(!parse(None, None, Some("asc")).unwrap().sort.desc);
    }

    #[test]
    fn a_cursor_must_fit_the_order_it_continues() {
        let k = Cursor::Keyset(Uuid::now_v7()).encode();
        let o = Cursor::Offset(50).encode();
        assert!(parse(Some(&k), None, None).is_ok());
        assert!(parse(Some(&o), Some("slug"), None).is_ok());
        assert_eq!(parse(Some(&o), None, None), Err(PageError::Cursor));
        assert_eq!(parse(Some(&k), Some("slug"), None), Err(PageError::Cursor));
    }

    #[test]
    fn limit_clamps_and_q_trims_to_none() {
        assert_eq!(
            PageArgs::parse(None, None, Some(9999), None, None, SORTS)
                .unwrap()
                .limit,
            200
        );
        assert_eq!(
            PageArgs::parse(Some("   "), None, None, None, None, SORTS)
                .unwrap()
                .q,
            None
        );
    }

    #[test]
    fn sql_keyset_and_offset_shapes() {
        let spec = ListSpec {
            select: "SELECT t.id, t.slug FROM tenants t",
            id: "id",
            search: &["slug"],
        };
        let keyset = spec.sql(Engine::Postgres, &parse(None, None, None).unwrap());
        assert!(keyset.contains("x.id < $3") && keyset.contains("ORDER BY x.id DESC"));
        assert!(keyset.contains("LIMIT $1") && !keyset.contains("OFFSET"));

        let sorted = spec.sql(
            Engine::Postgres,
            &parse(None, Some("slug"), Some("desc")).unwrap(),
        );
        assert!(sorted.contains("ORDER BY x.slug DESC NULLS LAST, x.id DESC"));
        assert!(sorted.contains("OFFSET $3"));
        // The search term stays a bind parameter on both engines.
        for engine in [Engine::Postgres, Engine::Sqlite] {
            assert!(spec
                .sql(engine, &parse(None, None, None).unwrap())
                .contains("'%' || $2 || '%'"));
        }
    }

    #[derive(Clone)]
    struct Row {
        id: Uuid,
        slug: &'static str,
        members: i64,
    }

    fn rows() -> Vec<Row> {
        // Ids are v7: creation order is a..c.
        ["bravo", "alpha", "charlie"]
            .iter()
            .enumerate()
            .map(|(i, slug)| Row {
                id: Uuid::now_v7(),
                slug,
                members: i as i64,
            })
            .collect()
    }

    fn by_col(col: &str, a: &Row, b: &Row) -> std::cmp::Ordering {
        match col {
            "slug" => a.slug.cmp(b.slug),
            "members" => a.members.cmp(&b.members),
            other => unreachable!("unknown col {other}"),
        }
    }

    #[test]
    fn page_vec_matches_the_contract() {
        let data = rows();
        // Default: newest first, keyset cursor on a full page.
        let args = PageArgs::parse(None, None, Some(2), None, None, SORTS).unwrap();
        let p1 = page_vec(data.clone(), &args, |r| r.id, by_col);
        assert_eq!(p1.rows[0].slug, "charlie");
        let cur = p1.next_cursor.expect("full page has a cursor");
        let args2 = PageArgs::parse(None, Some(&cur), Some(2), None, None, SORTS).unwrap();
        let p2 = page_vec(data.clone(), &args2, |r| r.id, by_col);
        assert_eq!(p2.rows.len(), 1);
        assert_eq!(p2.rows[0].slug, "bravo");
        assert!(p2.next_cursor.is_none(), "short page ends the walk");

        // Explicit sort: ascending by slug, offset cursor.
        let args = PageArgs::parse(None, None, Some(2), Some("slug"), None, SORTS).unwrap();
        let p1 = page_vec(data.clone(), &args, |r| r.id, by_col);
        assert_eq!(
            p1.rows.iter().map(|r| r.slug).collect::<Vec<_>>(),
            ["alpha", "bravo"]
        );
        let cur = p1.next_cursor.unwrap();
        assert_eq!(Cursor::decode(&cur), Ok(Cursor::Offset(2)));
        let args2 = PageArgs::parse(None, Some(&cur), Some(2), Some("slug"), None, SORTS).unwrap();
        let p2 = page_vec(data, &args2, |r| r.id, by_col);
        assert_eq!(
            p2.rows.iter().map(|r| r.slug).collect::<Vec<_>>(),
            ["charlie"]
        );
    }
}
