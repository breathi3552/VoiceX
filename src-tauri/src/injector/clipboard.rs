//! Clipboard-based text injection

use arboard::{Clipboard, ImageData};
#[cfg(target_os = "macos")]
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, EventField};
#[cfg(target_os = "macos")]
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
#[cfg(target_os = "windows")]
use enigo::Direction;
#[cfg(target_os = "windows")]
use enigo::Key as EnigoKey;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use enigo::{Enigo, Keyboard, Settings};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use rdev::{simulate, EventType, Key};
use std::borrow::Cow;
use std::thread;
use std::time::Duration;

/// Maximum characters to inject via typing mode before falling back to clipboard.
/// Typing is slower but avoids clipboard sync issues; use clipboard for long text.
const TYPING_MODE_MAX_CHARS: usize = 500;

/// How long we wait for a clipboard write to become readable again.
const CLIPBOARD_WRITE_VERIFY_RETRIES: usize = 5;
const CLIPBOARD_WRITE_VERIFY_INTERVAL_MS: u64 = 20;

/// Give clipboard bridges (for example remote desktop sessions) a brief window
/// to observe the new clipboard payload before we synthesize paste.
#[cfg(target_os = "macos")]
const CLIPBOARD_PRE_PASTE_DELAY_MS: u64 = 120;
#[cfg(not(target_os = "macos"))]
const CLIPBOARD_PRE_PASTE_DELAY_MS: u64 = 80;

/// Physical keycodes for a layout-independent Command+V shortcut on macOS.
#[cfg(target_os = "macos")]
const MACOS_COMMAND_KEYCODE: u16 = 55;
#[cfg(target_os = "macos")]
const MACOS_V_KEYCODE: u16 = 9;

/// How long to wait after paste before the detached restore thread puts the
/// user's previous clipboard content back. Targets that consume the paste
/// slowly (remote-desktop bridges, busy apps) need this headroom; it runs off
/// the injection critical path, so it does not delay injection completion.
#[cfg(target_os = "macos")]
const CLIPBOARD_RESTORE_DELAY_MS: u64 = 900;
#[cfg(not(target_os = "macos"))]
const CLIPBOARD_RESTORE_DELAY_MS: u64 = 500;

/// How long the paste target stays deactivated while focus bounces through
/// VoiceX before pasting. Remote-desktop clients on macOS only re-read the
/// Mac clipboard when they become active again, so the target has to see a
/// real deactivate → activate cycle before the new payload is announced.
#[cfg(target_os = "macos")]
const CLIPBOARD_FOCUS_ROUNDTRIP_AWAY_MS: u64 = 250;
/// How long to wait after reactivating the target before pasting: the target
/// regaining focus, its activation-time clipboard read and the remote-side
/// clipboard update all have to finish before the paste lands.
#[cfg(target_os = "macos")]
const CLIPBOARD_FOCUS_ROUNDTRIP_RETURN_MS: u64 = 600;

/// Outcome of trying to make VoiceX the active app during a focus
/// round-trip. See `focus_roundtrip_before_paste`.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
enum SelfActivationOutcome {
    /// VoiceX was already the active app.
    AlreadyActive,
    /// Activated without touching the activation policy.
    Activated,
    /// Activated after temporarily promoting VoiceX to a regular app; the
    /// original policy must be restored on the main thread afterwards.
    ActivatedWithPolicy(objc2_app_kit::NSApplicationActivationPolicy),
    /// Every activation attempt was denied or ignored.
    Failed,
}

