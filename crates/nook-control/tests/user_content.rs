//! MAIN-532: the user-content store — upload, serve, cap, delete.
//!
//! Handlers are driven directly, as the rest of this suite does, with two
//! exceptions that go through the real router because the thing under test IS
//! the wiring: that an unauthenticated GET is refused (the extractor, not the
//! handler, decides that), and that the upload route is mounted at all.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{FromRequest, Multipart, Path, State};
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use nook_control::auth::{AuthCtx, Principal};
use nook_control::routes::user_content::{delete, serve, upload};
use nook_control::storage::{ArtifactStore, ObjectMeta};
use nook_control::AppState;
use nook_testkit::TestBed;
use nook_types::*;
use sha2::Digest;
use tower::ServiceExt;
use uuid::Uuid;

const BOUNDARY: &str = "nook-test-boundary";

fn ctx(user: UserId, tenant: TenantId) -> AuthCtx {
    AuthCtx {
        session_id: AuthSessionId(Uuid::nil()),
        user_id: user,
        tenant_id: tenant,
        principal: Principal::User,
        cookie_session: true,
    }
}

/// A private disk root per test, so one test's objects are never another's.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "nook-user-content-{tag}-{}",
            Uuid::now_v7().simple()
        ));
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn state_on(bed: &TestBed, scratch: &Scratch) -> AppState {
    let mut cfg = bed.config();
    cfg.dist_dir = scratch.0.to_string_lossy().into_owned();
    AppState::new(bed.db(), cfg, None).await
}

fn multipart_bytes(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn multipart(filename: &str, content_type: &str, bytes: &[u8]) -> Multipart {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/user-content")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_bytes(filename, content_type, bytes)))
        .expect("a multipart request");
    Multipart::from_request(req, &()).await.expect("multipart")
}

async fn body_of(res: Response) -> Vec<u8> {
    axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("a readable body")
        .to_vec()
}

async fn put(
    state: &AppState,
    who: AuthCtx,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> UserContent {
    let res = upload(
        State(state.clone()),
        who,
        multipart(filename, content_type, bytes).await,
    )
    .await
    .expect("the upload succeeds");
    assert_eq!(res.status(), StatusCode::CREATED);
    serde_json::from_slice(&body_of(res).await).expect("a UserContent record")
}

fn header_of(res: &Response, name: header::HeaderName) -> String {
    res.headers()
        .get(&name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// AC-1 + AC-2 + AC-4's default: the bytes come back byte for byte, the record
/// describes them, and a disk-backed deployment streams rather than redirects.
#[tokio::test]
async fn an_upload_round_trips_through_the_disk_store() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("roundtrip");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("content").await;
    let (user, _) = bed.user(tenant, "member").await;

    let payload = b"%PDF-1.4 not really a pdf, but bytes are bytes".to_vec();
    let rec = put(
        &state,
        ctx(user, tenant),
        "report.pdf",
        "application/pdf",
        &payload,
    )
    .await;

    assert_eq!(rec.filename, "report.pdf");
    assert_eq!(rec.content_type, "application/pdf");
    assert_eq!(rec.size_bytes, payload.len() as i64);
    assert_eq!(
        rec.sha256,
        format!("{:x}", sha2::Sha256::digest(&payload)),
        "the checksum is over the bytes actually stored"
    );

    let res = serve(State(state.clone()), ctx(user, tenant), Path(rec.id))
        .await
        .expect("the fetch succeeds");
    assert_eq!(res.status(), StatusCode::OK, "a disk store always streams");
    assert_eq!(header_of(&res, header::CONTENT_TYPE), "application/pdf");
    assert_eq!(body_of(res).await, payload);

    // The bytes really are in the store, under a tenant-scoped key (AC-1).
    let row = state
        .user_content
        .get(rec.id, tenant)
        .await
        .expect("the row reads")
        .expect("the row exists");
    assert!(
        row.storage_key.contains(&tenant.0.to_string()),
        "the key is tenant-scoped: {}",
        row.storage_key
    );
    assert_eq!(
        state.artifacts.get(&row.storage_key).await.unwrap(),
        payload
    );

    bed.teardown().await;
}

/// AC-3: another tenant's id is a 404, not a 403 — a 403 would confirm the id
/// exists, which is the probe this forbids.
#[tokio::test]
async fn another_tenants_id_is_not_found_rather_than_forbidden() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("cross-tenant");
    let state = state_on(&bed, &scratch).await;
    let mine = bed.tenant("mine").await;
    let theirs = bed.tenant("theirs").await;
    let (me, _) = bed.user(mine, "member").await;
    // An OWNER in the other tenant: the refusal is about the tenant boundary,
    // not about the caller's role.
    let (them, _) = bed.user(theirs, "owner").await;

    let rec = put(&state, ctx(me, mine), "secret.txt", "text/plain", b"mine").await;

    let err = serve(State(state.clone()), ctx(them, theirs), Path(rec.id))
        .await
        .expect_err("another tenant may not read it");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        StatusCode::NOT_FOUND
    );

    let err = delete(State(state.clone()), ctx(them, theirs), Path(rec.id))
        .await
        .expect_err("nor delete it");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        StatusCode::NOT_FOUND
    );

    bed.teardown().await;
}

