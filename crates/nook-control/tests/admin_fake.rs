//! Operator/admin callers against the in-memory fakes, with **no database at
//! all** (MAIN-258 AC-3).
//!
//! The rules pinned here are the ones whose failure is silent rather than loud:
//! a seeder that clobbers an operator's edit on every redeploy, a settings read
//! that hands one user another's row, a theme lookup that reaches into another
//! tenant, a keyset that skips or repeats a row at a page boundary, and a grant
//! that duplicates a binding.
//!
//! `cargo test -p nook-control --test admin_fake` passes with the database
//! stopped.

use nook_control::repo::admin::{
    FakeManagedContentRepository, FakeOperatorRepository, FakeSettingRepository,
    FakeSkillRepository, FakeThemeRepository, OperatorRepository, SettingRepository, SettingWrite,
    SkillRepository, TaughtSkill, ThemeRepository,
};
use nook_control::routes::managed::upsert_default;
use nook_control::services::operator_queries::{operator_audit_page, operator_tenants_page};
use nook_types::*;

/// The old call shape as a shim: these tests reason in (q, after, limit).
fn pq(q: Option<String>, after: Option<String>, limit: i64) -> PageQuery {
    PageQuery {
        q,
        after,
        limit: Some(limit),
        ..Default::default()
    }
}
use uuid::Uuid;

fn tenant() -> TenantId {
    TenantId::new()
}

// ── the seeder's three-way decision (MAIN-78 AC-2) ──────────────────────────

/// A fresh row installs at version 1 with the content equal to the default.
#[tokio::test]
async fn a_fresh_managed_row_installs_at_version_one() {
    let repo = FakeManagedContentRepository::new();
    upsert_default(&repo, "skill", "probe", "DEFAULT-V1")
        .await
        .unwrap();

    assert_eq!(
        repo.stored("skill", "probe"),
        Some(("DEFAULT-V1".to_string(), 1))
    );
}

/// Redeploying the SAME binary must leave an operator's edit alone. This is the
/// rule with the worst failure mode in the set: getting it wrong silently
/// reverts a deliberate change on every restart, and nothing reports it.
#[tokio::test]
async fn an_unchanged_default_preserves_an_operator_edit() {
    let repo = FakeManagedContentRepository::new();
    upsert_default(&repo, "skill", "probe", "DEFAULT-V1")
        .await
        .unwrap();
    repo.edit("skill", "probe", "OPERATOR EDIT");

    // Same shipped default, second boot.
    upsert_default(&repo, "skill", "probe", "DEFAULT-V1")
        .await
        .unwrap();

    assert_eq!(
        repo.stored("skill", "probe"),
        Some(("OPERATOR EDIT".to_string(), 1)),
        "an unchanged default is a no-op, not a revert"
    );
}

/// A NEWER shipped default is the one case that overwrites, and it bumps the
/// version so the change is visible.
#[tokio::test]
async fn a_newer_default_wins_and_bumps_the_version() {
    let repo = FakeManagedContentRepository::new();
    upsert_default(&repo, "skill", "probe", "DEFAULT-V1")
        .await
        .unwrap();
    repo.edit("skill", "probe", "OPERATOR EDIT");

    upsert_default(&repo, "skill", "probe", "DEFAULT-V2")
        .await
        .unwrap();

    assert_eq!(
        repo.stored("skill", "probe"),
        Some(("DEFAULT-V2".to_string(), 2)),
        "the shipped default moved, so it wins and the version records it"
    );
}

// ── settings: mine, never another user's ────────────────────────────────────

#[tokio::test]
async fn a_user_scoped_setting_is_invisible_to_another_user() {
    let repo = FakeSettingRepository::new();
    let t = tenant();
    let (me, you) = (UserId::new(), UserId::new());

    for (user, value) in [(me, "mine"), (you, "yours")] {
        repo.put(SettingWrite {
            tenant: t,
            scope: "user".into(),
            user: Some(user),
            key: "editor.theme".into(),
            value: serde_json::json!(value),
        })
        .await
        .unwrap();
    }
    repo.put(SettingWrite {
        tenant: t,
        scope: "tenant".into(),
        user: None,
        key: "loops.enabled".into(),
        value: serde_json::json!(true),
    })
    .await
    .unwrap();

    let mine = repo.visible_to(t, me).await.unwrap();
    let keys: Vec<&str> = mine.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["editor.theme", "loops.enabled"],
        "the tenant row plus exactly one user row"
    );
    assert_eq!(
        mine.iter().find(|s| s.key == "editor.theme").unwrap().value,
        serde_json::json!("mine"),
        "and it is the caller's own, not the other user's"
    );
}

