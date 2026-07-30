//! Operator-console page reads (MAIN-245, behind `OperatorRepository` since
//! MAIN-258).
//!
//! Split verbatim out of `services/core.rs`. The keyset-pagination tests moved
//! with them; they also cover `tenant_members_page`, which now lives in
//! `services::identity` — the shared cursor/search behaviour is the thing under
//! test, so they stayed together rather than being split in half.
//!
//! What is left here after MAIN-258 is the one rule these four lists share and
//! the repository does not: how a page decides whether there is another one.
//! The queries themselves live in `repo::admin`.

use nook_types::*;

use crate::error::ApiResult;
use crate::repo::admin::{Keyset, OperatorRepository};
use crate::services::core::search_filter;

/// The console's page size, clamped the same way for every list.
fn keyset(after: Option<uuid::Uuid>, limit: i64) -> Keyset {
    Keyset {
        after,
        limit: limit.clamp(1, 200),
    }
}

/// The cursor is the last id of a FULL page: a page short of `limit` means
/// there is no more, so `next_cursor` is null. A caller that pages one past the
/// end therefore gets an empty page and a null cursor — a clean end-of-list,
/// not an error.
fn next_cursor<T, F, Id>(rows: &[T], limit: i64, id_of: F) -> Option<Id>
where
    F: Fn(&T) -> Id,
{
    (rows.len() as i64 == limit)
        .then(|| rows.last().map(id_of))
        .flatten()
}

/// The operator audit trail, paged by keyset cursor and filtered by an optional
/// server-side search (MAIN-43).
///
/// Search (`q`) is case-insensitive and matches across the event kind, the
/// tenant slug, and the actor (type or id) — the whole log, not just the page
/// in hand, because the `WHERE` runs before `LIMIT`. Pagination is keyset on the
/// row's UUID v7 `id`: `after` is the last id the caller has seen, and rows are
/// walked `id DESC`, so each page is strictly older with no offset to drift.
///
/// Kinds, actors and times only — never payloads, which can carry a branch name
/// or task title this surface must not hand over (the same rule `audit_log`
/// enforced before it grew a cursor).
pub async fn operator_audit_page(
    repo: &dyn OperatorRepository,
    q: Option<String>,
    after: Option<EventId>,
    limit: i64,
) -> ApiResult<OperatorAuditPage> {
    let page = keyset(after.map(|e| e.0), limit);
    // An empty or whitespace-only search is "no filter", not "match the empty
    // string" — the search box clears to that and must show the whole log.
    let rows = repo.audit_page(search_filter(q), page).await?;
    let next_cursor = next_cursor(&rows, page.limit, |r| r.id);
    Ok(OperatorAuditPage { rows, next_cursor })
}

/// Operator tenants, keyset-paginated + searched (slug/name), mirroring
/// `operator_audit_page`. Rows come back WITHOUT the policy-gated fields
/// (`repositories`/`task_titles`); the handler enriches them per opted-in org.
pub async fn operator_tenants_page(
    repo: &dyn OperatorRepository,
    q: Option<String>,
    after: Option<TenantId>,
    limit: i64,
) -> ApiResult<OperatorTenantPage> {
    let page = keyset(after.map(|t| t.0), limit);
    let rows = repo.tenants_page(search_filter(q), page).await?;
    let next_cursor = next_cursor(&rows, page.limit, |r| r.id);
    Ok(OperatorTenantPage { rows, next_cursor })
}

/// Operator nodes, keyset-paginated + searched (name/tenant slug/platform/status).
pub async fn operator_nodes_page(
    repo: &dyn OperatorRepository,
    q: Option<String>,
    after: Option<NodeId>,
    limit: i64,
) -> ApiResult<OperatorNodePage> {
    let page = keyset(after.map(|n| n.0), limit);
    let rows = repo.nodes_page(search_filter(q), page).await?;
    let next_cursor = next_cursor(&rows, page.limit, |r| r.id);
    Ok(OperatorNodePage { rows, next_cursor })
}

