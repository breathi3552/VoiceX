//! Streaming audio output for speech backends that return audio bytes.
//!
//! The macOS system voice speaks through the OS and never hands us samples, so
//! nothing here is involved in that path. Cloud backends do produce bytes, and
//! plan §5.4 measured why they must be played *while* they arrive: a 222-character
//! selection takes 5 s to synthesize in full but only 621 ms to start. Buffering
//! the whole response first would put those five seconds of silence in front of
//! every long read.
//!
//! [`Playback`] owns a `cpal` output stream, which is `!Send`, so one synthesis
//! creates it, feeds it, and drops it all on the same thread. Other threads stop
//! it through the `Send` [`PlaybackHandle`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};

use super::CancelToken;

/// Sample rates the cloud providers can render at, best first.
///
/// We ask the provider for a rate the output device already runs at, which
/// keeps a resampler out of the playback path entirely — one less stage to get
/// subtly wrong. Every provider in plan §5.4 offers all of these.
const NEGOTIABLE_RATES: [u32; 5] = [48_000, 44_100, 24_000, 22_050, 16_000];

/// How long the device may go without consuming anything while samples are
/// waiting before we call the stream wedged. Generous: a healthy stream
/// consumes every few milliseconds.
const STALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Grace period after the last sample reaches the device, covering the buffer
/// the driver still has in hand. Without it the stream would be dropped
/// mid-word on short utterances.
const DRAIN_TAIL: Duration = Duration::from_millis(180);

const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlaybackError {
    #[error("No audio output device is available")]
    NoDevice,

    #[error("The output device supports none of the sample rates we can request")]
    NoUsableSampleRate,

    #[error("Unsupported output sample format: {0}")]
    UnsupportedFormat(String),

    #[error("Audio output failed: {0}")]
    Device(String),

    #[error("The audio device stopped consuming samples")]
    Stalled,
}

impl PlaybackError {
    pub fn code(&self) -> &'static str {
        match self {
            PlaybackError::NoDevice => "no_output_device",
            PlaybackError::NoUsableSampleRate => "no_usable_sample_rate",
            PlaybackError::UnsupportedFormat(_) => "unsupported_output_format",
            PlaybackError::Device(_) => "output_device_error",
            PlaybackError::Stalled => "output_stalled",
        }
    }
}

/// Pick the sample rate to request from the provider.
///
/// Prefers whatever the device already runs at so nothing has to be resampled.
/// Fails loudly rather than picking a rate the device cannot play — a silent
/// mismatch would come out as chipmunk audio, which is far harder to diagnose
/// than an error code.
pub fn negotiate_sample_rate() -> Result<u32, PlaybackError> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(PlaybackError::NoDevice)?;

    if let Ok(default) = device.default_output_config() {
        let rate = default.sample_rate().0;
        if NEGOTIABLE_RATES.contains(&rate) {
            return Ok(rate);
        }
    }

    let supported: Vec<_> = device
        .supported_output_configs()
        .map_err(|err| PlaybackError::Device(err.to_string()))?
        .collect();

    NEGOTIABLE_RATES
        .iter()
        .copied()
        .find(|rate| {
            supported.iter().any(|config| {
                config.min_sample_rate().0 <= *rate && *rate <= config.max_sample_rate().0
            })
        })
        .ok_or(PlaybackError::NoUsableSampleRate)
}

#[derive(Default)]
struct PlaybackShared {
    /// Mono samples handed over by the decoder, waiting for the audio callback
    /// to pick them up.
    staging: Mutex<Vec<f32>>,
    /// Mono samples accepted from the decoder.
    pushed: AtomicU64,
    /// Mono samples actually written to the device.
    played: AtomicU64,
    /// The producer will not push again.
    ended: AtomicBool,
    /// Stop now and discard whatever is still buffered.
    stopped: AtomicBool,
    /// Linear gain, stored as `f32` bits.
    gain: AtomicU32,
    /// Set by cpal's error callback; turns an otherwise silent device failure
    /// into a reported error instead of a wait that never ends.
    failed: AtomicBool,
}

impl PlaybackShared {
    fn take_staged(&self, local: &mut VecDeque<f32>) {
        // Never block the audio thread. If the decoder happens to hold the lock
        // we play from what we already took rather than inserting a gap; the
        // next callback picks the samples up a few milliseconds later.
        if let Ok(mut staging) = self.staging.try_lock() {
            if !staging.is_empty() {
                local.extend(staging.drain(..));
            }
        }
    }
}

