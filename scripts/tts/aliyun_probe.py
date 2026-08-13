#!/usr/bin/env python3
"""Probe the Alibaba Cloud Model Studio TTS interfaces against a live account.

The published parameter tables are incomplete — several parameters that the docs
do not list are in fact honoured, and one documented limit does not exist. This
script is what established that, and it is kept so the claims in
`docs/aliyun-tts-provider-research-2026-08-13.md` can be re-checked when the
service changes.

Reads the DashScope key from the app's own database, so it exercises the same
account the app uses:

    ./scripts/tts/aliyun_probe.py all
    ./scripts/tts/aliyun_probe.py params      # which parameters actually apply
    ./scripts/tts/aliyun_probe.py sse         # streaming shape and first-packet latency
    ./scripts/tts/aliyun_probe.py cross       # endpoint / voice interchangeability
    ./scripts/tts/aliyun_probe.py length      # accepted text length
    ./scripts/tts/aliyun_probe.py ws          # WebSocket variants (needs `websockets`)

Every call is billed per input character, so the texts here are deliberately
short. `length` is the exception and costs a few thousand characters.
"""

import base64
import json
import pathlib
import sqlite3
import sys
import time
import urllib.error
import urllib.request

DB = pathlib.Path.home() / "Library/Application Support/com.voicex.app/voicex.db"

MULTIMODAL = "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
SPEECH_SYNTH = "https://dashscope.aliyuncs.com/api/v1/services/audio/tts/SpeechSynthesizer"

QWEN3 = "qwen3-tts-flash"
QWEN_AUDIO = "qwen-audio-3.0-tts-flash"
QWEN3_VOICE = "Cherry"
QWEN_AUDIO_VOICE = "longanfengyue"

TEXT = "这是一段用来测量参数是否真的生效的测试文本，长度固定不变。"


def credentials():
    """API key and workspace id from the app's settings, so this probes the
    same account the app talks to rather than a hand-pasted copy."""
    if not DB.exists():
        sys.exit(f"settings database not found: {DB}")
    with sqlite3.connect(f"file:{DB}?mode=ro", uri=True) as conn:
        row = conn.execute(
            "SELECT value FROM user_config WHERE key='app_settings'"
        ).fetchone()
    if not row:
        sys.exit("no app_settings row in user_config")
    settings = json.loads(row[0])
    key = (settings.get("qwenAsrApiKey") or "").strip()
    if not key:
        sys.exit("no DashScope API key configured (settings key: qwenAsrApiKey)")
    return key, (settings.get("qwenAsrWorkspaceId") or "").strip()


KEY, WORKSPACE = credentials()
HEADERS = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}


def post(url, body, stream=False, timeout=120):
    headers = dict(HEADERS)
    if stream:
        headers["X-DashScope-SSE"] = "enable"
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers)
    return urllib.request.urlopen(request, timeout=timeout)


def describe_error(exc):
    try:
        body = json.loads(exc.read())
        return f"HTTP {exc.code} {body.get('code')}: {str(body.get('message'))[:70]}"
    except Exception:
        return f"HTTP {exc.code}"


def synthesize(label, model, url, voice, params=None, extra_input=None, text=TEXT):
    """One non-streaming call, reported by payload size.

    Size is the measurement that matters: the returned WAV header carries a
    bogus frame count, so its own duration field cannot be trusted, while byte
    length tracks audio length directly. Run-to-run variation is under 2%, so a
    parameter that moves the size has demonstrably applied.
    """
    body = {"model": model, "input": {"text": text, "voice": voice}}
    if extra_input:
        body["input"].update(extra_input)
    if params:
        body["parameters"] = params
    try:
        with post(url, body) as response:
            output = json.loads(response.read())
    except urllib.error.HTTPError as exc:
        print(f"{label:38s} {describe_error(exc)}")
        return None

    audio_url = output["output"]["audio"]["url"]
    audio = urllib.request.urlopen(audio_url).read()
    ext = audio_url.split("?")[0].rsplit(".", 1)[-1]
    print(
        f"{label:38s} ext={ext:4s} bytes={len(audio):7d} "
        f"chars={output.get('usage', {}).get('characters')}"
    )
    return len(audio)


