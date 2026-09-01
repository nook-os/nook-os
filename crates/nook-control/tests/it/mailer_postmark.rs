//! The Postmark HTTP provider, verified against a local mock endpoint that
//! captures the header and JSON body it receives — so the wire contract (the
//! `X-Postmark-Server-Token` header + the `{From,To,Subject,TextBody,HtmlBody}`
//! mapping) is exercised without touching the real Postmark API. No DB needed.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use nook_control::mailer::postmark::PostmarkMailer;
use nook_control::mailer::{Category, Mailer};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct Captured {
    token: Arc<Mutex<Option<String>>>,
    body: Arc<Mutex<Option<Value>>>,
}

/// A mock Postmark endpoint that records what it was sent and answers success.
async fn ok_handler(
    State(cap): State<Captured>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    *cap.token.lock().unwrap() = headers
        .get("X-Postmark-Server-Token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *cap.body.lock().unwrap() = Some(body);
    Json(json!({ "ErrorCode": 0, "Message": "OK" }))
}

async fn spawn(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn postmark_send_puts_the_token_and_json_on_the_wire() {
    let cap = Captured::default();
    let addr = spawn(
        Router::new()
            .route("/email", post(ok_handler))
            .with_state(cap.clone()),
    )
    .await;

    let mailer = PostmarkMailer::build(
        &format!("http://{addr}/email"),
        "server-token-xyz",
        "NookOS <no-reply@hein.network>",
    );

    mailer
        .send(
            "her@example.com",
            "Hi",
            "plain text",
            Some("<b>rich</b>"),
            Category::Transactional,
        )
        .await
        .expect("the mock endpoint answers success");

    assert_eq!(
        cap.token.lock().unwrap().as_deref(),
        Some("server-token-xyz"),
        "the server token rides in the X-Postmark-Server-Token header"
    );
    let body = cap.body.lock().unwrap().clone().expect("a body was posted");
    assert_eq!(body["From"], "NookOS <no-reply@hein.network>");
    assert_eq!(body["To"], "her@example.com");
    assert_eq!(body["Subject"], "Hi");
    assert_eq!(body["TextBody"], "plain text");
    assert_eq!(body["HtmlBody"], "<b>rich</b>");
}

/// Postmark can answer 200 with a non-zero ErrorCode; that is a failed send.
async fn err_handler() -> Json<Value> {
    Json(json!({ "ErrorCode": 406, "Message": "Inactive recipient" }))
}

#[tokio::test]
async fn postmark_treats_a_nonzero_errorcode_as_a_failure() {
    let addr = spawn(Router::new().route("/email", post(err_handler))).await;

    let mailer = PostmarkMailer::build(&format!("http://{addr}/email"), "t", "from@x.test");

    let err = mailer
        .send("x@y.test", "s", "b", None, Category::Transactional)
        .await
        .expect_err("a non-zero ErrorCode must surface as a send error");
    assert!(
        err.to_string().contains("406"),
        "the error names the Postmark code: {err}"
    );
}
