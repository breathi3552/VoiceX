//! Alibaba Cloud Model Studio (百炼) speech synthesis backend.
//!
//! Covers two model families behind one provider. They are separate services
//! with separate endpoints — the paths are not interchangeable, and neither are
//! their voices — but over HTTP with server-sent events they hand back the same
//! thing in the same shape: base64 MP3 in `output.audio.data`, chunk after
//! chunk, with a URL in the final frame that we discard. That similarity is
//! what makes one backend reasonable; [`ModelSpec`] holds everything that is
//! genuinely per model.
//!
//! Chosen over the two WebSocket protocols on the same measurement that settled
//! the Volcengine backend: the text is fully known before the request goes out,
//! so incremental input buys nothing, and first audio arrives in 407-539 ms
//! either way. See `docs/aliyun-tts-provider-research-2026-08-13.md` for the
//! probe results, including the several documented parameters that do not exist
//! and the several undocumented ones that do.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use base64::Engine;
use futures_util::StreamExt;
use serde_json::{json, Value};

use super::decode::{decode_mp3_stream, ChunkSource};
use super::playback::{negotiate_sample_rate_among, Playback, PlaybackHandle};
use super::{log_event, CancelToken, TtsBackend, TtsError, TtsRequest, TtsStatus, TtsVoice};

/// Region host. The workspace-scoped `{id}.cn-beijing.maas.aliyuncs.com` form
/// is what the documentation now advertises, but the plain host still serves
/// both models over both HTTP and WebSocket, so requiring a workspace id would
/// be one more mandatory field for no gain. If the plain host is ever retired,
/// `crate::asr::funasr_client::qwen_workspace_host` already builds the other.
const HOST: &str = "https://dashscope.aliyuncs.com";

pub const MODEL_QWEN3: &str = "qwen3-tts-flash";
pub const MODEL_QWEN_AUDIO: &str = "qwen-audio-3.0-tts-flash";

pub fn default_model() -> &'static str {
    MODEL_QWEN3
}

/// Everything that differs between the two model families.
///
/// The parameter names look like gratuitous variation — `speech_rate` against
/// `rate`, `response_format` against `format` — but they are separate services
/// and each ignores the other's spelling silently, returning 200 and audio at
/// the default setting. That silence is why these are a table rather than an
/// attempt to send both spellings and let the server sort it out.
struct ModelSpec {
    id: &'static str,
    path: &'static str,
    /// Rates the model renders, best first. Narrower than the playback module's
    /// own list for `qwen3-tts-flash`, which has no 44.1 or 22.05 kHz.
    sample_rates: &'static [u32],
    /// Longest text accepted in one request.
    max_chars: usize,
    voices: &'static [(&'static str, &'static str, &'static str)],
    build_body: fn(&Synthesis) -> Value,
}

/// The synthesis parameters, already on the provider's own scales.
struct Synthesis<'a> {
    text: &'a str,
    voice: &'a str,
    sample_rate: u32,
    /// Speed multiplier, 1.0 neutral, as both families take it.
    rate: f32,
}

/// Qwen3-TTS. Voices are English given names; the dialect voices are the
/// reason to reach for this family over the other one.
const QWEN3_VOICES: [(&str, &str, &str); 12] = [
    ("Cherry", "芊悦", "zh-CN"),
    ("Serena", "苏瑶", "zh-CN"),
    ("Ethan", "晨煦", "zh-CN"),
    ("Chelsie", "千雪", "zh-CN"),
    ("Nofish", "不吃鱼", "zh-CN"),
    ("Dylan", "北京-晓东", "zh-CN"),
    ("Jada", "上海-阿珍", "zh-CN"),
    ("Sunny", "四川-晴儿", "zh-CN"),
    ("Rocky", "粤语-阿强", "zh-CN"),
    ("Kiki", "粤语-阿清", "zh-CN"),
    ("Jennifer", "詹妮弗", "en-US"),
    ("Ryan", "甜茶", "en-US"),
];

