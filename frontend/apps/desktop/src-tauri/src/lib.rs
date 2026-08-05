// The desktop shell wraps the same @nookos/app the web serves — but unlike the
// web build it has no control plane on its own origin. It is served from
// `tauri://localhost`, so "which server?" and "who am I?" are questions only
// the person running it can answer, and the answers have to survive a restart.
//
// That is all this shell does: keep those two values somewhere the OS
// considers ours, and hand them to the web app at startup.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::Manager;

/// What the app needs to reach a control plane — the shape the web bundle is
/// handed at startup, and the shape the pre-list `desktop.json` stored.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// e.g. `https://nook.example.com`
    #[serde(default)]
    pub base_url: String,
    /// A `nook_user_…` token. Sent as a bearer, never as a cookie.
    #[serde(default)]
    pub token: String,
}

/// One stored control plane. `base_url` is the identity — one entry per URL
/// (AC-5). `label` is a human rename (the host still shows underneath, AC-3);
/// `account` is who last authenticated here, for the row subtitle.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlPlane {
    pub base_url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// The whole desktop store: every control plane, and which one is active (by
/// `base_url`). This is the new on-disk shape; the old single-endpoint file is
/// migrated into a one-entry active list on first read (AC-1).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub control_planes: Vec<ControlPlane>,
    #[serde(default)]
    pub active: Option<String>,
}