/// Operator role bindings, keyset-paginated + searched (email/role/scope).
pub async fn operator_bindings_page(
    repo: &dyn OperatorRepository,
    q: Option<String>,
    after: Option<uuid::Uuid>,
    limit: i64,
) -> ApiResult<OperatorBindingPage> {
    let page = keyset(after, limit);
    let rows = repo.bindings_page(search_filter(q), page).await?;
    let next_cursor = next_cursor(&rows, page.limit, |r| r.id);
    Ok(OperatorBindingPage { rows, next_cursor })
}

#[cfg(test)]
mod db_tests {

    /// The real repositories over this test's pool — these stay DB-backed
    /// (NG-4): the SQL is what is under test.
    fn repo_of(db: &DbPool) -> crate::repo::identity::DbIdentityRepository {
        crate::repo::identity::DbIdentityRepository::new(db.clone())
    }

    fn operator_repo(db: &DbPool) -> crate::repo::admin::DbOperatorRepository {
        crate::repo::admin::DbOperatorRepository::new(db.clone())
    }
    use super::{
        operator_audit_page, operator_bindings_page, operator_nodes_page, operator_tenants_page,
    };
    // Lives in the identity aggregate since MAIN-245; the keyset behaviour under
    // test here is shared with the operator pages, so the tests stayed together.
    use crate::services::identity::tenant_members_page;
    use nook_db::{params, Db, DbPool};
    use nook_types::{EventId, NodeId, TenantId};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    async fn pool() -> Option<DbPool> {
        if std::env::var("NOOK_REQUIRE_DB").ok().as_deref() != Some("1") {
            return None;
        }
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        crate::MIGRATOR.run(&db).await.ok()?;
        Some(nook_db::EnginePool::from_pg(db))
    }