/// Qwen-Audio-3.0-TTS, which shares its engine with CosyVoice — the error
/// messages say `[cosyvoice:]` outright.
const QWEN_AUDIO_VOICES: [(&str, &str, &str); 8] = [
    ("longanfengyue", "龙安风悦", "zh-CN"),
    ("longanyuanfei", "龙安元妃", "zh-CN"),
    ("longanlingxi", "龙安灵希", "zh-CN"),
    ("longanxiaoxin", "龙安小昕", "zh-CN"),
    ("longanhuan_v3.6", "龙安欢", "zh-CN"),
    ("longjielidou_v3.6", "龙杰力豆", "zh-CN"),
    ("loongmary", "loongmary", "en-GB"),
    ("loongjohn", "loongJohn", "en-US"),
];

const SPECS: [ModelSpec; 2] = [
    ModelSpec {
        id: MODEL_QWEN3,
        path: "/api/v1/services/aigc/multimodal-generation/generation",
        sample_rates: &[48_000, 24_000, 16_000],
        // Measured: 5000 characters are accepted, and the documented 512-token
        // ceiling does not exist. First audio does grow with length — 479 ms at
        // 500 characters, 1.9 s at 5000 — so this is the limit, not a target.
        max_chars: 5_000,
        voices: &QWEN3_VOICES,
        build_body: |s| {
            json!({
                "model": MODEL_QWEN3,
                "input": {
                    "text": s.text,
                    "voice": s.voice,
                    // Selections are routinely mixed Chinese and English, and
                    // naming one language makes the other one read badly.
                    "language_type": "Auto",
                },
                "parameters": {
                    "response_format": "mp3",
                    "sample_rate": s.sample_rate,
                    "speech_rate": s.rate,
                },
            })
        },
    },
    ModelSpec {
        id: MODEL_QWEN_AUDIO,
        path: "/api/v1/services/audio/tts/SpeechSynthesizer",
        sample_rates: &[48_000, 44_100, 24_000, 22_050, 16_000],
        // The service rejects at 20000, but counts something other than
        // characters getting there — a 20000-character body was reported back
        // as 36000. Left well short of the edge rather than guessing the rule.
        max_chars: 15_000,
        voices: &QWEN_AUDIO_VOICES,
        build_body: |s| {
            json!({
                "model": MODEL_QWEN_AUDIO,
                "input": {
                    "text": s.text,
                    "voice": s.voice,
                    "format": "mp3",
                    "sample_rate": s.sample_rate,
                    "rate": s.rate,
                },
            })
        },
    },
];

fn spec_for(model: &str) -> &'static ModelSpec {
    SPECS
        .iter()
        .find(|spec| spec.id == model)
        .unwrap_or(&SPECS[0])
}

pub fn default_voice_for(model: &str) -> &'static str {
    spec_for(model).voices[0].0
}

#[derive(Debug, Clone)]
pub struct AliyunConfig {
    pub api_key: String,
    pub model: String,
}

/// Convert the stored 0.0..=1.0 rate into the provider's own multiplier.
///
/// The stored value is normalized around the macOS engine default of 0.5, shown
/// as 1x on a 0.5x-2x slider, and both model families take exactly that
/// multiplier over exactly that range — so unlike Volcengine's -50..=100 this
/// needs no remapping, only the same clamp.
fn speed_from_normalized(rate: Option<f32>) -> f32 {
    let Some(rate) = rate else { return 1.0 };
    (rate / 0.5).clamp(0.5, 2.0)
}

pub struct AliyunBackend {
    config: Mutex<AliyunConfig>,
    /// Filled in by the decode thread once it owns the output device, so `stop`
    /// can cut the audio immediately instead of waiting for the network side to
    /// notice. Shared rather than copied: the handle does not exist yet when
    /// `start` returns.
    playback: Arc<Mutex<Option<PlaybackHandle>>>,
    speaking: Arc<AtomicBool>,
}

impl AliyunBackend {
    pub fn new(config: AliyunConfig) -> Self {
        Self {
            config: Mutex::new(config),
            playback: Arc::new(Mutex::new(None)),
            speaking: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn apply_config(&self, config: AliyunConfig) {
        if let Ok(mut slot) = self.config.lock() {
            *slot = config;
        }
    }

    fn config(&self) -> Result<AliyunConfig, TtsError> {
        let config = self
            .config
            .lock()
            .map(|slot| slot.clone())
            .map_err(|_| TtsError::Backend("configuration is poisoned".to_string()))?;
        if config.api_key.trim().is_empty() {
            return Err(TtsError::Backend("no API key configured".to_string()));
        }
        Ok(config)
    }

    fn spec(&self) -> &'static ModelSpec {
        self.config
            .lock()
            .map(|slot| spec_for(&slot.model))
            .unwrap_or(&SPECS[0])
    }
}

impl TtsBackend for AliyunBackend {
    fn name(&self) -> &'static str {
        "aliyun"
    }