/// Trailing slashes must not split one server into two rows, so every URL is
/// compared and stored in this normalized form.
fn normalize(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

impl Store {
    /// The active entry, if any.
    fn active_entry(&self) -> Option<&ControlPlane> {
        let active = self.active.as_deref()?;
        self.control_planes.iter().find(|c| c.base_url == active)
    }

    /// The active control plane as the back-compat `Endpoint` the web bundle
    /// loads at startup (empty when nothing is configured yet).
    fn active_endpoint(&self) -> Endpoint {
        self.active_entry()
            .map(|c| Endpoint {
                base_url: c.base_url.clone(),
                token: c.token.clone(),
            })
            .unwrap_or_default()
    }

    /// Add a server, or — if its URL is already stored — replace that entry's
    /// token/account in place (never a second row, AC-5). Either way it becomes
    /// active. A rename (`label`) already set on the entry is preserved.
    fn upsert_active(&mut self, ep: Endpoint) {
        let url = normalize(&ep.base_url);
        if url.is_empty() {
            return;
        }
        match self.control_planes.iter_mut().find(|c| c.base_url == url) {
            Some(existing) => {
                existing.token = ep.token;
            }
            None => self.control_planes.push(ControlPlane {
                base_url: url.clone(),
                token: ep.token,
                label: None,
                account: None,
            }),
        }
        self.active = Some(url);
    }

    /// Remove a server and its token. Forgetting the active one re-points active
    /// to the first remaining entry, or `None` when the list empties (AC-7).
    fn forget(&mut self, url: &str) {
        let url = normalize(url);
        self.control_planes.retain(|c| c.base_url != url);
        if self.active.as_deref() == Some(url.as_str()) {
            self.active = self.control_planes.first().map(|c| c.base_url.clone());
        }
    }

    /// Set (or, with an empty string, clear) a server's custom label.
    fn rename(&mut self, url: &str, label: &str) {
        let url = normalize(url);
        if let Some(c) = self.control_planes.iter_mut().find(|c| c.base_url == url) {
            c.label = Some(label.to_string()).filter(|s| !s.trim().is_empty());
        }
    }

    /// Record which account is signed in on a server (backfilled once
    /// `/auth/me` resolves after a connect or switch).
    fn set_account(&mut self, url: &str, account: &str) {
        let url = normalize(url);
        if let Some(c) = self.control_planes.iter_mut().find(|c| c.base_url == url) {
            c.account = Some(account.to_string());
        }
    }
}

/// Read `text` into a `Store`, migrating the old single-endpoint shape
/// forward. Pure, so the migration is unit-testable without a Tauri handle.
fn parse_store(text: &str) -> Store {
    // The new shape carries `control_planes`.
    if let Ok(store) = serde_json::from_str::<Store>(text) {
        if !store.control_planes.is_empty() || text.contains("\"control_planes\"") {
            return store;
        }
    }
    // The old shape: a single `{base_url, token}`. Convert it to a one-entry
    // active list — nobody is asked to reconnect after upgrading (AC-1).
    if let Ok(old) = serde_json::from_str::<Endpoint>(text) {
        if !old.base_url.is_empty() {
            let url = normalize(&old.base_url);
            return Store {
                active: Some(url.clone()),
                control_planes: vec![ControlPlane {
                    base_url: url,
                    token: old.token,
                    label: None,
                    account: None,
                }],
            };
        }
    }
    Store::default()
}

/// `~/.config/nook/desktop.json` on Linux, and the platform equivalent
/// elsewhere — Tauri resolves it, so this lands where each OS expects rather
/// than scattering a dotfile in $HOME.
fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("no config directory: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir.join("desktop.json"))
}

fn read_store(app: &tauri::AppHandle) -> Result<Store, String> {
    let path = config_path(app)?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(parse_store(&text)),
        // Not configured yet is the ordinary first-run state, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Write the store as the new shape at `0600` — forward-only (NG-7): an older
/// build that later reads this sees no single endpoint and asks to connect.
fn write_store(app: &tauri::AppHandle, store: &Store) -> Result<(), String> {
    let path = config_path(app)?;
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    // The file holds credentials that drive every machine in the fleet.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Load the ACTIVE control plane as an `Endpoint`, migrating the on-disk file
/// forward on the way (writing the upgraded shape back so it happens once).
#[tauri::command]
fn load_endpoint(app: tauri::AppHandle) -> Result<Endpoint, String> {
    let store = read_store(&app)?;
    // Persist the migrated shape so the one-time conversion is durable.
    let _ = write_store(&app, &store);
    Ok(store.active_endpoint())
}

/// The whole store, for the control-plane switcher (rows, active, accounts).
#[tauri::command]
fn list_control_planes(app: tauri::AppHandle) -> Result<Store, String> {
    read_store(&app)
}

/// Add a control plane (or replace an existing URL's token) and make it active.
#[tauri::command]
fn add_control_plane(app: tauri::AppHandle, endpoint: Endpoint) -> Result<(), String> {
    let mut store = read_store(&app)?;
    store.upsert_active(endpoint);
    write_store(&app, &store)
}

/// Choose which stored control plane is active.
#[tauri::command]
fn set_active_control_plane(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let mut store = read_store(&app)?;
    let url = normalize(&url);
    if store.control_planes.iter().any(|c| c.base_url == url) {
        store.active = Some(url);
    }
    write_store(&app, &store)
}

/// Remove a control plane and its token; re-point active if it was active.
#[tauri::command]
fn forget_control_plane(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let mut store = read_store(&app)?;
    store.forget(&url);
    write_store(&app, &store)
}

/// Set (or clear, with an empty string) a control plane's custom label.
#[tauri::command]
fn rename_control_plane(app: tauri::AppHandle, url: String, label: String) -> Result<(), String> {
    let mut store = read_store(&app)?;
    store.rename(&url, &label);
    write_store(&app, &store)
}

/// Record which account is signed in on a server (called once `/auth/me`
/// resolves), so other rows can show it without being switched to.
#[tauri::command]
fn set_control_plane_account(
    app: tauri::AppHandle,
    url: String,
    account: String,
) -> Result<(), String> {
    let mut store = read_store(&app)?;
    store.set_account(&url, &account);
    write_store(&app, &store)
}

/// Open a URL in the OS browser.
///
/// The webview must never go anywhere itself — see `allow_navigation`. This is
/// the other half of that rule: somewhere for a link to go instead.
#[tauri::command]
async fn open_external(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("could not open that link: {e}"))
}

/// Whether the webview may navigate to `url`.
///
/// Only ever its own bundle. This app is served from `tauri://localhost` (and
/// `http://localhost:5173` while developing), and Tauri denies every command
/// above to any other origin — so a webview that wanders is an app that can no
/// longer read its own configuration or sign anybody in. It reported that as
/// "connect to a control plane", which sent people to re-enter an address that
/// had been correct all along.
///
/// The frontend intercepts link clicks and hands them to `open_external`
/// before they get here. This is the backstop for the ones it misses:
/// `window.location`, a form post, a redirect from a page we did load.
fn allow_navigation(url: &tauri::Url) -> bool {
    match url.scheme() {
        "tauri" | "asset" => true,
        "http" | "https" => matches!(
            url.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("tauri.localhost")
        ),
        _ => false,
    }
}

/// Carries `allow_navigation` as a plugin, which is where Tauri hangs that
/// hook — the app builder has no equivalent, and the alternative is building
/// the window in Rust purely to attach it, abandoning the window config.
fn nav_guard<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("nook-nav-guard")
        .on_navigation(|_webview, url| allow_navigation(url))
        .build()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
/// Where the bundled control plane and node are, as Tauri resolves them
/// (MAIN-395 AC-3).
///
/// The LOCATING is `tauri-plugin-shell`'s: `shell().sidecar(name)` builds the
/// very `Command` the boot card will spawn, so a bundle and a `pnpm tauri dev`
/// run resolve identically and nothing here hardcodes a relative path into
/// `target/`.
///
/// It does NOT start them (NG-1). That leaves one gap the plugin cannot close:
/// `sidecar()` computes a path and never checks the file is there, so `Ok`
/// alone would say nothing about whether the binary actually shipped. The
/// presence probe therefore resolves against `platform::current_exe()` — the
/// SAME base the plugin resolves against, and Tauri's own API rather than a
/// guess — purely to `exists()` the result.
///
/// Windows returns `present: false` for both, and that is correct rather than
/// broken: neither binary compiles for Windows and the node shells out to
/// `tmux`, so the ruling on this card ships Windows as a client app with no
/// `externalBin` at all (`tauri.windows.conf.json`).
#[derive(serde::Serialize)]
struct SidecarInfo {
    name: String,
    /// The sidecar API accepted the name and produced a command.
    resolved: bool,
    /// A file is actually there — the half `sidecar()` does not answer.
    present: bool,
    path: Option<String>,
}

// ── local stack boot (MAIN-396) ──────────────────────────────────────────────
//
// The bundled control plane needs three things before the webview can load: a
// database file, a port nothing else holds, and a health check that has
// actually answered. The parts that can be wrong without a running app are
// separated out here so they can be tested without one.

/// A port nothing is listening on, chosen by the OS.
///
/// Bind `:0`, read what the kernel assigned, drop the listener. There is a race
/// — anything may take it between the drop and the child's bind — and it is the
/// standard one, accepted because the alternative is worse: a literal (MAIN-376
/// is the standing lesson that a hardcoded local port does not fail loudly, it
/// silently targets whatever else is already there). A dev stack on 8080 and a
/// second copy of this app both have to coexist with it.
fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("could not find a free port: {e}"))?
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("could not read the chosen port: {e}"))
}

/// The environment the bundled control plane boots with.
///
/// Pure so it can be asserted without launching anything — the values here are
/// exactly the ones MAIN-376 showed go wrong silently. `PUBLIC_BASE_URL` and
/// `WEB_ORIGIN` must carry the CHOSEN port: left at a default they point every
/// task-key link, invite and agent-authored URL at a port nothing is serving.
///
/// Both doors are bound to LOOPBACK. The control plane's own defaults are
/// `0.0.0.0:8080` and `0.0.0.0:8081`, which are right for a server and wrong
/// for a laptop: they publish a control plane to the coffee-shop wifi, and they
/// are fixed numbers that collide with a dev stack on the same machine.
fn control_plane_env(
    db_path: &std::path::Path,
    port: u16,
    agent_port: u16,
    secrets: &LocalSecrets,
) -> Vec<(String, String)> {
    let base = format!("http://127.0.0.1:{port}");
    vec![
        // `sqlite://` selects the engine by URL, the same way boot does
        // (MAIN-195). A virgin file is the ordinary case: migrate + seed +
        // /healthz from nothing is what `tests/sqlite_boot.rs` already proves.
        (
            "DATABASE_URL".into(),
            format!("sqlite://{}", db_path.display()),
        ),
        // Never `production`: that arm makes a ledger it cannot account for
        // fatal, which is right for a server and wrong for a desktop app whose
        // database is a file the user can corrupt.
        ("APP_ENV".into(), "desktop".into()),
        // The variable the control plane actually reads. `NOOK_CONTROL_PORT` —
        // which this used to set — is a COMPOSE-side variable that publishes a
        // host port; the process itself has never read it, so the chosen port
        // was silently ignored and the health poll could never succeed.
        ("CONTROL_PLANE_BIND".into(), format!("127.0.0.1:{port}")),
        ("NOOK_AGENT_BIND".into(), format!("127.0.0.1:{agent_port}")),
        // Required — with it unset the process exits at config load with
        // "SESSION_SECRET is required" before binding anything. Persisted
        // rather than fresh per launch so a restart does not sign the user out.
        ("SESSION_SECRET".into(), secrets.session_secret.clone()),
        // What the bundled node trades for an identity (MAIN-398). Seeded into
        // the local tenant at boot; see `nook_infra::Config::local_join_token`.
        ("NOOK_LOCAL_JOIN_TOKEN".into(), secrets.join_token.clone()),
        ("PUBLIC_BASE_URL".into(), base.clone()),
        ("WEB_ORIGIN".into(), base),
    ]
}

