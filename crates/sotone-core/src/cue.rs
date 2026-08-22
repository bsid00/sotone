//! Audio cues: the only channel that still works in a fullscreen game.
//!
//! Four short tones — recording started, line saved, something failed, the
//! length cap split the recording — played
//! through a **persistent** cpal output stream that is opened once and feeds
//! silence between cues. The persistence is the whole design:
//! `IAudioClient::Initialize` costs tens of milliseconds, and a cue that fires
//! tens of milliseconds after the key went down is a cue that lies about when
//! recording started. So the output side mirrors the input side exactly (see
//! [`audio`](crate::audio)): one stream, opened at startup, owned by its own
//! thread, dropped on shutdown.
//!
//! Three properties are load-bearing:
//!
//! * **The data callback mixes or writes silence, and nothing else.** It runs on
//!   an OS audio thread the mixer waits on. No allocation, no locking, no
//!   logging, no panicking path (invariant 5). The tones are synthesized once,
//!   at startup, and moved into the callback; playback is an index walk.
//! * **[`CuePlayer::play`] is wait-free and infallible.** It is called from the
//!   push-to-talk consumer thread, which is in line behind every input event on
//!   the machine. It does one atomic store and returns (invariant 5).
//! * **Cues are an enhancement, never a blocker.** A machine with no output
//!   device, or a stream that refuses to open, costs the user their cues and
//!   nothing else — [`CuePlayer::start`] returns an error the caller is expected
//!   to log and carry on from. Dictation never depends on this module.
//!
//! Nothing here reads audio, opens a network socket, or creates a window
//! (invariants 2 and 3): a cpal output stream is a mixer client, not a UI.

use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, Stream, StreamConfig};

/// Loudest sample any cue may produce.
///
/// Cues sit *under* whatever the user is testing; a cue that makes someone
/// reach for the volume knob mid-playtest has failed at its job. The ceiling
/// is 0.3 and the tones below stay beneath it.
const CUE_PEAK: f32 = 0.28;

/// Fade applied to each end of every tone segment. Without it the abrupt start
/// and stop are broadband clicks, which are far more startling than the tone.
const FADE: Duration = Duration::from_millis(5);

/// How often the stream thread wakes to re-check the shutdown flag. It is also
/// unparked on shutdown, so this is only a backstop. Mirrors the input side.
const STREAM_PARK: Duration = Duration::from_millis(100);

/// Which cue to play.
///
/// Deliberately closed and tiny: cues are a fixed vocabulary, and the callback
/// indexes a fixed-size table with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cue {
    /// Recording started. Rising, so it reads as "go".
    Begin,
    /// A line was appended to the draft. Short double blip.
    Saved,
    /// Something failed. Low and longer, so it is never mistaken for a save.
    Error,
    /// The utterance length cap split the recording. Two falling notes — the drop
    /// says "something was cut", and it is still recording, so it must not
    /// sound like the [`Cue::Error`] buzz or the [`Cue::Saved`] blips.
    Capped,
}

/// Every cue, in table order. The array index is [`Cue::index`].
const ALL_CUES: [Cue; 4] = [Cue::Begin, Cue::Saved, Cue::Error, Cue::Capped];

impl Cue {
    /// Position in the pre-rendered tone table.
    const fn index(self) -> usize {
        match self {
            Cue::Begin => 0,
            Cue::Saved => 1,
            Cue::Error => 2,
            Cue::Capped => 3,
        }
    }

    /// How long the whole cue lasts.
    #[must_use]
    pub const fn duration(self) -> Duration {
        match self {
            Cue::Begin => Duration::from_millis(150),
            // Two 45 ms blips with a 40 ms gap.
            Cue::Saved => Duration::from_millis(130),
            Cue::Error => Duration::from_millis(250),
            // Two 150 ms notes with a 50 ms gap. Long enough to hear the fall,
            // short enough not to sit on top of what the user is still saying.
            Cue::Capped => Duration::from_millis(350),
        }
    }
}

/// Why the cue player could not start.
///
/// Every one of these is survivable: the caller logs it and runs without cues.
#[derive(Debug, thiserror::Error)]
pub enum CueError {
    /// The system reports no usable output device.
    #[error("no audio output device is available, so Sotone will run without cues")]
    NoOutputDevice,

