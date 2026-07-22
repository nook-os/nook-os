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
            clear_endpoint
        ])
        .run(tauri::generate_context!())
        .expect("error while running NookOS desktop");
}
