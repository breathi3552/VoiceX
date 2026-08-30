//! Tauri commands for the shared explicit HTTP proxy setting.

use tauri::AppHandle;

#[tauri::command]
pub fn get_http_proxy() -> String {
    crate::network_proxy::current_http_proxy()
}

#[tauri::command]
pub fn set_http_proxy(app: AppHandle, proxy: String) -> Result<String, String> {
    crate::network_proxy::update(&app, &proxy)
}
