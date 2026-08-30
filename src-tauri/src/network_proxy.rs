//! Shared explicit HTTP proxy support for Google cloud speech transports.
//!
//! The setting is stored independently from ASR provider configuration so local
//! ASR remains untouched. An empty string explicitly disables the proxy.

use serde::{Deserialize, Serialize};
use std::io;
use std::sync::{LazyLock, RwLock};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_HTTP_PROXY: &str = "http://127.0.0.1:7890";
const SETTINGS_FILE: &str = "network-proxy.json";
const MAX_CONNECT_RESPONSE_BYTES: usize = 16 * 1024;

static HTTP_PROXY: LazyLock<RwLock<String>> =
    LazyLock::new(|| RwLock::new(DEFAULT_HTTP_PROXY.to_string()));

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkProxySettings {
    http_proxy: String,
}

pub fn init(app: &AppHandle) -> Result<(), String> {
    let path = settings_path(app)?;
    let value = match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<NetworkProxySettings>(&raw) {
            Ok(settings) => settings.http_proxy,
            Err(err) => {
                log::warn!("Invalid network proxy settings, using default: {err}");
                DEFAULT_HTTP_PROXY.to_string()
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => DEFAULT_HTTP_PROXY.to_string(),
        Err(err) => return Err(format!("Failed to read network proxy settings: {err}")),
    };

    validate_http_proxy(&value)?;
    set_in_memory(&value)?;

    if !path.exists() {
        persist(app, &value)?;
    }

    log::info!(
        "Explicit HTTP proxy initialized: {}",
        if value.trim().is_empty() { "disabled" } else { &value }
    );
    Ok(())
}

pub fn current_http_proxy() -> String {
    HTTP_PROXY
        .read()
        .map(|value| value.clone())
        .unwrap_or_else(|_| DEFAULT_HTTP_PROXY.to_string())
}

pub fn update(app: &AppHandle, value: &str) -> Result<String, String> {
    let normalized = value.trim().to_string();
    validate_http_proxy(&normalized)?;
    persist(app, &normalized)?;
    set_in_memory(&normalized)?;
    Ok(normalized)
}

fn set_in_memory(value: &str) -> Result<(), String> {
    let mut proxy = HTTP_PROXY
        .write()
        .map_err(|_| "HTTP proxy state lock poisoned".to_string())?;
    *proxy = value.to_string();
    Ok(())
}

fn settings_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join(SETTINGS_FILE))
        .map_err(|err| format!("Failed to resolve app config directory: {err}"))
}

fn persist(app: &AppHandle, value: &str) -> Result<(), String> {
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create app config directory: {err}"))?;
    }
    let payload = serde_json::to_string_pretty(&NetworkProxySettings {
        http_proxy: value.to_string(),
    })
    .map_err(|err| format!("Failed to serialize proxy settings: {err}"))?;
    std::fs::write(&path, payload)
        .map_err(|err| format!("Failed to persist HTTP proxy setting: {err}"))
}

pub fn validate_http_proxy(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Ok(());
    }
    parse_http_proxy(value)
        .map(|_| ())
        .map_err(|err| format!("Invalid HTTP proxy: {err}"))
}

fn parse_http_proxy(value: &str) -> io::Result<(String, u16)> {
    let trimmed = value.trim();
    let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "only explicit http:// proxies are supported",
        )
    })?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "proxy host is empty",
        ));
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "proxy host is empty",
        ));
    }
    Ok((host, port))
}

/// Establish a TCP stream to `target_host:target_port`, using an explicit HTTP
/// CONNECT tunnel whenever the in-app proxy setting is non-empty.
pub async fn connect_tcp_via_http_proxy(
    target_host: &str,
    target_port: u16,
) -> io::Result<TcpStream> {
    let proxy = current_http_proxy();
    if proxy.trim().is_empty() {
        return TcpStream::connect((target_host, target_port)).await;
    }

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
    use super::parse_http_proxy;

    #[test]
    fn parses_default_proxy() {
        let (host, port) = parse_http_proxy("http://127.0.0.1:7890").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 7890);
    }

    #[test]
    fn rejects_non_http_proxy() {
        assert!(parse_http_proxy("socks5://127.0.0.1:7890").is_err());
    }
}
