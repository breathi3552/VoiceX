#!/usr/bin/env python3
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def load(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def save(path: str, text: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text.replace("\r\n", "\n"), encoding="utf-8", newline="\n")


def replace_once(path: str, old: str, new: str) -> None:
    text = load(path)
    if new in text:
        return
    if old not in text:
        raise RuntimeError(f"anchor missing in {path}: {old[:120]!r}")
    save(path, text.replace(old, new, 1))


def regex_replace_once(path: str, pattern: str, replacement: str) -> None:
    text = load(path)
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.S)
    if count == 0:
        if replacement.strip() in text:
            return
        raise RuntimeError(f"regex anchor missing in {path}: {pattern[:120]!r}")
    save(path, updated)


NETWORK_PROXY_RS = r'''//! Global network proxy configuration shared by cloud transports.
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
'''

COMMANDS_NETWORK_RS = r'''//! Tauri commands for the global network proxy configuration.

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
'''

NETWORK_SETTINGS_VUE = r'''<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { NButton, NInput, NRadio, NRadioGroup } from 'naive-ui'
import { useI18n } from 'vue-i18n'

type ProxyMode = 'direct' | 'system' | 'custom'

interface NetworkProxyConfig {
  mode: ProxyMode
  customProxy: string
}

const { t } = useI18n()
const mode = ref<ProxyMode>('custom')
const customProxy = ref('http://127.0.0.1:7890')
const saving = ref(false)
const loading = ref(true)
const status = ref('')
const statusError = ref(false)

const isCustom = computed(() => mode.value === 'custom')
const canSave = computed(() => {
  if (saving.value || loading.value) return false
  if (!isCustom.value) return true
  return /^http:\/\/[^\s/]+(?::\d+)?(?:\/.*)?$/i.test(customProxy.value.trim())
})

async function loadProxyConfig() {
  loading.value = true
  status.value = ''
  statusError.value = false
  try {
    const config = await invoke<NetworkProxyConfig>('get_network_proxy_config')
    mode.value = config.mode
    customProxy.value = config.customProxy || 'http://127.0.0.1:7890'
  } catch (error) {
    statusError.value = true
    status.value = t('network.loadFailed', { error: error instanceof Error ? error.message : String(error) })
  } finally {
    loading.value = false
  }
}

async function saveProxyConfig() {
  if (!canSave.value) {
    statusError.value = true
    status.value = t('network.invalidProxy')
    return
  }

  saving.value = true
  status.value = ''
  statusError.value = false
  try {
    const config = await invoke<NetworkProxyConfig>('set_network_proxy_config', {
      config: {
        mode: mode.value,
        customProxy: customProxy.value.trim()
      }
    })
    mode.value = config.mode
    customProxy.value = config.customProxy
    status.value = t('network.saved')
  } catch (error) {
    statusError.value = true
    status.value = t('network.saveFailed', { error: error instanceof Error ? error.message : String(error) })
  } finally {
    saving.value = false
  }
}

onMounted(loadProxyConfig)
</script>

<template>
  <div class="page settings-page network-page">
    <div class="page-header">
      <h1 class="page-title">{{ t('network.title') }}</h1>
    </div>

    <div class="surface-card network-card">
      <div class="card-header">
        <div class="card-title">{{ t('network.proxyTitle') }}</div>
        <div class="card-sub">{{ t('network.proxySub') }}</div>
      </div>

      <div class="field-list">
        <NRadioGroup v-model:value="mode" :disabled="loading || saving" class="proxy-mode-group">
          <label class="proxy-mode-option">
            <NRadio value="direct">{{ t('network.modeDirect') }}</NRadio>
            <span class="field-note">{{ t('network.modeDirectNote') }}</span>
          </label>
          <label class="proxy-mode-option">
            <NRadio value="system">{{ t('network.modeSystem') }}</NRadio>
            <span class="field-note">{{ t('network.modeSystemNote') }}</span>
          </label>
          <label class="proxy-mode-option">
            <NRadio value="custom">{{ t('network.modeCustom') }}</NRadio>
            <span class="field-note">{{ t('network.modeCustomNote') }}</span>
          </label>
        </NRadioGroup>

        <div v-if="isCustom" class="field-row align-start">
          <div class="field-text">
            <div class="field-label">{{ t('network.customProxy') }}</div>
            <div class="field-note">{{ t('network.customProxyNote') }}</div>
          </div>
          <NInput
            v-model:value="customProxy"
            :placeholder="t('network.customProxyPlaceholder')"
            :disabled="loading || saving"
            class="proxy-input"
            @keyup.enter="saveProxyConfig"
          />
        </div>

        <div class="network-actions">
          <div v-if="status" class="field-note" :class="{ 'status-error': statusError, 'status-ok': !statusError }">
            {{ status }}
          </div>
          <NButton type="primary" secondary size="small" :loading="saving" :disabled="!canSave" @click="saveProxyConfig">
            {{ t('network.save') }}
          </NButton>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
@import '../styles/asr-settings.css';

.settings-page {
  width: 100%;
  max-width: 1120px;
  padding-bottom: var(--spacing-2xl);
}

.network-card {
  max-width: 860px;
}

.proxy-mode-group {
  display: grid;
  gap: 10px;
}

.proxy-mode-option {
  display: grid;
  grid-template-columns: auto 1fr;
  column-gap: 10px;
  row-gap: 3px;
  align-items: start;
  padding: 12px 14px;
  border: 1px solid var(--color-border);
  border-radius: var(--radius-md);
  background: rgba(255, 255, 255, 0.02);
  cursor: pointer;
}

.proxy-mode-option .field-note {
  grid-column: 2;
}

.proxy-input {
  width: min(430px, 100%);
}

.network-actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 12px;
  min-height: 30px;
}

.status-error {
  color: #f87171;
}

.status-ok {
  color: #4ade80;
}
</style>
'''

