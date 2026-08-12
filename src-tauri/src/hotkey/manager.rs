//! Hotkey manager - records and listens for global hotkeys using rdev.

use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use rdev::{Event, EventType, Key};
use tauri::Emitter;

use super::config::HotkeyConfiguration;
use crate::session::{SessionCoordinator, SessionMessage};
use crate::tts::TtsController;

#[derive(Debug)]
enum HookEvent {
    RecordComplete(HotkeyConfiguration),
    Pressed(HotkeyConfiguration),
    Released(HotkeyConfiguration),
    EscapePressed,
    /// Selected-text reading hotkey. Phase 0 binds this to a hardcoded
    /// combination; phase 2 generalises the hook into an action map with
    /// conflict detection. There is still exactly one system-level listener.
    ReadSelectionPressed,
    /// Escape while reading aloud. Kept separate from `EscapePressed` so the
    /// dictation cancel path is untouched.
    ReadSelectionEscape,
    /// The dictation hotkey went down; reading must yield to it.
    DictationTakesOver,
}

#[derive(Clone)]
pub struct HotkeyManager {
    config: Arc<Mutex<Option<HotkeyConfiguration>>>,
    active_key_code: Arc<AtomicU32>,
    active_modifiers: Arc<AtomicU32>,
    active_uses_fn: Arc<AtomicBool>,
    active_enabled: Arc<AtomicBool>,
    read_selection_config: Arc<Mutex<Option<HotkeyConfiguration>>>,
    read_selection_key_code: Arc<AtomicU32>,
    read_selection_modifiers: Arc<AtomicU32>,
    read_selection_uses_fn: Arc<AtomicBool>,
    read_selection_enabled: Arc<AtomicBool>,
    suspension_count: Arc<AtomicU32>,
    listener_started: Arc<AtomicBool>,
    recording_sender: Arc<Mutex<Option<Sender<HotkeyConfiguration>>>>,
    swallow_escape: Arc<AtomicBool>,
    /// Whether we are actively recording a hotkey combination
    recording_active: Arc<AtomicBool>,
    /// Accumulated hotkey configuration during recording
    recording_accumulated_config: Arc<Mutex<Option<HotkeyConfiguration>>>,
}

