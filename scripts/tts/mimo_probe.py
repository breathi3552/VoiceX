#!/usr/bin/env python3
"""Probe the Xiaomi MiMo TTS interface against a live account.

The official docs (mimo.mi.com, checked 2026-08-14) describe a
chat-completions-shaped TTS: the assistant message carries the text to
synthesize, an optional user message carries a style instruction, and audio
comes back base64-encoded — `message.audio.data` non-streaming,
`delta.audio.data` per SSE chunk when `stream: true`. Several details the docs
leave open decide the Rust backend design, so verify them against the real
service:

    ./scripts/tts/mimo_probe.py all
    ./scripts/tts/mimo_probe.py auth       # api-key header vs Authorization: Bearer
    ./scripts/tts/mimo_probe.py messages   # is the user/instruction message optional?
    ./scripts/tts/mimo_probe.py voices     # Chinese voice ids (冰糖 …), default voice
    ./scripts/tts/mimo_probe.py sse        # stream chunk shape + first-packet latency
    ./scripts/tts/mimo_probe.py formats    # mp3/wav/pcm16, streaming mp3
    ./scripts/tts/mimo_probe.py length     # long text: truncation / max_tokens behaviour

Reads the MiMo key from the app's own database (same account the app uses).
The service is in a limited-time free period, but keep texts short anyway.
Outputs land in /tmp-style scratch next to nothing in the repo: pass --outdir
to keep the audio for listening checks.
"""

import argparse
import base64
import json
import pathlib
import sqlite3
import sys
import time
import urllib.error
import urllib.request

DB = pathlib.Path.home() / "Library/Application Support/com.voicex.app/voicex.db"
ENDPOINT = "https://api.xiaomimimo.com/v1/chat/completions"
MODEL = "mimo-v2.5-tts"

ZH_TEXT = "这是一段测试文本，用来验证小米合成接口的基本行为。"
MIX_TEXT = "VoiceX 使用 Rust 和 Tauri 构建，支持 streaming TTS 与 API key 配置。"


def api_key() -> str:
    if not DB.exists():
        sys.exit(f"settings database not found: {DB}")
    with sqlite3.connect(f"file:{DB}?mode=ro", uri=True) as conn:
        row = conn.execute(
            "SELECT value FROM user_config WHERE key='app_settings'"
        ).fetchone()
    if not row:
        sys.exit("no app_settings row in user_config")
    key = (json.loads(row[0]).get("mimoApiKey") or "").strip()
    if not key:
        sys.exit("no MiMo API key configured (settings key: mimoApiKey)")
    return key


def request(body: dict, *, header: str = "api-key", stream: bool = False):
    data = json.dumps(body).encode()
    req = urllib.request.Request(ENDPOINT, data=data, method="POST")
    req.add_header("Content-Type", "application/json")
    if header == "api-key":
        req.add_header("api-key", KEY)
    else:
        req.add_header("Authorization", f"Bearer {KEY}")
    return urllib.request.urlopen(req, timeout=120)


def synth_body(text: str, *, voice="mimo_default", fmt="wav", instruction=None,
               stream=False, extra=None, messages=None) -> dict:
    if messages is None:
        messages = []
        if instruction is not None:
            messages.append({"role": "user", "content": instruction})
        messages.append({"role": "assistant", "content": text})
    body = {
        "model": MODEL,
        "messages": messages,
        "audio": {"format": fmt, "voice": voice},
    }
    if stream:
        body["stream"] = True
    if extra:
        body.update(extra)
    return body


def run_plain(tag: str, body: dict, *, header="api-key", save=None):
    t0 = time.monotonic()
    try:
        with request(body, header=header) as resp:
            payload = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:500]
        print(f"[{tag}] HTTP {e.code}: {detail}")
        return None
    elapsed = time.monotonic() - t0
    choice = (payload.get("choices") or [{}])[0]
    msg = choice.get("message") or {}
    audio = msg.get("audio") or {}
    data = audio.get("data")
    nbytes = len(base64.b64decode(data)) if data else 0
    print(
        f"[{tag}] ok in {elapsed:.2f}s, audio={nbytes}B, "
        f"finish_reason={choice.get('finish_reason')}, "
        f"usage={json.dumps(payload.get('usage'))}, "
        f"audio-keys={sorted(audio.keys())}"
    )
    if save and data:
        save.write_bytes(base64.b64decode(data))
        print(f"[{tag}] saved -> {save}")
    return payload


