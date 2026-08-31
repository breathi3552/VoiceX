//! Global network proxy configuration shared by cloud transports.
//!
//! The persisted configuration has an explicit mode. Legacy `httpProxy`
//! settings are migrated without losing the previously saved proxy URL.

use serde::{Deserialize, Serialize};
use std::io;
use std::sync::{LazyLock, RwLock};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_HTTP_PROXY: &str = "http://127.0.0.1:7890";
const SETTINGS_FILE: &str = "network-proxy.json";
const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProxyMode {
    Direct,
    System,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProxyConfig {
    pub mode: NetworkProxyMode,
    #[serde(default = "default_custom_proxy")]
    pub custom_proxy: String,
}

impl Default for NetworkProxyConfig {
    fn default() -> Self {
        Self {
            // Preserve VoiceX's existing first-run behavior from the previous
            // proxy implementation while making the mode explicit.
            mode: NetworkProxyMode::Custom,
            custom_proxy: DEFAULT_HTTP_PROXY.to_string(),
        }
    }
}

fn default_custom_proxy() -> String {
    DEFAULT_HTTP_PROXY.to_string()
}

static NETWORK_PROXY: LazyLock<RwLock<NetworkProxyConfig>> =
    LazyLock::new(|| RwLock::new(NetworkProxyConfig::default()));

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyNetworkProxySettings {
    #[serde(default)]
    http_proxy: String,
}

pub fn init(app: &AppHandle) -> Result<(), String> {
    let path = settings_path(app)?;
    let (config, should_persist) = match std::fs::read_to_string(&path) {
        Ok(raw) => match parse_stored_settings(&raw) {
            Ok((config, migrated)) => (config, migrated),
            Err(err) => {
                log::warn!("Invalid network proxy settings, using default: {err}");
                (NetworkProxyConfig::default(), true)
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => (NetworkProxyConfig::default(), true),
        Err(err) => return Err(format!("Failed to read network proxy settings: {err}")),
    };

    validate_config(&config)?;
    set_in_memory(&config)?;
    if should_persist {
        persist(app, &config)?;
    }

    log::info!("Network proxy initialized: {}", describe_config(&config));
    Ok(())
}

pub fn current_config() -> NetworkProxyConfig {
    NETWORK_PROXY
        .read()
        .map(|value| value.clone())
        .unwrap_or_default()
}

pub fn update(app: &AppHandle, mut config: NetworkProxyConfig) -> Result<NetworkProxyConfig, String> {
    config.custom_proxy = config.custom_proxy.trim().to_string();
    validate_config(&config)?;
    persist(app, &config)?;
    set_in_memory(&config)?;
    Ok(config)
}

/// Backward-compatible getter for older frontends. Only custom mode has an
/// explicit application proxy URL; direct/system therefore return an empty string.
pub fn current_http_proxy() -> String {
    let config = current_config();
    if config.mode == NetworkProxyMode::Custom {
        config.custom_proxy
    } else {
        String::new()
    }
}

/// Backward-compatible setter used by older UI builds.
pub fn update_legacy_http_proxy(app: &AppHandle, value: &str) -> Result<String, String> {
    let normalized = value.trim().to_string();
    let config = NetworkProxyConfig {
        mode: if normalized.is_empty() {
            NetworkProxyMode::Direct
        } else {
            NetworkProxyMode::Custom
        },
        custom_proxy: if normalized.is_empty() {
            current_config().custom_proxy
        } else {
            normalized
        },
    };
    let updated = update(app, config)?;
    Ok(if updated.mode == NetworkProxyMode::Custom {
        updated.custom_proxy
    } else {
        String::new()
    })
}

pub fn describe_current() -> String {
    describe_config(&current_config())
}

fn describe_config(config: &NetworkProxyConfig) -> String {
    match config.mode {
        NetworkProxyMode::Direct => "direct".to_string(),
        NetworkProxyMode::System => "system (Windows current-user proxy)".to_string(),
        NetworkProxyMode::Custom => format!("custom ({})", config.custom_proxy),
    }
}

fn parse_stored_settings(raw: &str) -> Result<(NetworkProxyConfig, bool), String> {
    if let Ok(mut config) = serde_json::from_str::<NetworkProxyConfig>(raw) {
        config.custom_proxy = config.custom_proxy.trim().to_string();
        validate_config(&config)?;
        return Ok((config, false));
    }

    let legacy: LegacyNetworkProxySettings = serde_json::from_str(raw)
        .map_err(|err| format!("Failed to parse proxy settings: {err}"))?;
    let legacy_proxy = legacy.http_proxy.trim().to_string();
    let config = if legacy_proxy.is_empty() {
        NetworkProxyConfig {
            mode: NetworkProxyMode::Direct,
            custom_proxy: DEFAULT_HTTP_PROXY.to_string(),
        }
    } else {
        NetworkProxyConfig {
            mode: NetworkProxyMode::Custom,
            custom_proxy: legacy_proxy,
        }
    };
    validate_config(&config)?;
    Ok((config, true))
}

fn set_in_memory(config: &NetworkProxyConfig) -> Result<(), String> {
    let mut state = NETWORK_PROXY
        .write()
        .map_err(|_| "Network proxy state lock poisoned".to_string())?;
    *state = config.clone();
    Ok(())
}

fn settings_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join(SETTINGS_FILE))
        .map_err(|err| format!("Failed to resolve app config directory: {err}"))
}

fn persist(app: &AppHandle, config: &NetworkProxyConfig) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create app config directory: {err}"))?;
    }
    let payload = serde_json::to_string_pretty(config)
        .map_err(|err| format!("Failed to serialize proxy settings: {err}"))?;
    std::fs::write(&path, payload)
        .map_err(|err| format!("Failed to persist network proxy setting: {err}"))
}