// ── the local node (MAIN-398) ────────────────────────────────────────────────
//
// Sessions run on NODES, so a control plane with none can show a board and open
// nothing. The bundled node is an ordinary node that happens to join over
// loopback: same join, same protocol, same session handling as any machine in a
// fleet (NG-1 — none of that changes here).

/// The two credentials a local install generates once and then keeps.
///
/// Kept rather than regenerated because both are agreements between two
/// processes across restarts. A fresh `session_secret` invalidates every browser
/// session, so the person is signed out every launch; a fresh `join_token` is
/// one the already-joined node has no use for.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalSecrets {
    pub session_secret: String,
    pub join_token: String,
}

/// A credential from the OS CSPRNG. Alphanumeric so it survives a TOML string
/// and a shell environment without escaping.
fn random_secret(prefix: &str, len: usize) -> String {
    use rand::distr::Alphanumeric;
    use rand::Rng;
    let body: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect();
    format!("{prefix}{body}")
}

/// Read this install's secrets, generating them on first run.
///
/// A file that is missing, unreadable or half-written is replaced rather than
/// treated as fatal: it is regenerable state, and refusing to start because of
/// it would strand an install that nothing else is wrong with.
fn load_or_create_secrets(dir: &std::path::Path) -> Result<LocalSecrets, String> {
    let path = dir.join("local-secrets.json");
    if let Some(s) = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<LocalSecrets>(&t).ok())
        .filter(|s| !s.session_secret.is_empty() && !s.join_token.is_empty())
    {
        return Ok(s);
    }
    let secrets = LocalSecrets {
        session_secret: random_secret("", 48),
        join_token: random_secret("nook_join_", 32),
    };
    let text = serde_json::to_string(&secrets).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    restrict_to_owner(&path);
    Ok(secrets)
}

/// Best-effort `0600`. A failure is not fatal — the file is already inside the
/// app-data directory, and refusing to start over a chmod would be worse than
/// the exposure it prevents.
fn restrict_to_owner(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Where the bundled node keeps its identity — NOT `~/.config/nook`.
///
/// That path belongs to the person's own `nook` CLI. A desktop install writing
/// `node.toml` there would overwrite the identity of a machine they joined to a
/// real fleet, and `nook join` overwrites without asking.
fn node_config_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app-data directory: {e}"))?
        .join("node");
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The ports the local node advertises for MAIN-301 leasing (AC-5).
///
/// A choice, not a default. It continues this repo's own convention (the
/// operator node leases `4100-4199`, the dev node `4200-4299`), sits well below
/// the ephemeral range a kernel hands out on its own, and 100 ports is nine
/// concurrent sessions at eleven declared listeners each. Advertising nothing
/// was the alternative and is worse: a workspace with a `required` listener
/// then cannot start a session at all, and the allocator already recovers from
/// a port that turns out to be busy (`lease_for_avoiding`).
const LOCAL_PORT_RANGE: &str = "4300-4399";

/// The environment both `nook join` and `nook run` get.
fn node_env(config_dir: &std::path::Path) -> Vec<(String, String)> {
    vec![
        ("NOOK_CONFIG_DIR".into(), config_dir.display().to_string()),
        ("NOOK_PORT_RANGE".into(), LOCAL_PORT_RANGE.into()),
    ]
}

/// The join spec handed to `nook join --config`.
///
/// A file rather than `--token` on the command line: process arguments are
/// readable by every other user on the machine, and this one enrolls a node.
/// No escaping is needed because both values are constructed here — the token
/// is alphanumeric by generation and the URL is a loopback address.
fn join_spec_toml(base_url: &str, token: &str) -> String {
    format!("server = \"{base_url}\"\ntoken = \"{token}\"\n")
}

/// How long the node must stay up before a restart stops counting as a flap.
const NODE_SETTLED_SECS: u64 = 30;

/// Consecutive fast failures before the UI is told rather than shown a node
/// that keeps almost-starting (AC-2).
const NODE_FLAPPING_AFTER: u32 = 3;

/// Backoff between restarts: 1s doubling to a 32s ceiling.
///
/// A node that cannot start at all — a missing binary, a control plane that
/// went away — must not spin a core while the window sits there.
fn restart_delay(consecutive_failures: u32) -> std::time::Duration {
    std::time::Duration::from_secs(1 << consecutive_failures.min(5))
}

/// Append to a bounded tail. A boot log is unbounded and the UI wants its end,
/// which is where the failure is.
fn push_tail(buf: &mut String, line: &str) {
    buf.push_str(line);
    if buf.len() > 16_384 {
        let cut = buf.len() - 8_192;
        *buf = buf.split_off(cut);
    }
}

/// Where the local database lives, under the OS-conventional app-data directory
/// Tauri resolves (AC-5): `~/.local/share/<id>` on Linux,
/// `~/Library/Application Support/<id>` on macOS, `%APPDATA%\<id>` on Windows.
fn local_db_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app-data directory: {e}"))?;
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir.join("nook.db"))
}