GOOGLE_CHANNEL_FN = r'''async fn get_or_create_channel(endpoint_url: &str) -> Result<Channel, AsrError> {
    let resolved_proxy = crate::network_proxy::resolve_proxy_for_url(endpoint_url).map_err(|err| {
        AsrError::ConnectionFailed(format!("Failed to resolve global proxy for Google STT: {err}"))
    })?;
    let proxy_key = resolved_proxy.clone().unwrap_or_default();

    {
        let cache = CHANNEL_CACHE.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.endpoint == endpoint_url && cached.proxy == proxy_key {
                log::debug!("Google STT: reusing cached gRPC channel");
                return Ok(cached.channel.clone());
            }
        }
    }

    let tls_domain = endpoint_url
        .strip_prefix("https://")
        .unwrap_or(endpoint_url)
        .trim_end_matches('/')
        .to_string();
    let tls_config = ClientTlsConfig::new()
        .domain_name(tls_domain.clone())
        .with_enabled_roots();

    let endpoint = Channel::from_shared(endpoint_url.to_string())
        .map_err(|e| AsrError::ConnectionFailed(format!("Invalid endpoint URL: {e}")))?
        .tls_config(tls_config)
        .map_err(|e| AsrError::ConnectionFailed(format!("TLS config error: {e}")))?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300));

    let channel = if resolved_proxy.is_none() {
        endpoint.connect().await.map_err(|e| {
            AsrError::ConnectionFailed(format!("gRPC connect to {endpoint_url} failed: {e:?}"))
        })?
    } else {
        let target_host = tls_domain.clone();
        endpoint
            .connect_with_connector(service_fn(move |_| {
                let target_host = target_host.clone();
                async move {
                    crate::network_proxy::connect_tcp_via_http_proxy(&target_host, 443)
                        .await
                        .map(TokioIo::new)
                }
            }))
            .await
            .map_err(|e| {
                AsrError::ConnectionFailed(format!(
                    "gRPC proxy connect to {endpoint_url} failed: {e:?}"
                ))
            })?
    };

    log::info!(
        "Google STT: new gRPC channel connected to {} (proxy={})",
        endpoint_url,
        resolved_proxy.as_deref().unwrap_or("direct")
    );

    {
        let mut cache = CHANNEL_CACHE.lock().await;
        *cache = Some(CachedChannel {
            endpoint: endpoint_url.to_string(),
            proxy: proxy_key,
            channel: channel.clone(),
        });
    }

    Ok(channel)
}'''