impl HotkeyManager {
    pub fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            active_key_code: Arc::new(AtomicU32::new(0)),
            active_modifiers: Arc::new(AtomicU32::new(0)),
            active_uses_fn: Arc::new(AtomicBool::new(false)),
            active_enabled: Arc::new(AtomicBool::new(false)),
            read_selection_config: Arc::new(Mutex::new(None)),
            read_selection_key_code: Arc::new(AtomicU32::new(0)),
            read_selection_modifiers: Arc::new(AtomicU32::new(0)),
            read_selection_uses_fn: Arc::new(AtomicBool::new(false)),
            read_selection_enabled: Arc::new(AtomicBool::new(false)),
            suspension_count: Arc::new(AtomicU32::new(0)),
            listener_started: Arc::new(AtomicBool::new(false)),
            recording_sender: Arc::new(Mutex::new(None)),
            swallow_escape: Arc::new(AtomicBool::new(false)),
            recording_active: Arc::new(AtomicBool::new(false)),
            recording_accumulated_config: Arc::new(Mutex::new(None)),
        }
    }

    /// Start the global listener once for the app lifetime.
    ///
    /// All hotkey actions share this one listener; adding an action must never
    /// mean adding a second system-level keyboard hook.
    pub fn start_listener(
        &self,
        app: tauri::AppHandle,
        session: Option<SessionCoordinator>,
        tts: Option<TtsController>,
    ) {
        if self.listener_started.swap(true, Ordering::SeqCst) {
            return;
        }

        let active_key_code = self.active_key_code.clone();
        let active_modifiers = self.active_modifiers.clone();
        let active_uses_fn = self.active_uses_fn.clone();
        let active_enabled = self.active_enabled.clone();
        let read_selection_key_code = self.read_selection_key_code.clone();
        let read_selection_modifiers = self.read_selection_modifiers.clone();
        let read_selection_uses_fn = self.read_selection_uses_fn.clone();
        let read_selection_enabled = self.read_selection_enabled.clone();
        let suspension_count = self.suspension_count.clone();
        let recording_sender = self.recording_sender.clone();
        let swallow_escape = self.swallow_escape.clone();
        let session_handler = session.clone();
        let recording_active = self.recording_active.clone();
        let recording_accumulated_config = self.recording_accumulated_config.clone();
        let tts_active = tts.as_ref().map(|controller| controller.active_handle());

        thread::spawn(move || {
            let modifier_state = RefCell::new(ModifierState::default());
            let last_key_for_config: RefCell<Option<Key>> = RefCell::new(None);
            let last_active_config: RefCell<Option<HotkeyConfiguration>> = RefCell::new(None);
            let active_hotkey_pressed = RefCell::new(false);
            let read_selection_key: RefCell<Option<Key>> = RefCell::new(None);
            let (hook_tx, hook_rx) = mpsc::channel::<HookEvent>();

            // Worker thread to process hotkey actions off the hook callback.
            let worker_app = app.clone();
            let worker_session = session_handler.clone();
            let worker_recording = recording_sender.clone();
            let worker_tts = tts.clone();
            thread::spawn(move || {
                while let Ok(event) = hook_rx.recv() {
                    match event {
                        HookEvent::RecordComplete(cfg) => {
                            if let Ok(mut guard) = worker_recording.lock() {
                                if let Some(sender) = guard.take() {
                                    let _ = sender.send(cfg);
                                }
                            }
                        }
                        HookEvent::Pressed(cfg) => {
                            if let Some(handler) = worker_session.as_ref() {
                                handler.send(SessionMessage::HotkeyPressed);
                            }
                            let _ = worker_app.emit("hotkey:pressed", cfg.display_string());
                        }
                        HookEvent::Released(cfg) => {
                            if let Some(handler) = worker_session.as_ref() {
                                handler.send(SessionMessage::HotkeyReleased);
                            }
                            let _ = worker_app.emit("hotkey:released", cfg.display_string());
                        }
                        HookEvent::EscapePressed => {
                            if let Some(handler) = worker_session.as_ref() {
                                handler.send(SessionMessage::CancelSession(
                                    crate::session::CancelReason::EscapeKey,
                                ));
                            }
                        }
                        HookEvent::ReadSelectionPressed => {
                            if let Some(controller) = worker_tts.as_ref() {
                                controller.handle_read_selection_hotkey();
                            }
                        }
                        HookEvent::ReadSelectionEscape => {
                            if let Some(controller) = worker_tts.as_ref() {
                                controller.stop(crate::tts::StopReason::Escape);
                            }
                        }
                        HookEvent::DictationTakesOver => {
                            if let Some(controller) = worker_tts.as_ref() {
                                controller.stop_for_dictation();
                            }
                        }
                    }
                }
            });

            // Use grab so we can optionally swallow the active hotkey from the system (e.g., IME).
            let callback = move |event: Event| -> Option<Event> {
                let mut suppress = false;
                match event.event_type {
                    EventType::KeyPress(key) => {
                        let mut mods = modifier_state.borrow_mut();
                        mods.on_press(key);
                        if key == Key::Escape {
                            let _ = hook_tx.send(HookEvent::EscapePressed);
                            // Swallow ESC only when hotkey handling is active (not during recording suspension).
                            if active_enabled.load(Ordering::SeqCst)
                                && suspension_count.load(Ordering::SeqCst) == 0
                                && swallow_escape.load(Ordering::SeqCst)
                            {
                                suppress = true;
                            }

                            // Escape also cancels reading, but only while
                            // reading is actually happening (plan §3.3) —
                            // otherwise Escape must reach the foreground app
                            // untouched. Read lock-free: taking a lock inside
                            // the event tap callback risks the tap timing out.
                            if tts_active
                                .as_ref()
                                .is_some_and(|flag| flag.load(Ordering::SeqCst) != 0)
                                && suspension_count.load(Ordering::SeqCst) == 0
                            {
                                let _ = hook_tx.send(HookEvent::ReadSelectionEscape);
                                suppress = true;
                            }
                        }
                        if let Some(snapshot) = HotkeySnapshot::from_event(key, &mods) {
                            let cfg = snapshot.to_config();

                            // Recording mode: accumulate the configuration instead of sending immediately
                            if recording_active.load(Ordering::SeqCst) {
                                if let Ok(mut guard) = recording_accumulated_config.lock() {
                                    *guard = Some(cfg.clone());
                                }
                            }

                            let enabled = active_enabled.load(Ordering::SeqCst);
                            let active_match = snapshot.matches_active(
                                active_key_code.load(Ordering::SeqCst),
                                active_modifiers.load(Ordering::SeqCst),
                                active_uses_fn.load(Ordering::SeqCst),
                            );

                            let read_selection_match = snapshot.matches_active(
                                read_selection_key_code.load(Ordering::SeqCst),
                                read_selection_modifiers.load(Ordering::SeqCst),
                                read_selection_uses_fn.load(Ordering::SeqCst),
                            );

                            if enabled
                                && active_match
                                && suspension_count.load(Ordering::SeqCst) == 0
                            {
                                *last_key_for_config.borrow_mut() = Some(key);
                                *last_active_config.borrow_mut() = Some(cfg.clone());
                                if !*active_hotkey_pressed.borrow() {
                                    *active_hotkey_pressed.borrow_mut() = true;
                                    // Dictation takes priority over reading:
                                    // speech playing into a live microphone
                                    // gets transcribed back (plan §3.3).
                                    if tts_active
                                        .as_ref()
                                        .is_some_and(|flag| flag.load(Ordering::SeqCst) != 0)
                                    {
                                        let _ = hook_tx.send(HookEvent::DictationTakesOver);
                                    }
                                    let _ = hook_tx.send(HookEvent::Pressed(cfg));
                                }
                                suppress = true;
                            } else if read_selection_enabled.load(Ordering::SeqCst)
                                && read_selection_match
                                && suspension_count.load(Ordering::SeqCst) == 0
                            {
                                // Fire once per physical press; key repeat while
                                // held must not re-trigger the action.
                                if read_selection_key.borrow().is_none() {
                                    *read_selection_key.borrow_mut() = Some(key);
                                    let _ = hook_tx.send(HookEvent::ReadSelectionPressed);
                                }
                                suppress = true;
                            }
                        }
                    }
                    EventType::KeyRelease(key) => {
                        modifier_state.borrow_mut().on_release(key);

                        // Recording mode: send accumulated config on key release
                        if recording_active.load(Ordering::SeqCst) {
                            if let Ok(mut guard) = recording_accumulated_config.lock() {
                                if let Some(cfg) = guard.take() {
                                    let _ = hook_tx.send(HookEvent::RecordComplete(cfg));
                                }
                            }
                        }

                        let active_key_opt = *last_key_for_config.borrow();
                        if let Some(active_key) = active_key_opt {
                            if key == active_key {
                                let cfg_opt = last_active_config.borrow().as_ref().cloned();
                                let was_pressed = *active_hotkey_pressed.borrow();
                                *active_hotkey_pressed.borrow_mut() = false;
                                if suspension_count.load(Ordering::SeqCst) == 0 {
                                    if was_pressed {
                                        if let Some(cfg) = cfg_opt {
                                            let _ = hook_tx.send(HookEvent::Released(cfg));
                                        }
                                    }
                                }
                                *last_key_for_config.borrow_mut() = None;
                                *last_active_config.borrow_mut() = None;
                                suppress = true;
                            }
                        }

                        // Swallow the matching release so the target app never
                        // sees a stray key-up for a hotkey whose press we ate.
                        // Cleared regardless of suspension to avoid a stuck latch.
                        let read_selection_key_opt = *read_selection_key.borrow();
                        if let Some(pressed_key) = read_selection_key_opt {
                            if key == pressed_key {
                                *read_selection_key.borrow_mut() = None;
                                suppress = true;
                            }
                        }
                    }
                    _ => {}
                }
                if suppress {
                    None
                } else {
                    Some(event)
                }
            };

            if let Err(err) = rdev::grab(callback) {
                log::error!("Global hotkey listener failed: {:?}", err);
            }
        });
    }

    /// Update active hotkey configuration used for recognition.
    pub fn set_config(&self, config: Option<HotkeyConfiguration>) {
        if let Ok(mut guard) = self.config.lock() {
            *guard = config.clone();
        }

        if let Some(cfg) = config {
            self.active_key_code.store(cfg.key_code, Ordering::SeqCst);
            self.active_modifiers
                .store(cfg.modifiers_bits(), Ordering::SeqCst);
            self.active_uses_fn.store(cfg.uses_fn, Ordering::SeqCst);
            self.active_enabled.store(true, Ordering::SeqCst);
        } else {
            self.active_enabled.store(false, Ordering::SeqCst);
        }

        // The dictation key can change at runtime, so re-evaluate the reading
        // binding against it rather than only at registration time.
        self.refresh_read_selection_binding();
    }

    /// Get current configuration.
    pub fn current_config(&self) -> Option<HotkeyConfiguration> {
        self.config.lock().ok().and_then(|c| c.clone())
    }

    /// Set the selected-text reading binding.
    ///
    /// Phase 0 only ever receives the hardcoded default; persistence and a
    /// proper settings-time conflict UI arrive in phase 2.
    pub fn set_read_selection_config(&self, config: Option<HotkeyConfiguration>) {
        if let Ok(mut guard) = self.read_selection_config.lock() {
            *guard = config;
        }
        self.refresh_read_selection_binding();
    }

    /// Re-apply the reading binding, disabling it when it collides with the
    /// dictation key.
    ///
    /// The dictation branch is checked first in the hook, so an identical
    /// binding would make reading unreachable with no sign of why. Refusing the
    /// binding loudly beats swallowing the key and doing nothing.
    fn refresh_read_selection_binding(&self) {
        let Some(cfg) = self
            .read_selection_config
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        else {
            self.read_selection_enabled.store(false, Ordering::SeqCst);
            return;
        };

        if self.conflicts_with_dictation(&cfg) {
            log::warn!(
                "Selected-text reading hotkey ({}) is also the dictation hotkey; \
                 reading is disabled until one of them changes",
                cfg.display_string()
            );
            self.read_selection_enabled.store(false, Ordering::SeqCst);
            return;
        }

        self.read_selection_key_code
            .store(cfg.key_code, Ordering::SeqCst);
        self.read_selection_modifiers
            .store(cfg.modifiers_bits(), Ordering::SeqCst);
        self.read_selection_uses_fn
            .store(cfg.uses_fn, Ordering::SeqCst);
        self.read_selection_enabled.store(true, Ordering::SeqCst);
    }

    fn conflicts_with_dictation(&self, cfg: &HotkeyConfiguration) -> bool {
        if !self.active_enabled.load(Ordering::SeqCst) {
            return false;
        }
        cfg.key_code == self.active_key_code.load(Ordering::SeqCst)
            && cfg.modifiers_bits() == self.active_modifiers.load(Ordering::SeqCst)
            && cfg.uses_fn == self.active_uses_fn.load(Ordering::SeqCst)
    }

    /// Control whether ESC should be swallowed by the global hook.
    pub fn set_escape_swallowing(&self, enabled: bool) {
        self.swallow_escape.store(enabled, Ordering::SeqCst);
    }

    /// Suspend hotkey triggers (e.g., during hotkey recording)
    pub fn begin_suspension(&self) {
        self.suspension_count.fetch_add(1, Ordering::SeqCst);
        log::debug!("Hotkey suspension started");
    }

    /// Resume hotkey triggers
    pub fn end_suspension(&self) {
        let prev = self.suspension_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 0 {
            self.suspension_count.store(0, Ordering::SeqCst);
        }
        log::debug!("Hotkey suspension ended");
    }

    /// Capture the next key combination globally (with timeout).
    /// Uses accumulative recording: waits for key release to capture the full combination.
    pub fn record_once(&self, timeout_ms: u64) -> Result<HotkeyConfiguration, HotkeyError> {
        let (tx, rx): (Sender<HotkeyConfiguration>, Receiver<HotkeyConfiguration>) =
            mpsc::channel();
        if let Ok(mut guard) = self.recording_sender.lock() {
            *guard = Some(tx);
        }
        // Clear any previously accumulated config
        if let Ok(mut guard) = self.recording_accumulated_config.lock() {
            *guard = None;
        }
        self.begin_suspension();
        // Enable recording mode
        self.recording_active.store(true, Ordering::SeqCst);
        let result = rx
            .recv_timeout(Duration::from_millis(timeout_ms))
            .map_err(|_| HotkeyError::Timeout);
        // Disable recording mode
        self.recording_active.store(false, Ordering::SeqCst);
        self.end_suspension();
        result
    }
}

