//! Getting `nook` onto another machine.
//!
//! The control plane generates the install script but does NOT host the
//! binaries: those come from the project's GitHub releases. Hosting them here
//! was a mistake worth naming — a control plane could only ever serve what its
//! own build host could compile, which meant no macOS build without shipping a
//! second upload path, and it quietly made every deployment a binary mirror
//! responsible for bytes it never built.
//!
//! The script is still generated rather than static, because the *server URL*
//! genuinely has to be baked in: it is copied to a machine that has never
//! heard of this instance. Binary bytes come from the tag; "which control
//! plane do I join" comes from here.
//!
//! Artifacts are named `nook-<os>-<arch>` (`nook-linux-x86_64`,
//! `nook-darwin-aarch64`) — the install script derives that from `uname` and
//! asks GitHub for it.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nook_types::*;

use crate::error::ApiResult;
use crate::state::AppState;

/// The version this control plane was built from. Artifacts served next to it
/// are assumed to be the same build — that assumption is the point.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Platforms we know how to name, in the order the UI should offer them.
/// Detection failing is normal (a browser can't see `uname`), so this list is
/// also the manual picker.
const KNOWN_PLATFORMS: &[(&str, &str, &str)] = &[
    ("linux", "x86_64", "Linux · x86_64"),
    ("linux", "aarch64", "Linux · arm64"),
    ("darwin", "aarch64", "macOS · Apple silicon"),
    ("darwin", "x86_64", "macOS · Intel"),
];

fn artifact_name(os: &str, arch: &str) -> String {
    format!("nook-{os}-{arch}")
}

/// Public URL of this instance as the *caller* reached it.
///
/// Deliberately not `PUBLIC_BASE_URL`: the install script's whole job is to be
/// copied to another machine, and a base URL configured as `localhost` would
/// produce a command that silently targets the wrong host. What the browser
/// used to get here is what the next machine should use too.
pub fn request_base(headers: &HeaderMap, state: &AppState) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok());
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if state.cfg.is_production() {
            "https"
        } else {
            "http"
        });
    match host {
        Some(h) => format!("{proto}://{h}"),
        None => state.cfg.public_base_url.clone(),
    }
}

/// What this control plane can hand out, and how to ask for it.
#[utoipa::path(get, path = "/api/v1/node/releases",
    operation_id = "node_releases",
    responses((status = 200, body = NodeReleases)))]
pub async fn releases(
    State(state): State<AppState>,
    headers: HeaderMap,
    _auth: crate::auth::AuthCtx,
) -> ApiResult<Json<NodeReleases>> {
    let base = request_base(&headers, &state);
    let version = VERSION.to_string();
    let repo = state.cfg.releases_repo.clone();

    // Every platform is offered, because GitHub — not this server's build host
    // — decides what exists. A machine that asks for a platform with no asset
    // published gets a 404 from GitHub, which is a clearer failure than this
    // server pretending the platform is unsupported.
    let artifacts = KNOWN_PLATFORMS
        .iter()
        .map(|(os, arch, label)| {
            let filename = artifact_name(os, arch);
            NodeArtifact {
                os: (*os).to_string(),
                arch: (*arch).to_string(),
                label: (*label).to_string(),
                url: release_asset_url(&repo, &filename),
                filename,
            }
        })
        .collect::<Vec<_>>();

    Ok(Json(NodeReleases {
        version,
        install_url: format!("{base}/install.sh"),
        base_url: base,
        artifacts,
    }))
}

/// The `latest` release asset for a platform.
///
/// `releases/latest/download/<asset>` always resolves to the newest published
/// release, so the install script never has to know a version number.
pub fn release_asset_url(repo: &str, filename: &str) -> String {
    format!("https://github.com/{repo}/releases/latest/download/{filename}")
}

/// `curl -fLsS <server>/install.sh | sh -s -- --token nook_join_…`
///
/// Generated rather than a file on disk because the server URL has to be baked
/// in: the command is copied to a machine that has never heard of this
/// instance, and a script that then asks where to phone home is a script
/// someone gets wrong at 1am.
pub async fn install_script(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = request_base(&headers, &state);
    // Read the certificate fresh: an operator who renews it should not have to
    // restart the control plane for new installs to pin the right one.
    let fingerprint = state
        .cfg
        .agent_tls_cert
        .as_deref()
        .and_then(|path| {
            let pem = std::fs::read_to_string(path).ok()?;
            crate::ca::fingerprint_pem(&pem).ok()
        })
        .unwrap_or_default();
    let agent_url = state
        .cfg
        .agent_public_url
        .clone()
        .unwrap_or_else(|| base.clone());

    let script = INSTALL_SH
        .replace("@@SERVER@@", &base)
        .replace("@@AGENT_URL@@", &agent_url)
        .replace("@@FINGERPRINT@@", &fingerprint)
        .replace(
            "@@RELEASES@@",
            &format!(
                "https://github.com/{}/releases/latest/download",
                state.cfg.releases_repo
            ),
        )
        .replace("@@VERSION@@", VERSION);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            // Never let a proxy hand out an installer pointing at a stale
            // version of this control plane.
            (header::CACHE_CONTROL, "no-store"),
        ],
        script,
    )
        .into_response()
}

/// The installer, shared verbatim with the copy published on nookos.dev and as
/// a release asset. One file, three places it can be fetched from — three files
/// would be three chances to fetch a stale one.
const INSTALL_SH: &str = include_str!("../../../../install/install.sh");