def probe_params():
    print("=== qwen3-tts-flash: which parameters actually apply ===")
    print("(byte count moves => the parameter applied; unchanged => silently ignored)")
    synthesize("baseline", QWEN3, MULTIMODAL, QWEN3_VOICE)
    for label, params in [
        ("parameters.speech_rate=0.5", {"speech_rate": 0.5}),
        ("parameters.speech_rate=2.0", {"speech_rate": 2.0}),
        ("parameters.pitch_rate=0.5", {"pitch_rate": 0.5}),
        ("parameters.response_format=mp3", {"response_format": "mp3"}),
        ("parameters.sample_rate=16000", {"sample_rate": 16000}),
        ("parameters.rate=2.0 (wrong name)", {"rate": 2.0}),
        ("parameters.format=mp3 (wrong name)", {"format": "mp3"}),
    ]:
        synthesize(label, QWEN3, MULTIMODAL, QWEN3_VOICE, params=params)

    print()
    print("=== qwen-audio-3.0-tts-flash: parameter placement ===")
    synthesize("baseline", QWEN_AUDIO, SPEECH_SYNTH, QWEN_AUDIO_VOICE,
               extra_input={"format": "mp3"})
    synthesize("input.rate=0.5", QWEN_AUDIO, SPEECH_SYNTH, QWEN_AUDIO_VOICE,
               extra_input={"format": "mp3", "rate": 0.5})
    synthesize("parameters.rate=0.5", QWEN_AUDIO, SPEECH_SYNTH, QWEN_AUDIO_VOICE,
               extra_input={"format": "mp3"}, params={"rate": 0.5})


def probe_sse():
    """Streaming shape and first-packet latency — the number that decides
    whether reading starts promptly or after a visible pause."""
    long_text = TEXT + "第二句在这里。第三句也在这里。第四句收尾。"

    def run(label, url, body):
        started = time.time()
        first = None
        chunks = 0
        total = 0
        head = None
        last = None
        with post(url, body, stream=True) as response:
            for raw in response:
                line = raw.decode("utf-8", "replace").strip()
                if not line.startswith("data:"):
                    continue
                payload = json.loads(line[5:])
                last = payload
                data = ((payload.get("output") or {}).get("audio") or {}).get("data")
                if data:
                    blob = base64.b64decode(data)
                    chunks += 1
                    total += len(blob)
                    if first is None:
                        first = time.time() - started
                        head = blob[:4]
        audio = (last.get("output") or {}).get("audio") or {}
        print(f"\n--- {label} ---")
        print(f"first packet {first * 1000:.0f} ms | {chunks} chunks | {total} B "
              f"| magic {head.hex() if head else '-'}")
        print(f"final chunk keys {sorted(audio)} | usage {last.get('usage')}")

    print("=== streaming (X-DashScope-SSE: enable) ===")
    run(f"{QWEN3} default", MULTIMODAL,
        {"model": QWEN3, "input": {"text": long_text, "voice": QWEN3_VOICE}})
    run(f"{QWEN3} response_format=mp3", MULTIMODAL,
        {"model": QWEN3, "input": {"text": long_text, "voice": QWEN3_VOICE},
         "parameters": {"response_format": "mp3"}})
    run(f"{QWEN_AUDIO} format=mp3", SPEECH_SYNTH,
        {"model": QWEN_AUDIO,
         "input": {"text": long_text, "voice": QWEN_AUDIO_VOICE,
                   "format": "mp3", "sample_rate": 24000}})
    if WORKSPACE:
        run(f"{QWEN_AUDIO} format=mp3 @ workspace host",
            f"https://{WORKSPACE}.cn-beijing.maas.aliyuncs.com"
            "/api/v1/services/audio/tts/SpeechSynthesizer",
            {"model": QWEN_AUDIO,
             "input": {"text": long_text, "voice": QWEN_AUDIO_VOICE,
                       "format": "mp3", "sample_rate": 24000}})