fn validate_config(config: &NetworkProxyConfig) -> Result<(), String> {
    if config.mode == NetworkProxyMode::Custom {
        if config.custom_proxy.trim().is_empty() {
            return Err("Custom proxy mode requires a proxy URL".to_string());
        }
        validate_http_proxy(&config.custom_proxy)?;
    } else if !config.custom_proxy.trim().is_empty() {
        // Preserve the dormant custom value when users switch modes, but keep
        // persisted data valid so switching back cannot activate a broken URL.
        validate_http_proxy(&config.custom_proxy)?;
    }
    Ok(())
}

pub fn validate_http_proxy(value: &str) -> Result<(), String> {
    parse_http_proxy(value)
        .map(|_| ())
        .map_err(|err| format!("Invalid HTTP proxy: {err}"))
}

fn parse_http_proxy(value: &str) -> io::Result<(String, u16)> {
    let trimmed = value.trim();
    let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http:// proxies are supported by the shared CONNECT transport",
        )
    })?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "proxy host is empty"));
    }
    if authority.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "authenticated proxy URLs are not supported",
        ));
    }

    if let Some(inner) = authority.strip_prefix('[') {
        let end = inner.find(']').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid IPv6 proxy host")
        })?;
        let host = inner[..end].to_string();
        let suffix = &inner[end + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid proxy port"))?
                .parse::<u16>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid proxy port"))?
        };
        return Ok((host, port));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => {
            let port = port.parse::<u16>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid proxy port")
            })?;
            (host.to_string(), port)
        }
        _ => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "proxy host is empty"));
    }
    Ok((host, port))
}

fn target_scheme(target_url: &str) -> &str {
    let scheme = target_url.split("://").next().unwrap_or("https");
    match scheme {
        "wss" => "https",
        "ws" => "http",
        other => other,
    }
}

fn target_host(target_url: &str) -> &str {
    let authority = target_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(target_url)
        .split('/')
        .next()
        .unwrap_or_default();
    authority
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or(authority)
}

fn normalize_system_proxy_address(value: &str) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let normalized = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    validate_http_proxy(&normalized)?;
    Ok(Some(normalized))
}

fn select_system_proxy(proxy_server: &str, scheme: &str) -> Result<Option<String>, String> {
    let proxy_server = proxy_server.trim();
    if proxy_server.is_empty() {
        return Ok(None);
    }
    if !proxy_server.contains('=') {
        return normalize_system_proxy_address(proxy_server);
    }

    for entry in proxy_server.split(';') {
        let Some((entry_scheme, address)) = entry.split_once('=') else {
            continue;
        };
        if entry_scheme.trim().eq_ignore_ascii_case(scheme) {
            return normalize_system_proxy_address(address);
        }
    }
    Ok(None)
}

fn host_matches_bypass(host: &str, bypass: &str) -> bool {
    let host = host.to_ascii_lowercase();
    bypass.split(';').any(|raw| {
        let pattern = raw.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            return false;
        }
        if pattern == "<local>" {
            return !host.contains('.');
        }
        if let Some(suffix) = pattern.strip_prefix("*.") {
            return host == suffix || host.ends_with(&format!(".{suffix}"));
        }
        pattern == host
    })
}

#[cfg(target_os = "windows")]
fn system_proxy_for_url(target_url: &str) -> Result<Option<String>, String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let settings = hkcu
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
        .map_err(|err| format!("Failed to read Windows system proxy settings: {err}"))?;
    let enabled: u32 = settings.get_value("ProxyEnable").unwrap_or(0);
    if enabled != 1 {
        return Ok(None);
    }

    let proxy_server: String = settings
        .get_value("ProxyServer")
        .map_err(|err| format!("Windows system proxy is enabled but ProxyServer is unavailable: {err}"))?;
    let bypass: String = settings.get_value("ProxyOverride").unwrap_or_default();
    if host_matches_bypass(target_host(target_url), &bypass) {
        return Ok(None);
    }
    select_system_proxy(&proxy_server, target_scheme(target_url))
}

