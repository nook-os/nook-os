//! Named secret items end to end through the real router (MAIN-625).
//!
//! The properties these pin are the ones a refactor can quietly break: that a
//! value never comes back out of a read (AC-4), that the three scopes are
//! stored and listed independently (AC-1), that a write is recorded without its
//! value (AC-10), and that the password-sealed `.env` path this deliberately
//! does not touch behaves exactly as it did (AC-9).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use nook_control::repo::secret_items::NewSecretItem;
use nook_control::services::secret_items;
use nook_db::dialect::time_math;
use nook_db::{params, Db};
use nook_testkit::TestBed;
use nook_types::*;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

/// The value every test writes and no read may ever contain.
const VALUE: &str = "hunter2-do-not-leak";

struct Fixture {
    tenant: TenantId,
    user: UserId,
    workspace: WorkspaceId,
    node: NodeId,
    cookie: Uuid,
}

async fn seed(bed: &TestBed) -> Fixture {
    let tenant = bed.tenant("m625").await;
    let (user, person) = bed.user(tenant, "owner").await;
    bed.db()
        .exec(
            "INSERT INTO tenant_members (id, tenant_id, principal_type, principal_id, role)
             VALUES ($1, $2, 'user', $3, 'owner')",
            params![Uuid::new_v4(), tenant, user],
        )
        .await
        .expect("grant");
    let sid = Uuid::new_v4();
    let expires = time_math(bed.db().engine()).now_plus_scaled("$4", "1 hour");
    bed.db()
        .exec(
            &format!(
                "INSERT INTO sessions_auth (id, user_id, tenant_id, expires_at)
                 VALUES ($1, $2, $3, {expires})"
            ),
            params![sid, user, tenant, 1_i32],
        )
        .await
        .expect("auth session");
    Fixture {
        tenant,
        user,
        workspace: bed.workspace(tenant).await,
        node: bed.node(tenant, person).await,
        cookie: sid,
    }
}

fn signed_in(method: &str, uri: &str, cookie: Uuid, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", format!("nook_session={cookie}"));
    match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

/// AC-1: insert, replace, list and delete, for all three scopes.
#[tokio::test]
async fn every_scope_stores_replaces_lists_and_deletes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    for (scope, scope_id) in [
        (SecretScope::Tenant, f.tenant.0),
        (SecretScope::Workspace, f.workspace.0),
        (SecretScope::Node, f.node.0),
    ] {
        let item =
            secret_items::set_item(&state, f.tenant, f.user, scope, scope_id, "API_KEY", VALUE)
                .await
                .expect("set");
        assert_eq!(item.scope, scope);
        assert_eq!(item.scope_id, scope_id);
    }

    // Three rows, one per scope: the unique key is (scope, scope_id, name), so
    // one name in three scopes is three items and not a conflict.
    let rows = state.secret_items.list(f.tenant).await.expect("list");
    assert_eq!(rows.len(), 3, "{rows:?}");

    // Replace: the same triple writes the value again rather than adding a row,
    // and `created_at` survives it — "when was this first set" is what an audit
    // asks, and a rotation must not answer "just now".
    let before = rows
        .iter()
        .find(|r| r.scope == "tenant")
        .expect("tenant row")
        .created_at;
    let replaced = secret_items::set_item(
        &state,
        f.tenant,
        f.user,
        SecretScope::Tenant,
        f.tenant.0,
        "API_KEY",
        "rotated",
    )
    .await
    .expect("replace");
    assert_eq!(replaced.created_at, before);
    assert_eq!(state.secret_items.list(f.tenant).await.unwrap().len(), 3);
    let stored = state
        .secret_items
        .get(f.tenant, SecretScope::Tenant, f.tenant.0, "API_KEY")
        .await
        .unwrap()
        .expect("still there");
    assert_eq!(
        state
            .vault
            .open_envelope(&stored.dek_wrapped, &stored.value_enc)
            .unwrap(),
        b"rotated"
    );

    // Delete removes one scope's item and leaves the others.
    assert!(state
        .secret_items
        .delete(f.tenant, SecretScope::Node, f.node.0, "API_KEY")
        .await
        .unwrap());
    assert!(
        !state
            .secret_items
            .delete(f.tenant, SecretScope::Node, f.node.0, "API_KEY")
            .await
            .unwrap(),
        "a second delete reports nothing was there"
    );
    assert_eq!(state.secret_items.list(f.tenant).await.unwrap().len(), 2);

    bed.teardown().await;
}