/// Two users writing the same key must not overwrite each other — the upsert
/// key is the whole `(tenant, scope, user, key)` tuple, not just the key.
#[tokio::test]
async fn two_users_hold_the_same_setting_key_independently() {
    let repo = FakeSettingRepository::new();
    let t = tenant();
    let (me, you) = (UserId::new(), UserId::new());

    for (user, value) in [(me, "amber"), (you, "green")] {
        repo.put(SettingWrite {
            tenant: t,
            scope: "user".into(),
            user: Some(user),
            key: "theme".into(),
            value: serde_json::json!(value),
        })
        .await
        .unwrap();
    }

    assert_eq!(repo.visible_to(t, me).await.unwrap()[0].value, "amber");
    assert_eq!(repo.visible_to(t, you).await.unwrap()[0].value, "green");
}

// ── themes: built-ins plus mine, and nobody else's ──────────────────────────

#[tokio::test]
async fn themes_are_built_ins_plus_the_callers_own() {
    let repo = FakeThemeRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    repo.add("amber-crt", "Amber CRT", None);
    repo.add("house", "House Style", Some(mine));
    repo.add("secret", "Their Style", Some(theirs));

    let slugs: Vec<String> = repo
        .visible_to(mine)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.slug)
        .collect();
    assert_eq!(slugs, vec!["amber-crt", "house"]);

    assert!(repo.by_slug("amber-crt", mine).await.unwrap().is_some());
    assert!(repo.by_slug("house", mine).await.unwrap().is_some());
    assert!(
        repo.by_slug("secret", mine).await.unwrap().is_none(),
        "another tenant's theme is not found, not forbidden — it does not exist here"
    );
}

// ── skills: re-teaching replaces, forgetting reports what it removed ────────

#[tokio::test]
async fn re_teaching_a_skill_replaces_it_rather_than_adding_a_second() {
    let repo = FakeSkillRepository::new();
    let t = tenant();
    let author = Uuid::now_v7();

    for content in ["v1", "v2 much longer"] {
        repo.teach(TaughtSkill {
            tenant: t,
            name: "deploy".into(),
            content: content.into(),
            sha256: format!("sha-{content}"),
            updated_by: author,
        })
        .await
        .unwrap();
    }

    assert_eq!(repo.count(), 1, "one verb, one row");
    let stored = repo.get(t, "deploy").await.unwrap().unwrap();
    assert_eq!(stored.content, "v2 much longer");
    assert_eq!(
        repo.list(t).await.unwrap()[0].size,
        "v2 much longer".len() as i64
    );
}

#[tokio::test]
async fn forgetting_reports_whether_anything_was_removed() {
    let repo = FakeSkillRepository::new();
    let t = tenant();
    repo.teach(TaughtSkill {
        tenant: t,
        name: "deploy".into(),
        content: "body".into(),
        sha256: "sha".into(),
        updated_by: Uuid::now_v7(),
    })
    .await
    .unwrap();

    assert_eq!(repo.forget(t, "deploy").await.unwrap(), 1);
    assert_eq!(
        repo.forget(t, "deploy").await.unwrap(),
        0,
        "the caller 404s on this, so it must not report a phantom delete"
    );
    assert_eq!(
        repo.forget(tenant(), "deploy").await.unwrap(),
        0,
        "and another tenant cannot unteach this one's skill"
    );
}

/// A skill is a tenant's own; the fleet sync must never hand one tenant's
/// skills to another's node.
#[tokio::test]
async fn the_fleet_sync_payload_is_tenant_scoped() {
    let repo = FakeSkillRepository::new();
    let (mine, theirs) = (tenant(), tenant());
    for (t, name) in [(mine, "ours"), (theirs, "theirs")] {
        repo.teach(TaughtSkill {
            tenant: t,
            name: name.into(),
            content: "body".into(),
            sha256: "sha".into(),
            updated_by: Uuid::now_v7(),
        })
        .await
        .unwrap();
    }

    let names: Vec<String> = repo
        .payloads_for(mine)
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, vec!["ours"]);
}

// ── the console: keyset paging and grants ───────────────────────────────────