#[cfg(not(target_os = "windows"))]
fn system_proxy_for_url(target_url: &str) -> Result<Option<String>, String> {
    let scheme = target_scheme(target_url);
    let keys: &[&str] = if scheme == "https" {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    for key in keys {
        if let Ok(value) = std::env::var(key) {
            if let Some(proxy) = normalize_system_proxy_address(&value)? {
                return Ok(Some(proxy));
            }
        }
    }
    Ok(None)
}

/// Resolve the configured proxy for a concrete destination. System mode reads
/// Windows' current user proxy settings on every new connection, so changes in
/// Windows Settings take effect without restarting VoiceX.
pub fn resolve_proxy_for_url(target_url: &str) -> Result<Option<String>, String> {
    let config = current_config();
    match config.mode {
        NetworkProxyMode::Direct => Ok(None),
        NetworkProxyMode::System => system_proxy_for_url(target_url),
        NetworkProxyMode::Custom => {
            validate_http_proxy(&config.custom_proxy)?;
            Ok(Some(config.custom_proxy))
        }
    }
}

/// Build an HTTP client that obeys VoiceX's explicit global proxy mode. Calling
/// `no_proxy()` first is important: Direct mode must not silently inherit proxy
/// environment variables, and System mode must use the Windows settings we read.
pub fn build_reqwest_client(target_url: &str) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder().no_proxy();
    if let Some(proxy_url) = resolve_proxy_for_url(target_url)? {
        let proxy = reqwest::Proxy::all(&proxy_url)
            .map_err(|err| format!("Invalid resolved proxy {proxy_url}: {err}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|err| format!("Failed to build HTTP client: {err}"))
}

/// Establish a TCP stream to `target_host:target_port`, using an HTTP CONNECT
/// tunnel when Custom/System mode resolves a proxy for the target.
pub async fn connect_tcp_via_http_proxy(
    target_host: &str,
    target_port: u16,
) -> io::Result<TcpStream> {
    let target_url = format!("https://{target_host}:{target_port}/");
    let proxy = resolve_proxy_for_url(&target_url)
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    let Some(proxy) = proxy else {
        return TcpStream::connect((target_host, target_port)).await;
    };

    let (proxy_host, proxy_port) = parse_http_proxy(&proxy)?;
    let mut stream = TcpStream::connect((proxy_host.as_str(), proxy_port)).await?;
    stream.set_nodelay(true)?;

    let target = format!("{target_host}:{target_port}");
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\nConnection: Keep-Alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut response = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if response.len() >= MAX_CONNECT_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP proxy CONNECT response is too large",
            ));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP proxy closed before CONNECT completed",
            ));
        }
        response.extend_from_slice(&chunk[..read]);
        if response.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }

    let header = String::from_utf8_lossy(&response);
    let status = header.lines().next().unwrap_or_default();
    let ok = status
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .map(|code| (200..300).contains(&code))
        .unwrap_or(false);
    if !ok {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("HTTP proxy CONNECT failed: {status}"),
        ));
    }

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::{
        parse_http_proxy, parse_stored_settings, select_system_proxy, NetworkProxyConfig,
        NetworkProxyMode, DEFAULT_HTTP_PROXY,
    };

    #[test]
    fn migrates_legacy_explicit_proxy_to_custom_mode() {
        let (config, migrated) =
            parse_stored_settings(r#"{"httpProxy":"http://127.0.0.1:7890"}"#).unwrap();
        assert!(migrated);
        assert_eq!(config.mode, NetworkProxyMode::Custom);
        assert_eq!(config.custom_proxy, "http://127.0.0.1:7890");
    }

    #[test]
    fn migrates_legacy_empty_proxy_to_direct_mode() {
        let (config, migrated) = parse_stored_settings(r#"{"httpProxy":""}"#).unwrap();
        assert!(migrated);
        assert_eq!(config.mode, NetworkProxyMode::Direct);
        assert_eq!(config.custom_proxy, DEFAULT_HTTP_PROXY);
    }

    #[test]
    fn explicit_mode_round_trip_does_not_require_inference() {
        let original = NetworkProxyConfig {
            mode: NetworkProxyMode::System,
            custom_proxy: "http://127.0.0.1:7890".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let (decoded, migrated) = parse_stored_settings(&json).unwrap();
        assert!(!migrated);
        assert_eq!(decoded, original);
    }

    #[test]
    fn parses_default_proxy() {
        let (host, port) = parse_http_proxy(DEFAULT_HTTP_PROXY).unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 7890);
    }

    #[test]
    fn selects_windows_per_protocol_proxy() {
        let proxy = select_system_proxy(
            "http=127.0.0.1:8080;https=127.0.0.1:7890",
            "https",
        )
        .unwrap();
        assert_eq!(proxy.as_deref(), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn rejects_non_http_custom_proxy() {
        assert!(parse_http_proxy("socks5://127.0.0.1:7890").is_err());
    }
}
