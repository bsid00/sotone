//! The dictation loop: utterance in, one durable line out.
//!
//! This is the worker that sits between the audio engine and the draft store.
//! It owns the [`Transcriber`] and the [`Draft`], because both are blocking and
//! single-owner by design, and it reports everything it does on a
//! [`SessionEvent`] channel the frontend drains — a console runner or the
//! Tauri shell. Nothing here is Tauri-aware, and nothing here owns
//! the [`AudioEngine`](crate::audio::AudioEngine) or the hotkey listener: the
//! caller wires those and hands over the two receivers.
//!
//! ```text
//! hotkey consumer ──ReleaseInfo──┐
//!                                ├─→ SessionWorker ─→ whisper ─→ draft ─→ event
//! audio worker ────Utterance─────┘
//! ```
//!
//! # Why two channels and FIFO pairing
//!
//! A note's timestamp is pinned to the moment the key came **up**, not to the
//! moment the line was written, so that rapid-fire notes keep the order they
//! were spoken. The release time is captured inside the hook (see
//! [`hotkey`](crate::hotkey)) and travels here on its own channel, because the
//! audio it belongs to only arrives ~200 ms later, once the post-roll has been
//! captured. One utterance is one release, in order, so pairing is a queue.
//!
//! The exception is the length cap
//! ([`MAX_UTTERANCE`]), which fires while the key
//! is still down. The engine splits the recording there and flags the chunk
//! [`Utterance::capped`]; capture carries on. Such a chunk **never consumes a
//! release**: the user has not released anything, and taking the release meant
//! for the *next* chunk would mis-stamp every note after it for the rest of the
//! session. It is stamped `now()` instead, which is the split moment and
//! therefore the right `spoken_at` for that line, and the release the user
//! eventually produces pairs with the final, unflagged chunk. Deterministic, no
//! grace window.
//!
//! Nothing is ever dropped on that path: the capped chunk is announced, gated
//! and appended like any other. Dropping audio the user spoke is never the right
//! answer (invariant 4 in spirit — the draft is the user's work).
//!
//! # The silence gate
//!
//! Whisper hallucinates words out of silence — in testing, three seconds of
//! digital silence reliably transcribes as "you". A dictation logger that
//! invents a line when the user brushed the key is worse than one that misses a
//! line, so three cheap gates run before anything is appended:
//!
//! 1. **Hold gate** — the key was down for less than [`MIN_HOLD`]:
//!    utterances under ~400 ms are discarded.
//! 2. **Level gate** — the audio, *with peak normalization undone* and with the
//!    pre-roll and our own begin cue skipped, is quieter than
//!    [`SILENCE_FLOOR_DBFS`]. The skip is not an optimisation: the cue bleeds
//!    from the speakers into the microphone loudly enough to defeat the gate on
//!    its own. See [`CUE_CONTAMINATION`].
//! 3. **Empty-text gate** — whisper produced nothing but whitespace.
//!
//! # The cue trim
//!
//! When a begin cue actually played, that same head is not merely skipped by
//! the gate — it is cut off the clip before anything sees it, so whisper never
//! decodes our own tone and the stored wav plays back clean. Only
//! press-started utterances are trimmed, and only when a [`CuePlayer`] exists;
//! a cap-split continuation keeps every sample it was given. The head is
//! skipped exactly once, which is enforced by [`Clip`] rather than by a flag.
//!
//! Every skip is reported as [`SessionEvent::Skipped`] with its reason. A
//! silently eaten utterance would read to the user as "the app dropped my note",
//! which is the one impression this app cannot afford.
//!
//! # Who owns the draft
//!
//! The worker owns the active [`Draft`] handle, and it is the only thing that
//! does — a second handle on the same directory would interleave appends, which
//! is exactly what [`Draft`] refuses to defend against. So switching drafts is
//! a *command* to this worker ([`SessionCommand`]), not something the frontend
//! does behind its back, and discarding the active draft is executed here too:
//! the handle is provably dropped before the directory is renamed. Saving is a
//! command for the same reason plus one more — the render re-reads the log, the
//! write is atomic, and the guard hashes whatever is already at the path. None
//! of that may run on a thread that services input or IPC (invariant 5).
//!
//! There may be **no** active draft. Startup deliberately creates none:
//! eager creation would litter the outstanding-drafts list with empty
//! drafts nobody spoke into. The draft is created lazily, here, at the moment a
//! line actually needs somewhere to land — never on a round trip to the UI,
//! because a line must never wait on a window.
//!
//! # Threading
//!
//! The worker runs on its own thread and blocks freely: [`Transcriber::transcribe`]
//! takes as long as the model takes, and [`Draft::append_line`] fsyncs. Neither
//! may ever run on the `rdev` hook callback or the `cpal` data callback
//! (invariant 5), which is precisely why they live behind these channels.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset, Local};
use ulid::Ulid;

use crate::audio::{Utterance, MAX_UTTERANCE, TARGET_SAMPLE_RATE};
use crate::cue::{Cue, CuePlayer};
use crate::draft::{
    ClashChoice, Draft, DraftError, DraftStore, LineRecord, MoveOptions, NewLine, NoteMove,
    SaveMode, SaveOptions, SessionDividers,
};
use crate::template;
use crate::transcribe::{Language, Transcriber};

/// Shortest hold that counts as speech: utterances under ~400 ms are
/// discarded. Anything briefer is a brush against the key, and transcribing it
/// is how a note file fills up with hallucinated filler.
pub const MIN_HOLD: Duration = Duration::from_millis(400);

/// Level below which an utterance is treated as silence, in dBFS RMS measured
/// *before* our peak normalization.
///
/// Tuned by ear and by the loopback rig, not exposed as configuration: it is a
/// property of the "is this speech at all" question, not a user preference.
/// −50 dBFS sits well under a quiet voice on a far-field laptop microphone and
/// well above the noise floor of every device measured so far. Raise it only
/// with a recording that proves the old value was wrong.
pub const SILENCE_FLOOR_DBFS: f32 = -50.0;

/// How often the worker wakes to notice a shutdown request while no utterance is
/// arriving. Utterances themselves are still handled the moment they land.
const POLL: Duration = Duration::from_millis(100);

/// Audio the engine prepends to every utterance, from *before* the key went
/// down.
///
/// Mirrors `audio::PRE_ROLL`, which is private to that module — duplicated
/// rather than exported for the same reason [`draft`](crate::draft) duplicates
/// its `APP_DIR`: the audio module keeps its internals to itself. If that
/// constant ever changes, this one changes with it.
const PRE_ROLL: Duration = Duration::from_millis(400);

/// How long after the key goes down our own begin cue is still audible in the
/// capture.
///
/// The cue is 150 ms of tone; the rest is output latency before it starts and
/// speaker/room decay after it stops. Measured at −10…−22 dBFS in the loopback
/// artifacts, i.e. tens of dB above the silence floor — easily enough to make a
/// silent hold look like speech.
const CUE_CONTAMINATION: Duration = Duration::from_millis(450);

/// How much of a press-started clip is *cut away* when the begin cue really
/// played, so neither whisper nor the stored wav ever contains our own beep
/// (it was audible in playback before the trim existed).
///
/// Same reasoning and same value as [`CUE_CONTAMINATION`] — tone plus output
/// latency plus decay — but a separate constant because it is a separate
/// decision: that one only stops the gate *looking* at the head, this one
/// destroys it. **This is the tuning lever.** The intended flow is press →
/// beep → speak, so for that flow only bleed is lost; anyone who starts
/// speaking during the beep loses those words, and a report of clipped first
/// words is answered by lowering this number, not by redesigning the trim.
const CUE_TRIM: Duration = Duration::from_millis(450);

/// How long a release may wait in the pairing queue before it is treated as an
/// orphan rather than as somebody's timestamp.
///
/// Generous on purpose, and the reasoning is worth keeping: the wait that
/// matters is not the ~200 ms of post-roll between a release and its chunk, it
/// is *this worker's own pass*. Intake runs once per pass, ahead of a decode —
/// and any re-transcribes — that block the thread, so a perfectly legitimate
/// release sits here for as long as one model run takes. That run's input is
/// capped at [`MAX_UTTERANCE`], so this covers a backend running well over
/// twice as slow as realtime.
///
/// Sized against the worst legitimate wait rather than tuned for quick
/// detection, because the two errors are not symmetric: dropping a *good*
/// release does not merely mistime one line, it strands that line's real
/// release at the front of the queue and so creates exactly the one-behind
/// desync this guard exists to end.
const RELEASE_STALE_AFTER: Duration = Duration::from_secs(300);

/// Samples of pre-roll at [`TARGET_SAMPLE_RATE`].
const PRE_ROLL_SAMPLES: usize = samples_at_16k(PRE_ROLL);

/// Head of the utterance the level gate must not look at: pre-roll (before the
/// press) plus the cue that the press itself caused.
const CONTAMINATED_SAMPLES: usize = PRE_ROLL_SAMPLES + samples_at_16k(CUE_CONTAMINATION);

/// Head dropped from a press-started clip when the cue player exists: the
/// pre-roll (before the press, so before the beep) plus the cue itself.
const TRIMMED_SAMPLES: usize = PRE_ROLL_SAMPLES + samples_at_16k(CUE_TRIM);

/// Shortest slice worth measuring. Below this the RMS is a coin toss, so the
/// gate measures the whole clip instead — see [`speech_window`].
const MIN_MEASURED_SAMPLES: usize = samples_at_16k(Duration::from_millis(200));

/// Whole samples of `duration` at whisper's fixed input rate, which is the rate
/// every [`Utterance`] arrives at.
const fn samples_at_16k(duration: Duration) -> usize {
    // Integer arithmetic so these stay `const`: milliseconds × 16 samples/ms.
    duration.as_millis() as usize * (TARGET_SAMPLE_RATE as usize / 1_000)
}

/// What the hotkey side sends when the key comes up.
///
/// Built on the consumer thread, not in the hook: only `released_at` needs
/// hook-level precision, and it is carried straight through from
/// [`PttEvent::Released`](crate::hotkey::PttEvent::Released).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseInfo {
    /// When the key came up, as the hook saw it.
    pub released_at: SystemTime,
    /// How long the key was held. Measured on the consumer thread with
    /// `Instant`, which is monotonic — a wall-clock subtraction would be at the
    /// mercy of an NTP step in the middle of a note.
    pub held: Duration,
}

/// Why an utterance produced no line.
#[derive(Debug, Clone, PartialEq)]
pub enum SkipReason {
    /// The key was not held long enough to be speech.
    TooShort {
        /// How long it was actually held.
        held: Duration,
    },
    /// The audio was below the silence floor.
    TooQuiet {
        /// Measured level, dBFS RMS, normalization undone.
        level_dbfs: f32,
    },
    /// Whisper produced nothing but whitespace.
    NoSpeech,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { held } => write!(
                f,
                "held for {} ms, under the {} ms minimum",
                held.as_millis(),
                MIN_HOLD.as_millis()
            ),
            Self::TooQuiet { level_dbfs } => {
                write!(
                    f,
                    "silent ({level_dbfs:.1} dBFS, floor {SILENCE_FLOOR_DBFS:.0})"
                )
            }
            Self::NoSpeech => f.write_str("no words in the audio"),
        }
    }
}

/// What one [`SessionCommand::Save`] did.
///
/// Three outcomes and no fourth: the file was written, the file changed
/// underneath us, or something went wrong. In particular there is no "nothing to
/// save" — a save of an unchanged draft rewrites the same bytes, which is what
/// makes the button honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// The markdown is on disk and the draft is bound to that path forever.
    Saved {
        /// What was written.
        path: PathBuf,
        /// Bytes written.
        bytes: usize,
        /// Bullets rendered.
        lines: usize,
    },
    /// The file at `path` is not the one Sotone last wrote there, so **nothing
    /// was written** (invariant 4). Stop and offer overwrite or diff.
    ///
    /// Both texts travel so the window can show the difference without reading
    /// anything itself. This is a stop, not a failure, and the dialog is the
    /// whole of the signal.
    Conflict {
        /// The file that changed.
        path: PathBuf,
        /// What is on disk now, lossily decoded. Empty when the file could not
        /// be read at all: the dialog still has to be able to open.
        disk_text: String,
        /// What the save would have written. Exact, because rendering is
        /// deterministic.
        pending_markdown: String,
    },
    /// Anything else: no active draft, an unwritable directory, an unreadable
    /// log.
    Error {
        /// Human-readable, already flattened.
        message: String,
    },
}

/// Everything the worker has to say. One event per utterance, plus failures.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A line was transcribed and durably appended to the draft.
    LineAdded {
        /// The record exactly as it went into `lines.jsonl`. Boxed so the
        /// happy path does not make every other variant of this enum — which
        /// travels a channel — carry a record's worth of padding.
        record: Box<LineRecord>,
        /// The intake token this line grew out of, so the pending row it had
        /// on screen can be retired. `None` only for lines that
        /// never had one.
        token: Option<String>,
        /// Whether this is a [`SessionEvent::Held`] line finally reaching disk
        /// rather than a fresh decode.
        ///
        /// The distinction is not bookkeeping: a held line's words were spoken
        /// minutes ago, and revealing them on the overlay as if they had just
        /// been said would be a lie about *now*, while the user is looking at
        /// the thing under test.
        held_flush: bool,
    },
    /// An utterance was deliberately not turned into a line.
    Skipped {
        /// Which gate stopped it.
        reason: SkipReason,
        /// The intake token, when the skip happened late enough that a pending
        /// row already existed. `None` for the intake gates (too short, too
        /// quiet), which run before a row is ever announced — a skip must
        /// never flash a row into being.
        token: Option<String>,
    },
    /// An utterance was accepted and is waiting for the model.
    ///
    /// Capture never waits for transcription, so the queue between them is
    /// real; this is what makes it visible. The row it produces shows *what was
    /// heard* — a duration, never a guess at the words.
    Queued {
        /// Identifies this utterance until it becomes a line, is skipped, or is
        /// held. Not a line id: nothing is on disk yet.
        token: String,
        /// How much audio was captured, in seconds, after the cue trim.
        seconds: f32,
        /// The moment stamped at intake.
        spoken_at: DateTime<FixedOffset>,
    },
    /// The model has started on this utterance.
    Decoding {
        /// The token [`SessionEvent::Queued`] announced.
        token: String,
    },
    /// A transcribed line could **not** be written and is being kept in memory
    /// until it can be.
    ///
    /// Nothing is discarded because the disk refused: the words and the audio
    /// stay in the worker and are re-attempted before the next append, on the
    /// next save, on activation, and once at shutdown.
    Held {
        /// The token [`SessionEvent::Queued`] announced.
        token: String,
        /// The draft it is waiting for, or `None` when the draft itself could
        /// not be created — in which case it lands in whatever draft is active
        /// when the flush finally succeeds.
        draft_id: Option<String>,
        /// The words, which exist: only the write failed.
        text: String,
        /// How much audio it holds, in seconds.
        seconds: f32,
        /// When it was spoken.
        spoken_at: DateTime<FixedOffset>,
    },
    /// Worker-side detail for the debug log and nothing else.
    ///
    /// Separate from [`SessionEvent::Notice`] because the level *is* the
    /// routing: a notice reaches the user, this reaches the log. The
    /// model's own error text travels here — the red row in the transcript is
    /// the user's surface, and an error code in the footer is what the design
    /// rules out.
    Debug {
        /// Already flattened.
        message: String,
    },
    /// The recording hit the length cap and was split. Sent the moment the
    /// capped chunk arrives — before it is transcribed — because the user is
    /// still talking and needs to know now, not in a second's time. Recording
    /// has *not* stopped: the next chunk is already being captured.
    Capped {
        /// The cap that fired, for the message.
        cap: Duration,
    },
    /// The capture engine died; no further utterances will arrive. Reported by
    /// the caller through [`SessionWorker::report_engine_dead`], because the
    /// worker does not own the engine.
    EngineDead {
        /// What cpal said.
        reason: String,
    },
    /// The active draft changed: resumed, started, closed, or lazily created
    /// because a line needed somewhere to go. Emitted on *every* activation
    /// change, so the frontend never has to infer one.
    DraftChanged {
        /// The new active draft, or `None` when there is none.
        id: Option<String>,
        /// Its directory, for the readout.
        dir: Option<PathBuf>,
        /// Live lines it already holds — the number the next line follows.
        line_count: usize,
    },
    /// The active draft was discarded (or the attempt failed). Either way the
    /// handle is gone; on failure the draft is still on disk, just not active.
    DraftDiscarded {
        /// The draft that was asked about.
        id: String,
        /// Whether it reached `.trash/`.
        ok: bool,
        /// Why not, when it did not.
        message: Option<String>,
    },
    /// The active draft's transcript, folded and complete.
    ///
    /// A full snapshot rather than a delta, deliberately: a draft is tens of
    /// lines, one file read, and a snapshot keeps the frontend contract trivial
    /// — there is no way for the UI's idea of the transcript to drift from the
    /// log. Sent on every activation change and after anything that changes a
    /// line, including a newly spoken one.
    Transcript {
        /// The draft these lines belong to, so a snapshot that crosses a draft
        /// switch can be recognised as stale.
        draft_id: String,
        /// Every parseable line, with its edits folded in.
        lines: Vec<LineRecord>,
    },
    /// A save was attempted. Boxed for the same reason [`SessionEvent::LineAdded`]
    /// is: a conflict carries two whole documents, and every other variant of
    /// this enum would otherwise be that size on the channel.
    Saved {
        /// What happened.
        outcome: Box<SaveOutcome>,
    },
    /// One [`SessionCommand::SaveAll`] batch, as one outcome.
    ///
    /// Deliberately **not** N [`SessionEvent::Saved`]s: the window answers a
    /// save outcome with a dialog or a notice, and replaying that N times for
    /// one click would be N popups and N notices for a single act. One click,
    /// one event, one notice.
    ///
    /// A batch that found nothing to save still reports `saved: 0` — the
    /// button said it would do something, so silence would read as a failure.
    SavedAll {
        /// How many notes were written.
        saved: usize,
        /// How many dirty drafts the batch had nowhere to put.
        ///
        /// A draft with no project — or one naming a project that is not in the
        /// configuration, or has no folder chosen — **is not a note**: notes
        /// exist only within projects, so there is no file for a batch to
        /// write. Counted rather than reported as an error, because nothing went
        /// wrong, and counted rather than passed over in silence, because "saved
        /// 3 notes" while four dots are lit would read as a lie.
        skipped: usize,
        /// The notes a stale-file guard stopped, by path. Nothing was written
        /// for any of these (invariant 4); each is resolved one at a time
        /// through the ordinary Save, which is where the diff lives.
        conflicts: Vec<PathBuf>,
        /// Anything else that went wrong, already flattened, one per draft.
        errors: Vec<String>,
    },
    /// Something worth saying that is not a failure — currently only a
    /// re-transcribe that produced no words, which leaves the line as it was.
    Notice {
        /// Human-readable, already complete.
        message: String,
    },
    /// A rename reached the draft store.
    ///
    /// Two commands report through one event because the answer to both is the
    /// same one: re-list, so every label in the window follows the store, and
    /// say something only when something went wrong. A rename that worked
    /// needs no sentence — the new name is on screen, which says it better.
    Renamed {
        /// What went wrong, already flattened, one per draft. Empty on success.
        errors: Vec<String>,
    },
    /// A note was dropped into another project.
    ///
    /// Its own event rather than another [`SessionEvent::Renamed`], because a
    /// drop has an answer a rename never has: it can come back with a
    /// *question* ([`NoteMove::Clash`]), and a question needs the draft and the
    /// destination echoed so the window can put the same drop back together
    /// when the user answers it. Nothing was mutated when that variant arrives.
    NoteMoved {
        /// Which draft the drop was about.
        draft: String,
        /// Where it was headed, so the answer can be sent for the same target.
        project: Option<String>,
        /// What happened, or the sentence saying why nothing did.
        outcome: Result<NoteMove, String>,
    },
    /// Something the user asked for could not be done: a re-transcribe, a
    /// language the model refused, a save with no draft behind it.
    ///
    /// **No longer emitted by the live dictation path.** A
    /// transcribe failure there writes a failed line and a decode that cannot
    /// be written is held, because the audio must survive either way — the row
    /// is the surface, not a sentence in the footer.
    Error {
        /// Human-readable, already flattened through the error's `Display`
        /// chain so a frontend can show one string.
        message: String,
    },
}

/// One project's save context, as a batch needs it.
///
/// Everything a note of that project needs in order to be written: where its
/// folder is, what an unbound one would be called, what goes at the top of the
/// file, and whether a resumed sitting gets a divider. The batch carries one of
/// these per configured project because **a note saves through its own
/// project's rules** — a note of "Backlog" lands in Backlog's folder under
/// Backlog's template even when the user is dictating into "Ludo".
///
/// Built shell-side, never read from the configuration here: the worker has no
/// access to it by design, so the same command shape that carries a rename's
/// notes root carries a batch's projects.
#[derive(Debug, Clone)]
pub struct ProjectSaveContext {
    /// The project's name, as `meta.project` spells it. The batch matches a
    /// draft to its context on this, exactly.
    pub project: String,
    /// The project's notes folder. Relative bindings resolve against it and
    /// first saves land in it.
    pub notes_root: PathBuf,
    /// The project's filename template, **unexpanded**: it is expanded per
    /// draft, at that draft's own save moment, because it is only used for
    /// drafts that have never been saved and each of those needs its own name.
    pub filename_template: String,
    /// The project's header template, already expanded (expansion happens at
    /// the shell boundary, where the clock is).
    pub header: Option<String>,
    /// Whether resumed sittings get a `---`.
    pub dividers: bool,
}