/// The cursor rule the four lists share: a FULL page carries a cursor, a short
/// one does not, and walking the cursor neither skips nor repeats a row.
#[tokio::test]
async fn a_console_page_walks_its_cursor_without_a_gap_or_an_overlap() {
    let repo = FakeOperatorRepository::new();
    let mut ids = Vec::new();
    for i in 0..5 {
        let id = TenantId::new();
        ids.push(id);
        repo.add_tenant_row(OperatorTenant {
            id,
            slug: format!("t{i}"),
            org_id: None,
            created_at: chrono::Utc::now(),
            members: 0,
            nodes: 0,
            active_sessions: 0,
            workspaces: 0,
            repositories: None,
            task_titles: None,
        });
    }

    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = operator_tenants_page(&repo, &pq(None, cursor, 2))
            .await
            .unwrap();
        seen.extend(page.rows.iter().map(|r| r.id));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }

    let mut expected = ids.clone();
    expected.sort_by_key(|t| std::cmp::Reverse(t.0)); // newest first
    assert_eq!(seen, expected, "every row exactly once, newest first");
}

/// A page short of `limit` ends the walk: no cursor, so a caller does not loop
/// forever on an empty tail.
#[tokio::test]
async fn a_short_page_carries_no_cursor() {
    let repo = FakeOperatorRepository::new();
    repo.add_tenant_row(OperatorTenant {
        id: TenantId::new(),
        slug: "only".into(),
        org_id: None,
        created_at: chrono::Utc::now(),
        members: 0,
        nodes: 0,
        active_sessions: 0,
        workspaces: 0,
        repositories: None,
        task_titles: None,
    });

    let page = operator_tenants_page(&repo, &pq(None, None, 50))
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert!(page.next_cursor.is_none());
}

/// An empty or whitespace-only search is "no filter", not "match the empty
/// string" — the search box clears to that and must show the whole log.
#[tokio::test]
async fn a_blank_search_is_no_filter() {
    let repo = FakeOperatorRepository::new();
    for kind in ["operator.read", "rbac.granted"] {
        repo.add_audit_row(OperatorAuditEntry {
            id: EventId::new(),
            kind: kind.into(),
            actor_type: Some("user".into()),
            actor_id: Some(Uuid::now_v7()),
            tenant_id: tenant(),
            tenant_slug: "acme".into(),
            occurred_at: chrono::Utc::now(),
        });
    }

    for blank in [Some("   ".to_string()), Some(String::new()), None] {
        let page = operator_audit_page(&repo, &pq(blank.clone(), None, 50))
            .await
            .unwrap();
        assert_eq!(
            page.rows.len(),
            2,
            "blank search {blank:?} filtered nothing"
        );
    }

    let hit = operator_audit_page(&repo, &pq(Some("RBAC".into()), None, 50))
        .await
        .unwrap();
    assert_eq!(hit.rows.len(), 1, "and a real term is case-insensitive");
}

/// Granting twice is not an error and does not duplicate the binding — the
/// `ON CONFLICT DO NOTHING` the operator console relies on to be re-runnable.
#[tokio::test]
async fn granting_the_same_role_twice_is_idempotent() {
    let repo = FakeOperatorRepository::new();
    let (subject, by) = (Uuid::now_v7(), Uuid::now_v7());

    for _ in 0..2 {
        repo.grant_deployment_role(subject, "operator", by)
            .await
            .unwrap();
    }
    assert_eq!(repo.roles_of(subject), vec!["operator".to_string()]);

    repo.revoke_deployment_role(subject, "operator")
        .await
        .unwrap();
    assert!(repo.roles_of(subject).is_empty());
    // Revoking what is not held is a no-op, not an error.
    repo.revoke_deployment_role(subject, "operator")
        .await
        .unwrap();
}

#[tokio::test]
async fn moving_a_tenant_reports_where_it_came_from_before_it_moves() {
    let repo = FakeOperatorRepository::new();
    let (from, to) = (Uuid::now_v7(), Uuid::now_v7());
    let t = TenantId::new();
    repo.add_tenant(t, Some(from), "acme");

    // The caller authorizes against BOTH ends, so it must be able to learn the
    // "from" before the move happens.
    let (before, slug) = repo.tenant_org_and_slug(t).await.unwrap().unwrap();
    assert_eq!(before, Some(from));
    assert_eq!(slug, "acme");

    repo.move_tenant_to_org(t, to).await.unwrap();
    assert_eq!(repo.org_of(t), Some(to));
}

#[tokio::test]
async fn a_missing_org_renames_to_none_rather_than_reporting_success() {
    let repo = FakeOperatorRepository::new();
    assert!(repo
        .rename_org(Uuid::now_v7(), "Ghost")
        .await
        .unwrap()
        .is_none());

    let made = repo.create_org("Acme", "acme").await.unwrap();
    let renamed = repo.rename_org(made.id, "Acme Two").await.unwrap().unwrap();
    assert_eq!(renamed.name, "Acme Two");
    assert_eq!(renamed.slug, "acme", "a rename does not re-slug");
}