/// Whether `/healthz` has answered 200 yet.
async fn healthy(base: &str) -> bool {
    reqwest::Client::new()
        .get(format!("{base}/healthz"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// How the local stack came up, or did not — what the UI renders instead of a
/// blank window (AC-3).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalStack {
    /// `http://127.0.0.1:<port>` once healthy.
    pub base_url: String,
    pub ready: bool,
    /// Present only on failure: the child's own output, so a control plane that
    /// could not start explains itself rather than showing nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the bundled node PROCESS is running (MAIN-398 AC-2). Not whether
    /// the control plane has seen it: that is what the Nodes page shows, and
    /// this is the half only the shell can know.
    #[serde(default)]
    pub node_ready: bool,
    /// Set once the node has failed to stay up several times running, so a node
    /// that is quietly gone is visible rather than merely absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_error: Option<String>,
}

#[tauri::command]
#[allow(clippy::unused_async)]
async fn local_stack(state: tauri::State<'_, LocalStackState>) -> Result<LocalStack, String> {
    Ok(state.0.lock().unwrap().clone())
}

/// The boot result, shared with the webview through `local_stack`.
pub struct LocalStackState(pub std::sync::Mutex<LocalStack>);

#[tauri::command]
fn sidecars(app: tauri::AppHandle) -> Vec<SidecarInfo> {
    use tauri_plugin_shell::ShellExt;

    let base = tauri::utils::platform::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));

    ["nook", "nook-control"]
        .into_iter()
        .map(|name| {
            let resolved = app.shell().sidecar(name).is_ok();
            let path = base.as_ref().map(|b| {
                let mut p = b.join(name);
                if cfg!(windows) {
                    p.set_extension("exe");
                }
                p
            });
            SidecarInfo {
                name: name.to_string(),
                resolved,
                present: path.as_ref().is_some_and(|p| p.exists()),
                path: path.map(|p| p.display().to_string()),
            }
        })
        .collect()
}

/// Start the bundled control plane and record when it is serving (MAIN-396).
///
/// Spawned rather than awaited: `setup` must return for the window to appear,
/// and AC-3 wants the window up showing progress — a blank window is the thing
/// being avoided, so the UI is what waits, not the process.
///
/// The child's stdout/stderr are kept and handed back on failure. A control
/// plane that dies during migration says why; without this the user sees an app
/// that simply never loads.
fn start_local_stack(app: tauri::AppHandle) {
    use tauri_plugin_shell::process::CommandEvent;
    use tauri_plugin_shell::ShellExt;

    tauri::async_runtime::spawn(async move {
        let set = |s: LocalStack| {
            if let Some(st) = app.try_state::<LocalStackState>() {
                *st.0.lock().unwrap() = s;
            }
        };
        let fail = |msg: String| LocalStack {
            error: Some(msg),
            ..LocalStack::default()
        };

        let db = match local_db_path(&app) {
            Ok(p) => p,
            Err(e) => return set(fail(e)),
        };
        let secrets = match db
            .parent()
            .ok_or_else(|| "the database path has no directory".to_string())
            .and_then(load_or_create_secrets)
        {
            Ok(s) => s,
            Err(e) => return set(fail(e)),
        };
        // Both doors get their own free port. Binding them is all-or-nothing in
        // the control plane, so a fixed agent port that something else holds
        // takes the whole boot down with it.
        let (port, agent_port) = match (free_port(), free_port()) {
            (Ok(p), Ok(a)) => (p, a),
            (Err(e), _) | (_, Err(e)) => return set(fail(e)),
        };
        let base = format!("http://127.0.0.1:{port}");

        let cmd = match app.shell().sidecar("nook-control") {
            Ok(c) => c.envs(control_plane_env(&db, port, agent_port, &secrets)),
            Err(e) => return set(fail(format!("the bundled control plane is missing: {e}"))),
        };
        let (mut rx, _child) = match cmd.spawn() {
            Ok(v) => v,
            Err(e) => return set(fail(format!("could not start the control plane: {e}"))),
        };

        // Drain the child's output continuously. Reading it only on failure
        // would deadlock a chatty child on a full pipe, and the log is wanted
        // precisely in the case where it never becomes healthy.
        let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = log.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let line = match ev {
                    CommandEvent::Stdout(b) | CommandEvent::Stderr(b) => {
                        String::from_utf8_lossy(&b).to_string()
                    }
                    _ => continue,
                };
                push_tail(&mut sink.lock().unwrap(), &line);
            }
        });

        // Poll rather than parse the log for a ready line: /healthz answering is
        // the actual contract, and a log format is not.
        for _ in 0..120 {
            if healthy(&base).await {
                set(LocalStack {
                    base_url: base.clone(),
                    ready: true,
                    ..LocalStack::default()
                });
                // Only now: the node has nothing to join until the control
                // plane answers.
                return supervise_local_node(app, base, secrets.join_token);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let tail = log.lock().unwrap().clone();
        set(fail(format!(
            "the control plane did not answer {base}/healthz within 60s\n\n{tail}"
        )));
    });
}

/// What one `nook run` did before it stopped.
struct NodeRun {
    ran_for: std::time::Duration,
    tail: String,
}

/// Enroll this machine's bundled node, once ever (AC-1).
///
/// Guarded by `node.toml` rather than a flag of our own: that file IS the
/// record of having joined, and re-joining an existing node rotates its token
/// on the server, which would strand the config we already hold.
async fn join_local_node(
    app: &tauri::AppHandle,
    base: &str,
    token: &str,
    dir: &std::path::Path,
) -> Result<(), String> {
    use tauri_plugin_shell::process::CommandEvent;
    use tauri_plugin_shell::ShellExt;

    if dir.join("node.toml").exists() {
        return Ok(());
    }
    let spec = dir.join("join.toml");
    fs::write(&spec, join_spec_toml(base, token))
        .map_err(|e| format!("could not write {}: {e}", spec.display()))?;
    restrict_to_owner(&spec);

    let run = async {
        let (mut rx, _child) = app
            .shell()
            .sidecar("nook")
            .map_err(|e| format!("the bundled node is missing: {e}"))?
            .envs(node_env(dir))
            .args(["join", "--config", &spec.display().to_string()])
            .spawn()
            .map_err(|e| format!("could not start the node: {e}"))?;

        let mut tail = String::new();
        let mut code = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                CommandEvent::Stdout(b) | CommandEvent::Stderr(b) => {
                    push_tail(&mut tail, &String::from_utf8_lossy(&b));
                }
                CommandEvent::Terminated(p) => code = p.code,
                _ => {}
            }
        }
        match code {
            Some(0) => Ok(()),
            _ => Err(format!("the node could not join {base}\n\n{tail}")),
        }
    }
    .await;

    // The spec carries a credential; it does not outlive the command that
    // needed it, on either path.
    let _ = fs::remove_file(&spec);
    run
}

