//! Gemini 3.5 Live Transcription WebSocket client.
//!
//! Uses TEXT + SMART transcription, multilingual language hints, dictionary
//! customVocabulary, and the shared explicit HTTP CONNECT proxy.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc::Receiver;
use tokio_tungstenite::{
    client_async_tls,
    tungstenite::{client::IntoClientRequest, Message},
};

use super::audio_utils::resample_to_16k;
use super::config::AsrConfig;
use super::protocol::{AsrError, AsrEvent};

const GEMINI_LIVE_WS_URL: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
const GEMINI_LIVE_HOST: &str = "generativelanguage.googleapis.com";
const GEMINI_LIVE_MODEL: &str = "gemini-3.5-transcribe-live";
const SETUP_COMPLETE_WAIT_MS: u64 = 5_000;
const STREAM_COMPLETION_WAIT_MS: u64 = 10_000;

pub struct GeminiLiveClient {
    config: AsrConfig,
}

impl GeminiLiveClient {
    pub fn new(config: AsrConfig) -> Self {
        Self { config }
    }

    pub async fn stream_session<F>(
        &self,
        sample_rate: u32,
        channels: u16,
        audio_rx: Receiver<Vec<u8>>,
        cancel: tokio_util::sync::CancellationToken,
        _history: Vec<String>,
        on_event: F,
    ) -> Result<(), AsrError>
    where
        F: Fn(AsrEvent) + Send + Sync + 'static,
    {
        if self.config.gemini_api_key.trim().is_empty() {
            return Err(AsrError::ConnectionFailed(
                "Gemini API key is required".to_string(),
            ));
        }

        let stream_rate = if sample_rate == 16_000 { sample_rate } else { 16_000 };
        let diagnostics_enabled = self.config.enable_diagnostics;
        let ws_url = format!("{}?key={}", GEMINI_LIVE_WS_URL, self.config.gemini_api_key);

        log::info!(
            "Gemini Live Transcription connecting (capture {} Hz -> stream {} Hz, {} ch, model={}, proxy={})",
            sample_rate,
            stream_rate,
            channels,
            GEMINI_LIVE_MODEL,
            display_proxy(),
        );

        let request = ws_url.as_str().into_client_request().map_err(|err| {
            AsrError::ConnectionFailed(format!("Invalid Gemini Live WebSocket request: {err}"))
        })?;
        let tcp = crate::network_proxy::connect_tcp_via_http_proxy(GEMINI_LIVE_HOST, 443)
            .await
            .map_err(|err| {
                AsrError::ConnectionFailed(format!(
                    "Gemini Live proxy/TCP connection failed: {err}"
                ))
            })?;
        let (ws_stream, _) = client_async_tls(request, tcp).await.map_err(|err| {
            AsrError::ConnectionFailed(format!("Gemini Live TLS/WebSocket handshake failed: {err}"))
        })?;
        let (mut ws_write, mut ws_read) = ws_stream.split();

        let setup_message = build_setup_message(&self.config);
        if diagnostics_enabled {
            log::info!("Gemini Live transcription setup payload: {}", setup_message);
        }
        ws_write
            .send(Message::Text(setup_message.to_string()))
            .await
            .map_err(|err| {
                AsrError::ConnectionFailed(format!(
                    "Failed to send Gemini Live transcription setup: {err}"
                ))
            })?;

        wait_for_setup(&mut ws_read, &cancel, diagnostics_enabled).await?;

        let on_event = Arc::new(on_event);
        let reader_events = on_event.clone();
        let cancel_reader = cancel.clone();
        let audio_stream_ended = Arc::new(AtomicBool::new(false));
        let audio_stream_ended_reader = audio_stream_ended.clone();

        let reader_handle = tokio::spawn(async move {
            let mut transcript = TranscriptAccumulator::default();
            while let Some(frame) = tokio::select! {
                _ = cancel_reader.cancelled() => None,
                next = ws_read.next() => next,
            } {
                match frame {
                    Ok(Message::Text(text)) => {
                        let payload = parse_json_message(Message::Text(text), "runtime")?;
                        if handle_runtime_payload(
                            &payload,
                            &mut transcript,
                            &reader_events,
                            audio_stream_ended_reader.load(Ordering::SeqCst),
                            diagnostics_enabled,
                        )? {
                            break;
                        }
                    }
                    Ok(Message::Binary(bytes)) => {
                        let payload = parse_json_message(Message::Binary(bytes), "runtime")?;
                        if handle_runtime_payload(
                            &payload,
                            &mut transcript,
                            &reader_events,
                            audio_stream_ended_reader.load(Ordering::SeqCst),
                            diagnostics_enabled,
                        )? {
                            break;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        log::debug!("Gemini Live WebSocket closed by server: {:?}", frame);
                        break;
                    }
                    Ok(other) => {
                        log::debug!("Gemini Live received non-transcription frame: {:?}", other);
                    }
                    Err(err) => {
                        return Err(AsrError::ConnectionFailed(format!(
                            "Gemini Live read failed: {err}"
                        )));
                    }
                }
            }

            if !cancel_reader.is_cancelled() {
                if let Some(text) = transcript.flush_pending() {
                    reader_events(AsrEvent {
                        text,
                        is_final: true,
                        prefetch: false,
                        definite: true,
                        confidence: None,
                    });
                }
            }
            Ok::<(), AsrError>(())
        });

        let mut audio_rx = audio_rx;
        let resample_needed = sample_rate != stream_rate;
        while let Some(chunk) = tokio::select! {
            _ = cancel.cancelled() => None,
            next = audio_rx.recv() => next,
        } {
            let pcm = if resample_needed {
                resample_to_16k(&chunk, sample_rate)
            } else {
                chunk
            };
            if pcm.is_empty() {
                continue;
            }

            let audio_message = json!({
                "realtimeInput": {
                    "audio": {
                        "data": STANDARD.encode(pcm),
                        "mimeType": format!("audio/pcm;rate={}", stream_rate),
                    }
                }
            });
            ws_write
                .send(Message::Text(audio_message.to_string()))
                .await
                .map_err(|err| {
                    AsrError::ConnectionFailed(format!(
                        "Failed to send Gemini Live audio chunk: {err}"
                    ))
                })?;
        }

        if cancel.is_cancelled() {
            let _ = ws_write.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(1), reader_handle).await;
            return Ok(());
        }

        ws_write
            .send(Message::Text(
                json!({ "realtimeInput": { "audioStreamEnd": true } }).to_string(),
            ))
            .await
            .map_err(|err| {
                AsrError::ConnectionFailed(format!(
                    "Failed to send Gemini Live audioStreamEnd: {err}"
                ))
            })?;
        audio_stream_ended.store(true, Ordering::SeqCst);

        match tokio::time::timeout(Duration::from_millis(STREAM_COMPLETION_WAIT_MS), reader_handle)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => Err(AsrError::ConnectionFailed(format!(
                "Gemini Live reader task failed: {err}"
            ))),
            Err(_) => {
                log::warn!("Gemini Live timed out waiting for final transcription");
                let _ = ws_write.close().await;
                Ok(())
            }
        }
    }
}

