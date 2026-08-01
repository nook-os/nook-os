//! Node configuration, persisted at `~/.config/nook/node.toml` after join.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub server: String,
    pub node_id: String,
    pub node_name: String,
    pub node_token: String,
    pub workspace_roots: Vec<String>,
    /// Private key used for git operations (clones of private repos). When
    /// unset, the node's own generated key (~/.config/nook/id_ed25519) is
    /// used. Set by `nook setup` when you pick an existing ~/.ssh key.
    #[serde(default)]
    pub ssh_key_path: Option<String>,
    /// SHA-256 of the control plane's certificate, from the join token.
    ///
    /// Once set, this machine talks to that certificate and nothing else. It
    /// survives in node.toml so every later reconnect is pinned too, not just
    /// the enrolment that established it.
    #[serde(default)]
    pub server_fingerprint: Option<String>,
    /// Where the *agent* connection goes, when that is not the same place as
    /// the API.
    ///
    /// The agent port terminates mTLS in the control-plane process itself,
    /// because only it can decide which tenant's CA a client certificate should
    /// be judged against — so it cannot sit behind a proxy that terminates TLS
    /// the way the API does. When unset the two are the same host, which is the
    /// single-container case.
    #[serde(default)]
    pub agent_server: Option<String>,
    /// How this agent is kept running: "systemd-user", "systemd-system",
    /// "launchd", "docker", or absent when nothing supervises it.
    ///
    /// Recorded because self-update is only safe when something will start the
    /// process again. Told to update, an unsupervised agent would replace its
    /// binary, exit, and never come back — and the fleet-wide version of that
    /// mistake is every machine at once.
    #[serde(default)]
    pub service: Option<String>,

    /// The tmux server this node runs its sessions on, as an `-L <socket>` name
    /// (MAIN-108). Absent means the user's DEFAULT tmux server — byte-identical
    /// to a pre-108 node (AC-1). A fresh enrollment writes `nook-<cp-slug>`, so
    /// every new node gets a PRIVATE server and two nodes on one host never see
    /// or clobber each other's sessions.
    ///
    /// Cutover is forward-only and manual (AC-6): changing this on a node that
    /// already has live sessions STRANDS them on the old server — the control
    /// plane marks them exited on the next connect (its `live_tmux_sessions`
    /// reconcile) and nothing adopts them across sockets. Change it only when the
    /// node has no sessions worth keeping.
    #[serde(default)]
    pub tmux_socket: Option<String>,

    /// The slug of the tenant this node enrolled into (MAIN-347). The default
    /// workspace root is scoped by it (`~/.nook/workspace/<tenant_slug>`), so two
    /// tenants on one control-plane host never share a checkout tree. `None` on a
    /// pre-347 `node.toml` — the root then falls back to the host slug (MAIN-58).
    /// Forward-only: a fresh enrolment records it; an existing config keeps the
    /// root it already has.
    #[serde(default)]
    pub tenant_slug: Option<String>,
}

impl NodeConfig {
    /// The endpoint `nook run` dials. Falls back to `server`.
    pub fn agent_endpoint(&self) -> &str {
        self.agent_server.as_deref().unwrap_or(&self.server)
    }
}

pub fn config_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("node.toml"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/node.toml"))
}

impl NodeConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no node config at {} — run `nook join` first",
                path.display()
            )
        })?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// Where `nook run` records its PID while the agent is live (MAIN-107 AC-2).
///
/// Beside `node.toml`, in the config/state dir. It exists so
/// `nook migrate-workspaces --apply` can refuse to move checkouts out from
/// under a running agent: a periodic or gitop-triggered discovery mid-move
/// would report a half-emptied tree, and the reconcile deletes every
/// `node_workspaces` row whose path is no longer reported.
pub fn pidfile_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("agent.pid"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/agent.pid"))
}

/// The pidfile `nook run` holds for its lifetime. Best-effort: it is written on
/// start and removed on a clean exit, but a crash or a hard kill leaves it
/// behind — which is why the reader is stale-PID tolerant rather than trusting
/// the file's mere existence.
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    pub fn write() -> Result<Self> {
        let path = pidfile_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A person's credential for this CLI, at `~/.config/nook/auth.toml`.
///
/// Kept apart from `node.toml` on purpose: node.toml is the machine's identity,
/// written by `nook join` and owned by the service; this is *yours*, written by
/// `nook login`, and it is what lets the CLI drive machines other than this one
/// (a node token is confined to its own machine by design).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Control plane URL. Falls back to node.toml's when this file is written
    /// on a machine that has already joined.
    #[serde(default)]
    pub server: Option<String>,
    pub token: String,
}