/// What the frontend can ask the worker to do with the active draft.
///
/// Deliberately small, and deliberately one-way: everything the caller learns
/// back comes as a [`SessionEvent`], because the worker is busy transcribing
/// and must never be waited on.
#[derive(Debug)]
pub enum SessionCommand {
    /// Make this draft the active one. The previous handle is dropped and
    /// nothing else happens to it — a closed draft is simply an outstanding
    /// draft again. Boxed: a [`Draft`] is large next to the other variants.
    SetDraft(Box<Draft>),
    /// Drop the handle; the session goes back to having no active draft. The
    /// next line lazily creates a new one.
    CloseDraft,
    /// Drop the handle, then move the directory to `.trash/`. Executed here so
    /// the handle is provably gone before the rename.
    DiscardActive,
    /// Replace one line's text. Applies to the **active draft only**: the
    /// worker owns the one handle, and editing a draft it is not holding would
    /// mean a second handle on the same directory. Resuming the draft first is
    /// the flow.
    EditLine {
        /// Line ulid.
        id: String,
        /// What the line should now read.
        text: String,
    },
    /// Soft-delete or restore one line. Never a hard delete — the record and
    /// the wav stay on disk (invariant 4).
    SetDeleted {
        /// Line ulid.
        id: String,
        /// The state to move to.
        deleted: bool,
    },
    /// Move one line so it sits immediately after another, or to the top.
    ///
    /// Appended as a move record, never a rewrite: the order the lines were
    /// spoken in stays on disk (invariant 4).
    MoveLine {
        /// The line that moved.
        id: String,
        /// The line it now follows; `None` means the top of the transcript.
        after: Option<String>,
    },
    /// Move one line out of the active draft and into another one.
    ///
    /// One command per line, not a batch: the frontend confirms once and sends
    /// N of these in transcript order, which keeps this the sibling of
    /// [`SessionCommand::SetDeleted`] rather than a second, wider write path.
    ///
    /// The **source is always the active draft** — a selection exists only in
    /// the open transcript — so this carries only the destination. The worker
    /// opens that destination, appends, and drops it inside one call (the
    /// [`DraftSlot::rename_project`] precedent), and refuses outright when the
    /// target is the active draft: the one-handle rule must not depend on the
    /// window having filtered the chooser correctly.
    ///
    /// Append-only on both sides, and destination first (invariant 4): see
    /// [`DraftSlot::move_line_to`].
    MoveLineTo {
        /// The line to move, in the active draft.
        id: String,
        /// The draft it should land in. Never the active one.
        target_id: String,
        /// Whether this is the **first** line of the batch, and so the one the
        /// destination's session divider goes above.
        ///
        /// It travels in the command because nothing downstream can work it
        /// out: once the first line has landed, the destination's log tail is
        /// a line record whether it arrived a moment ago as part of this move
        /// or last week as part of another, and no record carries a batch.
        /// The window is the only party that knows where a confirm's N calls
        /// begin, so it says so — `true` on the first, `false` on the rest —
        /// and [`Draft::import_line`] still refuses to write a divider that
        /// would separate nothing.
        divide: bool,
    },
    /// Run the model over one line's stored audio again and keep the result.
    Retranscribe {
        /// Line ulid.
        id: String,
    },
    /// Render the **active draft** to markdown and write it.
    ///
    /// Routed here rather than done in the Tauri command because a save is
    /// blocking I/O on a handle only this worker holds: the render re-reads
    /// `lines.jsonl`, the write is atomic, and the guard hashes the file that is
    /// already there. None of that may happen on a thread that services input
    /// or IPC (invariant 5).
    Save {
        /// Where a *first* save goes. Ignored once the draft has a
        /// `saved_path`: a bound note is rewritten in place forever, and this
        /// carries no authority to move it.
        fallback_path: PathBuf,
        /// The governing project's notes folder.
        ///
        /// Two jobs: it resolves a `saved_path` stored *relative* to it, and a
        /// target inside it is stored relative in turn — which is what lets the
        /// user move the folder, re-point the project, and keep every note.
        /// `None` reproduces the older absolute-only behaviour.
        notes_root: Option<PathBuf>,
        /// Project to write into a draft that has none, on success only. A
        /// draft that already names a project is never reassigned by a save.
        adopt_project: Option<String>,
        /// The project's header template, **already expanded** (expansion
        /// happens in text mode at the shell boundary, where the clock is).
        header: Option<String>,
        /// Whether resumed sittings get a `---`.
        dividers: bool,
        /// `true` only ever as the answer to a conflict the user was shown.
        /// Everything else sends `false` and gets the guard.
        overwrite: bool,
    },
    /// Save every dirty note in the store, in one act.
    ///
    /// The case that earned it: the user deletes or moves a project's folder
    /// and wants every note back, which one-note-at-a-time makes into busywork.
    /// It shipped scoped to the *active* project; that boundary surprised
    /// people in real use, so the batch is now the whole store — "save all"
    /// as the words read.
    ///
    /// Three rules make this safe to hand a whole folder's worth of files to:
    ///
    /// * **Guarded, always.** There is no overwrite variant of this command by
    ///   design — a batch cannot be shown a diff, so it can never be answered
    ///   with "yes, discard what is there" (invariant 4). A note stopped by the
    ///   guard is reported and skipped; resolving it stays a per-note act
    ///   through the ordinary [`SessionCommand::Save`] and its dialog. One
    ///   project's conflict stops that note and nothing else.
    /// * **No adoption.** A draft is saved through the context of the project it
    ///   *already names*, and one whose project is absent from `projects` — or
    ///   which has none at all — is skipped rather than swept into somebody
    ///   else's folder. Filing notes en masse is how they end up under the
    ///   wrong thing, and the single Save is the deliberate, one-at-a-time
    ///   adoption path.
    /// * **One event.** The whole batch reports once, as
    ///   [`SessionEvent::SavedAll`].
    SaveAll {
        /// Every project the batch may write into, with its own folder and its
        /// own templates.
        ///
        /// Built on the shell side, exactly as [`SessionCommand::RenameActive`]
        /// and [`SessionCommand::SetDraftProject`] have their roots built there:
        /// **the worker never reads the configuration**, so anything a save
        /// needs to know about a project travels in the command that asks for
        /// it. A draft's project is looked up here by name; no entry means no
        /// save, and the batch reports it as `skipped`.
        projects: Vec<ProjectSaveContext>,
    },
    /// Rename the **active draft's** note file.
    ///
    /// Routed here for the reason every other draft mutation is: the worker
    /// owns the only [`Draft`] handle on the active draft, and a second handle
    /// opened over that directory would write `meta.json` from a copy that
    /// never saw this one's live state (the `save_all` rationale, and the
    /// one-handle rule). A draft that is *not* active is renamed by the
    /// control thread instead, through its own open→rename→drop — the discard
    /// split, exactly.
    RenameActive {
        /// The new name as the user typed it. Unsanitized on purpose: the
        /// store owns the filename rules, so there is one place that decides
        /// what lands on disk.
        name: String,
        /// The owning project's notes folder, for resolving the current
        /// binding and re-deriving the new one. `None` means an absolute
        /// binding or nothing at all.
        notes_root: Option<PathBuf>,
    },
    /// Move the **active draft** to another project — its file with it.
    ///
    /// Routed here for the one-handle rule, exactly as
    /// [`SessionCommand::RenameActive`] is: the worker owns the only [`Draft`]
    /// handle on the active draft, and a second handle opened over that
    /// directory would write `meta.json` from a copy that never saw this one's
    /// live state. A draft that is *not* active is moved by the control
    /// thread through its own open→mutate→drop — the discard split, again.
    ///
    /// The two roots are what makes this more than a name change: the binding
    /// is resolved against the old project's folder and re-derived against the
    /// new one, in one meta write, and the `.md` is moved into the
    /// new folder in the same act. See [`Draft::move_to_project`], which owns
    /// the whole story.
    SetDraftProject {
        /// The project the note now belongs to, or `None` for no project.
        project: Option<String>,
        /// The notes folder of the project it is leaving. `None` when it had
        /// none, or when that project is no longer in the config — in which
        /// case a relative binding is carried across as it is rather than
        /// refused, because moving a note *out* of the "not in your projects"
        /// group is the repair.
        old_root: Option<PathBuf>,
        /// The notes folder of the project it is joining, or `None` — which is
        /// the "no project" group, where there is no folder to move into and
        /// nothing on disk moves.
        new_root: Option<PathBuf>,
        /// What to do about a name already taken in the target folder. `Ask`
        /// on the drop itself; `KeepBoth` only ever as the user's answer to the
        /// question the drop came back with.
        clash: ClashChoice,
        /// The **new** project's header template, already expanded — needed
        /// only if the note has to be re-rendered because its file is gone.
        header: Option<String>,
        /// The **new** project's divider setting, on the same terms.
        dividers: SessionDividers,
    },
    /// A project was renamed: carry every draft that names it.
    ///
    /// The third step of a project rename, after the folder move and the one
    /// config write. Runs here, on the worker, in `save_all`'s shape: the live
    /// handle for the active draft and open→mutate→drop for every other, each
    /// meta write whole-file atomic. A draft left un-swept by a crash sits in
    /// the "not in your projects" group until it is re-saved or the rename is
    /// repeated — visible, honest, and nothing on disk is lost.
    RenameProject {
        /// The name drafts still carry.
        from: String,
        /// The name they should carry.
        to: String,
    },
    /// The project a *lazily created* draft is tagged with has changed.
    ///
    /// The worker creates a draft the moment a line needs somewhere to land, so
    /// it — not the shell — is the thing that has to know which project is
    /// active. Sent whenever the config's active project moves; it touches no
    /// existing draft.
    SetProject {
        /// The new active project, or `None`.
        name: Option<String>,
    },
    /// Turn the audio cues on or off, live.
    ///
    /// The [`CuePlayer`] is **kept either way**: the output stream is
    /// persistent by design, and dropping and
    /// rebuilding it on a checkbox would put a device open on the click path.
    /// Off simply means nothing is played.
    ///
    /// It also decides whether the cue window is trimmed off the front of a
    /// stored clip: with cues off there is no beep to trim, and trimming for
    /// one that never sounded would quietly eat the user's first syllable.
    SetCues(bool),
    /// Run every following utterance through a different model.
    ///
    /// The worker owns the [`Transcriber`] — it is blocking and single-owner —
    /// so a model change is a hand-over, not a mutation from outside. The new
    /// one arrives **already loaded and already warmed**: loading takes seconds
    /// and warming takes more, and neither may happen on this thread, where it
    /// would stall the line the user is speaking right now. Whoever builds it
    /// pays that cost on a thread of their own.
    ///
    /// Applied between utterances, like every other command: the swap happens
    /// at the top of a pass, so an utterance already being decoded finishes on
    /// the model it started with and every queued one is decoded by the new
    /// model. Nothing recorded is dropped by a swap.
    ///
    /// Boxed because a [`Transcriber`] is enormous next to the other variants,
    /// and this enum travels a channel.
    SetTranscriber(Box<Transcriber>),
    /// Decode in a different language from the next utterance on.
    ///
    /// Deliberately **not** a transcriber rebuild: whisper takes the language
    /// as a per-call decoding parameter, so this is a field assignment on the
    /// running model (see [`Transcriber::set_language`]) and costs nothing.
    SetLanguage(Language),
}

/// Why a session worker could not start.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The worker thread could not be spawned.
    #[error("could not start the transcription worker thread")]
    ThreadSpawn(#[source] std::io::Error),
}

/// What the worker owns for the life of the session.
pub struct SessionConfig {
    /// The loaded model. Blocking, single-owner.
    pub transcriber: Transcriber,
    /// The draft every line is appended to. Single-owner by construction, and
    /// optional: `None` means "no draft yet", and the first line creates one.
    /// The console runner passes `Some`, which is exactly its old behaviour.
    pub draft: Option<Draft>,
    /// Where drafts live, for the lazy creation and for the discard.
    pub store: DraftStore,
    /// Project to tag a lazily created draft with.
    pub project: Option<String>,
    /// Audio cues, if the output stream opened. `None` runs the session
    /// silently — cues are an enhancement, never a blocker.
    pub cues: Option<Arc<CuePlayer>>,
}

impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("model", &self.transcriber.model_path())
            .field("draft", &self.draft.as_ref().map(Draft::dir))
            .field("store", &self.store.root())
            .field("project", &self.project)
            .field("cues", &self.cues.is_some())
            .finish()
    }
}

/// The worker's one draft slot: at most one open handle, created on demand.
///
/// Split out of the loop so the whole of the draft lifecycle — lazy creation,
/// switching, discarding — can be tested without a model, a device or a window.
#[derive(Debug)]
struct DraftSlot {
    store: DraftStore,
    project: Option<String>,
    draft: Option<Draft>,
    /// Which drafts this *process* has met with lines already in them, and
    /// whether their session marker is still owed. See [`DraftSlot::note_sitting`].
    ///
    /// Keyed by draft id and never cleared: switching away from a draft and back
    /// must not make it look freshly resumed, and a draft that has already had
    /// its marker written must not get a second one.
    sittings: HashMap<String, Sitting>,
}

/// Whether a resumed draft still owes a [`SessionRecord`](crate::draft::SessionRecord).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sitting {
    /// Found with lines in it, and nothing new said yet. The next appended line
    /// gets a marker in front of it.
    Owed,
    /// Either the marker is written, or this draft was born in this process and
    /// never needed one.
    Settled,
}

/// What one command changed, so a whole batch can be reported with one event of
/// each kind rather than one per command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Change {
    /// Which draft is active changed.
    activation: bool,
    /// The active draft's lines changed.
    transcript: bool,
}

impl Change {
    /// Nothing happened, or it failed and was reported as an error.
    const NOTHING: Self = Self {
        activation: false,
        transcript: false,
    };
    /// A different draft (or none) is active. Always a transcript change too: a
    /// switch replaces the whole list, and a close empties it.
    const ACTIVATION: Self = Self {
        activation: true,
        transcript: true,
    };
    /// A line changed inside the same draft.
    const TRANSCRIPT: Self = Self {
        activation: false,
        transcript: true,
    };

    /// Fold another command's outcome in.
    fn absorb(&mut self, other: Self) {
        self.activation |= other.activation;
        self.transcript |= other.transcript;
    }
}

/// One save, borrowed out of the command that asked for it.
///
/// A struct rather than six arguments: half of them are `Option`s of similar
/// shape, and a call site that reads `save(&path, None, None, dividers, mode,
/// out)` says nothing about which `None` is the notes folder.
#[derive(Debug, Clone, Copy)]
struct SaveRequest<'a> {
    /// Where a first save goes, ignored once the draft is bound.
    fallback_path: &'a Path,
    /// The governing project's notes folder, for resolving and storing the
    /// binding.
    notes_root: Option<&'a Path>,
    /// Project for a draft that has none, applied only on success.
    adopt_project: Option<&'a str>,
    /// The header, already expanded.
    header: Option<&'a str>,
    /// Whether resumed sittings get a `---`.
    dividers: SessionDividers,
    /// Guarded, or the explicit answer to a conflict the user was shown.
    mode: SaveMode,
}

impl<'a> SaveRequest<'a> {
    /// The same request as the store's own options.
    const fn options(&self) -> SaveOptions<'a> {
        SaveOptions {
            header: self.header,
            mode: self.mode,
            dividers: self.dividers,
            notes_root: self.notes_root,
            adopt_project: self.adopt_project,
        }
    }
}

/// One project's share of a batch, borrowed out of the command that asked for
/// it.
///
/// No `mode` and no `adopt_project`, and their absence is the design: every
/// save in a batch is [`SaveMode::Guarded`], and a batch never moves a draft
/// into a project. Neither is a field that could be set wrong at a call site
/// because neither exists.
///
/// A batch holds one of these per project rather than one for
/// the whole run: a draft is saved through **its own** project's context, so
/// two projects' notes land in two folders under two templates in one act.
#[derive(Debug, Clone, Copy)]
struct SaveAllRequest<'a> {
    /// Whose notes.
    project: &'a str,
    /// The project's notes folder.
    notes_root: &'a Path,
    /// The filename template, unexpanded — see [`SaveAllRequest::first_save_path`].
    filename_template: &'a str,
    /// The header, already expanded.
    header: Option<&'a str>,
    /// Whether resumed sittings get a `---`.
    dividers: SessionDividers,
}

impl<'a> SaveAllRequest<'a> {
    /// The borrowed view of one project's context, as the batch uses it.
    fn of(context: &'a ProjectSaveContext) -> Self {
        Self {
            project: &context.project,
            notes_root: &context.notes_root,
            filename_template: &context.filename_template,
            header: context.header.as_deref(),
            dividers: SessionDividers::when(context.dividers),
        }
    }

    /// Where a never-saved draft in this batch would land, expanded for *now*.
    ///
    /// Called once per unbound draft rather than once per batch, deliberately:
    /// the template is the project's, but the moment is each note's own save
    /// moment. Two unbound drafts inside the same second therefore resolve to
    /// the same name — and that is left exactly as it falls, because the guard
    /// already has the right answer for it. The second draft lands on a file
    /// with a hash it never wrote, which is the unrelated-file conflict, so it
    /// is reported and skipped instead of quietly overwriting the first note
    /// (invariant 4). A uniquifying scheme here would invent filenames the user
    /// never asked for; a reported conflict tells them to change the template.
    fn first_save_path(&self) -> PathBuf {
        self.notes_root.join(template::expand_filename_now(
            self.filename_template,
            self.project,
        ))
    }

    /// One draft's save, as the single-save path already understands it.
    const fn per_draft(&self, fallback_path: &'a Path) -> SaveRequest<'a> {
        SaveRequest {
            fallback_path,
            notes_root: Some(self.notes_root),
            // Never in a batch. A draft reaches this context only by already
            // naming this project, so there is nothing here to adopt in the
            // first place.
            adopt_project: None,
            header: self.header,
            dividers: self.dividers,
            // The whole safety story of this command, in one line.
            mode: SaveMode::Guarded,
        }
    }
}

/// What a batch of saves did, tallied as it goes.
#[derive(Debug, Default)]
struct BatchOutcome {
    saved: usize,
    /// Dirty drafts that are not notes: no project, or a project the batch was
    /// given no context for.
    skipped: usize,
    conflicts: Vec<PathBuf>,
    errors: Vec<String>,
}

impl BatchOutcome {
    /// Fold one draft's outcome in. A conflict is a *stop*, not a failure, so
    /// it is counted apart from the errors — the window says different things
    /// about them.
    fn absorb(&mut self, outcome: SaveOutcome) {
        match outcome {
            SaveOutcome::Saved { .. } => self.saved += 1,
            SaveOutcome::Conflict { path, .. } => self.conflicts.push(path),
            SaveOutcome::Error { message } => self.errors.push(message),
        }
    }

    /// The one event the whole batch reports.
    fn into_event(self) -> SessionEvent {
        SessionEvent::SavedAll {
            saved: self.saved,
            skipped: self.skipped,
            conflicts: self.conflicts,
            errors: self.errors,
        }
    }
}

/// Render one draft to markdown and write it: resolve the target, create the
/// folder, save.
///
/// Shared by the single save and by every draft of a batch, so the two cannot
/// drift about which file a note belongs in — the batch would be exactly the
/// place for such a drift to overwrite the wrong file.
///
/// Blocking I/O — worker thread only (invariant 5).
fn save_one(draft: &mut Draft, request: &SaveRequest<'_>) -> SaveOutcome {
    // One decision about which file this is: the bound path
    // resolved against the project's notes folder, or the first-save path.
    // The folder created below, the file the guard hashes and the path a
    // conflict names are therefore always the same one.
    let target = match draft.save_target(request.fallback_path, request.notes_root) {
        Ok(target) => target,
        Err(err) => {
            return SaveOutcome::Error {
                message: flatten(&err),
            }
        }
    };

    // Creating the folder is additive and cannot destroy anything; doing it
    // for a re-save too covers the case where the user moved the whole notes
    // folder away, which the store already treats as "recreate the file".
    // With a relative binding that recreation lands under the *current*
    // notes folder, so a moved folder is not resurrected.
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                return SaveOutcome::Error {
                    message: format!("could not create {}: {err}", parent.display()),
                };
            }
        }
    }

    match draft.save_to(&target, request.options()) {
        Ok(report) => SaveOutcome::Saved {
            path: report.path,
            bytes: report.bytes,
            lines: report.lines,
        },
        Err(DraftError::SaveConflict { path }) => {
            conflict(draft, path, request.header, request.dividers)
        }
        Err(err) => SaveOutcome::Error {
            message: flatten(&err),
        },
    }
}

impl DraftSlot {
    /// What the frontend should be showing right now.
    fn changed_event(&self) -> SessionEvent {
        SessionEvent::DraftChanged {
            id: self.draft.as_ref().map(|d| d.id().to_owned()),
            dir: self.draft.as_ref().map(|d| d.dir().to_path_buf()),
            line_count: self.draft.as_ref().map_or(0, Draft::line_count),
        }
    }

    /// The active draft's transcript, read back off disk and folded.
    ///
    /// `None` when there is no active draft: [`SessionEvent::DraftChanged`]
    /// with `id: None` has already told the frontend to show nothing, and an
    /// empty transcript for a draft with no id would need one invented.
    ///
    /// Re-read rather than remembered, for the same reason the save re-reads:
    /// the log on disk is the truth, and a cache of it is one more thing that
    /// can be wrong about the user's words.
    fn transcript_event(&self) -> Option<SessionEvent> {
        let draft = self.draft.as_ref()?;
        Some(match draft.read_lines() {
            Ok(lines) => SessionEvent::Transcript {
                draft_id: draft.id().to_owned(),
                lines,
            },
            Err(err) => SessionEvent::Error {
                message: flatten(&err),
            },
        })
    }

    /// Apply one command, pushing anything the frontend needs to hear onto
    /// `out`.
    ///
    /// Everything except [`SessionCommand::Retranscribe`] lands here, which is
    /// what keeps the whole draft lifecycle — and now the whole editing
    /// surface — testable without a model, a device or a window.
    fn apply(&mut self, command: SessionCommand, out: &mut Vec<SessionEvent>) -> Change {
        match command {
            SessionCommand::SetDraft(draft) => {
                // The previous handle is dropped here and nothing else happens
                // to it: closing a draft leaves it outstanding, never deletes
                // it (invariant 4).
                self.note_sitting(&draft);
                self.draft = Some(*draft);
                Change::ACTIVATION
            }
            SessionCommand::CloseDraft => {
                if self.draft.take().is_some() {
                    Change::ACTIVATION
                } else {
                    Change::NOTHING
                }
            }
            SessionCommand::DiscardActive => self.discard_active(out),
            SessionCommand::EditLine { id, text } => {
                self.correct(&id, out, |draft, id| draft.edit_line(id, &text))
            }
            SessionCommand::SetDeleted { id, deleted } => {
                self.correct(&id, out, |draft, id| draft.set_deleted(id, deleted))
            }
            SessionCommand::MoveLine { id, after } => {
                self.correct(&id, out, |draft, id| draft.move_line(id, after.as_deref()))
            }
            SessionCommand::MoveLineTo {
                id,
                target_id,
                divide,
            } => self.move_line_to(&id, &target_id, divide, out),
            SessionCommand::RenameActive { name, notes_root } => {
                self.rename_active(&name, notes_root.as_deref(), out)
            }
            SessionCommand::SetDraftProject {
                project,
                old_root,
                new_root,
                clash,
                header,
                dividers,
            } => self.set_draft_project(
                project.clone(),
                MoveOptions {
                    project: project.as_deref(),
                    old_root: old_root.as_deref(),
                    new_root: new_root.as_deref(),
                    clash,
                    header: header.as_deref(),
                    dividers,
                },
                out,
            ),
            SessionCommand::RenameProject { from, to } => self.rename_project(&from, &to, out),
            // Only ever the *next* lazily created draft: no draft on disk is
            // touched, and a draft already tagged keeps its tag.
            SessionCommand::SetProject { name } => {
                self.project = name;
                Change::NOTHING
            }
            // Executed by the worker loop, which owns the transcriber. Routed
            // there before this function is ever reached; reported rather than
            // panicked on, because a dropped correction must not be silent.
            SessionCommand::Retranscribe { id } => {
                tracing::error!(line = %id, "a re-transcribe reached the draft slot; it is executed by the worker loop");
                Change::NOTHING
            }
            // Executed by `drain_commands`, which unpacks its fields; routed
            // there before this function is ever reached. Reported rather than
            // panicked on, because a dropped save must not be silent.
            SessionCommand::Save { .. } | SessionCommand::SaveAll { .. } => {
                tracing::error!("a save reached the draft slot's generic path; it is routed");
                Change::NOTHING
            }
            // Cue state belongs to the worker loop, which owns the player;
            // routed there before this function is reached.
            SessionCommand::SetCues(_) => {
                tracing::error!("a cue toggle reached the draft slot; it is routed");
                Change::NOTHING
            }
            // The model and the language belong to the transcriber, which the
            // worker loop owns; routed there before this function is reached.
            SessionCommand::SetTranscriber(_) | SessionCommand::SetLanguage(_) => {
                tracing::error!("a model change reached the draft slot; it is routed");
                Change::NOTHING
            }
        }
    }

    /// Note whether a draft that has just become active still owes a session
    /// marker.
    ///
    /// The rule is deliberately about the *process*, not about the
    /// draft: a note is one sitting per launch of Sotone, so a draft found with
    /// lines already in it gets one marker the first time it is spoken into
    /// here, and never another however many times the user switches back to it.
    /// A draft with no lines — including one this process just created — is
    /// already its own first sitting and gets nothing.
    ///
    /// `entry` and not `insert`: a draft already settled stays settled.
    fn note_sitting(&mut self, draft: &Draft) {
        let owed = if draft.line_count() > 0 {
            Sitting::Owed
        } else {
            Sitting::Settled
        };
        self.sittings.entry(draft.id().to_owned()).or_insert(owed);
    }

    /// Render the active draft to markdown and write it.
    ///
    /// Every path through here upholds invariant 4. The default is the guarded
    /// save, which reads the file first and refuses rather than clobbering;
    /// `SaveMode::Overwrite` is reachable only when the caller set `overwrite`,
    /// which the shell only ever does from the conflict dialog's Overwrite
    /// button; and the write itself is `write_atomic`, so an interrupted save
    /// leaves the previous file whole.
    ///
    /// Blocking I/O — worker thread only (invariant 5). That is the whole
    /// reason a save is a command instead of a Tauri handler.
    ///
    /// **No cues, on any outcome.** Cues belong to the
    /// recording loop and nothing else: they exist because the user is looking
    /// at the thing they are testing and cannot see the window, and every one
    /// of them — Begin, the blip when a dictated line lands, Capped — answers a
    /// key press. A save is a click in a window the user is already looking at,
    /// so a sound for it is noise in the room, not confirmation. The window
    /// says what happened.
    fn save(&mut self, request: &SaveRequest<'_>, out: &mut Vec<SessionEvent>) -> Change {
        let Some(draft) = self.draft.as_mut() else {
            out.push(saved_event(SaveOutcome::Error {
                message: "there is no active note to save — resume one first".to_owned(),
            }));
            return Change::NOTHING;
        };

        let outcome = save_one(draft, request);
        out.push(saved_event(outcome));
        // The lines did not change and neither did which draft is active. What
        // *did* change is the dirty flag, and that reaches the frontend through
        // the save event, which is what makes it re-list.
        Change::NOTHING
    }

    /// Save every dirty note in the store.
    ///
    /// # Which drafts
    ///
    /// The store is re-listed here rather than trusting anything the frontend
    /// sent, for the same reason a single save re-reads the log: the disk is
    /// the truth about what is dirty and what belongs to whom, and a batch
    /// driven off a stale list would write files nobody asked for. A draft is
    /// in the batch if it is dirty **and** `projects` holds a context for the
    /// project it names — every project the user has, so "save all" means all
    /// of them. A dirty draft with no project, or with one the configuration no
    /// longer has, is counted as skipped and left exactly as it was: notes exist
    /// only within projects, so there is no file to write and nothing failed.
    ///
    /// Each draft is saved through **its own** project's context, which is what
    /// makes the widened scope safe: a never-saved note of a project the user is
    /// not currently dictating into first-saves into *that* project's folder,
    /// under *that* project's filename template, never the active one's.
    ///
    /// # Two ways to hold a draft, and why
    ///
    /// The active draft is saved through the handle this slot already owns.
    /// That is not an optimisation: this process's live state for that draft —
    /// its `lines` count, its torn-tail flag, its in-memory `meta`, and the
    /// session-marker bookkeeping in [`DraftSlot::sittings`] — lives on that
    /// handle, and a second handle opened over the same directory would write
    /// `meta.json` from a copy that never saw them, then be dropped, leaving
    /// the live handle to write its own stale `dirty: true` back over it on the
    /// next append.
    ///
    /// Every other draft is opened, saved, and dropped inside one iteration of
    /// the loop, so the handle provably does not outlive its save — the same
    /// reasoning as the discard-in-worker rule. None of them can race the
    /// active handle either, because there is no concurrency here to race
    /// *with*: this runs **on** the worker thread, which is the only thread in
    /// the process that ever holds a [`Draft`], and it holds this one for the
    /// length of one `save_one` call.
    ///
    /// Blocking I/O — worker thread only (invariant 5). No cues, like every
    /// other save.
    fn save_all(&mut self, projects: &[ProjectSaveContext], out: &mut Vec<SessionEvent>) -> Change {
        let mut batch = BatchOutcome::default();

        let scan = match self.store.list_drafts() {
            Ok(scan) => scan,
            Err(err) => {
                // Nothing was written and nothing can be: report the batch as
                // one failed batch rather than as silence.
                batch.errors.push(flatten(&err));
                out.push(batch.into_event());
                return Change::NOTHING;
            }
        };

        for summary in scan.drafts {
            if !summary.dirty {
                continue;
            }

            // The draft's own project decides which rules it saves under, and a
            // draft the batch has no context for is not a note: skipped, never
            // filed under whichever project happened to be first.
            let Some(context) = summary
                .project
                .as_deref()
                .and_then(|name| projects.iter().find(|context| context.project == name))
            else {
                batch.skipped += 1;
                continue;
            };
            let request = SaveAllRequest::of(context);

            // Per draft, and only ever used by one that has never been saved:
            // a bound note ignores it entirely and is rewritten in place.
            let fallback = request.first_save_path();
            let save = request.per_draft(&fallback);

            let outcome = match self.draft.as_mut() {
                Some(draft) if draft.id() == summary.id => save_one(draft, &save),
                _ => match Draft::open(&summary.dir) {
                    Ok(loaded) => {
                        let mut draft = loaded.draft;
                        let outcome = save_one(&mut draft, &save);
                        // Explicit, not incidental: the handle exists for this
                        // one save and for nothing else, and it must be gone
                        // before the next iteration can open another.
                        drop(draft);
                        outcome
                    }
                    Err(err) => SaveOutcome::Error {
                        message: flatten(&err),
                    },
                },
            };
            batch.absorb(outcome);
        }

        tracing::info!(
            projects = projects.len(),
            saved = batch.saved,
            skipped = batch.skipped,
            conflicts = batch.conflicts.len(),
            errors = batch.errors.len(),
            "saved every dirty note in the store"
        );
        out.push(batch.into_event());
        // As with a single save: no line moved and no draft became active. The
        // dirty flags that changed reach the frontend through the re-list the
        // shell does when this event lands.
        Change::NOTHING
    }