def probe_cross():
    """Whether the two families share endpoints, voices, or each other's fields."""
    def call(label, url, body):
        try:
            with post(url, body) as response:
                output = json.loads(response.read())
            tail = output["output"]["audio"]["url"].split("/prod/")[-1][:36]
            print(f"{label:52s} OK   {tail}")
        except urllib.error.HTTPError as exc:
            print(f"{label:52s} {describe_error(exc)}")

    short = "测试。"
    print("=== endpoints are not interchangeable ===")
    call(f"{QWEN3} @ multimodal-generation", MULTIMODAL,
         {"model": QWEN3, "input": {"text": short, "voice": QWEN3_VOICE}})
    call(f"{QWEN3} @ SpeechSynthesizer", SPEECH_SYNTH,
         {"model": QWEN3, "input": {"text": short, "voice": QWEN3_VOICE}})
    call(f"{QWEN_AUDIO} @ SpeechSynthesizer", SPEECH_SYNTH,
         {"model": QWEN_AUDIO, "input": {"text": short, "voice": QWEN_AUDIO_VOICE}})
    call(f"{QWEN_AUDIO} @ multimodal-generation", MULTIMODAL,
         {"model": QWEN_AUDIO, "input": {"text": short, "voice": QWEN_AUDIO_VOICE}})

    print("\n=== voices do not carry across families ===")
    call(f"{QWEN3} + voice={QWEN_AUDIO_VOICE}", MULTIMODAL,
         {"model": QWEN3, "input": {"text": short, "voice": QWEN_AUDIO_VOICE}})
    call(f"{QWEN_AUDIO} + voice={QWEN3_VOICE}", SPEECH_SYNTH,
         {"model": QWEN_AUDIO, "input": {"text": short, "voice": QWEN3_VOICE}})

    print("\n=== foreign fields are ignored, not rejected ===")
    call(f"{QWEN3} + input.material_id", MULTIMODAL,
         {"model": QWEN3, "input": {"text": short, "voice": QWEN3_VOICE, "material_id": "x"}})
    call(f"{QWEN3} + input.instruct", MULTIMODAL,
         {"model": QWEN3, "input": {"text": short, "voice": QWEN3_VOICE, "instruct": "开心一点"}})
    call(f"{QWEN_AUDIO} + input.language_type", SPEECH_SYNTH,
         {"model": QWEN_AUDIO,
          "input": {"text": short, "voice": QWEN_AUDIO_VOICE, "language_type": "Chinese"}})


