#!/usr/bin/env python3
from pathlib import Path
import json
import re

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_PROXY = "http://127.0.0.1:7890"


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(text: str, needle: str, label: str) -> None:
    if needle not in text:
        raise AssertionError(f"{label}: missing {needle!r}")


def migrate_legacy(raw: str) -> dict[str, str]:
    data = json.loads(raw)
    if "mode" in data:
        return {
            "mode": data["mode"],
            "customProxy": data.get("customProxy", DEFAULT_PROXY).strip(),
        }
    legacy = str(data.get("httpProxy", "")).strip()
    if legacy:
        return {"mode": "custom", "customProxy": legacy}
    return {"mode": "direct", "customProxy": DEFAULT_PROXY}


def test_persistence_contract() -> None:
    migrated = migrate_legacy('{"httpProxy":"http://127.0.0.1:7890"}')
    assert migrated == {"mode": "custom", "customProxy": "http://127.0.0.1:7890"}

    migrated = migrate_legacy('{"httpProxy":" http://10.0.0.2:8080 "}')
    assert migrated == {"mode": "custom", "customProxy": "http://10.0.0.2:8080"}

    migrated = migrate_legacy('{"httpProxy":""}')
    assert migrated == {"mode": "direct", "customProxy": DEFAULT_PROXY}

    explicit = migrate_legacy('{"mode":"system","customProxy":"http://127.0.0.1:7890"}')
    assert explicit == {"mode": "system", "customProxy": DEFAULT_PROXY}


def test_backend_wiring() -> None:
    proxy = read("src-tauri/src/network_proxy.rs")
    require(proxy, "pub enum NetworkProxyMode", "explicit proxy mode")
    for variant in ("Direct,", "System,", "Custom,"):
        require(proxy, variant, "proxy mode variant")
    require(proxy, "LegacyNetworkProxySettings", "legacy migration")
    require(proxy, "mode: NetworkProxyMode::Custom", "legacy custom migration")
    require(proxy, "mode: NetworkProxyMode::Direct", "legacy direct migration")
    require(proxy, 'open_subkey("Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Internet Settings")', "Windows system proxy registry")
    require(proxy, 'get_value("ProxyEnable")', "Windows ProxyEnable")
    require(proxy, 'get_value("ProxyServer")', "Windows ProxyServer")
    require(proxy, 'get_value("ProxyOverride")', "Windows ProxyOverride")
    require(proxy, "NetworkProxyMode::Direct => Ok(None)", "strict direct mode")
    require(proxy, "NetworkProxyMode::System => system_proxy_for_url(target_url)", "system mode resolution")
    require(proxy, "reqwest::Client::builder().no_proxy()", "reqwest direct-mode proxy bypass")

    commands = read("src-tauri/src/commands/network.rs")
    require(commands, "get_network_proxy_config", "global proxy getter")
    require(commands, "set_network_proxy_config", "global proxy setter")

    google = read("src-tauri/src/asr/google_client.rs")
    require(google, "resolve_proxy_for_url(endpoint_url)", "Google gRPC global proxy")
    require(google, "build_reqwest_client(token_uri)", "Google OAuth global proxy")
    require(google, "connect_tcp_via_http_proxy(&target_host, 443)", "Google gRPC CONNECT transport")

    gemini = read("src-tauri/src/asr/gemini_live_client.rs")
    require(gemini, "connect_tcp_via_http_proxy(GEMINI_LIVE_HOST, 443)", "Gemini Live global proxy")


def test_frontend_wiring() -> None:
    asr = read("src/views/AsrSettings.vue")
    if "NetworkProxySettings" in asr or "get_http_proxy" in asr or "set_http_proxy" in asr:
        raise AssertionError("ASR settings still owns proxy UI/configuration")

    network = read("src/views/NetworkSettings.vue")
    for mode in ('value="direct"', 'value="system"', 'value="custom"'):
        require(network, mode, "network mode UI")
    require(network, 'v-if="isCustom"', "custom-only proxy input")
    require(network, "get_network_proxy_config", "network config load")
    require(network, "set_network_proxy_config", "network config save")

    router = read("src/router/index.ts")
    require(router, "'/network-settings'", "network settings route")

    sidebar = read("src/components/Sidebar.vue")
    require(sidebar, "nav.networkSettings", "network settings navigation")


def test_local_asr_is_not_rewired() -> None:
    # Global proxy calls should stay limited to cloud transports/network config.
    for path in (ROOT / "src-tauri/src/asr").glob("*.rs"):
        text = path.read_text(encoding="utf-8")
        if "network_proxy::" not in text:
            continue
        if path.name not in {"gemini_live_client.rs", "google_client.rs"}:
            raise AssertionError(f"unexpected global proxy dependency in local/other ASR path: {path.name}")


def main() -> None:
    test_persistence_contract()
    test_backend_wiring()
    test_frontend_wiring()
    test_local_asr_is_not_rewired()
    print("Global network proxy contract checks passed")


if __name__ == "__main__":
    main()