/// Poll `cond` every 10ms until it holds or `timeout_ms` elapses.
#[cfg(target_os = "macos")]
fn wait_until(cond: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if cond() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Runtime macOS version check (e.g. `macos_at_least(14)`), used to avoid
/// calling AppKit APIs that only exist on newer systems.
#[cfg(target_os = "macos")]
fn macos_at_least(major: isize) -> bool {
    use objc2_foundation::NSProcessInfo;
    NSProcessInfo::processInfo().operatingSystemVersion().majorVersion >= major
}

/// Run `f` on the app's main thread and wait for its result. Returns `None`
/// when the main-thread dispatcher is unavailable or the main thread is
/// stuck for longer than `timeout`.
#[cfg(target_os = "macos")]
fn run_on_main_thread_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Option<T> {
    let app = super::MAIN_THREAD_APP.get()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = tx.send(f());
    })
    .ok()?;
    rx.recv_timeout(timeout).ok()
}

/// Must run on the main thread. Attempts to make VoiceX the active app.
///
/// A plain activation request comes first, but macOS 14+ denies
/// self-activation while another app is frontmost unless the requester is a
/// regular app — and VoiceX deliberately runs as an accessory app without
/// Dock presence. The escalation therefore briefly promotes VoiceX to a
/// regular app (the Dock icon flashes for the duration of the bounce),
/// activates it, and reports the original policy so the caller can restore
/// it once the round-trip finishes.
#[cfg(target_os = "macos")]
fn activate_self_on_main_thread() -> SelfActivationOutcome {
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationOptions, NSApplicationActivationPolicy,
        NSRunningApplication,
    };

    let current = NSRunningApplication::currentApplication();
    if current.isActive() {
        return SelfActivationOutcome::AlreadyActive;
    }

    if current.activateWithOptions(NSApplicationActivationOptions::empty())
        && wait_until(|| current.isActive(), 150)
    {
        return SelfActivationOutcome::Activated;
    }

    let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
    let ns_app = NSApplication::sharedApplication(mtm);
    let original_policy = ns_app.activationPolicy();
    let promoted = ns_app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // Deprecated in macOS 14 in favor of `activate()` (called below), but
    // still the right call on older systems where `activate()` is absent.
    #[allow(deprecated)]
    ns_app.activateIgnoringOtherApps(true);
    if macos_at_least(14) {
        ns_app.activate();
    }
    let activated = wait_until(|| current.isActive(), 300);
    log::info!(
        "Focus round-trip: promoted-to-Regular self-activation (policy_changed={}, active={})",
        promoted,
        activated
    );
    if activated {
        SelfActivationOutcome::ActivatedWithPolicy(original_policy)
    } else {
        // The bounce is not going to happen; undo the promotion right away.
        let _ = ns_app.setActivationPolicy(original_policy);
        SelfActivationOutcome::Failed
    }
}

/// Must run on the main thread. Reactivates the paste target so its
/// clipboard bridge re-reads the Mac pasteboard, then restores the
/// activation policy saved by `activate_self_on_main_thread`.
#[cfg(target_os = "macos")]
fn reactivate_target_on_main_thread(
    target_pid: u32,
    policy_to_restore: Option<objc2_app_kit::NSApplicationActivationPolicy>,
) -> bool {
    use objc2_app_kit::{NSApplication, NSApplicationActivationOptions, NSRunningApplication};

    let mut reactivated = false;
    if let Some(target) =
        NSRunningApplication::runningApplicationWithProcessIdentifier(target_pid as libc::pid_t)
    {
        let requested = target.activateWithOptions(NSApplicationActivationOptions::empty());
        reactivated = requested && wait_until(|| target.isActive(), 400);
        if !reactivated {
            log::warn!(
                "Focus round-trip: target reactivation fell short (requested={}, active={})",
                requested,
                target.isActive()
            );
        }
    } else {
        log::warn!(
            "Focus round-trip: target pid {} is no longer running",
            target_pid
        );
    }

    if let Some(policy) = policy_to_restore {
        let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
        let ns_app = NSApplication::sharedApplication(mtm);
        let _ = ns_app.setActivationPolicy(policy);
    }

    reactivated
}

