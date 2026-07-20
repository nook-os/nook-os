// The desktop shell wraps the same @nookos/app the web serves. No custom
// commands in milestone 1 — this proves the packaging path.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running NookOS desktop");
}