    /// The device has no default output configuration we can open.
    #[error("could not read the default output configuration of '{device}'")]
    DefaultConfig {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// The device wants a sample format we do not write.
    #[error("output device '{device}' wants samples as {format:?}, which Sotone cannot write")]
    UnsupportedSampleFormat {
        /// Human-readable device name.
        device: String,
        /// The format cpal reported.
        format: SampleFormat,
    },

    /// The stream could not be created.
    #[error("could not open an output stream on '{device}'")]
    BuildStream {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// The stream was created but refused to start.
    #[error("could not start the output stream on '{device}'")]
    PlayStream {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// The stream thread could not be spawned.
    #[error("could not start the audio cue thread")]
    ThreadSpawn(#[source] std::io::Error),

    /// The stream thread went away before it reported success or failure.
    #[error("the audio cue thread stopped before the stream was ready")]
    StreamThreadDied,
}

/// State shared between the handle and the output stream.
struct Shared {
    /// The cue the callback should start on its next pass, plus one, so that
    /// zero can mean "nothing requested". A single byte written by [`CuePlayer::play`]
    /// and swapped to zero by the callback — no locks anywhere on either side.
    pending: AtomicU8,
    /// Set on Drop; the stream thread polls it.
    shutdown: AtomicBool,
    /// Set by the cpal error callback. Read by [`CuePlayer::failure`].
    failed: AtomicBool,
}

/// Facts about the opened output stream, reported back from the stream thread.
struct StreamFacts {
    device: String,
    sample_rate: u32,
    channels: u16,
}

/// The error channel plus the reason drained out of it, behind one lock.
///
/// Locked only from caller threads, never from a cpal callback.
struct Failures {
    errors: Receiver<cpal::Error>,
    reason: Option<String>,
}

/// A persistent output stream that can play one of three short tones.
///
/// Dropping it stops the stream; it does not block on anything longer than
/// [`STREAM_PARK`].
pub struct CuePlayer {
    shared: Arc<Shared>,
    facts: StreamFacts,
    failures: Mutex<Failures>,
    stream_thread: Option<JoinHandle<()>>,
}

impl CuePlayer {
    /// Open the default output device and start feeding it silence.
    ///
    /// # Errors
    /// Returns [`CueError`] if there is no output device or its stream cannot be
    /// opened. Treat that as "run without cues": nothing else in Sotone depends
    /// on this succeeding.
    pub fn start() -> Result<Self, CueError> {
        let shared = Arc::new(Shared {
            pending: AtomicU8::new(0),
            shutdown: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });

        let (init_tx, init_rx) = mpsc::channel::<Result<StreamFacts, CueError>>();
        let (error_tx, error_rx) = mpsc::channel::<cpal::Error>();

        let stream_shared = Arc::clone(&shared);
        let stream_thread = thread::Builder::new()
            .name("sotone-cue-stream".to_owned())
            .spawn(move || stream_thread_main(&stream_shared, &init_tx, error_tx))
            .map_err(CueError::ThreadSpawn)?;

        let facts = match init_rx.recv() {
            Ok(Ok(facts)) => facts,
            Ok(Err(err)) => {
                let _ = stream_thread.join();
                return Err(err);
            }
            Err(_) => {
                let _ = stream_thread.join();
                return Err(CueError::StreamThreadDied);
            }
        };

        tracing::info!(
            device = %facts.device,
            sample_rate = facts.sample_rate,
            channels = facts.channels,
            "cue output stream open"
        );

        Ok(Self {
            shared,
            facts,
            failures: Mutex::new(Failures {
                errors: error_rx,
                reason: None,
            }),
            stream_thread: Some(stream_thread),
        })
    }

    /// Ask for a cue. Returns immediately, always.
    ///
    /// One `Release` store into a byte the output callback swaps out on its next
    /// pass: no lock, no allocation, no channel, nothing that can block. That is
    /// what makes this callable from the push-to-talk consumer thread, which is
    /// queued behind every input event on the machine (invariant 5).
    ///
    /// Best-effort by design. A request that arrives while another cue is
    /// playing replaces it, and a request that arrives twice within one callback
    /// period is heard once — both are correct for a cue and neither is worth
    /// reporting.
    pub fn play(&self, cue: Cue) {
        // +1 so that 0 stays free to mean "nothing requested".
        let request = cue.index() as u8 + 1;
        self.shared.pending.store(request, Ordering::Release);
    }

    /// The output device in use.
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.facts.device
    }

    /// The stream's sample rate.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.facts.sample_rate
    }

    /// The stream's channel count.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.facts.channels
    }

    /// Whether the output stream has failed, and why.
    ///
    /// `None` while everything is fine. There is no reconnect: a dead cue stream
    /// costs the user their cues, and dictation carries on regardless.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        if !self.shared.failed.load(Ordering::Acquire) {
            return None;
        }
        let Ok(mut failures) = self.failures.lock() else {
            return Some("the audio output device stopped".to_owned());
        };
        if failures.reason.is_none() {
            // The callback flagged first and sent second, so the channel may
            // still be empty for a moment; the generic message covers that.
            failures.reason = failures.errors.try_iter().next().map(|err| err.to_string());
        }
        Some(
            failures
                .reason
                .clone()
                .unwrap_or_else(|| "the audio output device stopped".to_owned()),
        )
    }
}