/// Stop control that can cross threads, unlike [`Playback`] itself.
#[derive(Clone)]
pub struct PlaybackHandle {
    shared: Arc<PlaybackShared>,
}

impl PlaybackHandle {
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared
            .gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

pub struct Playback {
    shared: Arc<PlaybackShared>,
    sample_rate: u32,
    /// Kept alive for the utterance; dropping it closes the device.
    _stream: Stream,
}

impl Playback {
    /// Open the default output device at `sample_rate`.
    ///
    /// Must be called from the thread that will feed and drop it.
    pub fn open(sample_rate: u32, gain: f32) -> Result<Self, PlaybackError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(PlaybackError::NoDevice)?;
        let default = device
            .default_output_config()
            .map_err(|err| PlaybackError::Device(err.to_string()))?;

        let channels = default.channels() as usize;
        let config = cpal::StreamConfig {
            channels: default.channels(),
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let shared = Arc::new(PlaybackShared::default());
        shared
            .gain
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);

        let error_shared = shared.clone();
        let on_error = move |err| {
            log::error!("Audio output error: {err}");
            error_shared.failed.store(true, Ordering::SeqCst);
        };

        let stream = match default.sample_format() {
            SampleFormat::F32 => {
                let cb_shared = shared.clone();
                let mut local: VecDeque<f32> = VecDeque::new();
                device.build_output_stream(
                    &config,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        fill(&cb_shared, &mut local, out, channels, |sample, slot| {
                            *slot = sample;
                        });
                    },
                    on_error,
                    None,
                )
            }
            SampleFormat::I16 => {
                let cb_shared = shared.clone();
                let mut local: VecDeque<f32> = VecDeque::new();
                device.build_output_stream(
                    &config,
                    move |out: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        fill(&cb_shared, &mut local, out, channels, |sample, slot| {
                            *slot = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        });
                    },
                    on_error,
                    None,
                )
            }
            other => return Err(PlaybackError::UnsupportedFormat(format!("{other:?}"))),
        }
        .map_err(|err| PlaybackError::Device(err.to_string()))?;

        stream
            .play()
            .map_err(|err| PlaybackError::Device(err.to_string()))?;

        Ok(Self {
            shared,
            sample_rate,
            _stream: stream,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn handle(&self) -> PlaybackHandle {
        PlaybackHandle {
            shared: self.shared.clone(),
        }
    }

    /// Hand mono samples to the device. Returns false once playback has been
    /// stopped, so the decoder can give up instead of decoding into a void.
    pub fn push(&self, samples: &[f32]) -> bool {
        if self.shared.stopped.load(Ordering::SeqCst) {
            return false;
        }
        if let Ok(mut staging) = self.shared.staging.lock() {
            staging.extend_from_slice(samples);
            self.shared
                .pushed
                .fetch_add(samples.len() as u64, Ordering::SeqCst);
        }
        true
    }

    pub fn mark_end_of_stream(&self) {
        self.shared.ended.store(true, Ordering::SeqCst);
    }

    /// Block until everything pushed has reached the device.
    ///
    /// Returns `Ok(false)` when the wait ended because the token was cancelled
    /// or playback was stopped — the caller then has nothing to report as a
    /// completion.
    pub fn wait_until_drained(&self, token: &CancelToken) -> Result<bool, PlaybackError> {
        let mut last_played = self.shared.played.load(Ordering::SeqCst);
        let mut last_progress = Instant::now();

        loop {
            if token.is_cancelled() || self.shared.stopped.load(Ordering::SeqCst) {
                return Ok(false);
            }
            if self.shared.failed.load(Ordering::SeqCst) {
                return Err(PlaybackError::Stalled);
            }

            let played = self.shared.played.load(Ordering::SeqCst);
            let pushed = self.shared.pushed.load(Ordering::SeqCst);
            if self.shared.ended.load(Ordering::SeqCst) && played >= pushed {
                // The device still holds a buffer we already wrote into.
                thread::sleep(DRAIN_TAIL);
                return Ok(true);
            }

            if played != last_played {
                last_played = played;
                last_progress = Instant::now();
            } else if played < pushed && last_progress.elapsed() > STALL_TIMEOUT {
                // Samples are waiting but the device is not taking them.
                return Err(PlaybackError::Stalled);
            }

            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
    }
}

/// Shared body of the audio callback for every sample format.
///
/// Runs on the realtime audio thread: no allocation beyond the local queue's
/// own growth, no blocking locks, no logging.
fn fill<T, W>(
    shared: &Arc<PlaybackShared>,
    local: &mut VecDeque<f32>,
    out: &mut [T],
    channels: usize,
    write: W,
) where
    T: Copy + Default,
    W: Fn(f32, &mut T),
{
    shared.take_staged(local);

    if shared.stopped.load(Ordering::SeqCst) {
        local.clear();
        out.fill(T::default());
        return;
    }

    let gain = f32::from_bits(shared.gain.load(Ordering::Relaxed));
    let mut written = 0u64;

    for frame in out.chunks_mut(channels.max(1)) {
        match local.pop_front() {
            Some(sample) => {
                let value = sample * gain;
                // Mono from the provider, duplicated across the device's
                // channels — the alternative is silence on one ear.
                for slot in frame.iter_mut() {
                    write(value, slot);
                }
                written += 1;
            }
            None => frame.fill(T::default()),
        }
    }

    shared.played.fetch_add(written, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_into<T: Copy + Default + std::fmt::Debug>(
        shared: &Arc<PlaybackShared>,
        local: &mut VecDeque<f32>,
        frames: usize,
        channels: usize,
    ) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * channels];
        fill(shared, local, &mut out, channels, |sample, slot| {
            *slot = sample;
        });
        out
    }

    #[test]
    fn mono_samples_are_duplicated_across_device_channels() {
        // A stereo device fed a mono stream must hear it on both ears.
        let shared = Arc::new(PlaybackShared::default());
        shared.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        shared
            .staging
            .lock()
            .unwrap()
            .extend_from_slice(&[0.5, -0.25]);

        let mut local = VecDeque::new();
        let out = drain_into::<f32>(&shared, &mut local, 2, 2);

        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
        assert_eq!(shared.played.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_underrun_writes_silence_rather_than_repeating_the_last_sample() {
        // Asked for more frames than we have. The tail must be silent, and only
        // the frames actually filled may count as played — otherwise the drain
        // check would think the utterance finished early.
        let shared = Arc::new(PlaybackShared::default());
        shared.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        shared.staging.lock().unwrap().extend_from_slice(&[1.0]);

        let mut local = VecDeque::new();
        let out = drain_into::<f32>(&shared, &mut local, 3, 1);

        assert_eq!(out, vec![1.0, 0.0, 0.0]);
        assert_eq!(shared.played.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn stopping_discards_buffered_audio_instead_of_letting_it_finish() {
        // Pressing the hotkey again must cut the voice off now, not after
        // whatever is already buffered has played out.
        let shared = Arc::new(PlaybackShared::default());
        shared.gain.store(1.0f32.to_bits(), Ordering::Relaxed);
        shared
            .staging
            .lock()
            .unwrap()
            .extend_from_slice(&[1.0, 1.0, 1.0]);
        shared.stopped.store(true, Ordering::SeqCst);

        let mut local = VecDeque::new();
        let out = drain_into::<f32>(&shared, &mut local, 3, 1);

        assert_eq!(out, vec![0.0, 0.0, 0.0]);
        assert!(local.is_empty(), "buffered audio must be dropped, not held");
    }

    #[test]
    fn gain_scales_the_output() {
        let shared = Arc::new(PlaybackShared::default());
        shared.gain.store(0.5f32.to_bits(), Ordering::Relaxed);
        shared
            .staging
            .lock()
            .unwrap()
            .extend_from_slice(&[1.0, -1.0]);

        let mut local = VecDeque::new();
        let out = drain_into::<f32>(&shared, &mut local, 2, 1);

        assert_eq!(out, vec![0.5, -0.5]);
    }

    #[test]
    fn negotiable_rates_are_ordered_best_first() {
        // The list is a preference order, not a set: picking 16 kHz when the
        // device can do 48 kHz would throw away quality for nothing.
        let mut sorted = NEGOTIABLE_RATES;
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(sorted, NEGOTIABLE_RATES);
    }
}
