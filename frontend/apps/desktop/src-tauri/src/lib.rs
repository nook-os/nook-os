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
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
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
            update_install
        ])
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
