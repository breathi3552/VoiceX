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
    ./scripts/tts/aliyun_probe.py cosyvoice   # CosyVoice-v3 availability on the same account

Every call is billed per input character, so the texts here are deliberately
short. The exceptions are `length` and the length section of `cosyvoice`,
which cost a few thousand characters each.
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
COSY_FLASH = "cosyvoice-v3-flash"
COSY_PLUS = "cosyvoice-v3-plus"
COSY_V35_FLASH = "cosyvoice-v3.5-flash"
QWEN3_VOICE = "Cherry"
QWEN_AUDIO_VOICE = "longanfengyue"
COSY_VOICE = "longanyang"

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
    tts_key = (settings.get("aliyunTtsApiKey") or "").strip()
    asr_key = (settings.get("qwenAsrApiKey") or "").strip()
    key = tts_key or asr_key
    if not key:
        sys.exit(
            "no DashScope API key configured "
            "(settings keys: aliyunTtsApiKey, qwenAsrApiKey)"
        )
    # The workspace id belongs to the ASR account. If the TTS key is a
    # different account's, pairing them would send one account's token to the
    # other's workspace host and the 403 would read as "the workspace host
    # stopped serving this model" — so the workspace probes are skipped instead.
    workspace = (settings.get("qwenAsrWorkspaceId") or "").strip()
    if key != asr_key:
        workspace = ""
    return key, workspace


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


def sse_frames(response):
    """The `data:` JSON payloads of a streaming response, in order."""
    for raw in response:
        line = raw.decode("utf-8", "replace").strip()
        if line.startswith("data:"):
            yield json.loads(line[5:])


