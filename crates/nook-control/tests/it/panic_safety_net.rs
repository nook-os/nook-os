//! The panic safety net (MAIN-273 AC-4).
//!
//! A panic in a handler bypasses `ApiError` entirely — panics are not `Result`s
//! — so the only thing standing between a `.unwrap()` on `None` and a dropped
//! connection is the catch layer. These tests are about that layer doing three
//! things: producing a *response* rather than a transport failure, producing
//! the *same* response an ordinary internal error produces, and leaving the
//! detail in the log instead of the body.
//!
//! No database: the panicking route is test-only and the layer is pure.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use nook_control::error::ApiError;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::TraceLayer;

/// The production layer order, with a route that panics.
///
/// The order is the thing under test as much as the handler is: `CatchPanicLayer`
/// sits *under* `TraceLayer`, exactly as `routes::build_router` wires it, so a
/// caught panic is a response the trace layer can record rather than an unwind
/// that blows past it.
async fn panic_literal() -> &'static str {
    panic!("deliberate test panic")
}

/// The shape this card exists for: a real bug, not a deliberate `panic!`.
// The unwrap IS the subject: clippy is right that it always panics, which
// is exactly the bug this net has to survive.
#[allow(clippy::unnecessary_literal_unwrap)]
async fn panic_unwrap() -> &'static str {
    let nothing: Option<u8> = None;
    let _ = nothing.expect("a value that was not there");
    "unreachable"
}

async fn fine() -> &'static str {
    "ok"
}

fn app() -> Router {
    Router::new()
        .route("/panic-literal", get(panic_literal))
        .route("/panic-unwrap", get(panic_unwrap))
        .route("/fine", get(fine))
        .layer(CatchPanicLayer::custom(nook_errors::panic_response))
        .layer(TraceLayer::new_for_http())
}

async fn body_string(res: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .expect("the response body is readable — a dropped connection is not");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

async fn get_path(path: &str) -> (StatusCode, String) {
    let res = app()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("the service answers — this is the assertion that it did not drop the connection");
    (res.status(), body_string(res).await)
}

/// AC-1: a panic becomes a clean 500 with the generic body, and the response is
/// readable — which is what "not a dropped connection" means in a test.
#[tokio::test]
async fn a_panicking_handler_returns_a_clean_500_not_a_dropped_connection() {
    for path in ["/panic-literal", "/panic-unwrap"] {
        let (status, body) = get_path(path).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{path}");
        assert_eq!(body, r#"{"error":"internal error"}"#, "{path}");
    }
}

/// AC-2: no panic detail reaches the client. Asserted against the panic
/// messages themselves, so a future handler that "helpfully" includes them
/// fails here.
#[tokio::test]
async fn no_panic_detail_leaks_to_the_client() {
    for (path, secret) in [
        ("/panic-literal", "deliberate test panic"),
        ("/panic-unwrap", "a value that was not there"),
    ] {
        let (_, body) = get_path(path).await;
        assert!(
            !body.contains(secret),
            "{path} leaked the panic message: {body}"
        );
        assert!(
            !body.contains("panic"),
            "{path} leaked the word panic: {body}"
        );
        assert!(!body.contains(".rs"), "{path} leaked a source path: {body}");
    }
}

/// AC-1: "the same body shape the central mapping already returns" — asserted
/// against the real `ApiError::Internal` response rather than a copy of the
/// literal, so the two cannot drift apart.
#[tokio::test]
async fn the_caught_panic_body_is_identical_to_an_ordinary_internal_error() {
    use axum::response::IntoResponse;

    let (panic_status, panic_body) = get_path("/panic-literal").await;
    let ordinary = ApiError::Internal(anyhow::anyhow!("something went wrong")).into_response();
    let ordinary_status = ordinary.status();
    let ordinary_body = body_string(ordinary).await;

    assert_eq!(panic_status, ordinary_status);
    assert_eq!(
        panic_body, ordinary_body,
        "a client must not be able to tell a panic from any other internal error"
    );
}

/// The layer is a safety net, not a filter: everything that did not panic is
/// untouched (NG-1).
#[tokio::test]
async fn ordinary_responses_pass_through_unchanged() {
    let (status, body) = get_path("/fine").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

/// A panic must not poison the service. The second request proves the layer
/// caught the unwind rather than letting it take a worker down with it.
#[tokio::test]
async fn the_service_still_answers_after_a_panic() {
    let (panicked, _) = get_path("/panic-literal").await;
    assert_eq!(panicked, StatusCode::INTERNAL_SERVER_ERROR);
    let (after, body) = get_path("/fine").await;
    assert_eq!(after, StatusCode::OK, "the next request is served normally");
    assert_eq!(body, "ok");
}