impl Default for HotkeyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("Failed to register hotkey: {0}")]
    RegistrationFailed(String),

    #[error("Hotkey conflict with another application")]
    Conflict,

    #[error("Permission denied - accessibility access required")]
    PermissionDenied,

    #[error("Timed out waiting for hotkey input")]
    Timeout,
}

#[derive(Default)]
struct ModifierState {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    fn_key: bool,
}

impl ModifierState {
    fn on_press(&mut self, key: Key) {
        match key {
            Key::ControlLeft | Key::ControlRight => self.ctrl = true,
            Key::Alt | Key::AltGr => self.alt = true,
            Key::ShiftLeft | Key::ShiftRight => self.shift = true,
            Key::MetaLeft | Key::MetaRight => self.meta = true,
            Key::Function => self.fn_key = true,
            _ => {}
        }
    }

    fn on_release(&mut self, key: Key) {
        match key {
            Key::ControlLeft | Key::ControlRight => self.ctrl = false,
            Key::Alt | Key::AltGr => self.alt = false,
            Key::ShiftLeft | Key::ShiftRight => self.shift = false,
            Key::MetaLeft | Key::MetaRight => self.meta = false,
            Key::Function => self.fn_key = false,
            _ => {}
        }
    }

    fn modifiers_bits(&self) -> u32 {
        let mut bits = 0;
        if self.ctrl {
            bits |= 0x1000;
        }
        if self.alt {
            bits |= 0x0800;
        }
        if self.shift {
            bits |= 0x0200;
        }
        if self.meta {
            bits |= 0x0100;
        }
        bits
    }
}