#[cfg(target_os = "macos")]
fn text_chunks(s: &str, max_chars: usize) -> Vec<&str> {
    assert!(max_chars > 0);
    let mut chunks = Vec::new();
    let mut chunk_start = 0;
    let mut count = 0;

    for (byte_idx, ch) in s.char_indices() {
        count += 1;

        if count >= max_chars {
            let end = byte_idx + ch.len_utf8();
            chunks.push(&s[chunk_start..end]);
            chunk_start = end;
            count = 0;
        }
    }

    if chunk_start < s.len() {
        chunks.push(&s[chunk_start..]);
    }

    chunks
}

/// Text injection mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextInjectionMode {
    Pasteboard, // Clipboard paste (default, cross-platform)
    Typing,     // Simulated typing (best effort)
}

impl Default for TextInjectionMode {
    fn default() -> Self {
        Self::Pasteboard
    }
}

impl TextInjectionMode {
    pub fn from_str(value: &str) -> Self {
        match value {
            "typing" => TextInjectionMode::Typing,
            _ => TextInjectionMode::Pasteboard,
        }
    }
}

/// Backup of clipboard content before injection
/// Supports text and image formats (the two main formats arboard can handle)
enum ClipboardBackup {
    Text(String),
    Image {
        width: usize,
        height: usize,
        bytes: Vec<u8>,
    },
    None,
}

impl ClipboardBackup {
    /// Save current clipboard content (tries text first, then image)
    fn save(clipboard: &mut Clipboard) -> Self {
        // Try text first (most common case)
        if let Ok(text) = clipboard.get_text() {
            if !text.is_empty() {
                return ClipboardBackup::Text(text);
            }
        }

        // Try image
        if let Ok(img) = clipboard.get_image() {
            return ClipboardBackup::Image {
                width: img.width,
                height: img.height,
                bytes: img.bytes.into_owned(),
            };
        }

        ClipboardBackup::None
    }

    /// Restore clipboard content
    fn restore(self, clipboard: &mut Clipboard) {
        match self {
            ClipboardBackup::Text(text) => {
                if let Err(e) = clipboard.set_text(&text) {
                    log::warn!("Failed to restore clipboard text: {}", e);
                }
            }
            ClipboardBackup::Image {
                width,
                height,
                bytes,
            } => {
                let img_data = ImageData {
                    width,
                    height,
                    bytes: Cow::Owned(bytes),
                };
                if let Err(e) = clipboard.set_image(img_data) {
                    log::warn!("Failed to restore clipboard image: {}", e);
                }
            }
            ClipboardBackup::None => {
                // Nothing to restore; optionally clear clipboard
                let _ = clipboard.clear();
            }
        }
    }
}

/// Text injector for inserting recognized text into applications
pub struct TextInjector {
    mode: TextInjectionMode,
    pre_paste_delay_ms: u64,
    restore_delay_ms: u64,
    skip_clipboard_restore: bool,
    focus_roundtrip_pid: Option<u32>,
}

impl TextInjector {
    pub fn new() -> Self {
        Self {
            mode: TextInjectionMode::Pasteboard,
            pre_paste_delay_ms: CLIPBOARD_PRE_PASTE_DELAY_MS,
            restore_delay_ms: CLIPBOARD_RESTORE_DELAY_MS,
            skip_clipboard_restore: false,
            focus_roundtrip_pid: None,
        }
    }

    /// `skip_clipboard_restore` should be set for targets known to bridge the
    /// clipboard over a variable-latency channel (for example a per-app
    /// text-injection override configured for a remote-desktop client). A
    /// fixed restore delay races that bridge: if the delay fires before the
    /// remote side has actually read the fresh clipboard content, we hand it
    /// back whatever was there before instead, and the remote host pastes
    /// that stale value. Restoring the clipboard is not safe to do
    /// automatically for these targets, so we skip it entirely.
    pub fn with_mode(mode: TextInjectionMode, skip_clipboard_restore: bool) -> Self {
        Self {
            mode,
            pre_paste_delay_ms: CLIPBOARD_PRE_PASTE_DELAY_MS,
            restore_delay_ms: CLIPBOARD_RESTORE_DELAY_MS,
            skip_clipboard_restore,
            focus_roundtrip_pid: None,
        }
    }

