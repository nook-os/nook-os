//! How a failure becomes an HTTP response, for both NookOS services.
//!
//! Today that is the panic safety net (MAIN-273); MAIN-274 brings the shared
//! `ApiError` here to retire nook-chat's `ChatError`. They belong in one crate
//! because the panic net's entire contract is that it emits the *same* body the
//! error mapping does — a shape agreed across two crates is a shape that
//! drifts.
//!
//! Shared, for the reason `nook-auth` gives for existing: a copy in each
//! service is two panic hooks, two thread-locals and two error bodies that
//! agree only until somebody edits one of them.
//!
//! A panic in a handler is not an `ApiError` — panics are not `Result`s — so it
//! bypasses each service's central error mapping entirely. Without a catch
//! layer the connection is dropped mid-response: the caller sees a transport
//! error rather than a status code, and the only record is whatever the default
//! panic hook happened to print. For a service someone else runs, one bug must
//! not look like a network fault.
//!
//! Two pieces, and they are separate on purpose:
//!
//! - [`install_panic_hook`] runs once at boot in each service. A panic payload carries the
//!   *message* but not the *location* — `Location` reaches only the hook — so
//!   the hook stashes it for the layer to pick up. It chains to the previous
//!   hook, so panics that never reach an HTTP handler (a background task, the
//!   dispatcher) keep printing exactly as they do today.
//! - [`panic_response`] is the layer's handler. It logs one structured record —
//!   message, location, backtrace — through `tracing::error!`, the same stream
//!   the services' `Internal` error variant writes to, and returns the same
//!   `{"error":"internal error"}` body a 500 already returns. The client learns
//!   nothing it would not have learned from an ordinary internal error.
//!
//! **Backtraces are off unless asked for.** `std::backtrace::Backtrace::capture`
//! honours `RUST_BACKTRACE`: unset, the log carries `backtrace=disabled` and
//! costs nothing; `RUST_BACKTRACE=1` (already set the usual way in the compose
//! stack or a pod spec) fills it in. Nothing here forces it on, because a
//! captured backtrace on a hot panic path is expensive and the operator, not
//! this module, decides.

use std::any::Any;
use std::backtrace::Backtrace;
use std::cell::RefCell;
use std::sync::Once;

use axum::body::Body;
use axum::http::{header, Response, StatusCode};
use serde_json::json;

/// Where the last panic on THIS thread happened, and what the runtime could
/// tell us about it.
///
/// Thread-local because that is exactly the scope that makes it correct: the
/// hook runs on the thread that panicked, and `catch_unwind` catches on that
/// same thread, so the value the layer reads is always the panic it is
/// handling. A process-global would race between tokio workers.
#[derive(Debug, Clone, Default)]
struct PanicContext {
    location: Option<String>,
    backtrace: Option<String>,
}

thread_local! {
    static LAST_PANIC: RefCell<Option<PanicContext>> = const { RefCell::new(None) };
}

static HOOK: Once = Once::new();

/// Install the hook that records panic location for [`panic_response`].
///
/// Idempotent — call it from `main` in either service; a second call is a no-op,
/// so a test that calls it too cannot chain the hook onto itself.
pub fn install_panic_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let ctx = PanicContext {
                location: info.location().map(|l| l.to_string()),
                backtrace: match Backtrace::capture().status() {
                    std::backtrace::BacktraceStatus::Captured => {
                        Some(Backtrace::force_capture().to_string())
                    }
                    _ => None,
                },
            };
            LAST_PANIC.with(|slot| *slot.borrow_mut() = Some(ctx));
            // Chain, so a panic that never reaches a handler still surfaces the
            // way it always has. Replacing the default outright would silence
            // every background-task panic in exchange for catching HTTP ones.
            previous(info);
        }));
    });
}

/// The message a panic payload carries, for the two shapes `panic!` produces.
///
/// `panic!("literal")` gives a `&'static str`; `panic!("{x}")` and
/// `.expect("…")` on a formatted message give a `String`. Anything else — a
/// `panic_any` with a custom type — has no printable form here, and saying so
/// is better than an empty string that reads like a panic with no message.
fn panic_message(err: &(dyn Any + Send)) -> String {
    if let Some(s) = err.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Turn a caught panic into the same 500 the central mapping returns.
///
/// The body is byte-identical to the services' `Internal` error variant's — deliberately, so a
/// client cannot tell a panic from any other internal error, and no panic
/// detail leaks. Everything useful goes to the log instead.
pub fn panic_response(err: Box<dyn Any + Send + 'static>) -> Response<Body> {
    let message = panic_message(err.as_ref());
    let ctx = LAST_PANIC
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default();

    tracing::error!(
        panic = %message,
        location = %ctx.location.as_deref().unwrap_or("unknown"),
        backtrace = %ctx
            .backtrace
            .as_deref()
            .unwrap_or("disabled (set RUST_BACKTRACE=1)"),
        "handler panicked — returning 500"
    );

    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "error": "internal error" }).to_string()))
        // The builder fails only on an invalid status or header, both of which
        // are literals here. A panic in the panic handler would be the one
        // thing this module exists to prevent.
        .expect("a constant status and header always build")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_lifted_from_both_panic_payload_shapes() {
        // `panic!("literal")`
        let s: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_message(s.as_ref()), "boom");
        // `panic!("{x}")` / `.expect("…")`
        let s: Box<dyn Any + Send> = Box::new("formatted boom".to_string());
        assert_eq!(panic_message(s.as_ref()), "formatted boom");
        // `panic_any(42)` — no printable form, and it says so rather than
        // reading as a panic with an empty message.
        let s: Box<dyn Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(s.as_ref()), "<non-string panic payload>");
    }

    #[test]
    fn the_response_is_the_same_500_the_central_mapping_returns() {
        let res = panic_response(Box::new("boom"));
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
    }
}

#[cfg(test)]
mod layer_tests {
    use super::*;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;
    use tower_http::catch_panic::CatchPanicLayer;

    // The unwrap IS the subject: clippy is right that it always panics, which
    // is exactly the bug this net has to survive.
    #[allow(clippy::unnecessary_literal_unwrap)]
    async fn boom() -> &'static str {
        let nothing: Option<u8> = None;
        let _ = nothing.expect("a handler bug");
        "unreachable"
    }

    /// The layer, end to end: a panicking handler answers with the generic 500
    /// instead of dropping the connection, and the message stays in the log.
    ///
    /// Both services mount this same layer, so proving it once here is what
    /// replaces the copy each of them used to carry.
    #[tokio::test]
    async fn a_panicking_handler_answers_with_the_generic_500() {
        let app = Router::new()
            .route("/boom", get(boom))
            .layer(CatchPanicLayer::custom(panic_response));

        let res = app
            .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
            .await
            .expect("the service answers rather than dropping the connection");
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .expect("a readable body — a dropped connection has none");
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(body, r#"{"error":"internal error"}"#);
        assert!(!body.contains("a handler bug"), "no detail leaks: {body}");
    }
}
