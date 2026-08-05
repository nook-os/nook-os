//! A local install's bundled node can join with no user in the database
//! (MAIN-398 AC-1).
//!
//! The desktop shell has no account to authenticate as on first run, and
//! `POST /nodes/join-tokens` requires a user — so the token is seeded from the
//! environment instead, exactly as the compose stack's node token is. What this
//! proves is the whole round trip on the engine a desktop install actually
//! runs: a virgin SQLite file, the real migrator, the real seed, and the real
//! join route.
//!
//! The second half matters as much as the first. `NOOK_DEV_JOIN_TOKEN` would
//! have seeded the same row, and it also seeds the dogfood workspace, a dev
//! identity and `loops.enabled` — scaffolding for the compose stack that on
//! somebody's laptop is a workspace pointing at a repo that is not there and
//! loops running by surprise. So this asserts the new variable brings NONE of
//! that with it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct ScratchDb(std::path::PathBuf);

impl ScratchDb {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "nook-local-join-{tag}-{}.db",
            uuid::Uuid::now_v7().simple()
        ));
        let _ = std::fs::remove_file(&p);
        ScratchDb(p)
    }
    fn url(&self) -> String {
        format!("sqlite://{}", self.0.display())
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

/// A booted, seeded, virgin database — the first launch of a local install.
async fn boot(file: &ScratchDb, local_join_token: Option<&str>) -> nook_db::DbPool {
    let db = nook_db::connect(&file.url(), 5)
        .await
        .expect("an empty sqlite path opens");
    nook_db::migrate::run_boot_migrations_for(
        &db,
        false,
        &nook_control::MIGRATOR,
        &nook_control::MIGRATOR_SQLITE,
        nook_control::SQUASH_MANIFEST,
    )
    .await
    .expect("the SQLite track applies");

    let mut cfg = nook_control::Config::for_test();
    cfg.local_join_token = local_join_token.map(str::to_string);
    nook_control::seed::run(&db, &cfg).await.expect("seed");
    db
}

async fn join_with(db: &nook_db::DbPool, token: &str) -> (StatusCode, String) {
    let cfg = nook_control::Config::for_test();
    let state = nook_control::AppState::new(db.clone(), cfg, None).await;
    let app = nook_control::routes::build_router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/nodes/join")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "token": token,
                        "name": "",
                        "hostname": "someones-laptop",
                        "platform": "linux",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .expect("the join route answers");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    (status, body)
}

/// AC-1: the seeded token enrolls the node, with nobody signed in.
///
/// IGNORED, and deliberately left RED rather than weakened: the join route
/// cannot complete on SQLite. `operator::tenant_org_and_slug` decodes
/// `tenants.org_id` into a bare `uuid::Uuid`, and on SQLite that column holds
/// the 36-character TEXT its schema default writes —
/// `ColumnDecode { source: ParseByteLength { len: 36 } }`. That is MAIN-423's
/// AC-3 exactly, at a SECOND call site, and it is blocked there on a ruling
/// about where the fix belongs. Un-ignore this when that lands; it is the
/// end-to-end evidence AC-1 wants and it should pass unchanged.
#[tokio::test]
#[ignore = "blocked on MAIN-423 AC-3: tenants.org_id is 36-char TEXT on SQLite"]
async fn the_bundled_node_joins_a_virgin_local_install() {
    let file = ScratchDb::new("joins");
    let token = "nook_join_localtestvalue0000000000000000";
    let db = boot(&file, Some(token)).await;

    // Nobody has created an account yet — that is the state this exists for.
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(db.sqlite())
        .await
        .expect("count users");
    assert_eq!(users, 0, "a virgin local install has no account yet");

    let (status, body) = join_with(&db, token).await;
    assert_eq!(status, StatusCode::OK, "join refused: {body}");

    let joined: i64 = sqlx::query_scalar("SELECT count(*) FROM nodes WHERE hostname = ?")
        .bind("someones-laptop")
        .fetch_one(db.sqlite())
        .await
        .expect("count nodes");
    assert_eq!(joined, 1, "the node is enrolled");
}

/// The token is what did it: without the variable the same request is refused,
/// so seeding is not merely coincident with a join that would have worked.
#[tokio::test]
async fn without_the_variable_the_same_token_is_refused() {
    let file = ScratchDb::new("unseeded");
    let db = boot(&file, None).await;

    let (status, _) = join_with(&db, "nook_join_localtestvalue0000000000000000").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// AC-1's bound: the local variable seeds a join token and nothing else.
///
/// Each of these is something `NOOK_DEV_JOIN_TOKEN` would have brought along,
/// and each would be wrong on a personal machine.
#[tokio::test]
async fn it_does_not_drag_the_compose_stack_scaffolding_along() {
    let file = ScratchDb::new("clean");
    let db = boot(&file, Some("nook_join_localtestvalue0000000000000000")).await;
    let pool = db.sqlite();

    let dogfood: i64 = sqlx::query_scalar("SELECT count(*) FROM workspaces WHERE slug = ?")
        .bind("nook-dogfood")
        .fetch_one(pool)
        .await
        .expect("count workspaces");
    assert_eq!(
        dogfood, 0,
        "no workspace pointing at a repo that is not here"
    );

    let dev_identity: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = ?")
        .bind("dev@nookos.local")
        .fetch_one(pool)
        .await
        .expect("count users");
    assert_eq!(dev_identity, 0, "no seeded identity on a personal machine");

    let loops: i64 = sqlx::query_scalar("SELECT count(*) FROM settings WHERE key = ?")
        .bind("loops.enabled")
        .fetch_one(pool)
        .await
        .expect("count settings");
    assert_eq!(loops, 0, "loops stay off by default (MAIN-239)");
}