def patch_google() -> None:
    path = "src-tauri/src/asr/google_client.rs"
    text = load(path)
    pattern = r"async fn get_or_create_channel\(endpoint_url: &str\) -> Result<Channel, AsrError> \{.*?\n\}\n\n/// Simple hash"
    updated, count = re.subn(pattern, GOOGLE_CHANNEL_FN + "\n\n/// Simple hash", text, count=1, flags=re.S)
    if count != 1:
        raise RuntimeError("Google gRPC channel function anchor missing")

    oauth_pattern = r"    let proxy_url = crate::network_proxy::current_http_proxy\(\);.*?    let resp = http\n"
    oauth_replacement = '''    let http = crate::network_proxy::build_reqwest_client(token_uri).map_err(|err| {
        AsrError::ConnectionFailed(format!("Failed to configure global proxy for Google OAuth: {err}"))
    })?;
    let resp = http
'''
    updated, count = re.subn(oauth_pattern, oauth_replacement, updated, count=1, flags=re.S)
    if count != 1 and "Failed to configure global proxy for Google OAuth" not in updated:
        raise RuntimeError("Google OAuth proxy block anchor missing")
    save(path, updated)


def patch_gemini_live() -> None:
    path = "src-tauri/src/asr/gemini_live_client.rs"
    text = load(path)
    pattern = r"fn display_proxy\(\) -> String \{.*?\n\}\n"
    replacement = '''fn display_proxy() -> String {
    crate::network_proxy::describe_current()
}
'''
    updated, count = re.subn(pattern, replacement, text, count=1, flags=re.S)
    if count != 1:
        raise RuntimeError("Gemini Live display_proxy anchor missing")
    save(path, updated)


def patch_frontend() -> None:
    path = "src/views/AsrSettings.vue"
    text = load(path)
    text = text.replace("import NetworkProxySettings from '../components/asr/NetworkProxySettings.vue'\n", "")
    text = text.replace("\n    <NetworkProxySettings />\n", "")
    save(path, text)

    replace_once(
        "src/router/index.ts",
        "    {\n      path: '/reading-settings',\n",
        "    {\n      path: '/network-settings',\n      name: 'network-settings',\n      component: () => import('../views/NetworkSettings.vue')\n    },\n    {\n      path: '/reading-settings',\n",
    )

    replace_once(
        "src/components/Sidebar.vue",
        "      { path: '/input-settings', name: 'input-settings', icon: 'keyboard', labelKey: 'nav.inputSettings' },\n",
        "      { path: '/input-settings', name: 'input-settings', icon: 'keyboard', labelKey: 'nav.inputSettings' },\n      { path: '/network-settings', name: 'network-settings', icon: 'network', labelKey: 'nav.networkSettings' },\n",
    )
    replace_once(
        "src/components/Sidebar.vue",
        "          <!-- Speaker icon -->\n",
        "          <!-- Network icon -->\n          <svg v-else-if=\"item.icon === 'network'\" viewBox=\"0 0 24 24\" fill=\"currentColor\">\n            <path d=\"M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm6.92 6h-3.08a15.7 15.7 0 0 0-1.38-3.56A8.05 8.05 0 0 1 18.92 8zM12 4c.83 1.2 1.46 2.54 1.82 4h-3.64A13.6 13.6 0 0 1 12 4zM4.26 14a7.8 7.8 0 0 1 0-4h3.4a16.5 16.5 0 0 0 0 4h-3.4zm.82 2h3.08c.3 1.26.76 2.46 1.38 3.56A8.05 8.05 0 0 1 5.08 16zm3.08-8H5.08a8.05 8.05 0 0 1 4.46-3.56A15.7 15.7 0 0 0 8.16 8zM12 20a13.6 13.6 0 0 1-1.82-4h3.64A13.6 13.6 0 0 1 12 20zm2.2-6H9.8a14.2 14.2 0 0 1 0-4h4.4a14.2 14.2 0 0 1 0 4zm.26 5.56c.62-1.1 1.08-2.3 1.38-3.56h3.08a8.05 8.05 0 0 1-4.46 3.56zM16.34 14a16.5 16.5 0 0 0 0-4h3.4a7.8 7.8 0 0 1 0 4h-3.4z\"/>\n          </svg>\n          <!-- Speaker icon -->\n",
    )

    replace_once(
        "src/i18n/locales/zh-CN.ts",
        "    inputSettings: '输入',\n",
        "    inputSettings: '输入',\n    networkSettings: '网络',\n",
    )
    replace_once(
        "src/i18n/locales/zh-CN.ts",
        "  overview: {\n",
        "  network: {\n    title: '网络',\n    proxyTitle: '全局代理',\n    proxySub: 'Gemini Live、Google Cloud STT / Chirp 3 的 OAuth 与 gRPC，以及已接入全局代理的网络链路统一使用这里的设置。本地 ASR 不受影响。',\n    modeDirect: '无代理',\n    modeDirectNote: '强制直接连接，不读取系统代理，也不使用环境变量或自定义代理。',\n    modeSystem: '系统代理',\n    modeSystemNote: '读取 Windows 当前用户的系统代理设置；Windows 设置发生变化后，新连接会自动使用新配置。',\n    modeCustom: '自定义代理',\n    modeCustomNote: '使用 VoiceX 内单独指定的 HTTP CONNECT 代理。',\n    customProxy: '代理地址',\n    customProxyNote: '仅自定义代理模式使用。当前共享传输支持 http:// 代理。',\n    customProxyPlaceholder: '例如 http://127.0.0.1:7890',\n    save: '保存网络设置',\n    saved: '网络代理设置已保存',\n    invalidProxy: '请输入有效的 http:// 代理地址。',\n    loadFailed: '读取网络代理设置失败：{error}',\n    saveFailed: '保存网络代理设置失败：{error}'\n  },\n  overview: {\n",
    )

    replace_once(
        "src/i18n/locales/en-US.ts",
        "    inputSettings: 'Input',\n",
        "    inputSettings: 'Input',\n    networkSettings: 'Network',\n",
    )
    replace_once(
        "src/i18n/locales/en-US.ts",
        "  overview: {\n",
        "  network: {\n    title: 'Network',\n    proxyTitle: 'Global Proxy',\n    proxySub: 'Gemini Live, Google Cloud STT / Chirp 3 OAuth and gRPC, and every transport already wired to the global proxy use this setting. Local ASR is unaffected.',\n    modeDirect: 'No proxy',\n    modeDirectNote: 'Force direct connections without system, environment, or custom proxies.',\n    modeSystem: 'System proxy',\n    modeSystemNote: 'Read the current Windows user proxy settings. New connections pick up Windows proxy changes automatically.',\n    modeCustom: 'Custom proxy',\n    modeCustomNote: 'Use the HTTP CONNECT proxy configured inside VoiceX.',\n    customProxy: 'Proxy URL',\n    customProxyNote: 'Used only in Custom proxy mode. The shared transport currently supports http:// proxies.',\n    customProxyPlaceholder: 'For example http://127.0.0.1:7890',\n    save: 'Save network settings',\n    saved: 'Network proxy settings saved',\n    invalidProxy: 'Enter a valid http:// proxy URL.',\n    loadFailed: 'Failed to load network proxy settings: {error}',\n    saveFailed: 'Failed to save network proxy settings: {error}'\n  },\n  overview: {\n",
    )

    save("src/views/NetworkSettings.vue", NETWORK_SETTINGS_VUE)
    old_component = ROOT / "src/components/asr/NetworkProxySettings.vue"
    if old_component.exists():
        old_component.unlink()


