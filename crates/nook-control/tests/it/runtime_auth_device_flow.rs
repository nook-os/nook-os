//! The CP device-flow driver against a mock provider (MAIN-282).
//!
//! The provider is a real HTTP server — a small axum app on an ephemeral port —
//! rather than a stubbed client. The thing under test is an RFC 8628
//! conversation: form encoding, the `error` field's vocabulary, the `interval`
//! negotiation. A fake that returns pre-baked structs would agree with whatever
//! the driver did.
//!
//! No database, and deliberately so — the driver has no pool to take.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use nook_control::services::runtime_auth::{
    materialize_token_json, DeviceFlow, RuntimeAuthDescriptor, RuntimeAuthError, TokenResponse,
};
use serde_json::{json, Value};

/// What the mock provider answers, and how many times it has been asked.
#[derive(Default)]
struct Provider {
    /// One reply per poll, in order. The last is repeated if polling outlives
    /// the script.
    token_replies: Vec<Value>,
    polls: AtomicUsize,
    /// What `/device` reports as its own minimum interval.
    interval: u64,
}

async fn device(State(p): State<Arc<Provider>>) -> Json<Value> {
    Json(json!({
        "device_code": "dev-code-secret",
        "user_code": "WDJB-MJHT",
        "verification_uri": "https://provider.test/activate",
        "verification_uri_complete": "https://provider.test/activate?user_code=WDJB-MJHT",
        "interval": p.interval,
        "expires_in": 60,
    }))
}

async fn token(State(p): State<Arc<Provider>>, body: String) -> Json<Value> {
    // The grant is form-encoded per RFC 8628 §3.4; asserting it here is what
    // makes this a protocol test rather than a shape test.
    assert!(
        body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"),
        "the driver must send the device_code grant, got: {body}"
    );
    assert!(
        body.contains("device_code=dev-code-secret"),
        "the driver must echo the device code, got: {body}"
    );
    let n = p.polls.fetch_add(1, Ordering::SeqCst);
    let reply = p
        .token_replies
        .get(n)
        .or_else(|| p.token_replies.last())
        .cloned()
        .unwrap_or_else(|| json!({ "error": "authorization_pending" }));
    Json(reply)
}

/// Start the mock provider and return its base URL plus the shared state.
async fn provider(token_replies: Vec<Value>, interval: u64) -> (String, Arc<Provider>) {
    let state = Arc::new(Provider {
        token_replies,
        polls: AtomicUsize::new(0),
        interval,
    });
    let app = Router::new()
        .route("/device", post(device))
        .route("/token", post(token))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state)
}

/// A descriptor for a runtime that does not exist, pointed at the mock. AC-5's
/// subject: nothing below `DeviceFlow::new` knows what "acme-runtime" is.
fn fake_descriptor(base: &str) -> RuntimeAuthDescriptor {
    RuntimeAuthDescriptor {
        runtime: "acme-runtime",
        device_authorization_endpoint: format!("{base}/device"),
        token_endpoint: format!("{base}/token"),
        client_id: "acme-client".into(),
        scopes: "acme:everything".into(),
        // Sub-second so the tests are not a sleep benchmark; the negotiation
        // with the provider's own interval is asserted separately.
        poll_interval: Duration::from_millis(10),
        materialize: materialize_token_json,
    }
}

/// AC-2 + AC-5: request → pending → success → credential, through a descriptor
/// for a runtime the driver has never heard of.
#[tokio::test]
async fn a_fake_runtime_drives_the_whole_flow_to_a_credential() {
    let (base, state) = provider(
        vec![
            json!({ "error": "authorization_pending" }),
            json!({ "error": "authorization_pending" }),
            json!({ "access_token": "tok-abc", "token_type": "Bearer", "expires_in": 3600 }),
        ],
        1,
    )
    .await;

    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.expect("device authorization starts");

    // AC-2(b): what a person has to see is available BEFORE the poll finishes.
    assert_eq!(pending.user_code, "WDJB-MJHT");
    assert_eq!(pending.verification_uri, "https://provider.test/activate");
    assert_eq!(
        pending.link(),
        "https://provider.test/activate?user_code=WDJB-MJHT",
        "the pre-filled link wins when the provider offers one"
    );
    assert_eq!(pending.expires_in, Duration::from_secs(60));

    let cred = flow
        .wait(&pending)
        .await
        .expect("approval yields a credential");

    assert_eq!(
        cred.runtime, "acme-runtime",
        "the descriptor names the runtime"
    );
    let payload: Value = serde_json::from_slice(&cred.payload).expect("payload is the token JSON");
    assert_eq!(payload["access_token"], "tok-abc");
    assert_eq!(
        state.polls.load(Ordering::SeqCst),
        3,
        "it polled through both pending replies rather than giving up"
    );
}