/// Run the node once, returning when it stops.
async fn run_node_once(app: &tauri::AppHandle, dir: &std::path::Path) -> Result<NodeRun, String> {
    use tauri_plugin_shell::process::CommandEvent;
    use tauri_plugin_shell::ShellExt;

    let (mut rx, _child) = app
        .shell()
        .sidecar("nook")
        .map_err(|e| format!("the bundled node is missing: {e}"))?
        .envs(node_env(dir))
        .args(["run"])
        .spawn()
        .map_err(|e| format!("could not start the node: {e}"))?;

    let started = std::time::Instant::now();
    let mut tail = String::new();
    while let Some(ev) = rx.recv().await {
        if let CommandEvent::Stdout(b) | CommandEvent::Stderr(b) = ev {
            push_tail(&mut tail, &String::from_utf8_lossy(&b));
        }
    }
    Ok(NodeRun {
        ran_for: started.elapsed(),
        tail,
    })
}

/// Join once, then keep the node running for as long as the app is up (AC-2).
fn supervise_local_node(app: tauri::AppHandle, base: String, token: String) {
    tauri::async_runtime::spawn(async move {
        let node = |ready: bool, error: Option<String>| {
            if let Some(st) = app.try_state::<LocalStackState>() {
                let mut s = st.0.lock().unwrap();
                s.node_ready = ready;
                s.node_error = error;
            }
        };

        let dir = match node_config_dir(&app) {
            Ok(d) => d,
            Err(e) => return node(false, Some(e)),
        };
        if let Err(e) = join_local_node(&app, &base, &token, &dir).await {
            return node(false, Some(e));
        }

        let mut consecutive = 0u32;
        loop {
            node(true, None);
            let run = match run_node_once(&app, &dir).await {
                Ok(r) => r,
                // Spawning failed rather than the node exiting: the binary is
                // missing or unrunnable, and retrying that on a timer would only
                // hide it.
                Err(e) => return node(false, Some(e)),
            };
            consecutive = if run.ran_for >= std::time::Duration::from_secs(NODE_SETTLED_SECS) {
                0
            } else {
                consecutive + 1
            };
            node(
                false,
                (consecutive >= NODE_FLAPPING_AFTER).then(|| {
                    format!(
                        "the local node has stopped {consecutive} times without staying up — \
                         sessions will not start\n\n{}",
                        run.tail
                    )
                }),
            );
            tokio::time::sleep(restart_delay(consecutive)).await;
        }
    });
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        // The control plane and the node travel INSIDE the bundle as sidecars
        // (MAIN-395). This plugin is how they are located; starting them is the
        // boot card's job, not this one's.
        .plugin(tauri_plugin_shell::init())
        .plugin(nav_guard())
        .invoke_handler(tauri::generate_handler![
            load_endpoint,
            list_control_planes,
            add_control_plane,
            set_active_control_plane,
            forget_control_plane,
            rename_control_plane,
            set_control_plane_account,
            open_external,
            device_start,
            device_poll,
            update_check,
            update_install,
            sidecars,
            local_stack
        ])
        // Locate the sidecars ONCE at startup and say what was found. This is
        // what makes AC-3 a runtime fact rather than a command nobody calls:
        // the log line names both resolved paths, so "did the binaries ship in
        // this bundle?" is answerable from a signed build without a debugger.
        // It starts neither of them (NG-1).
        .manage(LocalStackState(
            std::sync::Mutex::new(LocalStack::default()),
        ))
        .setup(|app| {
            for s in sidecars(app.handle().clone()) {
                eprintln!(
                    "sidecar {}: resolved={} present={} at {}",
                    s.name,
                    s.resolved,
                    s.present,
                    s.path.as_deref().unwrap_or("<unresolved>")
                );
            }
            start_local_stack(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running NookOS desktop");
}

#[cfg(test)]
mod nav_tests {
    use super::allow_navigation;

    fn allowed(u: &str) -> bool {
        allow_navigation(&tauri::Url::parse(u).expect("test url"))
    }

    #[test]
    fn own_bundle_and_dev_server_are_allowed() {
        assert!(allowed("tauri://localhost/board"));
        assert!(allowed("http://localhost:5173/sessions/abc"));
        assert!(allowed("http://127.0.0.1:5173/"));
        // Windows serves the bundle from here.
        assert!(allowed("http://tauri.localhost/"));
    }

    #[test]
    fn control_planes_and_providers_are_not() {
        // The exact navigation that stranded the app on the connect screen: a
        // notification link is absolute, and its origin is not this app.
        assert!(!allowed("https://nook.hein.network/board?task=MAIN-9"));
        // And the one that made device sign-in impossible to finish.
        assert!(!allowed(
            "https://id.example.com/device?user_code=ABCD-EFGH"
        ));
        assert!(!allowed("http://nook.hein.network/"));
    }

    #[test]
    fn other_schemes_are_not() {
        assert!(!allowed("file:///etc/passwd"));
        assert!(!allowed("javascript:alert(1)"));
    }
}

#[cfg(test)]
mod store_tests {
    use super::{parse_store, Endpoint, Store};

    #[test]
    fn old_single_endpoint_migrates_to_a_one_entry_active_list() {
        // The pre-list shape on disk: a bare {base_url, token}.
        let store =
            parse_store(r#"{"base_url":"https://nook.example.com/","token":"nook_user_abc"}"#);
        assert_eq!(store.control_planes.len(), 1, "one entry");
        assert_eq!(
            store.active.as_deref(),
            Some("https://nook.example.com"),
            "and it is active (trailing slash normalized away)"
        );
        let ep = store.active_endpoint();
        assert_eq!(ep.base_url, "https://nook.example.com");
        assert_eq!(ep.token, "nook_user_abc", "nobody is asked to reconnect");
    }

    #[test]
    fn the_new_list_shape_round_trips() {
        let json = r#"{"control_planes":[{"base_url":"https://a","token":"t1","label":"work"}],"active":"https://a"}"#;
        let store = parse_store(json);
        assert_eq!(store.control_planes.len(), 1);
        assert_eq!(store.control_planes[0].label.as_deref(), Some("work"));
        assert_eq!(store.active.as_deref(), Some("https://a"));
    }

    #[test]
    fn an_empty_or_unconfigured_file_is_an_empty_store() {
        assert!(parse_store("").control_planes.is_empty());
        assert!(parse_store("{}").control_planes.is_empty());
        assert!(parse_store(r#"{"base_url":"","token":""}"#)
            .control_planes
            .is_empty());
        // An empty new-shape list stays empty and active-less, not misread as old.
        let empty = parse_store(r#"{"control_planes":[],"active":null}"#);
        assert!(empty.control_planes.is_empty());
        assert!(empty.active.is_none());
    }

    fn ep(url: &str, token: &str) -> Endpoint {
        Endpoint {
            base_url: url.into(),
            token: token.into(),
        }
    }

    #[test]
    fn adding_an_existing_url_replaces_its_token_rather_than_appending() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.upsert_active(ep("https://b", "t2"));
        assert_eq!(store.control_planes.len(), 2);
        assert_eq!(store.active.as_deref(), Some("https://b"));

        // Re-adding a with a fresh token: one row, new token, still one 'a'.
        store.upsert_active(ep("https://a/", "t1-new"));
        assert_eq!(store.control_planes.len(), 2, "no duplicate row (AC-5)");
        let a = store
            .control_planes
            .iter()
            .find(|c| c.base_url == "https://a")
            .unwrap();
        assert_eq!(a.token, "t1-new", "token replaced");
        assert_eq!(
            store.active.as_deref(),
            Some("https://a"),
            "and made active"
        );
    }

    #[test]
    fn upsert_preserves_a_rename() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.rename("https://a", "work");
        store.upsert_active(ep("https://a", "t1-new")); // re-auth
        let a = &store.control_planes[0];
        assert_eq!(a.label.as_deref(), Some("work"), "rename survives re-auth");
        assert_eq!(a.token, "t1-new");
    }

    #[test]
    fn forgetting_the_active_entry_repoints_active_to_the_first_remaining() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.upsert_active(ep("https://b", "t2"));
        store.upsert_active(ep("https://c", "t3")); // c is active
        store.forget("https://c");
        assert_eq!(store.control_planes.len(), 2);
        assert_eq!(
            store.active.as_deref(),
            Some("https://a"),
            "active re-points to the first remaining (AC-7)"
        );
    }

    #[test]
    fn forgetting_a_non_active_entry_leaves_active_alone() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.upsert_active(ep("https://b", "t2")); // b active
        store.forget("https://a");
        assert_eq!(store.active.as_deref(), Some("https://b"));
        assert_eq!(store.control_planes.len(), 1);
    }

    #[test]
    fn forgetting_the_last_entry_clears_active() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.forget("https://a");
        assert!(store.control_planes.is_empty());
        assert!(store.active.is_none(), "no server left to be active");
    }

    #[test]
    fn rename_with_empty_string_clears_the_label() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.rename("https://a", "work");
        assert_eq!(store.control_planes[0].label.as_deref(), Some("work"));
        store.rename("https://a", "   ");
        assert!(store.control_planes[0].label.is_none(), "blank clears it");
    }

    #[test]
    fn set_account_records_who_signed_in() {
        let mut store = Store::default();
        store.upsert_active(ep("https://a", "t1"));
        store.set_account("https://a/", "me@example.com");
        assert_eq!(
            store.control_planes[0].account.as_deref(),
            Some("me@example.com")
        );
    }
}

