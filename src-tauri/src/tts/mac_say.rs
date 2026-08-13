//! macOS Spoken Content voice, reached by spawning `/usr/bin/say` without `-v`.
//!
//! [`super::mac_system::MacSystemBackend`] speaks through `AVSpeechSynthesizer`,
//! whose catalogue is compact-only — Siri Neural voices are absent from
//! `speechVoices()` and rejected by `voiceWithIdentifier`. `say` without `-v`
//! uses the voice from System Settings → Accessibility → Spoken Content, which
//! on current macOS is typically a Siri Neural voice. Passing `-v` would drop
//! back to the compact catalogue, so this backend never does that.
//!
//! Volume and pitch have no `say` flags and are ignored; rate maps onto `-r`
//! in words per minute, and the stored 1x mark omits `-r` so Siri keeps its
//! own pacing. There is no audio tap, so [`TtsBackend::audio_level`] stays
//! `None` as with AVSpeech.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::{log_event, CancelToken, TtsBackend, TtsError, TtsRequest, TtsStatus, TtsVoice};

const SAY_BIN: &str = "/usr/bin/say";
/// Stored 0..=1 scale where 0.5 is the engine's 1x mark, matching AVSpeech.
const DEFAULT_RATE: f32 = 0.5;
/// `say -r` unit. NSSpeechSynthesizer's documented default, used only when the
/// stored rate is off the 1x mark so we actually pass `-r`.
const DEFAULT_WPM: f32 = 175.0;
const WAIT_POLL: Duration = Duration::from_millis(20);

struct Shared {
    child: Mutex<Option<Child>>,
    speaking: AtomicBool,
}

pub struct MacSayBackend {
    shared: Arc<Shared>,
}

impl MacSayBackend {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                child: Mutex::new(None),
                speaking: AtomicBool::new(false),
            }),
        }
    }

    /// Kill the current child, if any. `log_cancel` is for an explicit stop;
    /// replacing the child for a newer request stays quiet, matching AVSpeech
    /// ignoring the stale `didCancel`.
    fn stop_child(&self, log_cancel: bool) {
        let Some(mut child) = self
            .shared
            .child
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
        else {
            return;
        };
        let _ = child.kill();
        // Reaped off-thread because this runs on the hotkey worker, which has
        // dictation's `Pressed` behind it. Failure to spawn is not fatal and
        // must not panic: `Drop` comes through here during teardown, where a
        // panic would abort the process over an already-killed child that the
        // OS is about to reap anyway.
        let reaper = thread::Builder::new()
            .name("voicex-tts-say-stop".to_string())
            .spawn(move || {
                let _ = child.wait();
                if log_cancel {
                    log_event("speak_cancelled", &[]);
                }
            });
        if let Err(err) = reaper {
            log::warn!("Could not spawn the say reaper; the child was killed anyway: {err}");
        }
    }

    fn spawn_waiter(&self, token: CancelToken) {
        let shared = self.shared.clone();
        thread::Builder::new()
            .name("voicex-tts-say-wait".to_string())
            .spawn(move || loop {
                {
                    let mut slot = match shared.child.lock() {
                        Ok(slot) => slot,
                        Err(_) => return,
                    };
                    // The slot is shared, so what is in it now is not
                    // necessarily what this waiter was started for: `start`
                    // takes the old child out and puts a new one in, and that
                    // can happen entirely inside one `WAIT_POLL` nap. Reaping
                    // someone else's child would strand them — their own
                    // waiter would find the slot empty and return without ever
                    // finishing the session, leaving the HUD driver spinning
                    // on a read that already ended.
                    //
                    // The token says whose turn it is: a newer request claims
                    // the session before it calls `start`, so a cancelled
                    // token means the child below belongs to someone else.
                    // Checked under the lock, because `start` needs the same
                    // lock to install the replacement — so if the token is
                    // still live here, the child in the slot is ours.
                    if token.is_cancelled() {
                        return;
                    }
                    match slot.as_mut().map(Child::try_wait) {
                        Some(Ok(Some(status))) => {
                            let mut child = slot.take().expect("try_wait found an exit");
                            drop(slot);
                            let stderr = read_stderr(&mut child);
                            if token.finish() {
                                shared.speaking.store(false, Ordering::SeqCst);
                                if status.success() {
                                    log_event("speak_finished", &[]);
                                } else {
                                    let detail = if stderr.is_empty() {
                                        format!("say exited with {status}")
                                    } else {
                                        format!("say exited with {status}: {stderr}")
                                    };
                                    log_event(
                                        "speak_err",
                                        &[
                                            (
                                                "error",
                                                TtsError::Backend(detail.clone())
                                                    .code()
                                                    .to_string(),
                                            ),
                                            ("detail", detail),
                                        ],
                                    );
                                }
                            }
                            return;
                        }
                        Some(Ok(None)) => {}
                        Some(Err(err)) => {
                            let _ = slot.take();
                            drop(slot);
                            if token.finish() {
                                shared.speaking.store(false, Ordering::SeqCst);
                                log_event(
                                    "speak_err",
                                    &[
                                        (
                                            "error",
                                            TtsError::Backend(err.to_string()).code().to_string(),
                                        ),
                                        ("detail", err.to_string()),
                                    ],
                                );
                            }
                            return;
                        }
                        None => return,
                    }
                }
                thread::sleep(WAIT_POLL);
            })
            .expect("failed to spawn the say waiter");
    }
}

impl Default for MacSayBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MacSayBackend {
    fn drop(&mut self) {
        self.stop_child(false);
    }
}