/// AC-3's other half: no session, no bytes. Driven through the real router,
/// because it is the extractor on the mounted route that decides this.
#[tokio::test]
async fn a_signed_out_caller_gets_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("anon");
    let state = state_on(&bed, &scratch).await;
    let router = nook_control::routes::build_router(state);

    let res = router
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/user-content/{}", Uuid::now_v7()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("the router answers");
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    bed.teardown().await;
}

/// AC-6: over the cap is refused as a typed error, and nothing is stored.
#[tokio::test]
async fn an_oversized_upload_is_refused_and_stores_nothing() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("oversize");
    let mut cfg = bed.config();
    cfg.dist_dir = scratch.0.to_string_lossy().into_owned();
    cfg.user_content_max_bytes = 1024;
    let state = AppState::new(bed.db(), cfg, None).await;
    let tenant = bed.tenant("cap").await;
    let (user, _) = bed.user(tenant, "member").await;

    let err = upload(
        State(state.clone()),
        ctx(user, tenant),
        multipart("big.bin", "application/octet-stream", &vec![7u8; 4096]).await,
    )
    .await
    .expect_err("over the cap");
    let res = axum::response::IntoResponse::into_response(err);
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body: serde_json::Value =
        serde_json::from_slice(&body_of(res).await).expect("a JSON error body the UI can render");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("larger"),
        "a readable message, not a raw 413: {body}"
    );

    // Nothing reached the store, and nothing reached the table.
    assert!(
        state.artifacts.list("").await.unwrap().is_empty(),
        "an over-cap upload must not write an object"
    );

    // A file at exactly the cap is still fine — the check is "over", not "at".
    let rec = put(
        &state,
        ctx(user, tenant),
        "exact.bin",
        "application/octet-stream",
        &vec![3u8; 1024],
    )
    .await;
    assert_eq!(rec.size_bytes, 1024);

    bed.teardown().await;
}

/// AC-5: what is stored is not what is served. An uploaded `.html` comes back
/// as an octet-stream attachment with `nosniff`; a PNG comes back inline.
#[tokio::test]
async fn a_stored_html_file_is_never_served_as_html() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("headers");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("headers").await;
    let (user, _) = bed.user(tenant, "member").await;

    let html = put(
        &state,
        ctx(user, tenant),
        "evil.html",
        "text/html",
        b"<script>alert(1)</script>",
    )
    .await;
    let res = serve(State(state.clone()), ctx(user, tenant), Path(html.id))
        .await
        .unwrap();
    assert_eq!(
        header_of(&res, header::CONTENT_TYPE),
        "application/octet-stream"
    );
    assert!(
        header_of(&res, header::CONTENT_DISPOSITION).starts_with("attachment"),
        "an html upload downloads rather than renders"
    );
    assert_eq!(header_of(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    // The record still remembers what the uploader called it — only the
    // *serving* decision changed.
    assert_eq!(html.content_type, "text/html");

    let png = put(
        &state,
        ctx(user, tenant),
        "shot.png",
        "image/png",
        b"\x89PNG",
    )
    .await;
    let res = serve(State(state.clone()), ctx(user, tenant), Path(png.id))
        .await
        .unwrap();
    assert_eq!(header_of(&res, header::CONTENT_TYPE), "image/png");
    assert!(header_of(&res, header::CONTENT_DISPOSITION).starts_with("inline"));
    assert_eq!(header_of(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");

    bed.teardown().await;
}

/// AC-8: the uploader may delete, an unrelated member may not, an admin may —
/// and the object goes with the row.
#[tokio::test]
async fn delete_removes_both_halves_and_is_gated() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("delete");
    let state = state_on(&bed, &scratch).await;
    let tenant = bed.tenant("delete").await;
    let (uploader, _) = bed.user(tenant, "member").await;
    let (bystander, _) = bed.user(tenant, "member").await;
    let (admin, _) = bed.user(tenant, "admin").await;

    let rec = put(&state, ctx(uploader, tenant), "a.txt", "text/plain", b"a").await;
    let key = state
        .user_content
        .get(rec.id, tenant)
        .await
        .unwrap()
        .unwrap()
        .storage_key;

    let err = delete(State(state.clone()), ctx(bystander, tenant), Path(rec.id))
        .await
        .expect_err("a member who did not upload it may not delete it");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        delete(State(state.clone()), ctx(uploader, tenant), Path(rec.id))
            .await
            .expect("the uploader may"),
        StatusCode::NO_CONTENT
    );
    assert!(
        state.artifacts.head(&key).await.unwrap().is_none(),
        "the bytes go with the row"
    );

    let err = delete(State(state.clone()), ctx(uploader, tenant), Path(rec.id))
        .await
        .expect_err("a second delete has nothing to delete");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        StatusCode::NOT_FOUND
    );
    let err = serve(State(state.clone()), ctx(uploader, tenant), Path(rec.id))
        .await
        .expect_err("and it is gone from the GET too");
    assert_eq!(
        axum::response::IntoResponse::into_response(err).status(),
        StatusCode::NOT_FOUND
    );

    // A tenant admin may remove someone else's upload.
    let theirs = put(&state, ctx(uploader, tenant), "b.txt", "text/plain", b"b").await;
    assert_eq!(
        delete(State(state.clone()), ctx(admin, tenant), Path(theirs.id))
            .await
            .expect("an admin may"),
        StatusCode::NO_CONTENT
    );

    bed.teardown().await;
}