def patch_backend_registration() -> None:
    replace_once(
        "src-tauri/src/lib.rs",
        "            commands::network::get_http_proxy,\n            commands::network::set_http_proxy,\n",
        "            commands::network::get_network_proxy_config,\n            commands::network::set_network_proxy_config,\n            commands::network::get_http_proxy,\n            commands::network::set_http_proxy,\n",
    )

    cargo = load("src-tauri/Cargo.toml")
    anchor = 'windows-sys = { version = "0.59", features = ["Win32_UI_Input_KeyboardAndMouse", "Win32_UI_WindowsAndMessaging", "Win32_Foundation", "Win32_System_Threading"] }\n'
    addition = anchor + 'winreg = "0.50"\n'
    if 'winreg = "0.50"' not in cargo:
        if anchor not in cargo:
            raise RuntimeError("Windows dependency anchor missing in Cargo.toml")
        cargo = cargo.replace(anchor, addition, 1)
    save("src-tauri/Cargo.toml", cargo)


def main() -> None:
    save("src-tauri/src/network_proxy.rs", NETWORK_PROXY_RS)
    save("src-tauri/src/commands/network.rs", COMMANDS_NETWORK_RS)
    patch_backend_registration()
    patch_google()
    patch_gemini_live()
    patch_frontend()
    print("Global network proxy modes and UI migration applied")


if __name__ == "__main__":
    main()