impl TtsBackend for MacSayBackend {
    fn name(&self) -> &'static str {
        "mac_say"
    }

    fn list_voices(&self) -> Result<Vec<TtsVoice>, TtsError> {
        // The picker lists AVSpeech compact voices plus an empty "system
        // default" entry. This backend is only the empty-id speak path.
        Ok(Vec::new())
    }

    fn start(&self, request: TtsRequest, token: CancelToken) -> Result<(), TtsError> {
        if token.is_cancelled() {
            token.finish();
            log_event("speak_discarded", &[("reason", "superseded".to_string())]);
            return Ok(());
        }

        self.stop_child(false);

        if token.is_cancelled() {
            token.finish();
            log_event("speak_discarded", &[("reason", "superseded".to_string())]);
            return Ok(());
        }

        let mut command = Command::new(SAY_BIN);
        command.args(say_args(request.rate));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|err| TtsError::Backend(format!("failed to start {SAY_BIN}: {err}")))?;

        match child.stdin.take() {
            Some(mut stdin) => {
                if let Err(err) = stdin.write_all(request.text.as_bytes()) {
                    let _ = child.kill();
                    let _ = child.wait();
                    token.finish();
                    return Err(TtsError::Backend(format!(
                        "failed to send text to say: {err}"
                    )));
                }
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                token.finish();
                return Err(TtsError::Backend("say stdin was not piped".to_string()));
            }
        }

        if token.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            token.finish();
            log_event("speak_discarded", &[("reason", "superseded".to_string())]);
            return Ok(());
        }

        {
            let mut slot = self
                .shared
                .child
                .lock()
                .map_err(|err| TtsError::Backend(format!("say child lock: {err}")))?;
            *slot = Some(child);
        }

        self.shared.speaking.store(true, Ordering::SeqCst);
        log_event("speak_started", &[]);
        self.spawn_waiter(token);
        Ok(())
    }

    fn stop(&self) -> Result<(), TtsError> {
        self.shared.speaking.store(false, Ordering::SeqCst);
        // Non-blocking: dictation's Pressed sits behind this call on the same
        // worker, same reason MacSystemBackend does not wait for the main thread.
        self.stop_child(true);
        Ok(())
    }

    /// Weaker than [`super::mac_system::MacSystemBackend`]'s: this reports
    /// `Speaking` once `say` has been spawned, not once audio is audible.
    /// `say` gives us no start signal, and the process exiting is the only
    /// thing we can observe — so `speak_started` is logged at spawn here too.
    ///
    /// The visible cost is the HUD's `Preparing` phase, which exists to cover
    /// the silence before a voice begins: on this path it is skipped, and a
    /// Siri voice loading for the first time shows as "reading" while still
    /// quiet. Preferred to inventing a delay that would be wrong on every
    /// machine but the one it was measured on.
    fn status(&self) -> TtsStatus {
        if self.shared.speaking.load(Ordering::SeqCst) {
            TtsStatus::Speaking
        } else {
            TtsStatus::Idle
        }
    }
}

/// Arguments for `/usr/bin/say`. Text goes on stdin via `-f -`; `-v` is never
/// present, which is the whole point of this backend.
fn say_args(rate: Option<f32>) -> Vec<String> {
    let mut args = vec!["-f".to_string(), "-".to_string()];
    if let Some(wpm) = rate_to_wpm(rate) {
        args.push("-r".to_string());
        args.push(wpm.to_string());
    }
    args
}

/// Map the stored 0..=1 rate onto `say -r` words-per-minute.
///
/// `None` or a value at the 1x mark omits `-r`, so the Siri voice keeps the
/// pacing the user hears from the terminal.
fn rate_to_wpm(rate: Option<f32>) -> Option<u32> {
    let rate = rate?;
    if (rate - DEFAULT_RATE).abs() < 0.01 {
        return None;
    }
    let wpm = (DEFAULT_WPM * (rate / DEFAULT_RATE)).round();
    Some(wpm.clamp(80.0, 500.0) as u32)
}

fn read_stderr(child: &mut Child) -> String {
    let mut buf = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut buf);
    }
    buf.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rate_omits_the_rate_flag() {
        assert_eq!(rate_to_wpm(None), None);
        assert_eq!(rate_to_wpm(Some(0.5)), None);
        assert_eq!(say_args(Some(0.5)), ["-f", "-"]);
    }

    #[test]
    fn off_default_rate_becomes_words_per_minute() {
        // 0.5x and 2x on the stored scale, around a 175 wpm 1x.
        assert_eq!(rate_to_wpm(Some(0.25)), Some(88));
        assert_eq!(rate_to_wpm(Some(1.0)), Some(350));
        assert_eq!(say_args(Some(1.0)), ["-f", "-", "-r", "350"]);
    }

    #[test]
    fn say_args_never_select_a_voice() {
        // `-v` would force a compact voice and throw away Siri quality.
        for args in [say_args(None), say_args(Some(0.5)), say_args(Some(1.0))] {
            assert!(
                !args.iter().any(|a| a == "-v" || a == "--voice"),
                "say args must not pick a voice: {args:?}"
            );
        }
    }

    #[test]
    fn say_accepts_stdin_without_a_voice_flag() {
        // Pins the command shape against a real binary: stdin, no `-v`, write
        // to a file so this does not play through the speakers.
        let path =
            std::env::temp_dir().join(format!("voicex-say-test-{}.aiff", std::process::id()));
        let mut child = Command::new(SAY_BIN)
            .args(["-f", "-", "-o"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("say should be at /usr/bin/say");
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all("测试".as_bytes())
            .expect("write stdin");
        let status = child.wait().expect("wait for say");
        let _ = std::fs::remove_file(&path);
        assert!(status.success(), "say exited with {status}");
    }
}
