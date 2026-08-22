//! The persistent stream, the ring buffer, and the capture worker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::device::{device_name, pick_input_device};
use super::resample::{downmix_to_mono, normalize_peak, resample_to_16k};
use super::{AudioError, MAX_UTTERANCE, POST_ROLL, PRE_ROLL, TARGET_SAMPLE_RATE};

/// Seconds of native-rate audio the ring can hold. One second is the stated
/// minimum; two costs 768 kB at 48 kHz stereo and means a worker stalled by a
/// GC-sized hiccup elsewhere in the process still loses nothing.
const RING_SECONDS: usize = 2;

/// Frames of conversion scratch owned by the data callback. Preallocated at
/// stream build time so the callback never allocates; buffers larger than this
/// are processed in several passes.
const CALLBACK_SCRATCH_FRAMES: usize = 2048;

/// How often the worker wakes to drain the ring when nothing is happening.
const WORKER_POLL: Duration = Duration::from_millis(5);

/// How often the stream thread wakes to re-check the shutdown flag. It is also
/// unparked on shutdown, so this is only a backstop.
const STREAM_PARK: Duration = Duration::from_millis(100);

/// Minimum gap between overrun log lines, so a permanently overrunning stream
/// does not fill the log.
const OVERRUN_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum gap between mic-level readings — 30 Hz, which is as fast as anything
/// watching a VU meter can perceive and slow enough that the whole feature is
/// noise on the event bus rather than load.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);

/// How many level readings may sit in the channel before they are dropped.
///
/// Bounded and tiny on purpose. A level is only true for the 33 ms it
/// describes, so a backlog is worthless by definition — and a nobody-is-draining
/// case (the receiver was never taken) must not grow without limit.
const LEVEL_QUEUE: usize = 8;

/// The quietest level the meter distinguishes, in dBFS. Below it the bars are
/// at rest: a room with nobody speaking in it sits around −55 dBFS.
const LEVEL_FLOOR_DB: f64 = -60.0;

/// The loudest, in dBFS. Speech at a sane recording level is −30 to −15 dBFS
/// RMS, so this puts ordinary dictation in the upper half of the bars without
/// pinning them.
const LEVEL_CEILING_DB: f64 = -12.0;

/// How the engine should behave for its whole life. Set once at start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    /// Also hand back the untouched native-rate interleaved audio with every
    /// utterance. Off in production: it roughly quadruples the memory a note
    /// costs, and only a diagnostic harness has any use for it.
    pub retain_native: bool,
}

/// The raw capture behind an utterance, before downmix, resample or normalize.
///
/// Only present when [`CaptureOptions::retain_native`] was set. It exists so a
/// bad recording can be pinned on the device or on our processing without
/// guessing.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeAudio {
    /// Interleaved samples exactly as the device delivered them.
    pub samples: Vec<f32>,
    /// The device's rate, not [`TARGET_SAMPLE_RATE`].
    pub sample_rate: u32,
    /// The device's channel count.
    pub channels: u16,
}

/// One finished utterance, ready for whisper.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// Mono samples at [`Utterance::sample_rate`], pre-roll first.
    pub samples: Vec<f32>,
    /// Always [`TARGET_SAMPLE_RATE`]; carried explicitly so downstream code
    /// never has to assume.
    pub sample_rate: u32,
    /// Gain peak normalization applied. `1.0` means none was applied — either
    /// the utterance was already at target or it was too quiet to be anything
    /// but noise.
    pub normalize_gain: f32,
    /// See [`NativeAudio`]. `None` unless the engine was started with
    /// [`CaptureOptions::retain_native`].
    pub native: Option<NativeAudio>,
    /// True when [`MAX_UTTERANCE`] ended this chunk rather than the user.
    ///
    /// Recording did *not* stop: the next utterance is already being captured,
    /// starting from this one's last 400 ms. Consumers use the flag for two
    /// things — say so to the user, and never pair the chunk with a key
    /// release, because the user has not released anything yet (see
    /// [`session`](crate::session)).
    pub capped: bool,
    /// True when this chunk was *opened* by a cap split rather than by a press.
    ///
    /// The mirror image of [`Utterance::capped`], which says how a chunk
    /// **ended**. A continuation is seeded from the pre-roll mid-speech, so no
    /// begin cue was ever played at its start — which is why the session
    /// trims its own cue out of press-started clips only, and leaves a
    /// continuation's head exactly as captured (splits lose nothing).
    pub continued: bool,
}

impl Utterance {
    /// How long the utterance is.
    #[must_use]
    pub fn duration(&self) -> Duration {
        if self.sample_rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.samples.len() as f64 / f64::from(self.sample_rate))
    }
}

/// Whether the engine is still capturing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineStatus {
    /// The stream is live.
    Running,
    /// The stream failed. Nothing reopens it by itself — the engine says so
    /// rather than pretending to record. An explicit
    /// [`AudioEngine::reconnect`] is the only way back, and a successful one
    /// clears this.
    Dead {
        /// What cpal reported.
        reason: String,
    },
}

/// What one [`AudioEngine::reconnect`] did.
///
/// Three outcomes and no fourth, and none of them is an `Err`: a device change
/// that fails is an ordinary answer the user has to be told in full — *which*
/// device is capturing now — and a `Result` would let a caller report the
/// failure without reporting what is running instead.
#[derive(Debug)]
pub enum Reconnect {
    /// The requested device is open and capturing.
    Switched {
        /// The device the request actually resolved to.
        device: String,
    },
    /// The request failed; the previous device is open again and capturing.
    Reverted {
        /// What is capturing now.
        device: String,
        /// Why the requested device was refused.
        error: AudioError,
    },
    /// The request failed and so did putting the old one back: **nothing is
    /// capturing**. The engine reports [`EngineStatus::Dead`] from here on.
    Lost {
        /// Why the requested device was refused.
        error: AudioError,
        /// Why the previous one could not be reopened either.
        revert_error: AudioError,
    },
}

/// What the hotkey layer asks the worker to do.
enum Command {
    Begin,
    End,
}

/// State shared between the engine handle, the stream thread and the worker,
/// for the engine's **whole life** — a reconnect replaces the stream, not this.
struct Shared {
    /// Set on Drop; both threads poll it.
    shutdown: AtomicBool,
    /// Samples dropped because the ring was full. Written only by the data
    /// callback, with `Relaxed` — it is a diagnostic, not a synchronisation
    /// point, and ordering it would put a fence in the callback for nothing.
    overruns: AtomicU64,
    /// Set by the cpal error callback.
    dead: AtomicBool,
    /// Filled in by the worker once it has seen the error. Never locked from a
    /// cpal callback.
    death_reason: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            overruns: AtomicU64::new(0),
            dead: AtomicBool::new(false),
            death_reason: Mutex::new(None),
        }
    }
}

/// Facts about the opened stream, reported back from the stream thread.
struct StreamFacts {
    device: String,
    sample_rate: u32,
    channels: u16,
}

/// A freshly opened stream, on its way to the **running** worker.
///
/// Everything a device change moves travels in here: the new ring's consumer,
/// the shape of what comes out of it, and that stream's error channel. The
/// utterance channel is deliberately not in this list — see
/// [`AudioEngine::reconnect`].
struct StreamSwap {
    consumer: HeapCons<f32>,
    sample_rate: u32,
    channels: u16,
    errors: Receiver<cpal::Error>,
}

/// One opened stream: the thread that owns it, and the flag that ends it.
///
/// Per stream rather than per engine, which is the whole reason a reconnect is
/// possible: the engine-wide `shutdown` in [`Shared`] would take the worker
/// down with the stream.
struct StreamHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StreamHandle {
    /// End this stream and wait for its thread, which is where the
    /// `cpal::Stream` is dropped. Idempotent.
    ///
    /// Joining rather than detaching is the point: when this returns, the old
    /// stream is closed and its callbacks can no longer fire, so nothing that
    /// happens afterwards can be attributed to it.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.thread.take() {
            // The stream thread parks; wake it so the close is immediate rather
            // than up to STREAM_PARK late.
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

/// The persistent microphone pipeline.
///
/// Dropping the engine stops both threads; it does not block on anything longer
/// than [`STREAM_PARK`].
pub struct AudioEngine {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    /// Device changes, to the worker. See [`AudioEngine::reconnect`].
    swaps: Sender<StreamSwap>,
    facts: StreamFacts,
    /// What the engine was last *asked* for, which is not the same as what it
    /// opened: the pin is a name substring and the device it resolves to can
    /// change. Kept so a failed reconnect knows what to put back.
    requested: Option<String>,
    stream: StreamHandle,
    worker_thread: Option<JoinHandle<()>>,
    /// The mic-level feed, until somebody takes it. See
    /// [`AudioEngine::take_levels`].
    levels: Option<Receiver<f32>>,
}

impl AudioEngine {
    /// Open the microphone and start capturing.
    ///
    /// Returns the engine and the receiver finished utterances arrive on. The
    /// stream stays open for the engine's whole life — opening a device per
    /// keypress is forbidden, because the first few hundred milliseconds after a
    /// WASAPI open are unreliable and that is exactly the audio we care about.
    pub fn start(mic_substring: Option<&str>) -> Result<(Self, Receiver<Utterance>), AudioError> {
        Self::start_with_options(mic_substring, CaptureOptions::default())
    }