    /// Make the pasteboard path bounce focus through VoiceX before pasting.
    ///
    /// Remote-desktop clients on macOS only push the Mac clipboard to the
    /// remote session when they become active again; while they stay
    /// frontmost — which they are during injection, since VoiceX never takes
    /// focus — a freshly written clipboard is never announced and the remote
    /// side keeps pasting stale content. Briefly activating VoiceX and then
    /// the target app (looked up by pid) forces that deactivate → activate
    /// cycle, so the new payload is announced before the synthetic paste
    /// lands. Only effective for the pasteboard path on macOS; elsewhere the
    /// configured pid is ignored.
    pub fn with_focus_roundtrip_target(mut self, pid: Option<u32>) -> Self {
        self.focus_roundtrip_pid = pid;
        self
    }

    /// Inject text using the configured mode
    pub fn inject(&self, text: &str) -> Result<(), InjectorError> {
        if text.is_empty() {
            return Ok(());
        }

        match self.mode {
            TextInjectionMode::Pasteboard => self.inject_via_pasteboard(text),
            TextInjectionMode::Typing => self.inject_via_typing(text),
        }
    }

    fn inject_via_pasteboard(&self, text: &str) -> Result<(), InjectorError> {
        let mut clipboard =
            Clipboard::new().map_err(|e| InjectorError::ClipboardError(e.to_string()))?;

        // 1. Save current clipboard content (text or image)
        let backup = ClipboardBackup::save(&mut clipboard);
        log::debug!(
            "Clipboard backup: {}",
            match &backup {
                ClipboardBackup::Text(t) => format!("text ({} chars)", t.len()),
                ClipboardBackup::Image { width, height, .. } =>
                    format!("image ({}x{})", width, height),
                ClipboardBackup::None => "none".to_string(),
            }
        );

        // 2. Set new text
        clipboard
            .set_text(text)
            .map_err(|e| InjectorError::ClipboardError(e.to_string()))?;
        self.verify_clipboard_text(&mut clipboard, text)?;

        // 3. Give the target a chance to observe the new clipboard content.
        // Clipboard-bridge targets get a focus round-trip, which forces the
        // announcement; plain targets just wait out the pre-paste delay.
        if !self.run_focus_roundtrip_if_configured() && self.pre_paste_delay_ms > 0 {
            thread::sleep(Duration::from_millis(self.pre_paste_delay_ms));
        }

        // 4. Send paste command
        self.send_paste_command()?;

        // 5. Schedule the clipboard restore off the injection critical path.
        // The restore itself still waits `restore_delay_ms` for the target to
        // consume the pasted payload, but blocking here would delay injection
        // completion (and with it InjectDone / the HUD) by that whole delay
        // on every pasteboard injection.
        if self.skip_clipboard_restore {
            log::debug!(
                "Skipping clipboard restore for this target (configured as a variable-latency clipboard bridge)"
            );
        } else {
            self.schedule_clipboard_restore(backup, text);
        }

        log::debug!("Injected {} characters via pasteboard", text.len());
        Ok(())
    }

    /// Perform the activate-self → reactivate-target focus bounce if a
    /// round-trip target is configured. Returns `true` when a round-trip ran
    /// (its return wait replaces the plain pre-paste delay).
    fn run_focus_roundtrip_if_configured(&self) -> bool {
        let Some(target_pid) = self.focus_roundtrip_pid else {
            return false;
        };

        #[cfg(target_os = "macos")]
        {
            self.focus_roundtrip_before_paste(target_pid);
            true
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = target_pid;
            log::debug!("Focus round-trip is macOS-only; ignoring the configured target");
            false
        }
    }