    /// Rename the active note's file, through the handle this slot owns.
    ///
    /// The one-handle rule again: opening a second [`Draft`] over the active
    /// directory to rename it would write `meta.json` from a copy that never
    /// saw this handle's `lines` count or its torn-tail flag, and the live
    /// handle would then write its own stale copy straight back over the new
    /// binding on the next append. The whole rename — including the refusals —
    /// lives in [`Draft::rename_note`]; what is decided here is only *which*
    /// handle performs it.
    ///
    /// Blocking I/O — worker thread only (invariant 5).
    fn rename_active(
        &mut self,
        name: &str,
        notes_root: Option<&Path>,
        out: &mut Vec<SessionEvent>,
    ) -> Change {
        let Some(draft) = self.draft.as_mut() else {
            out.push(SessionEvent::Renamed {
                errors: vec!["there is no active note to rename".to_owned()],
            });
            return Change::NOTHING;
        };

        let errors = match draft.rename_note(name, notes_root) {
            Ok(report) => {
                tracing::info!(
                    from = %report.from.display(),
                    to = %report.to.display(),
                    moved = report.moved,
                    "renamed the active note"
                );
                Vec::new()
            }
            // Every refusal is an ordinary answer — the name is taken, the
            // note has never been saved, its project is gone — and none of
            // them wrote anything.
            Err(err) => vec![flatten(&err)],
        };
        out.push(SessionEvent::Renamed { errors });
        // No line moved and no draft became active. The re-list the shell does
        // when this event lands is what puts the new name on every surface.
        Change::NOTHING
    }

    /// Move the active note to another project, through the handle this slot
    /// owns.
    ///
    /// The one-handle rule again, and nothing else is decided here: the whole
    /// move — the resolve, the clash question, the `fs::rename` or the recreate,
    /// and the single meta write — lives in [`Draft::move_to_project`], so the
    /// active route and the control thread's route cannot come to disagree
    /// about what moving a note between projects means.
    ///
    /// It answers with [`SessionEvent::NoteMoved`], which carries the outcome
    /// rather than only a list of failures: a drop can come back with
    /// a question, and a question has to name the draft it is about.
    ///
    /// Blocking I/O — worker thread only (invariant 5).
    fn set_draft_project(
        &mut self,
        project: Option<String>,
        opts: MoveOptions<'_>,
        out: &mut Vec<SessionEvent>,
    ) -> Change {
        // The worker tags a *lazily created* draft with `self.project`, which is
        // the active project and is nothing to do with this: moving one note
        // must not change where the next dictated line is filed.
        let Some(draft) = self.draft.as_mut() else {
            out.push(SessionEvent::NoteMoved {
                draft: String::new(),
                project,
                outcome: Err("there is no active note to move".to_owned()),
            });
            return Change::NOTHING;
        };

        let id = draft.id().to_owned();
        // A failed move is an ordinary answer, and it is always an answer about
        // a note that is still on disk where it was: every failure inside
        // `move_to_project` happens before, or instead of, the delete.
        let outcome = draft.move_to_project(opts).map_err(|err| flatten(&err));
        out.push(SessionEvent::NoteMoved {
            draft: id,
            project,
            outcome,
        });
        Change::NOTHING
    }

    /// Carry every draft of a renamed project to the new name.
    ///
    /// The last of the rename's three steps, and the only one that touches
    /// drafts. It runs here for the same two reasons `save_all` does: the
    /// active draft must be reached through the live handle, and every other
    /// draft is opened, mutated and dropped **inside one iteration**, on the
    /// one thread in the process that ever holds a [`Draft`].
    ///
    /// The store is re-listed rather than trusting anything the frontend sent —
    /// the disk is the truth about which drafts name which project — and a
    /// draft this cannot open is reported and skipped rather than stopping the
    /// sweep: one unreadable draft must not strand every other one in the
    /// "not in your projects" group.
    ///
    /// Blocking I/O — worker thread only (invariant 5).
    fn rename_project(&mut self, from: &str, to: &str, out: &mut Vec<SessionEvent>) -> Change {
        // The worker tags a *lazily created* draft with this name, so it has to
        // move too — otherwise the next line spoken lands in a draft filed
        // under a project that no longer exists.
        if self.project.as_deref() == Some(from) {
            self.project = Some(to.to_owned());
        }

        let mut errors = Vec::new();
        let scan = match self.store.list_drafts() {
            Ok(scan) => scan,
            Err(err) => {
                errors.push(flatten(&err));
                out.push(SessionEvent::Renamed { errors });
                return Change::NOTHING;
            }
        };

        let mut swept = 0_usize;
        for summary in scan.drafts {
            if summary.project.as_deref() != Some(from) {
                continue;
            }
            let result = match self.draft.as_mut() {
                Some(draft) if draft.id() == summary.id => draft.set_project(Some(to)),
                _ => match Draft::open(&summary.dir) {
                    Ok(loaded) => {
                        let mut draft = loaded.draft;
                        let written = draft.set_project(Some(to));
                        // Explicit, not incidental: the handle exists for this
                        // one write and must be gone before the next iteration
                        // opens another.
                        drop(draft);
                        written
                    }
                    Err(err) => Err(err),
                },
            };
            match result {
                Ok(true) => swept += 1,
                Ok(false) => {}
                Err(err) => errors.push(flatten(&err)),
            }
        }

        tracing::info!(
            from = %from,
            to = %to,
            swept,
            failed = errors.len(),
            "carried drafts to a renamed project"
        );
        out.push(SessionEvent::Renamed { errors });
        Change::NOTHING
    }

    /// The one path for every correction: apply it to the active draft, or say
    /// why not.
    ///
    /// A failure is an event naming the line, never a panic and never a silent
    /// drop — an edit that vanishes reads to the user as "my correction did not
    /// stick", which is the impression this app can least afford.
    ///
    /// Generic in what the correction returns: an edit hands back one record and
    /// a move hands back the whole folded order, and neither is used here — the
    /// transcript snapshot that follows is what the frontend reads.
    fn correct<T>(
        &mut self,
        id: &str,
        out: &mut Vec<SessionEvent>,
        edit: impl FnOnce(&mut Draft, &str) -> Result<T, DraftError>,
    ) -> Change {
        let Some(draft) = self.draft.as_mut() else {
            out.push(SessionEvent::Error {
                message: format!(
                    "there is no active draft, so line {id} could not be edited — resume the \
                     draft first"
                ),
            });
            return Change::NOTHING;
        };

        match edit(draft, id) {
            // A no-op edit lands here too, having written nothing. Reporting
            // the transcript anyway is harmless and keeps a UI that thought it
            // had changed something honest.
            Ok(_) => Change::TRANSCRIPT,
            Err(err) => {
                out.push(SessionEvent::Error {
                    message: flatten(&err),
                });
                Change::NOTHING
            }
        }
    }

    /// Move one line out of the active draft and into another one.
    ///
    /// # Two handles, one at a time
    ///
    /// The source is the handle this slot already owns. The destination is
    /// opened, appended to, and **dropped inside this call** — the
    /// [`DraftSlot::rename_project`] and `save_all` precedent, and safe for the
    /// same reason: this runs on the worker, the only thread in the process
    /// that ever holds a [`Draft`], so there is no concurrency to race with.
    /// A target equal to the active draft is refused *before* anything is
    /// opened, because that is the one case where the two would be the same
    /// directory.
    ///
    /// # Destination first, source last
    ///
    /// The order is the whole crash story (invariant 4). The wav copy, the
    /// destination's records and its `dirty` flag all land before the source's
    /// soft delete, so a power cut anywhere in the middle leaves the line in
    /// **both** notes — never in neither. A failure before the destination
    /// append completes skips the source delete entirely: the line stays
    /// exactly where it was, and the refusal names it.
    ///
    /// Every write on both sides is an append or the existing atomic meta
    /// write. Nothing is removed, nothing is rewritten, and the source's
    /// `last_save_hash` is untouched — the blake3 guard is about the markdown
    /// file being edited underneath us, which a move does not do.
    ///
    /// # The divider is the batch's, not the line's
    ///
    /// `divide` is carried straight through to [`Draft::import_line`] and is
    /// true only for the first line of a confirm's N calls. This slot does not
    /// remember batches: a latch here would have to guess when one ended, and
    /// the window — which is what a batch *is* — already knows.
    ///
    /// Blocking I/O — worker thread only (invariant 5).
    fn move_line_to(
        &mut self,
        id: &str,
        target_id: &str,
        divide: bool,
        out: &mut Vec<SessionEvent>,
    ) -> Change {
        let refuse = |out: &mut Vec<SessionEvent>, why: String| {
            out.push(SessionEvent::Error { message: why });
            Change::NOTHING
        };

        let Some(draft) = self.draft.as_mut() else {
            return refuse(
                out,
                format!(
                    "there is no active note, so line {id} could not be moved — resume the note \
                     first"
                ),
            );
        };
        if draft.id() == target_id {
            return refuse(
                out,
                format!("line {id} is already in that note — nothing was moved"),
            );
        }
        // Owned, because the handle is borrowed mutably for the rest of this.
        let source_id = draft.id().to_owned();

        // The id arrived from the frontend, so it is input rather than fact:
        // the store validates it before it is joined onto the drafts root.
        let dir = match self.store.draft_path(target_id) {
            Ok(dir) => dir,
            Err(err) => {
                return refuse(
                    out,
                    format!("line {id} could not be moved: {}", flatten(&err)),
                )
            }
        };

        // Reads only. Nothing in the source has changed when this returns, so
        // every refusal below leaves the line where it is.
        let moved = match draft.export_line(id) {
            Ok(moved) => moved,
            Err(err) => {
                return refuse(
                    out,
                    format!("line {id} could not be moved: {}", flatten(&err)),
                )
            }
        };

        let mut target = match Draft::open(&dir) {
            Ok(loaded) => loaded.draft,
            Err(err) => {
                return refuse(
                    out,
                    format!(
                        "line {id} could not be moved into note {target_id}: {}",
                        flatten(&err)
                    ),
                )
            }
        };
        let arrival = target.import_line(&moved, divide);
        // Explicit, not incidental: the destination handle exists for this one
        // append and must be gone before the source is touched — and before the
        // next line of the same selection opens it again.
        drop(target);

        if let Err(err) = arrival {
            return refuse(
                out,
                format!(
                    "line {id} could not be moved into note {target_id}: {} — it is still in \
                     this note",
                    flatten(&err)
                ),
            );
        }

        // The ordinary soft delete, the same one Delete N writes, which is also
        // what makes leaving the source undoable by the ordinary
        // `line_set_deleted` the frontend pushes as its one step.
        if let Err(err) = draft.set_deleted(id, true) {
            out.push(SessionEvent::Error {
                message: format!(
                    "line {id} was copied into note {target_id} but could not be taken out of \
                     this one ({}) — it is now in both",
                    flatten(&err)
                ),
            });
            // A transcript all the same: the *destination* grew, and the
            // re-list that rides on this event is what puts its line count and
            // its dirty dot on screen. The source's lines are unchanged, so
            // re-reporting them costs one snapshot and keeps a window that
            // thought something happened honest.
            return Change::TRANSCRIPT;
        }

        tracing::info!(
            from = %source_id,
            to = %target_id,
            line = %id,
            "moved a line into another note"
        );
        Change::TRANSCRIPT
    }

    /// Drop the handle, then move the directory. The order is the point.
    fn discard_active(&mut self, out: &mut Vec<SessionEvent>) -> Change {
        let Some(draft) = self.draft.take() else {
            tracing::debug!("discard asked for with no active draft; nothing to do");
            return Change::NOTHING;
        };
        let id = draft.id().to_owned();
        // Explicit, not incidental: `discard` renames the directory out from
        // under this path, and a live handle would be appending into a moved
        // directory on some platforms and a deleted one on others.
        drop(draft);

        let event = match self.store.discard(&id) {
            Ok(path) => {
                tracing::info!(draft = %id, trash = %path.display(), "discarded the active draft");
                SessionEvent::DraftDiscarded {
                    id,
                    ok: true,
                    message: None,
                }
            }
            Err(err) => {
                // The handle stays dropped either way: the draft is still on
                // disk and still in the list, it is simply no longer active.
                let message = flatten(&err);
                SessionEvent::DraftDiscarded {
                    id,
                    ok: false,
                    message: Some(message),
                }
            }
        };
        out.push(event);
        Change::ACTIVATION
    }

    /// Append a line, creating the draft first if there is none.
    ///
    /// Returns the record, and — when a draft had to be created for it — the
    /// activation event as it stood *before* the append, so the frontend can
    /// announce the empty new draft and then count this line as its first.
    fn append(
        &mut self,
        line: NewLine<'_>,
    ) -> Result<(LineRecord, Option<SessionEvent>), DraftError> {
        let mut created = None;
        if self.draft.is_none() {
            let fresh = self.store.create_draft(self.project.as_deref())?;
            // Born here, so it is its own first sitting and owes no marker.
            self.sittings
                .insert(fresh.id().to_owned(), Sitting::Settled);
            self.draft = Some(fresh);
            created = Some(self.changed_event());
        }
        let owed = self
            .draft
            .as_ref()
            .is_some_and(|draft| self.sittings.get(draft.id()) == Some(&Sitting::Owed));
        let Some(draft) = self.draft.as_mut() else {
            unreachable!("a draft was just created for this line")
        };
        if owed {
            // Immediately before the line, not when the draft was resumed: a
            // resumed note nobody spoke into must not grow a divider.
            draft.mark_session()?;
        }
        let record = draft.append_line(line)?.clone();
        if owed {
            // Settled only once the line is really there. A failed append
            // leaves the debt standing, so the marker is not spent on a line
            // that never landed.
            let id = draft.id().to_owned();
            self.sittings.insert(id, Sitting::Settled);
        }
        Ok((record, created))
    }
}

/// One save outcome as the event that carries it.
fn saved_event(outcome: SaveOutcome) -> SessionEvent {
    SessionEvent::Saved {
        outcome: Box::new(outcome),
    }
}

/// Both sides of a stopped save, for the dialog.
///
/// The disk read is best-effort by design: a file we cannot read is still a file
/// we refuse to clobber, and an empty left-hand side is a far better answer than
/// an error the user cannot act on. `from_utf8_lossy` for the same reason — the
/// bytes stay exactly where they are, so nothing is lost by rendering them
/// imperfectly.
fn conflict(
    draft: &Draft,
    path: PathBuf,
    header: Option<&str>,
    dividers: SessionDividers,
) -> SaveOutcome {
    let disk_text = std::fs::read(&path).map_or_else(
        |err| {
            tracing::warn!(path = %path.display(), error = %err, "could not read the file a save stopped on");
            String::new()
        },
        |bytes| String::from_utf8_lossy(&bytes).into_owned(),
    );

    match draft.preview_markdown(header, dividers) {
        Ok(pending_markdown) => SaveOutcome::Conflict {
            path,
            disk_text,
            pending_markdown,
        },
        // The log became unreadable between the save attempt and this re-read.
        // There is no diff to show, so this is an error, not a conflict.
        Err(err) => SaveOutcome::Error {
            message: flatten(&err),
        },
    }
}

/// Out-of-band messages to the worker.
enum Control {
    EngineDead { reason: String },
}

/// A change to the model the worker decodes with, waiting for the top of a pass.
///
/// One queue rather than two slots, so a model swap and a language change keep
/// the order the user made them in: a swap carries the language the new model
/// was loaded with, and a language chosen afterwards has to win.
#[derive(Debug)]
enum ModelChange {
    /// A model that is already loaded and already warmed.
    Swap(Box<Transcriber>),
    /// A decoding parameter on whatever model is running.
    Language(Language),
}

/// Apply one model change to the running transcriber.
///
/// Cheap by construction: a swap is a move (the load and the warm-up were paid
/// for on someone else's thread) and a language is a field assignment. Neither
/// touches the audio queue, so utterances recorded while a model was loading
/// are still sitting there and are decoded by the model that just arrived.
fn apply_model_change(
    transcriber: &mut Transcriber,
    change: ModelChange,
    out: &mut Vec<SessionEvent>,
) {
    match change {
        // The old transcriber is dropped here, freeing the old model's memory.
        // It provably cannot be mid-decode: this thread is the only thing that
        // decodes, and it is running this line.
        ModelChange::Swap(next) => {
            tracing::info!(
                from = %transcriber.model_path().display(),
                to = %next.model_path().display(),
                "the transcription model was swapped"
            );
            *transcriber = *next;
        }
        // A failure is a refusal, not a silent drop: a hand-edited config can
        // hold a language that cannot cross the FFI boundary, and the user has
        // to be told the model kept the one it had.
        ModelChange::Language(language) => {
            if let Err(err) = transcriber.set_language(language) {
                out.push(SessionEvent::Error {
                    message: flatten(&err),
                });
            }
        }
    }
}

/// A running dictation worker.
///
/// Dropping it asks the worker to stop and joins it, which takes up to
/// [`POLL`] plus however long the utterance in flight needs.
#[derive(Debug)]
pub struct SessionWorker {
    control: Sender<Control>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SessionWorker {
    /// Start the worker.
    ///
    /// `utterances` is the receiver [`AudioEngine::start`](crate::audio::AudioEngine::start)
    /// handed back; `releases` is fed by the hotkey consumer thread. Returns the
    /// handle and the channel every [`SessionEvent`] arrives on.
    ///
    /// # Errors
    /// Returns [`SessionError::ThreadSpawn`] if the OS refuses a thread.
    pub fn spawn(
        utterances: Receiver<Utterance>,
        releases: Receiver<ReleaseInfo>,
        commands: Receiver<SessionCommand>,
        config: SessionConfig,
    ) -> Result<(Self, Receiver<SessionEvent>), SessionError> {
        let (control_tx, control_rx) = mpsc::channel::<Control>();
        let (event_tx, event_rx) = mpsc::channel::<SessionEvent>();
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_shutdown = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name("sotone-session".to_owned())
            .spawn(move || {
                run(
                    &worker_shutdown,
                    &utterances,
                    &releases,
                    &commands,
                    &control_rx,
                    &event_tx,
                    config,
                );
            })
            .map_err(SessionError::ThreadSpawn)?;

        Ok((
            Self {
                control: control_tx,
                shutdown,
                handle: Some(handle),
            },
            event_rx,
        ))
    }

    /// Tell the worker the capture engine is gone.
    ///
    /// The caller owns the engine and is the only one who can see
    /// [`EngineStatus::Dead`](crate::audio::EngineStatus::Dead); the worker owns
    /// the cue player and the event channel, so the report has to come here to
    /// be heard.
    pub fn report_engine_dead(&self, reason: String) {
        // A dead worker has already stopped reporting anything; nothing useful
        // to do about a failed send.
        let _ = self.control.send(Control::EngineDead { reason });
    }
}

impl Drop for SessionWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The worker loop.
///
/// Blocking by design and on its own thread: everything in here — the decode,
/// the fsync — is work that must never touch a callback (invariant 5).
fn run(
    shutdown: &AtomicBool,
    utterances: &Receiver<Utterance>,
    releases: &Receiver<ReleaseInfo>,
    commands: &Receiver<SessionCommand>,
    control: &Receiver<Control>,
    events: &Sender<SessionEvent>,
    config: SessionConfig,
) {
    let SessionConfig {
        mut transcriber,
        draft,
        store,
        project,
        cues,
    } = config;
    let mut slot = DraftSlot {
        store,
        project,
        draft: None,
        sittings: HashMap::new(),
    };
    // Through the same door every other activation goes through, so a draft
    // handed over at construction (the console runner does this) is treated as
    // the resumption it may well be.
    if let Some(draft) = draft {
        slot.note_sitting(&draft);
        slot.draft = Some(draft);
    }
    let mut pending: VecDeque<ReleaseInfo> = VecDeque::new();
    // Utterances that have been paired, stamped and gated, and are waiting for
    // the model. The queue *is* the "queued" rows on screen: intake
    // fills it as fast as audio arrives, decode empties it one per pass so
    // commands and model swaps keep their between-utterances slot.
    let mut queue: VecDeque<Pending> = VecDeque::new();
    // Lines the disk refused. See [`HeldLine`]: unbounded, never persisted,
    // flushed ahead of every new append.
    let mut held: VecDeque<HeldLine> = VecDeque::new();
    // Re-transcribe requests wait here rather than running inside
    // `drain_commands`: they need the transcriber, and the audio the user is
    // speaking *now* outranks a correction to something they said earlier.
    let mut retranscribes: VecDeque<String> = VecDeque::new();
    // Model and language changes wait here for the same reason, minus the
    // expense: they are applied a few lines below, before the next utterance is
    // looked at, so a model chosen a moment ago decodes the line about to
    // arrive rather than the one after it.
    let mut model_changes: VecDeque<ModelChange> = VecDeque::new();
    // Whether cues are *audible*, as against whether a player exists. Starts on
    // and is turned off by `SessionCommand::SetCues` — the shell sends one at
    // startup when the config says so, before any key can be pressed.
    let mut cues_on = true;

    // A labelled block rather than bare `return`s, so every way out of the loop
    // — shutdown, a dead channel, a webview that stopped listening — passes
    // through the one shutdown flush below. Lines held in memory are the user's
    // words; a worker that exited without one last try at writing them would be
    // the discard this whole mechanism exists to prevent (invariant 4).
    'running: {
        while !shutdown.load(Ordering::Acquire) {
            // Commands first, before the next utterance is looked at: a draft
            // switch the user made a moment ago has to be in force for the line
            // that is about to be written, not for the one after it.
            if !drain_commands(
                commands,
                &mut slot,
                &mut held,
                &mut retranscribes,
                &mut cues_on,
                &mut model_changes,
                events,
            ) {
                break 'running;
            }
            // Immediately after the commands and before any audio is looked at:
            // this is what "the swap happens between utterances" means.
            if !model_changes.is_empty() {
                let mut out = Vec::new();
                while let Some(change) = model_changes.pop_front() {
                    apply_model_change(&mut transcriber, change, &mut out);
                }
                if !send_all(events, out) {
                    break 'running;
                }
            }
            // Re-derived every pass, after the commands: one `Option<&CuePlayer>`
            // that is `None` while cues are off, so every cue decision downstream —
            // playing, and trimming the cue window off the clip — follows the
            // switch without a second flag to keep in step.
            let cues = cues.as_deref().filter(|_| cues_on);
            drain_control(control, events, cues);
            drain_releases(releases, &mut pending);

            // ---- Intake ----------------------------------------------------
            //
            // Every utterance waiting, not just one: capture never waits for
            // transcription, so a backlog has to be *taken in* at the speed the
            // audio arrives even though it is decoded one per pass. Only the
            // wait is conditional — with nothing queued this is the loop's
            // sleep, and with work in hand it must not add 100 ms to every
            // line.
            let first = if queue.is_empty() {
                match utterances.recv_timeout(POLL) {
                    Ok(utterance) => Some(utterance),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => {
                        tracing::debug!("the audio engine is gone; stopping the session worker");
                        break 'running;
                    }
                }
            } else {
                match utterances.try_recv() {
                    Ok(utterance) => Some(utterance),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        tracing::debug!("the audio engine is gone; stopping the session worker");
                        break 'running;
                    }
                }
            };

            let mut out = Vec::new();
            for utterance in first.into_iter().chain(utterances.try_iter()) {
                // Drain again: the release is sent before the post-roll ends, so
                // it is normally already queued, but this closes the window
                // where the two crossed.
                drain_releases(releases, &mut pending);
                // The cue and the event at intake, before any backlog: a split
                // the user only hears about after transcription is a split they
                // spoke over.
                out.extend(announce_cap(&utterance, cues));
                let release = take_release(utterance.capped, &mut pending, SystemTime::now());
                queue.extend(intake(utterance, release, cues, &mut out));
            }
            if !send_all(events, out) {
                break 'running;
            }

            // ---- Decode ----------------------------------------------------
            //
            // One per pass, deliberately: a backlog must not lock the worker out
            // of its commands, its model swaps or its saves for as long as it
            // takes to clear.
            if let Some(next) = queue.pop_front() {
                let mut out = Vec::new();
                let change = decode(next, &mut transcriber, &mut slot, &mut held, cues, &mut out);
                if change.transcript {
                    // The transcript panel has to be able to edit the line that
                    // was just spoken, which means it needs its id — and one
                    // re-read of a few kilobytes, right after an fsync we have
                    // already paid for, is nothing next to the decode.
                    out.extend(slot.transcript_event());
                }
                if !send_all(events, out) {
                    break 'running;
                }
            }

            // Post-session tidy-up, after any utterance that was already waiting.
            // Same thread, same transcriber, same blocking decode — which is
            // exactly why it belongs here and not on the control thread or in a
            // Tauri command (invariant 5).
            while let Some(id) = retranscribes.pop_front() {
                let mut out = Vec::new();
                let change = retranscribe(&mut slot, &id, &mut transcriber, &mut out);
                if change.transcript {
                    out.extend(slot.transcript_event());
                }
                if !send_all(events, out) {
                    break 'running;
                }
            }
        }
    }

    // One last attempt at anything held, best effort. Utterances still waiting
    // in `queue` are *not* drained: they are lost, because a shutdown that
    // first ran the model over a backlog would hang the app's own exit. A
    // known gap.
    let mut out = Vec::new();
    if flush_held(&mut slot, &mut held, &mut out).transcript {
        out.extend(slot.transcript_event());
    }
    if !held.is_empty() {
        tracing::warn!(
            held = held.len(),
            "the worker is stopping with lines that could not be written"
        );
    }
    send_all(events, out);
}