impl std::fmt::Debug for CuePlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CuePlayer")
            .field("device", &self.facts.device)
            .field("sample_rate", &self.facts.sample_rate)
            .field("channels", &self.facts.channels)
            .finish_non_exhaustive()
    }
}

impl Drop for CuePlayer {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.stream_thread.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Stream thread
// ---------------------------------------------------------------------------

fn stream_thread_main(
    shared: &Arc<Shared>,
    init_tx: &Sender<Result<StreamFacts, CueError>>,
    error_tx: Sender<cpal::Error>,
) {
    match open_stream(shared, error_tx) {
        Ok((stream, facts)) => {
            if init_tx.send(Ok(facts)).is_err() {
                return;
            }
            while !shared.shutdown.load(Ordering::Acquire) {
                thread::park_timeout(STREAM_PARK);
            }
            // `Stream` is only portably usable on the thread that built it, so
            // it is dropped here, explicitly, before this thread exits.
            drop(stream);
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
        }
    }
}

fn open_stream(
    shared: &Arc<Shared>,
    error_tx: Sender<cpal::Error>,
) -> Result<(Stream, StreamFacts), CueError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(CueError::NoOutputDevice)?;
    let name = device_name(&device);

    // Whatever the device already runs at. WASAPI shared mode means opening
    // here never takes the speakers away from the app under test.
    let supported = device
        .default_output_config()
        .map_err(|source| CueError::DefaultConfig {
            device: name.clone(),
            source,
        })?;

    let channels = supported.channels().max(1);
    let sample_rate = supported.sample_rate();
    let format = supported.sample_format();
    let config = supported.config();

    // Rendered here, on the stream thread, before the callback exists: the
    // callback must never allocate.
    let tones = render_all(sample_rate);

    let stream = build_stream(&device, &name, &config, format, tones, shared, error_tx)?;
    stream.play().map_err(|source| CueError::PlayStream {
        device: name.clone(),
        source,
    })?;

    Ok((
        stream,
        StreamFacts {
            device: name,
            sample_rate,
            channels,
        },
    ))
}

/// The device's human-readable name.
///
/// Same shape as the input side's helper; duplicated rather than shared because
/// that one is private to the `audio` module and this one is three lines.
fn device_name(device: &Device) -> String {
    match device.description() {
        Ok(description) => description.name().to_owned(),
        Err(_) => device.to_string(),
    }
}

