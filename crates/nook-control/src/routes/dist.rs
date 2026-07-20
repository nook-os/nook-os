//! Node binary distribution: how `nook` gets onto another machine, and how it
//! stays the same version as the control plane it talks to.
//!
//! A self-hosted fleet drifts the moment installing the agent is a manual job,
//! so the control plane ships the binary it was built alongside. One artifact
//! directory, one install script, one command to run on the new machine — and
//! the same command, without a token, is the updater.
//!
//! Artifacts are named `nook-<os>-<arch>` (`nook-linux-x86_64`,
//! `nook-darwin-aarch64`). The name is the whole protocol: the install script
//! derives it from `uname`, and anything dropped in the directory following
//! that convention is offered without further configuration.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use nook_types::*;
use sha2::{Digest, Sha256};

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

/// Reject anything that isn't a bare artifact name. The directory is served
/// unauthenticated, so path traversal here would be a file-read primitive.
fn safe_artifact(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains('/')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
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
    let dir = std::path::Path::new(&state.cfg.dist_dir);

    let artifacts = KNOWN_PLATFORMS
        .iter()
        .filter_map(|(os, arch, label)| {
            let filename = artifact_name(os, arch);
            let path = dir.join(&filename);
            let meta = std::fs::metadata(&path).ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(NodeArtifact {
                os: (*os).to_string(),
                arch: (*arch).to_string(),
                label: (*label).to_string(),
                filename: filename.clone(),
                size: meta.len(),
                // Cheap enough for a handful of files, and it's the only way a
                // download over a flaky link can be checked.
                sha256: std::fs::read(&path)
                    .map(|bytes| format!("{:x}", Sha256::digest(&bytes)))
                    .unwrap_or_default(),
                url: format!("{base}/dist/{filename}"),
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(NodeReleases {
        version: VERSION.to_string(),
        install_url: format!("{base}/install.sh"),
        base_url: base,
        artifacts,
    }))
}

/// Serve an artifact. Unauthenticated on purpose: it is fetched by a machine
/// that has no session yet and nothing but a join token, and the binary is the
/// same one anyone can build from the public source.
pub async fn download(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if !safe_artifact(&name) {
        return (StatusCode::BAD_REQUEST, "bad artifact name").into_response();
    }
    let path = std::path::Path::new(&state.cfg.dist_dir).join(&name);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            Body::from(bytes),
        )
            .into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            format!(
                "no {name} on this server — the control plane was built without \
                 a node binary for that platform"
            ),
        )
            .into_response(),
    }
}

/// `curl -fLsS <server>/install.sh | sh -s -- --token nook_join_…`
///
/// Generated rather than a file on disk because the server URL has to be baked
/// in: the command is copied to a machine that has never heard of this
/// instance, and a script that then asks where to phone home is a script
/// someone gets wrong at 1am.
pub async fn install_script(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let base = request_base(&headers, &state);
    let script = INSTALL_SH
        .replace("@@SERVER@@", &base)
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

/// The installer. POSIX sh, no dependencies beyond curl/uname/install.
///
/// Three jobs, in one file because three files is three chances to run the
/// wrong one: install or update the binary, optionally join, optionally set up
/// systemd. Run with no arguments it is purely an updater, which is what makes
/// "keep the fleet on one version" a single command per machine.
const INSTALL_SH: &str = r#"#!/bin/sh
# NookOS node installer — from @@SERVER@@ (control plane @@VERSION@@)
#
#   curl -fLsS @@SERVER@@/install.sh | sh -s -- --token nook_join_xxx
#   curl -fLsS @@SERVER@@/install.sh | sh                 # update in place
#
set -eu

SERVER="@@SERVER@@"
TOKEN=""
NAME=""
PREFIX="${NOOK_PREFIX:-$HOME/.local/bin}"
SYSTEMD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --token) TOKEN="${2:-}"; shift 2 ;;
    --name) NAME="${2:-}"; shift 2 ;;
    --prefix) PREFIX="${2:-}"; shift 2 ;;
    --server) SERVER="${2:-}"; shift 2 ;;
    --systemd) SYSTEMD=1; shift ;;
    -h|--help)
      echo "usage: install.sh [--token TOKEN] [--name NAME] [--prefix DIR] [--systemd]"
      exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[33m▸\033[0m %s\n' "$*"; }
die() { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# --- platform ---------------------------------------------------------------
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$os" in
  linux|darwin) ;;
  *) die "unsupported OS '$os' — build from source: cargo build --release -p nook-node" ;;
esac
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture '$arch'" ;;
esac
artifact="nook-$os-$arch"

command -v curl >/dev/null 2>&1 || die "curl is required"

# --- download ---------------------------------------------------------------
say "Fetching $artifact from $SERVER"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fLsS "$SERVER/dist/$artifact" -o "$tmp" \
  || die "no build for $os/$arch on this server (see Nodes → add node for what is available)"
chmod +x "$tmp"

mkdir -p "$PREFIX"
# Replace by rename: an in-place overwrite of a running binary fails with
# ETXTBSY, which is exactly what an update on a live node would hit.
mv -f "$tmp" "$PREFIX/nook"
trap - EXIT
say "Installed $PREFIX/nook"

case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) say "Add it to your PATH:  export PATH=\"$PREFIX:\$PATH\"" ;;
esac

"$PREFIX/nook" --version || true

# --- join -------------------------------------------------------------------
if [ -n "$TOKEN" ]; then
  say "Joining $SERVER"
  if [ -n "$NAME" ]; then
    "$PREFIX/nook" join --server "$SERVER" --token "$TOKEN" --name "$NAME"
  else
    "$PREFIX/nook" join --server "$SERVER" --token "$TOKEN"
  fi
else
  say "No token given — binary updated, existing config untouched."
fi

# --- systemd ----------------------------------------------------------------
if [ "$SYSTEMD" = "1" ]; then
  command -v systemctl >/dev/null 2>&1 || die "systemd not available on this machine"
  user=$(id -un)
  unit=/etc/systemd/system/nook-node.service
  say "Writing $unit (sudo)"
  sudo tee "$unit" >/dev/null <<UNIT
[Unit]
Description=NookOS node agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$user
Group=$(id -gn)
ExecStart=$PREFIX/nook run
Restart=always
RestartSec=5
Environment=RUST_LOG=nook=info
Environment=HOME=$HOME
WorkingDirectory=$HOME

# tmux is the buffer of record: sessions outlive the agent, and Restart=always
# means it restarts. systemd's default KillMode would take the tmux server —
# and every one of the user's terminals — down with it.
KillMode=process

NoNewPrivileges=yes
ProtectSystem=full
ReadWritePaths=$HOME /tmp

[Install]
WantedBy=multi-user.target
UNIT
  sudo systemctl daemon-reload
  sudo systemctl enable --now nook-node
  say "nook-node is running — systemctl status nook-node"
elif [ -n "$TOKEN" ]; then
  say "Start it now:        $PREFIX/nook run"
  say "Or install a service: curl -fLsS $SERVER/install.sh | sh -s -- --systemd"
fi
"#;