    fn list_voices(&self) -> Result<Vec<TtsVoice>, TtsError> {
        Ok(self
            .spec()
            .voices
            .iter()
            .map(|(id, name, language)| TtsVoice {
                id: id.to_string(),
                name: name.to_string(),
                language: language.to_string(),
            })
            .collect())
    }

    fn start(&self, request: TtsRequest, token: CancelToken) -> Result<(), TtsError> {
        // Every failure path has to hand the session back, or the session stays
        // claimed: the hotkey is stuck in "stop" mode from then on and the HUD
        // sits on "preparing" forever.
        self.begin(request, token.clone()).inspect_err(|_| {
            token.finish();
        })
    }

    fn stop(&self) -> Result<(), TtsError> {
        self.speaking.store(false, Ordering::SeqCst);
        if let Ok(slot) = self.playback.lock() {
            if let Some(handle) = slot.as_ref() {
                handle.stop();
            }
        }
        Ok(())
    }

    fn status(&self) -> TtsStatus {
        if self.speaking.load(Ordering::SeqCst) {
            TtsStatus::Speaking
        } else {
            TtsStatus::Idle
        }
    }

    fn audio_level(&self) -> Option<f32> {
        self.playback
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|handle| handle.level()))
    }

    fn max_chars(&self) -> Option<usize> {
        Some(self.spec().max_chars)
    }
}

impl AliyunBackend {
    fn begin(&self, request: TtsRequest, token: CancelToken) -> Result<(), TtsError> {
        let config = self.config()?;
        let spec = spec_for(&config.model);
        let sample_rate = negotiate_sample_rate_among(spec.sample_rates)
            .map_err(|err| TtsError::Backend(format!("{} ({})", err, err.code())))?;

        let voice = request
            .voice
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| spec.voices[0].0.to_string());
        // Pitch is deliberately not sent. `pitch_rate` does apply, but halving
        // it stretched the audio to 4.2x rather than the 2x a resampling pitch
        // shift would give, so what it actually changes is unclear and the
        // settings page hides the row for cloud providers anyway.
        let speed = speed_from_normalized(request.rate);
        let gain = request.volume.unwrap_or(1.0);

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Lets the decode side tell "the provider failed" apart from "the audio
        // ended", which otherwise both look like a closed channel.
        let network_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let source = ChunkSource::new(rx, token.clone());
        let decode_token = token.clone();
        let decode_error = network_error.clone();

        // Clear any handle left by the previous utterance before publishing the
        // slot, so a stop arriving now cannot reach a device we already closed.
        if let Ok(mut slot) = self.playback.lock() {
            *slot = None;
        }

        let playback_slot = self.playback.clone();
        // Raised by the decode thread when the first samples reach the device,
        // not here: a request in flight is not a sound, and the HUD tells those
        // two states apart.
        let speaking = self.speaking.clone();
        thread::Builder::new()
            .name("voicex-tts-cloud".to_string())
            .spawn(move || {
                run_playback(
                    source,
                    sample_rate,
                    gain,
                    decode_token,
                    decode_error,
                    playback_slot,
                    speaking.clone(),
                );
                speaking.store(false, Ordering::SeqCst);
            })
            .map_err(|err| TtsError::Backend(format!("failed to spawn the decoder: {err}")))?;

        let text = request.text;
        let http_token = token;
        let http_error = network_error;
        tauri::async_runtime::spawn(async move {
            let outcome = stream_audio(
                &config.api_key,
                spec,
                &Synthesis {
                    text: &text,
                    voice: &voice,
                    sample_rate,
                    rate: speed,
                },
                &tx,
                &http_token,
            )
            .await;
            if let Err(err) = outcome {
                if let Ok(mut slot) = http_error.lock() {
                    *slot = Some(err);
                }
            }
            // Closing the channel is what ends the decode loop.
            drop(tx);
        });

        Ok(())
    }
}

