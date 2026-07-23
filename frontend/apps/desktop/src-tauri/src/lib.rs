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

/// What the app needs to reach a control plane.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// e.g. `https://nook.example.com`
    #[serde(default)]
    pub base_url: String,
    /// A `nook_user_…` token. Sent as a bearer, never as a cookie.
    #[serde(default)]
    pub token: String,
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

#[tauri::command]
fn load_endpoint(app: tauri::AppHandle) -> Result<Endpoint, String> {
    let path = config_path(&app)?;
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("{} is unreadable: {e}", path.display()))
        }
        // Not configured yet is the ordinary first-run state, not a failure.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Endpoint::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

#[tauri::command]
fn save_endpoint(app: tauri::AppHandle, endpoint: Endpoint) -> Result<(), String> {
    let path = config_path(&app)?;
    let text = serde_json::to_string_pretty(&endpoint).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    // The file holds a credential that drives every machine in the fleet.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Forget the stored credential — "sign out" for a client with no session.
#[tauri::command]
fn clear_endpoint(app: tauri::AppHandle) -> Result<(), String> {
    let path = config_path(&app)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            load_endpoint,
            save_endpoint,
            clear_endpoint,
            device_start,
            device_poll
        ])
        .run(tauri::generate_context!())
        .expect("error while running NookOS desktop");
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
        return Err("this control plane has no identity provider — sign in with a \
                    username and password, or paste a token"
            .into());
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