    /// As [`AudioEngine::start`], with diagnostics turned on.
    pub fn start_with_options(
        mic_substring: Option<&str>,
        options: CaptureOptions,
    ) -> Result<(Self, Receiver<Utterance>), AudioError> {
        let shared = Arc::new(Shared::new());

        let (stream, consumer, facts, errors) = spawn_stream(&shared, mic_substring)?;

        let (command_tx, command_rx) = mpsc::channel::<Command>();
        let (swap_tx, swap_rx) = mpsc::channel::<StreamSwap>();
        let (utterance_tx, utterance_rx) = mpsc::channel::<Utterance>();
        // Bounded, because a level nobody drained is worthless rather than
        // owed. See `LEVEL_QUEUE`.
        let (level_tx, level_rx) = mpsc::sync_channel::<f32>(LEVEL_QUEUE);

        let worker_shared = Arc::clone(&shared);
        let config = WorkerConfig {
            sample_rate: facts.sample_rate,
            channels: usize::from(facts.channels.max(1)),
            options,
        };
        let worker_thread = thread::Builder::new()
            .name("sotone-audio-worker".to_owned())
            .spawn(move || {
                worker_main(
                    &worker_shared,
                    consumer,
                    config,
                    &command_rx,
                    errors,
                    &swap_rx,
                    &utterance_tx,
                    level_tx,
                );
            })
            .map_err(AudioError::ThreadSpawn)?;

        tracing::info!(
            device = %facts.device,
            sample_rate = facts.sample_rate,
            channels = facts.channels,
            "microphone stream open"
        );

        Ok((
            Self {
                shared,
                commands: command_tx,
                swaps: swap_tx,
                facts,
                requested: mic_substring.map(str::to_owned),
                stream,
                worker_thread: Some(worker_thread),
                levels: Some(level_rx),
            },
            utterance_rx,
        ))
    }

    /// Take the mic-level feed: a number between 0.0 and 1.0 per reading, at
    /// most one every [`LEVEL_INTERVAL`], and **only while a recording is
    /// live** (for the overlay pill's VU bars).
    ///
    /// Once, by contract — the second call returns `None`. One consumer keeps
    /// the contract honest: a level is a fact about *now*, and two readers with
    /// different drain rates would each see a different half of it.
    ///
    /// **Invariant 5.** The cpal data callback gains nothing at all from this:
    /// it still converts and enqueues, exactly as it did. The arithmetic
    /// happens on the capture worker thread, over samples it has already
    /// drained for its own reasons, and is throttled where it is *computed* so
    /// the cost is bounded rather than the channel's.
    ///
    /// Silence between recordings is deliberate: an idle pill costs nothing,
    /// and a stream of zeroes would be a heartbeat nobody asked for.
    pub fn take_levels(&mut self) -> Option<Receiver<f32>> {
        self.levels.take()
    }

    /// Point the engine at a different microphone, without restarting anything
    /// above it.
    ///
    /// **The utterance channel does not change.** The `Receiver` handed back by
    /// [`AudioEngine::start`] belongs to the session worker, which owns the
    /// transcriber and the draft; re-creating it would mean tearing down the
    /// session to change a device. So the worker thread survives a reconnect
    /// and only its *input* — the ring, the device's rate and channel count,
    /// and the assembly state that depends on them — is replaced, in one
    /// message it applies between passes.
    ///
    /// **Nothing recorded is dropped.** Anything the worker was mid-assembly on
    /// is delivered under the configuration it was captured with, before the
    /// swap takes effect. In practice there is never anything there: the shell
    /// refuses a device change while a recording is live, which is the rule
    /// that actually protects the clip.
    ///
    /// **Invariant 5.** Every step here — closing the old stream, opening the
    /// new one, waiting for it — runs on the *calling* (control) thread and on
    /// the new stream thread. No cpal callback does any of it; the data
    /// callback still only converts and enqueues, and the error callback still
    /// only flags and forwards.
    ///
    /// [`CaptureOptions`] survive: the worker carries its own copy across the
    /// swap, so a diagnostic harness does not silently lose `retain_native`
    /// when the device changes.
    ///
    /// The pre-roll starts empty and refills from the new device within
    /// [`PRE_ROLL`]: audio captured at the old device's rate
    /// cannot be carried into a buffer measured in the new one's samples, and
    /// resampling history from a microphone the user just stopped using would
    /// be worse than a moment of silence.
    ///
    /// Never returns `Err`: see [`Reconnect`].
    pub fn reconnect(&mut self, mic_substring: Option<&str>) -> Reconnect {
        let previous = self.requested.clone();
        tracing::info!(
            from = %self.facts.device,
            to = ?mic_substring,
            "switching the microphone"
        );
        match self.open(mic_substring) {
            Ok(device) => Reconnect::Switched { device },
            Err(error) => {
                tracing::warn!(error = %error, "could not open the chosen microphone; putting the previous one back");
                match self.open(previous.as_deref()) {
                    Ok(device) => Reconnect::Reverted { device, error },
                    Err(revert_error) => {
                        // Truthful rather than quiet: `status()` now reports
                        // Dead, so the app says it cannot hear rather than
                        // waiting for a key press that can never produce audio.
                        self.mark_dead(&revert_error);
                        Reconnect::Lost {
                            error,
                            revert_error,
                        }
                    }
                }
            }
        }
    }

    /// Close the current stream and open one on `substring`, handing the result
    /// to the running worker.
    ///
    /// **Old stream first, new stream second**, deliberately. Opening the new
    /// one while the old is still live would make a failed switch free — but a
    /// host that hands out a device exclusively (ALSA `hw:`, WASAPI exclusive)
    /// would then refuse the *same* device the user is already on, turning a
    /// no-op change into a failure. One device open at a time is the honest
    /// order; the cost is the reopen in [`AudioEngine::reconnect`]'s revert.
    fn open(&mut self, substring: Option<&str>) -> Result<String, AudioError> {
        self.stream.stop();
        // A death reported by the device we have just closed is not news about
        // the one we are about to open. Cleared after the join, so the old
        // error callback provably cannot set it again afterwards.
        self.clear_death();

        let (stream, consumer, facts, errors) = spawn_stream(&self.shared, substring)?;
        self.swaps
            .send(StreamSwap {
                consumer,
                sample_rate: facts.sample_rate,
                channels: facts.channels,
                errors,
            })
            .map_err(|_| AudioError::WorkerGone)?;

        tracing::info!(
            device = %facts.device,
            sample_rate = facts.sample_rate,
            channels = facts.channels,
            "microphone stream reopened"
        );
        self.stream = stream;
        self.requested = substring.map(str::to_owned);
        let device = facts.device.clone();
        self.facts = facts;
        Ok(device)
    }

    /// Forget a death reported by a stream that is already closed.
    fn clear_death(&self) {
        self.shared.dead.store(false, Ordering::Release);
        match self.shared.death_reason.lock() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => *poisoned.into_inner() = None,
        }
    }

    /// Say, in the one place that answers "is it capturing", that it is not.
    ///
    /// [`AudioEngine::device_name`] deliberately still names the last device
    /// that was open — it describes a stream, and there is no stream to
    /// describe. [`AudioEngine::status`] is the honest answer, and the caller
    /// has the [`Reconnect`] value that says the same thing.
    fn mark_dead(&self, why: &AudioError) {
        let reason = format!("no microphone could be opened: {why}");
        tracing::error!(%reason, "the engine has no stream");
        match self.shared.death_reason.lock() {
            Ok(mut guard) => *guard = Some(reason),
            Err(poisoned) => *poisoned.into_inner() = Some(reason),
        }
        // After the reason, so a reader that sees `dead` also sees why.
        self.shared.dead.store(true, Ordering::Release);
    }