/// Decode and play, on a thread of its own because both block and because the
/// output stream may not cross threads.
fn run_playback(
    source: ChunkSource,
    sample_rate: u32,
    gain: f32,
    token: CancelToken,
    network_error: Arc<Mutex<Option<String>>>,
    handle_slot: Arc<Mutex<Option<PlaybackHandle>>>,
    speaking: Arc<AtomicBool>,
) {
    let playback = match Playback::open(sample_rate, gain) {
        Ok(playback) => playback,
        Err(err) => {
            if token.finish() {
                log_event(
                    "speak_err",
                    &[
                        ("error", err.code().to_string()),
                        ("detail", err.to_string()),
                    ],
                );
            }
            return;
        }
    };

    if let Ok(mut slot) = handle_slot.lock() {
        *slot = Some(playback.handle());
    }

    let mut started = false;
    let decoded = decode_mp3_stream(source, sample_rate, |samples| {
        if !started {
            started = true;
            speaking.store(true, Ordering::SeqCst);
            log_event("speak_started", &[]);
        }
        playback.push(samples)
    });

    let failure = network_error.lock().ok().and_then(|slot| slot.clone());

    match decoded {
        Ok(_) if failure.is_none() => {
            playback.mark_end_of_stream();
            match playback.wait_until_drained(&token) {
                Ok(true) => {
                    if token.finish() {
                        log_event("speak_finished", &[]);
                    }
                }
                // Cancelled: whoever cancelled owns the session now.
                Ok(false) => {}
                Err(err) => {
                    if token.finish() {
                        log_event(
                            "speak_err",
                            &[
                                ("error", err.code().to_string()),
                                ("detail", err.to_string()),
                            ],
                        );
                    }
                }
            }
        }
        // A network failure reads as a truncated stream, so report the real
        // cause rather than the decoder's confusion about it.
        _ => {
            let detail = failure.unwrap_or_else(|| match &decoded {
                Err(err) => err.to_string(),
                Ok(_) => "the audio stream ended early".to_string(),
            });
            if token.finish() {
                log_event(
                    "speak_err",
                    &[("error", "backend".to_string()), ("detail", detail)],
                );
            }
        }
    }
}

/// Stream the synthesis response, forwarding decoded audio bytes to `tx`.
async fn stream_audio(
    api_key: &str,
    spec: &ModelSpec,
    synthesis: &Synthesis<'_>,
    tx: &Sender<Vec<u8>>,
    token: &CancelToken,
) -> Result<(), String> {
    let response = reqwest::Client::new()
        .post(format!("{HOST}{}", spec.path))
        .header("Authorization", format!("Bearer {api_key}"))
        // Without this the service synthesizes the whole text before replying,
        // which for a long selection takes minutes rather than seconds.
        .header("X-DashScope-SSE", "enable")
        .json(&(spec.build_body)(synthesis))
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;

    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {}", describe_failure(&detail)));
    }

    let mut stream = response.bytes_stream();
    let mut pending = Vec::<u8>::new();

    while let Some(chunk) = stream.next().await {
        if token.is_cancelled() {
            return Ok(());
        }
        let chunk = chunk.map_err(|err| format!("stream broke: {err}"))?;
        pending.extend_from_slice(&chunk);

        // Server-sent events are line-oriented and a chunk may split a line.
        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=newline).collect();
            if let Some(audio) = parse_line(&line[..line.len() - 1])? {
                if tx.send(audio).is_err() {
                    // The decoder is gone; nothing left to stream into.
                    return Ok(());
                }
            }
        }
    }

    if !pending.is_empty() {
        if let Some(audio) = parse_line(&pending)? {
            let _ = tx.send(audio);
        }
    }

    Ok(())
}

/// Decode one SSE line into audio bytes, or `None` when it carries none.
///
/// Everything but `data:` is framing — `id:`, `event:`, the `:HTTP_STATUS/200`
/// comment, and the blank line between events.
fn parse_line(line: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let text = std::str::from_utf8(line).map_err(|_| "response was not UTF-8".to_string())?;
    let Some(payload) = text.trim_end_matches('\r').strip_prefix("data:") else {
        return Ok(None);
    };
    let payload = payload.trim_start();
    if payload.is_empty() {
        return Ok(None);
    }

    let value: Value =
        serde_json::from_str(payload).map_err(|err| format!("malformed response: {err}"))?;

    // Failures arrive mid-stream with the same 200 as the audio frames, so the
    // only signal is this field appearing.
    if let Some(code) = value.get("code").and_then(|code| code.as_str()) {
        let message = value
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("unknown error");
        return Err(format!("provider error {code}: {message}"));
    }

    let Some(data) = value
        .pointer("/output/audio/data")
        .and_then(|data| data.as_str())
    else {
        return Ok(None);
    };
    // The final frame carries the finished file's URL and an empty `data`. We
    // already have every byte it points at.
    if data.is_empty() {
        return Ok(None);
    }

    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map(Some)
        .map_err(|err| format!("bad base64 in response: {err}"))
}

