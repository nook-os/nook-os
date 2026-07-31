//! codex as the second real runtime (MAIN-291, C6).
//!
//! The card's claim is that adding a runtime is a *descriptor addition*. These
//! tests are written to make that claim falsifiable rather than asserted: the
//! flow below is driven by the SAME [`DeviceFlow`] the claude and fake-runtime
//! tests use, and the only codex-specific things in play are the descriptor and
//! its materializer.
//!
//! ## Where the expected `auth.json` shape comes from
//!
//! Measured against **codex-cli 0.145.0**, by writing candidate files into a
//! scratch `CODEX_HOME` and asking `codex login status`:
//!
//! | written                          | `codex login status`                     |
//! |----------------------------------|------------------------------------------|
//! | all three tokens                 | `Logged in using ChatGPT` (exit 0)       |
//! | no `refresh_token`               | `missing field ...` (exit 1)             |
//! | no `access_token`                | `missing field ...` (exit 1)             |
//! | no `id_token`                    | `missing field ...` (exit 1)             |
//! | `id_token` not a JWT             | `invalid ID token format` (exit 1)       |
//! | no `account_id` / `last_refresh` | `Logged in using ChatGPT` (exit 0)       |
//! | `{}`                             | `Logged in using ChatGPT` (exit 0)  ← !! |
//!
//! That last row is why the shape is asserted HERE rather than left to the
//! node's probe: an empty file satisfies `codex login status`, so "the probe
//! says authorized" is not on its own evidence that the materializer is right.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use nook_control::services::runtime_auth::{
    codex_descriptor, descriptor_for, materialize_codex_auth_json, DeviceFlow, RuntimeAuthError,
    TokenResponse,
};

// ── a mock provider, same shape the C1 tests use ────────────────────────────

struct Provider {
    token_replies: Vec<Value>,
    polls: AtomicUsize,
}

async fn device(State(_): State<Arc<Provider>>) -> Json<Value> {
    Json(json!({
        "device_code": "dev-secret",
        "user_code": "CODE-1234",
        "verification_uri": "https://provider.invalid/activate",
        "expires_in": 600,
        "interval": 1,
    }))
}

async fn token(State(p): State<Arc<Provider>>, _body: String) -> Json<Value> {
    let n = p.polls.fetch_add(1, Ordering::SeqCst);
    Json(
        p.token_replies
            .get(n)
            .or_else(|| p.token_replies.last())
            .cloned()
            .unwrap_or_else(|| json!({ "error": "authorization_pending" })),
    )
}