pub fn auth_path() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("NOOK_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("auth.toml"));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/nook/auth.toml"))
}

impl AuthConfig {
    pub fn load() -> Result<Self> {
        let path = auth_path()?;
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no login at {}", path.display()))?;
        Ok(toml::from_str(&raw)?)
    }

    pub fn save(&self) -> Result<()> {
        let path = auth_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        // It's a password. Nobody else on this machine needs to read it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

// ── Transport security ──────────────────────────────────────────────────────
//
// A node carries a credential that lets it act as a machine in someone's
// fleet, and it streams terminal output — including whatever a session prints.
// Sending that over plaintext is not a configuration preference, it is a
// vulnerability, so the default is to refuse rather than to warn and continue.
//
// The escape hatch exists for exactly one case: the docker-compose dev stack,
// where the control plane is `http://control-plane:8080` on a container
// network. It is deliberately awkward — an explicit opt-in, a warning on every
// start, and a hard refusal in production — because the two real answers for
// self-hosting without public DNS are a pinned CA fingerprint in the join
// token and an operator-supplied CA certificate, not turning encryption off.

/// Did the operator explicitly ask for an unencrypted/unverified control-plane
/// connection? `NOOK_INSECURE=1` or `--insecure-skip-verify`.
pub fn insecure_requested(flag: bool) -> bool {
    flag || matches!(
        std::env::var("NOOK_INSECURE").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Refuse a plaintext control plane unless the hatch is open.
///
/// Returns whether the hatch is in use, so the caller can warn once at startup.
/// `APP_ENV=production` refuses outright — mirroring how the control plane
/// rejects `AUTH_DEV_MODE` in production, so a machine that thinks it is
/// production cannot be talked into plaintext by an environment variable.
pub fn check_server_security(server: &str, insecure_flag: bool) -> Result<bool> {
    let plaintext = !server.trim().to_ascii_lowercase().starts_with("https://");
    if !plaintext {
        return Ok(false);
    }
    let insecure = insecure_requested(insecure_flag);
    let production = std::env::var("APP_ENV").ok().as_deref() == Some("production");

    if insecure && production {
        anyhow::bail!(
            "refusing an unencrypted connection to {server}: NOOK_INSECURE is set but \
             APP_ENV=production. Point the node at an https:// control plane."
        );
    }
    if !insecure {
        anyhow::bail!(
            "refusing an unencrypted connection to {server}.\n\n\
             The node's credential and every session's terminal output would cross \
             the network in the clear.\n\n\
             Fix it one of these ways:\n\
             • point --server at an https:// URL (the normal answer)\n\
             • for LOCAL DEV ONLY, re-run with --insecure-skip-verify or NOOK_INSECURE=1"
        );
    }
    Ok(true)
}

/// Say it every time, not just once — an insecure link that has been running
/// for months should still be announcing itself.
pub fn warn_if_insecure(insecure_in_use: bool, server: &str) {
    if insecure_in_use {
        tracing::warn!(
            %server,
            "INSECURE: talking to the control plane over an unencrypted connection \
             (NOOK_INSECURE). The node token and all terminal output are in the clear. \
             Local development only — never for a real fleet."
        );
    }
}

/// Where this machine's own certificate and key live — beside `node.toml`,
/// `0600`. The key is generated locally and never leaves.
pub fn cert_paths() -> Result<(PathBuf, PathBuf)> {
    let base = config_path()?;
    let dir = base.parent().context("no config directory")?.to_path_buf();
    Ok((dir.join("node.crt"), dir.join("node.key")))
}

/// This machine's certificate and key, if it has enrolled.
pub fn load_identity() -> Option<(String, String)> {
    let (cert, key) = cert_paths().ok()?;
    Some((
        std::fs::read_to_string(cert).ok()?,
        std::fs::read_to_string(key).ok()?,
    ))
}

/// Expand a leading `~` against $HOME.
pub fn expand_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// A filesystem-safe slug identifying a control plane, derived from its server
/// URL's host (MAIN-58 AC-1). Scheme, userinfo, port and path are stripped; the
/// host is lowercased; anything outside `[a-z0-9.-]` becomes `-`. Idempotent —
/// slugging a bare host is a no-op — so composing the same root twice is stable.
///
/// `https://Nook.Hein.Network:8443/x` → `nook.hein.network`.
pub fn cp_slug(server_url: &str) -> String {
    // Strip the scheme, then take the authority up to the first path/query/frag.
    let after_scheme = server_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(server_url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Drop any `user@` prefix, then the `:port` suffix.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);

    sanitize_dir_segment(host).unwrap_or_else(|| "control-plane".to_string())
}

/// Reduce a string to a single filesystem-safe directory segment: lowercase,
/// characters outside `[a-z0-9.-]` become `-`, leading/trailing separators
/// trimmed. `None` when nothing usable remains, so each caller supplies its own
/// fallback. Shared by [`cp_slug`] (a server host) and the tenant path segment
/// (MAIN-347) so the two can never sanitize differently.
fn sanitize_dir_segment(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '.' || lower == '-' {
            out.push(lower);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The workspace root a fresh enrollment gets when none is set explicitly:
/// `~/.nook/workspace/<tenant-slug>` (MAIN-347). Scoping by the enrolling node's
/// tenant — not the control-plane host (MAIN-58's `cp-slug`) — is what lets two
/// tenants sharing ONE control-plane host keep separate checkout trees; the
/// layout under the root (`<owner>/<repo>`, worktrees, discovery) is unchanged.
/// An older control plane that sends no slug (or one that sanitizes to nothing)
/// falls back to the host slug rather than a rootless `~/.nook/workspace/`, so
/// boot never yields a bare root. Only the default; an explicit root always
/// wins.
pub fn default_workspace_root(tenant_slug: Option<&str>, server_url: &str) -> String {
    let segment = tenant_slug
        .and_then(sanitize_dir_segment)
        .unwrap_or_else(|| cp_slug(server_url));
    format!("~/.nook/workspace/{segment}")
}

/// The private tmux server name a fresh node enrolls with (MAIN-108):
/// `nook-<cp-slug>`. Because the control plane's slug is part of the name, two
/// nodes on one machine belonging to different control planes get different
/// servers and never share sessions (AC-5). Forward-only — only a NEW node gets
/// it; an existing node keeps the default server (`None`) until its operator
/// opts in (AC-1/NG-1). See [`NodeConfig::tmux_socket`].
pub fn derived_tmux_socket(server_url: &str) -> String {
    format!("nook-{}", cp_slug(server_url))
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn strips_scheme_port_path_and_lowercases() {
        assert_eq!(cp_slug("https://nook.hein.network"), "nook.hein.network");
        assert_eq!(
            cp_slug("https://Nook.Hein.Network:8443/foo?x=1"),
            "nook.hein.network"
        );
        assert_eq!(cp_slug("http://Localhost:8080"), "localhost");
        assert_eq!(
            cp_slug("https://user@host.example.com:443"),
            "host.example.com"
        );
    }

    #[test]
    fn sanitizes_unsafe_characters_to_dashes() {
        // Dots and dashes are kept (they are safe in a directory name); anything
        // else becomes a single dash, and leading/trailing separators are trimmed.
        assert_eq!(cp_slug("https://a_b~c.example"), "a-b-c.example");
        assert!(cp_slug("https://weird™host")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'));
        // A URL with nothing host-like still yields a usable directory name.
        assert_eq!(cp_slug("https://"), "control-plane");
    }

    #[test]
    fn slugging_a_slug_is_a_no_op() {
        let slug = cp_slug("https://nook.hein.network:8443");
        assert_eq!(slug, "nook.hein.network");
        assert_eq!(cp_slug(&slug), slug, "idempotent");
    }

    #[test]
    fn tenant_slug_scopes_the_root() {
        // The tenant slug is the top segment (MAIN-347): a fresh enrol into
        // tenant `hein` lands checkouts under `~/.nook/workspace/hein`.
        assert_eq!(
            default_workspace_root(Some("hein"), "https://nook.hein.network"),
            "~/.nook/workspace/hein"
        );
    }

    #[test]
    fn different_tenants_on_one_host_get_distinct_roots() {
        // The collision this fixes: two tenants sharing ONE control-plane host.
        // The server URL is identical; only the tenant differs, and the roots
        // are disjoint (AC-5).
        let server = "https://nook.hein.network";
        let a = default_workspace_root(Some("hein"), server);
        let b = default_workspace_root(Some("acme"), server);
        assert_eq!(a, "~/.nook/workspace/hein");
        assert_eq!(b, "~/.nook/workspace/acme");
        assert_ne!(a, b);
    }

    #[test]
    fn tenant_slug_is_sanitized_and_idempotent() {
        // A slug with unsafe characters is lowercased and cleaned the same way a
        // host is (AC-3); slugging the result again is a no-op.
        let root = default_workspace_root(Some("Acme_Corp~1"), "https://x");
        assert_eq!(root, "~/.nook/workspace/acme-corp-1");
        let seg = root.rsplit('/').next().unwrap();
        assert_eq!(
            default_workspace_root(Some(seg), "https://x"),
            root,
            "idempotent"
        );
    }

    #[test]
    fn no_tenant_falls_back_to_host_slug() {
        // An older control plane sends no slug, or one that sanitizes to nothing.
        // The root falls back to the host slug rather than a rootless
        // `~/.nook/workspace/` (AC-7).
        assert_eq!(
            default_workspace_root(None, "https://nook.hein.network"),
            "~/.nook/workspace/nook.hein.network"
        );
        assert_eq!(
            default_workspace_root(Some("  "), "https://nook.hein.network"),
            "~/.nook/workspace/nook.hein.network"
        );
    }

    #[test]
    fn node_toml_without_tenant_slug_still_deserializes() {
        // A pre-347 node.toml has no `tenant_slug` line; `#[serde(default)]`
        // must let it load as `None` rather than fail the node's boot (AC-2).
        let cfg: NodeConfig = toml::from_str(
            r#"
            server = "https://nook.hein.network"
            node_id = "n1"
            node_name = "box"
            node_token = "t"
            workspace_roots = ["~/.nook/workspace/nook.hein.network"]
            "#,
        )
        .expect("legacy node.toml deserializes");
        assert_eq!(cfg.tenant_slug, None);
    }

    #[test]
    fn derived_tmux_socket_is_nook_prefixed_and_per_control_plane() {
        // The private server name a fresh node enrolls with (MAIN-108 AC-1): a
        // `nook-` prefix plus the control-plane slug, so two nodes on one host
        // belonging to different control planes get disjoint servers (AC-5).
        assert_eq!(
            derived_tmux_socket("https://nook.hein.network:8443/x"),
            "nook-nook.hein.network"
        );
        assert_ne!(
            derived_tmux_socket("https://nook.hein.network"),
            derived_tmux_socket("https://other.example.com"),
        );
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;

    /// The env vars these tests set are process-global, so they run serially.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear() {
        std::env::remove_var("NOOK_INSECURE");
        std::env::remove_var("APP_ENV");
    }

    #[test]
    fn https_needs_no_hatch() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // Not insecure, and no error: the ordinary path stays silent.
        assert!(!check_server_security("https://nook.example.com", false).unwrap());
        // Even in production, and even with the hatch set — https is just fine.
        std::env::set_var("APP_ENV", "production");
        std::env::set_var("NOOK_INSECURE", "1");
        assert!(!check_server_security("https://nook.example.com", false).unwrap());
        clear();
    }

    #[test]
    fn plaintext_is_refused_by_default() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        let err = check_server_security("http://control-plane:8080", false).unwrap_err();
        assert!(err
            .to_string()
            .contains("refusing an unencrypted connection"));
        // A bare host is plaintext too — the old code silently made it ws://.
        assert!(check_server_security("nook.example.com", false).is_err());
        clear();
    }

    #[test]
    fn hatch_opens_only_when_asked() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // Via the flag...
        assert!(check_server_security("http://localhost:8080", true).unwrap());
        // ...or the env var.
        std::env::set_var("NOOK_INSECURE", "1");
        assert!(check_server_security("http://localhost:8080", false).unwrap());
        clear();
    }

    #[test]
    fn production_refuses_the_hatch() {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        std::env::set_var("APP_ENV", "production");
        std::env::set_var("NOOK_INSECURE", "1");
        let err = check_server_security("http://control-plane:8080", true).unwrap_err();
        assert!(
            err.to_string().contains("APP_ENV=production"),
            "production must refuse the hatch outright, got: {err}"
        );
        clear();
    }
}