/// Send a batch, reporting whether anyone is still listening.
fn send_all(events: &Sender<SessionEvent>, batch: Vec<SessionEvent>) -> bool {
    for event in batch {
        if events.send(event).is_err() {
            tracing::debug!("session events have no listener; stopping the worker");
            return false;
        }
    }
    true
}

/// Run the model over one line's stored audio again and keep the result.
///
/// The read is of a file this crate wrote, through
/// [`Draft::line_audio_path`](crate::draft::Draft::line_audio_path) so an id
/// off the IPC boundary cannot name anything else. Every failure — no active
/// draft, a bad id, an unreadable wav, a model error — becomes an event naming
/// the line rather than a panic or a silent drop.
///
/// An empty result deliberately writes **nothing**: whisper finding no words in
/// a clip it already transcribed once is a worse answer than the one already
/// there, and overwriting a line with "" would be the app eating a finding.
fn retranscribe(
    slot: &mut DraftSlot,
    id: &str,
    transcriber: &mut Transcriber,
    out: &mut Vec<SessionEvent>,
) -> Change {
    let Some(draft) = slot.draft.as_ref() else {
        out.push(SessionEvent::Error {
            message: format!(
                "there is no active draft, so line {id} could not be re-transcribed — resume the \
                 draft first"
            ),
        });
        return Change::NOTHING;
    };

    let path = match draft.line_audio_path(id) {
        Ok(path) => path,
        Err(err) => {
            out.push(SessionEvent::Error {
                message: flatten(&err),
            });
            return Change::NOTHING;
        }
    };

    let samples = match read_line_audio(&path) {
        Ok(samples) => samples,
        Err(err) => {
            out.push(SessionEvent::Error {
                message: format!("could not re-transcribe that line: {}", flatten(&err)),
            });
            return Change::NOTHING;
        }
    };

    let transcript = match transcriber.transcribe(&samples) {
        Ok(transcript) => transcript,
        Err(err) => {
            out.push(SessionEvent::Error {
                message: format!("could not re-transcribe that line: {}", flatten(&err)),
            });
            return Change::NOTHING;
        }
    };

    let Some(text) = retranscribed_text(&transcript.text) else {
        out.push(SessionEvent::Notice {
            message: "re-transcribe produced nothing for that line; it is unchanged".to_owned(),
        });
        return Change::NOTHING;
    };
    let transcribe_ms = u64::try_from(transcript.duration.as_millis()).ok();

    slot.correct(id, out, |draft, id| {
        draft.retranscribed_line(id, &text, transcribe_ms)
    })
}

/// What a re-transcription's text is worth keeping as, or `None` to leave the
/// line exactly as it is.
///
/// Pure, and split out on purpose: this is the rule that stops a second pass
/// over a clip whisper heard nothing in from replacing a real finding with an
/// empty line, and it is the same trim the live path applies before an append.
/// The model call is the only part of a re-transcribe a test cannot reach.
fn retranscribed_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Why a stored line's audio could not be turned back into samples.
#[derive(Debug, thiserror::Error)]
enum AudioReadError {
    /// The file is missing, truncated, or not a wav at all.
    #[error("could not read the audio at {}: {source}", .path.display())]
    Wav {
        /// The wav we tried to read.
        path: PathBuf,
        /// What `hound` said.
        #[source]
        source: hound::Error,
    },
    /// It is a wav, but not one this app wrote.
    #[error("{} is not the 16 kHz mono 16-bit audio Sotone writes ({detail})", .path.display())]
    Unexpected {
        /// The wav we tried to read.
        path: PathBuf,
        /// What it turned out to be.
        detail: String,
    },
}

/// One stored line's wav, back as the f32 mono whisper consumes.
///
/// A decode of our own format, not a general importer: the draft store writes
/// 16 kHz mono 16-bit PCM and nothing else ever writes into `audio/`, so
/// anything different is a file we should refuse rather than resample. There is
/// no resampler here on purpose — reaching for one would mean guessing at what
/// the user's audio is.
fn read_line_audio(path: &Path) -> Result<Vec<f32>, AudioReadError> {
    let wav = |source| AudioReadError::Wav {
        path: path.to_path_buf(),
        source,
    };

    let mut reader = hound::WavReader::open(path).map_err(wav)?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.sample_rate != TARGET_SAMPLE_RATE {
        return Err(AudioReadError::Unexpected {
            path: path.to_path_buf(),
            detail: format!("{}-channel, {} Hz", spec.channels, spec.sample_rate),
        });
    }

    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| {
                sample
                    .map(|s| f32::from(s) / f32::from(i16::MAX))
                    .map_err(wav)
            })
            .collect(),
        (hound::SampleFormat::Float, 32) => {
            reader.samples::<f32>().map(|s| s.map_err(wav)).collect()
        }
        (format, bits) => Err(AudioReadError::Unexpected {
            path: path.to_path_buf(),
            detail: format!("{bits}-bit {format:?}"),
        }),
    }
}

/// Apply every pending draft command, then report the resulting activation
/// once.
///
/// Last-wins by construction: three queued switches leave the third draft
/// active and produce one [`SessionEvent::DraftChanged`], not three. A discard
/// in the middle of the batch still gets its own event, because it says
/// something no activation event does — that a directory moved.
///
/// Edits are folded in the same way: a batch of them produces one
/// [`SessionEvent::Transcript`] at the end, not one per line, so the frontend
/// re-renders the list once however big an Undo was.
///
/// [`SessionCommand::Retranscribe`] is the exception. It needs the transcriber,
/// which the worker loop owns, so it is only queued here.
/// [`SessionCommand::Save`] and [`SessionCommand::SaveAll`] are routed too,
/// because each carries fields only it uses and produces an outcome rather than
/// a [`Change`]. The two that change the model
/// ([`SessionCommand::SetTranscriber`], [`SessionCommand::SetLanguage`]) are
/// queued like a re-transcribe and for the same reason: the transcriber belongs
/// to the loop, which applies them before it looks at the next utterance.
///
/// A save is *not* folded into the batch: each one produces its own outcome
/// event, because each one either wrote a file or did not.
///
/// Returns `false` when the event channel is gone, i.e. when the worker should
/// stop.
fn drain_commands(
    commands: &Receiver<SessionCommand>,
    slot: &mut DraftSlot,
    held: &mut VecDeque<HeldLine>,
    retranscribes: &mut VecDeque<String>,
    cues_on: &mut bool,
    model_changes: &mut VecDeque<ModelChange>,
    events: &Sender<SessionEvent>,
) -> bool {
    let mut out = Vec::new();
    let mut change = Change::NOTHING;
    // Whether a draft was *opened* in this batch (as against closed or
    // discarded), which is the only activation that can take held lines.
    let mut activated = false;

    while let Ok(command) = commands.try_recv() {
        match command {
            SessionCommand::Retranscribe { id } => retranscribes.push_back(id),
            // Activation is a flush point: a draft that has held lines waiting
            // for it gets them the moment it is the open one again. The flush
            // itself waits until the end of the drain, so the frontend hears
            // "this draft is active" before it is handed that draft's lines.
            SessionCommand::SetDraft(draft) => {
                activated = true;
                change.absorb(slot.apply(SessionCommand::SetDraft(draft), &mut out));
            }
            // The draft is on its way to `.trash`, and its held lines go with
            // it — writing them into a discarded note would resurrect it.
            // Taken before the discard, while the id is still reachable.
            SessionCommand::DiscardActive => {
                if let Some(id) = slot.draft.as_ref().map(|draft| draft.id().to_owned()) {
                    let before = held.len();
                    held.retain(|line| line.draft_id.as_deref() != Some(id.as_str()));
                    let dropped = before - held.len();
                    if dropped > 0 {
                        out.push(SessionEvent::Debug {
                            message: format!(
                                "{dropped} held line(s) went to .trash with the note they \
                                 belonged to"
                            ),
                        });
                    }
                }
                change.absorb(slot.apply(SessionCommand::DiscardActive, &mut out));
            }
            SessionCommand::Save {
                fallback_path,
                notes_root,
                adopt_project,
                header,
                dividers,
                overwrite,
            } => {
                let request = SaveRequest {
                    fallback_path: &fallback_path,
                    notes_root: notes_root.as_deref(),
                    adopt_project: adopt_project.as_deref(),
                    header: header.as_deref(),
                    dividers: SessionDividers::when(dividers),
                    mode: if overwrite {
                        SaveMode::Overwrite
                    } else {
                        SaveMode::Guarded
                    },
                };
                // **Before the render, not after.** A save renders whatever is
                // in the log, so a held line that goes in first is in the file;
                // one that goes in afterwards would have needed a second save.
                // A save with lines still held proceeds anyway — the file is
                // honestly behind, and the footer says by how many.
                change.absorb(flush_held(slot, held, &mut out));
                change.absorb(slot.save(&request, &mut out));
            }
            SessionCommand::SaveAll { projects } => {
                change.absorb(slot.save_all(&projects, &mut out));
            }
            // Nothing to report and nothing on disk to change: the switch is
            // read by the loop on its next pass, before the next utterance.
            SessionCommand::SetCues(on) => {
                tracing::info!(cues = on, "audio cues switched");
                *cues_on = on;
            }
            // Queued rather than applied, for the same reason a re-transcribe
            // is: the transcriber belongs to the worker loop. Queued *in order*
            // with each other, because a model swap carries the language it was
            // loaded with and a language change after it must still win.
            SessionCommand::SetTranscriber(next) => {
                model_changes.push_back(ModelChange::Swap(next));
            }
            SessionCommand::SetLanguage(language) => {
                model_changes.push_back(ModelChange::Language(language));
            }
            command => change.absorb(slot.apply(command, &mut out)),
        }
    }

    if change.activation {
        out.push(slot.changed_event());
    }
    // Between the two, and only for an *opened* draft: a close or a discard
    // leaves nothing to write into, and flushing there would resurrect held
    // lines into a draft nobody asked for.
    if activated {
        change.absorb(flush_held(slot, held, &mut out));
    }
    // After the activation event, never before: the frontend must have heard of
    // the draft before it is handed that draft's lines.
    if change.transcript {
        out.extend(slot.transcript_event());
    }
    send_all(events, out)
}

/// Forward any out-of-band reports, with their cue.
fn drain_control(
    control: &Receiver<Control>,
    events: &Sender<SessionEvent>,
    cues: Option<&CuePlayer>,
) {
    while let Ok(Control::EngineDead { reason }) = control.try_recv() {
        tracing::error!(%reason, "capture engine died mid-session");
        play(cues, Cue::Error);
        let _ = events.send(SessionEvent::EngineDead { reason });
    }
}

/// Move every waiting release into the pairing queue.
fn drain_releases(releases: &Receiver<ReleaseInfo>, pending: &mut VecDeque<ReleaseInfo>) {
    while let Ok(info) = releases.try_recv() {
        pending.push_back(info);
    }
}

/// How long this release has waited, if that is long enough to call it
/// orphaned. `None` while a chunk could still legitimately claim it.
///
/// Pure, with `now` injected, because this is the rule that decides whether a
/// user's timestamp survives and it has to be testable to the second.
fn stale_age(info: &ReleaseInfo, now: SystemTime) -> Option<Duration> {
    // `duration_since` errors when the release is in the *future*, which a
    // clock stepped backwards mid-session will do. Nothing younger than `now`
    // is stale, so that case keeps the release.
    let age = now.duration_since(info.released_at).ok()?;
    (age > RELEASE_STALE_AFTER).then_some(age)
}

/// Which release, if any, this utterance consumes.
///
/// The pairing rule in one function: a capped chunk takes nothing out of the
/// queue, because the cap fired while the key was still down and the release
/// waiting there (or arriving later) belongs to a chunk that has not been
/// delivered yet. Consuming it would shift every timestamp in the session by
/// one line — a latent bug that real use exposed.
///
/// The staleness sweep is defense in depth for the *other* direction.
/// Its one known producer — a toggle start swallowed inside the previous
/// stop's post-roll, so the recording never existed and the user's stop had
/// nothing to end — is fixed in [`audio`](crate::audio); nothing else should be
/// able to strand a release. If something does, this keeps the queue from
/// staying one deep for the rest of the session, which is what made the
/// original so hard to see: every later line quietly carried the previous
/// line's time. Only on the pairing path, never for a capped chunk: that is the
/// moment an orphan does its damage, and the moment we know a chunk is here.
fn take_release(
    capped: bool,
    pending: &mut VecDeque<ReleaseInfo>,
    now: SystemTime,
) -> Option<ReleaseInfo> {
    if capped {
        return None;
    }
    while let Some(age) = pending.front().and_then(|info| stale_age(info, now)) {
        pending.pop_front();
        tracing::warn!(
            waited_seconds = age.as_secs(),
            "a key release waited far longer than any recording can take; dropping it rather \
             than stamping this line with it"
        );
    }
    pending.pop_front()
}

/// Tell the user, immediately, that the recording was split.
///
/// Returns the event to send; the cue is played here so it lands before the
/// event travels and long before the decode does.
fn announce_cap(utterance: &Utterance, cues: Option<&CuePlayer>) -> Option<SessionEvent> {
    if !utterance.capped {
        return None;
    }
    play(cues, Cue::Capped);
    tracing::info!(
        cap_seconds = MAX_UTTERANCE.as_secs(),
        "the recording hit the cap and was split; capture is still running"
    );
    Some(SessionEvent::Capped { cap: MAX_UTTERANCE })
}

/// One accepted utterance, waiting for the model.
///
/// Everything decided at intake travels here: the timestamp, the token the
/// frontend already has a row for, and which way the clip was trimmed. The
/// samples come along as the whole [`Utterance`] rather than as a copy of the
/// trimmed slice — a move of a `Vec`, where a copy would be up to ~5.7 MB per
/// capped utterance.
#[derive(Debug)]
struct Pending {
    token: String,
    utterance: Utterance,
    clip: ClipDecision,
    spoken_at: DateTime<FixedOffset>,
}

/// A transcribed line the disk refused, kept until it can be written.
///
/// Unbounded on purpose: a disk-full episode is short, ~5.7 MB per
/// capped line is the price of "nothing is discarded because a dependency
/// failed", and the alternative — a cap — is a rule that throws away the user's
/// words at exactly the moment they are hardest to get back. Never persisted:
/// these exist *because* writes are failing, so there is nowhere truthful to
/// put them.
#[derive(Debug)]
struct HeldLine {
    token: String,
    /// The draft it belongs to, or `None` when the draft itself could not be
    /// created. `None` lands in whatever draft is active at flush time.
    draft_id: Option<String>,
    text: String,
    spoken_at: DateTime<FixedOffset>,
    samples: Vec<f32>,
    transcribe_ms: Option<u64>,
    /// A failed line whose *append* also failed. Held with its state intact, so
    /// the flush writes the same failed record the decode meant to write.
    failed: bool,
}

impl HeldLine {
    /// How much audio it holds, in seconds.
    fn seconds(&self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            self.samples.len() as f32 / TARGET_SAMPLE_RATE as f32
        }
    }
}

/// Stage one: pair, stamp, gate, and announce one utterance as queued.
///
/// Everything that can say "this was never a line" happens **here**, before a
/// row exists, so a skip can never flash a row into being and out of it again.
/// What survives gets a token and a [`SessionEvent::Queued`], and waits.
fn intake(
    utterance: Utterance,
    release: Option<ReleaseInfo>,
    cues: Option<&CuePlayer>,
    out: &mut Vec<SessionEvent>,
) -> Option<Pending> {
    let spoken_at = match release {
        Some(info) => local_time(info.released_at),
        // The cap split the recording. There is no release by design, and
        // `now()` *is* the split moment, so this is the correct timestamp for
        // the line rather than a fallback. Not a warning, then.
        None if utterance.capped => {
            tracing::debug!(
                samples = utterance.samples.len(),
                "stamping a capped chunk at the split"
            );
            local_time(SystemTime::now())
        }
        None => {
            // A release was swallowed (an elevated window had focus — see
            // `hotkey`). Keep the audio; a note stamped a second late beats a
            // note thrown away.
            //
            // Stamped at *intake*, which is strictly more
            // truthful than the old stamp: this happens as the audio
            // arrives, rather than after however many earlier utterances were
            // still waiting for the model.
            tracing::warn!(
                samples = utterance.samples.len(),
                "an utterance arrived with no matching key release; stamping it now"
            );
            local_time(SystemTime::now())
        }
    };

    // The hold gate cannot apply to a capped chunk: it ran for the whole cap by
    // definition, and it has no release to measure. `release` is `None` there,
    // so this is already a no-op — stated because it is load-bearing, not
    // incidental.
    if let Some(reason) = release.and_then(|info| hold_gate(info.held)) {
        tracing::debug!(%reason, "utterance skipped");
        out.push(SessionEvent::Skipped {
            reason,
            token: None,
        });
        return None;
    }

    // Everything below sees the same slice: gate, model, and the wav that is
    // written. Nothing on disk is touched by this — the trim happens before the
    // audio is a note at all, and wavs already stored keep whatever they hold
    // (invariant 4). The *decision* is taken here and carried, so the clip the
    // model sees is the one this cue state produced.
    let decision = ClipDecision::of(&utterance, cues.is_some());
    let clip = Clip::from(&utterance, decision);

    if let Some(reason) = level_gate(clip, utterance.normalize_gain) {
        tracing::debug!(%reason, "utterance skipped");
        out.push(SessionEvent::Skipped {
            reason,
            token: None,
        });
        return None;
    }

    let token = Ulid::new().to_string();
    out.push(SessionEvent::Queued {
        token: token.clone(),
        seconds: clip.seconds(),
        spoken_at,
    });
    Some(Pending {
        token,
        utterance,
        clip: decision,
        spoken_at,
    })
}

/// Stage two: transcribe one waiting utterance and store what came back.
///
/// Every outcome ends with the audio somewhere it can be got at again — a
/// line, a failed line that keeps its wav, or a held line in memory. Nothing
/// here drops samples on the floor.
fn decode(
    pending: Pending,
    transcriber: &mut Transcriber,
    slot: &mut DraftSlot,
    held: &mut VecDeque<HeldLine>,
    cues: Option<&CuePlayer>,
    out: &mut Vec<SessionEvent>,
) -> Change {
    let Pending {
        token,
        utterance,
        clip,
        spoken_at,
    } = pending;
    out.push(SessionEvent::Decoding {
        token: token.clone(),
    });

    let clip = Clip::from(&utterance, clip);
    let transcript = match transcriber.transcribe(clip.samples()) {
        Ok(transcript) => transcript,
        Err(err) => {
            // **The audio survives.** This once returned an error
            // and dropped the samples: the user spoke, the app heard, and the
            // recording was gone. Now it becomes a line with no words that
            // keeps its wav and offers Retry — the design's "nothing is
            // discarded because a dependency failed".
            let message = format!("line failed to transcribe: {}", flatten(&err));
            out.push(SessionEvent::Debug { message });
            return store_line(
                slot,
                held,
                out,
                cues,
                StoredLine {
                    token,
                    text: String::new(),
                    spoken_at,
                    samples: clip.samples(),
                    transcribe_ms: None,
                    failed: true,
                },
            );
        }
    };

    if let Some(reason) = text_gate(&transcript.text) {
        tracing::debug!(%reason, "utterance skipped");
        out.push(SessionEvent::Skipped {
            reason,
            token: Some(token),
        });
        return Change::NOTHING;
    }

    store_line(
        slot,
        held,
        out,
        cues,
        StoredLine {
            token,
            text: transcript.text.trim().to_owned(),
            spoken_at,
            samples: clip.samples(),
            transcribe_ms: u64::try_from(transcript.duration.as_millis()).ok(),
            failed: false,
        },
    )
}

/// One line on its way to disk, whatever produced it.
#[derive(Debug)]
struct StoredLine<'a> {
    token: String,
    text: String,
    spoken_at: DateTime<FixedOffset>,
    samples: &'a [f32],
    transcribe_ms: Option<u64>,
    failed: bool,
}

/// What one append did, with the "a draft had to be created for it"
/// announcement already on `out`.
///
/// The announcement is pushed on **both** outcomes, which is the point of this
/// wrapper: [`DraftSlot::append`] creates the draft before it writes, so a
/// write that fails can leave a brand-new draft active that the frontend has
/// never heard of — and the held line that comes out of that failure names it.
struct Appended {
    result: Result<LineRecord, DraftError>,
    change: Change,
}

fn append_line(slot: &mut DraftSlot, out: &mut Vec<SessionEvent>, new: NewLine<'_>) -> Appended {
    let before = slot.draft.as_ref().map(|draft| draft.id().to_owned());
    match slot.append(new) {
        Ok((record, created)) => {
            out.extend(created);
            Appended {
                result: Ok(record),
                change: Change::TRANSCRIPT,
            }
        }
        Err(err) => {
            let after = slot.draft.as_ref().map(|draft| draft.id().to_owned());
            let created = after != before;
            if created {
                out.push(slot.changed_event());
            }
            Appended {
                result: Err(err),
                // An empty new draft is still a transcript the window has to be
                // shown; nothing else changed.
                change: if created {
                    Change::TRANSCRIPT
                } else {
                    Change::NOTHING
                },
            }
        }
    }
}

/// Append one decoded line, or hold it if the disk will not take it.
///
/// The **one** path a fresh line takes, so the failed line and the ordinary one
/// get the same wav, the same fsync and the same crash story.
fn store_line(
    slot: &mut DraftSlot,
    held: &mut VecDeque<HeldLine>,
    out: &mut Vec<SessionEvent>,
    cues: Option<&CuePlayer>,
    line: StoredLine<'_>,
) -> Change {
    // **Before the new append, never after.** Anything already held was spoken
    // earlier than this line, so it has to go in first or the note would read
    // out of order — and if the flush fails, this line queues up behind it
    // rather than jumping the queue.
    let mut change = flush_held(slot, held, out);

    let appended = append_line(
        slot,
        out,
        NewLine {
            text: line.text.clone(),
            spoken_at: line.spoken_at,
            samples: line.samples,
            transcribe_ms: line.transcribe_ms,
            failed: line.failed,
        },
    );
    change.absorb(appended.change);

    match appended.result {
        Ok(record) => {
            // Exactly one cue per utterance: the blip that says a line landed,
            // or the error tone that says one came back empty.
            play(cues, if line.failed { Cue::Error } else { Cue::Saved });
            out.push(SessionEvent::LineAdded {
                record: Box::new(record),
                token: Some(line.token),
                held_flush: false,
            });
        }
        Err(err) => {
            play(cues, Cue::Error);
            let entry = HeldLine {
                token: line.token,
                draft_id: slot.draft.as_ref().map(|draft| draft.id().to_owned()),
                text: line.text,
                spoken_at: line.spoken_at,
                samples: line.samples.to_vec(),
                transcribe_ms: line.transcribe_ms,
                failed: line.failed,
            };
            out.push(SessionEvent::Debug {
                message: format!(
                    "a line could not be written and is held in memory: {}",
                    flatten(&err)
                ),
            });
            out.push(SessionEvent::Held {
                token: entry.token.clone(),
                draft_id: entry.draft_id.clone(),
                text: entry.text.clone(),
                seconds: entry.seconds(),
                spoken_at: entry.spoken_at,
            });
            held.push_back(entry);
        }
    }
    change
}

/// Try to write every held line that belongs to the draft now open.
///
/// Oldest first, and **stopping at the first failure**: the queue is spoken
/// order, and letting a later line past a stuck one would rearrange the note.
/// A line pinned to a draft that is not open is stepped over rather than
/// failed — order only means anything within one note — and stays queued for
/// that draft's own activation.
///
/// A failed attempt is quiet: one debug entry, no footer message. The count on
/// screen is the surface, and a message per retry would be the footer flood
/// this design deliberately removed.
fn flush_held(
    slot: &mut DraftSlot,
    held: &mut VecDeque<HeldLine>,
    out: &mut Vec<SessionEvent>,
) -> Change {
    if held.is_empty() {
        return Change::NOTHING;
    }
    let active = slot.draft.as_ref().map(|draft| draft.id().to_owned());
    let mut change = Change::NOTHING;
    let mut waiting: VecDeque<HeldLine> = VecDeque::with_capacity(held.len());
    let mut stopped = false;

    while let Some(line) = held.pop_front() {
        let mine = line.draft_id.is_none() || line.draft_id == active;
        if stopped || !mine {
            waiting.push_back(line);
            continue;
        }
        let appended = append_line(
            slot,
            out,
            NewLine {
                text: line.text.clone(),
                spoken_at: line.spoken_at,
                samples: &line.samples,
                transcribe_ms: line.transcribe_ms,
                failed: line.failed,
            },
        );
        change.absorb(appended.change);
        match appended.result {
            Ok(record) => {
                out.push(SessionEvent::LineAdded {
                    record: Box::new(record),
                    token: Some(line.token),
                    held_flush: true,
                });
            }
            Err(err) => {
                out.push(SessionEvent::Debug {
                    message: format!("a held line still could not be written: {}", flatten(&err)),
                });
                waiting.push_back(line);
                stopped = true;
            }
        }
    }

    *held = waiting;
    change
}

/// Play a cue if there is a player. Cues never gate anything.
fn play(cues: Option<&CuePlayer>, cue: Cue) {
    if let Some(player) = cues {
        player.play(cue);
    }
}

/// An error and its whole `source` chain as one line.
///
/// The frontend shows one string; the causes are where the actionable detail
/// lives (a path, a device name), so they must not be dropped.
fn flatten(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    tracing::error!(%message, "session failure");
    message
}