def probe_length():
    """Accepted single-request text length.

    Judged on the first audio chunk rather than on completion: a 5000-character
    non-streaming request takes many minutes to synthesize, which is itself the
    reason the app has to stream.
    """
    def accepted(model, url, voice, count):
        body = {"model": model,
                "input": {"text": "测试文本。" * (count // 5), "voice": voice},
                "parameters": {"response_format": "mp3", "format": "mp3"}}
        started = time.time()
        try:
            with post(url, body, stream=True, timeout=90) as response:
                for raw in response:
                    line = raw.decode("utf-8", "replace").strip()
                    if not line.startswith("data:"):
                        continue
                    payload = json.loads(line[5:])
                    if ((payload.get("output") or {}).get("audio") or {}).get("data"):
                        print(f"{model:26s} {count:6d} chars  accepted  "
                              f"first packet {(time.time() - started) * 1000:.0f} ms")
                        return True
        except urllib.error.HTTPError as exc:
            print(f"{model:26s} {count:6d} chars  rejected  {describe_error(exc)}")
            return False
        print(f"{model:26s} {count:6d} chars  no audio returned")
        return False

    print("=== accepted single-request length ===")
    for count in (500, 1200, 2000, 5000):
        if not accepted(QWEN3, MULTIMODAL, QWEN3_VOICE, count):
            break
    for count in (2000, 5000, 20000):
        if not accepted(QWEN_AUDIO, SPEECH_SYNTH, QWEN_AUDIO_VOICE, count):
            break


def probe_ws():
    """The WebSocket variants, including which model names they accept."""
    import asyncio
    import uuid

    import websockets

    headers = {"Authorization": f"Bearer {KEY}"}
    text = "这是一段用来验证 WebSocket 链路的测试文本。"

    async def realtime(model):
        url = f"wss://dashscope.aliyuncs.com/api-ws/v1/realtime?model={model}"
        try:
            async with websockets.connect(url, extra_headers=headers, open_timeout=15) as ws:
                await asyncio.wait_for(ws.recv(), 10)  # session.created
                for event in (
                    {"event_id": "e1", "type": "session.update",
                     "session": {"voice": QWEN3_VOICE, "mode": "commit",
                                 "response_format": "mp3", "sample_rate": 24000,
                                 "speech_rate": 1.0}},
                    {"event_id": "e2", "type": "input_text_buffer.append", "text": text},
                    {"event_id": "e3", "type": "input_text_buffer.commit"},
                    {"event_id": "e4", "type": "session.finish"},
                ):
                    await ws.send(json.dumps(event))
                chunks = total = 0
                head = None
                while True:
                    message = json.loads(await asyncio.wait_for(ws.recv(), 20))
                    kind = message.get("type")
                    if kind == "response.audio.delta":
                        blob = base64.b64decode(message["delta"])
                        chunks += 1
                        total += len(blob)
                        if head is None:
                            head = blob[:4]
                    elif kind == "error":
                        print(f"realtime model={model:32s} "
                              f"ERROR {message['error'].get('code')}")
                        return
                    elif kind in ("session.finished", "response.done") and chunks:
                        break
                print(f"realtime model={model:32s} OK {chunks} chunks {total} B "
                      f"magic {head.hex() if head else '-'}")
        except Exception as exc:
            print(f"realtime model={model:32s} refused "
                  f"{type(exc).__name__}: {str(exc)[:60]}")

    async def run_task(host, model):
        task_id = uuid.uuid4().hex
        try:
            async with websockets.connect(f"wss://{host}/api-ws/v1/inference",
                                          extra_headers=headers, open_timeout=15) as ws:
                await ws.send(json.dumps({
                    "header": {"action": "run-task", "task_id": task_id, "streaming": "duplex"},
                    "payload": {"task_group": "audio", "task": "tts",
                                "function": "SpeechSynthesizer", "model": model,
                                "parameters": {"text_type": "PlainText",
                                               "voice": QWEN_AUDIO_VOICE,
                                               "format": "mp3", "sample_rate": 24000},
                                "input": {}}}))
                frames = total = 0
                head = None
                while True:
                    message = await asyncio.wait_for(ws.recv(), 20)
                    if isinstance(message, bytes):
                        frames += 1
                        total += len(message)
                        if head is None:
                            head = message[:4]
                        continue
                    event = json.loads(message)["header"]
                    if event.get("event") == "task-started":
                        for action, payload in (("continue-task", {"text": text}),
                                                ("finish-task", {})):
                            await ws.send(json.dumps({
                                "header": {"action": action, "task_id": task_id,
                                           "streaming": "duplex"},
                                "payload": {"input": payload}}))
                    elif event.get("event") == "task-failed":
                        print(f"run-task @ {host[:34]:36s} FAILED "
                              f"{event.get('error_code')}")
                        return
                    elif event.get("event") == "task-finished":
                        break
                print(f"run-task @ {host[:34]:36s} OK {frames} binary frames {total} B "
                      f"magic {head.hex() if head else '-'}")
        except Exception as exc:
            print(f"run-task @ {host[:34]:36s} refused "
                  f"{type(exc).__name__}: {str(exc)[:60]}")

    async def main():
        print("=== realtime WebSocket requires the -realtime model name ===")
        await realtime(QWEN3)
        await realtime("qwen3-tts-flash-realtime")
        print("\n=== run-task WebSocket ===")
        if WORKSPACE:
            await run_task(f"{WORKSPACE}.cn-beijing.maas.aliyuncs.com", QWEN_AUDIO)
        await run_task("dashscope.aliyuncs.com", QWEN_AUDIO)

    asyncio.run(main())


PROBES = {
    "params": probe_params,
    "sse": probe_sse,
    "cross": probe_cross,
    "length": probe_length,
    "ws": probe_ws,
}

if __name__ == "__main__":
    requested = sys.argv[1:] or ["all"]
    names = list(PROBES) if requested == ["all"] else requested
    for index, name in enumerate(names):
        if name not in PROBES:
            sys.exit(f"unknown probe {name!r}; choose from {', '.join(PROBES)} or 'all'")
        if index:
            print()
        PROBES[name]()