    /// Start recording an utterance, pre-roll included.
    ///
    /// Cheap and non-blocking by contract: the rdev path drains its own
    /// queue into this, and nothing on that path may ever wait.
    pub fn begin_utterance(&self) {
        // A failed send means the worker is gone, which Drop is already
        // handling; there is nothing useful to do about it here.
        let _ = self.commands.send(Command::Begin);
    }

    /// Stop recording after the post-roll elapses. Same contract as
    /// [`AudioEngine::begin_utterance`].
    pub fn end_utterance(&self) {
        let _ = self.commands.send(Command::End);
    }

    /// The microphone actually in use, which may not be the pinned one.
    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.facts.device
    }

    /// The device's native sample rate. Utterances are always
    /// [`TARGET_SAMPLE_RATE`]; this is for diagnostics.
    #[must_use]
    pub fn input_sample_rate(&self) -> u32 {
        self.facts.sample_rate
    }

    /// The device's native channel count.
    #[must_use]
    pub fn input_channels(&self) -> u16 {
        self.facts.channels
    }

    /// Samples dropped because the worker could not keep up.
    #[must_use]
    pub fn overruns(&self) -> u64 {
        self.shared.overruns.load(Ordering::Relaxed)
    }

    /// Whether the stream is still alive.
    #[must_use]
    pub fn status(&self) -> EngineStatus {
        if !self.shared.dead.load(Ordering::Acquire) {
            return EngineStatus::Running;
        }
        let reason = match self.shared.death_reason.lock() {
            Ok(guard) => guard.clone(),
            // A poisoned lock means the worker panicked while holding it. The
            // stored reason is still readable and still true.
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        EngineStatus::Dead {
            reason: reason.unwrap_or_else(|| "the audio device stopped".to_owned()),
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.stream.stop();
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Stream thread
// ---------------------------------------------------------------------------

/// Open a device on a thread of its own and wait for the verdict.
///
/// Shared by startup and by every reconnect, so a swapped stream is opened by
/// exactly the same code that opened the first one — including the rule that
/// the `cpal::Stream` never leaves the thread that built it.
///
/// Blocking (a device open is tens to hundreds of milliseconds). Control-thread
/// work only; nothing on a callback path may call this.
fn spawn_stream(
    shared: &Arc<Shared>,
    substring: Option<&str>,
) -> Result<
    (
        StreamHandle,
        HeapCons<f32>,
        StreamFacts,
        Receiver<cpal::Error>,
    ),
    AudioError,
> {
    let (init_tx, init_rx) = mpsc::channel::<Result<(HeapCons<f32>, StreamFacts), AudioError>>();
    let (error_tx, error_rx) = mpsc::channel::<cpal::Error>();
    let stop = Arc::new(AtomicBool::new(false));

    let substring = substring.map(str::to_owned);
    let stream_shared = Arc::clone(shared);
    let stream_stop = Arc::clone(&stop);
    let thread = thread::Builder::new()
        .name("sotone-audio-stream".to_owned())
        .spawn(move || {
            stream_thread_main(
                &stream_shared,
                &stream_stop,
                substring.as_deref(),
                &init_tx,
                error_tx,
            );
        })
        .map_err(AudioError::ThreadSpawn)?;

    // The stream thread owns the device, so it also decides the ring size; the
    // consumer comes back here to be handed to the worker.
    let (consumer, facts) = match init_rx.recv() {
        Ok(Ok(ready)) => ready,
        Ok(Err(err)) => {
            let _ = thread.join();
            return Err(err);
        }
        Err(_) => {
            let _ = thread.join();
            return Err(AudioError::StreamThreadDied);
        }
    };

    Ok((
        StreamHandle {
            stop,
            thread: Some(thread),
        },
        consumer,
        facts,
        error_rx,
    ))
}

fn stream_thread_main(
    shared: &Arc<Shared>,
    stop: &AtomicBool,
    substring: Option<&str>,
    init_tx: &Sender<Result<(HeapCons<f32>, StreamFacts), AudioError>>,
    error_tx: Sender<cpal::Error>,
) {
    match open_stream(shared, substring, error_tx) {
        Ok((stream, consumer, facts)) => {
            if init_tx.send(Ok((consumer, facts))).is_err() {
                // Caller gave up waiting; drop the stream and go.
                return;
            }
            // Two flags: this stream's own (a device change closes one stream,
            // not the engine) and the engine's (Drop closes everything).
            while !stop.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire) {
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
    substring: Option<&str>,
    error_tx: Sender<cpal::Error>,
) -> Result<(Stream, HeapCons<f32>, StreamFacts), AudioError> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, substring)?;
    let name = device_name(&device);

    // The device's own default config, whatever it is. WASAPI is shared-mode
    // only in cpal, so opening here never takes the microphone away from
    // anything else the user is running.
    let supported = device
        .default_input_config()
        .map_err(|source| AudioError::DefaultConfig {
            device: name.clone(),
            source,
        })?;

    let channels = supported.channels();
    let sample_rate = supported.sample_rate();
    let format = supported.sample_format();
    if channels == 0 {
        return Err(AudioError::ImpossibleChannelCount {
            device: name,
            channels,
        });
    }

    let capacity = sample_rate as usize * usize::from(channels) * RING_SECONDS;
    let (producer, consumer) = HeapRb::<f32>::new(capacity.max(1)).split();

    let config = supported.config();
    let stream = build_stream(&device, &name, &config, format, producer, shared, error_tx)?;
    stream.play().map_err(|source| AudioError::PlayStream {
        device: name.clone(),
        source,
    })?;

    Ok((
        stream,
        consumer,
        StreamFacts {
            device: name,
            sample_rate,
            channels,
        },
    ))
}

/// Dispatch on the runtime sample format to a single generic stream builder.
#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &Device,
    name: &str,
    config: &StreamConfig,
    format: SampleFormat,
    producer: HeapProd<f32>,
    shared: &Arc<Shared>,
    error_tx: Sender<cpal::Error>,
) -> Result<Stream, AudioError> {
    match format {
        SampleFormat::F32 => {
            build_stream_as::<f32>(device, name, config, producer, shared, error_tx)
        }
        SampleFormat::F64 => {
            build_stream_as::<f64>(device, name, config, producer, shared, error_tx)
        }
        SampleFormat::I8 => build_stream_as::<i8>(device, name, config, producer, shared, error_tx),
        SampleFormat::I16 => {
            build_stream_as::<i16>(device, name, config, producer, shared, error_tx)
        }
        SampleFormat::I32 => {
            build_stream_as::<i32>(device, name, config, producer, shared, error_tx)
        }
        SampleFormat::U8 => build_stream_as::<u8>(device, name, config, producer, shared, error_tx),
        SampleFormat::U16 => {
            build_stream_as::<u16>(device, name, config, producer, shared, error_tx)
        }
        SampleFormat::U32 => {
            build_stream_as::<u32>(device, name, config, producer, shared, error_tx)
        }
        other => Err(AudioError::UnsupportedSampleFormat {
            device: name.to_owned(),
            format: other,
        }),
    }
}

fn build_stream_as<T>(
    device: &Device,
    name: &str,
    config: &StreamConfig,
    mut producer: HeapProd<f32>,
    shared: &Arc<Shared>,
    error_tx: Sender<cpal::Error>,
) -> Result<Stream, AudioError>
where
    T: SizedSample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    let data_shared = Arc::clone(shared);
    let error_shared = Arc::clone(shared);

    // Sized in whole frames so every chunk boundary is also a frame boundary,
    // which keeps the ring's interleaving intact.
    let mut scratch = vec![0.0f32; CALLBACK_SCRATCH_FRAMES * usize::from(config.channels.max(1))];

    let stream = device
        .build_input_stream::<T, _, _>(
            *config,
            // ---- THE DATA CALLBACK ----------------------------------------
            // Runs on a cpal-owned thread the OS audio engine waits on, so it
            // must never allocate, lock, log, or panic. It converts to f32 in
            // preallocated scratch and enqueues. That is the whole body.
            // `chunks` cannot panic (scratch is non-empty), `get_mut` is the
            // non-panicking slice form, and `zip` truncates rather than
            // indexing out of range.
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for chunk in data.chunks(scratch.len()) {
                    if let Some(converted) = scratch.get_mut(..chunk.len()) {
                        for (dst, src) in converted.iter_mut().zip(chunk) {
                            *dst = <f32 as cpal::Sample>::from_sample(*src);
                        }
                        enqueue(&mut producer, converted, &data_shared.overruns);
                    }
                }
            },
            // ---- THE ERROR CALLBACK ---------------------------------------
            // Fires on unplug, device-disable, xrun or a default-device change.
            // Also not our thread, so it flags and forwards; the worker does
            // the logging.
            move |err: cpal::Error| {
                error_shared.dead.store(true, Ordering::Release);
                let _ = error_tx.send(err);
            },
            None,
        )
        .map_err(|source| AudioError::BuildStream {
            device: name.to_owned(),
            source,
        })?;

    Ok(stream)
}

