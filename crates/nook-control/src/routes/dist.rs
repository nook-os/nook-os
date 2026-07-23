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
                url: release_asset_url(&repo, &version, &filename),
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

/// Where a platform's asset lives, for THIS control plane's version.
///
/// Pinned rather than `releases/latest/download/…`, and the difference is a
/// bug that was live: a node asks the control plane which version it should be
/// and then downloads whatever GitHub's newest release happens to be. Those are
/// only the same thing while nobody has published ahead of what is deployed.
///
/// The moment they diverge, every supervised node loops — fetch `latest`,
/// restart, still not the version the control plane expects, fetch again — and
/// it does so across the whole fleet at once, on reconnect, which is exactly
/// when nobody is watching.
///
/// Pinning costs a 404 when a control plane runs a version that was never
/// published (a local build, usually). That is the better failure: it names the
/// version it wanted, it happens on one machine at a time, and it leaves the
/// agent running.
pub fn release_asset_url(repo: &str, version: &str, filename: &str) -> String {
    format!("{}/{filename}", release_base_url(repo, version))
}

/// The download directory for a version's assets. GitHub tags are `v`-prefixed
/// and `VERSION` is not, so exactly one place adds it.
pub fn release_base_url(repo: &str, version: &str) -> String {
    format!("https://github.com/{repo}/releases/download/v{version}")
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
            // Pinned for the same reason as `releases()`: a machine joining
            // this control plane should install the version it expects, not
            // whatever shipped most recently — otherwise a fresh install's
            // first act is to update itself.
            &release_base_url(&state.cfg.releases_repo, VERSION),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A node must download the version the control plane asked it to be.
    ///
    /// The self-update loop is: `RegisterAck` carries `expected_agent_version`,
    /// the node compares it against its own, and if they differ it fetches from
    /// here and restarts. That converges only if what it fetches IS the
    /// expected version. Pointing at `latest` made the comparison and the
    /// download answer two different questions, so a control plane one release
    /// behind GitHub would put every supervised node into an update loop.
    #[test]
    fn the_download_url_is_pinned_to_this_control_planes_version() {
        let url = release_asset_url("nook-os/nook-os", "0.4.3", "nook-linux-x86_64");
        assert_eq!(
            url,
            "https://github.com/nook-os/nook-os/releases/download/v0.4.3/nook-linux-x86_64"
        );
        assert!(
            !url.contains("/latest/"),
            "`releases/latest/download` resolves to whatever shipped most \
             recently, which is not what the node was told to become — see this \
             function's docs for the loop that causes"
        );
    }

    /// The tag carries a `v`; the crate version does not. Adding it twice, or
    /// not at all, 404s every download — quietly, on machines nobody is
    /// watching.
    #[test]
    fn the_tag_is_v_prefixed_exactly_once() {
        let base = release_base_url("nook-os/nook-os", "1.2.3");
        assert!(base.ends_with("/v1.2.3"), "got {base}");
        assert!(!base.contains("/vv"), "double-prefixed: {base}");
    }

    /// The installer and the self-update path must agree. They are two ways to
    /// put the same binary on a machine, and if only one is pinned then a
    /// freshly installed node's first act is to update itself away from what it
    /// just installed.
    #[test]
    fn the_installer_and_the_updater_point_at_the_same_place() {
        let src = include_str!("dist.rs");
        // Bound the scan to the handler: the file's own test text mentions the
        // pattern it is looking for, and a scan running to EOF would match
        // itself and pass for the wrong reason.
        let body = src
            .split("pub async fn install_script(")
            .nth(1)
            .expect("install_script handler")
            .split("const INSTALL_SH")
            .next()
            .expect("handler body");
        assert!(
            body.contains("release_base_url("),
            "the generated installer must build its RELEASES base with \
             release_base_url so it is pinned like the updater is"
        );
        assert!(
            !body.contains(concat!("releases/", "latest", "/download")),
            "the generated installer still points at the floating release"
        );
    }
}