/// Dispatch on the runtime sample format to a single generic stream builder.
#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &Device,
    name: &str,
    config: &StreamConfig,
    format: SampleFormat,
    tones: Tones,
    shared: &Arc<Shared>,
    error_tx: Sender<cpal::Error>,
) -> Result<Stream, CueError> {
    match format {
        SampleFormat::F32 => build_stream_as::<f32>(device, name, config, tones, shared, error_tx),
        SampleFormat::F64 => build_stream_as::<f64>(device, name, config, tones, shared, error_tx),
        SampleFormat::I8 => build_stream_as::<i8>(device, name, config, tones, shared, error_tx),
        SampleFormat::I16 => build_stream_as::<i16>(device, name, config, tones, shared, error_tx),
        SampleFormat::I32 => build_stream_as::<i32>(device, name, config, tones, shared, error_tx),
        SampleFormat::U8 => build_stream_as::<u8>(device, name, config, tones, shared, error_tx),
        SampleFormat::U16 => build_stream_as::<u16>(device, name, config, tones, shared, error_tx),
        SampleFormat::U32 => build_stream_as::<u32>(device, name, config, tones, shared, error_tx),
        other => Err(CueError::UnsupportedSampleFormat {
            device: name.to_owned(),
            format: other,
        }),
    }
}

fn build_stream_as<T>(
    device: &Device,
    name: &str,
    config: &StreamConfig,
    tones: Tones,
    shared: &Arc<Shared>,
    error_tx: Sender<cpal::Error>,
) -> Result<Stream, CueError>
where
    T: SizedSample + cpal::FromSample<f32> + Send + 'static,
{
    let data_shared = Arc::clone(shared);
    let error_shared = Arc::clone(shared);

    // At least one, so the frame chunking below can never be a zero-size split.
    let channels = usize::from(config.channels.max(1));

    // Playback position, owned by the callback. `None` is "feeding silence".
    let mut active: Option<usize> = None;
    let mut position = 0usize;

    let stream = device
        .build_output_stream::<T, _, _>(
            *config,
            // ---- THE DATA CALLBACK ----------------------------------------
            // Runs on a cpal-owned thread the OS mixer waits on, so it must
            // never allocate, lock, log, or panic. One atomic swap, then a walk
            // over a table rendered at startup. That is the whole body.
            // `chunks_mut` cannot panic (`channels` is at least 1) and every
            // lookup is a non-panicking `get`.
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let requested = data_shared.pending.swap(0, Ordering::Acquire);
                if requested != 0 {
                    active = Some(usize::from(requested - 1));
                    position = 0;
                }

                for frame in data.chunks_mut(channels) {
                    let value = match active {
                        Some(index) => match tones.sample(index, position) {
                            Some(sample) => {
                                position += 1;
                                sample
                            }
                            None => {
                                active = None;
                                0.0
                            }
                        },
                        None => 0.0,
                    };
                    // Same value to every channel: a cue is mono and must not
                    // appear to come from one side of the user's headphones.
                    for slot in frame.iter_mut() {
                        *slot = T::from_sample(value);
                    }
                }
            },
            // ---- THE ERROR CALLBACK ---------------------------------------
            // Not our thread either: flag and forward, never format or log.
            move |err: cpal::Error| {
                error_shared.failed.store(true, Ordering::Release);
                let _ = error_tx.send(err);
            },
            None,
        )
        .map_err(|source| CueError::BuildStream {
            device: name.to_owned(),
            source,
        })?;

    Ok(stream)
}

// ---------------------------------------------------------------------------
// Tone synthesis (no device, fully testable)
// ---------------------------------------------------------------------------

/// The pre-rendered cue table, at the output device's own rate.
///
/// Owned by the data callback for the life of the stream. Built once, read
/// forever: the callback allocating a tone would be exactly the stall invariant
/// 5 forbids.
struct Tones {
    tables: [Vec<f32>; ALL_CUES.len()],
}

impl Tones {
    /// One sample, or `None` past the end of that cue.
    #[inline]
    fn sample(&self, index: usize, position: usize) -> Option<f32> {
        self.tables.get(index)?.get(position).copied()
    }
}

/// Render every cue at `sample_rate`.
fn render_all(sample_rate: u32) -> Tones {
    Tones {
        tables: [
            cue_samples(Cue::Begin, sample_rate),
            cue_samples(Cue::Saved, sample_rate),
            cue_samples(Cue::Error, sample_rate),
            cue_samples(Cue::Capped, sample_rate),
        ],
    }
}

