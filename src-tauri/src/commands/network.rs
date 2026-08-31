//! Tauri commands for the global network proxy configuration.

use crate::network_proxy::{NetworkProxyConfig, NetworkProxyMode};
use tauri::AppHandle;

#[tauri::command]
pub fn get_network_proxy_config() -> NetworkProxyConfig {
    crate::network_proxy::current_config()
}

#[tauri::command]
pub fn set_network_proxy_config(
    app: AppHandle,
    config: NetworkProxyConfig,
) -> Result<NetworkProxyConfig, String> {
    crate::network_proxy::update(&app, config)
}

// Keep the previous command names working for older frontend bundles during
// upgrades. Empty legacy values map to Direct; non-empty values map to Custom.
#[tauri::command]
pub fn get_http_proxy() -> String {
    crate::network_proxy::current_http_proxy()
}

#[tauri::command]
pub fn set_http_proxy(app: AppHandle, proxy: String) -> Result<String, String> {
    crate::network_proxy::update_legacy_http_proxy(&app, &proxy)
}

#[allow(dead_code)]
fn _assert_mode_is_public(_: NetworkProxyMode) {}