// ---------------------------------------------------------------------------
// Gates (pure, no audio device, no model)
// ---------------------------------------------------------------------------

/// Layer one: was the key held long enough to be speech?
fn hold_gate(held: Duration) -> Option<SkipReason> {
    (held < MIN_HOLD).then_some(SkipReason::TooShort { held })
}

/// The samples of one utterance, carrying in the *type* whether our own begin
/// cue has already been cut off the front.
///
/// This exists so the contaminated head can never be skipped twice. A boolean
/// threaded into the level gate's arithmetic would sooner or later subtract the
/// same 850 ms both at trim time and at measure time, and the symptom — the
/// first second of every note silently unjudged — is invisible in a review.
/// Here the two cases cannot share a code path: a [`Clip::Trimmed`] has no head
/// left to skip and is measured whole, a [`Clip::Untrimmed`] is measured
/// through [`speech_window`] exactly as before.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Clip<'a> {
    /// Nothing was removed: no cue player exists (so no beep was ever emitted),
    /// or this is a cap-split continuation whose head is live speech.
    Untrimmed(&'a [f32]),
    /// The pre-roll and the begin cue are already gone.
    Trimmed(&'a [f32]),
}

/// Which branch [`Clip::from`] rebuilds, as a value with no lifetime.
///
/// The decision is made at **intake**, with the cue state that was in force
/// then, and carried on the queue until the decode: the beep that
/// did or did not sound for that utterance is already history, so a cue switch
/// flipped while the utterance waits must not retroactively change what was
/// trimmed off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipDecision {
    /// Nothing to remove.
    Untrimmed,
    /// The pre-roll and the begin cue come off the front.
    Trimmed,
}

impl<'a> Clip<'a> {
    /// Decide once, at the top of the pipeline, what this utterance's samples
    /// are — and hand back a value everything downstream must go through, so
    /// the gate, whisper and the stored wav cannot disagree about it.
    ///
    /// `cued` is whether the session has a cue player at all: `None` there
    /// means the output stream never opened, no beep was ever emitted, and
    /// there is nothing to trim.
    ///
    /// Both halves in one call. The pipeline itself takes them apart — the
    /// decision is made at intake and the clip rebuilt at decode (see
    /// [`ClipDecision`]) — so this is now the gate tests' form, where the two
    /// happen in the same breath.
    #[cfg(test)]
    fn of(utterance: &'a Utterance, cued: bool) -> Self {
        Self::from(utterance, ClipDecision::of(utterance, cued))
    }

    /// The same clip, rebuilt from a decision taken earlier.
    fn from(utterance: &'a Utterance, decision: ClipDecision) -> Self {
        match decision {
            ClipDecision::Untrimmed => Self::Untrimmed(&utterance.samples),
            // A clip shorter than the trim becomes empty. No special case: the
            // level gate reads empty as −∞ dBFS and skips it with a reason.
            ClipDecision::Trimmed => {
                Self::Trimmed(utterance.samples.get(TRIMMED_SAMPLES..).unwrap_or(&[]))
            }
        }
    }

    /// Everything downstream keeps: what whisper decodes and what the wav
    /// stores.
    fn samples(self) -> &'a [f32] {
        match self {
            Self::Untrimmed(samples) | Self::Trimmed(samples) => samples,
        }
    }

    /// How much audio this is, in seconds — "what was heard", for a queued row.
    fn seconds(self) -> f32 {
        #[allow(clippy::cast_precision_loss)]
        {
            self.samples().len() as f32 / TARGET_SAMPLE_RATE as f32
        }
    }

    /// The part of the clip the level gate is allowed to judge.
    fn measured(self) -> &'a [f32] {
        match self {
            Self::Untrimmed(samples) => speech_window(samples),
            // The head is gone already; skipping another one would judge the
            // note on its own second half.
            Self::Trimmed(samples) => samples,
        }
    }
}

impl ClipDecision {
    /// Which way the clip goes for this utterance right now — the decision
    /// [`Clip::from`] rebuilds from at decode time.
    const fn of(utterance: &Utterance, cued: bool) -> Self {
        // A continuation is seeded from pre-roll mid-speech and no begin cue
        // sounds at that seam. The *Capped* cue does bleed in there, over live
        // speech — deliberately left alone: a split promises to lose nothing,
        // and that promise outranks the beep.
        if !cued || utterance.continued {
            Self::Untrimmed
        } else {
            Self::Trimmed
        }
    }
}

/// Layer two: was there any signal at all?
///
/// Two corrections have to be applied before the number means anything.
///
/// **Normalization.** The utterance arrives peak-normalized, so a room-tone clip
/// can look as loud as a shout. `normalize_gain` is exactly the factor that was
/// applied, so dividing it back out recovers the level the microphone actually
/// delivered — which is the only level worth thresholding.
///
/// **Our own begin cue.** The cue plays through the speakers and the microphone
/// hears it. Measured on the first agentic loopback run: the beep occupies
/// roughly [400 ms, 800 ms] of *every* clip at −10…−22 dBFS — the press sits at
/// the [`PRE_ROLL_SAMPLES`] mark by construction, and the 150 ms tone plus
/// output latency and room decay covers the rest. Whole-clip RMS of a silent
/// hold therefore clears the floor comfortably, the gate passes, and whisper
/// duly hallucinates "Thank you" chains out of the beep. Which is why the level
/// is never measured over that head — either it was already cut off the clip
/// ([`Clip::Trimmed`]) or [`speech_window`] steps over it here.
fn level_gate(clip: Clip<'_>, normalize_gain: f32) -> Option<SkipReason> {
    let level_dbfs = original_level_dbfs(clip.measured(), normalize_gain);
    (level_dbfs < SILENCE_FLOOR_DBFS).then_some(SkipReason::TooQuiet { level_dbfs })
}

/// The part of an utterance that can only contain the user's voice.
///
/// Everything before [`CONTAMINATED_SAMPLES`] is pre-roll plus our own cue, and
/// judging a hold by that is judging our own beep.
///
/// The fallback is deliberate: a clip too short to spare the head is measured
/// whole. Transcribing a borderline clip costs a possibly-junk line, while
/// skipping one costs a finding the user actually spoke — and the hold gate has
/// already thrown out the taps, so anything reaching here was a deliberate hold.
fn speech_window(samples: &[f32]) -> &[f32] {
    match samples.get(CONTAMINATED_SAMPLES..) {
        Some(tail) if tail.len() >= MIN_MEASURED_SAMPLES => tail,
        _ => samples,
    }
}

/// RMS of the audio as captured, in dBFS.
///
/// An empty or all-zero buffer is negative infinity, which is below every
/// possible floor — correct, and it keeps the caller free of special cases.
fn original_level_dbfs(samples: &[f32], normalize_gain: f32) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    let rms = (sum / samples.len() as f64).sqrt() as f32;
    // A gain of zero or worse cannot have been applied; treat it as none rather
    // than dividing by it.
    let gain = if normalize_gain > 0.0 {
        normalize_gain
    } else {
        1.0
    };
    dbfs(rms / gain)
}

/// A linear amplitude as dBFS. Silence is negative infinity, not an error.
fn dbfs(level: f32) -> f32 {
    if level > 0.0 {
        20.0 * level.log10()
    } else {
        f32::NEG_INFINITY
    }
}