async fn wait_for_setup<S>(
    ws_read: &mut S,
    cancel: &tokio_util::sync::CancellationToken,
    diagnostics_enabled: bool,
) -> Result<(), AsrError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => {
                return Err(AsrError::ConnectionFailed("Gemini Live setup cancelled".to_string()));
            }
            result = tokio::time::timeout(Duration::from_millis(SETUP_COMPLETE_WAIT_MS), ws_read.next()) => {
                result.map_err(|_| AsrError::ConnectionFailed(
                    "Timed out waiting for Gemini Live setupComplete".to_string()
                ))?
            }
        };

        match frame {
            Some(Ok(Message::Text(text))) => {
                let payload = parse_json_message(Message::Text(text), "setup")?;
                if diagnostics_enabled {
                    log::info!("Gemini Live setup inbound: {}", payload);
                }
                if let Some(reason) = extract_server_error(&payload) {
                    return Err(AsrError::ServerError(reason));
                }
                if payload.get("setupComplete").is_some() {
                    return Ok(());
                }
            }
            Some(Ok(Message::Binary(bytes))) => {
                let payload = parse_json_message(Message::Binary(bytes), "setup")?;
                if let Some(reason) = extract_server_error(&payload) {
                    return Err(AsrError::ServerError(reason));
                }
                if payload.get("setupComplete").is_some() {
                    return Ok(());
                }
            }
            Some(Ok(Message::Close(frame))) => {
                return Err(AsrError::ConnectionFailed(format!(
                    "Gemini Live closed during setup: {:?}", frame
                )));
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => {
                return Err(AsrError::ConnectionFailed(format!(
                    "Gemini Live setup read failed: {err}"
                )));
            }
            None => {
                return Err(AsrError::ConnectionFailed(
                    "Gemini Live closed before setup completed".to_string(),
                ));
            }
        }
    }
}