    async fn tenant(db: &DbPool, slug: &str) -> TenantId {
        // v7 (creation-ordered), matching production `TenantId::new()`, so the
        // keyset `ORDER BY id DESC` walks newest-first as the real endpoints do.
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)",
            params![id, slug, format!("{slug}-{id}")],
        )
        .await
        .unwrap();
        TenantId(id)
    }

    async fn node(db: &DbPool, tenant: TenantId, name: &str, status: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO nodes (id, tenant_id, name, node_token_hash, platform, status)
             VALUES ($1, $2, $3, $4, 'linux', $5)",
            // Token hash unique per node — node_token_hash is unique instance-wide.
            params![id, tenant.0, name, id.to_string(), status],
        )
        .await
        .unwrap();
        id
    }

    async fn user(db: &DbPool, tenant: TenantId, email: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, 'U', $3, 'member')",
            params![id, tenant.0, email],
        )
        .await
        .unwrap();
        id
    }

    async fn binding(db: &DbPool, subject: Uuid, role_key: &str) -> Uuid {
        let id = Uuid::now_v7();
        db.exec(
            "INSERT INTO role_bindings (id, subject_id, role_key, scope_type)
             VALUES ($1, $2, $3, 'deployment')",
            params![id, subject, role_key],
        )
        .await
        .unwrap();
        id
    }

    /// Insert one audit-visible event and return its (v7, creation-ordered) id.
    async fn event(db: &DbPool, tenant: TenantId, kind: &str, actor_type: &str) -> EventId {
        let id = EventId::new();
        db.exec(
            "INSERT INTO events (id, tenant_id, kind, actor_type, actor_id)
             VALUES ($1, $2, $3, $4, $5)",
            params![id, tenant.0, kind, actor_type, Uuid::new_v4()],
        )
        .await
        .unwrap();
        id
    }

    async fn cleanup(db: &DbPool, t: TenantId) {
        // role_bindings have no tenant_id column, so delete them via their
        // subjects first (both role_bindings and tenant_members reference users).
        let _ = db
            .exec(
                "DELETE FROM role_bindings WHERE subject_id IN (SELECT id FROM users WHERE tenant_id = $1)",
                params![t.0],
            )
            .await;
        for tbl in ["events", "nodes", "tenant_members", "users"] {
            let _ = db
                .exec(
                    &format!("DELETE FROM {tbl} WHERE tenant_id = $1"),
                    params![t.0],
                )
                .await;
        }
        let _ = db
            .exec("DELETE FROM tenants WHERE id = $1", params![t.0])
            .await;
    }

    /// A member: a v7 `users` row (the keyset id) + its `tenant_members` grant.
    async fn member(db: &DbPool, tenant: TenantId, email: &str, name: &str, role: &str) -> Uuid {
        let uid = Uuid::now_v7();
        db.exec(
            "INSERT INTO users (id, tenant_id, display_name, email, role)
             VALUES ($1, $2, $3, $4, $5)",
            params![uid, tenant.0, name, email, role],
        )
        .await
        .unwrap();
        db.exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, $4)",
            params![Uuid::new_v4(), tenant.0, uid, role],
        )
        .await
        .unwrap();
        uid
    }

    /// AC-2 for members: bounded page + a cursor that walks older rows, and a
    /// search (email/name/role) that reaches a match beyond the first page.
    #[tokio::test]
    async fn member_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping member_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "mem-page").await;
        // The needle is the OLDEST member (smallest v7 id → a later page).
        let needle = member(&db, t, "needle@m.test", "Needle Person", "member").await;
        for i in 0..4 {
            member(
                &db,
                t,
                &format!("f{i}@m.test"),
                &format!("Filler {i}"),
                "member",
            )
            .await;
        }

        let p1 = tenant_members_page(&repo_of(&db), t, None, None, 2)
            .await
            .unwrap();
        assert!(p1.rows.len() <= 2, "page is bounded");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Search by (distinctive) email/name reaches the needle on a later page.
        let hit = tenant_members_page(&repo_of(&db), t, Some("NEEDLE".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.principal_id == needle),
            "case-insensitive search finds a later-page member"
        );
        assert!(
            hit.rows
                .iter()
                .all(|r| r.email.to_lowercase().contains("needle")
                    || r.display_name.to_lowercase().contains("needle")),
            "non-matching members are excluded"
        );

        // No matches → empty.
        assert!(
            tenant_members_page(&repo_of(&db), t, Some("zzno".into()), None, 50)
                .await
                .unwrap()
                .rows
                .is_empty(),
            "no matches is empty"
        );

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2: pages are bounded, the cursor walks strictly older rows with
    /// no overlap or gap, and the end of the list yields a null cursor.
    #[tokio::test]
    async fn cursor_walks_older_rows_with_no_overlap_or_gap() {
        let Some(db) = pool().await else {
            eprintln!("skipping cursor_walks_older_rows_with_no_overlap_or_gap — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-page").await;
        // Five events, oldest → newest (v7 ids increase with insertion order).
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(event(&db, t, "operator.audit", "user").await);
        }
        // Newest first is the reverse of insertion order.
        let newest_first: Vec<EventId> = ids.iter().rev().copied().collect();

        // Page 1: the two newest, with a cursor.
        let p1 = operator_audit_page(&operator_repo(&db), None, None, 2)
            .await
            .unwrap();
        // Filter to THIS tenant's rows so a shared dev DB's other events don't
        // perturb the assertions — we only reason about ids we inserted.
        let seen: Vec<EventId> = p1
            .rows
            .iter()
            .map(|r| r.id)
            .filter(|id| ids.contains(id))
            .collect();
        assert!(p1.rows.len() <= 2, "page is bounded by the limit");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Walk pages via the cursor and collect our ids, stopping once we have
        // all of OUR rows — NOT paging the global list to exhaustion (MAIN-93
        // AC-1). `operator_audit_page` is deployment-wide by design (NG-1), so
        // on the shared dev DB the global list is effectively unbounded; our
        // five rows are the newest, so a bounded walk reaches them in the first
        // pages. Walking to `next_cursor == None` tripped the guard once the DB
        // held more than ~40 audit rows.
        let mut collected = Vec::new();
        collected.extend(seen);
        let mut cursor = p1.next_cursor;
        let mut guard = 0;
        while cursor.is_some() && collected.len() < ids.len() {
            let after = cursor.take().unwrap();
            guard += 1;
            assert!(guard < 20, "cursor did not reach our rows");
            let page = operator_audit_page(&operator_repo(&db), None, Some(after), 2)
                .await
                .unwrap();
            for r in &page.rows {
                if ids.contains(&r.id) {
                    collected.push(r.id);
                }
            }
            cursor = page.next_cursor;
        }

        // No id appears twice (no overlap) and every id appears (no gap).
        let mut deduped = collected.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), collected.len(), "no row was returned twice");
        for id in &ids {
            assert!(collected.contains(id), "every inserted row was reached");
        }
        // And the order our ids came back in is newest-first.
        let ours_in_order: Vec<EventId> = collected
            .iter()
            .filter(|id| ids.contains(id))
            .copied()
            .collect();
        assert_eq!(ours_in_order, newest_first, "rows arrive newest-first");

        cleanup(&db, t).await;
    }

    /// AC-2: search filters the WHOLE log — a match that lives beyond the first
    /// page is still returned — and is case-insensitive.
    #[tokio::test]
    async fn search_finds_a_match_beyond_the_first_page() {
        let Some(db) = pool().await else {
            eprintln!("skipping search_finds_a_match_beyond_the_first_page — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-search").await;
        // The distinctive kind is the OLDEST row, so without server-side search
        // it would sit on a later page.
        let needle = event(&db, t, "node.RevokeD", "node").await;
        for _ in 0..5 {
            event(&db, t, "operator.audit", "user").await;
        }

        // Case-insensitive substring on the kind, small page — the match is not
        // on page one, yet search returns it.
        let hit = operator_audit_page(&operator_repo(&db), Some("revoked".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.id == needle),
            "server-side search reached a match beyond the first page"
        );
        // The noise rows do not match the needle.
        assert!(
            hit.rows
                .iter()
                .all(|r| r.kind.to_lowercase().contains("revoked")),
            "search excludes non-matching rows"
        );

        cleanup(&db, t).await;
    }

    /// AC-2: paging one past the end is a clean empty page with a null cursor,
    /// not an error; and a short page (fewer than the limit) has no cursor.
    #[tokio::test]
    async fn end_of_list_is_a_clean_null_cursor() {
        let Some(db) = pool().await else {
            eprintln!("skipping end_of_list_is_a_clean_null_cursor — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "audit-end").await;
        // A kind unique to this run, so the searched list is EXACTLY our rows —
        // the shared dev DB holds many other `operator.*` events, and searching
        // the common "operator.audit" would fill a 50-row page and never end
        // (MAIN-93 AC-3). It still starts `operator.` so it is an operator event.
        let kind = format!("operator.audit_end_{}", uuid::Uuid::now_v7().simple());
        let only = event(&db, t, &kind, "user").await;

        // Search by that unique token: a list of exactly one row, so the page is
        // short and the cursor is null.
        let page = operator_audit_page(&operator_repo(&db), Some(kind.clone()), None, 50)
            .await
            .unwrap();
        assert!(
            page.rows.iter().any(|r| r.id == only),
            "our row is in the page"
        );
        assert_eq!(page.rows.len(), 1, "only our unique-kind row matches");
        assert!(
            (page.rows.len() as i64) < 50,
            "the page did not fill, so there is no next page"
        );
        assert!(page.next_cursor.is_none(), "a short page ends the list");

        // Paging strictly past our row returns no error (empty of our id).
        let past = operator_audit_page(&operator_repo(&db), None, Some(only), 50)
            .await
            .unwrap();
        assert!(
            !past.rows.iter().any(|r| r.id == only),
            "the cursor excludes the row it points at"
        );

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2 for tenants: a bounded page + a cursor that walks older rows,
    /// and a slug/name search that reaches a match beyond the first page.
    #[tokio::test]
    async fn tenants_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping tenants_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        // The needle is created FIRST (oldest, smallest v7 id → a later page).
        let needle = tenant(&db, "zzneedle").await;
        let mut all = vec![needle];
        for i in 0..4 {
            all.push(tenant(&db, &format!("filler{i}")).await);
        }

        // Page 1 is bounded and carries a cursor.
        let p1 = operator_tenants_page(&operator_repo(&db), None, None, 2)
            .await
            .unwrap();
        assert!(p1.rows.len() <= 2, "page is bounded");
        assert!(p1.next_cursor.is_some(), "a full page carries a cursor");

        // Search reaches the needle even though it is not on page 1.
        let hit = operator_tenants_page(&operator_repo(&db), Some("ZZNEEDLE".into()), None, 2)
            .await
            .unwrap();
        assert!(
            hit.rows.iter().any(|r| r.id == needle),
            "case-insensitive search finds a later-page match"
        );
        assert!(
            hit.rows.iter().all(|r| r.slug.contains("zzneedle")),
            "non-matching tenants are excluded"
        );

        for t in all.drain(..) {
            cleanup(&db, t).await;
        }
    }

    /// AC-1/AC-2 for nodes: cursor + search on name/status.
    #[tokio::test]
    async fn nodes_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping nodes_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "nodes-host").await;
        let needle = node(&db, t, "edge-oddball", "online").await;
        for i in 0..4 {
            node(&db, t, &format!("worker{i}"), "offline").await;
        }

        let p1 = operator_nodes_page(&operator_repo(&db), None, None, 2)
            .await
            .unwrap();
        assert!(p1.rows.len() <= 2 && p1.next_cursor.is_some());

        // Search by (distinctive) name reaches the needle on a later page.
        let by_name = operator_nodes_page(&operator_repo(&db), Some("ODDBALL".into()), None, 2)
            .await
            .unwrap();
        assert!(by_name.rows.iter().any(|r| r.id == NodeId(needle)));
        // Search by status matches the whole set of that status.
        let online = operator_nodes_page(&operator_repo(&db), Some("online".into()), None, 50)
            .await
            .unwrap();
        assert!(online.rows.iter().all(|r| r.status == "online"));

        cleanup(&db, t).await;
    }

    /// AC-1/AC-2 for bindings: cursor + search on email/role.
    #[tokio::test]
    async fn bindings_page_cursors_and_searches() {
        let Some(db) = pool().await else {
            eprintln!("skipping bindings_page_cursors_and_searches — no DATABASE_URL");
            return;
        };
        let t = tenant(&db, "bind-host").await;
        // The needle binding is created first (oldest id → later page).
        let subj = user(&db, t, "needle@bind.test").await;
        let needle = binding(&db, subj, "operator").await;
        for i in 0..4 {
            let u = user(&db, t, &format!("filler{i}@bind.test")).await;
            binding(&db, u, "org_admin").await;
        }

        let p1 = operator_bindings_page(&operator_repo(&db), None, None, 2)
            .await
            .unwrap();
        assert!(p1.rows.len() <= 2 && p1.next_cursor.is_some());

        // Search by email reaches the needle beyond page 1.
        let by_email = operator_bindings_page(&operator_repo(&db), Some("NEEDLE@".into()), None, 2)
            .await
            .unwrap();
        assert!(by_email.rows.iter().any(|r| r.id == needle));
        // Search by role narrows to that role.
        let operators =
            operator_bindings_page(&operator_repo(&db), Some("operator".into()), None, 50)
                .await
                .unwrap();
        assert!(operators.rows.iter().all(|r| r.role_key == "operator"));

        cleanup(&db, t).await;
    }
}