    /// Bounce focus through VoiceX so the target's clipboard bridge re-reads
    /// the Mac pasteboard: activate ourselves (the target resigns active),
    /// wait out the away window, reactivate the target by pid (it becomes
    /// active again and announces the clipboard), then wait for the
    /// announcement to reach the remote side before the caller pastes.
    ///
    /// Failures are logged and never abort the injection: a paste without
    /// the round-trip is no worse than the previous behavior.
    #[cfg(target_os = "macos")]
    fn focus_roundtrip_before_paste(&self, target_pid: u32) {
        log::info!(
            "Focus round-trip: bouncing activation through VoiceX (target pid {})",
            target_pid
        );

        let activation =
            run_on_main_thread_blocking(activate_self_on_main_thread, Duration::from_secs(2));
        let policy_to_restore = match activation {
            Some(SelfActivationOutcome::AlreadyActive) => None,
            Some(SelfActivationOutcome::Activated) => None,
            Some(SelfActivationOutcome::ActivatedWithPolicy(policy)) => Some(policy),
            Some(SelfActivationOutcome::Failed) | None => {
                log::warn!(
                    "Focus round-trip: self-activation denied; pasting without bridge sync"
                );
                if self.pre_paste_delay_ms > 0 {
                    thread::sleep(Duration::from_millis(self.pre_paste_delay_ms));
                }
                return;
            }
        };

        thread::sleep(Duration::from_millis(CLIPBOARD_FOCUS_ROUNDTRIP_AWAY_MS));

        match run_on_main_thread_blocking(
            move || reactivate_target_on_main_thread(target_pid, policy_to_restore),
            Duration::from_secs(2),
        ) {
            Some(true) => {}
            Some(false) => {
                log::warn!(
                    "Focus round-trip: target did not regain active state; pasting anyway"
                );
            }
            None => {
                log::warn!(
                    "Focus round-trip: target reactivation dispatch failed; pasting anyway"
                );
                // The reactivation hop owns the policy restore; run a
                // best-effort restore here since it never did.
                if let Some(policy) = policy_to_restore {
                    let _ = run_on_main_thread_blocking(
                        move || {
                            use objc2_app_kit::NSApplication;
                            let mtm = unsafe { objc2::MainThreadMarker::new_unchecked() };
                            let ns_app = NSApplication::sharedApplication(mtm);
                            let _ = ns_app.setActivationPolicy(policy);
                        },
                        Duration::from_secs(1),
                    );
                }
            }
        }

        thread::sleep(Duration::from_millis(CLIPBOARD_FOCUS_ROUNDTRIP_RETURN_MS));
    }

    fn verify_clipboard_text(
        &self,
        clipboard: &mut Clipboard,
        expected: &str,
    ) -> Result<(), InjectorError> {
        for attempt in 0..CLIPBOARD_WRITE_VERIFY_RETRIES {
            match clipboard.get_text() {
                Ok(current) if current == expected => return Ok(()),
                Ok(current) => {
                    log::debug!(
                        "Clipboard verification mismatch on attempt {} (expected {} chars, got {} chars)",
                        attempt + 1,
                        expected.chars().count(),
                        current.chars().count()
                    );
                }
                Err(err) => {
                    log::debug!(
                        "Clipboard verification read failed on attempt {}: {}",
                        attempt + 1,
                        err
                    );
                }
            }

            thread::sleep(Duration::from_millis(CLIPBOARD_WRITE_VERIFY_INTERVAL_MS));
        }

        Err(InjectorError::ClipboardError(
            "Clipboard write could not be verified before paste".to_string(),
        ))
    }

    /// Restore the clipboard on a detached thread after `restore_delay_ms`.
    ///
    /// The delay gives the paste target time to read the injected payload
    /// before we put the user's previous content back. The restore only fires
    /// if the clipboard still holds the injected text, so anything the user
    /// copied in the meantime wins. The thread re-acquires the global
    /// injection lock (see `inject_serialized`) before touching the clipboard
    /// so the restore cannot interleave with a newer injection's
    /// write → verify → paste sequence.
    fn schedule_clipboard_restore(&self, backup: ClipboardBackup, injected_text: &str) {
        let delay = Duration::from_millis(self.restore_delay_ms);
        let injected_text = injected_text.to_string();
        thread::spawn(move || {
            thread::sleep(delay);
            let _guard = super::INJECTION_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let mut clipboard = match Clipboard::new() {
                Ok(clipboard) => clipboard,
                Err(err) => {
                    log::warn!("Failed to open clipboard for delayed restore: {}", err);
                    return;
                }
            };
            Self::restore_clipboard_if_unchanged(&mut clipboard, backup, &injected_text);
        });
    }