fn build_setup_message(config: &AsrConfig) -> Value {
    let vocabulary: Vec<String> = config
        .hotwords
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
        .take(1000)
        .map(ToString::to_string)
        .collect();

    json!({
        "setup": {
            "model": format!("models/{GEMINI_LIVE_MODEL}"),
            "generationConfig": {
                "responseModalities": ["TEXT"],
            },
            "realtimeInputConfig": {
                "automaticActivityDetection": {
                    "disabled": false,
                }
            },
            "inputAudioTranscription": {
                "languageCodes": language_codes(&config.gemini_language),
                "customVocabulary": vocabulary,
                "mode": "SMART",
            },
        }
    })
}

fn language_codes(language: &str) -> Vec<&'static str> {
    match language.trim() {
        "zh" => vec!["cmn-Hans-CN"],
        "en" => vec!["en-US"],
        "zh-en" => vec!["cmn-Hans-CN", "en-US"],
        _ => Vec::new(),
    }
}

fn handle_runtime_payload(
    payload: &Value,
    transcript: &mut TranscriptAccumulator,
    on_event: &Arc<impl Fn(AsrEvent) + Send + Sync + 'static>,
    audio_stream_ended: bool,
    diagnostics_enabled: bool,
) -> Result<bool, AsrError> {
    if let Some(reason) = extract_server_error(payload) {
        return Err(AsrError::ServerError(reason));
    }

    let server_content = payload.get("serverContent");
    let interim = server_content
        .and_then(|value| value.get("interimInputTranscription"))
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str);
    let final_text = server_content
        .and_then(|value| value.get("inputTranscription"))
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str);

    if diagnostics_enabled && (interim.is_some() || final_text.is_some()) {
        log::info!(
            "Gemini Live transcription event: interim={}, final={}",
            interim.is_some(), final_text.is_some()
        );
    }

    if let Some(text) = interim {
        if let Some(combined) = transcript.push_interim(text) {
            on_event(AsrEvent {
                text: combined,
                is_final: false,
                prefetch: false,
                definite: false,
                confidence: None,
            });
        }
    }

    let mut got_final = false;
    if let Some(text) = final_text {
        if let Some(combined) = transcript.commit(text) {
            got_final = true;
            on_event(AsrEvent {
                text: combined,
                is_final: true,
                prefetch: false,
                definite: true,
                confidence: None,
            });
        }
    }

    let turn_complete = server_content
        .and_then(|value| value.get("turnComplete"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(audio_stream_ended && (got_final || turn_complete))
}

fn extract_server_error(payload: &Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn parse_json_message(msg: Message, stage: &str) -> Result<Value, AsrError> {
    match msg {
        Message::Text(text) => serde_json::from_str(&text).map_err(|err| {
            AsrError::ProtocolError(format!("Invalid Gemini Live {stage} JSON event: {err}"))
        }),
        Message::Binary(bytes) => {
            let text = String::from_utf8(bytes).map_err(|err| {
                AsrError::ProtocolError(format!("Invalid Gemini Live {stage} UTF-8 event: {err}"))
            })?;
            serde_json::from_str(&text).map_err(|err| {
                AsrError::ProtocolError(format!("Invalid Gemini Live {stage} JSON event: {err}"))
            })
        }
        other => Err(AsrError::ProtocolError(format!(
            "Unsupported Gemini Live {stage} message: {:?}", other
        ))),
    }
}

fn display_proxy() -> String {
    let proxy = crate::network_proxy::current_http_proxy();
    if proxy.trim().is_empty() { "disabled".to_string() } else { proxy }
}

#[derive(Default)]
struct TranscriptAccumulator {
    committed: String,
    interim: String,
    last_emitted: String,
}

impl TranscriptAccumulator {
    fn push_interim(&mut self, text: &str) -> Option<String> {
        let normalized = text.trim().to_string();
        if normalized.is_empty() { return None; }
        self.interim = normalized;
        self.emit_current()
    }

    fn commit(&mut self, text: &str) -> Option<String> {
        let normalized = text.trim().to_string();
        if normalized.is_empty() { return None; }
        self.interim.clear();
        self.committed = merge_final(&self.committed, &normalized);
        self.emit_current()
    }

    fn flush_pending(&mut self) -> Option<String> {
        if self.interim.is_empty() { return None; }
        self.committed = merge_final(&self.committed, &self.interim);
        self.interim.clear();
        self.emit_current()
    }

    fn emit_current(&mut self) -> Option<String> {
        let combined = join_segments(&self.committed, &self.interim);
        if combined.is_empty() || combined == self.last_emitted { return None; }
        self.last_emitted = combined.clone();
        Some(combined)
    }
}

fn merge_final(existing: &str, incoming: &str) -> String {
    if existing.is_empty() { return incoming.to_string(); }
    if incoming.starts_with(existing) { return incoming.to_string(); }
    if existing.ends_with(incoming) { return existing.to_string(); }
    join_segments(existing, incoming)
}

fn join_segments(left: &str, right: &str) -> String {
    if left.is_empty() { return right.to_string(); }
    if right.is_empty() { return left.to_string(); }
    let needs_space = left
        .chars()
        .next_back()
        .zip(right.chars().next())
        .map(|(a, b)| a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric())
        .unwrap_or(false);
    if needs_space {
        format!("{} {}", left.trim_end(), right.trim_start())
    } else {
        format!("{}{}", left.trim_end(), right.trim_start())
    }
}

#[cfg(test)]
mod tests {
    use super::{build_setup_message, language_codes};
    use crate::asr::AsrConfig;

    #[test]
    fn smart_text_transcription_setup_contains_custom_vocabulary() {
        let mut config = AsrConfig::default();
        config.gemini_language = "zh-en".to_string();
        config.hotwords = vec!["VoiceX".to_string(), "Gemini".to_string()];
        let setup = build_setup_message(&config);
        let setup = &setup["setup"];
        assert_eq!(setup["generationConfig"]["responseModalities"][0], "TEXT");
        assert_eq!(setup["inputAudioTranscription"]["mode"], "SMART");
        assert_eq!(setup["inputAudioTranscription"]["customVocabulary"][0], "VoiceX");
    }

    #[test]
    fn auto_language_uses_empty_language_codes() {
        assert!(language_codes("auto").is_empty());
        assert_eq!(language_codes("zh-en"), vec!["cmn-Hans-CN", "en-US"]);
    }
}