/// AC-2, against a real row: the stored ciphertext survives an app-key
/// rotation byte for byte, and the item still opens under the new key.
#[tokio::test]
async fn an_app_key_rotation_rewraps_without_touching_the_ciphertext() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    secret_items::set_item(
        &state,
        f.tenant,
        f.user,
        SecretScope::Workspace,
        f.workspace.0,
        "API_KEY",
        VALUE,
    )
    .await
    .expect("set");
    let before = state
        .secret_items
        .get(f.tenant, SecretScope::Workspace, f.workspace.0, "API_KEY")
        .await
        .unwrap()
        .expect("row");

    // The rotation itself: unwrap the data key with the old app key, wrap it
    // with the new one, write back only that column.
    let next = nook_control::crypto::Vault::from_key([9u8; 32]);
    let rewrapped = state.vault.rewrap(&before.dek_wrapped, &next).unwrap();
    bed.db()
        .exec(
            "UPDATE secret_items SET dek_wrapped = $2 WHERE tenant_id = $1 AND name = 'API_KEY'",
            params![f.tenant, rewrapped],
        )
        .await
        .expect("rewrap");

    let after = state
        .secret_items
        .get(f.tenant, SecretScope::Workspace, f.workspace.0, "API_KEY")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        after.value_enc, before.value_enc,
        "an app-key rotation must not rewrite the value's own ciphertext"
    );
    assert_ne!(after.dek_wrapped, before.dek_wrapped);
    assert_eq!(
        next.open_envelope(&after.dek_wrapped, &after.value_enc)
            .unwrap(),
        VALUE.as_bytes()
    );
    // …and the value is genuinely not readable from the ciphertext alone.
    assert!(!String::from_utf8_lossy(&after.value_enc).contains(VALUE));

    bed.teardown().await;
}

/// AC-3 and AC-4 through the real router: a set, a list, and nothing that
/// carries a value back.
#[tokio::test]
async fn the_listing_carries_names_and_times_and_never_a_value() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let app = nook_control::routes::build_router(bed.app_state().await);

    for (scope, scope_id) in [
        ("tenant", None),
        ("workspace", Some(f.workspace.0)),
        ("node", Some(f.node.0)),
    ] {
        let mut body = json!({"scope": scope, "name": "API_KEY", "value": VALUE});
        if let Some(id) = scope_id {
            body["scope_id"] = json!(id);
        }
        let res = app
            .clone()
            .oneshot(signed_in("PUT", "/api/v1/secrets", f.cookie, Some(body)))
            .await
            .expect("set");
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            !body_text(res).await.contains(VALUE),
            "a write must not echo the value"
        );
    }

    let res = app
        .clone()
        .oneshot(signed_in("GET", "/api/v1/secrets", f.cookie, None))
        .await
        .expect("list");
    assert_eq!(res.status(), StatusCode::OK);
    let text = body_text(res).await;
    assert!(text.contains("API_KEY"), "the name must be listed: {text}");
    assert!(
        text.contains("updated_at"),
        "updated_at must be listed: {text}"
    );
    assert!(
        !text.contains(VALUE),
        "AC-4: no read path returns a value: {text}"
    );

    // Narrowing by scope is the same listing, filtered — still no value.
    let res = app
        .clone()
        .oneshot(signed_in(
            "GET",
            "/api/v1/secrets?scope=workspace",
            f.cookie,
            None,
        ))
        .await
        .expect("list");
    let text = body_text(res).await;
    let rows: Vec<Value> = serde_json::from_str(&text).expect("json");
    assert_eq!(rows.len(), 1, "{text}");
    assert_eq!(rows[0]["scope"], "workspace");
    assert!(!text.contains(VALUE));

    // And the delete path, by the address the listing gives.
    let res = app
        .clone()
        .oneshot(signed_in(
            "DELETE",
            &format!("/api/v1/secrets/workspace/{}/API_KEY", f.workspace.0),
            f.cookie,
            None,
        ))
        .await
        .expect("delete");
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = app
        .clone()
        .oneshot(signed_in(
            "DELETE",
            &format!("/api/v1/secrets/workspace/{}/API_KEY", f.workspace.0),
            f.cookie,
            None,
        ))
        .await
        .expect("delete again");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    bed.teardown().await;
}