#[derive(Clone, Debug)]
struct HotkeySnapshot {
    key: Key,
    modifiers: u32,
    uses_fn: bool,
}

impl HotkeySnapshot {
    fn from_event(key: Key, mods: &ModifierState) -> Option<Self> {
        let is_modifier = matches!(
            key,
            Key::ControlLeft
                | Key::ControlRight
                | Key::Alt
                | Key::AltGr
                | Key::ShiftLeft
                | Key::ShiftRight
                | Key::MetaLeft
                | Key::MetaRight
                | Key::Function
        );

        // For standard combos, only capture when a non-modifier key is pressed.
        if is_modifier
            && !HotkeyConfiguration::is_modifier_only_key_code(key_code_from_key(key))
            && key != Key::Function
        {
            return None;
        }

        let mut snapshot = Self {
            key,
            modifiers: mods.modifiers_bits(),
            uses_fn: mods.fn_key || key == Key::Function,
        };

        // If the key itself is a modifier, drop the matching modifier flag to avoid duplicate labels.
        match key {
            Key::ShiftLeft | Key::ShiftRight => snapshot.modifiers &= !0x0200,
            Key::ControlLeft | Key::ControlRight => snapshot.modifiers &= !0x1000,
            Key::Alt | Key::AltGr => snapshot.modifiers &= !0x0800,
            Key::MetaLeft | Key::MetaRight => snapshot.modifiers &= !0x0100,
            _ => {}
        }

        Some(snapshot)
    }