/// The only thing the data callback is allowed to do with samples.
///
/// Wait-free: `vacant_len` is a pair of atomic loads, `push_slice` is a memcpy
/// into storage allocated at startup, and the counter is a relaxed add.
///
/// The push is all-or-nothing on purpose. A partial push would leave half an
/// interleaved frame in the ring, and every frame after it would be
/// channel-swapped for the rest of the session — a silent, permanent corruption
/// far worse than dropping one buffer.
#[inline]
fn enqueue(producer: &mut HeapProd<f32>, samples: &[f32], overruns: &AtomicU64) {
    if producer.vacant_len() >= samples.len() {
        producer.push_slice(samples);
    } else {
        overruns.fetch_add(samples.len() as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Capture worker
// ---------------------------------------------------------------------------

/// The shape of the worker's input, decided at stream-open time and replaced
/// wholesale when the device changes.
#[derive(Debug, Clone, Copy)]
struct WorkerConfig {
    sample_rate: u32,
    /// Always at least 1.
    channels: usize,
    options: CaptureOptions,
}

/// One drain pass's buffer: 100 ms of native audio, whole frames only.
///
/// Reallocated on a device change rather than reused, because both of its
/// dimensions belong to the device.
fn drain_scratch(config: WorkerConfig) -> Vec<f32> {
    vec![0.0f32; (config.sample_rate as usize / 10).max(1) * config.channels]
}

#[allow(clippy::too_many_arguments)]
fn worker_main(
    shared: &Arc<Shared>,
    mut consumer: HeapCons<f32>,
    mut config: WorkerConfig,
    commands: &Receiver<Command>,
    mut errors: Receiver<cpal::Error>,
    swaps: &Receiver<StreamSwap>,
    utterances: &Sender<Utterance>,
    levels: SyncSender<f32>,
) {
    let mut state = CaptureState::new(config.sample_rate, config.channels);
    let mut scratch = drain_scratch(config);
    // The mic level for the overlay's VU bars. It lives here, on the
    // thread that has already touched every sample, rather than anywhere near
    // the cpal callback (invariant 5).
    let mut meter = LevelMeter::new(levels);

    let mut reported_overruns = 0u64;
    let mut last_overrun_log = Instant::now() - OVERRUN_LOG_INTERVAL;

    while !shared.shutdown.load(Ordering::Acquire) {
        // Commands first: a begin that arrived while we were asleep must be in
        // effect before this pass's audio is classified, or the first chunk
        // after a keypress lands in the pre-roll instead of the utterance.
        let mut finished: Vec<Chunk> = Vec::new();
        while let Ok(command) = commands.try_recv() {
            match command {
                // A begin can close a chunk of its own (a toggle restart inside
                // the previous stop's post-roll), and it lands in `finished`
                // before the recording it opens can produce anything — so the
                // delivery order still matches the order the user spoke in.
                Command::Begin => finished.extend(state.begin()),
                Command::End => finished.extend(state.end()),
            }
        }

        // Then a device change, before this pass reads any audio: the ring
        // being drained below must be the one the rate and channel count
        // describe, or a pass would resample the new device's samples with the
        // old device's arithmetic.
        while let Ok(swap) = swaps.try_recv() {
            // Whatever was mid-assembly is handed over first, measured with the
            // configuration it was captured under. Dropping audio the user
            // already spoke is never the right answer — and the
            // deliveries have to happen before `config` moves.
            let mut stranded = std::mem::take(&mut finished);
            stranded.extend(state.abandon());
            for chunk in stranded {
                deliver(chunk, &config, utterances);
            }

            consumer = swap.consumer;
            // The old stream's error channel goes with the old stream: an error
            // from a device that is already closed is not news about this one.
            errors = swap.errors;
            config = WorkerConfig {
                sample_rate: swap.sample_rate,
                channels: usize::from(swap.channels.max(1)),
                options: config.options,
            };
            scratch = drain_scratch(config);
            // Fresh, so the pre-roll refills from the device now in use.
            state = CaptureState::new(config.sample_rate, config.channels);
            tracing::info!(
                sample_rate = config.sample_rate,
                channels = config.channels,
                "capture worker switched to the new microphone"
            );
        }

        // Then stream errors, so a dead device does not leave an utterance
        // waiting forever for a post-roll that will never arrive.
        let mut died = false;
        while let Ok(err) = errors.try_recv() {
            died = true;
            let reason = err.to_string();
            tracing::error!(%reason, "microphone stream failed; capture has stopped");
            match shared.death_reason.lock() {
                Ok(mut guard) => *guard = Some(reason),
                Err(poisoned) => *poisoned.into_inner() = Some(reason),
            }
        }
        if died {
            finished.extend(state.abandon());
        }

        // Then the audio itself.
        let mut drained_any = false;
        loop {
            let available = consumer.occupied_len().min(scratch.len());
            // Only whole frames, so `CaptureState` always sees interleaved data
            // it can split by channel. A remainder waits for the next pass.
            let available = available - available % config.channels;
            if available == 0 {
                break;
            }
            let Some(target) = scratch.get_mut(..available) else {
                break;
            };
            let count = consumer.pop_slice(target);
            drained_any = true;
            // Before the push, because the phase in force *now* is the one that
            // classifies these samples: after a release `push` may have moved
            // into the post-roll, which the user is no longer speaking into.
            meter.observe(&target[..count], state.is_recording(), Instant::now());
            // Several chunks can come out of one pass only in the degenerate
            // case of a cap shorter than the pass; in production the cap is
            // two minutes and the pass is 100 ms, so this is at most one.
            state.push(&target[..count], &mut finished);
        }

        for chunk in finished {
            deliver(chunk, &config, utterances);
        }

        let overruns = shared.overruns.load(Ordering::Relaxed);
        if overruns > reported_overruns && last_overrun_log.elapsed() >= OVERRUN_LOG_INTERVAL {
            tracing::warn!(
                dropped_samples = overruns,
                "microphone ring buffer overran; audio was dropped"
            );
            reported_overruns = overruns;
            last_overrun_log = Instant::now();
        }

        if !drained_any {
            thread::sleep(WORKER_POLL);
        }
    }
}

/// Native interleaved samples → 16 kHz mono → the caller's channel.
///
/// The order is downmix → resample → normalize and must stay that way.
/// Normalizing first would change which channel the downmix reads as loudest;
/// normalizing between the two would be undone by the resampler's own passband
/// ripple pushing a sample back over the target peak.
fn deliver(chunk: Chunk, config: &WorkerConfig, utterances: &Sender<Utterance>) {
    let Chunk {
        raw,
        capped,
        continued,
    } = chunk;
    let channels = u16::try_from(config.channels).unwrap_or(1);
    let mono = downmix_to_mono(&raw, channels);
    match resample_to_16k(&mono, config.sample_rate) {
        Ok(mut samples) => {
            let normalize_gain = normalize_peak(&mut samples);
            let native = if config.options.retain_native {
                Some(NativeAudio {
                    samples: raw,
                    sample_rate: config.sample_rate,
                    channels,
                })
            } else {
                // Dropped here rather than carried: the native buffer is the
                // largest thing in the pipeline.
                None
            };
            let utterance = Utterance {
                samples,
                sample_rate: TARGET_SAMPLE_RATE,
                normalize_gain,
                native,
                capped,
                continued,
            };
            if utterances.send(utterance).is_err() {
                tracing::debug!("nobody is listening for utterances; discarding");
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "could not prepare the utterance for transcription");
        }
    }
}

// ---------------------------------------------------------------------------
// The mic level
//
// One number, 0.0–1.0, for the overlay pill's VU bars. It is *decoration*: the
// recording state is `sotone://recording` and nothing here may ever be used to
// infer it. Which is also why the feed goes silent between
// recordings rather than reporting a floor.
// ---------------------------------------------------------------------------

/// Mean square → the 0.0–1.0 the bars ride on.
///
/// **The whole mapping, in one place.** dB-mapped rather than linear: linear
/// RMS spends nine tenths of the bar height on the top 20 dB and leaves ordinary
/// speech twitching just off the floor, which looks like a broken meter. Reading
/// the mean square directly saves a square root — `20·log10(rms)` and
/// `10·log10(rms²)` are the same number.
///
/// Silence, and anything that is not a finite positive number, is 0.0: a NaN
/// sample from a misbehaving driver must produce a bar at rest, never a NaN
/// that JavaScript would turn into an unstyled element.
fn level_from_mean_square(mean_square: f64) -> f32 {
    if !mean_square.is_finite() || mean_square <= 0.0 {
        return 0.0;
    }
    let db = 10.0 * mean_square.log10();
    let level = (db - LEVEL_FLOOR_DB) / (LEVEL_CEILING_DB - LEVEL_FLOOR_DB);
    if !level.is_finite() {
        return 0.0;
    }
    level.clamp(0.0, 1.0) as f32
}

/// Accumulates power over drained samples and emits a level at most every
/// [`LEVEL_INTERVAL`].
///
/// **The throttle is at the computation site, not the emission site**: the sum
/// is a multiply-add per sample on a thread that is already reading every one
/// of them, and the logarithm — the only interesting arithmetic here — happens
/// 30 times a second whatever the device's buffer size is.
struct LevelMeter {
    sink: SyncSender<f32>,
    /// Σ x² since the last reading. `f64` so a two-minute utterance at 48 kHz
    /// cannot lose precision to accumulation.
    sum_squares: f64,
    samples: usize,
    last_sent: Instant,
}

impl LevelMeter {
    fn new(sink: SyncSender<f32>) -> Self {
        Self {
            sink,
            sum_squares: 0.0,
            samples: 0,
            last_sent: Instant::now(),
        }
    }

    /// Fold one drained buffer in, and send a level if one is due.
    ///
    /// `recording` is the capture phase, passed in rather than inferred: the
    /// pill's state comes from `sotone://recording`, and a meter that decided
    /// for itself when a recording was live would be a second answer to a
    /// question that already has one.
    fn observe(&mut self, samples: &[f32], recording: bool, now: Instant) {
        if !recording {
            // Nothing is sent between recordings, and nothing is carried across
            // one: the first reading of an utterance describes that utterance.
            self.sum_squares = 0.0;
            self.samples = 0;
            return;
        }
        for sample in samples {
            let sample = f64::from(*sample);
            self.sum_squares += sample * sample;
        }
        self.samples += samples.len();

        if self.samples == 0 || now.duration_since(self.last_sent) < LEVEL_INTERVAL {
            return;
        }
        let level = level_from_mean_square(self.sum_squares / self.samples as f64);
        self.sum_squares = 0.0;
        self.samples = 0;
        self.last_sent = now;
        // Dropped rather than queued when the channel is full: see LEVEL_QUEUE.
        let _ = self.sink.try_send(level);
    }
}

// ---------------------------------------------------------------------------
// Pre-roll and utterance assembly (no cpal, no device, fully testable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Recording,
    PostRoll,
}

/// One finished chunk of native interleaved audio, on its way to [`deliver`].
#[derive(Debug, Clone, PartialEq)]
struct Chunk {
    raw: Vec<f32>,
    /// The cap ended it, not the user — see [`CaptureState::split`].
    capped: bool,
    /// A previous cap split opened it, not a press — see
    /// [`CaptureState::split`]. Independent of `capped`: a recording long
    /// enough to split twice produces a chunk that is both.
    continued: bool,
}

/// Rolling pre-roll plus utterance assembly, in native interleaved samples.
///
/// All lengths are in *samples*, always whole multiples of the channel count,
/// so the buffer never loses interleaving alignment.
struct CaptureState {
    pre_roll: VecDeque<f32>,
    pre_roll_cap: usize,
    post_roll_len: usize,
    post_roll_left: usize,
    utterance: Vec<f32>,
    utterance_cap: usize,
    phase: Phase,
    /// Whether the chunk currently being assembled was opened by a split.
    /// Owned by the state rather than passed around so that every exit — a
    /// release, a further split, a device death — reports the same answer.
    continued: bool,
}

impl CaptureState {
    fn new(sample_rate: u32, channels: usize) -> Self {
        let frames = |d: Duration| (d.as_secs_f64() * f64::from(sample_rate)).round() as usize;
        Self::with_frame_limits(
            channels,
            frames(PRE_ROLL),
            frames(POST_ROLL),
            frames(MAX_UTTERANCE),
        )
    }

    fn with_frame_limits(
        channels: usize,
        pre_roll_frames: usize,
        post_roll_frames: usize,
        max_frames: usize,
    ) -> Self {
        let channels = channels.max(1);
        Self {
            pre_roll: VecDeque::with_capacity(pre_roll_frames * channels),
            pre_roll_cap: pre_roll_frames * channels,
            post_roll_len: post_roll_frames * channels,
            post_roll_left: 0,
            utterance: Vec::new(),
            utterance_cap: max_frames * channels,
            phase: Phase::Idle,
            continued: false,
        }
    }

    /// Take the chunk being assembled, tagged with how it ended and how it
    /// began.
    fn take_chunk(&mut self, capped: bool) -> Chunk {
        Chunk {
            raw: std::mem::take(&mut self.utterance),
            capped,
            continued: self.continued,
        }
    }

    /// Feed a frame-aligned chunk of native samples.
    ///
    /// Appends every utterance this chunk completed to `finished`. That is
    /// usually none or one; it is a list because the cap can close a chunk
    /// mid-pass and, with a cap shorter than one pass, more than once.
    fn push(&mut self, samples: &[f32], finished: &mut Vec<Chunk>) {
        match self.phase {
            // The pre-roll tracks the most recent audio unconditionally,
            // including while recording — otherwise the utterance right after
            // this one would start with stale audio from before the previous
            // one. `record` does its own remembering, chunk by chunk, so the
            // pre-roll is exact at the split point.
            Phase::Idle => self.remember(samples),
            Phase::Recording => self.record(samples, finished),
            Phase::PostRoll => {
                let take = self.post_roll_left.min(samples.len());
                if let Some(head) = samples.get(..take) {
                    // Deliberately not cap-limited: this utterance is already
                    // ending, and clipping its last 200 ms to honour a
                    // two-minute ceiling would cut a word off for nothing.
                    self.utterance.extend_from_slice(head);
                }
                self.post_roll_left -= take;
                if self.post_roll_left == 0 {
                    self.phase = Phase::Idle;
                    let chunk = self.take_chunk(false);
                    finished.push(chunk);
                }
                self.remember(samples);
            }
        }
    }

    /// Is the user speaking into this chunk right now?
    ///
    /// The post-roll is deliberately not "recording": the key is already up and
    /// the indicator has already gone idle, so a meter that kept moving through
    /// it would disagree with the pill for 200 ms.
    const fn is_recording(&self) -> bool {
        matches!(self.phase, Phase::Recording)
    }

    /// Open a new utterance, handing back whatever chunk this press closed.
    ///
    /// `None` for the ordinary press out of [`Phase::Idle`]. `Some` when the
    /// press lands inside the previous stop's post-roll: toggle mode makes that
    /// easy to do (stop, then start again within 200 ms), and such a press
    /// used to be simply ignored. The recording the user then made never
    /// existed, their stop had nothing to end, and its `ReleaseInfo` went on to
    /// stamp somebody else's line one behind for the rest of the session.
    ///
    /// So the post-roll is short-circuited instead: the old chunk closes here
    /// with however much tail it collected, and the new one opens exactly as it
    /// would from `Idle`. Padding the tail out to the full 200 ms would add
    /// nothing — the audio it would contain is the audio the user is speaking
    /// into the *next* utterance, which already keeps it via the pre-roll.
    fn begin(&mut self) -> Option<Chunk> {
        let early = match self.phase {
            Phase::Idle => None,
            // A second press without a release is the hotkey layer's problem,
            // not ours; ignoring it keeps the utterance we are already
            // building. Unreachable from the shell's `Machine`, which refuses a
            // press while it believes a recording runs — hence `warn`: this is
            // the tripwire for that ever ceasing to be true, and both silences
            // on this path were once unlogged.
            Phase::Recording => {
                tracing::warn!(
                    "a recording start arrived while one was already running; ignoring it"
                );
                return None;
            }
            Phase::PostRoll => {
                self.post_roll_left = 0;
                // Emitted even if it is empty (only a zero pre-roll can do
                // that): this chunk is what the user's stop pairs its release
                // with, and swallowing it would recreate the orphan.
                Some(self.take_chunk(false))
            }
        };
        self.utterance.clear();
        // The pre-roll is live through the post-roll too (`push`), so after an
        // early close it holds the tail of the chunk just handed over: the same
        // deliberate overlap an Idle-gap press gets, and no sample falls
        // between the two.
        self.utterance.extend(self.pre_roll.iter().copied());
        self.phase = Phase::Recording;
        // A press starts this chunk, so a begin cue is about to sound over it.
        self.continued = false;
        early
    }

    /// Begin the post-roll. Returns the utterance immediately in the degenerate
    /// case of a zero-length post-roll, so it can never be stranded.
    fn end(&mut self) -> Option<Chunk> {
        if self.phase != Phase::Recording {
            return None;
        }
        if self.post_roll_len == 0 {
            self.phase = Phase::Idle;
            return Some(self.take_chunk(false));
        }
        self.phase = Phase::PostRoll;
        self.post_roll_left = self.post_roll_len;
        None
    }

    /// Give up on an in-flight utterance, handing back whatever was captured.
    /// Used when the device dies mid-recording: truncated speech is worth more
    /// to the user than silently dropped speech.
    fn abandon(&mut self) -> Option<Chunk> {
        if self.phase == Phase::Idle {
            return None;
        }
        self.phase = Phase::Idle;
        self.post_roll_left = 0;
        let chunk = self.take_chunk(false);
        (!chunk.raw.is_empty()).then_some(chunk)
    }

    /// Live audio while the key is down: append, and split rather than drop
    /// when the cap is reached.
    ///
    /// The split is the whole point of the cap. The chunk is
    /// closed *at* the cap sample with no post-roll (capture is not stopping),
    /// and the next chunk opens immediately with the standard pre-roll — which
    /// is the tail of the chunk just closed, so nothing spoken across the seam
    /// is lost. A word may straddle it and appear in both lines; for raw notes
    /// that is the right trade against losing it.
    ///
    /// The phase never passes through [`Phase::Idle`], so the user's eventual
    /// release still ends the *last* chunk normally.
    fn record(&mut self, samples: &[f32], finished: &mut Vec<Chunk>) {
        if self.utterance_cap == 0 {
            // No cap configured at all (only reachable from a test): recording
            // is uncapped rather than instantly and endlessly split.
            self.utterance.extend_from_slice(samples);
            self.remember(samples);
            return;
        }
        let mut rest = samples;
        loop {
            let room = self.utterance_cap.saturating_sub(self.utterance.len());
            if room >= rest.len() {
                self.utterance.extend_from_slice(rest);
                self.remember(rest);
                return;
            }

            // `room < rest.len()`, so this split is in range, and it lands on a
            // frame boundary: the cap and the pre-roll are both whole frames.
            let (head, tail) = rest.split_at(room);
            self.utterance.extend_from_slice(head);
            self.remember(head);
            self.split(finished);
            rest = tail;

            if self.utterance.len() >= self.utterance_cap {
                // Only reachable if the pre-roll alone fills the cap, which no
                // real configuration does. Keep the audio in one over-long
                // chunk rather than spinning here emitting empty ones.
                self.utterance.extend_from_slice(rest);
                self.remember(rest);
                return;
            }
        }
    }

    /// Close the chunk at the cap and open the next one, still recording.
    fn split(&mut self, finished: &mut Vec<Chunk>) {
        let chunk = self.take_chunk(true);
        finished.push(chunk);
        // Info, not warn: this is designed behaviour now, and a stuck key
        // reaches it every cap period.
        tracing::info!(
            limit_seconds = MAX_UTTERANCE.as_secs(),
            "recording hit its length cap; splitting into a new line and carrying on"
        );
        self.utterance.extend(self.pre_roll.iter().copied());
        // The chunk now opening was not started by a press: no begin cue will
        // sound over its head, so nothing downstream may trim one out of it.
        self.continued = true;
    }

    fn remember(&mut self, samples: &[f32]) {
        if self.pre_roll_cap == 0 {
            return;
        }
        // Only the tail can survive: anything older than `pre_roll_cap` samples
        // would be evicted immediately anyway.
        let tail_start = samples.len().saturating_sub(self.pre_roll_cap);
        if let Some(tail) = samples.get(tail_start..) {
            self.pre_roll.extend(tail.iter().copied());
        }
        while self.pre_roll.len() > self.pre_roll_cap {
            self.pre_roll.pop_front();
        }
    }

    #[cfg(test)]
    fn pre_roll_len(&self) -> usize {
        self.pre_roll.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a monotonically increasing ramp, so any reordering, gap or
    /// duplication in the assembled utterance is visible as a broken sequence.
    struct Ramp {
        next: f32,
    }

    impl Ramp {
        fn new() -> Self {
            Self { next: 0.0 }
        }

        fn chunk(&mut self, len: usize) -> Vec<f32> {
            (0..len)
                .map(|_| {
                    let value = self.next;
                    self.next += 1.0;
                    value
                })
                .collect()
        }
    }

    fn ramp_range(start: usize, end: usize) -> Vec<f32> {
        (start..end).map(|v| v as f32).collect()
    }

    /// Feed a chunk and hand back every utterance it completed.
    fn feed(state: &mut CaptureState, samples: &[f32]) -> Vec<Chunk> {
        let mut finished = Vec::new();
        state.push(samples, &mut finished);
        finished
    }

    /// A press that is expected to close nothing. Where a press *can* close a
    /// chunk — inside a post-roll — the test names the return value instead, so
    /// a chunk can never be discarded here by accident.
    fn begin(state: &mut CaptureState) {
        assert!(
            state.begin().is_none(),
            "this press had no in-flight chunk to close"
        );
    }

    /// The single utterance a chunk was expected to complete.
    fn feed_one(state: &mut CaptureState, samples: &[f32], what: &str) -> Chunk {
        let mut finished = feed(state, samples);
        assert_eq!(finished.len(), 1, "{what}");
        finished.remove(0)
    }

    /// Every ramp sample between two consecutive chunks is in one of them. An
    /// overlap is the pre-roll doing its job; a gap is audio the user spoke and
    /// Sotone lost.
    fn assert_no_gap(first: &[f32], second: &[f32]) {
        let first_end = *first.last().expect("the first chunk has audio") + 1.0;
        let second_start = *second.first().expect("the second chunk has audio");
        assert!(
            second_start <= first_end,
            "samples {first_end}..{second_start} fell through the seam"
        );
    }

    #[test]
    fn idle_audio_only_feeds_the_pre_roll() {
        let mut state = CaptureState::with_frame_limits(1, 4, 2, 100);
        let mut ramp = Ramp::new();
        assert!(feed(&mut state, &ramp.chunk(20)).is_empty());
        assert_eq!(state.pre_roll_len(), 4);
    }

    #[test]
    fn the_pre_roll_keeps_only_the_most_recent_400ms() {
        // 1000 Hz mono makes the arithmetic legible: 400 ms is 400 samples.
        let mut state = CaptureState::new(1_000, 1);
        let mut ramp = Ramp::new();
        for _ in 0..20 {
            assert!(feed(&mut state, &ramp.chunk(100)).is_empty());
        }
        assert_eq!(state.pre_roll_len(), 400);

        // Beginning now must produce exactly the last 400 samples fed, in order.
        begin(&mut state);
        assert_eq!(state.utterance, ramp_range(1_600, 2_000));
    }

    #[test]
    fn an_utterance_is_pre_roll_then_live_then_post_roll_with_no_gaps() {
        let mut state = CaptureState::new(1_000, 1);
        let mut ramp = Ramp::new();

        // 1000 samples of ambient audio; the last 400 become the pre-roll.
        assert!(feed(&mut state, &ramp.chunk(1_000)).is_empty());

        begin(&mut state);
        assert!(feed(&mut state, &ramp.chunk(300)).is_empty());
        assert!(state.end().is_none());

        // The chunk that straddles the end of the post-roll: 200 samples are
        // taken, the remaining 300 are not.
        let finished = feed_one(&mut state, &ramp.chunk(500), "post-roll must complete");

        // 400 pre-roll + 300 live + 200 post-roll, contiguous.
        assert_eq!(finished.raw, ramp_range(600, 1_500));
        assert_eq!(finished.raw.len(), 900);
        assert!(!finished.capped, "the user ended this one");
    }

    #[test]
    fn audio_after_the_post_roll_still_refreshes_the_pre_roll() {
        let mut state = CaptureState::new(1_000, 1);
        let mut ramp = Ramp::new();
        begin(&mut state);
        assert!(state.end().is_none());
        assert_eq!(feed(&mut state, &ramp.chunk(1_000)).len(), 1);

        // The trailing 800 samples were not part of the utterance, but they are
        // the freshest audio, so the next utterance must start from them.
        begin(&mut state);
        assert_eq!(state.utterance, ramp_range(600, 1_000));
    }

    #[test]
    fn a_second_begin_does_not_restart_the_utterance() {
        let mut state = CaptureState::with_frame_limits(1, 0, 2, 100);
        let mut ramp = Ramp::new();
        begin(&mut state);
        feed(&mut state, &ramp.chunk(10));
        // Refused, and refused *quietly* as far as the audio goes: the helper
        // asserts nothing was closed, and the recording carries straight on.
        begin(&mut state);
        assert_eq!(state.phase, Phase::Recording);
        feed(&mut state, &ramp.chunk(10));
        assert!(state.end().is_none());
        let finished = feed_one(&mut state, &ramp.chunk(2), "post-roll must complete");
        assert_eq!(finished.raw, ramp_range(0, 22));
    }

    #[test]
    fn a_begin_inside_the_post_roll_closes_it_instead_of_vanishing() {
        // A toggle bug found in use: stop, then start again
        // before the 200 ms post-roll has run out. That second press used to be
        // ignored outright — no chunk, no log — so the user's next stop had
        // nothing to end and its release went on to stamp the following line.
        let mut state = CaptureState::with_frame_limits(1, 4, 8, 100);
        let mut ramp = Ramp::new();

        // Ambient audio, then the first recording.
        assert!(feed(&mut state, &ramp.chunk(10)).is_empty());
        begin(&mut state);
        assert!(feed(&mut state, &ramp.chunk(10)).is_empty());
        assert!(state.end().is_none());

        // Three samples into an eight-sample post-roll, the user starts again.
        assert!(feed(&mut state, &ramp.chunk(3)).is_empty());
        let first = state.begin().expect("the press must close the post-roll");
        assert_eq!(
            first.raw,
            ramp_range(6, 23),
            "pre-roll, live audio, and the tail collected up to the press"
        );
        assert!(!first.capped, "the user ended this one");
        assert!(!first.continued, "a press opened it");

        // And the second recording exists at all, which is the whole bug.
        assert_eq!(state.phase, Phase::Recording);
        assert!(feed(&mut state, &ramp.chunk(10)).is_empty());
        assert!(state.end().is_none());
        let second = feed_one(&mut state, &ramp.chunk(8), "post-roll must complete");
        assert_eq!(second.raw, ramp_range(19, 41));
        assert!(!second.continued, "a press opened this one too");

        assert_no_gap(&first.raw, &second.raw);
    }

    #[test]
    fn a_post_roll_cut_short_by_a_begin_loses_no_samples() {
        // No pre-roll, so the two chunks must concatenate to exactly the audio
        // fed — the overlap-free version of the seam check above.
        let mut state = CaptureState::with_frame_limits(1, 0, 6, 100);
        let mut ramp = Ramp::new();
        begin(&mut state);
        assert!(feed(&mut state, &ramp.chunk(5)).is_empty());
        assert!(state.end().is_none());
        assert!(feed(&mut state, &ramp.chunk(2)).is_empty());

        let first = state.begin().expect("the press must close the post-roll");
        assert!(feed(&mut state, &ramp.chunk(5)).is_empty());
        assert!(state.end().is_none());
        let second = feed_one(&mut state, &ramp.chunk(6), "post-roll must complete");

        let heard: Vec<f32> = first.raw.iter().chain(second.raw.iter()).copied().collect();
        assert_eq!(heard, ramp_range(0, 18));
    }

    #[test]
    fn a_zero_post_roll_leaves_no_chunk_for_the_next_begin_to_close() {
        // The degenerate `end()` arm hands the chunk back and goes idle, so a
        // press right behind it is an ordinary press with nothing to cut short.
        let mut state = CaptureState::with_frame_limits(1, 2, 0, 100);
        let mut ramp = Ramp::new();
        begin(&mut state);
        feed(&mut state, &ramp.chunk(3));
        let first = state.end().expect("a zero post-roll must finish on end");
        assert_eq!(first.raw, ramp_range(0, 3));

        begin(&mut state);
        assert_eq!(state.phase, Phase::Recording);
        let second = state.end().expect("a zero post-roll must finish on end");
        assert_eq!(second.raw, ramp_range(1, 3));
    }

    #[test]
    fn end_without_begin_is_ignored() {
        let mut state = CaptureState::new(1_000, 1);
        assert!(state.end().is_none());
        let mut ramp = Ramp::new();
        assert!(feed(&mut state, &ramp.chunk(1_000)).is_empty());
    }

    #[test]
    fn the_cap_splits_the_recording_instead_of_dropping_audio() {
        // 20 frames of cap, 4 of pre-roll, 2 of post-roll: the same shape as
        // 120 s / 400 ms / 200 ms, small enough to read.
        let mut state = CaptureState::with_frame_limits(1, 4, 2, 20);
        let mut ramp = Ramp::new();
        begin(&mut state);

        // 30 samples of speech over one cap: the first 20 close a chunk, the
        // rest carry on into the next one.
        let first = feed_one(&mut state, &ramp.chunk(30), "the cap must close a chunk");
        assert!(first.capped, "the cap ended this chunk, not the user");
        assert!(!first.continued, "a press opened this chunk");
        assert_eq!(first.raw, ramp_range(0, 20));

        // The seam: the new chunk opens with the pre-roll, which is the last 4
        // samples of the chunk just delivered, then continues with 16..30. No
        // sample is missing; the 4-sample overlap is the deliberate pre-roll.
        assert_eq!(state.utterance, ramp_range(16, 30));
        assert_eq!(state.phase, Phase::Recording, "capture must not stop");

        // And the user's eventual release ends this chunk normally, post-roll
        // and all.
        assert!(state.end().is_none());
        let second = feed_one(&mut state, &ramp.chunk(10), "post-roll must complete");
        assert!(!second.capped, "the user ended the second chunk");
        // The seam chunk was opened by the split, not by a press: no begin cue
        // ever sounded over its head, so the session must not trim one out.
        assert!(second.continued, "the split opened the second chunk");
        assert_eq!(second.raw, ramp_range(16, 32));

        // And the next press starts a press-opened chunk again.
        begin(&mut state);
        assert!(state.end().is_none());
        let third = feed_one(&mut state, &ramp.chunk(4), "post-roll must complete");
        assert!(!third.continued, "a press opened the third chunk");
    }

    #[test]
    fn nothing_is_lost_across_a_split_that_lands_mid_chunk() {
        // Cap 10, no pre-roll, so the two chunks must concatenate to exactly
        // the audio fed — the overlap-free version of the seam test.
        let mut state = CaptureState::with_frame_limits(1, 0, 1, 10);
        let mut ramp = Ramp::new();
        begin(&mut state);

        // 25 samples through a 10-sample cap: two full chunks, 5 left live.
        let finished = feed(&mut state, &ramp.chunk(25));
        assert_eq!(finished.len(), 2);
        assert!(finished.iter().all(|chunk| chunk.capped));

        let mut heard: Vec<f32> = finished.iter().flat_map(|c| c.raw.clone()).collect();
        heard.extend(state.utterance.iter().copied());
        assert_eq!(heard, ramp_range(0, 25));
    }

    #[test]
    fn the_cap_counts_the_pre_roll_too() {
        let mut state = CaptureState::with_frame_limits(1, 8, 1, 10);
        let mut ramp = Ramp::new();
        feed(&mut state, &ramp.chunk(8));
        begin(&mut state);
        assert_eq!(state.utterance.len(), 8);
        // Only 2 samples of room left, so the cap fires almost at once.
        let finished = feed(&mut state, &ramp.chunk(100));
        assert!(!finished.is_empty());
        assert_eq!(finished[0].raw, ramp_range(0, 10));
    }

    #[test]
    fn stereo_buffers_stay_frame_aligned() {
        // 4 frames of pre-roll on 2 channels is 8 samples.
        let mut state = CaptureState::with_frame_limits(2, 4, 2, 100);

        // Interleave a left ramp with a constant right channel.
        let samples: Vec<f32> = (0..20).flat_map(|i| [i as f32, -1.0]).collect();
        assert!(feed(&mut state, &samples).is_empty());
        assert_eq!(state.pre_roll_len(), 8);

        begin(&mut state);
        // Left channel first in every frame: even indices carry the ramp.
        assert_eq!(
            state.utterance,
            vec![16.0, -1.0, 17.0, -1.0, 18.0, -1.0, 19.0, -1.0]
        );
    }

    #[test]
    fn a_zero_length_post_roll_finishes_immediately() {
        let mut state = CaptureState::with_frame_limits(1, 2, 0, 100);
        let mut ramp = Ramp::new();
        feed(&mut state, &ramp.chunk(2));
        begin(&mut state);
        feed(&mut state, &ramp.chunk(3));
        let finished = state.end().expect("a zero post-roll must finish on end");
        assert_eq!(finished.raw, ramp_range(0, 5));
    }

    #[test]
    fn abandoning_returns_the_partial_utterance() {
        let mut state = CaptureState::with_frame_limits(1, 2, 4, 100);
        let mut ramp = Ramp::new();
        begin(&mut state);
        feed(&mut state, &ramp.chunk(6));
        let salvaged = state.abandon().expect("partial audio must be handed back");
        assert_eq!(salvaged.raw, ramp_range(0, 6));
        assert!(!salvaged.capped);
        // And the state is idle again, not stuck mid-utterance.
        assert!(state.abandon().is_none());
        assert!(feed(&mut state, &ramp.chunk(4)).is_empty());
    }

    #[test]
    fn the_overrun_counter_counts_samples_the_ring_could_not_take() {
        let (mut producer, mut consumer) = HeapRb::<f32>::new(8).split();
        let overruns = AtomicU64::new(0);

        enqueue(&mut producer, &[1.0, 2.0, 3.0, 4.0], &overruns);
        assert_eq!(overruns.load(Ordering::Relaxed), 0);

        // Only 4 slots are free, so this 8-sample push is dropped whole.
        enqueue(&mut producer, &[0.0; 8], &overruns);
        assert_eq!(overruns.load(Ordering::Relaxed), 8);

        // The all-or-nothing rule means the ring still holds exactly the first
        // push, undisturbed — no half-written frame.
        let mut out = [0.0f32; 8];
        let count = consumer.pop_slice(&mut out);
        assert_eq!(count, 4);
        assert_eq!(&out[..4], &[1.0, 2.0, 3.0, 4.0]);

        // With room again, pushes resume.
        enqueue(&mut producer, &[5.0; 8], &overruns);
        assert_eq!(overruns.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn utterance_duration_matches_its_sample_count() {
        let utterance = Utterance {
            samples: vec![0.0; TARGET_SAMPLE_RATE as usize / 2],
            sample_rate: TARGET_SAMPLE_RATE,
            normalize_gain: 1.0,
            native: None,
            capped: false,
            continued: false,
        };
        assert_eq!(utterance.duration(), Duration::from_millis(500));
    }

    // -- The mic level ------------------------------------------------------

    /// Mean square for a given dBFS RMS, so the tests read in the units the
    /// constants are written in.
    fn mean_square_at(db: f64) -> f64 {
        10f64.powf(db / 10.0)
    }

    #[test]
    fn silence_is_a_level_of_zero_and_full_scale_is_one() {
        assert_eq!(level_from_mean_square(0.0), 0.0);
        // A full-scale sine or square is well above the ceiling; the bars pin
        // rather than overshoot.
        assert_eq!(level_from_mean_square(1.0), 1.0);
    }

    #[test]
    fn the_level_curve_is_linear_in_decibels_between_floor_and_ceiling() {
        assert_eq!(level_from_mean_square(mean_square_at(LEVEL_FLOOR_DB)), 0.0);
        assert_eq!(
            level_from_mean_square(mean_square_at(LEVEL_CEILING_DB)),
            1.0
        );

        // The midpoint of the range in dB is the midpoint of the bar.
        let middle = (LEVEL_FLOOR_DB + LEVEL_CEILING_DB) / 2.0;
        let level = level_from_mean_square(mean_square_at(middle));
        assert!((level - 0.5).abs() < 1e-5, "{level}");

        // Below the floor is rest, not a negative bar.
        assert_eq!(level_from_mean_square(mean_square_at(-90.0)), 0.0);
    }

    #[test]
    fn the_level_never_decreases_as_the_room_gets_louder() {
        let mut previous = -1.0f32;
        for db in (-80..=0).step_by(4) {
            let level = level_from_mean_square(mean_square_at(f64::from(db)));
            assert!(level >= previous, "{db} dB went backwards");
            previous = level;
        }
    }

    #[test]
    fn a_broken_driver_cannot_produce_a_broken_level() {
        // A NaN in the buffer must reach the window as a bar at rest, never as
        // a NaN that would leave the element unstyled.
        assert_eq!(level_from_mean_square(f64::NAN), 0.0);
        assert_eq!(level_from_mean_square(f64::INFINITY), 0.0);
        assert_eq!(level_from_mean_square(-1.0), 0.0);
    }

    #[test]
    fn the_meter_says_nothing_at_all_while_no_recording_is_live() {
        let (tx, rx) = mpsc::sync_channel::<f32>(LEVEL_QUEUE);
        let mut meter = LevelMeter::new(tx);
        let start = Instant::now();
        for step in 0..20 {
            let now = start + LEVEL_INTERVAL * step;
            meter.observe(&[0.5; 512], false, now);
        }
        assert!(
            rx.try_recv().is_err(),
            "an idle pill must cost nothing on the wire"
        );
    }

    #[test]
    fn the_meter_reports_at_most_once_per_interval() {
        let (tx, rx) = mpsc::sync_channel::<f32>(LEVEL_QUEUE);
        let mut meter = LevelMeter::new(tx);
        let start = Instant::now();

        // Ten buffers inside one interval: at most one reading comes out.
        for step in 0..10 {
            meter.observe(&[0.2; 256], true, start + Duration::from_millis(step));
        }
        assert!(rx.try_recv().is_err(), "the throttle let a burst through");

        // One interval later, exactly one.
        meter.observe(&[0.2; 256], true, start + LEVEL_INTERVAL);
        let level = rx.try_recv().expect("a reading was due");
        assert!((0.0..=1.0).contains(&level), "{level}");
        assert!(rx.try_recv().is_err(), "one reading per interval, not two");
    }

    #[test]
    fn a_recording_starts_from_its_own_audio_and_not_the_last_ones() {
        let (tx, rx) = mpsc::sync_channel::<f32>(LEVEL_QUEUE);
        let mut meter = LevelMeter::new(tx);
        let start = Instant::now();

        // Loud, then the recording ends before a reading was due.
        meter.observe(&[0.9; 4_096], true, start);
        meter.observe(&[0.9; 4_096], false, start + Duration::from_millis(5));
        // A new, quiet recording: the reading must describe *it*.
        meter.observe(&[0.001; 4_096], true, start + LEVEL_INTERVAL);
        meter.observe(&[0.001; 4_096], true, start + LEVEL_INTERVAL * 2);

        let level = rx.try_recv().expect("a reading was due");
        assert!(level < 0.3, "the previous recording leaked in: {level}");
    }

    #[test]
    fn a_full_channel_drops_readings_instead_of_growing() {
        let (tx, _rx) = mpsc::sync_channel::<f32>(LEVEL_QUEUE);
        let mut meter = LevelMeter::new(tx);
        let start = Instant::now();
        // Nobody drains: this must stay bounded and must not block the worker.
        for step in 1..200 {
            meter.observe(&[0.4; 128], true, start + LEVEL_INTERVAL * step);
        }
    }

    #[test]
    fn the_post_roll_is_not_part_of_the_level() {
        let mut state = CaptureState::with_frame_limits(1, 2, 4, 100);
        assert!(!state.is_recording());
        begin(&mut state);
        assert!(state.is_recording(), "a press opens the meter");
        assert!(state.end().is_none());
        assert!(
            !state.is_recording(),
            "the key is up: the indicator and the bars are already idle"
        );
    }

    #[test]
    fn native_retention_is_off_by_default() {
        // Production must not pay for the diagnostic copy.
        assert!(!CaptureOptions::default().retain_native);
    }

    #[test]
    fn the_production_cap_is_two_minutes_of_frames() {
        // The one place the specified number and the frame arithmetic meet.
        assert_eq!(MAX_UTTERANCE, Duration::from_secs(120));
        let state = CaptureState::new(48_000, 2);
        assert_eq!(state.utterance_cap, 120 * 48_000 * 2);
        assert_eq!(state.pre_roll_cap, 400 * 48 * 2);
    }
}