// ── signing in ───────────────────────────────────────────────────────────
//
// The device authorization grant, run from Rust. It could not run in the
// webview: a request from `tauri://localhost` to the identity provider is
// cross-origin, and no provider is going to add CORS for a desktop app's
// private scheme. From here there is no origin and no preflight.

#[derive(Debug, Deserialize)]
struct Providers {
    #[serde(default)]
    oidc: bool,
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    device_authorization_endpoint: Option<String>,
    #[serde(default)]
    device_client_id: Option<String>,
}

/// What the person needs to see, plus what the next call needs to continue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStart {
    pub user_code: String,
    /// Already carries the code where the provider offers it, so the browser
    /// needs no typing.
    pub verification_uri: String,
    pub device_code: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

async fn get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    reqwest::get(url)
        .await
        .map_err(|e| format!("cannot reach {url}: {e}"))?
        .json::<T>()
        .await
        .map_err(|e| format!("unexpected reply from {url}: {e}"))
}

/// Ask the provider to start an authorization, and hand back the code to show.
#[tauri::command]
async fn device_start(server: String) -> Result<DeviceStart, String> {
    let server = server.trim_end_matches('/').to_string();

    // Where the provider is comes from the control plane, not from here: an app
    // carrying its own copy would need reconfiguring whenever an operator
    // changed theirs.
    let providers: Providers = get_json(&format!("{server}/api/v1/auth/providers")).await?;
    if !providers.oidc {
        return Err(
            "this control plane has no identity provider — sign in with a \
                    username and password, or paste a token"
                .into(),
        );
    }
    let endpoint = providers
        .device_authorization_endpoint
        .ok_or("the identity provider does not advertise a device authorization endpoint")?;
    let client_id = providers
        .device_client_id
        .ok_or("no public client is configured for native sign-in")?;
    let issuer = providers
        .oidc_issuer
        .ok_or("the control plane did not say which identity provider it uses")?;

    #[derive(Deserialize)]
    struct Meta {
        token_endpoint: String,
    }
    // Read, not constructed: `{issuer}/token` is right for some providers and a
    // guess for the rest.
    let meta: Meta = get_json(&format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ))
    .await?;

    #[derive(Deserialize)]
    struct Started {
        device_code: String,
        user_code: String,
        verification_uri: String,
        #[serde(default)]
        verification_uri_complete: Option<String>,
        #[serde(default)]
        interval: Option<u64>,
        #[serde(default)]
        expires_in: Option<u64>,
    }
    let started: Started = reqwest::Client::new()
        .post(&endpoint)
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "openid profile email"),
        ])
        .send()
        .await
        .map_err(|e| format!("cannot reach {endpoint}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("the device authorization reply was not RFC 8628 shaped: {e}"))?;

    Ok(DeviceStart {
        user_code: started.user_code,
        verification_uri: started
            .verification_uri_complete
            .unwrap_or(started.verification_uri),
        device_code: started.device_code,
        token_endpoint: meta.token_endpoint,
        client_id,
        interval_secs: started.interval.unwrap_or(5).max(1),
        expires_in_secs: started.expires_in.unwrap_or(600),
    })
}

/// One poll. The UI drives the loop so it can keep showing the code, offer a
/// cancel, and stay responsive — a command that blocked until approval would
/// freeze the window for up to ten minutes.
#[tauri::command]
async fn device_poll(server: String, start: DeviceStart) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct TokenReply {
        #[serde(default)]
        id_token: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }
    let reply: TokenReply = reqwest::Client::new()
        .post(&start.token_endpoint)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", start.device_code.as_str()),
            ("client_id", start.client_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("cannot reach the token endpoint: {e}"))?
        .json()
        .await
        .map_err(|e| format!("the token endpoint replied with something unexpected: {e}"))?;

    let Some(id_token) = reply.id_token else {
        return match reply.error.as_deref() {
            // Still waiting. `slow_down` is an instruction; the caller widens
            // its interval rather than being told anything is wrong.
            Some("authorization_pending") | Some("slow_down") => Ok(None),
            Some("access_denied") => Err("that request was declined".into()),
            Some("expired_token") => Err("the code expired — start again".into()),
            Some(other) => Err(format!("the identity provider refused: {other}")),
            None => Err("neither a token nor an error came back".into()),
        };
    };

    // Trade the provider's assertion for a credential of this control plane's
    // own, so what gets stored is revocable from its tokens list.
    #[derive(Deserialize)]
    struct Exchanged {
        token: String,
    }
    let exchanged: Exchanged = reqwest::Client::new()
        .post(format!(
            "{}/api/v1/auth/oidc/exchange",
            server.trim_end_matches('/')
        ))
        .json(&serde_json::json!({ "id_token": id_token, "client_name": "NookOS desktop" }))
        .send()
        .await
        .map_err(|e| format!("cannot reach the control plane: {e}"))?
        .error_for_status()
        .map_err(|e| format!("the control plane refused that identity: {e}"))?
        .json()
        .await
        .map_err(|e| format!("unexpected reply from the control plane: {e}"))?;

    Ok(Some(exchanged.token))
}