    fn to_config(&self) -> HotkeyConfiguration {
        HotkeyConfiguration::with_uses_fn(key_code_from_key(self.key), self.modifiers, self.uses_fn)
    }

    fn matches_active(&self, key_code: u32, modifiers: u32, uses_fn: bool) -> bool {
        key_code_from_key(self.key) == key_code
            && self.modifiers == modifiers
            && self.uses_fn == uses_fn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_selection_enabled(manager: &HotkeyManager) -> bool {
        manager.read_selection_enabled.load(Ordering::SeqCst)
    }

    #[test]
    fn distinct_bindings_both_stay_enabled() {
        let manager = HotkeyManager::new();
        manager.set_config(Some(HotkeyConfiguration::default_primary()));
        manager.set_read_selection_config(Some(HotkeyConfiguration::default_read_selection()));

        assert!(read_selection_enabled(&manager));
        assert_eq!(
            manager.read_selection_key_code.load(Ordering::SeqCst),
            'R' as u32
        );
    }

    #[test]
    fn a_binding_identical_to_dictation_is_refused() {
        let manager = HotkeyManager::new();
        let shared = HotkeyConfiguration::default_read_selection();
        manager.set_config(Some(shared.clone()));
        manager.set_read_selection_config(Some(shared));

        // The dictation branch wins in the hook, so leaving this enabled would
        // swallow the key and silently do nothing.
        assert!(!read_selection_enabled(&manager));
    }

    #[test]
    fn changing_dictation_onto_the_reading_key_disables_reading() {
        let manager = HotkeyManager::new();
        manager.set_config(Some(HotkeyConfiguration::default_primary()));
        manager.set_read_selection_config(Some(HotkeyConfiguration::default_read_selection()));
        assert!(read_selection_enabled(&manager));

        manager.set_config(Some(HotkeyConfiguration::default_read_selection()));
        assert!(!read_selection_enabled(&manager));
    }

    #[test]
    fn moving_dictation_away_re_enables_reading() {
        let manager = HotkeyManager::new();
        manager.set_config(Some(HotkeyConfiguration::default_read_selection()));
        manager.set_read_selection_config(Some(HotkeyConfiguration::default_read_selection()));
        assert!(!read_selection_enabled(&manager));

        manager.set_config(Some(HotkeyConfiguration::default_primary()));
        assert!(read_selection_enabled(&manager));
    }

    #[test]
    fn clearing_the_reading_binding_disables_it() {
        let manager = HotkeyManager::new();
        manager.set_config(Some(HotkeyConfiguration::default_primary()));
        manager.set_read_selection_config(Some(HotkeyConfiguration::default_read_selection()));
        assert!(read_selection_enabled(&manager));

        manager.set_read_selection_config(None);
        assert!(!read_selection_enabled(&manager));
    }
}

fn key_code_from_key(key: Key) -> u32 {
    match key {
        Key::Space => 49,
        Key::Return | Key::KpReturn => 36,
        Key::Tab => 48,
        Key::Escape => 53,
        Key::Backspace => 51,
        Key::ShiftRight => 60,
        Key::ShiftLeft => 56,
        Key::MetaRight => 54,
        Key::MetaLeft => 55,
        Key::Alt => 58,   // Left Alt/Option
        Key::AltGr => 61, // Right Alt
        Key::ControlLeft => 59,
        Key::ControlRight => 62,
        Key::Function => 63,
        Key::KeyA => 'A' as u32,
        Key::KeyB => 'B' as u32,
        Key::KeyC => 'C' as u32,
        Key::KeyD => 'D' as u32,
        Key::KeyE => 'E' as u32,
        Key::KeyF => 'F' as u32,
        Key::KeyG => 'G' as u32,
        Key::KeyH => 'H' as u32,
        Key::KeyI => 'I' as u32,
        Key::KeyJ => 'J' as u32,
        Key::KeyK => 'K' as u32,
        Key::KeyL => 'L' as u32,
        Key::KeyM => 'M' as u32,
        Key::KeyN => 'N' as u32,
        Key::KeyO => 'O' as u32,
        Key::KeyP => 'P' as u32,
        Key::KeyQ => 'Q' as u32,
        Key::KeyR => 'R' as u32,
        Key::KeyS => 'S' as u32,
        Key::KeyT => 'T' as u32,
        Key::KeyU => 'U' as u32,
        Key::KeyV => 'V' as u32,
        Key::KeyW => 'W' as u32,
        Key::KeyX => 'X' as u32,
        Key::KeyY => 'Y' as u32,
        Key::KeyZ => 'Z' as u32,
        Key::Num0 | Key::Kp0 => '0' as u32,
        Key::Num1 | Key::Kp1 => '1' as u32,
        Key::Num2 | Key::Kp2 => '2' as u32,
        Key::Num3 | Key::Kp3 => '3' as u32,
        Key::Num4 | Key::Kp4 => '4' as u32,
        Key::Num5 | Key::Kp5 => '5' as u32,
        Key::Num6 | Key::Kp6 => '6' as u32,
        Key::Num7 | Key::Kp7 => '7' as u32,
        Key::Num8 | Key::Kp8 => '8' as u32,
        Key::Num9 | Key::Kp9 => '9' as u32,
        // Fallback to hash
        Key::Unknown(code) => code,
        _ => 0,
    }
}
