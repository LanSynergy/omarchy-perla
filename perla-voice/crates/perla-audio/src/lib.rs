//! Audio I/O for perla-voice.
//!
//! The realtime providers speak PCM16 mono @ 24 kHz. This crate owns the
//! device streams (cpal), resamples between the device rate and 24 kHz, and
//! exposes:
//!
//! - a capture channel of 24 kHz PCM16 frames (~100 ms each),
//! - a playback handle (`push` / `clear` / drained-watch — the drained signal
//!   is the WebSocket-transport equivalent of WebRTC's
//!   `output_audio_buffer.stopped`, i.e. "the user genuinely stopped hearing
//!   Perla"),
//! - a mic level watch (0..=1 RMS) for meters,
//! - a mute flag applied at the source (a muted mic sends nothing at all).
//!
//! cpal streams are !Send, so both live on one dedicated OS thread.

mod resample;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

pub const TARGET_SAMPLE_RATE: u32 = 24_000;
/// ~100ms of 24kHz mono.
const CAPTURE_FRAME: usize = 2_400;
/// How long after the last speaker sample the room keeps ringing. Mic audio
/// inside this window is Perla's own voice arriving back through the air, not
/// the user — see `AudioOptions::echo_guard`.
const ECHO_TAIL_MS: u64 = 300;

/// How the audio system handles the speaker → mic feedback path.
///
/// With speakers (not headphones) Perla hears herself: her voice leaves the
/// speaker, the mic picks it up, and the engine forwards it to the model as
/// user speech — so she answers herself, forever. Two layers stop that:
///
/// - `echo_guard` (this crate, always available): a half-duplex gate. While
///   Perla is audibly speaking, mic frames are dropped unless they are loud
///   enough to be a deliberate interruption (`barge_rms`).
/// - `aec` (feature `aec`): real acoustic echo cancellation, which subtracts
///   the speaker signal from the mic instead of gating, giving full duplex.
///   Falls back to the guard when unavailable.
#[derive(Debug, Clone, Copy)]
pub struct AudioOptions {
    /// Open with the mic muted.
    pub start_muted: bool,
    /// Gate the mic while Perla is audibly speaking.
    pub echo_guard: bool,
    /// Mic RMS (0..=1) above which audio passes the guard as a deliberate
    /// barge-in. Perla's echo is far quieter than someone talking over her.
    pub barge_rms: f32,
    /// Prefer real echo cancellation over the guard when compiled in.
    pub aec: bool,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            start_muted: false,
            echo_guard: true,
            barge_rms: 0.05,
            aec: true,
        }
    }
}

pub struct AudioSystem {
    /// 24kHz mono PCM16 frames from the mic (empty while muted). Taken once
    /// by the engine's capture pipe via `take_capture`.
    capture_rx: Option<mpsc::UnboundedReceiver<Vec<i16>>>,
    /// RMS mic level 0..=1, updated ~10Hz.
    pub mic_level: watch::Receiver<f32>,
    /// True whenever the playback queue is empty (Perla is not audibly
    /// speaking). Flips false on push, true when the last sample drains.
    pub playback_drained: watch::Receiver<bool>,
    playback: PlaybackHandle,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct PlaybackHandle {
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// Device output sample rate — pushes resample 24k → this.
    out_rate: Arc<Mutex<u32>>,
}

impl PlaybackHandle {
    /// Queue PCM16 mono 24kHz for playback.
    pub fn push_pcm16(&self, samples: &[i16]) {
        let out_rate = *self.out_rate.lock().unwrap();
        let resampled = resample::linear_i16_to_f32(samples, TARGET_SAMPLE_RATE, out_rate);
        let mut q = self.queue.lock().unwrap();
        q.extend(resampled);
    }