/// A store that CAN sign — the thing a disk store is not. Wraps a map so the
/// test needs no S3 at all; the only behaviour under test is which of the two
/// answers the route gives.
struct SigningStore {
    objects: dashmap::DashMap<String, Vec<u8>>,
}

#[async_trait]
impl ArtifactStore for SigningStore {
    async fn list(&self, _prefix: &str) -> Result<Vec<ObjectMeta>> {
        Ok(Vec::new())
    }
    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        self.objects
            .get(key)
            .map(|v| v.clone())
            .ok_or_else(|| anyhow::anyhow!("no object at {key}"))
    }
    async fn put(&self, key: &str, bytes: Vec<u8>) -> Result<()> {
        self.objects.insert(key.to_string(), bytes);
        Ok(())
    }
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        Ok(self.objects.get(key).map(|v| ObjectMeta {
            key: key.to_string(),
            size: v.len() as u64,
            sha256: None,
        }))
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.objects.remove(key);
        Ok(())
    }
    async fn presign(&self, key: &str, _ttl: Duration) -> Result<Option<String>> {
        Ok(Some(format!("https://store.example/{key}?sig=abc")))
    }
    fn describe(&self) -> String {
        "fake:signing".into()
    }
}

/// AC-4: the redirect switch. Both cases against the SAME presign-capable
/// store, so the only variable is the switch.
#[tokio::test]
async fn the_redirect_switch_decides_between_a_302_and_a_stream() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let store = Arc::new(SigningStore {
        objects: dashmap::DashMap::new(),
    });
    let tenant = bed.tenant("redirect").await;
    let (user, _) = bed.user(tenant, "member").await;

    let mut streaming = bed.app_state().await;
    streaming.artifacts = store.clone();
    let rec = put(
        &streaming,
        ctx(user, tenant),
        "shot.png",
        "image/png",
        b"\x89PNG-bytes",
    )
    .await;

    let res = serve(State(streaming.clone()), ctx(user, tenant), Path(rec.id))
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "off by default: a store that CAN sign is still streamed through"
    );
    assert_eq!(body_of(res).await, b"\x89PNG-bytes");

    let mut cfg = bed.config();
    cfg.user_content_redirect = true;
    let mut redirecting = AppState::new(bed.db(), cfg, None).await;
    redirecting.artifacts = store.clone();

    let res = serve(State(redirecting), ctx(user, tenant), Path(rec.id))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FOUND);
    assert!(
        header_of(&res, header::LOCATION).starts_with("https://store.example/"),
        "a short-lived URL to the store itself"
    );
    // The serving decision travels with the redirect too.
    assert_eq!(header_of(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");

    bed.teardown().await;
}

/// AC-4's other half: the switch being on cannot break a disk-backed
/// deployment. `presign` answers `None` there, and the route streams.
#[tokio::test]
async fn a_disk_store_streams_even_with_the_switch_on() {
    let Some(mut bed) = TestBed::new().await else {
        return;
    };
    let scratch = Scratch::new("disk-redirect");
    let mut cfg = bed.config();
    cfg.dist_dir = scratch.0.to_string_lossy().into_owned();
    cfg.user_content_redirect = true;
    let state = AppState::new(bed.db(), cfg, None).await;
    let tenant = bed.tenant("disk").await;
    let (user, _) = bed.user(tenant, "member").await;

    let rec = put(&state, ctx(user, tenant), "a.txt", "text/plain", b"plain").await;
    let res = serve(State(state.clone()), ctx(user, tenant), Path(rec.id))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_of(res).await, b"plain");

    bed.teardown().await;
}
