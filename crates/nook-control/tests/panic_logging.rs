//! The panic log record (MAIN-273 AC-2/AC-3).
//!
//! Its own test binary, deliberately. `tracing` caches a callsite's interest
//! process-wide the first time it is hit: if a sibling test in the same binary
//! drives a panic before a subscriber exists, the `tracing::error!` inside
//! `panic_response` is cached as "nobody is listening" and never fires again,
//! however many subscribers a later test installs. Isolating this one is the
//! difference between asserting the log and asserting an empty string.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::TraceLayer;

/// A real bug, not a deliberate `panic!` — the shape this card exists for.
// The unwrap IS the subject: clippy is right that it always panics, which
// is exactly the bug this net has to survive.
#[allow(clippy::unnecessary_literal_unwrap)]
async fn panic_unwrap() -> &'static str {
    let nothing: Option<u8> = None;
    let _ = nothing.expect("a value that was not there");
    "unreachable"
}

fn app() -> Router {
    Router::new()
        .route("/panic-unwrap", get(panic_unwrap))
        .layer(CatchPanicLayer::custom(nook_errors::panic_response))
        .layer(TraceLayer::new_for_http())
}

/// Collect formatted events so the test can assert what an operator would see.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

impl<S> tracing_subscriber::Layer<S> for Capture
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visit(String);
        impl tracing::field::Visit for Visit {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.push_str(&format!("{}={:?} ", f.name(), v));
            }
        }
        let mut v = Visit(String::new());
        event.record(&mut v);
        self.0.lock().unwrap().push(v.0);
    }
}

/// AC-2/AC-3: the panic reaches the log with its message and its location, in
/// the same stream `ApiError::Internal` writes to.
#[tokio::test]
async fn the_panic_is_logged_with_its_message_and_location() {
    use tracing_subscriber::layer::SubscriberExt;

    // The hook is what makes `location` available at all — a panic payload
    // carries the message but never the `Location`.
    nook_errors::install_panic_hook();

    let capture = Capture::default();
    // `set_default` rather than `with_default`: it returns a guard scoped to
    // this thread, which the request can be awaited inside. `with_default`
    // takes a closure and would need a nested runtime, which tokio refuses.
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));

    let res = app()
        .oneshot(
            Request::builder()
                .uri("/panic-unwrap")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("the service answers rather than dropping the connection");
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let events = capture.0.lock().unwrap().join("\n");
    assert!(
        events.contains("a value that was not there"),
        "the panic message must be in the log, got:\n{events}"
    );
    assert!(
        events.contains("panic_logging.rs"),
        "the panic LOCATION must be in the log — that is what the hook is for. Got:\n{events}"
    );
    assert!(
        events.contains("backtrace"),
        "the record carries a backtrace field (or says it is disabled), got:\n{events}"
    );
}