    fn restore_clipboard_if_unchanged(
        clipboard: &mut Clipboard,
        backup: ClipboardBackup,
        injected_text: &str,
    ) {
        match clipboard.get_text() {
            Ok(current) if current == injected_text => backup.restore(clipboard),
            Ok(current) => {
                log::info!(
                    "Skipping clipboard restore because clipboard changed after injection ({} chars)",
                    current.chars().count()
                );
            }
            Err(err) => {
                log::info!(
                    "Skipping clipboard restore because clipboard is no longer readable after injection: {}",
                    err
                );
            }
        }
    }

    fn inject_via_typing(&self, text: &str) -> Result<(), InjectorError> {
        // For long text, fall back to clipboard as typing is too slow
        let char_count = text.chars().count();
        if char_count > TYPING_MODE_MAX_CHARS {
            log::info!(
                "Text too long ({} chars > {} limit); using clipboard mode for speed",
                char_count,
                TYPING_MODE_MAX_CHARS
            );
            return self.inject_via_pasteboard(text);
        }

        #[cfg(target_os = "macos")]
        {
            if text.contains('\n') || text.contains('\r') {
                log::info!(
                    "Typing mode on macOS falls back to pasteboard for multiline text to avoid IME/newline injection bugs"
                );
                return self.inject_via_pasteboard(text);
            }

            // On macOS, enigo ultimately uses CGEventKeyboardSetUnicodeString,
            // which truncates each posted string to 20 Unicode scalars.
            // For now, multiline text uses pasteboard mode above; single-line text
            // stays on typing mode and is chunked to respect that limit.
            const CHUNK_SIZE: usize = 20;

            let chunks = text_chunks(text, CHUNK_SIZE);
            let mut enigo = Enigo::new(&Settings::default()).map_err(|e| {
                InjectorError::TypingError(format!("Failed to create Enigo: {}", e))
            })?;
            log::debug!(
                "Injecting {} characters via simulated typing on macOS ({} chunks of ≤{})",
                char_count,
                chunks.len(),
                CHUNK_SIZE
            );

            for chunk in chunks {
                enigo.text(chunk).map_err(|e| {
                    InjectorError::TypingError(format!("Failed to type text chunk: {}", e))
                })?;
            }

            Ok(())
        }

        #[cfg(target_os = "windows")]
        {
            let mut enigo = Enigo::new(&Settings::default()).map_err(|e| {
                InjectorError::TypingError(format!("Failed to create Enigo: {}", e))
            })?;
            enigo
                .text(text)
                .map_err(|e| InjectorError::TypingError(format!("Failed to type text: {}", e)))?;
            log::debug!(
                "Injected {} characters via simulated typing on Windows",
                char_count
            );
            Ok(())
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            log::warn!("Typing mode not supported on this platform; falling back to pasteboard");
            self.inject_via_pasteboard(text)
        }
    }

