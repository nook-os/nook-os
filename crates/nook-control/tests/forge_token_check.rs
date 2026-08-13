//! MAIN-469: what a pasted forge token is held to before it is sealed.
//!
//! Verified against a local stub that answers the way GitHub does — the repo
//! read, and the two deliberately-malformed write probes — so the refusals can
//! be held to their wording without a real token, a real repository, or a
//! network. No DB needed.
//!
//! The stub decides by BEARER TOKEN rather than by path, because that is the
//! one thing that differs between the cases: the same three requests, answered
//! as GitHub answers them for a good token, a read-only one, a dead one, and a
//! repository the token cannot see.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use nook_control::services::forge::{GithubForge, Repo, TokenRefusal};

/// Tokens the stub recognizes, each standing for one configuration.
const GOOD: &str = "github_pat_good";
const READ_ONLY: &str = "github_pat_readonly";
const ISSUES_ONLY: &str = "github_pat_issues_only";
const DEAD: &str = "github_pat_dead";
const UNSEEN: &str = "github_pat_unseen";

/// Every request the stub was sent, so the check can be held to making no
/// write it did not intend — and to leaving nothing behind.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

fn token_of(headers: &HeaderMap) -> String {
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .to_string()
}

/// `GET /repos/{owner}/{repo}` — authentication, and whether the token can see
/// this repository at all.
async fn repo_read(State(seen): State<Seen>, headers: HeaderMap) -> (StatusCode, String) {
    seen.0.lock().unwrap().push("GET /repos".into());
    match token_of(&headers).as_str() {
        DEAD => (
            StatusCode::UNAUTHORIZED,
            r#"{"message":"Bad credentials"}"#.into(),
        ),
        UNSEEN => (StatusCode::NOT_FOUND, r#"{"message":"Not Found"}"#.into()),
        _ => (StatusCode::OK, r#"{"full_name":"acme/api"}"#.into()),
    }
}

/// The write probes. A token that may write is answered by the BODY being
/// invalid (422) — which is the whole point: nothing is created either way.
///
/// One handler per permission, because the permissions are what the two
/// endpoints stand for: `labels` is the Issues one, `pulls` the Pull requests
/// one, and a token can hold either without the other.
async fn labels_probe(
    state: State<Seen>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let may = matches!(token_of(&headers).as_str(), GOOD | ISSUES_ONLY);
    probed(state, "labels", body, may)
}

async fn pulls_probe(state: State<Seen>, headers: HeaderMap, body: String) -> (StatusCode, String) {
    let may = token_of(&headers) == GOOD;
    probed(state, "pulls", body, may)
}

fn probed(State(seen): State<Seen>, what: &str, body: String, may: bool) -> (StatusCode, String) {
    seen.0.lock().unwrap().push(format!("POST /{what} {body}"));
    if may {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"message":"Validation Failed"}"#.into(),
        )
    } else {
        (
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by personal access token"}"#.into(),
        )
    }
}

async fn stub() -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/repos/{owner}/{name}", get(repo_read))
        .route("/repos/{owner}/{name}/labels", post(labels_probe))
        .route("/repos/{owner}/{name}/pulls", post(pulls_probe))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, seen)
}

fn repo() -> Repo {
    Repo {
        owner: "acme".into(),
        name: "api".into(),
    }
}

async fn check(addr: SocketAddr, token: &str) -> Result<(), TokenRefusal> {
    GithubForge::from_token_at(&format!("http://{addr}"), token)
        .check_access(&repo())
        .await
}

#[tokio::test]
async fn a_sufficient_token_passes_and_creates_nothing() {
    let (addr, seen) = stub().await;
    check(addr, GOOD)
        .await
        .expect("a sufficient token is stored");

    // The probes are writes, and a write that could succeed would leave a
    // label (or a pull request) behind on somebody's repository every time a
    // token was pasted. They are sent with bodies GitHub cannot accept.
    let calls = seen.0.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![
            "GET /repos".to_string(),
            r#"POST /labels {"name":""}"#.to_string(),
            "POST /pulls {}".to_string(),
        ],
        "the read first, then one probe per permission"
    );
}

#[tokio::test]
async fn a_read_only_token_is_refused_naming_the_permission_it_lacks() {
    let (addr, _) = stub().await;
    let refusal = check(addr, READ_ONLY)
        .await
        .expect_err("a fine-grained token defaults to read-only, and that cannot post a verdict");
    let said = refusal.to_string();
    assert!(
        said.contains("Issues: write"),
        "the refusal names the missing permission: {said}"
    );
    assert!(said.contains("acme/api"), "and the repository: {said}");
    assert!(
        said.contains("Resource not accessible by personal access token"),
        "with GitHub's own refusal quoted: {said}"
    );
    assert!(
        said.contains("Pull requests: write") && said.contains("Contents: read"),
        "and the whole requirement, so the next paste is the right one: {said}"
    );
}

#[tokio::test]
async fn the_second_permission_is_checked_too() {
    // Issues alone posts the comment and the label; the reviewer also reads and
    // writes pull requests. A token that stopped at the first probe would be
    // stored half-checked, and fail later in exactly the way this card exists
    // to end.
    let (addr, _) = stub().await;
    let said = check(addr, ISSUES_ONLY)
        .await
        .expect_err("issues-only is not enough")
        .to_string();
    assert!(
        said.contains("Pull requests: write"),
        "the second probe names its own permission: {said}"
    );
}

#[tokio::test]
async fn a_dead_token_is_named_dead_rather_than_under_scoped() {
    let (addr, seen) = stub().await;
    let refusal = check(addr, DEAD).await.expect_err("401 is a refusal");
    assert_eq!(
        refusal,
        TokenRefusal::Rejected {
            repo: "acme/api".into()
        }
    );
    assert!(
        refusal.to_string().contains("invalid, expired or revoked"),
        "a dead token is re-issued, not re-scoped: {refusal}"
    );
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "and nothing is probed with a credential GitHub has already refused"
    );
}

#[tokio::test]
async fn a_repository_the_token_cannot_see_is_named_as_such() {
    let (addr, _) = stub().await;
    let said = check(addr, UNSEEN)
        .await
        .expect_err("404 on the repo read is a refusal")
        .to_string();
    assert!(
        said.contains("cannot see acme/api"),
        "the commonest fine-grained mistake is not listing the repo: {said}"
    );
}

#[tokio::test]
async fn a_forge_that_cannot_answer_does_not_refuse_the_paste() {
    // Nothing is listening on this port. GitHub being unreachable is not a
    // statement about the token, and refusing the paste for it would leave an
    // operator unable to configure anything during an outage.
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    check(dead_addr, GOOD)
        .await
        .expect("an unverifiable token is stored, not refused");
}
