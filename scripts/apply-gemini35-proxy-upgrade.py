#!/usr/bin/env python3
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]


def load(path):
    return (ROOT / path).read_text(encoding="utf-8")


def save(path, text):
    (ROOT / path).write_text(text.replace("\r\n", "\n"), encoding="utf-8", newline="\n")


def replace_once(path, old, new):
    text = load(path)
    if new in text:
        return
    if old not in text:
        raise RuntimeError(f"anchor missing in {path}: {old[:100]!r}")
    save(path, text.replace(old, new, 1))


def replace_literal(path, old, new):
    text = load(path)
    if old in text:
        save(path, text.replace(old, new))
    elif new not in text:
        raise RuntimeError(f"literal missing in {path}: {old!r}")


def patch_google():
    path = "src-tauri/src/asr/google_client.rs"
    text = load(path)

    if "use tower::service_fn;" not in text:
        anchor = "use tonic::transport::{Channel, ClientTlsConfig};\n"
        if anchor not in text:
            raise RuntimeError("tonic import anchor missing")
        text = text.replace(anchor, anchor + "use tower::service_fn;\n", 1)

    if "use hyper_util::rt::TokioIo;" not in text:
        anchor = "use futures_util::{SinkExt, StreamExt};\n"
        if anchor in text:
            text = text.replace(anchor, anchor + "use hyper_util::rt::TokioIo;\n", 1)
        else:
            anchor = "use base64::Engine;\n"
            if anchor not in text:
                raise RuntimeError("hyper-util import anchor missing")
            text = text.replace(anchor, anchor + "use hyper_util::rt::TokioIo;\n", 1)

    text = text.replace(
        "struct CachedChannel {\n    endpoint: String,\n    channel: Channel,\n}",
        "struct CachedChannel {\n    endpoint: String,\n    proxy: String,\n    channel: Channel,\n}",
        1,
    )

    channel_fn = '''async fn get_or_create_channel(endpoint_url: &str) -> Result<Channel, AsrError> {
    let proxy = crate::network_proxy::current_http_proxy();

    {
        let cache = CHANNEL_CACHE.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.endpoint == endpoint_url && cached.proxy == proxy {
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

    let channel = if proxy.trim().is_empty() {
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
        if proxy.trim().is_empty() { "disabled" } else { &proxy }
    );

    {
        let mut cache = CHANNEL_CACHE.lock().await;
        *cache = Some(CachedChannel {
            endpoint: endpoint_url.to_string(),
            proxy,
            channel: channel.clone(),
        });
    }

    Ok(channel)
}'''

    pattern = r"async fn get_or_create_channel\(endpoint_url: &str\) -> Result<Channel, AsrError> \{.*?\n\}\n\n/// Simple hash"
    if channel_fn not in text:
        text, count = re.subn(pattern, channel_fn + "\n\n/// Simple hash", text, count=1, flags=re.S)
        if count != 1:
            raise RuntimeError("Google channel function anchor missing")

    old_http = "    let http = reqwest::Client::new();\n    let resp = http\n"
    new_http = '''    let proxy_url = crate::network_proxy::current_http_proxy();
    let mut http_builder = reqwest::Client::builder();
    if !proxy_url.trim().is_empty() {
        let proxy = reqwest::Proxy::all(&proxy_url).map_err(|err| {
            AsrError::ConnectionFailed(format!("Invalid HTTP proxy for Google OAuth: {err}"))
        })?;
        http_builder = http_builder.proxy(proxy);
    }
    let http = http_builder.build().map_err(|err| {
        AsrError::ConnectionFailed(format!("Failed to build Google OAuth HTTP client: {err}"))
    })?;
    let resp = http
'''
    if old_http in text:
        text = text.replace(old_http, new_http, 1)
    elif "Invalid HTTP proxy for Google OAuth" not in text:
        raise RuntimeError("Google OAuth HTTP anchor missing")

    save(path, text)


def main():
    replace_once(
        "src-tauri/src/commands/mod.rs",
        "pub mod hud;\n",
        "pub mod hud;\npub mod network;\n",
    )
    replace_once(
        "src-tauri/src/lib.rs",
        "pub mod llm;\n",
        "pub mod llm;\npub mod network_proxy;\n",
    )
    replace_once(
        "src-tauri/src/lib.rs",
        "    let app_data_dir = app.path().app_data_dir()?;\n    std::fs::create_dir_all(&app_data_dir)?;\n",
        "    let app_data_dir = app.path().app_data_dir()?;\n    std::fs::create_dir_all(&app_data_dir)?;\n    network_proxy::init(app.handle()).map_err(std::io::Error::other)?;\n",
    )
    replace_once(
        "src-tauri/src/lib.rs",
        "            commands::settings::get_settings,\n",
        "            commands::settings::get_settings,\n            commands::network::get_http_proxy,\n            commands::network::set_http_proxy,\n",
    )
    replace_once(
        "src-tauri/Cargo.toml",
        'tokio-stream = "0.1"\n',
        'tokio-stream = "0.1"\ntower = "0.4"\nhyper-util = { version = "0.1", features = ["tokio"] }\n',
    )

    patch_google()

    for path in (
        "src-tauri/src/asr/config.rs",
        "src-tauri/src/commands/settings.rs",
        "src/stores/settings.ts",
    ):
        replace_literal(path, "gemini-3.1-flash-live-preview", "gemini-3.5-transcribe-live")

    replace_once(
        "src/views/AsrSettings.vue",
        "import AsrColiSettings from '../components/asr/AsrColiSettings.vue'\n",
        "import AsrColiSettings from '../components/asr/AsrColiSettings.vue'\nimport NetworkProxySettings from '../components/asr/NetworkProxySettings.vue'\n",
    )
    replace_once(
        "src/views/AsrSettings.vue",
        "    <!-- Provider-specific configuration -->\n",
        "    <NetworkProxySettings />\n\n    <!-- Provider-specific configuration -->\n",
    )

    print("Gemini 3.5 transcription/proxy source migration applied")


if __name__ == "__main__":
    main()