    fn send_paste_command(&self) -> Result<(), InjectorError> {
        #[cfg(target_os = "macos")]
        {
            return self.send_paste_command_macos();
        }

        #[cfg(target_os = "windows")]
        {
            let mut enigo = Enigo::new(&Settings::default()).map_err(|e| {
                InjectorError::PasteCommandFailed(format!("Failed to create Enigo: {e}"))
            })?;
            enigo
                .key(EnigoKey::Control, Direction::Press)
                .map_err(|e| InjectorError::PasteCommandFailed(format!("{e}")))?;
            enigo
                .key(EnigoKey::V, Direction::Click)
                .map_err(|e| InjectorError::PasteCommandFailed(format!("{e}")))?;
            enigo
                .key(EnigoKey::Control, Direction::Release)
                .map_err(|e| InjectorError::PasteCommandFailed(format!("{e}")))?;
            return Ok(());
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let modifiers = [Key::ControlLeft];
            let sequence = modifiers
                .iter()
                .map(|m| EventType::KeyPress(*m))
                .chain(std::iter::once(EventType::KeyPress(Key::KeyV)))
                .chain(std::iter::once(EventType::KeyRelease(Key::KeyV)))
                .chain(modifiers.iter().rev().map(|m| EventType::KeyRelease(*m)));

            for evt in sequence {
                self.simulate_key(evt)?;
            }

            Ok(())
        }
    }

    /// Posts a synthetic Command+V as a raw CGEvent sourced from
    /// `HIDSystemState` rather than going through Enigo.
    ///
    /// Remote-desktop clients (confirmed against Microsoft's "Windows App")
    /// only bridge the Mac clipboard to the remote session for Command+V
    /// events sourced from `HIDSystemState` — the state real hardware key
    /// events share. Enigo only ever creates event sources with `Private` or
    /// `CombinedSessionState` (see its `Settings::independent_of_keyboard_state`),
    /// so a paste built on Enigo triggers the remote paste action but never
    /// the clipboard sync, and the remote side ends up pasting whatever was
    /// already in its clipboard. The flag construction below otherwise
    /// mirrors Enigo's own macOS backend so the paste behaves identically
    /// everywhere else.
    #[cfg(target_os = "macos")]
    fn send_paste_command_macos(&self) -> Result<(), InjectorError> {
        const NX_DEVICELCMDKEYMASK: CGEventFlags = CGEventFlags::from_bits_retain(0x0000_0008);
        let base_flags = {
            let mut flags = CGEventFlags::CGEventFlagNonCoalesced;
            flags.set(CGEventFlags::from_bits_retain(0x2000_0000), true);
            flags
        };
        let command_held_flags =
            base_flags | CGEventFlags::CGEventFlagCommand | NX_DEVICELCMDKEYMASK;

        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| {
            InjectorError::PasteCommandFailed("CGEventSource::new failed".to_string())
        })?;

        let post_key =
            |keycode: u16, keydown: bool, flags: CGEventFlags| -> Result<(), InjectorError> {
                let event = CGEvent::new_keyboard_event(source.clone(), keycode, keydown).map_err(
                    |_| {
                        InjectorError::PasteCommandFailed(
                            "CGEvent::new_keyboard_event failed".to_string(),
                        )
                    },
                )?;
                event.set_integer_value_field(
                    EventField::EVENT_SOURCE_USER_DATA,
                    i64::from(enigo::EVENT_MARKER),
                );
                event.set_flags(flags);
                event.post(CGEventTapLocation::HID);
                Ok(())
            };

        post_key(MACOS_COMMAND_KEYCODE, true, command_held_flags)?;
        post_key(MACOS_V_KEYCODE, true, command_held_flags)?;
        post_key(MACOS_V_KEYCODE, false, command_held_flags)?;
        post_key(MACOS_COMMAND_KEYCODE, false, base_flags)?;

        Ok(())
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn simulate_key(&self, evt: EventType) -> Result<(), InjectorError> {
        simulate(&evt).map_err(|e| InjectorError::PasteCommandFailed(format!("{e}")))?;
        // Tiny delay to preserve key ordering for some hosts.
        thread::sleep(Duration::from_millis(5));
        Ok(())
    }
}

impl Default for TextInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InjectorError {
    #[error("Clipboard error: {0}")]
    ClipboardError(String),

    #[error("Failed to send paste command: {0}")]
    PasteCommandFailed(String),

    #[error("Typing error: {0}")]
    TypingError(String),

    #[error("Platform not supported")]
    UnsupportedPlatform,
}