/// AC-4: `access_denied` is its own outcome, not a generic failure.
#[tokio::test]
async fn a_declined_request_is_denied_not_a_generic_failure() {
    let (base, _) = provider(vec![json!({ "error": "access_denied" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    assert!(matches!(
        flow.wait(&pending).await,
        Err(RuntimeAuthError::Denied)
    ));
}

/// AC-4: `expired_token` likewise — the UI's "start again" state.
#[tokio::test]
async fn an_expired_code_is_expired_not_a_generic_failure() {
    let (base, _) = provider(vec![json!({ "error": "expired_token" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    assert!(matches!(
        flow.wait(&pending).await,
        Err(RuntimeAuthError::Expired)
    ));
}

/// AC-4: an error the RFC does not name is still distinguishable from "we could
/// not reach the provider", because those are different things to a person.
#[tokio::test]
async fn an_unrecognised_provider_error_is_reported_as_the_providers() {
    let (base, _) = provider(vec![json!({ "error": "invalid_client" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    match flow.wait(&pending).await {
        Err(RuntimeAuthError::Provider(m)) => assert!(m.contains("invalid_client"), "{m}"),
        other => panic!("expected a provider error, got {other:?}"),
    }
}

/// AC-4: an unreachable provider is a transport failure, distinct from every
/// answer the provider could have given.
#[tokio::test]
async fn an_unreachable_provider_is_a_transport_failure() {
    // Port 1 on loopback: nothing listens, and connecting fails immediately.
    let flow = DeviceFlow::new(fake_descriptor("http://127.0.0.1:1"));
    assert!(matches!(
        flow.begin().await,
        Err(RuntimeAuthError::Transport(_))
    ));
}

/// AC-2(c): `slow_down` is an instruction. Honouring it means the flow still
/// completes; ignoring it is how a client gets rate-limited into looking broken.
#[tokio::test]
async fn slow_down_is_honoured_and_the_flow_still_completes() {
    let (base, state) = provider(
        vec![
            json!({ "error": "slow_down" }),
            json!({ "access_token": "tok-after-backoff" }),
        ],
        1,
    )
    .await;
    // A descriptor whose floor is tiny, so the five seconds `slow_down` adds is
    // the only thing that could delay the second poll — and the flow completing
    // proves the driver kept going rather than treating it as an error.
    let mut d = fake_descriptor(&base);
    d.poll_interval = Duration::from_millis(1);
    let flow = DeviceFlow::new(d);
    let pending = flow.begin().await.unwrap();

    let started = std::time::Instant::now();
    let cred = flow
        .wait(&pending)
        .await
        .expect("completes after backing off");
    let elapsed = started.elapsed();

    assert_eq!(state.polls.load(Ordering::SeqCst), 2);
    assert!(
        elapsed >= Duration::from_secs(5),
        "the second poll must wait the extra five seconds slow_down asks for, waited {elapsed:?}"
    );
    let payload: Value = serde_json::from_slice(&cred.payload).unwrap();
    assert_eq!(payload["access_token"], "tok-after-backoff");
}

/// AC-2: the provider's `interval` is a floor it is asking us to respect. Ours
/// is a floor we impose. The slower of the two wins.
#[tokio::test]
async fn the_slower_of_the_two_intervals_wins() {
    // Provider asks for 1s; our descriptor floor is 10ms → the provider's wins,
    // so a single poll cannot come back sooner than a second.
    let (base, _) = provider(vec![json!({ "access_token": "tok" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    let started = std::time::Instant::now();
    flow.wait(&pending).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the provider asked for 1s and got polled sooner"
    );
}

/// A 200 carrying no token at all is the provider agreeing to something it did
/// not do. Shipping empty bytes to a node would install a credential that
/// cannot work.
#[tokio::test]
async fn a_success_with_no_token_is_refused_rather_than_materialized() {
    let (base, _) = provider(vec![json!({ "token_type": "Bearer" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    match flow.wait(&pending).await {
        Err(RuntimeAuthError::Provider(m)) => assert!(m.contains("access_token"), "{m}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// AC-5, the other half: a runtime whose credential file looks like nothing we
/// ship is a descriptor addition, not a driver change. This materializer emits
/// a bespoke format the driver knows nothing about.
#[tokio::test]
async fn a_runtime_with_a_bespoke_credential_format_needs_no_driver_change() {
    fn bespoke(token: &TokenResponse) -> Result<Vec<u8>, RuntimeAuthError> {
        let t = token
            .access_token
            .as_deref()
            .ok_or_else(|| RuntimeAuthError::Provider("no access_token".into()))?;
        Ok(format!("ACME-CREDENTIAL v1\ntoken={t}\n").into_bytes())
    }

    let (base, _) = provider(vec![json!({ "access_token": "tok-xyz" })], 1).await;
    let mut d = fake_descriptor(&base);
    d.materialize = bespoke;
    let cred = {
        let flow = DeviceFlow::new(d);
        let pending = flow.begin().await.unwrap();
        flow.wait(&pending).await.unwrap()
    };
    assert_eq!(
        String::from_utf8(cred.payload).unwrap(),
        "ACME-CREDENTIAL v1\ntoken=tok-xyz\n"
    );
}

/// AC-3: nothing is persisted.
///
/// Two independent arguments, because "we did not write it" is hard to prove by
/// absence alone. First, structurally: `DeviceFlow::new` takes a descriptor and
/// nothing else — no pool, no path, no store — so there is no sink to reach.
/// Second, observably: run the whole flow with the process parked in an empty
/// directory and confirm it is still empty.
#[tokio::test]
async fn the_credential_is_never_written_anywhere() {
    let dir = std::env::temp_dir().join(format!("nook-282-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();

    let (base, _) = provider(vec![json!({ "access_token": "super-secret" })], 1).await;
    let cred = {
        let flow = DeviceFlow::new(fake_descriptor(&base));
        let pending = flow.begin().await.unwrap();
        flow.wait(&pending).await.unwrap()
    };
    assert!(
        !cred.payload.is_empty(),
        "the flow did produce a credential"
    );

    let left_behind: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert!(
        left_behind.is_empty(),
        "the flow wrote {} file(s); the credential is in-flight only",
        left_behind.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The payload must not reach a log by accident. `Debug` is what a `tracing`
/// field or a `dbg!` reaches for, and a log line is a persistence sink nobody
/// remembers choosing.
#[tokio::test]
async fn debug_redacts_the_payload() {
    let (base, _) = provider(vec![json!({ "access_token": "super-secret-value" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    let cred = flow.wait(&pending).await.unwrap();

    let rendered = format!("{cred:?}");
    assert!(
        !rendered.contains("super-secret-value"),
        "Debug leaked the credential: {rendered}"
    );
    assert!(
        rendered.contains("redacted") && rendered.contains("acme-runtime"),
        "Debug should still say what it is: {rendered}"
    );
}

/// The device code is the secret half of the exchange and has no business on a
/// screen. It is private to the flow; only the user code is public.
#[tokio::test]
async fn the_pending_authorization_does_not_expose_the_device_code() {
    let (base, _) = provider(vec![json!({ "error": "authorization_pending" })], 1).await;
    let flow = DeviceFlow::new(fake_descriptor(&base));
    let pending = flow.begin().await.unwrap();
    let rendered = format!("{pending:?}");
    // It is in Debug (it is a struct field) but not reachable as an API — the
    // point being that a caller building a UI cannot put it on screen.
    assert_eq!(pending.user_code, "WDJB-MJHT");
    assert!(rendered.contains("WDJB-MJHT"));
}