/// Pull the useful part out of an error body, falling back to its first line.
fn describe_failure(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            let code = value.get("code").and_then(|code| code.as_str())?;
            let message = value
                .get("message")
                .and_then(|message| message.as_str())
                .unwrap_or("");
            Some(format!("{code}: {message}"))
        })
        .unwrap_or_else(|| body.lines().next().unwrap_or_default().to_string())
        .chars()
        .take(200)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_scales_line_up_at_both_ends_and_the_middle() {
        // Stored 0..1 around a 0.5 default, shown as 0.5x-2x, sent as the same
        // 0.5-2.0 multiplier. Getting this wrong is silent: the voice just
        // speaks at the wrong speed.
        assert_eq!(speed_from_normalized(Some(0.25)), 0.5, "0.5x");
        assert_eq!(speed_from_normalized(Some(0.5)), 1.0, "1x is neutral");
        assert_eq!(speed_from_normalized(Some(1.0)), 2.0, "2x");
        assert_eq!(speed_from_normalized(None), 1.0, "unset is neutral");
    }

    #[test]
    fn rates_outside_the_slider_are_clamped_to_what_the_provider_accepts() {
        assert_eq!(speed_from_normalized(Some(0.0)), 0.5);
        assert_eq!(speed_from_normalized(Some(5.0)), 2.0);
    }

    #[test]
    fn each_model_spells_its_parameters_its_own_way() {
        // The two services ignore each other's spelling silently — 200 and
        // audio at the default rate — so a mix-up here would be audible only as
        // "the speed slider does nothing".
        let synthesis = Synthesis {
            text: "hi",
            voice: "Cherry",
            sample_rate: 24_000,
            rate: 1.5,
        };

        let qwen3 = (spec_for(MODEL_QWEN3).build_body)(&synthesis);
        assert_eq!(qwen3["parameters"]["speech_rate"], 1.5);
        assert_eq!(qwen3["parameters"]["response_format"], "mp3");
        assert_eq!(qwen3["input"]["language_type"], "Auto");
        assert!(qwen3["input"].get("format").is_none());

        let audio = (spec_for(MODEL_QWEN_AUDIO).build_body)(&synthesis);
        assert_eq!(audio["input"]["rate"], 1.5);
        assert_eq!(audio["input"]["format"], "mp3");
        assert!(audio["input"].get("language_type").is_none());
        assert!(audio.get("parameters").is_none());
    }

    #[test]
    fn qwen3_never_asks_for_a_rate_it_cannot_render() {
        // The playback module's own list includes 44.1 and 22.05 kHz, which
        // this model does not render. Asking anyway comes back as audio at some
        // other rate and surfaces as a decode mismatch, not as speech.
        for rate in spec_for(MODEL_QWEN3).sample_rates {
            assert!(
                matches!(rate, 48_000 | 24_000 | 16_000 | 8_000),
                "{rate} Hz is not a qwen3-tts-flash rate"
            );
        }
    }

    #[test]
    fn audio_frames_yield_bytes_and_framing_lines_yield_none() {
        let audio = parse_line(br#"data:{"output":{"audio":{"data":"aGVsbG8="}}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(audio, b"hello");

        // The final frame points at the finished file; we already have it.
        let last =
            parse_line(br#"data:{"output":{"audio":{"data":"","url":"http://x"}}}"#).unwrap();
        assert!(last.is_none());

        for framing in [
            &b"id:1"[..],
            b"event:result",
            b":HTTP_STATUS/200",
            b"",
            b"data:",
        ] {
            assert!(parse_line(framing).unwrap().is_none(), "{framing:?}");
        }
    }

    #[test]
    fn a_mid_stream_failure_is_reported_rather_than_read_as_end_of_audio() {
        // These arrive with the same HTTP 200 as the audio frames, so missing
        // one would look like a read that simply stopped early.
        let err = parse_line(
            br#"data:{"code":"InvalidParameter","message":"Invalid voice specified","request_id":"x"}"#,
        )
        .unwrap_err();
        assert!(err.contains("InvalidParameter"), "{err}");
        assert!(err.contains("Invalid voice"), "{err}");
    }

    #[test]
    fn a_carriage_return_does_not_hide_the_payload() {
        // CRLF framing would otherwise leave a trailing \r inside the JSON.
        let audio = parse_line(b"data:{\"output\":{\"audio\":{\"data\":\"aGk=\"}}}\r")
            .unwrap()
            .unwrap();
        assert_eq!(audio, b"hi");
    }

    #[test]
    fn http_failures_report_the_provider_code_not_the_raw_body() {
        let described = describe_failure(
            r#"{"code":"InvalidApiKey","message":"Invalid API-key provided.","request_id":"x"}"#,
        );
        assert_eq!(described, "InvalidApiKey: Invalid API-key provided.");
        assert_eq!(
            describe_failure("gateway timeout\nsecond line"),
            "gateway timeout"
        );
    }

    #[test]
    fn an_unknown_model_falls_back_rather_than_panicking() {
        // Settings are user-editable and survive downgrades, so a model string
        // this build has never heard of has to resolve to something.
        assert_eq!(spec_for("qwen9-tts-imaginary").id, MODEL_QWEN3);
        assert_eq!(default_voice_for("qwen9-tts-imaginary"), "Cherry");
        assert_eq!(default_voice_for(MODEL_QWEN_AUDIO), "longanfengyue");
    }

    #[test]
    fn switching_model_switches_the_voice_table_with_it() {
        // What the settings page's voice picker rides on: it re-asks after a
        // model change, and the controller answers by re-applying the config
        // first. If the table did not follow, the picker would offer voices the
        // selected model rejects.
        let backend = AliyunBackend::new(AliyunConfig {
            api_key: "sk-test".to_string(),
            model: MODEL_QWEN3.to_string(),
        });
        let listed = backend.list_voices().unwrap();
        assert!(listed.iter().any(|voice| voice.id == "Cherry"));
        assert!(!listed.iter().any(|voice| voice.id == "longanfengyue"));

        backend.apply_config(AliyunConfig {
            api_key: "sk-test".to_string(),
            model: MODEL_QWEN_AUDIO.to_string(),
        });
        let listed = backend.list_voices().unwrap();
        assert!(listed.iter().any(|voice| voice.id == "longanfengyue"));
        assert!(!listed.iter().any(|voice| voice.id == "Cherry"));

        // The text limit rides along, so a long selection is trimmed to what
        // the model in force actually accepts.
        assert_eq!(backend.max_chars(), Some(15_000));
    }

    #[test]
    fn a_refused_start_hands_the_session_back() {
        // Without this the session stays claimed forever: the read hotkey turns
        // into a permanent "stop", and the HUD sits on "preparing" with nothing
        // ever coming. Reachable by simply not having configured a key yet.
        let backend = AliyunBackend::new(AliyunConfig {
            api_key: String::new(),
            model: MODEL_QWEN3.to_string(),
        });
        let slot = crate::tts::SessionSlot::default();
        let token = slot.claim();
        assert!(slot.is_active());

        let outcome = backend.start(TtsRequest::plain("hi".to_string()), token);

        assert!(outcome.is_err(), "an unconfigured backend must refuse");
        assert!(!slot.is_active(), "a refusal must release the session");
        assert_eq!(backend.status(), TtsStatus::Idle);
    }

    /// End-to-end against the live service for both models: network, SSE parse,
    /// MP3 decode and playback, in the same arrangement the backend uses. Plays
    /// audio, so it is opt-in:
    ///
    /// ```text
    /// ALIYUN_TTS_API_KEY=... cargo test --lib aliyun::tests::live -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires network access and credentials"]
    fn live_synthesis_decodes_and_plays() {
        use crate::tts::SessionSlot;
        use std::time::Instant;

        let api_key = std::env::var("ALIYUN_TTS_API_KEY").expect("ALIYUN_TTS_API_KEY is not set");

        for spec in SPECS.iter() {
            let rate = negotiate_sample_rate_among(spec.sample_rates)
                .expect("no usable output sample rate");
            let voice = spec.voices[0].0;
            eprintln!("\n=== {} @ {rate} Hz, voice {voice} ===", spec.id);

            let text = "阿里云百炼语音合成，端到端链路验证：流式接收、MP3 解码与本地播放。";
            let slot = SessionSlot::default();
            let token = slot.claim();
            let (tx, rx) = mpsc::channel::<Vec<u8>>();
            let source = ChunkSource::new(rx, token.clone());

            let decode_token = token.clone();
            let decoder = thread::spawn(move || {
                let playback = Playback::open(rate, 1.0).expect("failed to open the output device");
                let handle = playback.handle();
                // Sampled while audio is playing, because this drives the HUD
                // waveform and a level that only appears after the last sample
                // would leave the bars hidden for the whole read.
                let mut levels: Vec<f32> = Vec::new();
                let samples = decode_mp3_stream(source, rate, |chunk| {
                    if let Some(level) = handle.level() {
                        levels.push(level);
                    }
                    playback.push(chunk)
                })
                .expect("decode failed");
                playback.mark_end_of_stream();
                playback
                    .wait_until_drained(&decode_token)
                    .expect("playback stalled");
                (samples, levels)
            });

            let started = Instant::now();
            let outcome = tauri::async_runtime::block_on(stream_audio(
                &api_key,
                spec,
                &Synthesis {
                    text,
                    voice,
                    sample_rate: rate,
                    rate: 1.0,
                },
                &tx,
                &token,
            ));
            drop(tx);
            outcome.expect("streaming failed");

            let (samples, levels) = decoder.join().expect("decoder panicked");
            let seconds = samples as f64 / rate as f64;
            let peak = levels.iter().copied().fold(0.0f32, f32::max);
            eprintln!(
                "decoded {samples} samples ({seconds:.2} s) in {:?}, peak level {peak:.4}",
                started.elapsed()
            );

            assert!(
                seconds > 2.0,
                "{}: expected several seconds of speech, decoded {seconds:.2} s",
                spec.id
            );
            assert!(
                peak > 0.01,
                "{}: the HUD waveform is driven by this level, and it stayed at {peak:.4} \
                 while audio was playing",
                spec.id
            );
        }
    }

    /// Which voice ids the account actually accepts. Voice tables are hand-kept
    /// (there is no list endpoint), and a wrong id fails only at speak time with
    /// `Invalid voice specified` — so this checks them all in one pass:
    ///
    /// ```text
    /// ALIYUN_TTS_API_KEY=... cargo test --lib aliyun::tests::voice_table -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires network access and credentials"]
    fn voice_table_matches_what_the_service_accepts() {
        let api_key = std::env::var("ALIYUN_TTS_API_KEY").expect("ALIYUN_TTS_API_KEY is not set");
        let mut rejected: Vec<String> = Vec::new();

        for spec in SPECS.iter() {
            for (id, name, _) in spec.voices {
                let slot = crate::tts::SessionSlot::default();
                let token = slot.claim();
                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                // Drained on a thread so a slow reader cannot stall the sender.
                let drain = thread::spawn(move || rx.iter().count());

                let outcome = tauri::async_runtime::block_on(stream_audio(
                    &api_key,
                    spec,
                    &Synthesis {
                        text: "测试。",
                        voice: id,
                        sample_rate: spec.sample_rates[0],
                        rate: 1.0,
                    },
                    &tx,
                    &token,
                ));
                drop(tx);
                let chunks = drain.join().unwrap_or(0);

                match outcome {
                    Ok(()) if chunks > 0 => eprintln!("  ok      {} {id} ({name})", spec.id),
                    Ok(()) => {
                        eprintln!("  SILENT  {} {id} ({name})", spec.id);
                        rejected.push(format!("{} {id}: no audio", spec.id));
                    }
                    Err(err) => {
                        eprintln!("  REJECT  {} {id} ({name}): {err}", spec.id);
                        rejected.push(format!("{} {id}: {err}", spec.id));
                    }
                }
            }
        }

        assert!(rejected.is_empty(), "voice table is wrong:\n{rejected:#?}");
    }
}