/// AC-8 through the endpoint: a real `.env` becomes items, in order, with the
/// bad line reported.
#[tokio::test]
async fn an_env_body_imports_one_item_per_assignment() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;
    let app = nook_control::routes::build_router(state.clone());

    let res = app
        .clone()
        .oneshot(signed_in(
            "POST",
            "/api/v1/secrets/import",
            f.cookie,
            Some(json!({
                "scope": "workspace",
                "scope_id": f.workspace.0,
                "content": "# a comment\n\nexport FIRST=one\nSECOND=\"two three\"\nnot an assignment\nTHIRD='four'\n",
            })),
        ))
        .await
        .expect("import");
    assert_eq!(res.status(), StatusCode::OK);
    let result: Value = serde_json::from_str(&body_text(res).await).expect("json");
    assert_eq!(
        result["imported"],
        json!(["FIRST", "SECOND", "THIRD"]),
        "in file order"
    );
    assert_eq!(result["problems"].as_array().unwrap().len(), 1);
    assert_eq!(result["problems"][0]["line"], 5);

    let stored = state
        .secret_items
        .get(f.tenant, SecretScope::Workspace, f.workspace.0, "SECOND")
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        state
            .vault
            .open_envelope(&stored.dek_wrapped, &stored.value_enc)
            .unwrap(),
        b"two three"
    );

    bed.teardown().await;
}

/// AC-10: the write is on the ledger, by name, and the value is not.
#[tokio::test]
async fn a_write_is_recorded_with_the_name_and_never_the_value() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    secret_items::set_item(
        &state,
        f.tenant,
        f.user,
        SecretScope::Workspace,
        f.workspace.0,
        "API_KEY",
        VALUE,
    )
    .await
    .expect("set");
    secret_items::record(
        &state,
        f.tenant,
        f.user,
        "secret.deleted",
        SecretScope::Workspace,
        f.workspace.0,
        "API_KEY",
    )
    .await;

    let events = state
        .read_model
        .events_page(nook_control::repo::read_model::EventsQuery {
            tenant: f.tenant,
            workspace: None,
            kind_prefix: Some("secret.".into()),
            before: None,
            limit: 50,
            scope: None,
        })
        .await
        .expect("events");
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"secret.set"), "{kinds:?}");
    assert!(kinds.contains(&"secret.deleted"), "{kinds:?}");
    for event in &events {
        assert_eq!(event.payload["name"], "API_KEY");
        assert_eq!(event.actor_id, Some(f.user.0));
        let rendered = serde_json::to_string(&event.payload).unwrap();
        assert!(
            !rendered.contains(VALUE),
            "an event must never carry a value: {rendered}"
        );
    }

    bed.teardown().await;
}

/// AC-9: a workspace holding a sealed `.env` and no items pushes exactly what
/// it pushed before — this ticket adds a second store, it does not change the
/// first.
#[tokio::test]
async fn a_sealed_env_is_unchanged_by_this_feature() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    bed.db()
        .exec(
            "INSERT INTO workspace_secrets
               (id, tenant_id, workspace_id, name, content_enc, kdf_salt, verifier)
             VALUES ($1, $2, $3, '.env', 'ciphertext', 'salt', 'verifier')",
            params![Uuid::now_v7(), f.tenant, f.workspace],
        )
        .await
        .expect("sealed");

    // The announce path's own guard: a workspace with a sealed blob still has
    // one, and the item store is invisible to it.
    assert!(state
        .workspace_secrets
        .any(f.tenant, f.workspace)
        .await
        .unwrap());
    let listed = state
        .workspace_secrets
        .list(f.tenant, f.workspace)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, ".env");
    // Sealed content is never in the listing, exactly as before.
    assert!(listed[0].content.is_none());

    // Adding items changes neither the sealed listing nor its count.
    secret_items::set_item(
        &state,
        f.tenant,
        f.user,
        SecretScope::Workspace,
        f.workspace.0,
        "API_KEY",
        VALUE,
    )
    .await
    .expect("set");
    let after = state
        .workspace_secrets
        .list(f.tenant, f.workspace)
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].name, ".env");
    // …and the other direction: the sealed blob is not an item.
    assert_eq!(state.secret_items.list(f.tenant).await.unwrap().len(), 1);

    bed.teardown().await;
}

/// AC-7 against real rows: what a workspace's session or job is handed comes
/// from the two scopes that inject, and a node item is not in it.
#[tokio::test]
async fn the_delivered_environment_holds_the_two_injecting_scopes() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    for (scope, scope_id, name, value) in [
        (SecretScope::Tenant, f.tenant.0, "FLEET_KEY", "fleet"),
        (SecretScope::Workspace, f.workspace.0, "REPO_KEY", "repo"),
        (SecretScope::Node, f.node.0, "NODE_KEY", "node"),
    ] {
        secret_items::set_item(&state, f.tenant, f.user, scope, scope_id, name, value)
            .await
            .expect("set");
    }

    let env = secret_items::env_for_workspace(&state, f.tenant, Some(f.workspace)).await;
    let names: Vec<&str> = env.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["FLEET_KEY", "REPO_KEY"], "{env:?}");
    assert_eq!(env[0].value, "fleet");
    assert_eq!(env[1].value, "repo");

    bed.teardown().await;
}