def accepted(model, url, voice, count, input_extra=None):
    """Whether one request of `count` characters starts producing audio.

    Judged on the first audio chunk rather than on completion: a long
    non-streaming request takes many minutes to synthesize, which is itself
    the reason the app has to stream. The text is billed in full either way.
    """
    body = {"model": model,
            "input": {"text": "测试文本。" * (count // 5), "voice": voice},
            "parameters": {"response_format": "mp3", "format": "mp3"}}
    if input_extra:
        body["input"].update(input_extra)
    started = time.time()
    try:
        with post(url, body, stream=True, timeout=90) as response:
            for payload in sse_frames(response):
                if payload.get("code"):
                    print(f"{model:26s} {count:6d} chars  rejected  "
                          f"{payload.get('code')}: {str(payload.get('message'))[:70]}")
                    return False
                if ((payload.get("output") or {}).get("audio") or {}).get("data"):
                    print(f"{model:26s} {count:6d} chars  accepted  "
                          f"first packet {(time.time() - started) * 1000:.0f} ms")
                    return True
    except urllib.error.HTTPError as exc:
        print(f"{model:26s} {count:6d} chars  rejected  {describe_error(exc)}")
        return False
    print(f"{model:26s} {count:6d} chars  no audio returned")
    return False


def mp3_sample_rate(blob):
    """Sample rate declared by the first MP3 frame header, or None."""
    tables = {
        3: (44100, 48000, 32000),  # MPEG1
        2: (22050, 24000, 16000),  # MPEG2
        0: (11025, 12000, 8000),   # MPEG2.5
    }
    for i in range(len(blob) - 3):
        if blob[i] != 0xFF or (blob[i + 1] & 0xE0) != 0xE0:
            continue
        rates = tables.get((blob[i + 1] >> 3) & 0x3)
        index = (blob[i + 2] >> 2) & 0x3
        if rates is None or index == 3:
            continue
        return rates[index]
    return None


def check_rate(model, url, voice, rate):
    """Whether a requested sample_rate is honoured, judged by the rate the
    returned MP3 actually declares — a clamped rate comes back as a 200 with
    audio at some other rate, which downstream surfaces as a decode mismatch,
    not as an error."""
    body = {"model": model,
            "input": {"text": "测试。", "voice": voice,
                      "format": "mp3", "sample_rate": rate}}
    try:
        with post(url, body, stream=True, timeout=60) as response:
            for payload in sse_frames(response):
                if payload.get("code"):
                    print(f"{model:26s} sample_rate={rate:5d}  rejected  "
                          f"{payload.get('code')}: {str(payload.get('message'))[:60]}")
                    return
                data = ((payload.get("output") or {}).get("audio") or {}).get("data")
                if data:
                    actual = mp3_sample_rate(base64.b64decode(data))
                    verdict = "honoured" if actual == rate else f"mp3 says {actual}"
                    print(f"{model:26s} sample_rate={rate:5d}  {verdict}")
                    return
        print(f"{model:26s} sample_rate={rate:5d}  no audio returned")
    except urllib.error.HTTPError as exc:
        print(f"{model:26s} sample_rate={rate:5d}  {describe_error(exc)}")


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
            for payload in sse_frames(response):
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
    """Accepted single-request text length."""
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


def probe_cosyvoice():
    """Availability of CosyVoice-v3 on the same SpeechSynthesizer path as
    qwen-audio-3.0-tts-flash. Short texts only: billed per input character."""
    short = "测试。"
    dialect = "叫你去买盐，你买回来一袋面，这不是弄啥嘞吗！"

    def call(label, url, body, stream=False, timeout=60):
        started = time.time()
        try:
            if not stream:
                with post(url, body, timeout=timeout) as response:
                    output = json.loads(response.read())
                # Rejections can arrive as a 200 with an error envelope, the
                # same shape they take mid-stream — OK has to mean "no error",
                # not "no HTTPError", or a rejected voice reads as accepted.
                if output.get("code"):
                    print(
                        f"{label:58s} ERR  {output.get('code')}: "
                        f"{str(output.get('message'))[:90]}"
                    )
                    return None
                audio = (output.get("output") or {}).get("audio") or {}
                usage = output.get("usage")
                print(
                    f"{label:58s} OK   {time.time() - started:5.2f}s  "
                    f"usage={usage}  has_url={bool(audio.get('url'))}"
                )
                return output
            first = None
            chunks = total = 0
            head = None
            last = None
            with post(url, body, stream=True, timeout=timeout) as response:
                for payload in sse_frames(response):
                    last = payload
                    if payload.get("code"):
                        print(
                            f"{label:58s} SSE_ERR {payload.get('code')}: "
                            f"{str(payload.get('message'))[:90]}"
                        )
                        return None
                    data = ((payload.get("output") or {}).get("audio") or {}).get("data")
                    if data:
                        blob = base64.b64decode(data)
                        chunks += 1
                        total += len(blob)
                        if first is None:
                            first = time.time() - started
                            head = blob[:4]
            # A stream can end with framing frames only — no audio, no error.
            # That is a finding ("the model produced silence"), not a crash.
            first_ms = f"{first * 1000:.0f}ms" if first is not None else "-"
            usage = (last or {}).get("usage")
            print(
                f"{label:58s} SSE  first={first_ms}  "
                f"chunks={chunks}  bytes={total}  "
                f"magic={head.hex() if head else '-'}  usage={usage}"
            )
            return last
        except urllib.error.HTTPError as exc:
            print(f"{label:58s} {describe_error(exc)}")
            return None
        except Exception as exc:
            print(f"{label:58s} {type(exc).__name__}: {str(exc)[:90]}")
            return None

    def body(model, voice, text=short, extra=None):
        payload = {
            "model": model,
            "input": {
                "text": text,
                "voice": voice,
                "format": "mp3",
                "sample_rate": 24000,
            },
        }
        if extra:
            payload["input"].update(extra)
        return payload

    print("=== CosyVoice models on SpeechSynthesizer ===")
    call(f"{COSY_FLASH} + {COSY_VOICE}", SPEECH_SYNTH, body(COSY_FLASH, COSY_VOICE))
    call(f"{COSY_FLASH} + longanhuan", SPEECH_SYNTH, body(COSY_FLASH, "longanhuan"))
    call(f"{COSY_FLASH} + longanhuan_v3", SPEECH_SYNTH, body(COSY_FLASH, "longanhuan_v3"))
    call(f"{COSY_PLUS} + {COSY_VOICE}", SPEECH_SYNTH, body(COSY_PLUS, COSY_VOICE))
    call(f"{COSY_PLUS} + longanhuan", SPEECH_SYNTH, body(COSY_PLUS, "longanhuan"))
    call(
        f"{COSY_V35_FLASH} + {COSY_VOICE} (no system voices)",
        SPEECH_SYNTH,
        body(COSY_V35_FLASH, COSY_VOICE),
    )
    call(
        f"{COSY_FLASH} @ multimodal-generation (wrong endpoint)",
        MULTIMODAL,
        body(COSY_FLASH, COSY_VOICE),
    )

    print("\n=== voices do not carry across CosyVoice / Qwen-Audio ===")
    call(
        f"{COSY_FLASH} + qwen-audio {QWEN_AUDIO_VOICE}",
        SPEECH_SYNTH,
        body(COSY_FLASH, QWEN_AUDIO_VOICE),
    )
    call(
        f"{COSY_FLASH} + qwen-audio longanhuan_v3.6",
        SPEECH_SYNTH,
        body(COSY_FLASH, "longanhuan_v3.6"),
    )
    call(
        f"{QWEN_AUDIO} + cosyvoice {COSY_VOICE}",
        SPEECH_SYNTH,
        body(QWEN_AUDIO, COSY_VOICE),
    )

    print("\n=== SSE first packet (the number that decides whether to reuse HTTP) ===")
    long_text = TEXT + "第二句在这里。第三句也在这里。第四句收尾。"
    call(
        f"{COSY_FLASH} SSE mp3",
        SPEECH_SYNTH,
        body(COSY_FLASH, COSY_VOICE, text=long_text),
        stream=True,
    )
    call(
        f"{COSY_PLUS} SSE mp3",
        SPEECH_SYNTH,
        body(COSY_PLUS, COSY_VOICE, text=long_text),
        stream=True,
    )
    if WORKSPACE:
        call(
            f"{COSY_FLASH} SSE mp3 @ workspace host",
            f"https://{WORKSPACE}.cn-beijing.maas.aliyuncs.com"
            "/api/v1/services/audio/tts/SpeechSynthesizer",
            body(COSY_FLASH, COSY_VOICE, text=long_text),
            stream=True,
        )

    print("\n=== representative v3-flash voices (accept / reject) ===")
    for voice, name in [
        ("longanyang", "龙安洋"),
        ("longanhuan", "龙安欢"),
        ("longanhuan_v3", "龙安欢V3 方言"),
        ("longhuhu_v3", "龙呼呼 童声"),
        ("longjiaxin_v3", "龙嘉欣 粤语"),
        ("longlaotie_v3", "龙老铁 东北"),
        ("longsanshu_v3", "龙三叔 有声书"),
        ("longshuo_v3", "龙硕 新闻"),
        ("loongabby_v3", "loongabby 美式英文"),
        ("loongbella_v3", "Bella3.0"),
        ("longxiaochun_v2", "龙小淳 v2 音色（应拒绝）"),
    ]:
        call(f"{COSY_FLASH} {voice} ({name})", SPEECH_SYNTH, body(COSY_FLASH, voice))

    print("\n=== rate actually applies (byte length of downloaded mp3) ===")
    synthesize(
        "v3-flash baseline",
        COSY_FLASH,
        SPEECH_SYNTH,
        COSY_VOICE,
        extra_input={"format": "mp3"},
    )
    synthesize(
        "v3-flash input.rate=0.5",
        COSY_FLASH,
        SPEECH_SYNTH,
        COSY_VOICE,
        extra_input={"format": "mp3", "rate": 0.5},
    )
    synthesize(
        "v3-flash input.rate=2.0",
        COSY_FLASH,
        SPEECH_SYNTH,
        COSY_VOICE,
        extra_input={"format": "mp3", "rate": 2.0},
    )

    print("\n=== instruction: accepted, and does it change the audio? ===")
    baseline = synthesize(
        "v3-flash longanhuan_v3 no instruct",
        COSY_FLASH,
        SPEECH_SYNTH,
        "longanhuan_v3",
        extra_input={"format": "mp3"},
        text=dialect,
    )
    instructed = synthesize(
        "v3-flash longanhuan_v3 请用河南话表达。",
        COSY_FLASH,
        SPEECH_SYNTH,
        "longanhuan_v3",
        extra_input={"format": "mp3", "instruction": "请用河南话表达。"},
        text=dialect,
    )
    if baseline and instructed:
        delta = instructed / baseline
        print(
            f"{'instruction changed size?':38s} "
            f"{'yes' if abs(delta - 1.0) > 0.05 else 'no / unclear'}  "
            f"ratio={delta:.2f}"
        )
    call(
        f"{COSY_PLUS} instruction on system voice",
        SPEECH_SYNTH,
        body(
            COSY_PLUS,
            COSY_VOICE,
            extra={"instruction": "你说话的情感是happy。"},
        ),
    )

    print("\n=== sample_rate actually honoured (mp3 header of first chunk) ===")
    for rate in (48000, 44100, 24000, 22050, 16000):
        check_rate(COSY_FLASH, SPEECH_SYNTH, COSY_VOICE, rate)

    print("\n=== accepted length (first audio chunk, then disconnect) ===")
    # The service caps a request at 20000 units, counting a CJK character as
    # two — so 10000 (18000 units) passes and 15000 (27000 units) is refused.
    # These runs bill every accepted character; most of this probe's cost.
    for count in (500, 2000, 5000, 10000, 15000):
        if not accepted(COSY_FLASH, SPEECH_SYNTH, COSY_VOICE, count,
                        input_extra={"format": "mp3", "sample_rate": 24000}):
            break


PROBES = {
    "params": probe_params,
    "sse": probe_sse,
    "cross": probe_cross,
    "length": probe_length,
    "ws": probe_ws,
    "cosyvoice": probe_cosyvoice,
}

if __name__ == "__main__":
    requested = sys.argv[1:] or ["all"]
    # CosyVoice is opt-in: `all` keeps the original two-model research set.
    names = [name for name in PROBES if name != "cosyvoice"] if requested == ["all"] else requested
    for index, name in enumerate(names):
        if name not in PROBES:
            sys.exit(f"unknown probe {name!r}; choose from {', '.join(PROBES)} or 'all'")
        if index:
            print()
        PROBES[name]()