// ── updates ──────────────────────────────────────────────────────────────
//
// GitHub is the right source here, and the distinction from nodes is the
// point. A node speaks a private protocol with its control plane, so it takes
// its version from the control plane or the two can drift apart. This app
// speaks the public HTTP API and shares no protocol, so it can follow releases
// on its own without being able to outrun anything.

/// What an available update looks like to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct Available {
    pub version: String,
    pub current: String,
    pub notes: String,
}

/// Is there a newer release? `None` means this is current.
///
/// Checking and installing are separate on purpose: an app that updated itself
/// the moment it found something would restart out from under whatever the
/// person was reading.
#[tauri::command]
async fn update_check(app: tauri::AppHandle) -> Result<Option<Available>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let current = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
        .map_err(|e| format!("cannot check for updates: {e}"))?;

    Ok(update.map(|u| Available {
        version: u.version.clone(),
        current,
        notes: u.body.clone().unwrap_or_default(),
    }))
}

/// Download, verify and install, then restart.
///
/// The signature is checked by the plugin against the public key compiled into
/// this build — that is what makes an update from GitHub trustworthy without
/// trusting GitHub itself.
#[tauri::command]
async fn update_install(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?
        .check()
        .await
        .map_err(|e| format!("cannot check for updates: {e}"))?
        .ok_or("already up to date")?;

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| format!("update failed: {e}"))?;

    app.restart();
}

#[cfg(test)]
mod boot_tests {
    use super::*;

    /// AC-2: the port is chosen at runtime and is genuinely free.
    ///
    /// Two calls in a row must not hand back the same number while the first is
    /// still bound — that is the collision the whole card exists to avoid, and
    /// it is what a literal guarantees.
    #[test]
    fn a_chosen_port_is_free_and_not_a_literal() {
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let taken = held.local_addr().unwrap().port();

        let p = free_port().expect("a free port");
        assert_ne!(p, 0, "0 is the ASK, never the answer");
        assert_ne!(p, taken, "must not hand back a port already bound");

        // And it is actually bindable — the point of asking the OS.
        std::net::TcpListener::bind(("127.0.0.1", p)).expect("the chosen port binds");
    }

    fn secrets() -> LocalSecrets {
        LocalSecrets {
            session_secret: "s3cr3t".into(),
            join_token: "nook_join_abc".into(),
        }
    }

    fn env_of(port: u16, agent_port: u16) -> Vec<(String, String)> {
        control_plane_env(
            std::path::Path::new("/tmp/x/nook.db"),
            port,
            agent_port,
            &secrets(),
        )
    }

    fn get(env: &[(String, String)], k: &str) -> String {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{k} is not set"))
    }

    /// AC-4: the URLs carry the CHOSEN port.
    ///
    /// MAIN-376's lesson, and the reason this is asserted rather than assumed:
    /// a stale literal here does not fail — it sends every task-key link,
    /// invite and agent-authored URL to a port nothing is serving.
    #[test]
    fn the_public_urls_carry_the_chosen_port() {
        let env = env_of(41007, 41008);
        assert_eq!(get(&env, "PUBLIC_BASE_URL"), "http://127.0.0.1:41007");
        assert_eq!(get(&env, "WEB_ORIGIN"), "http://127.0.0.1:41007");
        for (_, v) in &env {
            assert!(!v.contains("8080"), "no 8080 literal may survive: {v}");
        }
    }