def run_sse(tag: str, body: dict, save=None):
    t0 = time.monotonic()
    first = None
    chunks = []
    finish = usage = None
    delta_keys = set()
    try:
        with request(body, stream=True) as resp:
            ctype = resp.headers.get("Content-Type")
            for raw in resp:
                line = raw.decode(errors="replace").strip()
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if payload == "[DONE]":
                    break
                obj = json.loads(payload)
                usage = obj.get("usage") or usage
                for ch in obj.get("choices") or []:
                    finish = ch.get("finish_reason") or finish
                    delta = ch.get("delta") or {}
                    delta_keys |= set(delta.keys())
                    audio = delta.get("audio") or {}
                    if audio.get("data"):
                        if first is None:
                            first = time.monotonic() - t0
                        chunks.append(base64.b64decode(audio["data"]))
    except urllib.error.HTTPError as e:
        print(f"[{tag}] HTTP {e.code}: {e.read().decode(errors='replace')[:500]}")
        return
    total = time.monotonic() - t0
    print(
        f"[{tag}] content-type={ctype} chunks={len(chunks)} "
        f"first-audio={first and f'{first*1000:.0f}ms'} total={total:.2f}s "
        f"bytes={sum(map(len, chunks))} finish={finish} "
        f"delta-keys={sorted(delta_keys)} usage={json.dumps(usage)}"
    )
    if save and chunks:
        save.write_bytes(b"".join(chunks))
        print(f"[{tag}] saved -> {save}")


def probe_auth():
    print("== auth: which headers are accepted ==")
    run_plain("api-key header", synth_body(ZH_TEXT))
    run_plain("bearer header", synth_body(ZH_TEXT), header="bearer")


def probe_messages():
    print("== messages: is the instruction message optional ==")
    run_plain("assistant only", synth_body(ZH_TEXT))
    run_plain("empty user first", synth_body(ZH_TEXT, instruction=""))
    run_plain("with instruction", synth_body(ZH_TEXT, instruction="以平静的语气朗读"))
    # order matters? docs always show user first
    run_plain(
        "user role only",
        synth_body(ZH_TEXT, messages=[{"role": "user", "content": ZH_TEXT}]),
    )


def probe_voices(outdir):
    print("== voices ==")
    for voice in ["mimo_default", "冰糖", "茉莉", "苏打", "白桦", "Mia", "bogus-voice"]:
        run_plain(
            f"voice={voice}",
            synth_body(MIX_TEXT, voice=voice),
            save=outdir / f"voice_{voice}.wav" if outdir else None,
        )


def probe_sse(outdir):
    print("== sse: chunk shape and first-packet latency ==")
    run_sse("pcm16 zh", synth_body(ZH_TEXT, fmt="pcm16", stream=True),
            save=outdir / "sse_zh.pcm" if outdir else None)
    run_sse("pcm16 mixed", synth_body(MIX_TEXT, fmt="pcm16", stream=True),
            save=outdir / "sse_mix.pcm" if outdir else None)


def probe_formats(outdir):
    print("== formats ==")
    run_plain("wav", synth_body(ZH_TEXT, fmt="wav"))
    run_plain("mp3", synth_body(ZH_TEXT, fmt="mp3"),
              save=outdir / "fmt.mp3" if outdir else None)
    run_plain("pcm16 non-stream", synth_body(ZH_TEXT, fmt="pcm16"))
    run_sse("mp3 stream", synth_body(ZH_TEXT, fmt="mp3", stream=True),
            save=outdir / "sse.mp3" if outdir else None)
    run_plain("bogus fmt", synth_body(ZH_TEXT, fmt="ogg"))


def probe_length():
    print("== length: truncation on long text ==")
    long_text = "".join(f"第{i}句，中文长文本朗读测试，检查是否被输出上限截断。" for i in range(1, 41))
    print(f"text length: {len(long_text)} chars")
    run_plain("long default", synth_body(long_text))
    run_plain("long max_tokens=8192",
              synth_body(long_text, extra={"max_tokens": 8192}))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("probe", choices=["all", "auth", "messages", "voices", "sse",
                                      "formats", "length"])
    ap.add_argument("--outdir", type=pathlib.Path, default=None,
                    help="save returned audio here for listening checks")
    args = ap.parse_args()
    if args.outdir:
        args.outdir.mkdir(parents=True, exist_ok=True)

    todo = [args.probe] if args.probe != "all" else [
        "auth", "messages", "voices", "sse", "formats", "length"]
    for name in todo:
        if name == "auth":
            probe_auth()
        elif name == "messages":
            probe_messages()
        elif name == "voices":
            probe_voices(args.outdir)
        elif name == "sse":
            probe_sse(args.outdir)
        elif name == "formats":
            probe_formats(args.outdir)
        elif name == "length":
            probe_length()
        print()


if __name__ == "__main__":
    KEY = api_key()
    main()
else:
    KEY = None