/// The mono waveform for one cue.
///
/// Frequencies are a judgement call, not a spec: rising for "go", a bright pair
/// for "saved", a low buzz for "error". What is not a judgement call is the
/// peak ([`CUE_PEAK`]) and the fades — see the constants.
fn cue_samples(cue: Cue, sample_rate: u32) -> Vec<f32> {
    let total = samples_for(cue.duration(), sample_rate);
    let mut out = vec![0.0f32; total];

    match cue {
        // 660 Hz to 990 Hz, a rising fifth.
        Cue::Begin => render_segment(&mut out, sample_rate, 660.0, 990.0),
        Cue::Saved => {
            let blip = samples_for(Duration::from_millis(45), sample_rate);
            let gap = samples_for(Duration::from_millis(40), sample_rate);
            if let Some(first) = out.get_mut(..blip) {
                render_segment(first, sample_rate, 880.0, 880.0);
            }
            if let Some(second) = out.get_mut(blip + gap..) {
                render_segment(second, sample_rate, 880.0, 880.0);
            }
        }
        // Slightly falling, which reads as a buzz rather than a note.
        Cue::Error => render_segment(&mut out, sample_rate, 220.0, 196.0),
        // A clear two-note drop, 659 Hz (E5) to 440 Hz (A4). Two notes rather
        // than a glide because a glide at this length reads as a siren, and
        // this cue has to be recognisable through a game's own audio.
        Cue::Capped => {
            let note = samples_for(Duration::from_millis(150), sample_rate);
            let gap = samples_for(Duration::from_millis(50), sample_rate);
            if let Some(first) = out.get_mut(..note) {
                render_segment(first, sample_rate, 659.0, 659.0);
            }
            if let Some(second) = out.get_mut(note + gap..) {
                render_segment(second, sample_rate, 440.0, 440.0);
            }
        }
    }

    out
}