    /// Drop everything queued but not yet played — the barge-in kill. This is
    /// the client-side half of "stop actually stops": the server's
    /// `response.cancel` halts generation, this silences what's buffered.
    pub fn clear(&self) {
        self.queue.lock().unwrap().clear();
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

impl AudioSystem {
    /// Open default input+output devices and start streaming.
    pub fn start(opts: AudioOptions) -> Result<Self> {
        let (capture_tx, capture_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let (level_tx, mic_level) = watch::channel(0.0f32);
        let (drained_tx, playback_drained) = watch::channel(true);

        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let out_rate = Arc::new(Mutex::new(TARGET_SAMPLE_RATE));
        let playback = PlaybackHandle {
            queue: queue.clone(),
            out_rate: out_rate.clone(),
        };

        let muted = Arc::new(AtomicBool::new(opts.start_muted));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let muted = muted.clone();
            let stop = stop.clone();
            let queue = queue.clone();
            let out_rate = out_rate.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
            let handle = std::thread::Builder::new()
                .name("perla-audio".into())
                .spawn(move || {
                    let result = run_streams(
                        capture_tx, level_tx, drained_tx, queue, out_rate, muted, stop, opts,
                    );
                    match result {
                        Ok(guards) => {
                            let _ = ready_tx.send(Ok(()));
                            // Park until told to stop; dropping the guards
                            // closes the streams.
                            while !guards.stop.load(Ordering::Relaxed) {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            drop(guards);
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                })
                .map_err(|e| anyhow!("spawn audio thread: {e}"))?;
            ready_rx
                .recv()
                .map_err(|_| anyhow!("audio thread died during startup"))??;
            handle
        };

        Ok(Self {
            capture_rx: Some(capture_rx),
            mic_level,
            playback_drained,
            playback,
            muted,
            stop,
            thread: Some(thread),
        })
    }

    pub fn playback(&self) -> PlaybackHandle {
        self.playback.clone()
    }

    /// The mic frame stream — take it once and pipe it to the provider.
    pub fn take_capture(&mut self) -> Option<mpsc::UnboundedReceiver<Vec<i16>>> {
        self.capture_rx.take()
    }

    pub fn set_muted(&self, on: bool) {
        self.muted.store(on, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioSystem {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Keeps the cpal streams alive on the audio thread.
struct StreamGuards {
    _input: cpal::Stream,
    _output: cpal::Stream,
    stop: Arc<AtomicBool>,
}

fn run_streams(
    capture_tx: mpsc::UnboundedSender<Vec<i16>>,
    level_tx: watch::Sender<f32>,
    drained_tx: watch::Sender<bool>,
    queue: Arc<Mutex<VecDeque<f32>>>,
    out_rate: Arc<Mutex<u32>>,
    muted: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    opts: AudioOptions,
) -> Result<StreamGuards> {
    let host = cpal::default_host();

    // Shared speaker clock for the echo guard: the output callback stamps it
    // every time it hands real samples to the device, the input callback reads
    // it to know whether Perla can currently be heard in the room. An instant
    // plus an atomic millis counter keeps the hot path lock-free.
    let epoch = Instant::now();
    let last_render_ms = Arc::new(AtomicU64::new(0));

    // ── input ──────────────────────────────────────────────────────────
    let input = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device (microphone)"))?;
    let in_config = input.default_input_config()?;
    let in_rate = in_config.sample_rate();
    let in_channels = in_config.channels() as usize;
    debug!(rate = in_rate, channels = in_channels, "input device");

    // Accumulate device-rate mono f32, resample to 24k i16 in ~100ms frames.
    let mut resampler = resample::StreamResampler::new(in_rate, TARGET_SAMPLE_RATE);
    let mut pending: Vec<i16> = Vec::with_capacity(CAPTURE_FRAME * 2);
    let muted_in = muted.clone();
    let last_render_in = last_render_ms.clone();
    let echo_guard = opts.echo_guard;
    let barge_rms = opts.barge_rms;
    let mut level_accum: f32 = 0.0;
    let mut level_count: usize = 0;

    let input_stream = input.build_input_stream(
        in_config.into(),
        move |data: &[f32], _| {
            // Downmix to mono.
            let mono: Vec<f32> = data
                .chunks(in_channels)
                .map(|frame| frame.iter().sum::<f32>() / in_channels as f32)
                .collect();

            // Level meter (even while muted — the UI should show a live mic).
            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / mono.len().max(1) as f32).sqrt();
            level_accum += rms;
            level_count += 1;
            if level_count >= 4 {
                let _ = level_tx.send((level_accum / level_count as f32 * 4.0).min(1.0));
                level_accum = 0.0;
                level_count = 0;
            }

            if muted_in.load(Ordering::Relaxed) {
                pending.clear();
                return;
            }

            // Echo guard. Perla's voice reaching the mic is quiet; someone
            // talking over her is not. Anything under the barge threshold
            // while she is audible is her own echo, so it never reaches the
            // model. `pending` is dropped too, so a frame is never stitched
            // together from before and after her turn.
            if echo_guard {
                let last = last_render_in.load(Ordering::Relaxed);
                let now_ms = epoch.elapsed().as_millis() as u64;
                if last > 0 && now_ms.saturating_sub(last) < ECHO_TAIL_MS && rms < barge_rms {
                    pending.clear();
                    return;
                }
            }
            pending.extend(
                resampler
                    .process(&mono)
                    .iter()
                    .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
            );
            while pending.len() >= CAPTURE_FRAME {
                let frame: Vec<i16> = pending.drain(..CAPTURE_FRAME).collect();
                let _ = capture_tx.send(frame);
            }
        },
        |e| warn!("input stream error: {e}"),
        None,
    )?;
    input_stream.play()?;

    // ── output ─────────────────────────────────────────────────────────
    let output = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device (speaker)"))?;
    let out_config = output.default_output_config()?;
    *out_rate.lock().unwrap() = out_config.sample_rate();
    let out_channels = out_config.channels() as usize;
    debug!(
        rate = out_config.sample_rate(),
        channels = out_channels,
        "output device"
    );

    let mut was_playing = false;
    let last_render_out = last_render_ms.clone();
    let output_stream = output.build_output_stream(
        out_config.into(),
        move |data: &mut [f32], _| {
            let mut q = queue.lock().unwrap();
            let mut wrote_audio = false;
            for frame in data.chunks_mut(out_channels) {
                let sample = match q.pop_front() {
                    Some(s) => {
                        wrote_audio = true;
                        s
                    }
                    None => 0.0,
                };
                for slot in frame.iter_mut() {
                    *slot = sample;
                }
            }
            let empty = q.is_empty();
            drop(q);

            // Only real samples count: silence padding after the queue drains
            // must not hold the guard open.
            if wrote_audio {
                last_render_out.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
            }
            if empty && was_playing {
                was_playing = false;
                let _ = drained_tx.send(true);
            } else if !empty && !was_playing {
                was_playing = true;
                let _ = drained_tx.send(false);
            }
        },
        |e| warn!("output stream error: {e}"),
        None,
    )?;
    output_stream.play()?;

    Ok(StreamGuards {
        _input: input_stream,
        _output: output_stream,
        stop,
    })
}