async fn provider(token_replies: Vec<Value>) -> String {
    let state = Arc::new(Provider {
        token_replies,
        polls: AtomicUsize::new(0),
    });
    let app = Router::new()
        .route("/device", post(device))
        .route("/token", post(token))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// A syntactically valid, entirely fake JWT. codex parses the id_token, so a
/// bare string is rejected — the structure matters, the contents do not.
fn fake_jwt() -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        b64.encode(br#"{"alg":"none","typ":"JWT"}"#),
        b64.encode(br#"{"sub":"fake","email":"nobody@example.invalid"}"#)
    )
}

fn token_response(raw: Value) -> TokenResponse {
    let mut t: TokenResponse = serde_json::from_value(raw.clone()).unwrap();
    t.raw = raw;
    t
}

// ── AC-1: the descriptor ────────────────────────────────────────────────────

/// Env-driven exactly as claude's is, and absent rather than guessed when the
/// operator has configured nothing (NG-3: no OpenAI endpoint ships here).
///
/// Serialized with the other env-touching test in this file: `set_var` is
/// process-global and these would otherwise race.
#[test]
fn the_descriptor_is_configured_by_env_and_absent_without_it() {
    let _g = env_lock();
    clear_codex_env();
    assert!(
        codex_descriptor().is_none(),
        "with no NOOK_CODEX_* set there is no descriptor to guess at"
    );
    assert!(descriptor_for("codex").is_none());

    set_codex_env();
    let d = codex_descriptor().expect("configured");
    assert_eq!(d.runtime, "codex");
    assert_eq!(d.device_authorization_endpoint, "http://127.0.0.1:1/device");
    assert_eq!(d.token_endpoint, "http://127.0.0.1:1/token");
    assert_eq!(d.client_id, "codex-test-client");
    assert!(
        d.scopes.contains("offline_access"),
        "codex needs a refresh_token on disk, so the default scopes must ask \
         for one: {}",
        d.scopes
    );
    assert!(
        descriptor_for("codex").is_some(),
        "descriptor_for must route codex (AC-1)"
    );
    clear_codex_env();
}

/// AC-6, stated as a regression: adding codex must not have disturbed claude.
#[test]
fn adding_codex_left_the_other_runtimes_alone() {
    let _g = env_lock();
    clear_codex_env();
    assert!(
        descriptor_for("hermes").is_none(),
        "hermes stays unsettled (NG-4)"
    );
    assert!(
        descriptor_for("nonesuch").is_none(),
        "an unknown runtime is still refused rather than guessed at"
    );
}

// ── AC-2: the materializer, against the measured contract ───────────────────

/// The happy path, asserted field by field against what codex-cli 0.145.0
/// accepts — not against a golden blob, so a failure says which field moved.
#[test]
fn the_credential_is_codexs_wrapped_shape_not_the_raw_token_response() {
    let jwt = fake_jwt();
    let bytes = materialize_codex_auth_json(&token_response(json!({
        "id_token": jwt,
        "access_token": "acc-1",
        "refresh_token": "ref-1",
        "token_type": "Bearer",
    })))
    .expect("a complete token response materializes");

    let v: Value = serde_json::from_slice(&bytes).expect("valid JSON");
    assert!(
        v.get("tokens").is_some(),
        "codex nests the tokens; a verbatim response would not have this key"
    );
    assert_eq!(v["tokens"]["id_token"], json!(jwt));
    assert_eq!(v["tokens"]["access_token"], json!("acc-1"));
    assert_eq!(v["tokens"]["refresh_token"], json!("ref-1"));
    assert_eq!(
        v["OPENAI_API_KEY"],
        Value::Null,
        "the API-key slot is present and null for an OAuth login"
    );
    assert!(
        v.get("last_refresh").and_then(Value::as_str).is_some(),
        "codex writes a last_refresh timestamp"
    );
    // The distinguishing assertion: this is NOT the provider's response.
    assert!(
        v.get("access_token").is_none(),
        "a top-level access_token would mean we shipped claude's verbatim shape"
    );
}

/// Every token codex requires is required here, and the error names the missing
/// one. Delivering an incomplete credential would surface much later, as a node
/// reporting "installed but still not authorized".
#[test]
fn a_token_response_missing_anything_codex_needs_is_refused_by_name() {
    let jwt = fake_jwt();
    let full = json!({ "id_token": jwt, "access_token": "a", "refresh_token": "r" });

    for missing in ["id_token", "access_token", "refresh_token"] {
        let mut body = full.clone();
        body.as_object_mut().unwrap().remove(missing);
        let err = materialize_codex_auth_json(&token_response(body))
            .expect_err("codex cannot work without it, so this must not materialize");
        let msg = err.to_string();
        assert!(
            msg.contains(missing),
            "the refusal must name {missing}, got: {msg}"
        );
    }

    // An empty string is the same problem wearing a different hat: codex would
    // parse the file and then have nothing to send.
    let err = materialize_codex_auth_json(&token_response(
        json!({ "id_token": jwt, "access_token": "", "refresh_token": "r" }),
    ))
    .expect_err("an empty access_token is not a credential");
    assert!(err.to_string().contains("access_token"));
}

/// `account_id` is optional to codex, so it is emitted only when the provider
/// actually sent one. Inventing an account identifier is worse than omitting a
/// field codex reads out of the id_token anyway.
#[test]
fn the_account_id_is_carried_only_when_the_provider_sent_one() {
    let jwt = fake_jwt();
    let base = json!({ "id_token": jwt, "access_token": "a", "refresh_token": "r" });

    let without: Value = serde_json::from_slice(
        &materialize_codex_auth_json(&token_response(base.clone())).unwrap(),
    )
    .unwrap();
    assert!(
        without["tokens"].get("account_id").is_none(),
        "no account_id from the provider means no invented one"
    );

    let mut with_account = base;
    with_account.as_object_mut().unwrap().insert(
        "account_id".into(),
        Value::String("acct_from_provider".into()),
    );
    let with: Value = serde_json::from_slice(
        &materialize_codex_auth_json(&token_response(with_account)).unwrap(),
    )
    .unwrap();
    assert_eq!(with["tokens"]["account_id"], json!("acct_from_provider"));
}

// ── AC-5 / AC-6: end to end through the unmodified driver ───────────────────

/// The card's real claim: a full device flow for codex, driven by the same
/// `DeviceFlow` as every other runtime, ending in bytes codex would accept.
///
/// If adding codex had required a driver change, this test could not have been
/// written without one.
#[tokio::test]
async fn codex_drives_the_unmodified_device_flow_to_a_usable_credential() {
    let jwt = fake_jwt();
    let base = provider(vec![
        json!({ "error": "authorization_pending" }),
        json!({
            "id_token": jwt,
            "access_token": "acc-e2e",
            "refresh_token": "ref-e2e",
            "token_type": "Bearer",
        }),
    ])
    .await;

    // A codex descriptor pointed at the mock: the shape `codex_descriptor()`
    // builds from env, with the operator's endpoints supplied here.
    let d = nook_control::services::runtime_auth::RuntimeAuthDescriptor {
        runtime: "codex",
        device_authorization_endpoint: format!("{base}/device"),
        token_endpoint: format!("{base}/token"),
        client_id: "codex-test-client".into(),
        scopes: "openid profile email offline_access".into(),
        poll_interval: Duration::from_millis(10),
        materialize: materialize_codex_auth_json,
    };

    let flow = DeviceFlow::new(d);
    let pending = flow.begin().await.expect("the provider issues a code");
    assert_eq!(pending.user_code, "CODE-1234");
    let cred = flow.wait(&pending).await.expect("approval yields a token");

    assert_eq!(cred.runtime, "codex", "the credential carries its runtime");
    let v: Value = serde_json::from_slice(&cred.payload).expect("auth.json is JSON");
    assert_eq!(v["tokens"]["access_token"], json!("acc-e2e"));
    assert_eq!(v["tokens"]["refresh_token"], json!("ref-e2e"));
    assert_eq!(v["tokens"]["id_token"], json!(jwt));
}

/// A provider that returns a token response codex cannot use fails the FLOW,
/// rather than delivering bytes that would install and then not work.
#[tokio::test]
async fn a_provider_that_omits_the_refresh_token_fails_before_delivery() {
    let base = provider(vec![json!({
        "id_token": fake_jwt(),
        "access_token": "acc-only",
    })])
    .await;
    let d = nook_control::services::runtime_auth::RuntimeAuthDescriptor {
        runtime: "codex",
        device_authorization_endpoint: format!("{base}/device"),
        token_endpoint: format!("{base}/token"),
        client_id: "c".into(),
        scopes: "openid".into(),
        poll_interval: Duration::from_millis(10),
        materialize: materialize_codex_auth_json,
    };

    let flow = DeviceFlow::new(d);
    let pending = flow.begin().await.unwrap();
    let err = flow
        .wait(&pending)
        .await
        .expect_err("a credential codex cannot use is not a credential");
    assert!(
        matches!(err, RuntimeAuthError::Provider(ref m) if m.contains("refresh_token")),
        "the provider failure should name what was missing, got: {err:?}"
    );
}

// ── AC-2: the behavioural proof, against the real codex-cli ─────────────────

/// The assertion the card actually asks for: real `codex login status`, run
/// against the bytes this materializer produced, reports authorized.
///
/// Every other test here encodes a shape I *measured*; this one re-derives it
/// from the tool, so a future codex release that changes the layout fails here
/// instead of silently shipping credentials nodes cannot use.
///
/// **Skips when codex is not installed, and that is not a silent pass**: CI's
/// containers have no codex, so this cannot be an unconditional test — but
/// `NOOK_REQUIRE_CODEX=1` turns absence into a failure, the same contract
/// `NOOK_REQUIRE_DB` has for Postgres. Run it where codex lives:
/// `NOOK_REQUIRE_CODEX=1 ./test.sh --host rust runtime_auth_codex`.
///
/// It never touches a real `~/.codex`: `CODEX_HOME` points at a scratch
/// directory, and the tokens are fake.
#[test]
fn the_materialized_credential_is_accepted_by_the_real_codex_cli() {
    let required = std::env::var("NOOK_REQUIRE_CODEX").is_ok();
    if std::process::Command::new("codex")
        .arg("--version")
        .output()
        .is_err()
    {
        assert!(
            !required,
            "NOOK_REQUIRE_CODEX is set but codex is not on PATH — this test \
             would have skipped silently and reported success"
        );
        return;
    }

    let bytes = materialize_codex_auth_json(&token_response(json!({
        "id_token": fake_jwt(),
        "access_token": "acc-behavioural",
        "refresh_token": "ref-behavioural",
    })))
    .expect("materializes");

    // Not under /tmp: codex refuses to create its helper binaries there and
    // says so loudly, which would be noise in this test's output.
    let home = std::path::PathBuf::from(std::env::var("HOME").expect("HOME"))
        .join(format!(".nook-291-codex-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&home).expect("scratch CODEX_HOME");
    std::fs::write(home.join("auth.json"), &bytes).expect("write auth.json");

    let out = std::process::Command::new("codex")
        .args(["login", "status"])
        .env("CODEX_HOME", &home)
        .output()
        .expect("run codex login status");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // codex-cli 0.145.0 writes `login status` to STDERR, not stdout — which is
    // why the node's adapter carries `status_on_stderr`. Reading only stdout
    // here is what first surfaced that, so both streams are examined and the
    // split is asserted below rather than quietly tolerated.
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        out.status.success(),
        "codex rejected the credential this materializer produced \
         (exit {:?}): {stdout}{stderr}",
        out.status.code(),
    );
    assert!(
        format!("{stdout}{stderr}")
            .to_lowercase()
            .contains("logged in"),
        "expected codex to report a login; stdout={stdout:?} stderr={stderr:?}"
    );
    // The fact the node depends on. If a future codex moves this to stdout the
    // adapter's `status_on_stderr: true` becomes wrong, and this says so.
    assert!(
        stdout.trim().is_empty() && stderr.to_lowercase().contains("logged in"),
        "codex 0.145.0 reports status on stderr; if that changed, \
         nook-node's ADAPTERS entry for codex needs status_on_stderr updated. \
         stdout={stdout:?} stderr={stderr:?}"
    );
}

// ── env helpers ─────────────────────────────────────────────────────────────

/// `set_var`/`remove_var` are process-global; these tests share one binary and
/// would otherwise race each other.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const CODEX_VARS: [&str; 4] = [
    "NOOK_CODEX_DEVICE_AUTH_ENDPOINT",
    "NOOK_CODEX_TOKEN_ENDPOINT",
    "NOOK_CODEX_CLIENT_ID",
    "NOOK_CODEX_SCOPES",
];

fn clear_codex_env() {
    for v in CODEX_VARS {
        std::env::remove_var(v);
    }
}

fn set_codex_env() {
    std::env::set_var(
        "NOOK_CODEX_DEVICE_AUTH_ENDPOINT",
        "http://127.0.0.1:1/device",
    );
    std::env::set_var("NOOK_CODEX_TOKEN_ENDPOINT", "http://127.0.0.1:1/token");
    std::env::set_var("NOOK_CODEX_CLIENT_ID", "codex-test-client");
}