/// A stored value that will not open — the case an app-key change without a
/// re-wrap produces — is skipped, and does not take the rest of the tenant's
/// secrets (or the session start) down with it.
#[tokio::test]
async fn an_unreadable_item_is_skipped_rather_than_fatal() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = seed(&bed).await;
    let state = bed.app_state().await;

    secret_items::set_item(
        &state,
        f.tenant,
        f.user,
        SecretScope::Tenant,
        f.tenant.0,
        "GOOD",
        "readable",
    )
    .await
    .expect("set");
    // Sealed under an app key this deployment does not have.
    let stranger = nook_control::crypto::Vault::from_key([3u8; 32]);
    let envelope = stranger.seal_envelope(b"unreadable").unwrap();
    state
        .secret_items
        .put(NewSecretItem {
            tenant: f.tenant,
            scope: SecretScope::Tenant,
            scope_id: f.tenant.0,
            name: "BAD".into(),
            value_enc: envelope.ciphertext,
            dek_wrapped: envelope.wrapped_key,
            updated_by: None,
        })
        .await
        .expect("put");

    let env = secret_items::env_for_workspace(&state, f.tenant, Some(f.workspace)).await;
    assert_eq!(env.len(), 1, "{env:?}");
    assert_eq!(env[0].name, "GOOD");

    bed.teardown().await;
}

/// AC-6's wire half: the message that starts a loop job carries the two
/// injecting scopes' items, and no node-scoped one.
///
/// The node-side half is `loop_job::secret_env`'s unit test; between them the
/// chain from a stored row to the agent's environment is covered without
/// needing an authorized agent runtime. AC-5's live session proves the export
/// itself, which is the same `tmux::spawn` mechanism a tmux-adapter job uses.
#[tokio::test]
async fn a_dispatched_job_carries_its_workspaces_secrets() {
    use common::build_ports::*;

    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let f = fixture(&bed, "m625-wire").await;
    let node = build_node(&bed, &f, Some((4300, 4300))).await;
    let todo = column(&bed, &f, "Todo", "unstarted", 0).await;
    let task = card(&bed, &f, todo, 1).await;
    let job = claimed_build_job(&bed, &f, task, node).await;
    let state = bed.app_state().await;
    bed.db()
        .exec(
            "INSERT INTO node_workspaces (id, tenant_id, node_id, workspace_id, path,
                                          git_remote_url, git_branch)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
            params![
                Uuid::now_v7(),
                f.tenant,
                node,
                f.workspace,
                "/checkouts/x",
                "git@example.test:acme/repo.git",
                "main"
            ],
        )
        .await
        .expect("node_workspace");

    let (user, _) = (f.user, f.person);
    for (scope, scope_id, name, value) in [
        (SecretScope::Tenant, f.tenant.0, "FLEET_KEY", "fleet"),
        (SecretScope::Workspace, f.workspace.0, "REPO_KEY", VALUE),
        (SecretScope::Node, node.0, "NODE_KEY", "node-only"),
    ] {
        secret_items::set_item(&state, f.tenant, user, scope, scope_id, name, value)
            .await
            .expect("set");
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    state.registry.register_node(
        node,
        nook_control::ws::registry::NodeHandle {
            tenant_id: f.tenant,
            tx,
        },
    );
    let claimed = state
        .jobs
        .get(f.tenant, job)
        .await
        .expect("read back")
        .expect("job");
    nook_control::services::jobs::dispatch_to_node(&state, f.tenant, &claimed)
        .await
        .expect("dispatch");

    match rx.try_recv().expect("a RunLoopJob was sent") {
        nook_proto::ControlToNode::RunLoopJob { secrets, .. } => {
            assert_eq!(
                secrets,
                vec![
                    SecretEnv {
                        name: "FLEET_KEY".into(),
                        value: "fleet".into(),
                    },
                    SecretEnv {
                        name: "REPO_KEY".into(),
                        value: VALUE.into(),
                    },
                ],
                "the tenant's and the repo's items ride the dispatch, and the node's does not"
            );
        }
        other => panic!("expected RunLoopJob, got {other:?}"),
    }

    bed.teardown().await;
}