/// Layer three: did the model produce any words?
fn text_gate(text: &str) -> Option<SkipReason> {
    text.trim().is_empty().then_some(SkipReason::NoSpeech)
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// A hook-captured `SystemTime` as a local-offset timestamp.
///
/// The draft store writes RFC3339 with the offset baked in, so a note dictated
/// at 21:40 still reads 21:40 next spring. Kept as a free function so the
/// conversion is testable without a session.
fn local_time(time: SystemTime) -> DateTime<FixedOffset> {
    DateTime::<Local>::from(time).fixed_offset()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test here is pure arithmetic or pure text. No device, no model, no
    // GPU, no window — that is the rule for `cargo test` in this repo.

    /// A tone at a known RMS level, so the level gate can be aimed precisely.
    /// A sine's RMS is its amplitude over root two.
    fn tone_at_dbfs(level_dbfs: f32, len: usize) -> Vec<f32> {
        let rms = 10.0f32.powf(level_dbfs / 20.0);
        let amplitude = rms * std::f32::consts::SQRT_2;
        (0..len)
            .map(|i| amplitude * (i as f32 * 0.31).sin())
            .collect()
    }

    #[test]
    fn the_hold_gate_discards_taps_and_keeps_real_holds() {
        assert!(hold_gate(Duration::ZERO).is_some());
        assert!(hold_gate(Duration::from_millis(399)).is_some());
        // Exactly the minimum is a keeper: the constant is a floor, not a
        // threshold to clear.
        assert!(hold_gate(MIN_HOLD).is_none());
        assert!(hold_gate(Duration::from_millis(401)).is_none());
        assert!(hold_gate(Duration::from_secs(30)).is_none());
    }

    #[test]
    fn the_hold_gate_reports_what_it_measured() {
        let held = Duration::from_millis(120);
        assert_eq!(hold_gate(held), Some(SkipReason::TooShort { held }));
    }

    #[test]
    fn digital_silence_never_reaches_whisper() {
        // The regression this gate exists for: three seconds of zeroes
        // transcribed as "you".
        assert!(matches!(
            level_gate(Clip::Untrimmed(&[0.0; 48_000]), 1.0),
            Some(SkipReason::TooQuiet { .. })
        ));
        assert!(matches!(
            level_gate(Clip::Untrimmed(&[]), 1.0),
            Some(SkipReason::TooQuiet { .. })
        ));
    }

    #[test]
    fn room_tone_at_minus_60_dbfs_is_silence() {
        let samples = tone_at_dbfs(-60.0, 16_000);
        assert!(matches!(
            level_gate(Clip::Untrimmed(&samples), 1.0),
            Some(SkipReason::TooQuiet { .. })
        ));
    }

    #[test]
    fn speech_at_minus_20_dbfs_passes() {
        let samples = tone_at_dbfs(-20.0, 16_000);
        assert_eq!(level_gate(Clip::Untrimmed(&samples), 1.0), None);
    }

    #[test]
    fn normalization_is_undone_before_the_level_is_judged() {
        // What the engine delivers for near-silence: quiet audio multiplied up
        // to the normalization target. Judging the delivered level would let
        // every silent clip through.
        let quiet = tone_at_dbfs(-60.0, 16_000);
        let gain = 30.0f32;
        let boosted: Vec<f32> = quiet.iter().map(|s| s * gain).collect();

        // Without the undo this reads as −30 dBFS and passes.
        assert_eq!(level_gate(Clip::Untrimmed(&boosted), 1.0), None);
        // With it, the original −60 dBFS is what gets judged.
        assert!(matches!(
            level_gate(Clip::Untrimmed(&boosted), gain),
            Some(SkipReason::TooQuiet { .. })
        ));
    }

    /// A clip shaped like the ones the loopback rig produced: silence, with our
    /// own begin cue bleeding in from the speakers over [450 ms, 600 ms).
    fn clip_with_cue_bleed(total_ms: usize) -> Vec<f32> {
        let mut samples = vec![0.0f32; samples_at_16k(Duration::from_millis(total_ms as u64))];
        let start = samples_at_16k(Duration::from_millis(450));
        let cue = tone_at_dbfs(-16.0, samples_at_16k(Duration::from_millis(150)));
        for (slot, value) in samples.iter_mut().skip(start).zip(cue) {
            *slot = value;
        }
        samples
    }

    #[test]
    fn our_own_begin_cue_does_not_open_the_gate() {
        // The bug the first agentic run found: the cue is 30+ dB above the
        // floor, so a silent hold measured whole reads as speech and whisper
        // hallucinates a line out of the beep.
        let clip = clip_with_cue_bleed(3_000);

        // Whole-clip measurement is exactly what used to pass.
        assert!(original_level_dbfs(&clip, 1.0) > SILENCE_FLOOR_DBFS);
        // Measuring past the contaminated head is what must not.
        assert!(
            matches!(
                level_gate(Clip::Untrimmed(&clip), 1.0),
                Some(SkipReason::TooQuiet { .. })
            ),
            "a silent hold with only the cue in it must be skipped"
        );
    }

    #[test]
    fn speech_after_the_cue_still_passes() {
        let mut clip = clip_with_cue_bleed(3_000);
        let speech_start = CONTAMINATED_SAMPLES;
        let speech = tone_at_dbfs(-25.0, clip.len() - speech_start);
        for (slot, value) in clip.iter_mut().skip(speech_start).zip(speech) {
            *slot = value;
        }
        assert_eq!(level_gate(Clip::Untrimmed(&clip), 1.0), None);
    }

    #[test]
    fn the_speech_window_starts_after_the_pre_roll_and_the_cue() {
        // 400 ms of pre-roll + 450 ms of cue, at 16 kHz.
        assert_eq!(PRE_ROLL_SAMPLES, 6_400);
        assert_eq!(CONTAMINATED_SAMPLES, 13_600);
        assert_eq!(MIN_MEASURED_SAMPLES, 3_200);

        let long = vec![0.0f32; CONTAMINATED_SAMPLES + MIN_MEASURED_SAMPLES];
        assert_eq!(speech_window(&long).len(), MIN_MEASURED_SAMPLES);
    }

    #[test]
    fn a_clip_too_short_to_skip_the_head_is_measured_whole() {
        // One sample short of leaving a measurable tail: fall back rather than
        // judge a clip on nothing. The hold gate has already caught the taps,
        // so a borderline clip here is better transcribed than lost.
        let short = CONTAMINATED_SAMPLES + MIN_MEASURED_SAMPLES - 1;
        let silent = vec![0.0f32; short];
        assert_eq!(speech_window(&silent).len(), short);
        assert!(speech_window(&[]).is_empty());

        // And the fallback still decides both ways on its merits.
        let loud = tone_at_dbfs(-20.0, short);
        assert_eq!(level_gate(Clip::Untrimmed(&loud), 1.0), None);
        assert!(matches!(
            level_gate(Clip::Untrimmed(&silent), 1.0),
            Some(SkipReason::TooQuiet { .. })
        ));
    }

    // -----------------------------------------------------------------------
    // The cue trim. Pure slicing: no device, no cue player, no model.
    // -----------------------------------------------------------------------

    /// An utterance carrying real samples, so the trim can be watched.
    fn clip_utterance(samples: Vec<f32>, continued: bool) -> Utterance {
        Utterance {
            samples,
            sample_rate: TARGET_SAMPLE_RATE,
            normalize_gain: 1.0,
            native: None,
            // Independent of `continued`: the trim asks how the chunk *began*,
            // never how it ended.
            capped: false,
            continued,
        }
    }

    #[test]
    fn a_press_started_clip_loses_the_beep_before_anything_sees_it() {
        // The complaint this fixed: the beep was in the playback. The trim is
        // what whisper decodes *and* what the wav stores, so this one slice
        // covers both.
        let with_bleed = clip_with_cue_bleed(3_000);
        let utterance = clip_utterance(with_bleed.clone(), false);

        let clip = Clip::of(&utterance, true);
        assert!(matches!(clip, Clip::Trimmed(_)));
        assert_eq!(clip.samples().len(), with_bleed.len() - TRIMMED_SAMPLES);
        // 400 ms of pre-roll + 450 ms of cue, at 16 kHz — and the bleed the
        // rig recorded over [450 ms, 600 ms) is entirely inside it.
        assert_eq!(TRIMMED_SAMPLES, 13_600);
        assert!(
            clip.samples().iter().all(|s| *s == 0.0),
            "the begin cue survived the trim"
        );
    }

    #[test]
    fn without_a_cue_player_nothing_is_trimmed_and_the_old_gate_still_applies() {
        // No output stream means no beep was ever emitted, so there is nothing
        // to cut — and the level gate goes on stepping over the head itself.
        let utterance = clip_utterance(clip_with_cue_bleed(3_000), false);

        let clip = Clip::of(&utterance, false);
        assert_eq!(clip, Clip::Untrimmed(&utterance.samples));
        assert_eq!(clip.samples().len(), utterance.samples.len());
        assert_eq!(
            clip.measured().len(),
            utterance.samples.len() - CONTAMINATED_SAMPLES
        );
        assert!(matches!(
            level_gate(clip, 1.0),
            Some(SkipReason::TooQuiet { .. })
        ));
    }

    #[test]
    fn a_cap_split_continuation_is_never_trimmed() {
        // The seam is seeded from pre-roll mid-speech and no begin cue sounds
        // there; trimming it would cut 850 ms out of the middle of a sentence.
        let speech = tone_at_dbfs(-25.0, 48_000);
        let utterance = clip_utterance(speech.clone(), true);

        let clip = Clip::of(&utterance, true);
        assert_eq!(clip, Clip::Untrimmed(&utterance.samples));
        assert_eq!(clip.samples().len(), speech.len());
    }

    #[test]
    fn the_contaminated_head_is_never_skipped_twice() {
        // The failure this restructure exists to make impossible: trim the
        // head off, then have the gate skip another head's worth and judge the
        // note on what is left. Speech sits exactly in the window a second
        // skip would step over, and silence follows it.
        let total = TRIMMED_SAMPLES + CONTAMINATED_SAMPLES + MIN_MEASURED_SAMPLES;
        let mut samples = vec![0.0f32; total];
        let speech = tone_at_dbfs(-20.0, CONTAMINATED_SAMPLES);
        for (slot, value) in samples.iter_mut().skip(TRIMMED_SAMPLES).zip(speech) {
            *slot = value;
        }
        let utterance = clip_utterance(samples, false);
        let clip = Clip::of(&utterance, true);

        // A trimmed clip is measured whole — the head is already gone.
        assert_eq!(clip.measured().len(), clip.samples().len());
        assert_eq!(
            level_gate(clip, 1.0),
            None,
            "a double skip would have measured only the silent tail"
        );
    }

    #[test]
    fn a_clip_shorter_than_the_trim_becomes_empty_and_is_skipped_not_panicked_on() {
        for len in [0, 1, TRIMMED_SAMPLES - 1, TRIMMED_SAMPLES] {
            let utterance = clip_utterance(tone_at_dbfs(-20.0, len), false);
            let clip = Clip::of(&utterance, true);
            assert!(clip.samples().is_empty(), "{len} samples left something");
            assert!(
                matches!(level_gate(clip, 1.0), Some(SkipReason::TooQuiet { .. })),
                "an empty clip must be skipped with a reason"
            );
        }
    }

    #[test]
    fn a_nonsense_gain_is_treated_as_no_gain_rather_than_dividing_by_zero() {
        let samples = tone_at_dbfs(-20.0, 8_000);
        assert_eq!(level_gate(Clip::Untrimmed(&samples), 0.0), None);
        assert!(original_level_dbfs(&samples, 0.0).is_finite());
    }

    #[test]
    fn the_measured_level_is_the_one_reported() {
        let samples = tone_at_dbfs(-70.0, 16_000);
        match level_gate(Clip::Untrimmed(&samples), 1.0) {
            Some(SkipReason::TooQuiet { level_dbfs }) => {
                assert!(
                    (level_dbfs - (-70.0)).abs() < 1.0,
                    "reported {level_dbfs} dBFS for a −70 dBFS clip"
                );
            }
            other => panic!("expected a TooQuiet skip, got {other:?}"),
        }
    }

    #[test]
    fn the_empty_text_gate_catches_whitespace_only_transcripts() {
        assert_eq!(text_gate(""), Some(SkipReason::NoSpeech));
        assert_eq!(text_gate("   "), Some(SkipReason::NoSpeech));
        assert_eq!(text_gate("\t\n "), Some(SkipReason::NoSpeech));
        assert_eq!(text_gate("you"), None);
        assert_eq!(text_gate("the menu button does nothing"), None);
    }

    /// An utterance with nothing in it but the flag under test. No device is
    /// involved: `Utterance` is a plain struct.
    fn utterance(capped: bool) -> Utterance {
        Utterance {
            samples: vec![0.0; 16],
            sample_rate: TARGET_SAMPLE_RATE,
            normalize_gain: 1.0,
            native: None,
            capped,
            continued: false,
        }
    }

    fn release_at(secs: u64) -> ReleaseInfo {
        ReleaseInfo {
            released_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            held: Duration::from_secs(2),
        }
    }

    /// A pairing moment a second after the release under test: the ordinary
    /// case, where nothing is anywhere near stale.
    fn just_after(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs + 1)
    }

    #[test]
    fn a_capped_chunk_never_consumes_a_release() {
        // The bug real use found: the user's stop-press after the cap had no utterance
        // left to pair with, and would have stamped the *next* note.
        let mut pending = VecDeque::from([release_at(1_700_000_000)]);

        assert_eq!(
            take_release(true, &mut pending, just_after(1_700_000_000)),
            None
        );
        assert_eq!(pending.len(), 1, "the release must still be waiting");

        // And it goes to the chunk the user actually ended.
        assert_eq!(
            take_release(false, &mut pending, just_after(1_700_000_000)),
            Some(release_at(1_700_000_000))
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn an_ordinary_chunk_with_no_release_waiting_still_gets_none() {
        let mut pending = VecDeque::new();
        let now = just_after(1_700_000_000);
        assert_eq!(take_release(false, &mut pending, now), None);
        assert_eq!(take_release(true, &mut pending, now), None);
    }

    #[test]
    fn a_release_is_only_stale_strictly_past_the_bound() {
        let info = release_at(1_000);
        let at = |offset: Duration| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000) + offset;

        // The whole point of the bound: a release that has waited through one
        // very slow decode is still its chunk's release.
        assert_eq!(stale_age(&info, at(Duration::ZERO)), None);
        assert_eq!(stale_age(&info, at(RELEASE_STALE_AFTER)), None, "boundary");
        assert_eq!(
            stale_age(&info, at(RELEASE_STALE_AFTER + Duration::from_secs(1))),
            Some(RELEASE_STALE_AFTER + Duration::from_secs(1))
        );
    }

    #[test]
    fn a_release_from_the_future_is_never_stale() {
        // An NTP step backwards mid-session must not make Sotone throw away the
        // timestamp it is about to use.
        let info = release_at(2_000);
        assert_eq!(stale_age(&info, SystemTime::UNIX_EPOCH), None);
    }

    #[test]
    fn an_orphaned_release_is_dropped_so_the_next_line_keeps_its_own_time() {
        // The desync, staged: an orphan at the front and the real release
        // of the chunk arriving now behind it. Paired blind, this line would
        // carry the orphan's time and every later line would be one behind.
        let orphan = release_at(1_000);
        let mine = release_at(1_000 + RELEASE_STALE_AFTER.as_secs() + 30);
        let mut pending = VecDeque::from([orphan, mine]);
        let now = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_000)
            + RELEASE_STALE_AFTER
            + Duration::from_secs(31);

        assert_eq!(take_release(false, &mut pending, now), Some(mine));
        assert!(pending.is_empty(), "the orphan is gone, not just skipped");
    }

    #[test]
    fn a_run_of_orphans_clears_in_one_pairing() {
        // Whatever stranded one release could strand several; the queue must
        // not need one utterance per orphan to come back into step.
        let mut pending = VecDeque::from([release_at(0), release_at(1), release_at(2)]);
        let now = SystemTime::UNIX_EPOCH + RELEASE_STALE_AFTER + Duration::from_secs(10);
        assert_eq!(take_release(false, &mut pending, now), None);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_stale_release_survives_a_capped_chunk() {
        // A capped chunk is not a pairing, so it decides nothing about the
        // queue — including this.
        let mut pending = VecDeque::from([release_at(0)]);
        let now = SystemTime::UNIX_EPOCH + RELEASE_STALE_AFTER + Duration::from_secs(10);
        assert_eq!(take_release(true, &mut pending, now), None);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn only_a_capped_chunk_is_announced_and_it_carries_the_cap() {
        // Cues are `None` here: no output device in `cargo test`.
        assert_eq!(announce_cap(&utterance(false), None), None);
        assert_eq!(
            announce_cap(&utterance(true), None),
            Some(SessionEvent::Capped { cap: MAX_UTTERANCE })
        );
        // The message the frontend renders has to be the real cap, not a
        // duplicated literal that can drift from `audio`.
        assert_eq!(MAX_UTTERANCE, Duration::from_secs(120));
    }

    #[test]
    fn release_times_survive_the_trip_into_a_local_timestamp() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        let stamped = local_time(time);
        let back = SystemTime::from(stamped);
        let drift = back
            .duration_since(time)
            .or_else(|_| time.duration_since(back))
            .expect("the two must differ by a finite amount");
        assert!(drift < Duration::from_micros(1), "drifted by {drift:?}");
    }

    #[test]
    fn two_releases_a_second_apart_stay_a_second_apart_and_in_order() {
        // Rapid-fire notes keeping their spoken order is the whole reason the
        // timestamp is taken at release.
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let second = first + Duration::from_secs(1);
        let (a, b) = (local_time(first), local_time(second));
        assert!(a < b);
        assert_eq!((b - a).num_seconds(), 1);
    }

    #[test]
    fn skip_reasons_read_as_a_sentence() {
        // These strings go on screen next to a missing note; they have to say
        // why, not just that.
        let too_short = SkipReason::TooShort {
            held: Duration::from_millis(120),
        }
        .to_string();
        assert!(too_short.contains("120"), "{too_short}");
        let too_quiet = SkipReason::TooQuiet { level_dbfs: -71.25 }.to_string();
        assert!(
            too_quiet.contains("-71.2") || too_quiet.contains("-71.3"),
            "{too_quiet}"
        );
        assert!(SkipReason::NoSpeech.to_string().contains("no words"));
    }

    // -----------------------------------------------------------------------
    // The draft slot. Filesystem only — no model, no device, no window, which
    // is why the lifecycle lives in `DraftSlot` and not inline in the loop.
    // -----------------------------------------------------------------------

    fn slot() -> (tempfile::TempDir, DraftSlot) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let store = crate::draft::DraftStore::new(tmp.path().join("drafts"));
        (
            tmp,
            DraftSlot {
                store,
                project: Some("playtest".to_owned()),
                draft: None,
                sittings: HashMap::new(),
            },
        )
    }

    fn spoken(text: &str) -> NewLine<'static> {
        NewLine {
            text: text.to_owned(),
            spoken_at: DateTime::parse_from_rfc3339("2026-08-04T10:00:00+02:00")
                .expect("test timestamp"),
            samples: &[],
            transcribe_ms: None,
            failed: false,
        }
    }

    fn active_id(slot: &DraftSlot) -> Option<String> {
        match slot.changed_event() {
            SessionEvent::DraftChanged { id, .. } => id,
            other => panic!("expected a DraftChanged, got {other:?}"),
        }
    }

    /// The cue switch is worker state, not draft state: it is
    /// applied by `drain_commands` itself, changes nothing on disk, and reports
    /// nothing to the frontend — the checkbox is already showing the answer.
    #[test]
    fn the_cue_switch_is_applied_by_the_drain_and_touches_no_draft() {
        let (_tmp, mut slot) = slot();
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (events_tx, events_rx) = mpsc::channel::<SessionEvent>();
        let mut held = VecDeque::new();
        let mut retranscribes = VecDeque::new();
        let mut model_changes = VecDeque::new();
        let mut cues_on = true;

        tx.send(SessionCommand::SetCues(false)).expect("send");
        assert!(drain_commands(
            &rx,
            &mut slot,
            &mut held,
            &mut retranscribes,
            &mut cues_on,
            &mut model_changes,
            &events_tx
        ));
        assert!(!cues_on);
        assert!(slot.draft.is_none(), "no draft was created or opened");
        assert!(events_rx.try_recv().is_err(), "a cue toggle says nothing");

        // And back on, live, without anything being rebuilt.
        tx.send(SessionCommand::SetCues(true)).expect("send");
        assert!(drain_commands(
            &rx,
            &mut slot,
            &mut held,
            &mut retranscribes,
            &mut cues_on,
            &mut model_changes,
            &events_tx
        ));
        assert!(cues_on);
    }

    /// A language change is worker state too, and it must reach the
    /// transcriber rather than the draft: it is queued by the drain and applied
    /// by the loop, in order with any model swap, before the next utterance.
    ///
    /// The swap itself cannot be unit-tested — building a `Transcriber` needs a
    /// model file, and no weights exist in this repo — so what is pinned here is
    /// the routing, which is where a dropped command would actually hide.
    #[test]
    fn a_language_change_is_queued_for_the_transcriber_and_touches_no_draft() {
        let (_tmp, mut slot) = slot();
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (events_tx, events_rx) = mpsc::channel::<SessionEvent>();
        let mut held = VecDeque::new();
        let mut retranscribes = VecDeque::new();
        let mut model_changes = VecDeque::new();
        let mut cues_on = true;

        tx.send(SessionCommand::SetLanguage(Language::new("tr")))
            .expect("send");
        assert!(drain_commands(
            &rx,
            &mut slot,
            &mut held,
            &mut retranscribes,
            &mut cues_on,
            &mut model_changes,
            &events_tx
        ));

        assert_eq!(model_changes.len(), 1, "the loop applies it, not the drain");
        match model_changes.pop_front() {
            Some(ModelChange::Language(language)) => assert_eq!(language.as_str(), "tr"),
            other => panic!("expected a queued language change, got {other:?}"),
        }
        assert!(slot.draft.is_none(), "no draft was created or opened");
        assert!(
            events_rx.try_recv().is_err(),
            "a language change says nothing until it fails"
        );
    }

    /// The routing rule, from the other side: if one of the transcriber's
    /// commands ever reaches the draft slot it is reported and ignored, never
    /// applied to a draft and never a panic.
    #[test]
    fn the_draft_slot_refuses_the_transcriber_commands() {
        let (_tmp, mut slot) = slot();
        let mut out = Vec::new();
        let change = slot.apply(SessionCommand::SetLanguage(Language::new("de")), &mut out);
        assert_eq!(change, Change::NOTHING);
        assert!(out.is_empty());
        assert!(slot.draft.is_none());
    }

    #[test]
    fn the_first_line_creates_the_draft_and_announces_it_first() {
        // Startup creates nothing: an empty draft per launch would fill the
        // outstanding list with notes nobody spoke.
        let (_tmp, mut slot) = slot();
        assert_eq!(active_id(&slot), None);
        assert!(slot.store.list_drafts().expect("list").drafts.is_empty());

        let (record, created) = slot
            .append(spoken("the menu button does nothing"))
            .expect("append");

        assert_eq!(record.text, "the menu button does nothing");
        // The announcement carries the count as it was *before* this line, so
        // the frontend numbers it 1 and not 2.
        match created {
            Some(SessionEvent::DraftChanged {
                id,
                dir,
                line_count,
            }) => {
                assert_eq!(id, active_id(&slot));
                assert!(id.is_some());
                assert!(dir.is_some_and(|dir| dir.is_dir()));
                assert_eq!(line_count, 0);
            }
            other => panic!("the lazily created draft was not announced: {other:?}"),
        }

        // Tagged with the active project, and only one draft exists.
        let scan = slot.store.list_drafts().expect("list");
        assert_eq!(scan.drafts.len(), 1);
        assert_eq!(scan.drafts[0].project.as_deref(), Some("playtest"));
        assert_eq!(scan.drafts[0].line_count, 1);

        // The second line goes to the same draft and announces nothing.
        let (_, created) = slot.append(spoken("and the second")).expect("append");
        assert!(created.is_none());
        assert_eq!(slot.store.list_drafts().expect("list").drafts.len(), 1);
    }

    #[test]
    fn set_draft_routes_the_next_line_to_the_new_draft() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("first draft, first line"))
            .expect("append");
        let first = active_id(&slot).expect("a draft was created");

        // A resumed draft: opened by the shell, handed over as a command.
        let second = slot.store.create_draft(None).expect("create");
        let second_id = second.id().to_owned();
        let mut out = Vec::new();
        assert!(
            slot.apply(SessionCommand::SetDraft(Box::new(second)), &mut out)
                .activation
        );
        assert!(out.is_empty(), "a switch is not a discard: {out:?}");
        assert_eq!(active_id(&slot), Some(second_id.clone()));

        slot.append(spoken("second draft, first line"))
            .expect("append");

        let scan = slot.store.list_drafts().expect("list");
        let counts: Vec<(String, usize)> = scan
            .drafts
            .iter()
            .map(|d| (d.id.clone(), d.line_count))
            .collect();
        assert!(counts.contains(&(first, 1)), "{counts:?}");
        assert!(counts.contains(&(second_id, 1)), "{counts:?}");
    }

    #[test]
    fn a_resumed_draft_reports_the_count_it_already_had() {
        let (_tmp, mut slot) = slot();
        for _ in 0..7 {
            slot.append(spoken("a finding")).expect("append");
        }
        let dir = match slot.changed_event() {
            SessionEvent::DraftChanged { dir, .. } => dir.expect("a draft is active"),
            other => panic!("{other:?}"),
        };

        // Close it, then resume it exactly as the shell would.
        let mut out = Vec::new();
        assert!(slot.apply(SessionCommand::CloseDraft, &mut out).activation);
        assert_eq!(active_id(&slot), None);
        // Closing leaves the draft outstanding — it is not deleted.
        assert!(dir.is_dir());

        let resumed = Draft::open(&dir).expect("open").draft;
        slot.apply(SessionCommand::SetDraft(Box::new(resumed)), &mut out);
        assert_eq!(
            slot.changed_event(),
            SessionEvent::DraftChanged {
                id: Some(
                    dir.file_name()
                        .and_then(|name| name.to_str())
                        .expect("ulid dir name")
                        .to_owned()
                ),
                dir: Some(dir),
                line_count: 7,
            }
        );
    }

    #[test]
    fn discarding_the_active_draft_trashes_it_and_the_next_line_starts_a_fresh_one() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("a finding to throw away"))
            .expect("append");
        let discarded = active_id(&slot).expect("a draft is active");

        let mut out = Vec::new();
        assert!(
            slot.apply(SessionCommand::DiscardActive, &mut out)
                .activation
        );
        assert_eq!(
            out,
            vec![SessionEvent::DraftDiscarded {
                id: discarded.clone(),
                ok: true,
                message: None,
            }]
        );
        assert_eq!(active_id(&slot), None);

        // Moved, not destroyed, and out of the list.
        let trashed = slot.store.trash_dir().join(&discarded);
        assert!(trashed.is_dir());
        assert_eq!(Draft::open(&trashed).expect("open").lines.len(), 1);
        assert!(slot.store.list_drafts().expect("list").drafts.is_empty());

        // And the session carries on: the next line lazily creates a new draft.
        let (_, created) = slot.append(spoken("a keeper")).expect("append");
        assert!(created.is_some());
        let fresh = active_id(&slot).expect("a fresh draft");
        assert_ne!(fresh, discarded);
        let scan = slot.store.list_drafts().expect("list");
        assert_eq!(scan.drafts.len(), 1);
        assert_eq!(scan.drafts[0].id, fresh);
    }

    #[test]
    fn a_failed_discard_still_drops_the_handle_and_says_so() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("a finding")).expect("append");
        let id = active_id(&slot).expect("a draft is active");

        // Something already occupies the trash slot: the draft must stay
        // exactly where it is (invariant 4), and the user must be told.
        std::fs::create_dir_all(slot.store.trash_dir().join(&id)).expect("occupy the trash");

        let mut out = Vec::new();
        assert!(
            slot.apply(SessionCommand::DiscardActive, &mut out)
                .activation
        );
        assert_eq!(active_id(&slot), None);
        match out.as_slice() {
            [SessionEvent::DraftDiscarded {
                id: reported,
                ok: false,
                message: Some(message),
            }] => {
                assert_eq!(reported, &id);
                assert!(!message.is_empty());
            }
            other => panic!("expected a failed discard report, got {other:?}"),
        }
        assert!(slot.store.root().join(&id).is_dir(), "the draft was lost");
    }

    #[test]
    fn discarding_with_nothing_active_changes_nothing() {
        let (_tmp, mut slot) = slot();
        let mut out = Vec::new();
        assert_eq!(
            slot.apply(SessionCommand::DiscardActive, &mut out),
            Change::NOTHING
        );
        assert_eq!(
            slot.apply(SessionCommand::CloseDraft, &mut out),
            Change::NOTHING
        );
        assert!(out.is_empty());
        assert_eq!(active_id(&slot), None);
    }

    // -----------------------------------------------------------------------
    // Held lines. The store's own append is what fails here, so
    // these run against a real temp directory and no model: `store_line` and
    // `flush_held` are the two halves of "nothing is discarded because the
    // disk refused", and both are reachable without whisper.
    // -----------------------------------------------------------------------

    /// Make the next append fail, portably: the audio directory becomes a
    /// *file*, so `create_dir_all` — and therefore the wav write, and therefore
    /// the whole append — cannot succeed. Undone by `let_the_disk_back`.
    fn break_the_disk(slot: &DraftSlot) {
        let dir = slot.draft.as_ref().expect("a draft").audio_dir();
        std::fs::remove_dir_all(&dir).expect("clear audio dir");
        std::fs::write(&dir, b"not a directory").expect("block the audio dir");
    }

    fn let_the_disk_back(slot: &DraftSlot) {
        let dir = slot.draft.as_ref().expect("a draft").audio_dir();
        std::fs::remove_file(&dir).expect("unblock");
        std::fs::create_dir_all(&dir).expect("restore audio dir");
    }

    fn at(secs: u64) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(&format!("2026-08-12T10:00:{secs:02}+02:00"))
            .expect("test timestamp")
    }

    fn line_for(token: &str, text: &str, secs: u64) -> StoredLine<'static> {
        StoredLine {
            token: token.to_owned(),
            text: text.to_owned(),
            spoken_at: at(secs),
            samples: &[],
            transcribe_ms: None,
            failed: false,
        }
    }

    fn texts(slot: &DraftSlot) -> Vec<String> {
        transcript(slot).into_iter().map(|line| line.text).collect()
    }

    #[test]
    fn a_line_the_disk_refuses_is_held_with_its_audio_rather_than_lost() {
        let (_tmp, mut slot) = slot();
        let mut held = VecDeque::new();
        let mut out = Vec::new();
        // A first line, so there is a draft to break.
        slot.append(spoken("the first one landed")).expect("append");
        break_the_disk(&slot);

        let change = store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("TOKEN-A", "the submit button stays disabled", 14),
        );

        assert_eq!(change, Change::NOTHING, "nothing reached the transcript");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].text, "the submit button stays disabled");
        assert!(held[0].draft_id.is_some(), "held lines are pinned");
        // The user's surface is the row and the count; the reason is log-only.
        assert!(
            out.iter().any(
                |event| matches!(event, SessionEvent::Held { token, .. } if token == "TOKEN-A")
            ),
            "{out:?}"
        );
        assert!(
            out.iter()
                .any(|event| matches!(event, SessionEvent::Debug { .. })),
            "the reason belongs in the debug log: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|event| matches!(event, SessionEvent::Error { .. })),
            "a held line is not an error the footer shouts about: {out:?}"
        );
    }

    #[test]
    fn a_new_line_goes_in_behind_the_held_ones_so_spoken_order_never_inverts() {
        let (_tmp, mut slot) = slot();
        let mut held = VecDeque::new();
        let mut out = Vec::new();
        slot.append(spoken("first")).expect("append");

        break_the_disk(&slot);
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T1", "second", 10),
        );
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T2", "third", 20),
        );
        assert_eq!(held.len(), 2);

        // The disk comes back, and the next spoken line is the one that
        // triggers the flush. It must land *after* both held lines.
        let_the_disk_back(&slot);
        out.clear();
        let change = store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T3", "fourth", 30),
        );

        assert_eq!(change, Change::TRANSCRIPT);
        assert!(held.is_empty());
        assert_eq!(texts(&slot), ["first", "second", "third", "fourth"]);
        // Each flushed line is announced with its token, marked as a flush so
        // nothing reveals stale words as if they had just been said.
        let flushed: Vec<_> = out
            .iter()
            .filter_map(|event| match event {
                SessionEvent::LineAdded {
                    token, held_flush, ..
                } => Some((token.clone().unwrap_or_default(), *held_flush)),
                _ => None,
            })
            .collect();
        assert_eq!(
            flushed,
            [
                ("T1".to_owned(), true),
                ("T2".to_owned(), true),
                ("T3".to_owned(), false)
            ]
        );
    }

    #[test]
    fn a_flush_that_fails_again_keeps_the_queue_intact_and_in_order() {
        let (_tmp, mut slot) = slot();
        let mut held = VecDeque::new();
        let mut out = Vec::new();
        slot.append(spoken("first")).expect("append");

        break_the_disk(&slot);
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T1", "second", 10),
        );
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T2", "third", 20),
        );

        out.clear();
        let change = flush_held(&mut slot, &mut held, &mut out);

        assert_eq!(change, Change::NOTHING);
        assert_eq!(held.len(), 2, "nothing was dropped by a failed retry");
        assert_eq!(held[0].token, "T1");
        assert_eq!(held[1].token, "T2");
        // Quiet: one debug entry for the attempt, and no footer traffic per
        // retry.
        assert!(
            out.iter()
                .all(|event| matches!(event, SessionEvent::Debug { .. })),
            "{out:?}"
        );
    }

    #[test]
    fn a_failed_line_that_cannot_be_written_is_held_as_a_failed_line() {
        // Both failures at once: the model refused *and* the disk refused. The
        // audio and the state both survive, so the flush writes the same failed
        // record the decode meant to write.
        let (_tmp, mut slot) = slot();
        let mut held = VecDeque::new();
        let mut out = Vec::new();
        slot.append(spoken("first")).expect("append");
        break_the_disk(&slot);

        let mut line = line_for("T1", "", 10);
        line.failed = true;
        store_line(&mut slot, &mut held, &mut out, None, line);
        assert!(held[0].failed);

        let_the_disk_back(&slot);
        out.clear();
        flush_held(&mut slot, &mut held, &mut out);

        let lines = transcript(&slot);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].failed, "the failure state survived the hold");
    }

    #[test]
    fn a_held_line_waits_for_its_own_draft_and_is_written_when_it_is_opened() {
        let (_tmp, mut slot) = slot();
        let mut held = VecDeque::new();
        let mut out = Vec::new();
        slot.append(spoken("first")).expect("append");
        let owner = slot.draft.as_ref().expect("draft").dir().to_path_buf();

        break_the_disk(&slot);
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T1", "held for A", 10),
        );
        let_the_disk_back(&slot);

        // Somewhere else entirely. The held line must not follow.
        let other = slot.store.create_draft(None).expect("create");
        slot.draft = Some(other);
        let mut out = Vec::new();
        assert_eq!(flush_held(&mut slot, &mut held, &mut out), Change::NOTHING);
        assert_eq!(held.len(), 1, "a held line is pinned to its own note");
        assert!(texts(&slot).is_empty(), "it landed in the wrong note");

        // Back to the draft it belongs to: now it lands.
        let back = Draft::open(&owner).expect("reopen").draft;
        slot.draft = Some(back);
        let mut out = Vec::new();
        assert_eq!(
            flush_held(&mut slot, &mut held, &mut out),
            Change::TRANSCRIPT
        );
        assert!(held.is_empty());
        assert_eq!(texts(&slot), ["first", "held for A"]);
    }

    #[test]
    fn discarding_a_draft_drops_the_lines_that_were_held_for_it() {
        // The note went to `.trash`; writing its held lines afterwards would
        // resurrect it.
        let (_tmp, mut slot) = slot();
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (events_tx, _events_rx) = mpsc::channel::<SessionEvent>();
        let mut held = VecDeque::new();
        let mut retranscribes = VecDeque::new();
        let mut model_changes = VecDeque::new();
        let mut cues_on = true;
        let mut out = Vec::new();

        slot.append(spoken("first")).expect("append");
        break_the_disk(&slot);
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T1", "held", 10),
        );
        assert_eq!(held.len(), 1);
        let_the_disk_back(&slot);

        tx.send(SessionCommand::DiscardActive).expect("send");
        assert!(drain_commands(
            &rx,
            &mut slot,
            &mut held,
            &mut retranscribes,
            &mut cues_on,
            &mut model_changes,
            &events_tx
        ));

        assert!(
            held.is_empty(),
            "held lines went to the trash with the note"
        );
    }

    #[test]
    fn a_save_flushes_what_it_can_before_it_renders() {
        // A save with lines still held proceeds — the file is honestly behind
        // — but anything that *can* be written goes in first, or it would have
        // needed a second save to reach the file.
        let (tmp, mut slot) = slot();
        let (tx, rx) = mpsc::channel::<SessionCommand>();
        let (events_tx, events_rx) = mpsc::channel::<SessionEvent>();
        let mut held = VecDeque::new();
        let mut retranscribes = VecDeque::new();
        let mut model_changes = VecDeque::new();
        let mut cues_on = true;
        let mut out = Vec::new();

        slot.append(spoken("first")).expect("append");
        break_the_disk(&slot);
        store_line(
            &mut slot,
            &mut held,
            &mut out,
            None,
            line_for("T1", "held then written", 10),
        );
        let_the_disk_back(&slot);

        let path = tmp.path().join("note.md");
        tx.send(SessionCommand::Save {
            fallback_path: path.clone(),
            notes_root: None,
            adopt_project: None,
            header: None,
            dividers: false,
            overwrite: false,
        })
        .expect("send");
        assert!(drain_commands(
            &rx,
            &mut slot,
            &mut held,
            &mut retranscribes,
            &mut cues_on,
            &mut model_changes,
            &events_tx
        ));

        assert!(held.is_empty());
        let written = std::fs::read_to_string(&path).expect("read the note");
        assert!(
            written.contains("held then written"),
            "the flush did not reach the render: {written}"
        );
        drop(events_rx);
    }

    // -----------------------------------------------------------------------
    // Line editing. Filesystem and pure logic only: the `Retranscribe` command
    // is split so that everything except the whisper call itself — the wav
    // read, the empty-result rule, the edit append — is covered here without a
    // model.
    // -----------------------------------------------------------------------

    /// The active draft's transcript, as the frontend would receive it.
    fn transcript(slot: &DraftSlot) -> Vec<LineRecord> {
        match slot.transcript_event() {
            Some(SessionEvent::Transcript { lines, .. }) => lines,
            other => panic!("expected a transcript, got {other:?}"),
        }
    }

    #[test]
    fn editing_a_line_reports_the_whole_folded_transcript() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("the mennu does nothing"))
            .expect("first");
        slot.append(spoken("and the fence clips")).expect("second");
        let id = transcript(&slot)[0].id.clone();

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::EditLine {
                id: id.clone(),
                text: "the menu does nothing".to_owned(),
            },
            &mut out,
        );

        assert_eq!(change, Change::TRANSCRIPT);
        assert!(out.is_empty(), "an edit that worked said nothing: {out:?}");

        let lines = transcript(&slot);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "the menu does nothing");
        assert_eq!(lines[0].original.as_deref(), Some("the mennu does nothing"));
        assert_eq!(lines[1].text, "and the fence clips");
        // A switch away and back sees the same thing: the edit is on disk.
        let dir = match slot.changed_event() {
            SessionEvent::DraftChanged { dir, .. } => dir.expect("active"),
            other => panic!("{other:?}"),
        };
        assert_eq!(Draft::open(&dir).expect("reopen").lines, lines);
    }

    #[test]
    fn soft_deleting_a_line_keeps_it_in_the_transcript_and_drops_the_count() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("keep me")).expect("first");
        slot.append(spoken("drop me")).expect("second");
        let id = transcript(&slot)[1].id.clone();

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::SetDeleted {
                id: id.clone(),
                deleted: true,
            },
            &mut out,
        );

        assert_eq!(change, Change::TRANSCRIPT);
        let lines = transcript(&slot);
        // Still there — deletion is a state, not a removal (invariant 4).
        assert_eq!(lines.len(), 2);
        assert!(lines[1].deleted);
        assert_eq!(
            slot.store.list_drafts().expect("list").drafts[0].line_count,
            1
        );

        // And it comes back.
        slot.apply(SessionCommand::SetDeleted { id, deleted: false }, &mut out);
        assert!(!transcript(&slot)[1].deleted);
    }

    #[test]
    fn undoing_an_edit_is_an_edit_back_to_the_spoken_text() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("first as spoken")).expect("first");
        let id = transcript(&slot)[0].id.clone();

        let mut out = Vec::new();
        slot.apply(
            SessionCommand::EditLine {
                id: id.clone(),
                text: "first, tidied".to_owned(),
            },
            &mut out,
        );

        // What the frontend's undo stack sends: the same `EditLine` command,
        // carrying the text the line had before (the stack is in the
        // panel, the backend has no revert of its own).
        let change = slot.apply(
            SessionCommand::EditLine {
                id,
                text: "first as spoken".to_owned(),
            },
            &mut out,
        );

        assert_eq!(change, Change::TRANSCRIPT);
        let after = transcript(&slot);
        assert_eq!(after[0].text, "first as spoken");
        // Undone lines read as untouched, even though three records are on disk
        // for one line.
        assert!(after[0].original.is_none());
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn moving_a_line_reorders_the_transcript_and_keeps_every_line() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("first")).expect("first");
        slot.append(spoken("second")).expect("second");
        slot.append(spoken("third")).expect("third");
        let before = transcript(&slot);
        let (a, b, c) = (
            before[0].id.clone(),
            before[1].id.clone(),
            before[2].id.clone(),
        );

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLine {
                id: c.clone(),
                after: None,
            },
            &mut out,
        );

        assert_eq!(change, Change::TRANSCRIPT);
        assert!(out.is_empty(), "a move that worked said nothing: {out:?}");
        let ids: Vec<String> = transcript(&slot).iter().map(|l| l.id.clone()).collect();
        assert_eq!(ids, vec![c.clone(), a.clone(), b.clone()]);
        // Reordering is not deletion: the count and the texts are untouched.
        assert_eq!(
            slot.store.list_drafts().expect("list").drafts[0].line_count,
            3
        );

        // Anchored to a line, and the order survives a close and a resume.
        slot.apply(
            SessionCommand::MoveLine {
                id: a.clone(),
                after: Some(b.clone()),
            },
            &mut out,
        );
        let dir = match slot.changed_event() {
            SessionEvent::DraftChanged { dir, .. } => dir.expect("active"),
            other => panic!("{other:?}"),
        };
        let reopened: Vec<String> = Draft::open(&dir)
            .expect("reopen")
            .lines
            .iter()
            .map(|l| l.id.clone())
            .collect();
        assert_eq!(reopened, vec![c, b, a]);

        // A line that is not there is an error naming it, not a silent drop.
        let change = slot.apply(
            SessionCommand::MoveLine {
                id: "01JNOSUCHLINE".to_owned(),
                after: None,
            },
            &mut out,
        );
        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            [SessionEvent::Error { message }] => {
                assert!(message.contains("01JNOSUCHLINE"), "{message}");
            }
            other => panic!("expected one error, got {other:?}"),
        }
    }

    #[test]
    fn an_edit_to_a_line_that_is_not_there_is_an_error_naming_it() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("the only line")).expect("append");

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::EditLine {
                id: "01JNOSUCHLINE".to_owned(),
                text: "nope".to_owned(),
            },
            &mut out,
        );

        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            [SessionEvent::Error { message }] => {
                assert!(message.contains("01JNOSUCHLINE"), "{message}");
            }
            other => panic!("expected one error, got {other:?}"),
        }
        assert_eq!(transcript(&slot).len(), 1);
    }

    #[test]
    fn editing_with_no_active_draft_says_so_rather_than_creating_one() {
        // Lazy creation is for *spoken* lines only: an edit with nothing active
        // has nothing to be an edit of, and inventing a draft here would put an
        // empty note in the outstanding list.
        let (_tmp, mut slot) = slot();
        let mut out = Vec::new();

        let change = slot.apply(
            SessionCommand::EditLine {
                id: "01JLINE".to_owned(),
                text: "nope".to_owned(),
            },
            &mut out,
        );

        assert_eq!(change, Change::NOTHING);
        assert!(matches!(out.as_slice(), [SessionEvent::Error { .. }]));
        assert!(slot.store.list_drafts().expect("list").drafts.is_empty());
    }

    #[test]
    fn a_draft_switch_carries_the_new_drafts_transcript() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("in the first draft")).expect("append");

        let second = slot.store.create_draft(None).expect("create");
        let mut out = Vec::new();
        let change = slot.apply(SessionCommand::SetDraft(Box::new(second)), &mut out);

        // A switch replaces the whole list, so it is both kinds of change.
        assert_eq!(change, Change::ACTIVATION);
        assert!(transcript(&slot).is_empty());

        // And with nothing active there is no transcript to report at all: the
        // `DraftChanged { id: None }` has already said to show nothing.
        slot.apply(SessionCommand::CloseDraft, &mut out);
        assert!(slot.transcript_event().is_none());
    }

    #[test]
    fn a_stored_line_reads_back_as_the_samples_it_was_written_from() {
        // The half of `Retranscribe` that does not need a model: the wav this
        // crate wrote, decoded back to the f32 mono whisper takes.
        let tmp = tempfile::tempdir().expect("temp dir");
        let store = crate::draft::DraftStore::new(tmp.path().join("drafts"));
        let mut draft = store.create_draft(None).expect("create");
        let samples: Vec<f32> = (0..800).map(|i| ((i as f32) / 400.0) - 1.0).collect();
        let record = draft
            .append_line(NewLine {
                text: "spoken".to_owned(),
                spoken_at: DateTime::parse_from_rfc3339("2026-08-05T10:00:00+02:00")
                    .expect("timestamp"),
                samples: &samples,
                transcribe_ms: None,
                failed: false,
            })
            .expect("append")
            .clone();

        let path = draft.line_audio_path(&record.id).expect("path");
        let read = read_line_audio(&path).expect("read");

        assert_eq!(read.len(), samples.len());
        for (original, back) in samples.iter().zip(&read) {
            assert!(
                (original - back).abs() <= 2.0 / f32::from(i16::MAX),
                "expected {original}, read back {back}"
            );
        }

        // A file that is not there is an error naming it, never a panic.
        let missing = draft.audio_dir().join("01JNOSUCHLINE.wav");
        let err = read_line_audio(&missing).expect_err("missing wav");
        assert!(err.to_string().contains("01JNOSUCHLINE"), "{err}");
    }

    #[test]
    fn a_re_transcription_that_produced_nothing_leaves_the_line_alone() {
        // The empty-result rule, exercised through the same append path the
        // real command uses. Overwriting a finding with "" would be the app
        // eating the user's words.
        assert_eq!(retranscribed_text(""), None);
        assert_eq!(retranscribed_text("   \n\t"), None);
        assert_eq!(
            retranscribed_text("  a clear finding  "),
            Some("a clear finding".to_owned())
        );

        // And a kept result goes in as an ordinary edit, with its duration —
        // the rest of the command, minus the model call.
        let (_tmp, mut slot) = slot();
        slot.append(spoken("a mumbled finding")).expect("append");
        let id = transcript(&slot)[0].id.clone();
        let mut out = Vec::new();
        let change = slot.correct(&id, &mut out, |draft, id| {
            draft.retranscribed_line(id, "a clear finding", Some(512))
        });
        assert_eq!(change, Change::TRANSCRIPT);
        let lines = transcript(&slot);
        assert_eq!(lines[0].text, "a clear finding");
        assert_eq!(lines[0].transcribe_ms, Some(512));
        assert_eq!(lines[0].original.as_deref(), Some("a mumbled finding"));
    }

    // -----------------------------------------------------------------------
    // Saving. Filesystem only, and there is no cue on
    // any save outcome — cues belong to the recording loop — so the whole of
    // this path is reachable without an output device.
    // -----------------------------------------------------------------------

    /// Run one save through the worker's own path and hand back the outcome.
    fn save(slot: &mut DraftSlot, fallback: &Path, overwrite: bool) -> SaveOutcome {
        save_under(slot, fallback, None, None, overwrite)
    }

    /// [`save`] with a project: a notes folder to bind against, and a name to
    /// adopt a projectless draft into.
    fn save_under(
        slot: &mut DraftSlot,
        fallback: &Path,
        notes_root: Option<&Path>,
        adopt_project: Option<&str>,
        overwrite: bool,
    ) -> SaveOutcome {
        let mut out = Vec::new();
        let request = SaveRequest {
            fallback_path: fallback,
            notes_root,
            adopt_project,
            header: None,
            dividers: SessionDividers::Shown,
            mode: if overwrite {
                SaveMode::Overwrite
            } else {
                SaveMode::Guarded
            },
        };
        let change = slot.save(&request, &mut out);
        // A save changes no line and no activation; the dirty flag reaches the
        // frontend through the outcome event.
        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            [SessionEvent::Saved { outcome }] => (**outcome).clone(),
            other => panic!("expected exactly one save outcome, got {other:?}"),
        }
    }

    #[test]
    fn the_first_save_writes_the_fallback_and_every_later_one_rewrites_it() {
        let (tmp, mut slot) = slot();
        slot.append(spoken("the menu button does nothing"))
            .expect("append");
        // Nested on purpose: the parents have to be created for a first save.
        let first = tmp.path().join("notes").join("sotone 2026-08-05.md");

        match save(&mut slot, &first, false) {
            SaveOutcome::Saved { path, lines, bytes } => {
                assert_eq!(path, first);
                assert_eq!(lines, 1);
                assert!(bytes > 0);
            }
            other => panic!("expected a save, got {other:?}"),
        }
        let written = std::fs::read_to_string(&first).expect("read back");
        assert!(
            written.contains("the menu button does nothing"),
            "{written}"
        );

        // Bound forever: a later save ignores whatever fallback it is handed.
        slot.append(spoken("and the fence clips")).expect("append");
        let elsewhere = tmp.path().join("somewhere else.md");
        match save(&mut slot, &elsewhere, false) {
            SaveOutcome::Saved { path, lines, .. } => {
                assert_eq!(path, first, "a bound draft moved");
                assert_eq!(lines, 2);
            }
            other => panic!("expected a save, got {other:?}"),
        }
        assert!(!elsewhere.exists(), "the fallback was written to anyway");
    }

    #[test]
    fn a_save_under_a_project_binds_relative_adopts_and_survives_a_folder_move() {
        let (tmp, mut slot) = slot();
        // Dictated before any project existed — the case adoption is for.
        slot.project = None;
        slot.append(spoken("the fence clips")).expect("append");

        let root_a = tmp.path().join("A");
        let first = root_a.join("Ludo.md");
        match save_under(&mut slot, &first, Some(&root_a), Some("Ludo"), false) {
            SaveOutcome::Saved { path, .. } => assert_eq!(path, first),
            other => panic!("expected a save, got {other:?}"),
        }

        {
            let draft = slot.draft.as_ref().expect("a draft");
            assert_eq!(
                draft.saved_path(),
                Some(Path::new("Ludo.md")),
                "the binding is relative to the notes folder"
            );
            assert_eq!(draft.meta().project.as_deref(), Some("Ludo"));
        }

        // The user moves the whole notes folder and re-points the project.
        let root_b = tmp.path().join("B");
        std::fs::rename(&root_a, &root_b).expect("move the notes folder");
        slot.append(spoken("and the menu button")).expect("append");

        match save_under(
            &mut slot,
            Path::new("never used.md"),
            Some(&root_b),
            Some("Ludo"),
            false,
        ) {
            SaveOutcome::Saved { path, lines, .. } => {
                assert_eq!(path, root_b.join("Ludo.md"));
                assert_eq!(lines, 2);
            }
            other => panic!("expected a save, got {other:?}"),
        }
        assert!(
            !root_a.exists(),
            "the save recreated the folder the user moved away from"
        );
    }

    #[test]
    fn a_binding_whose_project_is_gone_is_an_error_not_a_panic() {
        let (tmp, mut slot) = slot();
        slot.append(spoken("a finding")).expect("append");
        let root = tmp.path().join("notes");
        save_under(&mut slot, &root.join("note.md"), Some(&root), None, false);

        // The project has been removed from the config, so there is no folder to
        // resolve "note.md" against.
        match save_under(&mut slot, Path::new("first.md"), None, None, false) {
            SaveOutcome::Error { message } => {
                assert!(message.contains("note.md"), "{message}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn setting_the_active_project_only_governs_the_next_lazily_created_draft() {
        let (_tmp, mut slot) = slot();
        slot.append(spoken("first note")).expect("append");
        let first = slot.draft.as_ref().expect("a draft").id().to_owned();
        assert_eq!(
            slot.draft
                .as_ref()
                .expect("a draft")
                .meta()
                .project
                .as_deref(),
            Some("playtest")
        );

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::SetProject {
                name: Some("Ludo".to_owned()),
            },
            &mut out,
        );
        assert_eq!(change, Change::NOTHING);
        assert!(
            out.is_empty(),
            "changing the active project is not an event"
        );
        // The draft already open keeps the project it was created with: nothing
        // on disk is touched.
        assert_eq!(
            slot.draft
                .as_ref()
                .expect("a draft")
                .meta()
                .project
                .as_deref(),
            Some("playtest")
        );

        // The next draft the worker has to invent gets the new one.
        slot.apply(SessionCommand::CloseDraft, &mut out);
        slot.append(spoken("second note")).expect("append");
        let second = slot.draft.as_ref().expect("a draft");
        assert_ne!(second.id(), first);
        assert_eq!(second.meta().project.as_deref(), Some("Ludo"));
    }

    #[test]
    fn an_external_edit_stops_the_save_and_hands_back_both_texts() {
        let (tmp, mut slot) = slot();
        slot.append(spoken("a finding")).expect("append");
        let path = tmp.path().join("note.md");
        save(&mut slot, &path, false);

        // Somebody else edited the note.
        let theirs = "# their own heading\n\n- 09:00:00 — their line\n";
        std::fs::write(&path, theirs).expect("external edit");

        match save(&mut slot, &path, false) {
            SaveOutcome::Conflict {
                path: reported,
                disk_text,
                pending_markdown,
            } => {
                assert_eq!(reported, path);
                assert_eq!(disk_text, theirs);
                assert!(pending_markdown.contains("a finding"), "{pending_markdown}");
                assert_ne!(disk_text, pending_markdown);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Nothing was written: invariant 4, and the whole point of the guard.
        assert_eq!(std::fs::read_to_string(&path).expect("read"), theirs);

        // And Overwrite — the only thing that ever sends it is the dialog the
        // user was just shown — goes through.
        match save(&mut slot, &path, true) {
            SaveOutcome::Saved { path: written, .. } => assert_eq!(written, path),
            other => panic!("expected a save, got {other:?}"),
        }
        let now = std::fs::read_to_string(&path).expect("read");
        assert!(now.contains("a finding"), "{now}");
        assert!(!now.contains("their line"), "{now}");
    }

    #[test]
    fn saving_with_no_active_draft_says_so_rather_than_creating_one() {
        let (tmp, mut slot) = slot();
        let path = tmp.path().join("note.md");

        match save(&mut slot, &path, false) {
            SaveOutcome::Error { message } => assert!(!message.is_empty()),
            other => panic!("expected an error, got {other:?}"),
        }
        assert!(!path.exists());
        assert!(slot.store.list_drafts().expect("list").drafts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Save all, store-wide. Filesystem only, like every
    // other save test.
    // -----------------------------------------------------------------------

    /// One project's context, the way the shell builds it.
    fn context(project: &str, notes_root: &Path, filename_template: &str) -> ProjectSaveContext {
        ProjectSaveContext {
            project: project.to_owned(),
            notes_root: notes_root.to_owned(),
            filename_template: filename_template.to_owned(),
            header: None,
            dividers: true,
        }
    }

    /// Run one batch through the worker's own path and hand back the tally.
    fn save_all(
        slot: &mut DraftSlot,
        projects: &[ProjectSaveContext],
    ) -> (usize, usize, Vec<PathBuf>, Vec<String>) {
        let mut out = Vec::new();
        let change = slot.save_all(projects, &mut out);
        // A batch changes no line and no activation; the dirty flags reach the
        // frontend through the re-list the shell does on this event.
        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            // Exactly one event for the whole batch, never one per draft: the
            // window turns a save outcome into a dialog or a notice, and N of
            // those for one click is the thing this design refuses.
            [SessionEvent::SavedAll {
                saved,
                skipped,
                conflicts,
                errors,
            }] => (*saved, *skipped, conflicts.clone(), errors.clone()),
            other => panic!("expected exactly one batch outcome, got {other:?}"),
        }
    }

    /// The single-project batch the first tests here were written against.
    fn save_all_of(
        slot: &mut DraftSlot,
        project: &str,
        notes_root: &Path,
        filename_template: &str,
    ) -> (usize, usize, Vec<PathBuf>, Vec<String>) {
        save_all(slot, &[context(project, notes_root, filename_template)])
    }

    /// A draft of `project` with one line in it, closed again.
    fn dictated(slot: &DraftSlot, project: Option<&str>, text: &str) -> String {
        let mut draft = slot.store.create_draft(project).expect("create");
        draft.append_line(spoken(text)).expect("append");
        draft.id().to_owned()
    }

    /// What is in the notes folder, sorted.
    fn note_files(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .expect("read the notes folder")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_batch_saves_the_dirty_notes_it_has_a_context_for_and_touches_nothing_else() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");
        std::fs::create_dir_all(&root).expect("notes folder");
        // Bound and dictated into again: rewritten in place.
        let mut bound = slot.store.create_draft(Some("Ludo")).expect("create");
        bound
            .append_line(spoken("the fence clips"))
            .expect("append");
        bound
            .save_to(
                &root.join("bound.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        bound.append_line(spoken("and the menu")).expect("append");
        drop(bound);

        // Never saved: a first save under the project's template.
        dictated(&slot, Some("Ludo"), "a fresh finding");

        // Saved and untouched since: not dirty, so not in the batch.
        let mut clean = slot.store.create_draft(Some("Ludo")).expect("create");
        clean.append_line(spoken("already filed")).expect("append");
        clean
            .save_to(
                &root.join("clean.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("save");
        drop(clean);

        // Projectless, and one of a project this batch was given no context for
        // — the shape of a project that is no longer in the configuration.
        // Neither is adopted or written: filing notes en masse is how they end
        // up under the wrong thing.
        let orphan = dictated(&slot, None, "dictated before any project existed");
        let other = dictated(&slot, Some("Backlog"), "someone else's note");

        let (saved, skipped, conflicts, errors) =
            save_all_of(&mut slot, "Ludo", &root, "{project} note.md");
        assert_eq!(saved, 2, "conflicts {conflicts:?}, errors {errors:?}");
        assert_eq!(skipped, 2, "the two the batch has no home for");
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert!(errors.is_empty(), "{errors:?}");

        let rewritten = std::fs::read_to_string(root.join("bound.md")).expect("read");
        assert!(rewritten.contains("and the menu"), "{rewritten}");
        let first = std::fs::read_to_string(root.join("Ludo note.md")).expect("read");
        assert!(first.contains("a fresh finding"), "{first}");
        // Nothing else was created — in particular no file for the projectless
        // draft or the other project's.
        assert_eq!(
            note_files(&root),
            vec![
                "Ludo note.md".to_owned(),
                "bound.md".to_owned(),
                "clean.md".to_owned()
            ]
        );

        let scan = slot.store.list_drafts().expect("list");
        let still_dirty: Vec<&str> = scan
            .drafts
            .iter()
            .filter(|d| d.dirty)
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(still_dirty.len(), 2, "{still_dirty:?}");
        assert!(still_dirty.contains(&orphan.as_str()), "{still_dirty:?}");
        assert!(still_dirty.contains(&other.as_str()), "{still_dirty:?}");
    }

    #[test]
    fn the_active_draft_is_saved_through_the_live_handle_not_a_second_one() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");

        // A note from an earlier launch: resumed here, so it owes exactly one
        // session marker before its next line.
        let id = dictated(&slot, Some("Ludo"), "from yesterday");
        let path = slot.store.draft_path(&id).expect("path");
        let loaded = Draft::open(&path).expect("open");
        let mut out = Vec::new();
        slot.apply(SessionCommand::SetDraft(Box::new(loaded.draft)), &mut out);

        let (saved, skipped, conflicts, errors) =
            save_all_of(&mut slot, "Ludo", &root, "{project}.md");
        assert_eq!(
            (saved, skipped, conflicts.len(), errors.len()),
            (1, 0, 0, 0)
        );

        // The live handle is the one that saved, so it knows: with a second
        // handle, this one's in-memory meta would still say "never saved" and
        // the next append would write that back over the binding on disk.
        {
            let draft = slot.draft.as_ref().expect("a draft");
            assert_eq!(draft.saved_path(), Some(Path::new("Ludo.md")));
            assert!(!draft.dirty());
        }

        slot.append(spoken("and today")).expect("append");
        assert_eq!(
            sessions(&slot),
            vec![0, 1],
            "the batch disturbed the session-marker bookkeeping"
        );
        let draft = slot.draft.as_ref().expect("a draft");
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("Ludo.md")),
            "the binding did not survive the next append"
        );
        assert!(draft.dirty(), "a new line left the note clean");
    }

    #[test]
    fn a_conflict_in_a_batch_skips_that_note_and_the_rest_still_save() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");
        std::fs::create_dir_all(&root).expect("notes folder");

        let mut bound = slot.store.create_draft(Some("Ludo")).expect("create");
        bound.append_line(spoken("a finding")).expect("append");
        bound
            .save_to(
                &root.join("theirs.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        bound
            .append_line(spoken("a second finding"))
            .expect("append");
        drop(bound);

        // Somebody else edited that note between the two saves.
        let theirs = "# their own heading\n\n- 09:00:00 — their line\n";
        std::fs::write(root.join("theirs.md"), theirs).expect("external edit");

        dictated(&slot, Some("Ludo"), "an unrelated finding");

        let (saved, skipped, conflicts, errors) =
            save_all_of(&mut slot, "Ludo", &root, "{project}.md");
        assert_eq!(saved, 1, "one stopped note stopped the whole batch");
        assert_eq!(skipped, 0);
        assert_eq!(conflicts, vec![root.join("theirs.md")]);
        assert!(errors.is_empty(), "a conflict is a stop, not a failure");

        // Invariant 4: a batch has no overwrite, so their edit is still there
        // byte for byte.
        assert_eq!(
            std::fs::read_to_string(root.join("theirs.md")).expect("read"),
            theirs
        );
        assert!(root.join("Ludo.md").exists());
    }

    #[test]
    fn two_unbound_notes_that_expand_to_one_name_report_a_conflict_not_a_clobber() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");

        // Both never saved, both under a template with nothing to tell them
        // apart — the same thing two notes dictated inside one second do to a
        // `{time}` template.
        dictated(&slot, Some("Ludo"), "the first finding");
        dictated(&slot, Some("Ludo"), "the second finding");

        let (saved, skipped, conflicts, errors) =
            save_all_of(&mut slot, "Ludo", &root, "{project}.md");
        assert_eq!((saved, skipped), (1, 0));
        assert_eq!(conflicts, vec![root.join("Ludo.md")]);
        assert!(errors.is_empty(), "{errors:?}");

        // The first note wrote the file and the second did not touch it: the
        // guard's unrelated-file case, doing exactly its job (invariant 4).
        let written = std::fs::read_to_string(root.join("Ludo.md")).expect("read");
        assert!(written.contains("the first finding"), "{written}");
        assert!(!written.contains("the second finding"), "{written}");
        assert_eq!(note_files(&root), vec!["Ludo.md".to_owned()]);
    }

    #[test]
    fn a_batch_with_nothing_to_save_reports_zero_rather_than_saying_nothing() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");
        // A clean note of the right project, and a dirty one of another: the
        // batch has nothing of its own to do.
        let mut clean = slot.store.create_draft(Some("Ludo")).expect("create");
        clean.append_line(spoken("already filed")).expect("append");
        std::fs::create_dir_all(&root).expect("notes folder");
        clean
            .save_to(
                &root.join("clean.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("save");
        drop(clean);
        dictated(&slot, Some("Backlog"), "someone else's note");

        // The button said it would do something, so silence is not an answer —
        // and the one draft the batch has no context for is *said*, not hidden.
        assert_eq!(
            save_all_of(&mut slot, "Ludo", &root, "{project}.md"),
            (0, 1, Vec::new(), Vec::new())
        );
        assert_eq!(note_files(&root), vec!["clean.md".to_owned()]);
    }

    // -- Store-wide ---------------------------------------------------------
    //
    // "Save all" means every dirty note in the store, whatever project it
    // belongs to. What these pin is that the
    // widening did not make a batch careless: each note still saves under its
    // *own* project's rules, and everything the old scope refused to touch —
    // an unrelated note, an external edit — it still refuses.

    #[test]
    fn a_batch_saves_two_projects_each_into_its_own_folder_under_its_own_template() {
        let (tmp, mut slot) = slot();
        let ludo = tmp.path().join("ludo notes");
        let backlog = tmp.path().join("backlog notes");

        // One never-saved note in each, and a bound one in the second that a
        // batch has to rewrite in place rather than re-file.
        dictated(&slot, Some("Ludo"), "the fence clips");
        dictated(&slot, Some("Backlog"), "the loader hangs");
        let mut bound = slot.store.create_draft(Some("Backlog")).expect("create");
        bound
            .append_line(spoken("an older finding"))
            .expect("append");
        std::fs::create_dir_all(&backlog).expect("notes folder");
        bound
            .save_to(
                &backlog.join("older note.md"),
                SaveOptions {
                    notes_root: Some(&backlog),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        bound.append_line(spoken("and another")).expect("append");
        drop(bound);

        let (saved, skipped, conflicts, errors) = save_all(
            &mut slot,
            &[
                context("Ludo", &ludo, "{project} log.md"),
                context("Backlog", &backlog, "{project}.md"),
            ],
        );
        assert_eq!(saved, 3, "conflicts {conflicts:?}, errors {errors:?}");
        assert_eq!((skipped, conflicts.len(), errors.len()), (0, 0, 0));

        // Each note under its own project's rules: its own folder, its own
        // filename template. Nothing crossed over.
        assert_eq!(note_files(&ludo), vec!["Ludo log.md".to_owned()]);
        assert_eq!(
            note_files(&backlog),
            vec!["Backlog.md".to_owned(), "older note.md".to_owned()]
        );
        let rewritten = std::fs::read_to_string(backlog.join("older note.md")).expect("read");
        assert!(rewritten.contains("and another"), "{rewritten}");

        // And the store agrees: a batch that says it saved three leaves none of
        // the three dirty.
        let scan = slot.store.list_drafts().expect("list");
        assert!(
            scan.drafts.iter().all(|draft| !draft.dirty),
            "{:?}",
            scan.drafts
        );
    }

    #[test]
    fn a_dirty_draft_with_no_project_is_skipped_by_a_batch_rather_than_filed_anywhere() {
        let (tmp, mut slot) = slot();
        let ludo = tmp.path().join("ludo notes");
        let backlog = tmp.path().join("backlog notes");

        let orphan = dictated(&slot, None, "dictated before any project existed");
        dictated(&slot, Some("Ludo"), "a finding with a home");

        let (saved, skipped, conflicts, errors) = save_all(
            &mut slot,
            &[
                context("Ludo", &ludo, "{project}.md"),
                context("Backlog", &backlog, "{project}.md"),
            ],
        );
        // Not a note (notes exist only within projects), so not an error
        // either — counted, and left alone.
        assert_eq!((saved, skipped), (1, 1));
        assert!(conflicts.is_empty() && errors.is_empty(), "{errors:?}");
        assert_eq!(note_files(&ludo), vec!["Ludo.md".to_owned()]);
        assert!(!backlog.exists(), "a project with no notes got a folder");

        let scan = slot.store.list_drafts().expect("list");
        let still_dirty: Vec<&str> = scan
            .drafts
            .iter()
            .filter(|d| d.dirty)
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(still_dirty, vec![orphan.as_str()]);
    }

    #[test]
    fn a_conflict_in_one_project_does_not_stop_another_projects_saves() {
        let (tmp, mut slot) = slot();
        let ludo = tmp.path().join("ludo notes");
        let backlog = tmp.path().join("backlog notes");
        std::fs::create_dir_all(&ludo).expect("notes folder");

        let mut bound = slot.store.create_draft(Some("Ludo")).expect("create");
        bound.append_line(spoken("a finding")).expect("append");
        bound
            .save_to(
                &ludo.join("theirs.md"),
                SaveOptions {
                    notes_root: Some(&ludo),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        bound
            .append_line(spoken("a second finding"))
            .expect("append");
        drop(bound);

        // Somebody else edited that note between the two saves.
        let theirs = "# their own heading\n\n- 09:00:00 — their line\n";
        std::fs::write(ludo.join("theirs.md"), theirs).expect("external edit");

        dictated(&slot, Some("Backlog"), "an unrelated project's finding");

        let (saved, skipped, conflicts, errors) = save_all(
            &mut slot,
            &[
                context("Ludo", &ludo, "{project}.md"),
                context("Backlog", &backlog, "{project}.md"),
            ],
        );
        // One project's stop is that note's stop and nobody else's.
        assert_eq!((saved, skipped), (1, 0));
        assert_eq!(conflicts, vec![ludo.join("theirs.md")]);
        assert!(errors.is_empty(), "a conflict is a stop, not a failure");

        // Invariant 4: a batch has no overwrite, so their edit is still there
        // byte for byte — and the other project's note was written anyway.
        assert_eq!(
            std::fs::read_to_string(ludo.join("theirs.md")).expect("read"),
            theirs
        );
        assert_eq!(note_files(&backlog), vec!["Backlog.md".to_owned()]);
    }

    #[test]
    fn an_unbound_note_first_saves_through_its_own_projects_context_not_the_first_one() {
        let (tmp, mut slot) = slot();
        // The first context stands in for the *active* project, which is what
        // the batch used to save everything under. A note of the second one has
        // never been saved, so this is the case where the wrong context would
        // actually put a file in the wrong folder.
        let active = tmp.path().join("active notes");
        let elsewhere = tmp.path().join("elsewhere");

        dictated(&slot, Some("Backlog"), "a finding from another project");

        let (saved, skipped, conflicts, errors) = save_all(
            &mut slot,
            &[
                context("Ludo", &active, "{project} active.md"),
                context("Backlog", &elsewhere, "{project} its own.md"),
            ],
        );
        assert_eq!(
            (saved, skipped, conflicts.len(), errors.len()),
            (1, 0, 0, 0)
        );
        assert_eq!(
            note_files(&elsewhere),
            vec!["Backlog its own.md".to_owned()]
        );
        assert!(
            !active.exists(),
            "the note landed in the active project's folder"
        );
    }

    // -----------------------------------------------------------------------
    // Renaming, on the worker
    //
    // Which handle does the work is the whole content of these: the active
    // draft through the live one, everything else open→mutate→drop, and a
    // refusal that writes nothing.
    // -----------------------------------------------------------------------

    /// One rename command through the slot, with its single event.
    fn rename(slot: &mut DraftSlot, command: SessionCommand) -> Vec<String> {
        let mut out = Vec::new();
        let change = slot.apply(command, &mut out);
        // A rename moves no line and activates no draft: the window catches up
        // through the re-list the shell does when this event lands.
        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            [SessionEvent::Renamed { errors }] => errors.clone(),
            other => panic!("expected exactly one rename outcome, got {other:?}"),
        }
    }

    #[test]
    fn renaming_the_active_note_goes_through_the_live_handle() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");
        std::fs::create_dir_all(&root).expect("notes folder");

        let mut draft = slot.store.create_draft(Some("Ludo")).expect("create");
        draft
            .append_line(spoken("the fence clips"))
            .expect("append");
        draft
            .save_to(
                &root.join("Ludo 2026-08-14.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        let id = draft.id().to_owned();
        let dir = draft.dir().to_path_buf();
        slot.apply(SessionCommand::SetDraft(Box::new(draft)), &mut Vec::new());

        let errors = rename(
            &mut slot,
            SessionCommand::RenameActive {
                name: "checkout rebuild".to_owned(),
                notes_root: Some(root.clone()),
            },
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(note_files(&root), vec!["checkout rebuild.md".to_owned()]);

        // The live handle is still the one that owns this draft, and the
        // binding it wrote is on disk — a second handle would have written
        // `meta.json` from a copy that never saw the rename.
        assert_eq!(active_id(&slot), Some(id));
        assert_eq!(
            slot.draft.as_ref().expect("active").saved_path(),
            Some(Path::new("checkout rebuild.md"))
        );
        let reloaded = Draft::open(&dir).expect("reopen");
        assert_eq!(
            reloaded.draft.saved_path(),
            Some(Path::new("checkout rebuild.md"))
        );

        // And the next line still lands in it, through the same handle.
        slot.append(spoken("and the menu")).expect("append");
    }

    #[test]
    fn a_refused_rename_is_reported_and_writes_nothing() {
        let (tmp, mut slot) = slot();
        let root = tmp.path().join("notes");
        std::fs::create_dir_all(&root).expect("notes folder");
        std::fs::write(root.join("taken.md"), b"somebody else's words").expect("write");

        let mut draft = slot.store.create_draft(Some("Ludo")).expect("create");
        draft
            .append_line(spoken("the fence clips"))
            .expect("append");
        draft
            .save_to(
                &root.join("mine.md"),
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        slot.apply(SessionCommand::SetDraft(Box::new(draft)), &mut Vec::new());

        let errors = rename(
            &mut slot,
            SessionCommand::RenameActive {
                name: "taken".to_owned(),
                notes_root: Some(root.clone()),
            },
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("taken.md"), "{errors:?}");
        assert_eq!(
            std::fs::read(root.join("taken.md")).expect("read"),
            b"somebody else's words"
        );
        assert_eq!(
            note_files(&root),
            vec!["mine.md".to_owned(), "taken.md".to_owned()]
        );

        // With no active draft at all it is an answer, not a panic.
        slot.apply(SessionCommand::CloseDraft, &mut Vec::new());
        let errors = rename(
            &mut slot,
            SessionCommand::RenameActive {
                name: "anything".to_owned(),
                notes_root: Some(root),
            },
        );
        assert_eq!(errors.len(), 1, "{errors:?}");
    }

    #[test]
    fn a_project_rename_carries_every_draft_that_names_it() {
        let (_tmp, mut slot) = slot();
        // The active draft, reached through the live handle.
        let mut active = slot.store.create_draft(Some("Ludo")).expect("create");
        active
            .append_line(spoken("the fence clips"))
            .expect("append");
        let active_dir = active.dir().to_path_buf();
        slot.apply(SessionCommand::SetDraft(Box::new(active)), &mut Vec::new());
        // Two more: one of the same project, one of another, one of none.
        let sibling = dictated(&slot, Some("Ludo"), "a second finding");
        let other = dictated(&slot, Some("Backlog"), "someone else's note");
        let orphan = dictated(&slot, None, "dictated before any project");

        slot.project = Some("Ludo".to_owned());
        let errors = rename(
            &mut slot,
            SessionCommand::RenameProject {
                from: "Ludo".to_owned(),
                to: "Checkout rebuild".to_owned(),
            },
        );
        assert!(errors.is_empty(), "{errors:?}");

        // The lazily-created draft's tag moved with it, or the next line
        // spoken would be filed under a project that no longer exists.
        assert_eq!(slot.project.as_deref(), Some("Checkout rebuild"));

        let project_of = |id: &str| {
            let dir = slot.store.draft_path(id).expect("path");
            Draft::open(&dir)
                .expect("open")
                .draft
                .project()
                .map(str::to_owned)
        };
        assert_eq!(
            slot.draft.as_ref().expect("active").project(),
            Some("Checkout rebuild")
        );
        assert_eq!(
            Draft::open(&active_dir).expect("reopen").draft.project(),
            Some("Checkout rebuild"),
            "the live handle's meta write did not reach disk"
        );
        assert_eq!(project_of(&sibling).as_deref(), Some("Checkout rebuild"));
        // Nothing else moved: a sweep is about one name.
        assert_eq!(project_of(&other).as_deref(), Some("Backlog"));
        assert_eq!(project_of(&orphan), None);

        // The active draft is still the active one, and still writable.
        slot.append(spoken("and the menu")).expect("append");
    }

    #[test]
    fn a_project_rename_with_nothing_to_sweep_still_reports_success() {
        let (_tmp, mut slot) = slot();
        dictated(&slot, Some("Backlog"), "someone else's note");
        assert!(rename(
            &mut slot,
            SessionCommand::RenameProject {
                from: "Ludo".to_owned(),
                to: "Checkout rebuild".to_owned(),
            },
        )
        .is_empty());
        assert!(slot.draft.is_none(), "the sweep opened a draft of its own");
    }

    /// A drop's one outcome, from the one event it answers with.
    fn dropped(slot: &mut DraftSlot, command: SessionCommand) -> Result<NoteMove, String> {
        let mut out = Vec::new();
        let change = slot.apply(command, &mut out);
        // A drop moves no line and activates no draft: the window catches up
        // through the re-list the shell does when this event lands.
        assert_eq!(change, Change::NOTHING);
        match out.as_slice() {
            [SessionEvent::NoteMoved { outcome, .. }] => outcome.clone(),
            other => panic!("expected exactly one move outcome, got {other:?}"),
        }
    }

    /// The drop as the shell sends it: ask about a clash, no header, dividers
    /// as the default.
    fn drop_into(project: &str, old_root: &Path, new_root: &Path) -> SessionCommand {
        SessionCommand::SetDraftProject {
            project: Some(project.to_owned()),
            old_root: Some(old_root.to_path_buf()),
            new_root: Some(new_root.to_path_buf()),
            clash: ClashChoice::Ask,
            header: None,
            dividers: SessionDividers::default(),
        }
    }

    #[test]
    fn moving_the_active_note_carries_its_file_into_the_new_project() {
        // The drop, on the worker: the live handle does it, the `.md` lands in
        // the target folder, and the binding is relative to it by construction.
        let (tmp, mut slot) = slot();
        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        std::fs::create_dir_all(&ludo).expect("notes folder");

        let mut draft = slot.store.create_draft(Some("Ludo")).expect("create");
        draft
            .append_line(spoken("the fence clips"))
            .expect("append");
        draft
            .save_to(
                &ludo.join("session 1.md"),
                SaveOptions {
                    notes_root: Some(&ludo),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        let dir = draft.dir().to_path_buf();
        slot.apply(SessionCommand::SetDraft(Box::new(draft)), &mut Vec::new());
        slot.project = Some("Ludo".to_owned());

        let outcome = dropped(&mut slot, drop_into("Backlog", &ludo, &backlog)).expect("the drop");
        assert_eq!(
            outcome,
            NoteMove::Moved {
                from: ludo.join("session 1.md"),
                to: backlog.join("session 1.md"),
                copied: false,
            }
        );

        let active = slot.draft.as_ref().expect("still active");
        assert_eq!(active.project(), Some("Backlog"));
        assert_eq!(
            active.saved_path(),
            Some(Path::new("session 1.md")),
            "the binding is relative to the folder the file is now in"
        );
        assert!(note_files(&ludo).is_empty(), "the file stayed behind");
        assert_eq!(note_files(&backlog), vec!["session 1.md".to_owned()]);

        // Written by the live handle, so it is on disk.
        let reloaded = Draft::open(&dir).expect("reopen");
        assert_eq!(reloaded.draft.project(), Some("Backlog"));

        // The tag on the *next* lazily created draft is untouched: moving one
        // note says nothing about where the next dictated line is filed.
        assert_eq!(slot.project.as_deref(), Some("Ludo"));
        // And the note is still the active, writable one.
        slot.append(spoken("and the menu")).expect("append");
    }

    #[test]
    fn a_clash_on_the_active_note_writes_nothing_until_it_is_answered() {
        // The whole of the clash rule on the worker: the question comes back, the
        // note has not moved, and neither file has been touched.
        let (tmp, mut slot) = slot();
        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        std::fs::create_dir_all(&backlog).expect("target folder");
        std::fs::write(backlog.join("session 1.md"), b"someone else's note").expect("seed");

        let mut draft = slot.store.create_draft(Some("Ludo")).expect("create");
        draft
            .append_line(spoken("the fence clips"))
            .expect("append");
        draft
            .save_to(
                &ludo.join("session 1.md"),
                SaveOptions {
                    notes_root: Some(&ludo),
                    ..SaveOptions::default()
                },
            )
            .expect("first save");
        slot.apply(SessionCommand::SetDraft(Box::new(draft)), &mut Vec::new());

        let outcome = dropped(&mut slot, drop_into("Backlog", &ludo, &backlog))
            .expect("a question, not a failure");
        assert_eq!(
            outcome,
            NoteMove::Clash {
                at: backlog.join("session 1.md"),
                free: Some(backlog.join("session 1 (2).md")),
            }
        );
        assert_eq!(
            slot.draft.as_ref().expect("still active").project(),
            Some("Ludo"),
            "the project moved while a question was open"
        );
        assert_eq!(note_files(&ludo), vec!["session 1.md".to_owned()]);
        assert_eq!(
            std::fs::read(backlog.join("session 1.md")).expect("read"),
            b"someone else's note"
        );

        // The answer: keep both, and now — and only now — anything moves.
        let outcome = dropped(
            &mut slot,
            SessionCommand::SetDraftProject {
                project: Some("Backlog".to_owned()),
                old_root: Some(ludo.clone()),
                new_root: Some(backlog.clone()),
                clash: ClashChoice::KeepBoth,
                header: None,
                dividers: SessionDividers::default(),
            },
        )
        .expect("keep both");
        assert_eq!(
            outcome,
            NoteMove::Moved {
                from: ludo.join("session 1.md"),
                to: backlog.join("session 1 (2).md"),
                copied: false,
            }
        );
        assert_eq!(
            std::fs::read(backlog.join("session 1.md")).expect("read"),
            b"someone else's note",
            "keep both wrote over the note that was there"
        );
    }

    #[test]
    fn moving_a_note_with_no_active_draft_is_an_answer_not_a_panic() {
        let (_tmp, mut slot) = slot();
        let outcome = dropped(
            &mut slot,
            SessionCommand::SetDraftProject {
                project: None,
                old_root: None,
                new_root: None,
                clash: ClashChoice::Ask,
                header: None,
                dividers: SessionDividers::default(),
            },
        );
        assert!(outcome.is_err(), "{outcome:?}");
    }

    // -----------------------------------------------------------------------
    // Move to note… — filesystem only. What these pin is the
    // *ordering* — destination first, source last — and the four refusals,
    // because the line's own arrival is `draft.rs`'s to prove.
    // -----------------------------------------------------------------------

    /// One line in a fresh active draft, plus an empty note to move it into.
    /// Returns the line id and the destination's id.
    fn a_line_and_somewhere_to_put_it(slot: &mut DraftSlot, text: &str) -> (String, String) {
        slot.append(spoken(text)).expect("append");
        let target = slot.store.create_draft(Some("playtest")).expect("create");
        let target_id = target.id().to_owned();
        // The destination is never held open: the worker opens it per line.
        drop(target);
        let line = slot
            .draft
            .as_ref()
            .expect("a draft is active")
            .read_lines()
            .expect("read")[0]
            .id
            .clone();
        (line, target_id)
    }

    /// The lines of a draft that is *not* the active one, off disk.
    fn lines_of(slot: &DraftSlot, id: &str) -> Vec<LineRecord> {
        let dir = slot.store.draft_path(id).expect("path");
        Draft::open(&dir).expect("open").lines
    }

    /// The one error a batch of events carries, or a panic naming what it got.
    fn only_error(out: &[SessionEvent]) -> String {
        let errors: Vec<&String> = out
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Error { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "expected one refusal, got {out:?}");
        errors[0].clone()
    }

    /// Make a file refuse writes, and say whether the platform meant it.
    ///
    /// The only way to break the disk between the two halves of a move without
    /// a hook in the code under test. A platform that ignores the bit — a run
    /// as root — cannot make the assertion, so it is *checked* rather than
    /// assumed.
    fn make_unwritable(path: &Path) -> bool {
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(path, perms).expect("set permissions");
        std::fs::OpenOptions::new().append(true).open(path).is_err()
    }

    fn make_writable(path: &Path) {
        let mut perms = std::fs::metadata(path).expect("metadata").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms).expect("set permissions");
    }

    #[test]
    fn a_moved_line_arrives_in_the_destination_and_leaves_the_open_note() {
        let (_tmp, mut slot) = slot();
        let (line, target_id) = a_line_and_somewhere_to_put_it(&mut slot, "the fence clips");
        let mut out = Vec::new();

        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: line.clone(),
                target_id: target_id.clone(),
                divide: true,
            },
            &mut out,
        );

        assert_eq!(change, Change::TRANSCRIPT);
        assert!(out.is_empty(), "a move that worked says nothing: {out:?}");

        // The destination has it, under a fresh id and with its stamp.
        let arrived = lines_of(&slot, &target_id);
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].text, "the fence clips");
        assert_ne!(arrived[0].id, line);
        assert!(!arrived[0].deleted);

        // The source still has the record — soft, always soft — and the handle
        // stopped counting it.
        let source = slot.draft.as_ref().expect("a draft is active");
        let left = source.read_lines().expect("read");
        assert_eq!(left.len(), 1);
        assert!(left[0].deleted);
        assert_eq!(source.line_count(), 0);
    }

    #[test]
    fn the_batchs_first_line_divides_and_the_rest_of_it_does_not() {
        // The plumbing, end to end on the worker: `divide` arrives on the
        // command and reaches `import_line` unchanged, so a two-line batch
        // into a note that already has lines becomes **one** sitting. What a
        // marker then does to the file is draft.rs's to prove; this is that
        // the flag is not dropped on the way through.
        let (_tmp, mut slot) = slot();
        let (first, target_id) = a_line_and_somewhere_to_put_it(&mut slot, "the fence clips");
        slot.append(spoken("and the gate does too"))
            .expect("append");
        let second = slot
            .draft
            .as_ref()
            .expect("a draft is active")
            .read_lines()
            .expect("read")[1]
            .id
            .clone();
        // Something for the divider to separate the batch from.
        let dir = slot.store.draft_path(&target_id).expect("path");
        let mut target = Draft::open(&dir).expect("open").draft;
        target.append_line(spoken("already here")).expect("append");
        drop(target);

        let mut out = Vec::new();
        for (at, id) in [first, second].into_iter().enumerate() {
            slot.apply(
                SessionCommand::MoveLineTo {
                    id,
                    target_id: target_id.clone(),
                    divide: at == 0,
                },
                &mut out,
            );
        }
        assert!(out.is_empty(), "both moves worked: {out:?}");

        let transcript = Draft::open(&dir)
            .expect("open")
            .draft
            .read_transcript()
            .expect("read");
        assert_eq!(
            transcript.sessions,
            vec![0, 1, 1],
            "one sitting for the batch, not one per line"
        );
    }

    #[test]
    fn a_line_lands_in_the_destination_before_it_leaves_the_source() {
        // The crash story, made into a test: break the disk between the two
        // halves and the line is in **both** notes. Never in neither — that is
        // the direction invariant 4 chooses.
        let (_tmp, mut slot) = slot();
        let (line, target_id) = a_line_and_somewhere_to_put_it(&mut slot, "the door clips");
        let source_log = slot.draft.as_ref().expect("a draft is active").lines_path();
        if !make_unwritable(&source_log) {
            // A platform that ignores the read-only bit cannot break the source
            // this way; the rest of the assertion would be meaningless.
            make_writable(&source_log);
            return;
        }

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: line.clone(),
                target_id: target_id.clone(),
                divide: true,
            },
            &mut out,
        );

        let refusal = only_error(&out);
        assert!(refusal.contains(&line), "{refusal}");
        assert!(refusal.contains("in both"), "{refusal}");
        assert_eq!(
            change,
            Change::TRANSCRIPT,
            "the destination grew, so the window has to re-list"
        );

        // In both, which is the point.
        assert_eq!(lines_of(&slot, &target_id).len(), 1);
        make_writable(&source_log);
        let left = slot
            .draft
            .as_ref()
            .expect("a draft is active")
            .read_lines()
            .expect("read");
        assert_eq!(left.len(), 1);
        assert!(!left[0].deleted, "the source line is still in the note");
    }

    #[test]
    fn a_destination_that_cannot_take_the_line_leaves_it_exactly_where_it_was() {
        // The other direction: nothing reached the destination, so the source
        // delete is skipped entirely and the line has not moved at all.
        let (_tmp, mut slot) = slot();
        let (line, target_id) = a_line_and_somewhere_to_put_it(&mut slot, "the lift never arrives");
        // A file where the audio directory should be: the wav copy is the first
        // write of the destination half, and it cannot even create the folder.
        let audio = slot
            .store
            .draft_path(&target_id)
            .expect("path")
            .join("audio");
        std::fs::remove_dir(&audio).expect("the fresh draft's audio folder is empty");
        std::fs::write(&audio, b"not a directory").expect("occupy the audio path");

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: line.clone(),
                target_id: target_id.clone(),
                divide: true,
            },
            &mut out,
        );

        assert_eq!(change, Change::NOTHING);
        let refusal = only_error(&out);
        assert!(refusal.contains(&line), "{refusal}");
        assert!(refusal.contains("still in this note"), "{refusal}");

        assert!(lines_of(&slot, &target_id).is_empty());
        let source = slot.draft.as_ref().expect("a draft is active");
        let left = source.read_lines().expect("read");
        assert!(!left[0].deleted, "the line never left");
        assert_eq!(source.line_count(), 1);
    }

    #[test]
    fn moving_a_line_into_the_note_it_is_already_in_is_refused() {
        // The one-handle rule, and it does not depend on the chooser having
        // excluded the open note: the worker refuses before it opens anything.
        let (_tmp, mut slot) = slot();
        let (line, _) = a_line_and_somewhere_to_put_it(&mut slot, "already here");
        let active = slot
            .draft
            .as_ref()
            .expect("a draft is active")
            .id()
            .to_owned();

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: line.clone(),
                target_id: active,
                divide: true,
            },
            &mut out,
        );

        assert_eq!(change, Change::NOTHING);
        assert!(only_error(&out).contains(&line));
        let source = slot.draft.as_ref().expect("a draft is active");
        assert_eq!(source.read_lines().expect("read").len(), 1);
        assert!(!source.read_lines().expect("read")[0].deleted);
    }

    #[test]
    fn moving_a_line_into_a_note_that_is_not_there_is_refused_and_changes_nothing() {
        let (_tmp, mut slot) = slot();
        let (line, _) = a_line_and_somewhere_to_put_it(&mut slot, "nowhere to go");

        for target in ["01JCPNOSUCHDRAFTATALL0", "../escape"] {
            let mut out = Vec::new();
            let change = slot.apply(
                SessionCommand::MoveLineTo {
                    id: line.clone(),
                    target_id: target.to_owned(),
                    divide: true,
                },
                &mut out,
            );
            assert_eq!(change, Change::NOTHING, "for {target}");
            let refusal = only_error(&out);
            assert!(refusal.contains(&line), "{refusal}");

            let source = slot.draft.as_ref().expect("a draft is active");
            let left = source.read_lines().expect("read");
            assert_eq!(left.len(), 1);
            assert!(!left[0].deleted, "for {target}");
            assert_eq!(source.line_count(), 1);
        }
        // And exactly two drafts still exist: the refusals created nothing.
        assert_eq!(slot.store.list_drafts().expect("list").drafts.len(), 2);
    }

    #[test]
    fn moving_a_line_with_no_active_note_is_an_answer_not_a_panic() {
        let (_tmp, mut slot) = slot();
        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: "01JCPNOLINE0000000000".to_owned(),
                target_id: "01JCPNOTARGET00000000".to_owned(),
                divide: true,
            },
            &mut out,
        );
        assert_eq!(change, Change::NOTHING);
        assert!(only_error(&out).contains("no active note"));
    }

    #[test]
    fn moving_a_line_the_open_note_does_not_have_is_refused() {
        let (_tmp, mut slot) = slot();
        let (_, target_id) = a_line_and_somewhere_to_put_it(&mut slot, "a real line");

        let mut out = Vec::new();
        let change = slot.apply(
            SessionCommand::MoveLineTo {
                id: "01JCPNOSUCHLINEATALL0".to_owned(),
                target_id: target_id.clone(),
                divide: true,
            },
            &mut out,
        );

        assert_eq!(change, Change::NOTHING);
        assert!(only_error(&out).contains("01JCPNOSUCHLINEATALL0"));
        assert!(
            lines_of(&slot, &target_id).is_empty(),
            "the destination was never opened"
        );
    }

    // -----------------------------------------------------------------------
    // The lazy session marker. "This process" is simulated by building a second
    // `DraftSlot` over the same store, which is exactly what a relaunch is.
    // -----------------------------------------------------------------------

    /// The active draft's session ordinals, in rendered order.
    fn sessions(slot: &DraftSlot) -> Vec<usize> {
        slot.draft
            .as_ref()
            .expect("a draft is active")
            .read_transcript()
            .expect("read")
            .sessions
    }

    #[test]
    fn a_resumed_draft_gets_exactly_one_marker_before_its_next_line() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path().join("drafts");
        let fresh_slot = || DraftSlot {
            store: crate::draft::DraftStore::new(root.clone()),
            project: None,
            draft: None,
            sittings: HashMap::new(),
        };

        let mut first = fresh_slot();
        first.append(spoken("said last night")).expect("append");
        let dir = match first.changed_event() {
            SessionEvent::DraftChanged { dir, .. } => dir.expect("active"),
            other => panic!("{other:?}"),
        };
        // No marker while it is still the same sitting.
        assert_eq!(sessions(&first), vec![0]);
        first.append(spoken("still last night")).expect("append");
        assert_eq!(sessions(&first), vec![0, 0]);
        drop(first);

        // A new process: a fresh slot over the same store, which is exactly
        // what a relaunch is.
        let mut next = fresh_slot();
        let resumed = Draft::open(&dir).expect("open").draft;
        let id = resumed.id().to_owned();
        let mut out = Vec::new();
        next.apply(SessionCommand::SetDraft(Box::new(resumed)), &mut out);
        // Resuming alone writes nothing — a note nobody spoke into again must
        // not grow a divider.
        assert_eq!(sessions(&next), vec![0, 0]);

        next.append(spoken("said this morning")).expect("append");
        assert_eq!(sessions(&next), vec![0, 0, 1]);
        // And only one marker, however many more lines this sitting gets.
        next.append(spoken("also this morning")).expect("append");
        assert_eq!(sessions(&next), vec![0, 0, 1, 1]);

        // Switching away and back is not a new sitting.
        let other = next.store.create_draft(None).expect("create");
        next.apply(SessionCommand::SetDraft(Box::new(other)), &mut out);
        let back = Draft::open(&dir).expect("reopen").draft;
        assert_eq!(back.id(), id);
        next.apply(SessionCommand::SetDraft(Box::new(back)), &mut out);
        next.append(spoken("and one more")).expect("append");
        assert_eq!(sessions(&next), vec![0, 0, 1, 1, 1]);
    }

    #[test]
    fn a_draft_created_in_this_process_never_gets_a_marker() {
        // Lazy creation is a first sitting by definition, and so is "New note".
        let (_tmp, mut slot) = slot();
        slot.append(spoken("first")).expect("append");
        slot.append(spoken("second")).expect("append");
        assert_eq!(sessions(&slot), vec![0, 0]);

        let fresh = slot.store.create_draft(None).expect("create");
        let mut out = Vec::new();
        slot.apply(SessionCommand::SetDraft(Box::new(fresh)), &mut out);
        slot.append(spoken("in the new note")).expect("append");
        assert_eq!(sessions(&slot), vec![0]);
    }

    #[test]
    fn a_flattened_error_keeps_its_causes() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Inner);
        #[derive(Debug, thiserror::Error)]
        #[error("inner detail")]
        struct Inner;

        assert_eq!(flatten(&Outer(Inner)), "outer: inner detail");
    }
}
