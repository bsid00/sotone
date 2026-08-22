//! Microphone capture.
//!
//! One persistent cpal input stream, opened once at startup in whatever the
//! device's native format is, feeding a lock-free ring buffer. A worker thread
//! drains that ring, keeps a rolling pre-roll, assembles utterances on demand,
//! and hands each finished one back as 16 kHz mono `f32` — the only shape
//! whisper accepts.
//!
//! Two threads and two callbacks are involved, so the ordering rules are worth
//! stating once here:
//!
//! * The cpal **data** callback runs on a cpal-owned thread that the OS audio
//!   engine waits on. It converts to `f32` and pushes into the ring. Nothing
//!   else — see [`capture`]'s `enqueue`.
//! * The cpal **error** callback sets a flag and forwards the error to the
//!   worker. It never logs, because formatting a log line is work on a thread
//!   we do not own.
//! * The **worker** thread does everything expensive: downmix, resample,
//!   normalize, logging, delivery. That order matters — see [`resample`].
//! * The **stream** thread only builds, owns and finally drops the stream.
//!   `cpal::Stream` is `Send` on Windows but not portably so; keeping it on one
//!   thread for its whole life avoids relying on that.
//! * A **device change** ([`AudioEngine::reconnect`]) closes that
//!   thread and opens another, on the caller's thread and never on a callback.
//!   The worker thread and the utterance channel survive it: the stream is
//!   opened once *per selected device*, still never per keypress.

mod capture;
mod device;
mod resample;

pub use capture::{AudioEngine, CaptureOptions, EngineStatus, NativeAudio, Reconnect, Utterance};
pub use device::{list_input_devices, InputDeviceInfo};
pub use resample::{
    channel_stats, downmix_to_mono, normalize_peak, resample_to_16k, ChannelCorrelation,
    ChannelStats,
};

use std::time::Duration;

/// Whisper's input rate. Not configurable — it is baked into the model's
/// mel-spectrogram front end.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

// Pre-roll, post-roll and the utterance cap are constants, not configuration:
// the design fixes the first two and exposes none of them. Changing them is a
// code change with a test behind it, not a knob a user can get wrong.

/// Audio kept from *before* the key went down, so the first consonant of the
/// first word survives the human's reaction time.
const PRE_ROLL: Duration = Duration::from_millis(400);

/// Audio kept after the key comes up, for the same reason at the other end.
const POST_ROLL: Duration = Duration::from_millis(200);

/// Hard ceiling on one *line* of audio: two minutes.
///
/// Reaching it never ends the recording and never costs the user a syllable:
/// the capture worker closes the chunk at the cap sample, flags it
/// [`Utterance::capped`], and starts the next one in the same pass with the
/// usual pre-roll — which is by construction the tail of the chunk just
/// delivered, so the seam loses nothing. Capture only stops when the user
/// stops it. The chunk opened at the seam carries [`Utterance::continued`],
/// because no begin cue sounds there and the session must not trim one out of
/// live speech.
///
/// Two minutes of 16 kHz mono `f32` is ~7.7 MB delivered (120 s × 16 000
/// samples × 4 bytes); the native-rate buffer it is resampled from is the
/// expensive one — ~46 MB at 48 kHz stereo (120 s × 48 000 × 2 × 4 bytes).
///
/// That memory, and the transcription latency of one chunk, are now the
/// *whole* purpose of the constant. It used to be a runaway guard; with
/// split-don't-drop a physically stuck push-to-talk key produces a chunk every
/// two minutes instead of one capped utterance, and in a quiet room those
/// chunks die at the session's level gate rather than filling the draft.
pub const MAX_UTTERANCE: Duration = Duration::from_secs(120);

/// Failures in the capture pipeline.
///
/// Device *loss* is deliberately not in here: once the stream is running, a
/// disappearing microphone is reported through [`AudioEngine::status`] rather
/// than as an error return, because there is nobody left holding a `Result`.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// The system reports no usable microphone at all.
    #[error("no audio input device is available")]
    NoInputDevice,

    /// The host refused to enumerate its devices.
    #[error("could not list audio input devices")]
    Enumerate(#[source] cpal::Error),

    /// The device has no default input configuration we can open.
    #[error("could not read the default input configuration of '{device}'")]
    DefaultConfig {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// The device reports zero channels, which we cannot make sense of.
    #[error("input device '{device}' reports {channels} channels")]
    ImpossibleChannelCount {
        /// Human-readable device name.
        device: String,
        /// What the device claimed.
        channels: u16,
    },

    /// The device delivers a sample format we do not convert.
    #[error("input device '{device}' delivers samples as {format:?}, which Sotone cannot read")]
    UnsupportedSampleFormat {
        /// Human-readable device name.
        device: String,
        /// The format cpal reported.
        format: cpal::SampleFormat,
    },

    /// The stream could not be created.
    #[error("could not open an input stream on '{device}'")]
    BuildStream {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// The stream was created but refused to start.
    #[error("could not start the input stream on '{device}'")]
    PlayStream {
        /// Human-readable device name.
        device: String,
        /// The underlying cpal error.
        #[source]
        source: cpal::Error,
    },

    /// A resampler could not be constructed for this rate pair.
    #[error("could not build a resampler to {TARGET_SAMPLE_RATE} Hz")]
    ResamplerConstruction(#[from] rubato::ResamplerConstructionError),

    /// Resampling a finished utterance failed.
    #[error("could not resample the utterance to {TARGET_SAMPLE_RATE} Hz")]
    Resample(#[from] rubato::ResampleError),

    /// A resampler buffer was rejected as the wrong size. Indicates a bug here,
    /// not bad input.
    #[error("internal resampler buffer has the wrong shape")]
    ResampleBuffer(#[from] rubato::audioadapter_buffers::SizeError),

    /// One of the audio threads could not be spawned.
    #[error("could not start the audio thread")]
    ThreadSpawn(#[source] std::io::Error),

    /// The stream thread went away before it reported success or failure.
    #[error("the audio stream thread stopped before the stream was ready")]
    StreamThreadDied,

    /// A reconnect opened a device with nobody left to drain it: the capture
    /// worker has stopped, which means the session is already over.
    #[error("the capture worker has stopped, so the microphone cannot be changed")]
    WorkerGone,
}