    /// AC-4: and the control plane actually BINDS it.
    ///
    /// The variable this asserts is the one the process reads. MAIN-396 set
    /// `NOOK_CONTROL_PORT`, which is a compose-side variable that publishes a
    /// host port — `nook_infra::Config` has never looked at it — so the chosen
    /// port was ignored, the server came up on `0.0.0.0:8080`, and the health
    /// poll it was gating could not succeed.
    #[test]
    fn both_doors_bind_the_chosen_ports_on_loopback() {
        let env = env_of(41007, 41008);
        assert_eq!(get(&env, "CONTROL_PLANE_BIND"), "127.0.0.1:41007");
        assert_eq!(get(&env, "NOOK_AGENT_BIND"), "127.0.0.1:41008");
        for k in ["CONTROL_PLANE_BIND", "NOOK_AGENT_BIND"] {
            assert!(
                !get(&env, k).starts_with("0.0.0.0"),
                "{k} must not publish a desktop control plane to the network"
            );
        }
    }

    /// AC-4: without this the child exits at config load, before binding
    /// anything, and the only symptom is a health check that never passes.
    #[test]
    fn the_session_secret_is_supplied() {
        let env = env_of(1, 2);
        assert_eq!(get(&env, "SESSION_SECRET"), "s3cr3t");
    }

    /// AC-1: the join token reaches the control plane so it can seed it, and
    /// the person is never in the loop.
    #[test]
    fn the_join_token_is_handed_to_the_control_plane() {
        let env = env_of(1, 2);
        assert_eq!(get(&env, "NOOK_LOCAL_JOIN_TOKEN"), "nook_join_abc");
        // Not the dev variable: that one also seeds the dogfood workspace, a
        // dev identity and loops-on, none of which belong on a laptop.
        assert!(
            !env.iter().any(|(n, _)| n == "NOOK_DEV_JOIN_TOKEN"),
            "the dev variable drags compose-stack scaffolding along with it"
        );
    }

    /// AC-1: a generated token is a credential, and it has to survive being
    /// written into TOML and read back without escaping.
    #[test]
    fn generated_secrets_are_long_and_alphanumeric() {
        let token = random_secret("nook_join_", 32);
        assert!(token.starts_with("nook_join_"));
        let body = &token["nook_join_".len()..];
        assert_eq!(body.len(), 32);
        assert!(
            body.chars().all(|c| c.is_ascii_alphanumeric()),
            "{token} would need escaping"
        );
        assert_ne!(
            random_secret("", 48),
            random_secret("", 48),
            "two calls must not agree"
        );
    }

    /// AC-1: the spec `nook join --config` reads.
    #[test]
    fn the_join_spec_names_the_local_server_and_token() {
        let spec = join_spec_toml("http://127.0.0.1:41007", "nook_join_abc");
        assert_eq!(
            spec,
            "server = \"http://127.0.0.1:41007\"\ntoken = \"nook_join_abc\"\n"
        );
    }

    /// AC-5: the node advertises the chosen range, and AC-3's root is left to
    /// the node's own default rather than overridden here.
    #[test]
    fn the_node_advertises_its_port_range_and_its_own_config_dir() {
        let env = node_env(std::path::Path::new("/home/a/.local/share/nook/node"));
        assert_eq!(get(&env, "NOOK_PORT_RANGE"), "4300-4399");
        assert_eq!(
            get(&env, "NOOK_CONFIG_DIR"),
            "/home/a/.local/share/nook/node",
            "never ~/.config/nook — that is the person's own CLI identity"
        );
        assert!(
            !env.iter().any(|(n, _)| n == "NOOK_WORKSPACE_ROOT"),
            "AC-3 takes the node's own ~/.nook/workspace/<tenant> default"
        );
    }

    /// AC-2: backoff grows and then stops growing. A node that cannot start
    /// must neither spin nor drift into never retrying.
    #[test]
    fn restart_backoff_climbs_to_a_ceiling() {
        assert_eq!(restart_delay(0).as_secs(), 1);
        assert_eq!(restart_delay(1).as_secs(), 2);
        assert_eq!(restart_delay(3).as_secs(), 8);
        assert_eq!(restart_delay(5).as_secs(), 32);
        assert_eq!(restart_delay(50).as_secs(), 32, "capped, not overflowing");
    }

    /// AC-2: the log tail stays bounded, so a chatty node cannot grow the
    /// state the UI reads without limit.
    #[test]
    fn the_log_tail_is_bounded_and_keeps_the_end() {
        let mut buf = String::new();
        for i in 0..4000 {
            push_tail(&mut buf, &format!("line {i}\n"));
        }
        assert!(buf.len() <= 16_384, "bounded, was {}", buf.len());
        assert!(buf.ends_with("line 3999\n"), "the END is what explains it");
    }

    /// AC-2: the state the webview reads distinguishes "no node yet" from
    /// "the node is gone", which is the difference AC-2 is about.
    #[test]
    fn a_fresh_state_reports_no_node_and_no_node_failure() {
        let s = LocalStack::default();
        assert!(!s.node_ready);
        assert!(s.node_error.is_none());
    }

    /// AC-1: a SQLite URL at the app-data path, and an APP_ENV that is not
    /// production — a desktop database is a file the user can corrupt, and the
    /// production arm makes an unaccountable ledger fatal.
    #[test]
    fn the_database_is_sqlite_at_the_given_path_and_env_is_not_production() {
        let env = control_plane_env(
            std::path::Path::new("/home/a/.local/share/nook/nook.db"),
            1,
            2,
            &secrets(),
        );
        let url = get(&env, "DATABASE_URL");
        assert_eq!(url, "sqlite:///home/a/.local/share/nook/nook.db");
        assert!(
            url.starts_with("sqlite://"),
            "engine is selected by URL scheme"
        );

        assert_ne!(get(&env, "APP_ENV"), "production");
    }

    /// The state the webview reads starts as not-ready with no error, so the UI
    /// shows progress rather than either a blank window or a false failure.
    #[test]
    fn the_initial_state_is_pending_not_failed() {
        let s = LocalStack::default();
        assert!(!s.ready);
        assert!(s.error.is_none());
        assert!(s.base_url.is_empty());
    }
}