/// Whole samples in `duration` at `sample_rate`.
fn samples_for(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

/// Write one faded tone over the whole of `dst`, sweeping `start_hz` to
/// `end_hz`.
///
/// The phase is accumulated rather than computed from `sin(2πft)`: with a
/// changing `f` the closed form produces a phase discontinuity every sample,
/// which is audible as a rasp.
fn render_segment(dst: &mut [f32], sample_rate: u32, start_hz: f32, end_hz: f32) {
    let len = dst.len();
    if len == 0 || sample_rate == 0 {
        return;
    }
    let rate = sample_rate as f32;
    // Never more than half the segment per side, so the two fades cannot
    // overlap and cancel the tone entirely.
    let fade = samples_for(FADE, sample_rate).min(len / 2).max(1);
    let mut phase = 0.0f32;

    for (index, slot) in dst.iter_mut().enumerate() {
        let progress = index as f32 / len as f32;
        let frequency = start_hz + (end_hz - start_hz) * progress;
        *slot = phase.sin() * CUE_PEAK * envelope(index, len, fade);
        phase += TAU * frequency / rate;
        // Wrap so a long tone never loses precision in `sin`.
        if phase >= TAU {
            phase -= TAU;
        }
    }
}

/// Linear fade in and out, so no segment starts or ends on a step.
fn envelope(index: usize, len: usize, fade: usize) -> f32 {
    let from_start = index + 1;
    let from_end = len.saturating_sub(index);
    let rise = (from_start.min(fade) as f32) / fade as f32;
    let fall = (from_end.min(fade) as f32) / fade as f32;
    rise.min(fall)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nothing in here opens a device. The tone tables are pure arithmetic,
    // which is the whole reason synthesis is a free function.

    const RATE: u32 = 48_000;

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()))
    }

    #[test]
    fn every_cue_is_finite_and_under_the_peak_ceiling() {
        for cue in ALL_CUES {
            let samples = cue_samples(cue, RATE);
            assert!(
                samples.iter().all(|s| s.is_finite()),
                "{cue:?} produced a non-finite sample"
            );
            assert!(
                peak(&samples) <= 0.3,
                "{cue:?} is louder than the cue ceiling allows"
            );
            assert!(peak(&samples) > 0.0, "{cue:?} is silent");
        }
    }

    #[test]
    fn every_cue_is_as_long_as_it_says_it_is() {
        for cue in ALL_CUES {
            for rate in [16_000, 44_100, 48_000] {
                let samples = cue_samples(cue, rate);
                assert_eq!(samples.len(), samples_for(cue.duration(), rate), "{cue:?}");
            }
        }
    }

    #[test]
    fn every_cue_starts_and_ends_near_silence() {
        // A step at either end is a click, and a click next to a headset mic is
        // worse than no cue at all.
        for cue in ALL_CUES {
            let samples = cue_samples(cue, RATE);
            let first = samples.first().copied().unwrap_or(1.0);
            let last = samples.last().copied().unwrap_or(1.0);
            assert!(first.abs() < 0.01, "{cue:?} starts at {first}");
            assert!(last.abs() < 0.01, "{cue:?} ends at {last}");
        }
    }

    #[test]
    fn the_saved_cue_has_a_gap_between_its_two_blips() {
        let samples = cue_samples(Cue::Saved, RATE);
        let blip = samples_for(Duration::from_millis(45), RATE);
        let gap = samples_for(Duration::from_millis(20), RATE);
        // 20 ms into the 40 ms gap is silent by construction.
        let middle = samples.get(blip + gap).copied().unwrap_or(1.0);
        assert_eq!(middle, 0.0, "the double blip has no gap");
        // And there is real signal on both sides of it.
        assert!(peak(&samples[..blip]) > 0.1);
        assert!(peak(&samples[blip + gap * 2..]) > 0.1);
    }

    /// Zero crossings in a slice — a cheap pitch proxy that needs no FFT.
    fn crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    }

    #[test]
    fn the_capped_cue_is_two_notes_and_the_second_one_is_lower() {
        // "Falling" is the whole message: something was cut, and this is not an
        // error. A rising or flat cue here would read as a save.
        let samples = cue_samples(Cue::Capped, RATE);
        let note = samples_for(Duration::from_millis(150), RATE);
        let gap = samples_for(Duration::from_millis(50), RATE);

        let first = &samples[..note];
        let second = &samples[note + gap..];
        assert!(peak(first) > 0.1, "the first note is missing");
        assert!(peak(second) > 0.1, "the second note is missing");
        assert!(
            crossings(second) < crossings(first),
            "the second note ({} crossings) must be lower than the first ({})",
            crossings(second),
            crossings(first)
        );

        // And there is real silence between them, so it is heard as two notes.
        let middle = samples
            .get(note + gap / 2)
            .copied()
            .expect("the gap is inside the cue");
        assert_eq!(middle, 0.0);
    }

    #[test]
    fn the_capped_cue_is_long_enough_to_register_but_not_a_jingle() {
        // The specified range is 300–400 ms.
        let cap = Cue::Capped.duration();
        assert!(cap >= Duration::from_millis(300) && cap <= Duration::from_millis(400));
        // And it is distinguishable from every other cue by length alone.
        for other in [Cue::Begin, Cue::Saved, Cue::Error] {
            assert!(other.duration() < cap, "{other:?} is as long as Capped");
        }
    }

    #[test]
    fn the_cue_table_indexes_line_up_with_the_enum() {
        for (index, cue) in ALL_CUES.iter().enumerate() {
            assert_eq!(cue.index(), index);
        }
        let tones = render_all(16_000);
        for cue in ALL_CUES {
            assert!(tones.sample(cue.index(), 0).is_some());
            let past_end = samples_for(cue.duration(), 16_000);
            assert_eq!(tones.sample(cue.index(), past_end), None);
        }
    }

    #[test]
    fn a_degenerate_rate_renders_nothing_rather_than_panicking() {
        // No device reports 0 Hz, but the callback must not be one bad
        // `default_output_config` away from a division by zero.
        for cue in ALL_CUES {
            assert!(cue_samples(cue, 0).is_empty());
        }
        let mut empty: [f32; 0] = [];
        render_segment(&mut empty, RATE, 440.0, 440.0);
    }

    #[test]
    fn the_envelope_is_a_symmetric_ramp() {
        assert_eq!(envelope(0, 100, 10), 0.1);
        assert_eq!(envelope(99, 100, 10), 0.1);
        assert_eq!(envelope(50, 100, 10), 1.0);
    }
}
