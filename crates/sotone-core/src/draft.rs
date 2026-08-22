//! The draft store: where a dictation session lives before it is ever saved.
//!
//! The rule is "Notepad semantics". A draft is durable from the first
//! utterance, survives app close, crash and reboot indefinitely, and goes away
//! only when the user explicitly discards it. Nothing in this module
//! deletes, truncates or rewrites a line the user spoke — that is invariant 4,
//! and it is the whole reason this module exists as its own layer instead of
//! being a `Vec` in the worker.
//!
//! Layout, one directory per draft:
//!
//! ```text
//! <drafts_root>/<draft_ulid>/
//!     meta.json                 rewritten whole, atomically
//!     lines.jsonl               append-only, fsync'd per line
//!     audio/<line_ulid>.wav     16 kHz mono 16-bit PCM
//! ```
//!
//! Four properties are load-bearing:
//!
//! * **Appends are appends.** `lines.jsonl` is opened in append mode and each
//!   record is one `write_all` followed by `sync_data`. This is the one
//!   deliberate exception to the write-through-[`fsutil`] rule:
//!   rewriting the whole file per line would be O(n²) and would trade away
//!   exactly the crash property this store needs ("fsync'd per line"). `meta.json`
//!   is small, is rewritten whole, and does go through
//!   [`fsutil::write_atomic`].
//! * **Wav first, then the record.** A crash between the two leaves an orphan
//!   wav, which is invisible and harmless. The reverse order would leave a
//!   record pointing at audio that does not exist.
//! * **Edits are appends too.** The user can retouch a line after the
//!   session. Nothing is rewritten: a correction is an [`EditRecord`] appended
//!   after the base record, and the loader folds the two (see [`parse_lines`]).
//!   A delete is one of those corrections, so a spoken line is never removed
//!   from the file at all. A [`MoveRecord`] works on the same terms:
//!   reordering the transcript is an append, and the file order of the
//!   [`LineRecord`]s stays the record of the order things were *spoken* in.
//!   There is also a [`SessionRecord`] — "the app was restarted here" — which is
//!   an append for exactly the same reasons.
//! * **Torn tails are repaired by adding, never by removing.** If a crash cut
//!   an append in half, the loader skips the trailing partial record and counts
//!   it; the writer prefixes its next append with a single `\n` so those orphan
//!   bytes become an isolated malformed line. No byte the user's machine wrote
//!   is ever removed.
//! * **Worker-thread-only.** [`Draft::append_line`] does blocking, fsync'ing
//!   I/O. Like [`Transcriber::transcribe`](crate::transcribe::Transcriber),
//!   it must never be called from the `rdev` hook callback or the `cpal` data
//!   callback: blocking either one stutters the whole OS input or audio stack,
//!   including the app under test (invariant 5). The API is synchronous and
//!   carries no locking — the single transcription worker is given
//!   exclusive ownership of a [`Draft`].
//!
//! Nothing here touches the network. Every path in this module is a local file
//! under a root the caller chose (invariant 3).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset, Local};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::audio::TARGET_SAMPLE_RATE;
use crate::fsutil;
use crate::render;
use crate::savepath;
use crate::template;

/// Directory name used under the platform data directory. Mirrors the private
/// constant in [`config`](crate::config); duplicated rather than shared so that
/// module keeps its internals to itself.
const APP_DIR: &str = "sotone";

/// The append-only record of what was spoken, one JSON object per line.
const LINES_FILE: &str = "lines.jsonl";

/// Draft-level state: id, timestamps, save binding, dirty flag.
const META_FILE: &str = "meta.json";

/// Per-line audio, named by line ulid.
const AUDIO_DIR: &str = "audio";

/// Schema version written into `meta.json`. Bumped only when a field changes
/// meaning; new *optional* fields default instead, so version-1 files keep
/// loading.
const META_VERSION: u32 = 1;

/// Where discarded drafts go, directly under the drafts root.
///
/// The rule is: discards go to `.trash/`, swept after 30 days. A discard is a
/// `rename` into here and nothing else — the user's lines, audio and metadata
/// are byte-for-byte the files they were, one directory over (invariant 4). The
/// leading dot is what keeps it out of [`DraftStore::list_drafts`].
pub const TRASH_DIR: &str = ".trash";

/// How long a discarded draft is kept before the startup sweep removes it.
///
/// The 30 days are fixed. The sweep is the *only* permanent delete in the
/// codebase, and it is deliberately measured from the discard, not from the
/// draft's creation: a note dictated last year and discarded this morning has
/// been in the trash for one morning.
pub const TRASH_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Marker file written inside a trashed draft, holding the RFC3339 moment it
/// was discarded. Advisory: the sweep falls back to the directory's modified
/// time, and skips an entry whose age it cannot establish at all.
const DISCARDED_MARKER: &str = "discarded_at";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a draft could not be created, appended to, or loaded.
///
/// Every variant names the path it failed on: a draft is the user's unsaved
/// work, so "could not write draft" without a location is unactionable.
#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    /// A filesystem operation failed. `action` completes the sentence
    /// "could not …".
    #[error("could not {action} {}: {source}", .path.display())]
    Io {
        /// What was being attempted, e.g. `"append to"`.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// `meta.json` is missing its contents or is not valid JSON. This is a hard
    /// error rather than a default-and-carry-on: the draft is damaged, and
    /// guessing an id or a `saved_path` could send a later save to the wrong
    /// file.
    #[error(
        "the draft metadata at {} is not readable as JSON ({source}) — \
         the draft directory is damaged; its lines.jsonl and audio are untouched",
        .path.display()
    )]
    BadMeta {
        /// Path of the offending `meta.json`.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: serde_json::Error,
    },

    /// A record could not be turned into JSON. Practically impossible for these
    /// types, but it is not worth a panic to say so.
    #[error("could not encode a draft record for {}: {source}", .path.display())]
    Encode {
        /// Path the record was destined for.
        path: PathBuf,
        /// Underlying serialisation error.
        #[source]
        source: serde_json::Error,
    },

    /// The caller named something that is not a draft directly under the root.
    ///
    /// Draft ids arrive over IPC, so this is the boundary check: anything with
    /// a separator, a `..`, a drive prefix or a leading dot in it is refused
    /// before it can be joined onto the root and escape it.
    #[error("{id:?} does not name a draft in {}", .path.display())]
    BadId {
        /// What was asked for, quoted so an empty or whitespace id is visible.
        id: String,
        /// The drafts root it was asked for in.
        path: PathBuf,
    },

    /// The id is well-formed but there is no such draft directory.
    #[error("there is no draft at {}", .path.display())]
    Missing {
        /// Where it would have been.
        path: PathBuf,
    },

    /// An edit named a line this draft does not have. Line ids arrive over IPC
    /// the same way draft ids do, and an edit that silently lands nowhere would
    /// read to the user as "my correction did not stick".
    #[error("there is no line {id:?} in the draft at {}", .path.display())]
    NoSuchLine {
        /// The line that was asked for.
        id: String,
        /// The draft directory it was asked for in.
        path: PathBuf,
    },

    /// `.trash/` already holds an entry of that name. Ulids do not collide, so
    /// this means something else is going on and the draft is left exactly
    /// where it is rather than overwritten (invariant 4).
    #[error(
        "a discarded draft is already at {} — nothing was moved",
        .path.display()
    )]
    TrashCollision {
        /// The occupied trash path.
        path: PathBuf,
    },

    /// The file at `saved_path` is not the file we last wrote there, so the
    /// save stopped before writing anything (invariant 4). On mismatch, stop
    /// and offer overwrite or diff rather than clobbering silently.
    #[error(
        "{} has been edited since Sotone last wrote it — nothing was saved",
        .path.display()
    )]
    SaveConflict {
        /// The markdown file that changed underneath us, so the UI can name it.
        path: PathBuf,
    },

    /// [`Draft::save`] on a draft that has never been bound to a file. The
    /// caller has to choose a path first; there is nothing sensible to guess.
    #[error("this draft has not been saved to a file yet ({})", .path.display())]
    NeverSaved {
        /// The draft directory, for context in a log line.
        path: PathBuf,
    },

    /// A rename would land on a file that already exists, so nothing moved.
    ///
    /// [`std::fs::rename`] **replaces** an existing destination on Windows and
    /// POSIX alike, so this check is the only thing standing between a rename
    /// and somebody else's note being overwritten (invariant 4). Never
    /// auto-numbered into `note (2).md`: that invents a filename the user did
    /// not ask for, and a refusal that says which file is in the way is the
    /// honest answer.
    #[error(
        "{} already exists — nothing was renamed",
        .path.display()
    )]
    NameTaken {
        /// The occupied target, so the message can name it.
        path: PathBuf,
    },

    /// The draft's `saved_path` is stored relative to a project's notes folder
    /// and no folder was supplied to resolve it against — the project it
    /// belonged to is gone from the config, or was renamed.
    ///
    /// Never a panic and never a guess: joining a relative binding onto whatever
    /// directory the process happens to be in is how a note gets written
    /// somewhere nobody chose.
    #[error(
        "the note for the draft at {} is stored as {} inside its project's notes folder, \
         and that project is no longer in the configuration — nothing was saved",
        .path.display(),
        .relative.display()
    )]
    UnresolvedPath {
        /// The draft directory, which names the draft in the message.
        path: PathBuf,
        /// The relative binding that could not be resolved.
        relative: PathBuf,
    },

    /// The per-line wav could not be encoded.
    #[error("could not encode the audio for a draft line at {}: {source}", .path.display())]
    Wav {
        /// Path of the wav being written.
        path: PathBuf,
        /// Underlying `hound` error.
        #[source]
        source: hound::Error,
    },

    /// A line was asked to be moved into the draft it came out of.
    ///
    /// Structural, not a nicety: [`Draft::import_line`] appending a line its
    /// own [`Draft::export_line`] produced would mean two live handles on one
    /// directory — the thing this crate has no interior locking for — so the
    /// carrier remembers where it came from and the import refuses rather than
    /// trusting the caller to have checked.
    #[error("that line is already in the draft at {} — nothing was moved", .path.display())]
    SameDraft {
        /// The draft that was asked to import its own line.
        path: PathBuf,
    },
}

impl DraftError {
    /// The path the failure is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Io { path, .. }
            | Self::BadMeta { path, .. }
            | Self::Encode { path, .. }
            | Self::BadId { path, .. }
            | Self::Missing { path }
            | Self::NoSuchLine { path, .. }
            | Self::TrashCollision { path }
            | Self::SaveConflict { path }
            | Self::NeverSaved { path }
            | Self::NameTaken { path }
            | Self::UnresolvedPath { path, .. }
            | Self::Wav { path, .. }
            | Self::SameDraft { path } => path,
        }
    }
}

/// Shorthand for the `map_err` at every filesystem call site: the path is what
/// makes these errors actionable, so it is never optional.
fn io_err<'a>(action: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> DraftError + 'a {
    move |source| DraftError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

// ---------------------------------------------------------------------------
// On-disk records
// ---------------------------------------------------------------------------

/// Contents of `meta.json`.
///
/// Every field added after version 1 must be `#[serde(default)]` so an older
/// draft still loads; unknown fields in a *newer* file are ignored rather than
/// preserved, which is why only additive changes are safe here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftMeta {
    /// Schema version. Currently [`META_VERSION`].
    pub version: u32,
    /// Draft ulid, matching the directory name.
    pub id: String,
    /// When the draft was created, RFC3339 with the local offset.
    pub created_at: DateTime<FixedOffset>,
    /// Project this draft belongs to, if any.
    #[serde(default)]
    pub project: Option<String>,
    /// The markdown file this draft is bound to. `None` until the first save;
    /// once set, re-saving rewrites that same file forever.
    #[serde(default)]
    pub saved_path: Option<PathBuf>,
    /// Whether there are changes not yet rendered to `saved_path`. Drives the
    /// unsaved indicator.
    #[serde(default)]
    pub dirty: bool,
    /// blake3 of the last bytes we wrote to `saved_path`, for external-edit
    /// detection.
    #[serde(default)]
    pub last_save_hash: Option<String>,
}

/// One utterance: a line of `lines.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRecord {
    /// Line ulid. Lexically sortable, so file order and creation order agree.
    pub id: String,
    /// When the key was *released*, supplied by the caller (timestamps are
    /// captured at release, not at write time, so rapid-fire notes keep the
    /// order they were spoken). The store never calls `now()` for this.
    pub spoken_at: DateTime<FixedOffset>,
    /// The text as it will be rendered to markdown.
    pub text: String,
    /// What the model first produced, kept when the user edits `text` so the
    /// markdown stays clean while the audit trail survives. `None`
    /// until an edit happens.
    #[serde(default)]
    pub original: Option<String>,
    /// Soft delete. The schema carries it from day one so that line editing
    /// needs no migration; nothing sets it yet.
    #[serde(default)]
    pub deleted: bool,
    /// Relative path to the audio, always `audio/<line_ulid>.wav`. Relative so
    /// a draft directory can be moved or copied without breaking.
    pub audio: String,
    /// How long whisper took, milliseconds. Diagnostics only.
    #[serde(default)]
    pub transcribe_ms: Option<u64>,
    /// The model could not transcribe this utterance, so the line was written
    /// with **no text and its audio kept**. The design's rule:
    /// "a line that cannot be transcribed keeps its audio and offers Retry.
    /// Nothing is discarded because a dependency failed."
    ///
    /// Written only when true, so every draft that existed before this field
    /// did is byte-identical on disk and parses with `failed: false`. It is
    /// **not** a discriminator: the four record shapes are still told apart by
    /// `edit_of` / `move_of` / `session_at`, and none of those three may ever
    /// gain this field either.
    ///
    /// On a *read* this is the folded state, not the base record's:
    ///
    /// > folded `failed` = the base record failed **and** the folded text is
    /// > still empty.
    ///
    /// The flag on disk is permanent history — this utterance is one the model
    /// refused, forever — and the *text* decides whether that is still the
    /// line's state. Words resolve it (a successful Retry, or the user typing
    /// what they said); an edit back to empty text returns the line to it. The
    /// rule is symmetric on purpose: undo replays the inverse `line_edit`
    /// directly, so a one-way clear would let an undone resolve leave an empty
    /// *ok* line — an empty bullet in the note, with the failure and its Retry
    /// gone. See [`parse_lines`].
    #[serde(default, skip_serializing_if = "not_failed")]
    pub failed: bool,
}

/// `skip_serializing_if` for [`LineRecord::failed`]: the field only appears in
/// the log when it is true.
// serde's `skip_serializing_if` hands the field by reference; the lint's
// suggestion would not compile against it.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn not_failed(failed: &bool) -> bool {
    !*failed
}

/// A correction to a line that is already in the log.
///
/// **This is why `lines.jsonl` never has to be rewritten.** Line editing needed
/// a way to change a line's text or hide it, and the two candidates were
/// "rewrite the whole file atomically" and "append a correction record". The
/// rewrite loses: [`parse_lines`] deliberately *preserves* malformed and torn
/// bytes on disk, so rewriting the file from the parsed records would silently
/// destroy exactly those bytes — an invariant-4 violation hiding inside an
/// "atomic" operation. Appending also keeps the crash story unchanged (one
/// `write_all` + `sync_data`, torn-tail repair already handles a half-written
/// append), and it makes the audit trail structural: the base [`LineRecord`] is
/// forever the record of what was *spoken*, and these are the record of the
/// tidy-up.
///
/// The presence of `edit_of` is what tells the two record types apart, so
/// [`LineRecord`] must never gain a field of that name. Old drafts, which
/// contain only [`LineRecord`]s, parse unchanged — no migration, no version
/// bump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRecord {
    /// The ulid of the line this corrects.
    pub edit_of: String,
    /// When the edit was made. The store *may* call `now()` here: the
    /// captured-at-release rule is about `spoken_at`, which this never touches.
    pub at: DateTime<FixedOffset>,
    /// New text. Absent means this record does not touch the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// New soft-delete state. Absent means this record does not touch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    /// How long a re-transcribe took, milliseconds. Diagnostics only, and it
    /// mirrors the base record's field so both come from the same place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe_ms: Option<u64>,
}

/// A change to where a line sits in the transcript.
///
/// The alternative was a stored `line_number` on each line, which
/// loses on two counts: it has to be **renumbered on every insert** (one drag
/// becomes O(n) correction records, and a crash mid-renumber can leave two
/// lines claiming the same slot), and it overwrites the record of what was
/// spoken. A move record is one appended record per drag, crash-safe by exactly
/// the argument every other append here uses, and the spoken order stays
/// recoverable forever — it is simply the file order of the [`LineRecord`]s.
///
/// The presence of `move_of` is the discriminator, so neither [`LineRecord`]
/// nor [`EditRecord`] may ever gain a field of that name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveRecord {
    /// The ulid of the line that moved.
    pub move_of: String,
    /// When it was moved. As with [`EditRecord::at`], the store may call
    /// `now()`: the captured-at-release rule governs `spoken_at`, which no move
    /// touches.
    pub at: DateTime<FixedOffset>,
    /// The line this one now sits immediately after; `null` means "to the top".
    ///
    /// **Required and nullable, not optional.** A move with no destination is
    /// meaningless, so a record missing the field is malformed and is skipped
    /// like any other malformed line, rather than silently reading as "move to
    /// the top" — `null` and absent must not be the same thing.
    ///
    /// The `deserialize_with` is what enforces that, and it is not decoration:
    /// serde's derive quietly fills a *plain* `Option` field with `None` when
    /// the key is absent, `#[serde(default)]` or not. Routing the field through
    /// a function makes the generated code demand the key. Pinned by
    /// `an_orphan_or_badly_anchored_move_is_counted_and_kept`.
    #[serde(deserialize_with = "required_nullable")]
    pub after: Option<String>,
}

/// A marker saying "everything after this was dictated in a later sitting".
///
/// A resumed session shows as a `---` rule in the saved
/// markdown, so the note reads as the two sittings it was. The alternative was a
/// per-line "session id" field, which loses on the same grounds
/// [`MoveRecord`] beat a stored line number: it would have to be written into
/// every [`LineRecord`], changing a shape that old drafts already have on disk.
/// One appended marker changes nothing that exists and costs one record per
/// resumed sitting.
///
/// The presence of `session_at` is the discriminator, so none of
/// [`LineRecord`], [`EditRecord`] or [`MoveRecord`] may ever gain a field of
/// that name. A draft written before markers existed contains none, so every
/// line in it has session ordinal 0 and renders exactly as it always did.
///
/// The marker carries no id and points at nothing: it is a *position* in the
/// file, and its meaning is entirely "the base records after me belong to a
/// later session than the ones before me".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// When the sitting that follows it started. As with [`EditRecord::at`],
    /// the store may call `now()`: the captured-at-release rule governs
    /// `spoken_at`, which no marker touches.
    pub session_at: DateTime<FixedOffset>,
}

/// Whether a resumed sitting shows as a `---` rule in the rendered markdown.
///
/// Not a `bool` at the call sites, for the reason [`SaveMode`] is not one
/// either: `save(header, mode, true)` says nothing about what is being turned
/// on. Comes from the active project's `session_dividers` key, which defaults
/// to on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionDividers {
    /// A `---` between consecutive rendered lines from different sittings.
    #[default]
    Shown,
    /// One continuous list, whatever the markers say.
    Hidden,
}

impl SessionDividers {
    /// The config key's `bool` as the enum, at the one boundary where a `bool`
    /// is what we actually have.
    #[must_use]
    pub const fn when(shown: bool) -> Self {
        if shown {
            Self::Shown
        } else {
            Self::Hidden
        }
    }
}

/// `Option<String>`, but the key has to be there.
///
/// See [`MoveRecord::after`]. Only reachable when serde found the field, so the
/// body is simply the normal `Option` deserialiser.
fn required_nullable<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(deserializer)
}

/// What the caller hands to [`Draft::append_line`].
#[derive(Debug)]
pub struct NewLine<'a> {
    /// The transcribed text.
    pub text: String,
    /// Key-release time, from the caller.
    pub spoken_at: DateTime<FixedOffset>,
    /// The 16 kHz mono audio the transcriber consumed. May be empty.
    pub samples: &'a [f32],
    /// Transcription duration, if measured.
    pub transcribe_ms: Option<u64>,
    /// The model refused this utterance. One shared write path
    /// rather than a second constructor: a failed line is an ordinary line
    /// with no text, so it must get the same wav, the same fsync and the same
    /// torn-tail story as any other — and a sibling function would be a second
    /// place for those to drift.
    pub failed: bool,
}

/// One line lifted out of a draft, on its way into another.
///
/// The carrier for "Move to note…", and the reason the move needs no new record
/// shape and no new field. What travels is exactly what
/// [`parse_lines`] would have to see again to fold the line the same way in its
/// new home:
///
/// * `base_text` — the **base** record's words, which is what `original` is
///   derived from at fold time and is therefore the only way an edit history
///   survives a move;
/// * `text` — the folded words, replayed as one [`EditRecord`] on the far side
///   **iff** it differs from `base_text`;
/// * `spoken_at` — the source's own key-release stamp. Timestamps are the
///   product; a move is not a new utterance and must never re-stamp one;
/// * `failed_at_birth` — the base record's flag, not the folded reading, for
///   the reason [`Draft::correct`] needs the same thing: a failed line that has
///   since been given words folds to `failed: false` while its birth stays what
///   it was;
/// * `audio` — the wav's **bytes**, copied verbatim. No decode, no re-encode: a
///   move is not a re-recording.
///
/// It also remembers which draft it came out of, which is what lets
/// [`Draft::import_line`] refuse to append a line into the handle that
/// exported it (see [`DraftError::SameDraft`]). Nothing here names a *path*, so
/// this API cannot be used to open a second handle on anything: the caller must
/// already hold the two [`Draft`]s, and the worker holds them one at a time.
#[derive(Debug, Clone)]
pub struct MovedLine {
    /// The draft id this came out of.
    source: String,
    /// The line id it came from. Never reused on the far side — a fresh ulid is
    /// minted there, because `line_audio(draft_id, line_id)` would otherwise be
    /// ambiguous while the line sits in both drafts.
    line: String,
    /// The base record's text.
    base_text: String,
    /// The folded text, as the note renders it.
    text: String,
    /// The source's key-release stamp, carried untouched.
    spoken_at: DateTime<FixedOffset>,
    /// How long the model took, carried as diagnostics.
    transcribe_ms: Option<u64>,
    /// Whether the **base** record was written as a failure.
    failed_at_birth: bool,
    /// The wav's bytes, or `None` when the source's file is not there.
    ///
    /// A missing wav does not stop a move: the line goes, and its audio is
    /// carried as missing exactly as the source had it (the design rule:
    /// nothing is discarded because a dependency failed).
    audio: Option<Vec<u8>>,
}

impl MovedLine {
    /// The draft this line came out of.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The line it came from, for a log line or a message that has to name it.
    #[must_use]
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The words as the note renders them.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the source's wav was there to copy.
    #[must_use]
    pub const fn has_audio(&self) -> bool {
        self.audio.is_some()
    }
}

/// Lowercase blake3 hex of exactly these bytes.
///
/// The one place the guard's hash is computed, so "the hash of what we wrote"
/// and "the hash of what is on disk now" can never drift apart in spelling.
fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether a save may overwrite a file that changed underneath us.
///
/// Not a `bool`: `save_as(path, header, true)` at a call site says nothing
/// about what is being permitted, and the thing being permitted here is
/// "discard whatever someone else wrote".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SaveMode {
    /// Stop with [`DraftError::SaveConflict`] if the file on disk is not the
    /// one we last wrote. The default, and what the Save button uses.
    #[default]
    Guarded,
    /// Write regardless. Only ever reachable from an explicit "overwrite"
    /// answer to a conflict the user was already shown.
    Overwrite,
}

/// Everything a save needs beyond the file it is writing.
///
/// A struct rather than four more positional arguments: `save_to(path, header,
/// mode, dividers, root, project)` at a call site is unreadable, and two of
/// these six are `Option<&Path>`-shaped in a way a reader would have to count
/// commas to tell apart.
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveOptions<'a> {
    /// The project's header template, already expanded. Written verbatim at the
    /// top of the markdown.
    pub header: Option<&'a str>,
    /// Guarded (the default) or the explicit answer to a conflict.
    pub mode: SaveMode,
    /// Whether resumed sittings get a `---`.
    pub dividers: SessionDividers,
    /// The owning project's notes folder, when there is one.
    ///
    /// Two jobs, both about surviving a folder move: it resolves a relative
    /// `saved_path`, and a target that lies inside it is *stored* relative.
    /// With `None`, a save behaves exactly as it did before relative bindings
    /// existed — absolute in, absolute out.
    pub notes_root: Option<&'a Path>,
    /// Project to write into `meta.project` **if it does not have one**.
    ///
    /// A draft that already belongs to a project is never reassigned by a save;
    /// this is the adoption path for one dictated before any project existed.
    pub adopt_project: Option<&'a str>,
}

/// A stored `saved_path` as an absolute path, or `None` when it cannot be one.
///
/// The whole of the relative-binding rule, in one function so the resolution the
/// save does and the resolution the UI displays cannot drift apart:
///
/// * absolute stored → itself, whatever the root says (bindings written before
///   relative ones existed keep working, untouched, forever);
/// * relative stored + a root → `root.join(rel)`, which is what makes moving the
///   notes folder and re-pointing the project carry every note with it;
/// * relative stored + no root → `None`. The caller decides whether that is an
///   error (a save) or a blank (a tooltip).
#[must_use]
pub fn resolve_binding(stored: &Path, notes_root: Option<&Path>) -> Option<PathBuf> {
    if stored.is_absolute() {
        return Some(stored.to_path_buf());
    }
    notes_root.map(|root| root.join(stored))
}

/// How a just-written path is stored in `meta.json`.
///
/// Relative when it lies under the project's notes folder, absolute otherwise.
/// The second case should not arise — the save target always comes
/// from the project — but a hand-edited config or a bound note the user moved
/// elsewhere must not be *corrupted* into a relative path pointing at the wrong
/// file.
fn binding_for(path: &Path, notes_root: Option<&Path>) -> PathBuf {
    notes_root
        .and_then(|root| path.strip_prefix(root).ok())
        // A target equal to the root itself strips to "", which is not a note.
        .filter(|rel| !rel.as_os_str().is_empty())
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

/// Is `to` the file `from` already is, spelled with different capitalization?
///
/// The one shape of "the target exists" a rename may proceed on. Windows looks
/// filenames up case-insensitively, so `Ludo.md` →
/// `ludo.md` makes `to.exists()` answer `true` about the *source file itself*
/// and the occupied-target refusal turns a legitimate rename into
/// [`DraftError::NameTaken`]. NTFS performs a
/// case-only rename in place, so there is nothing to overwrite.
///
/// Believed only when all three checks agree: the same directory, filenames
/// equal ignoring case, and `fs::canonicalize` resolving both names to one
/// path. Anything that cannot be proved — either canonicalize failing, a name
/// that is not UTF-8 — answers `false` and the rename is refused, which is what
/// keeps "nothing is ever overwritten by a rename" intact. On a case-sensitive
/// filesystem this is never reached for a case-only target: `to.exists()` is
/// already `false` there.
fn is_case_only_rename(from: &Path, to: &Path) -> bool {
    if from.parent() != to.parent() {
        return false;
    }
    let (Some(here), Some(there)) = (
        from.file_name().and_then(OsStr::to_str),
        to.file_name().and_then(OsStr::to_str),
    ) else {
        return false;
    };
    // Unicode-aware rather than `eq_ignore_ascii_case`: a case-only rename of
    // "Ünlü.md" is the same gesture and must not be refused for its accents.
    if here.to_lowercase() != there.to_lowercase() {
        return false;
    }
    matches!(
        (fs::canonicalize(from), fs::canonicalize(to)),
        (Ok(one), Ok(other)) if one == other
    )
}

/// Is this the error a rename gives for "those are two different volumes"?
///
/// `io::ErrorKind::CrossesDevices` would say this in one word, and it is newer
/// than this workspace's `rust-version = 1.77`, so the errno is spelled out.
/// Deliberately narrow: every *other* rename failure is reported rather than
/// retried as a copy, because a copy that fails leaves the same untouched
/// original with a longer story attached.
#[cfg(windows)]
fn is_cross_device(err: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE.
    err.raw_os_error() == Some(17)
}

#[cfg(not(windows))]
fn is_cross_device(err: &io::Error) -> bool {
    // EXDEV.
    err.raw_os_error() == Some(18)
}

/// Move a file across a volume boundary without ever being able to lose it.
///
/// **The order is the guarantee** (invariant 4): read the source, write the
/// copy through [`fsutil::write_atomic`] (temp file in the target directory,
/// `sync_all`, atomic rename), read the copy *back* off disk, compare length
/// and hash — and only then remove the original. Every failure before that last
/// step leaves the source exactly as it was and reports; the caller's note is
/// still on disk under its old name.
///
/// The one delete is deliberate and is **not** a discard: the `.trash` rule
/// is about a note the user threw away, and a verified move is the same file in
/// a new place. Trashing the original here would leave the user two copies and
/// a `.trash` entry for a file they did not delete.
fn move_across_volumes(from: &Path, to: &Path) -> Result<(), DraftError> {
    let bytes = fs::read(from).map_err(io_err("read", from))?;
    fsutil::write_atomic(to, &bytes).map_err(io_err("write", to))?;

    let written = fs::read(to).map_err(io_err("read back", to))?;
    if written.len() != bytes.len() || hash_hex(&written) != hash_hex(&bytes) {
        // The copy is ours — the caller proved the target was free — so
        // removing it takes back only what this function just wrote, and the
        // original is untouched behind it.
        let _ = fs::remove_file(to);
        return Err(DraftError::Io {
            action: "verify the copy of",
            path: to.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "the copy does not match the original — the original was left where it is",
            ),
        });
    }

    fs::remove_file(from).map_err(io_err("remove the moved original", from))
}

/// `fs::rename`, falling back to a verified copy across volumes. Returns
/// whether the fallback was used.
///
/// Missing parent directories are created first: a project's notes folder may
/// never have been written into, and a drop into it is as good a moment to
/// create it as a first save is.
fn move_file(from: &Path, to: &Path) -> Result<bool, DraftError> {
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(io_err("create", parent))?;
        }
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(false),
        Err(err) if is_cross_device(&err) => move_across_volumes(from, to).map(|()| true),
        Err(err) => Err(io_err("move", from)(err)),
    }
}

/// What one successful rename did, so the UI can say so.
///
/// Both paths are absolute and resolved: what changed on disk is the pair, and
/// a report carrying only the new name could not tell the user which file
/// moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameReport {
    /// Where the note was.
    pub from: PathBuf,
    /// Where it is now, and what the binding names from here on.
    pub to: PathBuf,
    /// Whether anything on disk actually moved. `false` when the sanitized name
    /// was the one the file already had — a no-op is a success, not a refusal,
    /// and saying so keeps the caller from reporting a move that never
    /// happened.
    pub moved: bool,
}

/// What to do when the target folder already holds a note of that name.
///
/// Not a `bool`: `move_to_project(project, roots, true)` says nothing about
/// what is being permitted, and what is being permitted here is "invent a
/// second name". **There is no overwrite variant, and there never will be** —
/// the whole point of asking is that a drop can never write over somebody
/// else's note (invariant 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClashChoice {
    /// Stop with [`NoteMove::Clash`] and let the window ask. The default, and
    /// what a drop sends the first time.
    #[default]
    Ask,
    /// Move it under the first free numbered name
    /// ([`savepath::first_free`]). Only ever
    /// reachable from the user's answer to a clash they were shown.
    KeepBoth,
}

/// Everything a cross-project move needs beyond the draft itself.
///
/// A struct for the reason [`SaveOptions`] is one: six positional arguments,
/// three of them `Option`-shaped, is a call site a reader has to count commas
/// in. The last two are only ever used by the recreate branch — a note whose
/// file is gone is *rendered* into the new folder, which is a save and needs a
/// save's inputs.
#[derive(Debug, Clone, Copy, Default)]
pub struct MoveOptions<'a> {
    /// The project the note joins, or `None` for the tree's "no project" group.
    pub project: Option<&'a str>,
    /// The notes folder the binding is currently relative to — the draft's own
    /// **old** project's. `None` when it had none, or when that project is gone
    /// from the config.
    pub old_root: Option<&'a Path>,
    /// The notes folder the note is moving into. `None` is "no project", and
    /// then nothing on disk moves at all: there is no folder to move into.
    pub new_root: Option<&'a Path>,
    /// What to do about a name already taken in `new_root`.
    pub clash: ClashChoice,
    /// The **new** project's header template, already expanded. Only read when
    /// the note has to be re-rendered.
    pub header: Option<&'a str>,
    /// The **new** project's divider setting, on the same terms.
    pub dividers: SessionDividers,
}

/// What a drop did to the note's file.
///
/// Five outcomes, and only two of them touched a file. The window says
/// different things about each, so they are different variants rather than a
/// path and a flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteMove {
    /// The note is already in that project. Nothing was written.
    Unchanged,
    /// Only `meta.json` changed: the draft has no file yet, or the destination
    /// is "no project" and there is no folder to move into. The tag moves and
    /// nothing else.
    Retagged,
    /// The `.md` was relocated into the new project's folder.
    Moved {
        /// Where it was.
        from: PathBuf,
        /// Where it is now, and what the binding names from here on.
        to: PathBuf,
        /// Whether the volume boundary forced the copy-verify-delete fallback
        /// rather than a plain rename. Worth logging: it is the only path in
        /// this codebase that deletes a note's file.
        copied: bool,
    },
    /// There was no file to move — it is gone from disk, or its binding was
    /// relative to a project that no longer exists — so the store rendered a
    /// fresh one into the new folder from the lines it still holds.
    /// Nothing was lost, because the draft *is* the note.
    Recreated {
        /// The file that was written, which is now the binding.
        to: PathBuf,
    },
    /// A note of that name is already in the target folder, so **nothing at all
    /// happened** and the window has a question to ask. Not an error: "there is
    /// already one called that" is an ordinary answer, and the drop is one
    /// atomic intent that has not started yet.
    Clash {
        /// The occupied path, so the question can name it.
        at: PathBuf,
        /// What "keep both" would use, or `None` if even the numbering ran out.
        free: Option<PathBuf>,
    },
}

/// What one successful save did, so the UI can say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReport {
    /// The file that was written, which is now the draft's `saved_path`.
    pub path: PathBuf,
    /// Bytes written.
    pub bytes: usize,
    /// Bullets rendered — live lines, not including soft-deleted ones.
    pub lines: usize,
}

// ---------------------------------------------------------------------------
// Draft
// ---------------------------------------------------------------------------

/// A writable handle on one draft directory.
///
/// Single-owner by construction: there is no interior locking, and two handles
/// on the same directory would interleave appends. Exactly one of these is
/// handed to the transcription worker.
#[derive(Debug)]
pub struct Draft {
    dir: PathBuf,
    meta: DraftMeta,
    /// The record produced by the most recent [`Draft::append_line`]. The
    /// caller owns the transcript list; this handle only needs to be able to
    /// hand back the record it just wrote.
    last: Option<LineRecord>,
    /// Set when the loader found a torn tail. The next append writes a single
    /// `\n` first so the orphan bytes become their own malformed line instead
    /// of swallowing the new record.
    needs_newline: bool,
    /// Live, non-deleted lines: what was on disk when the handle was opened,
    /// plus every append since. Kept on the handle so a *resumed* draft can go
    /// on numbering where it left off without re-reading the log.
    lines: usize,
}

/// A draft's lines plus which sitting each of them belongs to.
///
/// The ordinals travel *beside* the records rather than inside them, for the
/// same reason `original` is computed at fold time and never written: adding a
/// field to [`LineRecord`] would change a shape that is already on disk in every
/// draft the user has. This is derived state — the number of
/// [`SessionRecord`]s before a line's base record — and derived state does not
/// belong in the serialized record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    /// Every parseable line, folded, in rendered order.
    pub lines: Vec<LineRecord>,
    /// Session ordinal per line, parallel to [`Transcript::lines`]: 0 for
    /// everything before the first marker, 1 after it, and so on. A move
    /// permutes the lines and each line keeps the ordinal it was born with, so
    /// after a drag these need not be ascending.
    pub sessions: Vec<usize>,
}

/// A draft as read back off disk.
#[derive(Debug)]
pub struct LoadedDraft {
    /// Appendable handle, ready for more lines.
    pub draft: Draft,
    /// Every parseable record, in file order.
    pub lines: Vec<LineRecord>,
    /// Records that could not be parsed and were left exactly where they are:
    /// a torn tail from a crash, or a line something else mangled.
    pub skipped_lines: usize,
}

impl Draft {
    /// The draft directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Draft metadata as currently on disk.
    #[must_use]
    pub const fn meta(&self) -> &DraftMeta {
        &self.meta
    }

    /// The draft ulid.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.meta.id
    }

    /// How many live (non-deleted) lines this draft holds right now.
    ///
    /// Resuming a seven-line draft and speaking again has to produce line
    /// eight, not line one, so the count travels with the handle rather than
    /// being a counter the frontend keeps.
    #[must_use]
    pub const fn line_count(&self) -> usize {
        self.lines
    }

    /// Path of the append-only line log.
    #[must_use]
    pub fn lines_path(&self) -> PathBuf {
        self.dir.join(LINES_FILE)
    }

    /// Path of `meta.json`.
    #[must_use]
    pub fn meta_path(&self) -> PathBuf {
        self.dir.join(META_FILE)
    }

    /// Directory holding the per-line wavs.
    #[must_use]
    pub fn audio_dir(&self) -> PathBuf {
        self.dir.join(AUDIO_DIR)
    }

    /// Persist one utterance: audio, then record, then metadata.
    ///
    /// Blocking and fsync'ing — **worker thread only**, never a callback
    /// (invariant 5). Order is deliberate: the wav lands and is flushed before
    /// the record that references it exists, so a crash can orphan audio but
    /// can never produce a record pointing at a missing file.
    ///
    /// Empty `samples` is accepted and writes a valid zero-sample wav. The
    /// worker gates silence before it gets here; the store still must not panic
    /// on it.
    ///
    /// # Errors
    /// Returns [`DraftError`] if the audio, the record or the metadata cannot
    /// be written. On failure the previously written lines are untouched.
    pub fn append_line(&mut self, new: NewLine<'_>) -> Result<&LineRecord, DraftError> {
        let line_id = Ulid::new().to_string();
        let relative_audio = format!("{AUDIO_DIR}/{line_id}.wav");
        let audio_path = self.audio_dir().join(format!("{line_id}.wav"));

        write_wav(&audio_path, new.samples)?;

        let record = LineRecord {
            id: line_id,
            spoken_at: new.spoken_at,
            text: new.text,
            original: None,
            deleted: false,
            audio: relative_audio,
            transcribe_ms: new.transcribe_ms,
            failed: new.failed,
        };

        let lines_path = self.lines_path();
        let json = serde_json::to_vec(&record).map_err(|source| DraftError::Encode {
            path: lines_path.clone(),
            source,
        })?;

        self.append_record(&json)?;
        // A failed line has no words, so it is not in the note yet and must not
        // be counted as one — the same rule a soft-deleted line follows. A
        // successful Retry adds it to the count from `correct`. Spelled as the
        // fold spells it (`failed` *and* no words), so a re-read of this file
        // recounts to exactly the same number.
        if !(record.failed && record.text.is_empty()) {
            self.lines += 1;
        }
        self.mark_dirty()?;

        tracing::debug!(
            draft = %self.meta.id,
            line = %record.id,
            samples = new.samples.len(),
            "appended draft line"
        );

        self.last = Some(record);
        // `last` was just assigned, so this branch is unreachable; expressing it
        // as a match keeps the function free of `unwrap`.
        match &self.last {
            Some(record) => Ok(record),
            None => unreachable!("last was just set"),
        }
    }

    /// Append one already-encoded record to `lines.jsonl`, durably.
    ///
    /// The single write path for both record types. One buffer, one
    /// `write_all`: the torn-tail repair newline and the record must not be
    /// separated by a crash, and a single write keeps the common case to one
    /// syscall. Append mode, never truncate: on every platform this positions
    /// at the current end of file, so nothing already written can be
    /// overwritten (invariant 4).
    ///
    /// Blocking and fsync'ing — **worker thread only** (invariant 5).
    fn append_record(&mut self, json: &[u8]) -> Result<(), DraftError> {
        let lines_path = self.lines_path();

        let mut buf = Vec::with_capacity(json.len() + 2);
        if self.needs_newline {
            buf.push(b'\n');
        }
        buf.extend_from_slice(json);
        buf.push(b'\n');

        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(&lines_path)
            .map_err(io_err("open", &lines_path))?;
        file.write_all(&buf)
            .map_err(io_err("append to", &lines_path))?;
        // `sync_data`, not `sync_all`: the bytes are what must survive a power
        // cut. This is the per-line fsync the crash story rests on, and it is why this
        // function is worker-thread-only.
        file.sync_data().map_err(io_err("flush", &lines_path))?;
        drop(file);

        self.needs_newline = false;
        Ok(())
    }

    /// Note that there is something not yet in the saved markdown.
    ///
    /// Always called *after* the record is on disk, never before: a crash in
    /// between leaves a durable line and a stale `dirty: false`, which costs the
    /// user one click on Save. The other order would claim the work was saved.
    fn mark_dirty(&mut self) -> Result<(), DraftError> {
        if !self.meta.dirty {
            self.meta.dirty = true;
            self.write_meta()?;
        }
        Ok(())
    }

    /// Record that what follows was dictated in a later sitting.
    ///
    /// Appends one [`SessionRecord`]; nothing on disk is touched (invariant 4).
    /// The caller decides *when* a sitting has changed — the store cannot know,
    /// since it has no idea how many times the app has been started — and the
    /// rule is "immediately before the first new line appended to a draft
    /// this process found with lines already in it".
    ///
    /// Marks the draft dirty, because it changes what a save would render.
    ///
    /// Blocking, fsync'ing, **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::Io`] or [`DraftError::Encode`] if the log cannot be
    /// appended to.
    pub fn mark_session(&mut self) -> Result<(), DraftError> {
        let marker = SessionRecord {
            // `now()` is right here: this is when the *sitting* started. The
            // captured-at-release rule governs `spoken_at`, which no marker
            // touches.
            session_at: Local::now().fixed_offset(),
        };

        let lines_path = self.lines_path();
        let json = serde_json::to_vec(&marker).map_err(|source| DraftError::Encode {
            path: lines_path,
            source,
        })?;
        self.append_record(&json)?;
        self.mark_dirty()?;

        tracing::debug!(draft = %self.meta.id, "appended a session marker");
        Ok(())
    }

    /// Change a line's text, by appending a correction.
    ///
    /// Nothing already on disk is touched: the base record stays exactly as it
    /// was spoken and this adds an [`EditRecord`] after it, which the loader
    /// folds (invariant 4). Returns the line as it now reads.
    ///
    /// A text that already matches appends nothing — the file must not grow a
    /// record per blur of an untouched field.
    ///
    /// Blocking, fsync'ing, **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::NoSuchLine`] if this draft has no such line;
    /// [`DraftError::Io`] or [`DraftError::Encode`] if the log cannot be read or
    /// appended to.
    pub fn edit_line(&mut self, id: &str, text: &str) -> Result<LineRecord, DraftError> {
        self.correct(id, Some(text.to_owned()), None, None)
    }

    /// [`Draft::edit_line`] for a re-transcribe, which also has a duration
    /// worth recording next to the new text.
    ///
    /// # Errors
    /// As [`Draft::edit_line`].
    pub fn retranscribed_line(
        &mut self,
        id: &str,
        text: &str,
        transcribe_ms: Option<u64>,
    ) -> Result<LineRecord, DraftError> {
        self.correct(id, Some(text.to_owned()), None, transcribe_ms)
    }

    /// Soft-delete a line, or bring it back.
    ///
    /// Soft only, and that is the whole point: the record stays, the wav stays,
    /// and the line simply stops being rendered into the markdown (invariant
    /// 4). Nothing in this module can make a spoken line unrecoverable.
    ///
    /// # Errors
    /// As [`Draft::edit_line`].
    pub fn set_deleted(&mut self, id: &str, deleted: bool) -> Result<LineRecord, DraftError> {
        self.correct(id, None, Some(deleted), None)
    }

    /// The one append path for corrections.
    ///
    /// Re-reads and folds the log first, exactly as [`Draft::save_as`] does and
    /// for the same reason: the handle never carried the texts, and the folded
    /// state on disk is the only thing that can say whether this edit is a
    /// change at all.
    fn correct(
        &mut self,
        id: &str,
        text: Option<String>,
        deleted: Option<bool>,
        transcribe_ms: Option<u64>,
    ) -> Result<LineRecord, DraftError> {
        let parsed = self.parse_log()?;
        let at = parsed
            .lines
            .iter()
            .position(|line| line.id == id)
            .ok_or_else(|| DraftError::NoSuchLine {
                id: id.to_owned(),
                path: self.dir.clone(),
            })?;
        // The **base** flag, not the folded one: a line that has already been
        // given words folds to `failed: false` while its base record still says
        // it was refused, and the rule below needs the record's birth, not its
        // current reading.
        let failed_at_birth = parsed.base_failed[at];
        let mut record = parsed.lines[at].clone();

        // A field that already reads that way is not an edit. `transcribe_ms`
        // deliberately does not count: a re-transcribe that produced the same
        // words is not worth a record.
        let text = text.filter(|new| *new != record.text);
        let deleted = deleted.filter(|new| *new != record.deleted);
        if text.is_none() && deleted.is_none() {
            return Ok(record);
        }

        let edit = EditRecord {
            edit_of: id.to_owned(),
            // `now()` is right here: this is when the *edit* happened. The
            // captured-at-release rule governs `spoken_at`, which no edit
            // touches.
            at: Local::now().fixed_offset(),
            text,
            deleted,
            transcribe_ms,
        };

        let lines_path = self.lines_path();
        let json = serde_json::to_vec(&edit).map_err(|source| DraftError::Encode {
            path: lines_path,
            source,
        })?;
        self.append_record(&json)?;

        // Whether this line was in the note *before* the edit, by the same
        // definition the renderer uses: it has words and it is not deleted.
        let was_live = !record.deleted && !record.failed;

        // What a re-read would now report: the same fold, on one record. The
        // base record's text is `original` when it is still on the handle, and
        // otherwise the text this line currently reads.
        //
        // A line whose base record failed has no `original` to keep — the model
        // produced nothing, and reporting "" as what was first said would be a
        // lie in the audit trail. `parse_lines` suppresses it for the same
        // reason.
        let base = record
            .original
            .clone()
            .unwrap_or_else(|| record.text.clone());
        apply_edit(&mut record, edit);
        record.original = (!failed_at_birth && record.text != base).then_some(base);
        // The failed rule, folded exactly as `parse_lines` folds it — including
        // in reverse, so an edit that empties a resolved line puts it back.
        record.failed = failed_at_birth && record.text.is_empty();

        // The handle's live count follows the log, so a resumed draft that had
        // a line deleted — or a failed one resolved, or a resolve undone —
        // still numbers the next line correctly. One computation for all three,
        // rather than a rule per field that could disagree with the renderer.
        let now_live = !record.deleted && !record.failed;
        if was_live && !now_live {
            self.lines = self.lines.saturating_sub(1);
        } else if !was_live && now_live {
            self.lines += 1;
        }
        self.mark_dirty()?;

        tracing::debug!(
            draft = %self.meta.id,
            line = %id,
            deleted = record.deleted,
            failed = record.failed,
            "appended a draft line edit"
        );

        Ok(record)
    }

    /// Move a line so it sits immediately after `after`, or to the top when
    /// `after` is `None`.
    ///
    /// Nothing already on disk is touched: this appends one [`MoveRecord`],
    /// which the loader folds (invariant 4). The base records keep their file
    /// order forever, so the order the user *spoke* in is always recoverable —
    /// and a move that cannot be folded later (a hand-mangled anchor, say) only
    /// leaves the line where it already was.
    ///
    /// Returns the transcript in its new folded order, so the caller needs no
    /// second read.
    ///
    /// A move that would land the line exactly where it already is appends
    /// nothing — the same rule no-op edits get. `after == Some(id)` is one of
    /// those: a line cannot follow itself.
    ///
    /// Blocking, fsync'ing, **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::NoSuchLine`], naming whichever of the two ids is missing;
    /// [`DraftError::Io`] or [`DraftError::Encode`] if the log cannot be read or
    /// appended to.
    pub fn move_line(
        &mut self,
        id: &str,
        after: Option<&str>,
    ) -> Result<Vec<LineRecord>, DraftError> {
        let mut lines = self.read_lines()?;
        let missing = |id: &str| DraftError::NoSuchLine {
            id: id.to_owned(),
            path: self.dir.clone(),
        };

        let from = lines
            .iter()
            .position(|line| line.id == id)
            .ok_or_else(|| missing(id))?;
        let anchor = match after {
            None => None,
            Some(anchor) => Some(
                lines
                    .iter()
                    .position(|line| line.id == anchor)
                    .ok_or_else(|| missing(anchor))?,
            ),
        };

        // Nothing to record: the line already sits there. `Some(from)` is the
        // "after itself" case, which is the same non-event.
        if is_settled(from, anchor) {
            return Ok(lines);
        }

        let moved = MoveRecord {
            move_of: id.to_owned(),
            at: Local::now().fixed_offset(),
            after: after.map(str::to_owned),
        };

        let lines_path = self.lines_path();
        let json = serde_json::to_vec(&moved).map_err(|source| DraftError::Encode {
            path: lines_path,
            source,
        })?;
        self.append_record(&json)?;
        self.mark_dirty()?;

        tracing::debug!(
            draft = %self.meta.id,
            line = %id,
            after = ?after,
            "appended a draft line move"
        );

        // The live count is untouched: a move changes where a line reads, never
        // whether it is in the note.
        reposition(&mut lines, from, anchor);
        Ok(lines)
    }

    /// Lift one line out of this draft, ready for another draft to take it
    /// ("Move to note…").
    ///
    /// **Reads only.** Nothing on disk changes here, and that is the half of
    /// the ordering rule this side owns: the caller imports into the
    /// destination first and soft-deletes here last, so a crash between the two
    /// leaves the line in **both** notes and never in neither (invariant 4).
    ///
    /// The wav is resolved from the line **id** rather than from the record's
    /// `audio` field, exactly as playback resolves it: the field is a rendering
    /// of the id, and a hand-edited one must not be able to make a copy read
    /// some other file. A wav that is not there is carried as
    /// [`MovedLine::has_audio`] `== false` and warn-logged rather than
    /// refused — losing the line because its audio is already lost would
    /// discard the words as well.
    ///
    /// Blocking I/O: **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::NoSuchLine`] if this draft has no such line,
    /// [`DraftError::BadId`] if the id is not a plain component, and
    /// [`DraftError::Io`] if the log or the wav cannot be read.
    pub fn export_line(&self, id: &str) -> Result<MovedLine, DraftError> {
        let parsed = self.parse_log()?;
        let at = parsed
            .lines
            .iter()
            .position(|line| line.id == id)
            .ok_or_else(|| DraftError::NoSuchLine {
                id: id.to_owned(),
                path: self.dir.clone(),
            })?;
        let line = &parsed.lines[at];

        let audio_path = self.line_audio_path(id)?;
        let audio = match fs::read(&audio_path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::warn!(
                    draft = %self.meta.id,
                    line = %id,
                    path = %audio_path.display(),
                    "this line's audio is missing; moving the line without it"
                );
                None
            }
            Err(err) => return Err(io_err("read", &audio_path)(err)),
        };

        Ok(MovedLine {
            source: self.meta.id.clone(),
            line: id.to_owned(),
            // The base record's own words, not the folded ones: `original` is
            // derived at fold time and never written, so the base/edit split is
            // the only thing that can carry an edit history across.
            base_text: parsed.base_texts[at].clone(),
            text: line.text.clone(),
            spoken_at: line.spoken_at,
            transcribe_ms: line.transcribe_ms,
            failed_at_birth: parsed.base_failed[at],
            audio,
        })
    }

    /// Put a divider above what is about to arrive, if the note needs one.
    ///
    /// Called only for the **first** line of a batch — the caller says when a
    /// batch begins, because only the window knows where one ends — and what
    /// this adds on top is the guard against a divider that would separate
    /// nothing: **a marker iff the log is non-empty and its last complete
    /// record is not already a marker.** So an empty note gets none, and a
    /// tail that already ends in one is left alone, which is the whole of "a
    /// divider is never doubled".
    ///
    /// A torn tail is not a record and neither is a malformed segment: the last
    /// *complete* record decides, exactly as the loader's fold does.
    ///
    /// Append-only, like everything else here (invariant 4). If this succeeds
    /// and the import then fails, the dangling marker is acceptable and
    /// self-healing — the next successful import sees a marker tail and adds
    /// nothing, and a marker with no lines after it renders as nothing at all
    /// (`render_markdown_with` only ever puts a divider *between* two rendered
    /// lines).
    ///
    /// Blocking, fsync'ing, **worker thread only** (invariant 5).
    fn divide_arrivals(&mut self) -> Result<(), DraftError> {
        if self.parse_log()?.tail == LogTail::Record {
            self.mark_session()?;
        }
        Ok(())
    }

    /// Append a line another draft lifted out.
    ///
    /// The far half of [`Draft::export_line`], and the only writer in this
    /// module that produces **two** records for one act: a base [`LineRecord`]
    /// carrying the source's original words, its stamp and its birth failure,
    /// and — only when the folded text differs from it — one [`EditRecord`]
    /// replaying the tidy-up. That is what makes the arrival fold to exactly
    /// what the source read, `original` included, without either record shape
    /// gaining a field (`parse_lines`' four-shape rule is untouched, so an
    /// older build reads a file a newer one wrote).
    ///
    /// The id is **fresh**. Ids are never reused across drafts: a line sitting
    /// in two of them under one id would make `line_audio(draft_id, line_id)`
    /// ambiguous, and the `audio` field must name the id it really has rather
    /// than the one it used to.
    ///
    /// Order is [`Draft::append_line`]'s: the wav lands and is flushed before
    /// the record that references it, so a crash can orphan audio but can never
    /// produce a record pointing at a file that is not there — and ahead of
    /// both, the divider rule below.
    ///
    /// # Arrivals land under a divider of their own
    ///
    /// Stitching lines into a note is a sitting like any other, so the whole
    /// batch is separated from what was already there exactly as a new sitting
    /// is: **one** marker, above the first arrival.
    ///
    /// `divide` is that "first arrival", and it is an argument rather than
    /// something inferred here because **the log cannot tell one**: after the
    /// first line has landed, the tail is a `LineRecord` whether it arrived
    /// two milliseconds ago as part of this move or an hour ago as part of
    /// another, and the records carry no batch (the "no new record shape" rule
    /// stands — this flag is IPC, it is never written). Only the window knows
    /// where a batch begins, so it says so: `true` on the first
    /// `line_move_to` of a confirm, `false` on the rest. With `divide` false
    /// nothing is ever written here; with it true,
    /// [`Draft::divide_arrivals`] still refuses to write a divider that would
    /// separate nothing (an empty note, or a tail that is already a marker).
    ///
    /// The marker is always *written*; the per-project `session_dividers`
    /// setting still decides whether it is *rendered*, exactly as it does for
    /// dictated sittings.
    ///
    /// Blocking, fsync'ing, **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::SameDraft`] if this is the draft the line came out of,
    /// [`DraftError::Io`] / [`DraftError::Encode`] / [`DraftError::Wav`] if the
    /// audio, the records or the metadata cannot be written. Nothing already on
    /// disk is touched on any path.
    pub fn import_line(
        &mut self,
        moved: &MovedLine,
        divide: bool,
    ) -> Result<LineRecord, DraftError> {
        // The one-handle rule, enforced where it cannot be forgotten: a draft
        // importing its own export would be this handle and the one that
        // produced the carrier, both live, both appending.
        if self.meta.id == moved.source {
            return Err(DraftError::SameDraft {
                path: self.dir.clone(),
            });
        }

        // Before anything of this line reaches disk: a refusal above must
        // leave the destination untouched, and the marker belongs *above* the
        // arrival it introduces.
        if divide {
            self.divide_arrivals()?;
        }

        let line_id = Ulid::new().to_string();
        let relative_audio = format!("{AUDIO_DIR}/{line_id}.wav");

        if let Some(bytes) = &moved.audio {
            // A byte copy, never a decode and re-encode: the stored wav is the
            // recording, and re-quantising it on the way past would make a
            // move quietly lossy. `create_new` inside — the id is fresh, so an
            // occupied name means something is very wrong.
            let audio_path = self.audio_dir().join(format!("{line_id}.wav"));
            write_new_file(&audio_path, bytes)?;
        }

        let record = LineRecord {
            id: line_id.clone(),
            spoken_at: moved.spoken_at,
            text: moved.base_text.clone(),
            // Never written to disk on any path: `original` is derived at fold
            // time from the base record, which is what the edit below restores.
            original: None,
            deleted: false,
            audio: relative_audio,
            transcribe_ms: moved.transcribe_ms,
            failed: moved.failed_at_birth,
        };

        let lines_path = self.lines_path();
        let json = serde_json::to_vec(&record).map_err(|source| DraftError::Encode {
            path: lines_path.clone(),
            source,
        })?;
        self.append_record(&json)?;

        let mut folded = record;
        if moved.text != moved.base_text {
            let edit = EditRecord {
                edit_of: line_id,
                // `now()` is right here for the reason every other edit uses
                // it: this is when the *edit* landed in this note. The
                // captured-at-release rule governs `spoken_at`, which the move
                // carried across untouched.
                at: Local::now().fixed_offset(),
                text: Some(moved.text.clone()),
                deleted: None,
                transcribe_ms: None,
            };
            let json = serde_json::to_vec(&edit).map_err(|source| DraftError::Encode {
                path: lines_path,
                source,
            })?;
            self.append_record(&json)?;
            apply_edit(&mut folded, edit);
        }

        // The fold, spelled exactly as `parse_lines` spells it, so a re-read of
        // this file reports the same line and the live count below cannot
        // disagree with the loader's.
        folded.original = (!moved.failed_at_birth && folded.text != moved.base_text)
            .then(|| moved.base_text.clone());
        folded.failed = moved.failed_at_birth && folded.text.is_empty();

        if !folded.deleted && !folded.failed {
            self.lines += 1;
        }
        self.mark_dirty()?;

        tracing::info!(
            draft = %self.meta.id,
            line = %folded.id,
            from_draft = %moved.source,
            from_line = %moved.line,
            audio = moved.has_audio(),
            "took a line moved in from another note"
        );

        Ok(folded)
    }

    /// Where one line's audio lives, with the id validated first.
    ///
    /// Line ids reach this crate from the frontend over IPC just as draft ids
    /// do, so the same boundary check applies: a single plain path component,
    /// no separators, no leading dot, and no dot at all — the extension is ours
    /// to add, and `foo.wav.wav` or `..` joined onto the audio directory is how
    /// a "play this line" turns into reading an arbitrary file.
    ///
    /// # Errors
    /// [`DraftError::BadId`] if the id is not a plain component.
    pub fn line_audio_path(&self, id: &str) -> Result<PathBuf, DraftError> {
        line_audio_path_in(&self.dir, id)
    }

    /// The markdown file this draft is bound to, if it has ever been saved.
    #[must_use]
    pub fn saved_path(&self) -> Option<&Path> {
        self.meta.saved_path.as_deref()
    }

    /// Whether there are lines not yet written to [`Draft::saved_path`]. Drives
    /// the unsaved indicator.
    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.meta.dirty
    }

    /// Render this draft to markdown and write it to `path`, guarded against an
    /// external edit.
    ///
    /// Binding to a path other than the current [`Draft::saved_path`] is
    /// allowed — it is how the first save works, and how "save to a different
    /// file" will work — and the guard then applies to the *new* path. After a
    /// successful save the draft is bound to `path` forever, in the Notepad
    /// sense: every later [`Draft::save`] rewrites this same file.
    ///
    /// Blocking I/O, like [`Draft::append_line`]: **worker thread only**, never
    /// the `rdev` hook callback or the `cpal` data callback (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::SaveConflict`] if the file at `path` exists and is not
    /// byte-identical to what we last wrote there — in which case nothing at
    /// all is written. [`DraftError::Io`] if the lines, the target or the
    /// metadata cannot be read or written.
    pub fn save_as(&mut self, path: &Path, header: Option<&str>) -> Result<SaveReport, DraftError> {
        self.save_as_with(path, header, SaveMode::Guarded, SessionDividers::default())
    }

    /// Exactly the bytes a save would write, without writing them.
    ///
    /// The conflict dialog needs both sides of the comparison, and this is the
    /// half that is not on disk. Sound because [`render::render_markdown_with`]
    /// is pure: rendering the same log twice produces the same bytes, which is
    /// the property the hash guard already rests on.
    ///
    /// # Errors
    /// [`DraftError::Io`] if the log exists but cannot be read.
    pub fn preview_markdown(
        &self,
        header: Option<&str>,
        dividers: SessionDividers,
    ) -> Result<String, DraftError> {
        Ok(self.render(header, dividers)?.0)
    }

    /// The markdown and the number of bullets in it.
    ///
    /// Reads the lines back off disk rather than trusting anything on this
    /// handle: the handle never carried the texts, and edits land in
    /// `lines.jsonl` behind our back. Whatever the log says now is what gets
    /// rendered.
    fn render(
        &self,
        header: Option<&str>,
        dividers: SessionDividers,
    ) -> Result<(String, usize), DraftError> {
        let transcript = self.read_transcript()?;
        let sessions = match dividers {
            SessionDividers::Shown => Some(transcript.sessions.as_slice()),
            SessionDividers::Hidden => None,
        };
        let markdown = render::render_markdown_with(header, &transcript.lines, sessions);
        let rendered = transcript.lines.iter().filter(|line| !line.deleted).count();
        Ok((markdown, rendered))
    }

    /// [`Draft::save_as`] with an explicit [`SaveMode`] and divider setting.
    ///
    /// [`SaveMode::Overwrite`] is the resolution of a conflict the user has
    /// already been shown; nothing should reach for it by default.
    ///
    /// # Errors
    /// As [`Draft::save_as`]; `Overwrite` cannot return
    /// [`DraftError::SaveConflict`].
    pub fn save_as_with(
        &mut self,
        path: &Path,
        header: Option<&str>,
        mode: SaveMode,
        dividers: SessionDividers,
    ) -> Result<SaveReport, DraftError> {
        self.save_to(
            path,
            SaveOptions {
                header,
                mode,
                dividers,
                // No project, so the plain behaviour: absolute in, absolute
                // out, and nothing adopts this draft.
                ..SaveOptions::default()
            },
        )
    }

    /// Where a save would write, given this draft's binding and its project's
    /// notes folder.
    ///
    /// The bound path when there is one, resolved through
    /// [`resolve_binding`]; `first_save` when there is not. This is the one
    /// place "which file" is decided, so the folder the caller creates, the file
    /// the guard hashes and the path a conflict names are always the same one.
    ///
    /// # Errors
    /// [`DraftError::UnresolvedPath`] when the binding is relative and no notes
    /// folder was supplied — the project is gone from the config, and guessing
    /// would write the note somewhere nobody chose.
    pub fn save_target(
        &self,
        first_save: &Path,
        notes_root: Option<&Path>,
    ) -> Result<PathBuf, DraftError> {
        let Some(stored) = self.meta.saved_path.as_deref() else {
            return Ok(first_save.to_path_buf());
        };
        resolve_binding(stored, notes_root).ok_or_else(|| DraftError::UnresolvedPath {
            path: self.dir.clone(),
            relative: stored.to_path_buf(),
        })
    }

    /// [`Draft::saved_path`] resolved against a project's notes folder.
    ///
    /// `None` for a draft that has never been saved **or** one whose relative
    /// binding cannot be resolved — the display counterpart of
    /// [`Draft::save_target`], where a missing project is a blank rather than an
    /// error.
    #[must_use]
    pub fn resolved_saved_path(&self, notes_root: Option<&Path>) -> Option<PathBuf> {
        self.meta
            .saved_path
            .as_deref()
            .and_then(|stored| resolve_binding(stored, notes_root))
    }

    /// The whole of a save: render, guard, write, bind, adopt.
    ///
    /// `path` is absolute and already resolved (see [`Draft::save_target`]);
    /// what [`SaveOptions::notes_root`] decides here is only how the binding is
    /// *stored*.
    ///
    /// Blocking I/O, like [`Draft::append_line`]: **worker thread only**, never
    /// the `rdev` hook callback or the `cpal` data callback (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::SaveConflict`] if the file at `path` exists and is not
    /// byte-identical to what we last wrote there — in which case nothing at all
    /// is written. [`DraftError::Io`] if the lines, the target or the metadata
    /// cannot be read or written.
    pub fn save_to(
        &mut self,
        path: &Path,
        opts: SaveOptions<'_>,
    ) -> Result<SaveReport, DraftError> {
        let (markdown, rendered_lines) = self.render(opts.header, opts.dividers)?;

        if opts.mode == SaveMode::Guarded {
            self.check_unchanged(path)?;
        }

        fsutil::write_atomic(path, markdown.as_bytes()).map_err(io_err("write", path))?;

        // Markdown first, then meta. A crash in between leaves `dirty: true`
        // and a stale hash, so the next save reports a conflict and the user
        // overwrites their own file — a false positive, and the only safe
        // direction. Writing meta first would mean a crash left us believing we
        // had saved bytes that are not there, and the *next* save would then
        // silently clobber a real external edit (invariant 4).
        //
        // Everything below is one meta write. Adoption in particular is *not* a
        // second step that a crash could skip: a draft whose markdown landed and
        // whose project did not would be a note in a folder it does not admit to
        // belonging to.
        self.meta.saved_path = Some(binding_for(path, opts.notes_root));
        self.meta.last_save_hash = Some(hash_hex(markdown.as_bytes()));
        self.meta.dirty = false;
        // Only ever fills a hole. A draft that already names a project keeps it:
        // saving a note is not a request to move it between projects.
        if self.meta.project.is_none() {
            self.meta.project = opts.adopt_project.map(str::to_owned);
        }
        self.write_meta()?;

        tracing::info!(
            draft = %self.meta.id,
            path = %path.display(),
            bound = %self.meta.saved_path.as_deref().unwrap_or(path).display(),
            project = ?self.meta.project,
            bytes = markdown.len(),
            lines = rendered_lines,
            "saved draft to markdown"
        );

        Ok(SaveReport {
            path: path.to_path_buf(),
            bytes: markdown.len(),
            lines: rendered_lines,
        })
    }

    /// Re-save to the file this draft is already bound to.
    ///
    /// # Errors
    /// [`DraftError::NeverSaved`] if there is no `saved_path` yet; otherwise as
    /// [`Draft::save_as`].
    pub fn save(&mut self, header: Option<&str>) -> Result<SaveReport, DraftError> {
        self.save_with(header, SaveMode::Guarded, SessionDividers::default())
    }

    /// [`Draft::save`] with an explicit [`SaveMode`] and divider setting.
    ///
    /// # Errors
    /// As [`Draft::save`].
    pub fn save_with(
        &mut self,
        header: Option<&str>,
        mode: SaveMode,
        dividers: SessionDividers,
    ) -> Result<SaveReport, DraftError> {
        if self.meta.saved_path.is_none() {
            return Err(DraftError::NeverSaved {
                path: self.dir.clone(),
            });
        }
        // No notes folder, so a relative binding cannot be resolved and says so
        // rather than being joined onto the working directory.
        let path = self.save_target(Path::new(""), None)?;
        self.save_as_with(&path, header, mode, dividers)
    }

    /// Rename the note this draft is bound to, and follow it.
    ///
    /// **A note's name is its file's name.** There is no name field anywhere in
    /// this store — every label the app shows is the basename of `saved_path` —
    /// so renaming a note is renaming its `.md` file and re-deriving the
    /// binding, and nothing else has to be kept in step.
    ///
    /// The one place the rename happens, called by both routes the shell has
    /// (the worker's live handle for the active draft, an open→rename→drop for
    /// any other), so the two cannot drift about what a rename is.
    ///
    /// # Order of operations, and the crash window (invariant 4)
    ///
    /// Resolve → compute the target in the *same* directory → refuse an
    /// occupied target → `fs::rename` → re-derive the binding → write meta.
    ///
    /// * The target is always a sibling: this renames, it never moves a note
    ///   between folders, so a name containing separators is sanitized into one
    ///   component rather than being honoured as a path.
    /// * An existing target is refused, never overwritten and never
    ///   auto-numbered — `fs::rename` replaces a destination on every platform
    ///   we ship to, which makes the check the whole of the guarantee. The
    ///   check-then-rename race is documented and accepted, exactly as the
    ///   model store accepts it: one user, one app instance. The single
    ///   exception is [`is_case_only_rename`]: a target that
    ///   exists because Windows found *this file* under a different
    ///   capitalization is not an occupied target, and refusing it would make
    ///   "Ludo.md" → "ludo.md" impossible.
    /// * **A crash between the rename and the meta write is non-destructive.**
    ///   The file is on disk under the new name with all of its content, and
    ///   `meta.json` still names the old path. The next save finds the old path
    ///   missing — which the external-edit guard treats as fine, because "the
    ///   user moved the note" and "re-saving recreates it" is what one file
    ///   forever means — and re-renders there. Two complete files, neither of
    ///   them damaged, and re-renaming resolves it. The other order (meta
    ///   first) would leave a binding pointing at a file that does not exist
    ///   while the real note sits under the old name, unreferenced.
    /// * `last_save_hash` and `dirty` are deliberately **kept**: a rename moves
    ///   bytes and never changes them, so the guard goes on working at the new
    ///   path, and a note that was behind its file is still behind it.
    ///
    /// A missing source file is an [`DraftError::Io`], not a quiet success: the
    /// whole job here is to move bytes, and reporting a move that did not
    /// happen would be a lie about disk. (That is the one place this differs
    /// from [`Draft::check_unchanged`], where a missing file genuinely is fine.)
    ///
    /// Blocking I/O: **worker or control thread only**, never a callback
    /// (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::NeverSaved`] if there is no binding to rename,
    /// [`DraftError::UnresolvedPath`] if the binding is relative and its
    /// project's folder was not supplied, [`DraftError::NameTaken`] if the
    /// target exists, [`DraftError::Io`] if the rename or the meta write fails.
    pub fn rename_note(
        &mut self,
        name: &str,
        notes_root: Option<&Path>,
    ) -> Result<RenameReport, DraftError> {
        let Some(stored) = self.meta.saved_path.clone() else {
            return Err(DraftError::NeverSaved {
                path: self.dir.clone(),
            });
        };
        let from =
            resolve_binding(&stored, notes_root).ok_or_else(|| DraftError::UnresolvedPath {
                path: self.dir.clone(),
                relative: stored.clone(),
            })?;

        // The project's own filename sanitizer, not a second one: separators,
        // control characters and Windows' trailing-dot rule all have to be
        // decided in exactly one place, or the name on disk and the name the
        // guard remembers can differ.
        let file = template::file_safe(name);
        let to = match from.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(&file),
            // A bare filename with no directory part. Only reachable from a
            // hand-written binding; joining it onto nothing keeps it a sibling.
            _ => PathBuf::from(&file),
        };

        if to == from {
            return Ok(RenameReport {
                from,
                to,
                moved: false,
            });
        }
        // The one carve-out: a "taken" target that is provably this
        // very file under different capitalization is a case-only rename, which
        // NTFS does in place and which overwrites nothing.
        if to.exists() && !is_case_only_rename(&from, &to) {
            return Err(DraftError::NameTaken { path: to });
        }

        fs::rename(&from, &to).map_err(io_err("rename", &from))?;

        // Re-derived rather than patched: a target inside the notes folder is
        // stored relative and one outside it stays absolute, which is the same
        // decision a save makes, made by the same function.
        self.meta.saved_path = Some(binding_for(&to, notes_root));
        self.write_meta()?;

        tracing::info!(
            draft = %self.meta.id,
            from = %from.display(),
            to = %to.display(),
            bound = %self.meta.saved_path.as_deref().unwrap_or(&to).display(),
            "renamed a note's file"
        );

        Ok(RenameReport {
            from,
            to,
            moved: true,
        })
    }

    /// The project this draft belongs to, if any.
    #[must_use]
    pub fn project(&self) -> Option<&str> {
        self.meta.project.as_deref()
    }

    /// Re-file this draft under a different project name.
    ///
    /// One whole-file atomic meta write and **nothing else**: a draft's project
    /// is a name in `meta.json`, and neither the draft directory nor the user's
    /// notes folder is touched. Used by the project-rename sweep, which has to
    /// carry every referencing draft's `meta.project` to the new name or watch
    /// them all fall into the "not in your projects" group.
    ///
    /// Returns whether anything was written, so a sweep can count what it did.
    ///
    /// Blocking I/O: **worker thread only** for the active draft (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::Io`] or [`DraftError::Encode`] if `meta.json` cannot be
    /// written; the in-memory value is then already the new one, which is
    /// harmless — this handle's next write rewrites the whole file anyway.
    pub fn set_project(&mut self, project: Option<&str>) -> Result<bool, DraftError> {
        if self.meta.project.as_deref() == project {
            return Ok(false);
        }
        self.meta.project = project.map(str::to_owned);
        self.write_meta()?;
        Ok(true)
    }

    /// Re-file this note under another project **without touching a file**.
    ///
    /// The tag-and-binding half of a drop, and no longer the
    /// whole of one: [`Draft::move_to_project`] is the drop, and it calls this
    /// for the two cases where there is nothing on disk to move — an unbound
    /// draft, and a destination with no notes folder ("no project").
    ///
    /// [`Draft::set_project`] stays the *sweep's* tool: a project rename moves
    /// a name while every folder stays put, so `meta.project` alone is the
    /// whole of that job. Here the binding has to move with the name, because a
    /// binding is stored relative to its project's notes folder when it lies
    /// inside one — changing the project under a relative binding would leave
    /// `"session 1.md"` resolving against the *new* project's folder, a path
    /// this file has never been at, and the path the next save would render the
    /// note into. That is the quiet relocation invariant 4 forbids, which is
    /// why the two fields move together.
    ///
    /// The re-derivation, in order:
    ///
    /// 1. **Resolve** the stored binding against the **old** project's notes
    ///    folder ([`resolve_binding`]) — the absolute path the file is really
    ///    at right now.
    /// 2. **Re-derive** the stored form of *that same path* against the **new**
    ///    project's folder ([`binding_for`]) — the same decision a save makes,
    ///    made by the same function.
    /// 3. Write `meta.project` and `saved_path` in **one** whole-file atomic
    ///    meta write, so there is no instant in which the project has moved and
    ///    the binding has not.
    ///
    /// Two carve-outs, both about not refusing a repair:
    ///
    /// * An **unbound** draft has no binding to re-derive — only the project
    ///   name is written.
    /// * A binding that **cannot be resolved** (relative, with the old project
    ///   already gone from the config) is carried across exactly as it is
    ///   rather than refused. Such a draft is sitting in the "not in your
    ///   projects" group and dragging it out is precisely the repair; refusing
    ///   it would strand it there forever.
    ///
    /// Returns whether anything was written.
    ///
    /// Blocking I/O: **worker or control thread only**, never a callback
    /// (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::Io`] or [`DraftError::Encode`] if `meta.json` cannot be
    /// written; the in-memory value is then already the new one, which is
    /// harmless — this handle's next write rewrites the whole file anyway.
    pub fn reassign_project(
        &mut self,
        project: Option<&str>,
        old_root: Option<&Path>,
        new_root: Option<&Path>,
    ) -> Result<bool, DraftError> {
        if self.meta.project.as_deref() == project {
            return Ok(false);
        }

        let rebound = self.meta.saved_path.as_deref().map(|stored| {
            resolve_binding(stored, old_root)
                .map_or_else(|| stored.to_path_buf(), |at| binding_for(&at, new_root))
        });

        self.meta.project = project.map(str::to_owned);
        if let Some(binding) = rebound {
            self.meta.saved_path = Some(binding);
        }
        self.write_meta()?;

        let bound = self
            .meta
            .saved_path
            .as_deref()
            .map_or_else(|| "(unbound)".to_owned(), |path| path.display().to_string());
        tracing::info!(
            draft = %self.meta.id,
            project = project.unwrap_or("(none)"),
            bound = %bound,
            "re-filed a note under another project"
        );
        Ok(true)
    }

    /// Move this note into another project — the file with it.
    ///
    /// The drop. Files move when you move them: dragging a note into another
    /// project moves its `.md` into that project's notes folder,
    /// immediately, because that is what the gesture means to anyone who has
    /// ever dragged a file between folders. It replaces an earlier rule where
    /// the file stayed exactly where it was, which surprised users in the
    /// wild.
    ///
    /// # The order, and why nothing can be half-applied (invariant 4)
    ///
    /// 1. **Decide everything first.** The source path, the target path and the
    ///    clash question are all resolved before a single byte is written. A
    ///    clash returns [`NoteMove::Clash`] with **nothing mutated** — the drop
    ///    is one atomic intent, and a state waiting on a dialog is exactly the
    ///    half-applied thing this ordering forbids. The answer comes back as a
    ///    second call carrying [`ClashChoice::KeepBoth`].
    /// 2. **The file, then the metadata.** A crash in between leaves the note
    ///    on disk in the new folder with all of its content while `meta.json`
    ///    still names the old project and the old binding — so the note reads as
    ///    still belonging to the old project, its file looks missing there, and
    ///    the next save re-renders it. Two complete files, neither damaged, and
    ///    dropping it again resolves it. The other order would leave a binding
    ///    pointing into a folder nothing is in.
    /// 3. **One meta write** for `project` and `saved_path` together
    ///    ([`Draft::reassign_project`]'s rule, extended rather than duplicated):
    ///    there is no instant in which the note has changed project and its
    ///    binding has not.
    ///
    /// # Four shapes of drop
    ///
    /// * **Bound, and the file is there** — `fs::rename`, or a verified
    ///   copy-then-delete across a volume boundary ([`move_across_volumes`]).
    ///   The binding comes out relative to the new project's folder by
    ///   construction, since that is where the file now is.
    /// * **Bound, and the file is not there** — deleted, or bound relative to a
    ///   project that has left the config so the path cannot even be resolved.
    ///   The store still holds every line, so the note is *rendered* into the
    ///   new folder: the ordinary guarded save path, atomic write,
    ///   hash recorded, binding relative. Nothing stays missing after a drop.
    /// * **Unbound** — there is no file yet, so there is nothing to move: the
    ///   tag moves and nothing else.
    /// * **No destination folder** — "no project", or a project with a blank
    ///   `notes_dir`. Same answer: there is nowhere to move *to*, so the file
    ///   stays and only the tag and the binding change.
    ///
    /// # The external-edit guard is untouched, and deliberately
    ///
    /// `last_save_hash` and `dirty` survive a move unchanged. This is where a
    /// reader would expect the hash to be rewritten, and it must not be: a
    /// rename moves bytes without changing them, and the copy fallback verifies
    /// that the bytes it wrote are the bytes it read, so the hash that was
    /// valid at the old path is still valid at the new one. A note that was
    /// behind its file is still behind it — dirtiness travels, pointed at the
    /// new path. Recreating is the one branch that *does* rewrite them, because
    /// it genuinely wrote a new file, and it does so through
    /// [`Draft::save_to`] like every other save.
    ///
    /// Blocking I/O: **worker or control thread only**, never a callback
    /// (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::Io`] if the move, the render or the meta write fails —
    /// with the original still on disk in every case;
    /// [`DraftError::NameTaken`] if even the numbering had nowhere free to go;
    /// [`DraftError::SaveConflict`] if a recreate found something unexpected at
    /// the target.
    pub fn move_to_project(&mut self, opts: MoveOptions<'_>) -> Result<NoteMove, DraftError> {
        if self.meta.project.as_deref() == opts.project {
            return Ok(NoteMove::Unchanged);
        }

        let retag = |draft: &mut Self| -> Result<NoteMove, DraftError> {
            draft.reassign_project(opts.project, opts.old_root, opts.new_root)?;
            Ok(NoteMove::Retagged)
        };

        // Nothing bound, or nowhere to move it to: retag and nothing more.
        let (Some(stored), Some(new_root)) = (self.meta.saved_path.clone(), opts.new_root) else {
            return retag(self);
        };
        // The basename and only the basename: a note lands *in* the project's
        // folder, never in a subdirectory a stale binding happened to name.
        let Some(file) = stored.file_name() else {
            return retag(self);
        };
        let mut to = new_root.join(file);

        // The file as it is right now — `None` when the binding cannot be
        // resolved (the "not in your projects" group) or names nothing on disk.
        let source = resolve_binding(&stored, opts.old_root).filter(|at| at.exists());

        // Already the file it would be moved to: two projects sharing a folder,
        // or a binding that already pointed inside the new one. Nothing to
        // move, and a rename onto itself is not worth risking.
        if let Some(at) = &source {
            if *at == to || is_case_only_rename(at, &to) {
                return retag(self);
            }
        }

        // The question, asked before anything is written.
        if to.exists() {
            match opts.clash {
                ClashChoice::Ask => {
                    return Ok(NoteMove::Clash {
                        free: savepath::first_free(new_root, &file.to_string_lossy()),
                        at: to,
                    })
                }
                ClashChoice::KeepBoth => {
                    to = savepath::first_free(new_root, &file.to_string_lossy())
                        .ok_or(DraftError::NameTaken { path: to })?;
                }
            }
        }

        let outcome = match &source {
            Some(at) => {
                let copied = move_file(at, &to)?;
                self.meta.project = opts.project.map(str::to_owned);
                self.meta.saved_path = Some(binding_for(&to, Some(new_root)));
                self.write_meta()?;
                NoteMove::Moved {
                    from: at.clone(),
                    to: to.clone(),
                    copied,
                }
            }
            // The draft is the note, so render it. The project is set
            // first so the *save's* single meta write carries it — a second
            // write would be a second crash window — and put back if the save
            // fails, so this handle can never persist a move that did not
            // happen.
            None => {
                let previous = self.meta.project.clone();
                self.meta.project = opts.project.map(str::to_owned);
                match self.save_to(
                    &to,
                    SaveOptions {
                        header: opts.header,
                        mode: SaveMode::Guarded,
                        dividers: opts.dividers,
                        notes_root: Some(new_root),
                        adopt_project: None,
                    },
                ) {
                    Ok(report) => NoteMove::Recreated { to: report.path },
                    Err(err) => {
                        self.meta.project = previous;
                        return Err(err);
                    }
                }
            }
        };

        tracing::info!(
            draft = %self.meta.id,
            project = opts.project.unwrap_or("(none)"),
            bound = %self.meta.saved_path.as_deref().unwrap_or(&to).display(),
            outcome = ?outcome,
            "moved a note into another project"
        );
        Ok(outcome)
    }

    /// Refuse to write if the file at `path` is not the one we last wrote.
    ///
    /// A *missing* file is not a conflict: the user moved or deleted the note
    /// and re-saving recreates it — which is the same reading of a missing file
    /// the "files move when you move them" rule takes for a drop. A
    /// file that exists but cannot be read is an [`DraftError::Io`], not a
    /// conflict: we genuinely do not know what is in it, and calling that an
    /// external edit would offer the user an overwrite that is about to fail
    /// anyway.
    ///
    /// A draft with no `last_save_hash` that lands on an existing file is a
    /// conflict too — an unrelated file at the chosen path is precisely the
    /// clobber this guard exists to prevent.
    fn check_unchanged(&self, path: &Path) -> Result<(), DraftError> {
        let existing = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_err("read", path)(err)),
        };

        match &self.meta.last_save_hash {
            Some(hash) if *hash == hash_hex(&existing) => Ok(()),
            _ => Err(DraftError::SaveConflict {
                path: path.to_path_buf(),
            }),
        }
    }

    /// Every parseable line currently in `lines.jsonl`, folded, in file order.
    ///
    /// Read-only, like [`Draft::open`]: malformed records are skipped and left
    /// exactly where they are, and edits are folded in rather than applied to
    /// the file. This is the transcript — what the save renders and what the UI
    /// shows — so it is the one thing callers need after any correction.
    ///
    /// Blocking I/O: **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// [`DraftError::Io`] if the log exists but cannot be read. A missing log is
    /// an empty draft, not an error.
    pub fn read_lines(&self) -> Result<Vec<LineRecord>, DraftError> {
        Ok(self.read_transcript()?.lines)
    }

    /// [`Draft::read_lines`] plus each line's session ordinal.
    ///
    /// What the renderer needs when dividers are on. Separate from
    /// [`Draft::read_lines`] so every existing caller — which only ever wanted
    /// the lines — stays exactly as it was.
    ///
    /// Blocking I/O: **worker thread only** (invariant 5).
    ///
    /// # Errors
    /// As [`Draft::read_lines`].
    pub fn read_transcript(&self) -> Result<Transcript, DraftError> {
        let parsed = self.parse_log()?;
        Ok(Transcript {
            lines: parsed.lines,
            sessions: parsed.sessions,
        })
    }

    /// Read and fold the whole log, keeping everything the fold derived.
    ///
    /// The one read path inside this type, so [`Draft::correct`] — which needs
    /// the base failure flags the public [`Transcript`] does not carry — and
    /// [`Draft::read_transcript`] can never fold differently.
    ///
    /// Blocking I/O: **worker thread only** (invariant 5).
    fn parse_log(&self) -> Result<ParsedLines, DraftError> {
        let lines_path = self.lines_path();
        let raw = match fs::read(&lines_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(io_err("read", &lines_path)(err)),
        };
        Ok(parse_lines(&raw, &lines_path))
    }

    /// Rewrite `meta.json` whole, atomically.
    fn write_meta(&self) -> Result<(), DraftError> {
        let path = self.meta_path();
        let bytes = serde_json::to_vec_pretty(&self.meta).map_err(|source| DraftError::Encode {
            path: path.clone(),
            source,
        })?;
        fsutil::write_atomic(&path, &bytes).map_err(io_err("write", &path))
    }

    /// Read a draft directory: metadata plus every parseable line.
    ///
    /// A missing or unparseable `meta.json` is a hard error naming the path —
    /// that draft is damaged and the user should be told, not handed a guess. A
    /// missing `lines.jsonl` is simply an empty draft: the file is created on
    /// the first append.
    ///
    /// Unparseable lines are skipped and counted. Nothing is rewritten here; the
    /// repair (a single added newline) happens on the next append.
    ///
    /// # Errors
    /// Returns [`DraftError`] if the directory or `meta.json` cannot be read, or
    /// if `meta.json` is not valid JSON.
    pub fn open(dir: &Path) -> Result<LoadedDraft, DraftError> {
        let meta_path = dir.join(META_FILE);
        let meta_bytes = fs::read(&meta_path).map_err(io_err("read", &meta_path))?;
        let meta: DraftMeta =
            serde_json::from_slice(&meta_bytes).map_err(|source| DraftError::BadMeta {
                path: meta_path.clone(),
                source,
            })?;

        let lines_path = dir.join(LINES_FILE);
        let raw = match fs::read(&lines_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(io_err("read", &lines_path)(err)),
        };

        let parsed = parse_lines(&raw, &lines_path);
        // Live means "renders into the note": a failed line has no words yet
        // and is excluded exactly as a soft-deleted one is.
        let live = parsed
            .lines
            .iter()
            .filter(|line| !line.deleted && !line.failed)
            .count();

        Ok(LoadedDraft {
            draft: Self {
                dir: dir.to_path_buf(),
                meta,
                last: None,
                needs_newline: parsed.torn_tail,
                lines: live,
            },
            lines: parsed.lines,
            skipped_lines: parsed.skipped,
        })
    }
}

/// Result of scanning `lines.jsonl`.
struct ParsedLines {
    lines: Vec<LineRecord>,
    /// Session ordinal per line, parallel to `lines`.
    sessions: Vec<usize>,
    /// Whether each line's **base record** was written as a failure, parallel
    /// to `lines`. Permanent history, and the half of the failed rule that
    /// cannot be recovered from a folded record: `LineRecord::failed` on the
    /// way out is this *and* "the text is still empty", so a line that has
    /// been given words reads `false` while its base record still says
    /// otherwise. [`Draft::correct`] needs the base flag to fold one record
    /// the same way a whole re-read would.
    base_failed: Vec<bool>,
    /// The text each line's **base record** was written with, parallel to
    /// `lines`. The other half of the history that the fold consumes:
    /// `LineRecord::original` only exists when the folded text differs, and a
    /// line whose base record failed has it suppressed, so a caller that has to
    /// re-emit the base/edit split — [`Draft::export_line`] — cannot recover it
    /// from the folded record alone.
    base_texts: Vec<String>,
    skipped: usize,
    torn_tail: bool,
    /// What the log's last **complete** record was, which is the whole of what
    /// [`Draft::import_line`]'s divider rule is decided from.
    tail: LogTail,
}

/// The shape of the last complete record in a `lines.jsonl`.
///
/// Deliberately about *records*, not bytes: a torn tail is not a record (the
/// next append repairs it with a newline), and neither is a malformed segment —
/// both are things the loader skips and leaves on disk, and neither can decide
/// whether arrivals need a divider above them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogTail {
    /// No complete record of any shape: an empty log, or one holding nothing
    /// the four-shape rule recognises.
    Empty,
    /// The log already ends in a [`SessionRecord`].
    Session,
    /// A line, an edit or a move — something a divider would separate from.
    Record,
}

/// Parse the line log without ever modifying it, folding edits into their base
/// lines.
///
/// `from_utf8_lossy` is safe here precisely because we never write the result
/// back: a line with invalid UTF-8 fails to parse, is counted, and its original
/// bytes stay on disk untouched.
///
/// # The fold
///
/// Records are applied in file order, last writer wins per field, so everything
/// downstream — [`LoadedDraft::lines`], [`Draft::read_lines`], and therefore the
/// markdown render, which needed no change for this — sees the *folded* state
/// and never has to know corrections exist:
///
/// * folded `text` is the last text-bearing [`EditRecord`], else the base text;
/// * folded `deleted` is the last delete-bearing edit, else the base value;
/// * folded `failed` is the base value **and** the folded text is empty: a line
///   that has words is not a line that failed, and a line that has lost them
///   again is. Structural rather than a second field on [`EditRecord`] that
///   could disagree with the text, and *symmetric* — an undone resolve replays
///   `line_edit(id, "")` and has to land back on the failed row it came from;
/// * folded `original` is the base record's text **iff** the folded text
///   differs from it. It is computed here and never written to disk — the base
///   record *is* the original. A line whose base record failed has no
///   `original` at all: the model produced nothing, and reporting "" as what
///   was first said would put a lie in the audit trail.
///
/// Line *order* starts as base-record file order — an edit never reorders
/// anything — and is then folded the same way by [`MoveRecord`]s, replayed in
/// file order: remove the line, reinsert it after its anchor. A move whose
/// anchor is a soft-deleted line is perfectly valid; deleted lines hold their
/// place in the transcript.
///
/// # Session ordinals
///
/// Each line also gets the number of [`SessionRecord`]s that precede its **base
/// record** in file order. That is deliberately measured in file order and not
/// in folded order: which sitting a line was spoken in is a fact about when it
/// was written, and dragging it somewhere else cannot change it. The ordinals
/// are permuted along with the lines, so after a drag they need not ascend —
/// which is exactly why the renderer can emit two dividers for two interleaved
/// sittings, and why it must not try to be clever about it.
///
/// An edit or move naming a line this file does not contain is counted in
/// `skipped` and left exactly where it is, the same policy malformed lines get.
/// Losing it would be losing a thing the user did. A move that cannot be folded
/// simply leaves its line where it already was — the fold can never drop a line.
fn parse_lines(raw: &[u8], path: &Path) -> ParsedLines {
    let text = String::from_utf8_lossy(raw);
    let mut segments: Vec<&str> = text.split('\n').collect();

    // `split` always yields a trailing element: empty when the file ends in a
    // newline, and the half-written record when it does not.
    let mut skipped = 0;
    let torn_tail = match segments.pop() {
        Some(tail) if !tail.is_empty() => {
            tracing::warn!(
                path = %path.display(),
                bytes = tail.len(),
                "lines.jsonl ends mid-record; skipping the partial tail and keeping its bytes"
            );
            skipped += 1;
            true
        }
        _ => false,
    };

    let mut lines: Vec<LineRecord> = Vec::with_capacity(segments.len());
    // The text each line was born with, parallel to `lines`. Kept separately
    // because `LineRecord::text` is mutated by the fold and the base text is
    // what `original` has to report.
    let mut base_texts: Vec<String> = Vec::with_capacity(segments.len());
    // Whether each line's *base* record failed, parallel to `lines`. Kept
    // separately for the same reason as `base_texts`: the fold clears
    // `LineRecord::failed` on the first text-bearing edit, and the `original`
    // rule below has to know what the record was born as.
    let mut base_failed: Vec<bool> = Vec::with_capacity(segments.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    // The folded order, as indices into `lines`. Kept beside the records rather
    // than by shuffling them, because `index` and `base_texts` are positional:
    // one permutation applied at the end is cheaper and far harder to get wrong
    // than keeping three structures in step through every move.
    let mut order: Vec<usize> = Vec::with_capacity(segments.len());
    // How many session markers have been seen so far, i.e. the ordinal the next
    // base record belongs to.
    let mut session = 0_usize;
    // Session ordinal per base record, parallel to `lines`.
    let mut base_sessions: Vec<usize> = Vec::with_capacity(segments.len());
    // The shape of the last record that parsed, in file order. Skipped
    // segments leave it alone — they are not records.
    let mut tail = LogTail::Empty;

    for segment in segments {
        if segment.trim().is_empty() {
            continue;
        }
        // `LineRecord` first: it is the common case, and the four shapes cannot
        // be confused — a base record has none of `edit_of`, `move_of` or
        // `session_at`, and none of the other three has `id`, `spoken_at` or
        // `audio`, all of which are required fields.
        if let Ok(record) = serde_json::from_str::<LineRecord>(segment) {
            index.insert(record.id.clone(), lines.len());
            order.push(lines.len());
            base_texts.push(record.text.clone());
            base_failed.push(record.failed);
            base_sessions.push(session);
            lines.push(record);
            tail = LogTail::Record;
            continue;
        }

        if let Ok(edit) = serde_json::from_str::<EditRecord>(segment) {
            tail = LogTail::Record;
            match index.get(&edit.edit_of) {
                Some(&at) => apply_edit(&mut lines[at], edit),
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        line = %edit.edit_of,
                        "an edit in lines.jsonl names a line that is not in the file; \
                         skipping it and leaving it on disk"
                    );
                    skipped += 1;
                }
            }
            continue;
        }

        if let Ok(moved) = serde_json::from_str::<MoveRecord>(segment) {
            tail = LogTail::Record;
            fold_move(&moved, &index, &mut order, path, &mut skipped);
            continue;
        }

        match serde_json::from_str::<SessionRecord>(segment) {
            // Everything after this belongs to a later sitting. A marker with a
            // malformed timestamp is not a marker: it falls through to the skip
            // below rather than silently splitting the note in a place nobody
            // asked for.
            Ok(_) => {
                session += 1;
                tail = LogTail::Session;
            }
            // Matched none of the four shapes. `reason` is the last attempt's
            // complaint, which is the most useful one available: a record with
            // none of `edit_of`, `move_of` or `session_at` was not a correction
            // at all.
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    reason = %err,
                    "skipping a malformed line in lines.jsonl; leaving it on disk"
                );
                skipped += 1;
            }
        }
    }

    for ((line, base), was_failed) in lines.iter_mut().zip(&base_texts).zip(&base_failed) {
        // Only *set* it, never clear it: a record that arrived with `original`
        // already filled in (a hand-edited file, say) keeps what it says rather
        // than having it computed away.
        if line.text != *base && !was_failed {
            line.original = Some(base.clone());
        }
        // The whole of the failed rule, in one place and in both directions.
        line.failed = *was_failed && line.text.is_empty();
    }

    // `order` is a permutation of every index, so every line is taken exactly
    // once: a move can reorder the transcript but can never lose a line from it.
    // The session ordinals and the base failure flags ride along in the same
    // permutation, which is what keeps them parallel to the lines they belong
    // to.
    let mut slots: Vec<Option<LineRecord>> = lines.into_iter().map(Some).collect();
    // Taken rather than cloned, for the same reason the records are: one
    // permutation, one owner each.
    let mut spoken_slots: Vec<Option<String>> = base_texts.into_iter().map(Some).collect();
    let mut ordered: Vec<LineRecord> = Vec::with_capacity(order.len());
    let mut sessions: Vec<usize> = Vec::with_capacity(order.len());
    let mut failed_at_birth: Vec<bool> = Vec::with_capacity(order.len());
    let mut spoken: Vec<String> = Vec::with_capacity(order.len());
    for &at in &order {
        if let Some(line) = slots[at].take() {
            ordered.push(line);
            sessions.push(base_sessions[at]);
            failed_at_birth.push(base_failed[at]);
            // Filled in the same pass as the record it belongs to, so the two
            // stay parallel; the fallback is unreachable for the same reason
            // `slots[at]` was `Some`.
            spoken.push(spoken_slots[at].take().unwrap_or_default());
        }
    }

    ParsedLines {
        lines: ordered,
        sessions,
        base_failed: failed_at_birth,
        base_texts: spoken,
        skipped,
        torn_tail,
        tail,
    }
}

/// Fold one move into the running order, or count it and leave it alone.
///
/// Both "no such line" cases are skips rather than guesses: a move whose line or
/// whose anchor is not in this file has no defensible destination, and inventing
/// one would silently rearrange the user's note.
fn fold_move(
    moved: &MoveRecord,
    index: &HashMap<String, usize>,
    order: &mut Vec<usize>,
    path: &Path,
    skipped: &mut usize,
) {
    let mut orphan = |which: &str, id: &str| {
        tracing::warn!(
            path = %path.display(),
            line = %id,
            names = which,
            "a move in lines.jsonl names a line that is not in the file; \
             skipping it and leaving it on disk"
        );
        *skipped += 1;
    };

    let Some(&target) = index.get(&moved.move_of) else {
        orphan("the moved line", &moved.move_of);
        return;
    };
    let anchor = match &moved.after {
        None => None,
        Some(after) => match index.get(after) {
            Some(&at) => Some(at),
            None => {
                orphan("the anchor", after);
                return;
            }
        },
    };

    let Some(from) = order.iter().position(|&at| at == target) else {
        return;
    };
    let anchor = match anchor {
        None => None,
        Some(anchor) => order.iter().position(|&at| at == anchor),
    };

    // A line cannot follow itself, and a move to where the line already is is a
    // record the writer refuses to make — but a hand-edited file can contain
    // one, and it is harmlessly nothing.
    if is_settled(from, anchor) {
        return;
    }
    reposition(order, from, anchor);
}

/// Is a move from `from` to just after `anchor` (or to the top, for `None`) a
/// no-op on a list in this state?
///
/// The one place that question is answered, so [`Draft::move_line`]'s refusal to
/// write a no-op record and the fold's refusal to act on one cannot drift apart.
const fn is_settled(from: usize, anchor: Option<usize>) -> bool {
    match anchor {
        None => from == 0,
        Some(anchor) => anchor == from || anchor + 1 == from,
    }
}

/// Move the item at `from` so it sits immediately after the item at `anchor`, or
/// at the front when `anchor` is `None`.
///
/// Both positions are in the list as it stands *before* the removal, which is
/// how every caller has them.
fn reposition<T>(items: &mut Vec<T>, from: usize, anchor: Option<usize>) {
    let item = items.remove(from);
    let at = match anchor {
        None => 0,
        // The removal shifted everything after `from` down one.
        Some(anchor) if anchor > from => anchor,
        Some(anchor) => anchor + 1,
    };
    items.insert(at, item);
}

/// Fold one correction into the line it corrects. Absent fields are untouched
/// fields, which is what makes "last writer wins per field" work.
/// **`failed` is deliberately not touched here.** No edit record carries it,
/// and the folded value is not "last writer wins" but a function of the base
/// flag and the folded text — applied once, by whoever owns the base flag
/// ([`parse_lines`] for a whole file, [`Draft::correct`] for one record).
fn apply_edit(line: &mut LineRecord, edit: EditRecord) {
    if let Some(text) = edit.text {
        line.text = text;
    }
    if let Some(deleted) = edit.deleted {
        line.deleted = deleted;
    }
    if let Some(ms) = edit.transcribe_ms {
        line.transcribe_ms = Some(ms);
    }
}

// ---------------------------------------------------------------------------
// Wav
// ---------------------------------------------------------------------------

/// Write one utterance as 16 kHz mono 16-bit PCM.
///
/// 16-bit rather than the f32 the transcriber consumed: half the size, and
/// every audio tool on every platform opens it. Re-transcribe
/// converts back, and the quantisation is far below what whisper resolves.
///
/// The file is encoded in memory first and then written in a single durable
/// pass with `create_new`, so a half-written wav can only ever be a brand-new
/// file that no record references yet.
fn write_wav(path: &Path, samples: &[f32]) -> Result<(), DraftError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = io::Cursor::new(Vec::<u8>::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).map_err(|source| DraftError::Wav {
                path: path.to_path_buf(),
                source,
            })?;
        for &sample in samples {
            // Clamp before scaling: anything outside [-1, 1] would wrap on the
            // cast and turn a loud syllable into a click.
            let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round();
            writer
                .write_sample(scaled as i16)
                .map_err(|source| DraftError::Wav {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        // Patches the RIFF header with the final lengths. Zero samples is a
        // valid wav: a header and an empty data chunk.
        writer.finalize().map_err(|source| DraftError::Wav {
            path: path.to_path_buf(),
            source,
        })?;
    }

    write_new_file(path, cursor.get_ref())
}

/// Create a file that is not there yet and write exactly these bytes, durably.
///
/// The one write path for per-line audio, whether the bytes were just encoded
/// from samples ([`write_wav`]) or copied verbatim out of another draft
/// ([`Draft::import_line`]). `create_new`: every caller names a path carrying a
/// fresh ulid, so an existing file would mean something is very wrong — error
/// out rather than overwrite audio (invariant 4). `sync_all`, not `sync_data`:
/// the directory entry has to survive too, because the record that references
/// this file is written next.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), DraftError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err("create", parent))?;
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_err("create", path))?;
    file.write_all(bytes).map_err(io_err("write", path))?;
    file.sync_all().map_err(io_err("flush", path))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Search
//
// The whole of the matching rule, as a pure function over already-folded
// records, so the command in `shell.rs` is a walk and nothing else. It lives
// here rather than in a module of its own because it is a *reading* of a
// transcript and shares one definition with the loader: a line matches only if
// it is the kind of line that would render into the markdown.
// ---------------------------------------------------------------------------

/// A search term that is worth running.
///
/// A newtype rather than a `&str` because the normalisation — trim, then
/// lowercase — has to happen exactly once and before any comparison. A caller
/// holding one of these cannot accidentally compare against a raw field value
/// with its trailing space still on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerm {
    /// Trimmed and lowercased. Never empty — see [`SearchTerm::parse`].
    needle: String,
}

impl SearchTerm {
    /// Normalise a raw term, or `None` if it is search-off.
    ///
    /// **An empty (or all-whitespace) term is the off switch and there is no
    /// other one**. Returning `None` rather than a term that matches
    /// everything is what makes that structural: there is no way to ask this
    /// type for "every line".
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self {
            needle: trimmed.to_lowercase(),
        })
    }

    /// The normalised needle, for echoing back to the caller.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.needle
    }

    /// Case-insensitive literal substring. No tokenising, no fuzzy, no regex —
    /// the design draws "Table" matching "table" and nothing more than that.
    #[must_use]
    pub fn matches_text(&self, text: &str) -> bool {
        text.to_lowercase().contains(&self.needle)
    }

    /// Whether this *line* matches: it has to be live first.
    ///
    /// Live means the same thing it means in [`Draft::open`] — not soft-deleted
    /// and not an unresolved failure — because those are exactly the lines that
    /// never reach the markdown. A search result the user could not find in the
    /// note they hand to an agent would be a lie about what the note contains.
    #[must_use]
    pub fn matches_line(&self, line: &LineRecord) -> bool {
        if line.deleted || line.failed {
            return false;
        }
        self.matches_text(&line.text)
    }
}

/// Indices of every line in `lines` that matches.
///
/// **One index per matching line, however many times the term occurs in it**:
/// the counts the design shows ("6 matches · 3 notes") are counts of lines, and
/// a per-occurrence count would make the tree's numbers disagree with the rows
/// the pane lists.
///
/// Pure and allocation-light: the caller already has the folded records, which
/// is why a torn tail is a non-event here — [`parse_lines`] dropped the orphan
/// bytes long before this sees the list.
#[must_use]
pub fn matching_lines(term: &SearchTerm, lines: &[LineRecord]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| term.matches_line(line))
        .map(|(index, _)| index)
        .collect()
}

/// When a draft was last written to, if it has ever been: the newest **live**
/// line's `spoken_at`.
///
/// Decided here rather than in the window: "when it was last
/// written" is a fact about the store, and a frontend guessing it from a list
/// it may not have would be a second answer. `max` rather than "the last one in
/// the list" because a drag reorders the transcript and every line keeps the
/// timestamp it was spoken at.
///
/// `None` is a draft with nothing live in it — one that has never been written
/// to at all. What to say instead is the caller's rule, because it is the
/// caller that has the draft's creation time: `shell.rs`'s `written_at` falls
/// back to it.
#[must_use]
pub fn last_written(lines: &[LineRecord]) -> Option<DateTime<FixedOffset>> {
    lines
        .iter()
        .filter(|line| !line.deleted && !line.failed)
        .map(|line| line.spoken_at)
        .max()
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// `<draft dir>/audio/<line id>.wav`, with the line id validated.
///
/// One implementation for both callers — the worker, which has a [`Draft`], and
/// the playback command, which deliberately has only a directory (opening a
/// second [`Draft`] on the active draft just to build a path would be a second
/// handle on a directory the worker owns).
fn line_audio_path_in(dir: &Path, id: &str) -> Result<PathBuf, DraftError> {
    // No dots at all, on top of the plain-component rule: the extension is ours
    // to add, and `..` or `line.wav` joined onto the audio directory is how a
    // "play this line" turns into reading some other file.
    if !is_plain_component(id) || id.contains('.') {
        return Err(DraftError::BadId {
            id: id.to_owned(),
            path: dir.join(AUDIO_DIR),
        });
    }
    Ok(dir.join(AUDIO_DIR).join(format!("{id}.wav")))
}

/// Is this a single, ordinary path component that can safely be joined onto a
/// directory we own?
///
/// The boundary check for every id that arrives over IPC. Empty, dot-leading,
/// and anything with a separator, a `..` or a drive prefix in it is refused
/// before it can escape the directory it is about to be joined onto.
fn is_plain_component(id: &str) -> bool {
    if id.is_empty() || id.starts_with('.') {
        return false;
    }
    let mut components = Path::new(id).components();
    // The equality check is what rejects a Windows `C:` prefix and any form the
    // OS would normalise away: the component has to *be* the whole string.
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(name)), None) if name == OsStr::new(id)
    )
}

/// One line of the "outstanding drafts" list the app opens on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSummary {
    /// Draft ulid.
    pub id: String,
    /// Creation time from `meta.json`.
    pub created_at: DateTime<FixedOffset>,
    /// Owning project, if any.
    pub project: Option<String>,
    /// Whether there are changes not yet saved to `saved_path`.
    pub dirty: bool,
    /// The markdown file this draft is bound to, once saved.
    pub saved_path: Option<PathBuf>,
    /// Parseable, non-deleted lines.
    pub line_count: usize,
    /// The draft directory, for reopening it.
    pub dir: PathBuf,
}

/// The result of scanning the drafts root: what loaded, and what did not and
/// why.
///
/// Same shape as [`model::ScanResult`](crate::model::ScanResult), for the same
/// reason: a draft that silently fails to appear in the list looks to the user
/// like lost work.
#[derive(Debug, Default)]
pub struct DraftScan {
    /// Loadable drafts, sorted by id — which, ulids being lexically sortable,
    /// is creation order.
    pub drafts: Vec<DraftSummary>,
    /// Directories that look like drafts but could not be read, each with the
    /// reason to show the user.
    pub rejected: Vec<(PathBuf, DraftError)>,
}

/// A drafts root directory.
///
/// The root is always explicit so tests can point at a tempdir; see
/// [`default_drafts_dir`] for what the app passes.
#[derive(Debug, Clone)]
pub struct DraftStore {
    root: PathBuf,
}

impl DraftStore {
    /// Take a root. Nothing is created or read until it is used.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create a new draft: the directory skeleton and an initial `meta.json`.
    ///
    /// `meta.json` is written before the (empty) `lines.jsonl`, so a crash in
    /// between leaves a loadable empty draft rather than a directory the
    /// listing has to reject.
    ///
    /// # Errors
    /// Returns [`DraftError`] if the directories or either file cannot be
    /// created.
    pub fn create_draft(&self, project: Option<&str>) -> Result<Draft, DraftError> {
        let id = Ulid::new().to_string();
        let dir = self.root.join(&id);
        let audio_dir = dir.join(AUDIO_DIR);
        fs::create_dir_all(&audio_dir).map_err(io_err("create", &audio_dir))?;

        let draft = Draft {
            meta: DraftMeta {
                version: META_VERSION,
                id,
                created_at: Local::now().fixed_offset(),
                project: project.map(str::to_owned),
                saved_path: None,
                dirty: false,
                last_save_hash: None,
            },
            dir,
            last: None,
            needs_newline: false,
            lines: 0,
        };
        draft.write_meta()?;

        // `append`, not `truncate`: if a file somehow already exists under this
        // fresh ulid, we still must not blank it (invariant 4).
        let lines_path = draft.lines_path();
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(&lines_path)
            .map_err(io_err("create", &lines_path))?;

        tracing::info!(draft = %draft.meta.id, dir = %draft.dir.display(), "created draft");

        Ok(draft)
    }

    /// Every outstanding draft under the root.
    ///
    /// A missing root is an empty result, not an error — a fresh install has no
    /// drafts, exactly as a fresh install has no models. Entries that are not
    /// directories are ignored silently; directories that fail to load are
    /// reported as rejects rather than dropped.
    ///
    /// # Errors
    /// Returns [`DraftError::Io`] if the root exists but cannot be listed.
    pub fn list_drafts(&self) -> Result<DraftScan, DraftError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                tracing::info!(dir = %self.root.display(), "drafts dir does not exist yet");
                return Ok(DraftScan::default());
            }
            Err(err) => return Err(io_err("read", &self.root)(err)),
        };

        let mut scan = DraftScan::default();

        for entry in entries {
            let entry = entry.map_err(io_err("read", &self.root))?;
            let path = entry.path();
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                _ => continue,
            }
            // Dot-directories are ours, not the user's: `.trash` is the one
            // that exists today, and reporting it as an unreadable draft would
            // put a permanent error in the list the app opens on.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }

            match Draft::open(&path) {
                Ok(loaded) => scan.drafts.push(DraftSummary {
                    id: loaded.draft.meta.id.clone(),
                    created_at: loaded.draft.meta.created_at,
                    project: loaded.draft.meta.project.clone(),
                    dirty: loaded.draft.meta.dirty,
                    saved_path: loaded.draft.meta.saved_path.clone(),
                    line_count: loaded.lines.iter().filter(|line| !line.deleted).count(),
                    dir: path,
                }),
                Err(err) => {
                    tracing::warn!(path = %path.display(), reason = %err, "unreadable draft");
                    scan.rejected.push((path, err));
                }
            }
        }

        // Ulids sort lexically by creation time, so this is chronological and
        // stable across launches — directory iteration order is not.
        scan.drafts.sort_by(|a, b| a.id.cmp(&b.id));
        scan.rejected.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(scan)
    }

    /// Where `.trash/` lives.
    #[must_use]
    pub fn trash_dir(&self) -> PathBuf {
        self.root.join(TRASH_DIR)
    }

    /// The directory an id names, with the id validated first.
    ///
    /// Ids reach this crate from the frontend over IPC, so "a single normal
    /// path component, not starting with a dot" is checked here rather than
    /// trusted: `../../..` joined onto the root is how a discard turns into a
    /// catastrophe.
    ///
    /// # Errors
    /// Returns [`DraftError::BadId`] if the id is not a plain directory name.
    pub fn draft_path(&self, id: &str) -> Result<PathBuf, DraftError> {
        let bad = || DraftError::BadId {
            id: id.to_owned(),
            path: self.root.clone(),
        };

        if !is_plain_component(id) {
            return Err(bad());
        }

        Ok(self.root.join(id))
    }

    /// The wav of one line of one draft, with **both** ids validated.
    ///
    /// For the playback path, which reaches this crate straight off the IPC
    /// boundary with two strings from the frontend and must not be able to name
    /// anything but a line's audio under this root.
    ///
    /// # Errors
    /// [`DraftError::BadId`] if either id is not a plain component.
    pub fn line_audio_path(&self, draft_id: &str, line_id: &str) -> Result<PathBuf, DraftError> {
        let dir = self.draft_path(draft_id)?;
        line_audio_path_in(&dir, line_id)
    }

    /// Discard one draft: move its directory into `.trash/`.
    ///
    /// This is a `rename` and a marker file. Nothing is deleted, nothing is
    /// truncated, and no file inside the draft is opened — invariant 4 says the
    /// user's notes are never destroyed, and "discard" in this app means
    /// "moved somewhere the list does not show, for thirty days".
    ///
    /// The caller must not hold a [`Draft`] handle on this directory: the
    /// worker drops its handle before asking, which is why the
    /// discard of the *active* draft is executed there and not here.
    ///
    /// Returns the path the draft now lives at.
    ///
    /// # Errors
    /// [`DraftError::BadId`] for an id that is not a plain directory name,
    /// [`DraftError::Missing`] if there is no such draft,
    /// [`DraftError::TrashCollision`] if `.trash/` already holds that name, and
    /// [`DraftError::Io`] if the trash directory or the rename fails.
    pub fn discard(&self, id: &str) -> Result<PathBuf, DraftError> {
        let dir = self.draft_path(id)?;
        if !dir.is_dir() {
            return Err(DraftError::Missing { path: dir });
        }

        let trash = self.trash_dir();
        fs::create_dir_all(&trash).map_err(io_err("create", &trash))?;

        let target = trash.join(id);
        if target.exists() {
            // Ulids do not collide, so this is not a case to paper over by
            // picking another name: surface it and leave both directories be.
            return Err(DraftError::TrashCollision { path: target });
        }

        fs::rename(&dir, &target).map_err(io_err("move", &dir))?;

        // Advisory, and written *after* the move: the draft is already safe,
        // and a sweep that cannot read this falls back to the modified time.
        // Failing the discard over a marker would be the wrong trade.
        let marker = target.join(DISCARDED_MARKER);
        let stamp = Local::now().fixed_offset().to_rfc3339();
        if let Err(err) = fs::write(&marker, stamp) {
            tracing::warn!(
                path = %marker.display(),
                error = %err,
                "could not record when this draft was discarded; the sweep will use its modified time"
            );
        }

        tracing::info!(draft = %id, trash = %target.display(), "discarded draft");
        Ok(target)
    }

    /// Delete trashed drafts older than `retention`.
    ///
    /// **The only permanent delete in the codebase.** The rules sanction exactly
    /// this one (discards go to `.trash/`, swept after 30 days); every other
    /// path in this module adds or renames.
    ///
    /// Age is measured from the discard: the `discarded_at` marker if it can be
    /// read and parsed, else the directory's modified time. If neither is
    /// available the entry is **kept** with a warning — an unknown age is never
    /// treated as an old one, because the cost of guessing wrong is the user's
    /// work.
    ///
    /// A failed delete is counted and logged, never fatal: a locked file is not
    /// a reason to refuse to start.
    ///
    /// # Errors
    /// Returns [`DraftError::Io`] only if `.trash/` exists but cannot be
    /// listed. A missing `.trash/` is a zeroed result.
    pub fn sweep_trash(&self, retention: Duration) -> Result<TrashSweep, DraftError> {
        let trash = self.trash_dir();
        let entries = match fs::read_dir(&trash) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(TrashSweep::default()),
            Err(err) => return Err(io_err("read", &trash)(err)),
        };

        let now = SystemTime::now();
        let mut sweep = TrashSweep::default();

        for entry in entries {
            let entry = entry.map_err(io_err("read", &trash))?;
            let path = entry.path();
            let metadata = match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => metadata,
                _ => continue,
            };

            let marker = fs::read_to_string(path.join(DISCARDED_MARKER)).ok();
            let age = discard_age(marker.as_deref(), metadata.modified().ok(), now);

            match age {
                Some(age) if age >= retention => match fs::remove_dir_all(&path) {
                    Ok(()) => {
                        tracing::info!(
                            path = %path.display(),
                            days = age.as_secs() / 86_400,
                            "swept a discarded draft"
                        );
                        sweep.removed += 1;
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %err,
                            "could not sweep a discarded draft; leaving it for next time"
                        );
                        sweep.failed += 1;
                    }
                },
                Some(_) => sweep.kept += 1,
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "cannot tell when this draft was discarded; keeping it"
                    );
                    sweep.kept += 1;
                }
            }
        }

        Ok(sweep)
    }
}

/// What one [`DraftStore::sweep_trash`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrashSweep {
    /// Entries permanently deleted because they were past the retention.
    pub removed: usize,
    /// Entries left alone: still young, or of an age we could not establish.
    pub kept: usize,
    /// Entries that were old enough but could not be deleted.
    pub failed: usize,
}

/// How long ago a trashed draft was discarded, from the two things that might
/// know.
///
/// Pure so the "we do not know, so we do not delete" branch is testable without
/// contriving a filesystem that refuses to stat a directory.
///
/// A marker stamped in the future yields zero rather than nothing: the age is
/// known to be short, and a clock that moved backwards must not make an entry
/// look ancient — nor unknown.
fn discard_age(
    marker: Option<&str>,
    modified: Option<SystemTime>,
    now: SystemTime,
) -> Option<Duration> {
    let from_marker = marker
        .and_then(|text| DateTime::parse_from_rfc3339(text.trim()).ok())
        .map(SystemTime::from);

    let discarded_at = from_marker.or(modified)?;
    Some(now.duration_since(discarded_at).unwrap_or(Duration::ZERO))
}

/// `<platform data dir>/sotone/drafts`, or a relative `drafts` folder if the
/// platform reports no data directory. Never panics: a missing data dir must
/// not stop the app from starting.
#[must_use]
pub fn default_drafts_dir() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("drafts"),
        |dir| dir.join(APP_DIR).join("drafts"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).expect("test timestamp")
    }

    fn store() -> (tempfile::TempDir, DraftStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = DraftStore::new(dir.path().join("drafts"));
        (dir, store)
    }

    fn append(draft: &mut Draft, text: &str, at: &str, samples: &[f32]) -> LineRecord {
        draft
            .append_line(NewLine {
                text: text.to_owned(),
                spoken_at: ts(at),
                samples,
                transcribe_ms: Some(42),
                failed: false,
            })
            .expect("append")
            .clone()
    }

    /// The line a transcribe failure writes: no words, the audio kept, and
    /// `transcribe_ms` unset because nothing was measured.
    fn append_failed(draft: &mut Draft, at: &str, samples: &[f32]) -> LineRecord {
        draft
            .append_line(NewLine {
                text: String::new(),
                spoken_at: ts(at),
                samples,
                transcribe_ms: None,
                failed: true,
            })
            .expect("append")
            .clone()
    }

    #[test]
    fn appended_lines_reload_identically() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(Some("demo")).expect("create");
        let dir = draft.dir().to_path_buf();

        let written = vec![
            append(
                &mut draft,
                "first finding",
                "2026-08-03T10:00:00+02:00",
                &[0.1],
            ),
            append(
                &mut draft,
                "second",
                "2026-08-03T10:00:05+02:00",
                &[-0.2, 0.2],
            ),
            append(
                &mut draft,
                "third",
                "2026-08-03T10:00:09.5+02:00",
                &[0.0; 8],
            ),
        ];

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.lines, written);
        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(loaded.draft.meta().project.as_deref(), Some("demo"));
    }

    #[test]
    fn each_line_gets_a_readable_16k_mono_wav() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");

        let samples: Vec<f32> = (0..1000).map(|i| (i as f32 / 500.0) - 1.0).collect();
        let record = append(&mut draft, "spoken", "2026-08-03T10:00:00+02:00", &samples);

        let path = draft.dir().join(&record.audio);
        assert!(path.is_file(), "wav missing at {}", path.display());

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, TARGET_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);

        let read: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("read samples");
        assert_eq!(read.len(), samples.len());

        for (original, stored) in samples.iter().zip(&read) {
            let back = f32::from(*stored) / f32::from(i16::MAX);
            assert!(
                (back - original).abs() <= 2.0 / f32::from(i16::MAX),
                "expected {original}, read back {back}"
            );
        }
    }

    #[test]
    fn empty_samples_produce_a_valid_empty_wav() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");

        let record = append(&mut draft, "silence", "2026-08-03T10:00:00+02:00", &[]);

        let path = draft.dir().join(&record.audio);
        let reader = hound::WavReader::open(&path).expect("open wav");
        assert_eq!(reader.len(), 0);

        let loaded = Draft::open(draft.dir()).expect("open");
        assert_eq!(loaded.lines, vec![record]);
        assert_eq!(loaded.skipped_lines, 0);
    }

    #[test]
    fn a_torn_tail_is_skipped_counted_and_never_removed() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let dir = draft.dir().to_path_buf();

        append(&mut draft, "one", "2026-08-03T10:00:00+02:00", &[0.1]);
        append(&mut draft, "two", "2026-08-03T10:00:01+02:00", &[0.1]);
        drop(draft);

        // A crash mid-append: a partial record with no terminating newline.
        let lines_path = dir.join(LINES_FILE);
        let garbage = br#"{"id":"01HALF","spoken_at":"2026-08-0"#;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&lines_path)
            .expect("open");
        file.write_all(garbage).expect("write garbage");
        drop(file);

        let loaded = Draft::open(&dir).expect("open");
        assert_eq!(loaded.lines.len(), 2);
        assert_eq!(loaded.skipped_lines, 1);

        let mut reopened = loaded.draft;
        append(&mut reopened, "three", "2026-08-03T10:00:02+02:00", &[0.1]);

        let again = Draft::open(&dir).expect("reopen");
        assert_eq!(again.lines.len(), 3);
        assert_eq!(again.lines[2].text, "three");
        // The torn bytes survive as their own malformed line: nothing truncated.
        assert_eq!(again.skipped_lines, 1);
        let bytes = fs::read(&lines_path).expect("read lines");
        assert!(
            bytes.windows(garbage.len()).any(|window| window == garbage),
            "the torn bytes were removed"
        );
    }

    #[test]
    fn a_malformed_interior_line_is_skipped_and_its_neighbours_load() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let dir = draft.dir().to_path_buf();

        append(&mut draft, "before", "2026-08-03T10:00:00+02:00", &[0.1]);
        drop(draft);

        let lines_path = dir.join(LINES_FILE);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&lines_path)
            .expect("open");
        file.write_all(b"this is not json at all\n").expect("write");
        drop(file);

        let loaded = Draft::open(&dir).expect("open");
        let mut reopened = loaded.draft;
        append(&mut reopened, "after", "2026-08-03T10:00:02+02:00", &[0.1]);

        let again = Draft::open(&dir).expect("reopen");
        let texts: Vec<&str> = again.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["before", "after"]);
        assert_eq!(again.skipped_lines, 1);
    }

    #[test]
    fn meta_round_trips_and_the_first_append_marks_it_dirty() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(Some("playtest")).expect("create");
        let dir = draft.dir().to_path_buf();

        let fresh = Draft::open(&dir).expect("open fresh");
        assert!(!fresh.draft.meta().dirty);
        assert_eq!(fresh.draft.meta().version, META_VERSION);
        assert_eq!(fresh.draft.meta().id, draft.id());
        assert!(fresh.draft.meta().saved_path.is_none());
        assert!(fresh.draft.meta().last_save_hash.is_none());
        assert!(fresh.lines.is_empty());

        append(&mut draft, "a finding", "2026-08-03T10:00:00+02:00", &[0.1]);

        let after = Draft::open(&dir).expect("open after");
        assert!(after.draft.meta().dirty);
        assert_eq!(after.draft.meta().created_at, fresh.draft.meta().created_at);
    }

    #[test]
    fn meta_tolerates_unknown_fields_and_missing_optional_ones() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00",
                 "a_field_from_the_future":{"nested":true}}"#,
        )
        .expect("seed meta");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.draft.meta().id, "01JDRAFT");
        assert!(loaded.draft.meta().project.is_none());
        assert!(!loaded.draft.meta().dirty);
        assert!(loaded.lines.is_empty());
        assert_eq!(loaded.skipped_lines, 0);
    }

    #[test]
    fn a_line_without_the_optional_fields_still_loads() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00"}"#,
        )
        .expect("seed meta");
        fs::write(
            dir.join(LINES_FILE),
            b"{\"id\":\"01JLINE\",\"spoken_at\":\"2026-08-03T10:00:00+02:00\",\
              \"text\":\"terse\",\"original\":null,\"audio\":\"audio/01JLINE.wav\"}\n",
        )
        .expect("seed lines");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.lines.len(), 1);
        assert!(!loaded.lines[0].deleted);
        assert!(loaded.lines[0].transcribe_ms.is_none());
    }

    #[test]
    fn listing_a_missing_root_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let store = DraftStore::new(tmp.path().join("nothing-here"));

        let scan = store.list_drafts().expect("list");

        assert!(scan.drafts.is_empty());
        assert!(scan.rejected.is_empty());
    }

    #[test]
    fn listing_separates_loadable_drafts_from_damaged_ones() {
        let (_tmp, store) = store();

        let mut good = store.create_draft(Some("demo")).expect("create good");
        append(&mut good, "kept", "2026-08-03T10:00:00+02:00", &[0.1]);

        let broken = store.create_draft(None).expect("create broken");
        fs::write(broken.meta_path(), b"{ this is not json").expect("mangle meta");

        let scan = store.list_drafts().expect("list");

        assert_eq!(scan.drafts.len(), 1);
        assert_eq!(scan.drafts[0].id, good.id());
        assert_eq!(scan.drafts[0].line_count, 1);
        assert!(scan.drafts[0].dirty);
        assert_eq!(scan.drafts[0].project.as_deref(), Some("demo"));
        assert_eq!(scan.drafts[0].dir, good.dir());

        assert_eq!(scan.rejected.len(), 1);
        assert_eq!(scan.rejected[0].0, broken.dir());
        assert!(matches!(scan.rejected[0].1, DraftError::BadMeta { .. }));
        assert!(scan.rejected[0].1.path().ends_with(META_FILE));
    }

    #[test]
    fn listing_is_sorted_by_id_and_counts_only_live_lines() {
        let (_tmp, store) = store();

        let first = store.create_draft(None).expect("first");
        let second = store.create_draft(None).expect("second");

        // Ulids are monotonic enough here that creation order is id order.
        let mut ids = vec![first.id().to_owned(), second.id().to_owned()];
        ids.sort();

        let scan = store.list_drafts().expect("list");
        let listed: Vec<String> = scan.drafts.iter().map(|d| d.id.clone()).collect();
        assert_eq!(listed, ids);
        assert!(scan.drafts.iter().all(|d| d.line_count == 0));
    }

    #[test]
    fn discarding_moves_the_draft_into_trash_and_out_of_the_list() {
        let (_tmp, store) = store();
        let mut kept = store.create_draft(None).expect("kept");
        append(&mut kept, "stays", "2026-08-04T10:00:00+02:00", &[0.1]);
        let mut going = store.create_draft(Some("demo")).expect("going");
        append(&mut going, "goes", "2026-08-04T10:00:01+02:00", &[0.1]);
        let id = going.id().to_owned();
        let dir = going.dir().to_path_buf();
        // The handle must be gone before the rename; that ordering is the
        // worker's job in the app, and this test states it.
        drop(going);

        let trashed = store.discard(&id).expect("discard");

        assert!(!dir.exists(), "the draft directory was left behind");
        assert_eq!(trashed, store.trash_dir().join(&id));
        // Nothing was destroyed: the line and its audio are one directory over.
        let moved = Draft::open(&trashed).expect("open the trashed draft");
        assert_eq!(moved.lines.len(), 1);
        assert_eq!(moved.lines[0].text, "goes");
        assert!(trashed.join(&moved.lines[0].audio).is_file());

        let scan = store.list_drafts().expect("list");
        let ids: Vec<&str> = scan.drafts.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec![kept.id()]);
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
    }

    #[test]
    fn the_trash_directory_is_never_listed_as_a_draft() {
        let (_tmp, store) = store();
        let draft = store.create_draft(None).expect("create");
        let id = draft.id().to_owned();
        drop(draft);
        store.discard(&id).expect("discard");

        // `.trash` is a directory under the root with no meta.json in it; the
        // dot-skip is the only thing keeping it out of `rejected`.
        assert!(store.trash_dir().is_dir());
        let scan = store.list_drafts().expect("list");
        assert!(scan.drafts.is_empty());
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
    }

    #[test]
    fn a_discard_writes_a_marker_and_refuses_to_land_on_an_existing_one() {
        let (_tmp, store) = store();
        let first = store.create_draft(None).expect("first");
        let id = first.id().to_owned();
        drop(first);
        let trashed = store.discard(&id).expect("discard");

        let marker = fs::read_to_string(trashed.join(DISCARDED_MARKER)).expect("marker");
        assert!(
            DateTime::parse_from_rfc3339(marker.trim()).is_ok(),
            "marker is not RFC3339: {marker:?}"
        );

        // Same id again (only reachable if something outside Sotone put it
        // there): the second draft stays where it is rather than overwriting.
        fs::create_dir_all(store.root().join(&id)).expect("re-create");
        let err = store.discard(&id).expect_err("collision");
        assert!(matches!(err, DraftError::TrashCollision { .. }), "{err}");
        assert!(
            store.root().join(&id).is_dir(),
            "the draft was moved anyway"
        );
    }

    #[test]
    fn discard_refuses_traversal_and_unknown_ids() {
        let (_tmp, store) = store();
        store.create_draft(None).expect("create");

        // Forward slashes and the dot forms are separators on every platform we
        // build for; backslash is Windows-only and is left to the OS's own
        // component parsing rather than asserted here.
        for id in [
            "",
            ".",
            "..",
            ".trash",
            "../escape",
            "nested/draft",
            "/absolute",
        ] {
            match store.discard(id) {
                Err(DraftError::BadId { .. }) => {}
                other => panic!("{id:?} was not rejected as a bad id: {other:?}"),
            }
        }

        // Well-formed, but there is no such draft.
        let err = store.discard("01JNOSUCHDRAFT").expect_err("missing");
        assert!(matches!(err, DraftError::Missing { .. }), "{err}");
    }

    #[test]
    fn the_sweep_removes_old_entries_and_keeps_fresh_ones() {
        let (_tmp, store) = store();

        let old = store.create_draft(None).expect("old");
        let old_id = old.id().to_owned();
        drop(old);
        let old_path = store.discard(&old_id).expect("discard old");
        // Backdate the marker: the age is measured from the discard, and this
        // is the only part of it a test can control.
        fs::write(
            old_path.join(DISCARDED_MARKER),
            (Local::now() - chrono::Duration::days(31))
                .fixed_offset()
                .to_rfc3339(),
        )
        .expect("backdate");

        let fresh = store.create_draft(None).expect("fresh");
        let fresh_id = fresh.id().to_owned();
        drop(fresh);
        let fresh_path = store.discard(&fresh_id).expect("discard fresh");

        let sweep = store.sweep_trash(TRASH_RETENTION).expect("sweep");

        assert_eq!(
            sweep,
            TrashSweep {
                removed: 1,
                kept: 1,
                failed: 0
            }
        );
        assert!(!old_path.exists());
        assert!(fresh_path.is_dir());
    }

    #[test]
    fn sweeping_a_store_with_no_trash_is_a_zeroed_result() {
        let (_tmp, store) = store();
        assert_eq!(
            store.sweep_trash(TRASH_RETENTION).expect("sweep"),
            TrashSweep::default()
        );
    }

    #[test]
    fn an_entry_of_unknown_age_is_never_swept() {
        // The invariant-4 branch: no marker, no modified time, no delete. Pure,
        // because a directory that cannot be stat'd is not something a test can
        // conjure portably.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(discard_age(None, None, now), None);
        assert_eq!(discard_age(Some("not a timestamp"), None, now), None);

        // A missing or unreadable marker falls back to the modified time.
        let a_week = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(
            discard_age(None, Some(now - a_week), now),
            Some(a_week),
            "the modified time is the fallback"
        );
        assert_eq!(
            discard_age(Some("{}"), Some(now - a_week), now),
            Some(a_week),
            "an unparseable marker falls back too"
        );

        // The marker wins when it parses, and a future stamp reads as brand new
        // rather than as unknown.
        let stamp = DateTime::<Local>::from(now - a_week)
            .fixed_offset()
            .to_rfc3339();
        assert_eq!(discard_age(Some(&stamp), Some(now), now), Some(a_week));
        let future = DateTime::<Local>::from(now + a_week)
            .fixed_offset()
            .to_rfc3339();
        assert_eq!(discard_age(Some(&future), None, now), Some(Duration::ZERO));
    }

    #[test]
    fn a_resumed_draft_keeps_counting_from_where_it_stopped() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        assert_eq!(draft.line_count(), 0);
        for _ in 0..7 {
            append(&mut draft, "line", "2026-08-04T10:00:00+02:00", &[0.1]);
        }
        assert_eq!(draft.line_count(), 7);
        let dir = draft.dir().to_path_buf();
        drop(draft);

        let reopened = Draft::open(&dir).expect("reopen");
        assert_eq!(reopened.draft.line_count(), 7);
        let mut reopened = reopened.draft;
        append(&mut reopened, "eighth", "2026-08-04T10:01:00+02:00", &[0.1]);
        assert_eq!(reopened.line_count(), 8);
    }

    #[test]
    fn the_first_save_writes_the_file_binds_it_and_clears_dirty() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(
            &mut draft,
            "the menu does nothing",
            "2026-08-04T14:32:07+02:00",
            &[0.1],
        );
        append(
            &mut draft,
            "clipping at the fence",
            "2026-08-04T14:33:12+02:00",
            &[0.1],
        );
        assert!(draft.dirty());
        assert!(draft.saved_path().is_none());

        let target = tmp.path().join("notes").join("playtest.md");
        let report = draft.save_as(&target, Some("# Playtest")).expect("save");

        let written = fs::read_to_string(&target).expect("read markdown");
        assert_eq!(
            written,
            "# Playtest\n\n- 14:32:07 — the menu does nothing\n- 14:33:12 — clipping at the fence\n"
        );
        assert_eq!(report.path, target);
        assert_eq!(report.bytes, written.len());
        assert_eq!(report.lines, 2);

        assert!(!draft.dirty());
        assert_eq!(draft.saved_path(), Some(target.as_path()));

        // And the binding is on disk, not just on the handle.
        let reopened = Draft::open(draft.dir()).expect("reopen");
        assert!(!reopened.draft.meta().dirty);
        assert_eq!(
            reopened.draft.meta().saved_path.as_deref(),
            Some(target.as_path())
        );
        assert_eq!(
            reopened.draft.meta().last_save_hash.as_deref(),
            Some(hash_hex(written.as_bytes()).as_str())
        );
    }

    #[test]
    fn appending_after_a_save_makes_the_draft_dirty_again() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "one", "2026-08-04T14:32:07+02:00", &[0.1]);

        let target = tmp.path().join("notes.md");
        draft.save_as(&target, None).expect("save");
        assert!(!draft.dirty());

        append(&mut draft, "two", "2026-08-04T14:34:00+02:00", &[0.1]);
        assert!(draft.dirty());
        assert!(Draft::open(draft.dir()).expect("reopen").draft.meta().dirty);

        // Re-saving over our own unmodified file picks up the new line.
        let report = draft.save(None).expect("re-save");
        assert_eq!(report.lines, 2);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 14:32:07 — one\n- 14:34:00 — two\n"
        );
        assert!(!draft.dirty());
    }

    #[test]
    fn an_externally_edited_file_is_a_conflict_and_is_left_byte_identical() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "first", "2026-08-04T14:32:07+02:00", &[0.1]);

        let target = tmp.path().join("notes.md");
        draft.save_as(&target, None).expect("save");

        // The user tidied the file in their editor.
        let edited = "- 14:32:07 — first, reworded by hand\n";
        fs::write(&target, edited).expect("external edit");
        append(&mut draft, "second", "2026-08-04T14:35:00+02:00", &[0.1]);

        let err = draft.save(None).expect_err("conflict");
        match &err {
            DraftError::SaveConflict { path } => assert_eq!(path, &target),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // Provably untouched: the bytes on disk are the ones the user wrote.
        assert_eq!(fs::read_to_string(&target).expect("read"), edited);
        // And nothing was staged and left behind next to it either.
        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        // The draft still knows it has unsaved work.
        assert!(draft.dirty());

        // Explicit overwrite is the resolution, and it re-arms the guard.
        let report = draft
            .save_with(None, SaveMode::Overwrite, SessionDividers::default())
            .expect("overwrite");
        let after = "- 14:32:07 — first\n- 14:35:00 — second\n";
        assert_eq!(fs::read_to_string(&target).expect("read"), after);
        assert_eq!(report.bytes, after.len());
        assert!(!draft.dirty());

        append(&mut draft, "third", "2026-08-04T14:36:00+02:00", &[0.1]);
        draft.save(None).expect("guarded save after overwrite");
    }

    #[test]
    fn saving_onto_a_pre_existing_unrelated_file_is_a_conflict() {
        // No `last_save_hash` yet, and something is already there: this is the
        // clobber the guard exists for, not a first save.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "mine", "2026-08-04T14:32:07+02:00", &[0.1]);

        let target = tmp.path().join("someone-elses.md");
        fs::write(&target, b"important existing notes\n").expect("seed");

        let err = draft.save_as(&target, None).expect_err("conflict");
        assert!(matches!(err, DraftError::SaveConflict { .. }), "{err}");
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "important existing notes\n"
        );
        assert!(draft.saved_path().is_none(), "the draft was bound anyway");
    }

    #[test]
    fn a_missing_saved_file_is_recreated_rather_than_treated_as_a_conflict() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "only line", "2026-08-04T14:32:07+02:00", &[0.1]);

        let target = tmp.path().join("notes.md");
        draft.save_as(&target, None).expect("save");
        fs::remove_file(&target).expect("user moved the file away");

        draft.save(None).expect("re-save recreates it");
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 14:32:07 — only line\n"
        );
    }

    #[test]
    fn a_save_can_rebind_to_a_different_path() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "line", "2026-08-04T14:32:07+02:00", &[0.1]);

        let first = tmp.path().join("first.md");
        let second = tmp.path().join("second.md");
        draft.save_as(&first, None).expect("first save");
        draft.save_as(&second, None).expect("rebind");

        assert_eq!(draft.saved_path(), Some(second.as_path()));
        // The old file is left exactly as it was; rebinding is not a move.
        assert_eq!(
            fs::read_to_string(&first).expect("read first"),
            fs::read_to_string(&second).expect("read second")
        );
    }

    #[test]
    fn save_without_a_saved_path_says_so_instead_of_guessing() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "line", "2026-08-04T14:32:07+02:00", &[0.1]);

        let err = draft.save(None).expect_err("never saved");
        match &err {
            DraftError::NeverSaved { path } => assert_eq!(path, draft.dir()),
            other => panic!("expected NeverSaved, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Relative bindings and project adoption
    //
    // A note's `saved_path` is stored relative to its project's notes folder, so
    // moving the folder and re-pointing the project carries every note with it —
    // no per-note repair, and no resurrection of the old
    // folder by "missing = recreate".
    // -----------------------------------------------------------------------

    /// A save through the relative-binding path: resolve the target, then write it.
    fn save_in(
        draft: &mut Draft,
        root: &Path,
        first_save: &Path,
        adopt: Option<&str>,
    ) -> SaveReport {
        let target = draft
            .save_target(first_save, Some(root))
            .expect("a resolvable target");
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create the notes folder");
        }
        draft
            .save_to(
                &target,
                SaveOptions {
                    notes_root: Some(root),
                    adopt_project: adopt,
                    ..SaveOptions::default()
                },
            )
            .expect("save")
    }

    #[test]
    fn a_save_inside_the_notes_folder_is_bound_relative_to_it() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        let first = root.join("Ludo 2026-08-07.md");
        let report = save_in(&mut draft, &root, &first, None);

        assert_eq!(report.path, first, "the file itself is written absolutely");
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("Ludo 2026-08-07.md")),
            "the binding is stored relative to the notes folder"
        );
        assert_eq!(
            draft.resolved_saved_path(Some(&root)),
            Some(first.clone()),
            "and resolves back to the same file"
        );
        assert!(first.is_file());
    }

    #[test]
    fn a_relative_binding_follows_the_notes_folder_when_it_moves() {
        // With an absolute binding a re-save after a folder
        // move recreates the *old* folder ("missing = recreate" plus
        // create-parents). A relative binding is the fix, and this pins it.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);

        let old_root = tmp.path().join("A");
        let first = old_root.join("note.md");
        save_in(&mut draft, &old_root, &first, None);
        assert!(first.is_file());

        // The user moves the folder and re-points the project at it. Nothing
        // repairs the note by hand.
        let new_root = tmp.path().join("B");
        fs::rename(&old_root, &new_root).expect("move the notes folder");
        assert!(!old_root.exists());

        append(
            &mut draft,
            "another finding",
            "2026-08-07T14:40:00+02:00",
            &[0.1],
        );
        let report = save_in(&mut draft, &new_root, Path::new("unused.md"), None);

        assert_eq!(report.path, new_root.join("note.md"));
        assert!(new_root.join("note.md").is_file());
        assert!(
            !old_root.exists(),
            "the save resurrected the folder the user moved away from"
        );
        let written = fs::read_to_string(new_root.join("note.md")).expect("read back");
        assert!(written.contains("another finding"), "{written}");
    }

    #[test]
    fn a_legacy_absolute_binding_keeps_working_and_is_rebound_on_the_next_save() {
        // Older builds wrote absolute paths. They must go on resolving
        // untouched, and the migration is the next successful save — never a
        // load-time rewrite.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        fs::create_dir_all(&root).expect("notes dir");
        let target = root.join("legacy.md");
        draft.save_as(&target, None).expect("013-shaped save");
        assert_eq!(draft.saved_path(), Some(target.as_path()));

        // Reloaded from disk, the absolute binding is exactly what it was.
        let reloaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(reloaded.draft.saved_path(), Some(target.as_path()));
        assert_eq!(
            reloaded.draft.resolved_saved_path(Some(&root)),
            Some(target.clone()),
            "an absolute binding ignores the root"
        );
        drop(reloaded);

        // The next save under a project rebinds it, in place, to the same file.
        append(&mut draft, "another", "2026-08-07T14:40:00+02:00", &[0.1]);
        let report = save_in(&mut draft, &root, Path::new("unused.md"), None);
        assert_eq!(report.path, target);
        assert_eq!(draft.saved_path(), Some(Path::new("legacy.md")));
    }

    #[test]
    fn a_target_outside_the_notes_folder_stays_absolute() {
        // Should not happen — the target always comes from the project
        // — but a binding must never be *corrupted* into a relative path that
        // points at a different file.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        let elsewhere = tmp.path().join("elsewhere").join("note.md");
        let report = save_in(&mut draft, &root, &elsewhere, None);

        assert_eq!(report.path, elsewhere);
        assert_eq!(draft.saved_path(), Some(elsewhere.as_path()));
    }

    #[test]
    fn a_relative_binding_with_no_project_is_an_error_naming_the_draft() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("note.md"), None);

        // The project has been removed from the config: there is nothing to
        // resolve against, and the save must say so rather than write into the
        // process's working directory.
        let err = draft
            .save_target(Path::new("first.md"), None)
            .expect_err("unresolvable");
        match &err {
            DraftError::UnresolvedPath { path, relative } => {
                assert_eq!(path, draft.dir());
                assert_eq!(relative, Path::new("note.md"));
            }
            other => panic!("expected UnresolvedPath, got {other:?}"),
        }
        assert!(err.to_string().contains("note.md"), "{err}");
        // The display side is a blank, not an error: a tooltip has nothing true
        // to say about a path it cannot resolve.
        assert_eq!(draft.resolved_saved_path(None), None);
    }

    #[test]
    fn resolve_binding_is_the_one_rule_both_sides_use() {
        let root = Path::new("/notes/ludo");
        assert_eq!(
            resolve_binding(Path::new("note.md"), Some(root)),
            Some(root.join("note.md"))
        );
        assert_eq!(resolve_binding(Path::new("note.md"), None), None);
        // Absolute wins over any root, including a different one.
        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\elsewhere\note.md")
        } else {
            PathBuf::from("/elsewhere/note.md")
        };
        assert_eq!(
            resolve_binding(&absolute, Some(root)),
            Some(absolute.clone())
        );
        assert_eq!(resolve_binding(&absolute, None), Some(absolute));
    }

    // -----------------------------------------------------------------------
    // Renaming a note
    //
    // A note's display name *is* its file's basename, so a rename is one
    // `fs::rename` plus a re-derived binding. Every test here is about
    // invariant 4: what is on disk after, and what is refused before.
    // -----------------------------------------------------------------------

    #[test]
    fn renaming_a_note_moves_the_file_and_keeps_the_binding_relative() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("Ludo 2026-08-14.md"), None);

        let report = draft
            .rename_note("checkout rebuild", Some(&root))
            .expect("rename");

        assert!(report.moved);
        assert_eq!(report.from, root.join("Ludo 2026-08-14.md"));
        assert_eq!(report.to, root.join("checkout rebuild.md"));
        assert!(report.to.is_file(), "the renamed file is not there");
        assert!(!report.from.exists(), "the old name survived the rename");
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("checkout rebuild.md")),
            "a target inside the notes folder stays a relative binding"
        );
        // And it is on disk, not just on the handle.
        let reloaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(
            reloaded.draft.saved_path(),
            Some(Path::new("checkout rebuild.md"))
        );
    }

    #[test]
    fn renaming_a_note_bound_outside_the_notes_folder_stays_absolute() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        let elsewhere = tmp.path().join("elsewhere");
        save_in(&mut draft, &root, &elsewhere.join("note.md"), None);
        assert_eq!(
            draft.saved_path(),
            Some(elsewhere.join("note.md").as_path())
        );

        let report = draft
            .rename_note("moved out here", Some(&root))
            .expect("rename");

        assert_eq!(report.to, elsewhere.join("moved out here.md"));
        assert!(report.to.is_file());
        assert_eq!(
            draft.saved_path(),
            Some(elsewhere.join("moved out here.md").as_path()),
            "a binding outside the notes folder is never corrupted into a relative one"
        );
    }

    #[test]
    fn renaming_a_note_onto_an_existing_file_is_refused_and_moves_nothing() {
        // `fs::rename` replaces its destination on Windows and POSIX alike, so
        // this refusal is the whole of the "never destroy a note" guarantee for
        // this operation.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("mine.md"), None);

        let occupied = root.join("theirs.md");
        fs::write(&occupied, b"somebody else's words").expect("write the other note");

        let err = draft
            .rename_note("theirs", Some(&root))
            .expect_err("an occupied target");
        match &err {
            DraftError::NameTaken { path } => assert_eq!(path, &occupied),
            other => panic!("expected NameTaken, got {other:?}"),
        }

        assert_eq!(
            fs::read(&occupied).expect("read back"),
            b"somebody else's words",
            "the file in the way was overwritten"
        );
        assert!(root.join("mine.md").is_file(), "our own note moved anyway");
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("mine.md")),
            "a refused rename must not touch the binding"
        );
    }

    #[test]
    fn renaming_a_note_keeps_the_guard_hash_and_the_dirty_flag() {
        // A rename moves bytes and never changes them, so the external-edit
        // guard goes on working — at the new path — and a note that was behind
        // its file is still behind it.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("before.md"), None);
        let hash = draft.meta().last_save_hash.clone();
        assert!(hash.is_some());

        // Dirty again, the way a post-save edit leaves it.
        append(&mut draft, "another", "2026-08-14T14:40:00+02:00", &[0.1]);
        assert!(draft.dirty());

        draft.rename_note("after", Some(&root)).expect("rename");

        assert_eq!(draft.meta().last_save_hash, hash, "the hash was dropped");
        assert!(draft.dirty(), "the rename cleared the unsaved flag");

        // The guard still guards: an external edit at the *new* path is caught.
        fs::write(root.join("after.md"), b"someone else edited it").expect("edit");
        let target = draft
            .save_target(Path::new("unused.md"), Some(&root))
            .expect("a resolvable target");
        let err = draft
            .save_to(
                &target,
                SaveOptions {
                    notes_root: Some(&root),
                    ..SaveOptions::default()
                },
            )
            .expect_err("the guard should stop this");
        assert!(matches!(err, DraftError::SaveConflict { .. }), "{err:?}");
    }

    #[test]
    fn a_renamed_note_saves_to_its_new_file() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("before.md"), None);
        draft.rename_note("after", Some(&root)).expect("rename");

        append(&mut draft, "another", "2026-08-14T14:40:00+02:00", &[0.1]);
        let report = save_in(&mut draft, &root, Path::new("unused.md"), None);

        assert_eq!(report.path, root.join("after.md"));
        let written = fs::read_to_string(root.join("after.md")).expect("read back");
        assert!(written.contains("another"), "{written}");
        assert!(!root.join("before.md").exists());
    }

    #[test]
    fn a_rename_sanitizes_the_name_and_never_leaves_the_folder() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("before.md"), None);

        // Separators included: a rename renames, it never moves a note into
        // another folder, so the whole name collapses to one component.
        let report = draft
            .rename_note("../escape: 1/2", Some(&root))
            .expect("rename");

        assert_eq!(report.to.parent(), Some(root.as_path()));
        assert_eq!(
            report.to.file_name().and_then(OsStr::to_str),
            Some("..-escape- 1-2.md")
        );
        assert!(report.to.is_file());
    }

    #[test]
    fn a_rename_to_the_same_name_writes_nothing() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("same.md"), None);

        // Both spellings of "no change": with the extension and without it.
        for name in ["same", "same.md"] {
            let report = draft.rename_note(name, Some(&root)).expect("rename");
            assert!(!report.moved, "{name} was treated as a move");
            assert_eq!(report.from, report.to);
            assert!(root.join("same.md").is_file());
            assert_eq!(draft.saved_path(), Some(Path::new("same.md")));
        }
    }

    #[test]
    fn renaming_refuses_an_unbound_or_unresolvable_note() {
        let (tmp, store) = store();
        let root = tmp.path().join("notes");

        // Never saved: there is no file to rename and nothing to guess.
        let mut fresh = store.create_draft(None).expect("create");
        append(&mut fresh, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);
        let err = fresh
            .rename_note("anything", Some(&root))
            .expect_err("unbound");
        assert!(matches!(err, DraftError::NeverSaved { .. }), "{err:?}");

        // Bound relative, with the project gone from the config: refused rather
        // than renamed relative to the process's working directory.
        let mut bound = store.create_draft(None).expect("create");
        append(&mut bound, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);
        save_in(&mut bound, &root, &root.join("note.md"), None);
        let err = bound
            .rename_note("anything", None)
            .expect_err("unresolvable");
        match &err {
            DraftError::UnresolvedPath { path, relative } => {
                assert_eq!(path, bound.dir());
                assert_eq!(relative, Path::new("note.md"));
            }
            other => panic!("expected UnresolvedPath, got {other:?}"),
        }
        assert!(root.join("note.md").is_file(), "the note moved anyway");
    }

    #[test]
    fn a_relative_binding_rides_a_project_folder_rename() {
        // Renaming a project renames its folder, and every
        // relative binding inside it is still correct afterwards — by
        // construction, because the binding never named the folder.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);

        let old_root = tmp.path().join("Ludo");
        save_in(&mut draft, &old_root, &old_root.join("session 1.md"), None);
        assert_eq!(draft.saved_path(), Some(Path::new("session 1.md")));

        // The project is renamed: one folder rename, one config write, and no
        // per-note repair at all.
        let new_root = tmp.path().join("Checkout rebuild");
        fs::rename(&old_root, &new_root).expect("rename the project folder");

        assert_eq!(
            draft.resolved_saved_path(Some(&new_root)),
            Some(new_root.join("session 1.md")),
            "the binding did not follow its folder"
        );
        let report = save_in(&mut draft, &new_root, Path::new("unused.md"), None);
        assert_eq!(report.path, new_root.join("session 1.md"));
        assert!(!old_root.exists(), "the save resurrected the old folder");
    }

    #[test]
    fn set_project_rewrites_only_the_metadata() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        let record = append(&mut draft, "a finding", "2026-08-14T14:32:07+02:00", &[0.1]);
        let lines_before = fs::read(draft.lines_path()).expect("read the log");
        let audio = draft.dir().join(&record.audio);

        assert!(draft.set_project(Some("Checkout rebuild")).expect("sweep"));
        assert_eq!(draft.project(), Some("Checkout rebuild"));
        // A second pass over an already-swept draft writes nothing.
        assert!(!draft.set_project(Some("Checkout rebuild")).expect("sweep"));

        let reloaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(reloaded.draft.project(), Some("Checkout rebuild"));
        assert_eq!(
            fs::read(draft.lines_path()).expect("read the log"),
            lines_before,
            "the sweep touched the line log"
        );
        assert!(audio.is_file(), "the sweep touched the audio");
    }

    // -----------------------------------------------------------------------
    // Re-filing a note without touching disk
    //
    // These pin `reassign_project`, which is the *tag-and-
    // binding* half of a drop and not the whole of one — the cases where there
    // is nothing on disk to move. The point is still the binding: the stored
    // form has to be re-derived — resolved against the OLD project's folder,
    // re-derived against the NEW one — in the same meta write that moves the
    // project. Without it a later save would render the note into a folder it
    // has never been in, which is the quiet relocation invariant 4 forbids.
    // The drop itself is `moving_*`, below.
    // -----------------------------------------------------------------------

    #[test]
    fn reassigning_turns_a_relative_binding_absolute_and_the_file_stays_put() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-15T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);
        assert_eq!(draft.saved_path(), Some(Path::new("session 1.md")));

        assert!(draft
            .reassign_project(Some("Backlog"), Some(&ludo), Some(&backlog))
            .expect("reassign"));

        assert_eq!(draft.project(), Some("Backlog"));
        assert_eq!(
            draft.saved_path(),
            Some(ludo.join("session 1.md").as_path()),
            "the binding must name where the file actually is, not the new \
             project's folder"
        );
        assert!(
            ludo.join("session 1.md").is_file(),
            "the note's file moved on disk"
        );
        assert!(
            !backlog.join("session 1.md").exists(),
            "something was created in the new project's folder"
        );

        // On disk, and in one write: reopening finds both fields moved.
        let reloaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(reloaded.draft.project(), Some("Backlog"));
        assert_eq!(
            reloaded.draft.saved_path(),
            Some(ludo.join("session 1.md").as_path())
        );

        // And the save that follows rewrites the same file, in the old folder.
        append(&mut draft, "another", "2026-08-15T14:40:00+02:00", &[0.1]);
        let report = save_in(&mut draft, &backlog, Path::new("unused.md"), None);
        assert_eq!(report.path, ludo.join("session 1.md"));
    }

    #[test]
    fn reassigning_re_relativizes_a_note_that_lies_inside_the_new_folder() {
        // The other direction: the file happens to sit inside the project it is
        // being moved to, so the binding becomes relative to *that* folder and
        // the note rides a later folder move like any other.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-15T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        // Bound absolute, inside Backlog's folder, while it still belongs to
        // Ludo — the shape a hand-edited config or a moved note leaves.
        save_in(&mut draft, &ludo, &backlog.join("note.md"), None);
        assert_eq!(draft.saved_path(), Some(backlog.join("note.md").as_path()));

        assert!(draft
            .reassign_project(Some("Backlog"), Some(&ludo), Some(&backlog))
            .expect("reassign"));

        assert_eq!(
            draft.saved_path(),
            Some(Path::new("note.md")),
            "a file inside the new project's folder is bound relative to it"
        );
        assert!(backlog.join("note.md").is_file());
    }

    #[test]
    fn reassigning_an_unbound_draft_writes_only_the_project() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-15T14:32:07+02:00", &[0.1]);
        assert_eq!(draft.saved_path(), None);

        assert!(draft
            .reassign_project(None, None, None)
            .expect("reassign to no project"));
        assert_eq!(draft.project(), None);
        assert_eq!(draft.saved_path(), None, "a binding was invented");

        // A second pass over an already-filed draft writes nothing.
        assert!(!draft.reassign_project(None, None, None).expect("no-op"));
    }

    #[test]
    fn reassigning_out_of_a_missing_project_carries_an_unresolvable_binding() {
        // The repair case: the draft names a project the config no longer has,
        // so its relative binding cannot be resolved at all. Dragging it into a
        // real project must not be refused — it is how the user fixes it — and
        // the binding is carried across untouched rather than guessed at.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Gone")).expect("create");
        append(&mut draft, "a finding", "2026-08-15T14:32:07+02:00", &[0.1]);

        let gone = tmp.path().join("Gone");
        save_in(&mut draft, &gone, &gone.join("note.md"), None);
        assert_eq!(draft.saved_path(), Some(Path::new("note.md")));

        let backlog = tmp.path().join("Backlog");
        assert!(draft
            .reassign_project(Some("Backlog"), None, Some(&backlog))
            .expect("reassign"));

        assert_eq!(draft.project(), Some("Backlog"));
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("note.md")),
            "an unresolvable binding must be carried, never re-derived from a \
             root it was never relative to"
        );
        assert!(gone.join("note.md").is_file(), "the file moved");
    }

    // -----------------------------------------------------------------------
    // The drop: files move when you move them
    //
    // Every one of these is about
    // invariant 4 from a different side: what a move does to the file, what a
    // clash does *not* do to the file already there, and what a note whose file
    // is gone gets instead of an error.
    // -----------------------------------------------------------------------

    /// A drop as the window sends one: ask about a clash, no header.
    fn drop_into<'a>(old_root: &'a Path, new_root: &'a Path, project: &'a str) -> MoveOptions<'a> {
        MoveOptions {
            project: Some(project),
            old_root: Some(old_root),
            new_root: Some(new_root),
            ..MoveOptions::default()
        }
    }

    #[test]
    fn moving_a_note_carries_its_file_and_binds_relative_to_the_new_folder() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);
        let before = fs::read(ludo.join("session 1.md")).expect("read");

        let outcome = draft
            .move_to_project(drop_into(&ludo, &backlog, "Backlog"))
            .expect("the drop");
        assert_eq!(
            outcome,
            NoteMove::Moved {
                from: ludo.join("session 1.md"),
                to: backlog.join("session 1.md"),
                copied: false,
            }
        );

        // Gone from the source, there in the target, byte for byte.
        assert!(!ludo.join("session 1.md").exists(), "the original stayed");
        assert_eq!(
            fs::read(backlog.join("session 1.md")).expect("read"),
            before,
            "a move changed the bytes"
        );
        assert_eq!(draft.project(), Some("Backlog"));
        assert_eq!(
            draft.saved_path(),
            Some(Path::new("session 1.md")),
            "the binding is relative to the folder the file is in now"
        );

        // On disk and in one write: reopening finds both fields moved.
        let reloaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(reloaded.draft.project(), Some("Backlog"));
        assert_eq!(reloaded.draft.saved_path(), Some(Path::new("session 1.md")));

        // And Save rewrites the NEW path — the whole reason the binding moves.
        append(&mut draft, "another", "2026-08-19T14:40:00+02:00", &[0.1]);
        let report = save_in(&mut draft, &backlog, Path::new("unused.md"), None);
        assert_eq!(report.path, backlog.join("session 1.md"));
        assert!(
            !ludo.join("session 1.md").exists(),
            "the save resurrected the note in the old folder"
        );
    }

    #[test]
    fn the_external_edit_guard_survives_a_move() {
        // Where a reader would expect the hash to be rewritten. A rename moves
        // bytes without changing them, so the guard has to go on working at the
        // new path — and a file edited behind our back there must still stop a
        // save (invariant 4).
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);
        let hash_before = draft.meta().last_save_hash.clone();

        draft
            .move_to_project(drop_into(&ludo, &backlog, "Backlog"))
            .expect("the drop");
        assert_eq!(
            draft.meta().last_save_hash,
            hash_before,
            "the move rewrote the guard's hash"
        );

        // Untouched at the new path: the save goes through.
        append(&mut draft, "another", "2026-08-19T14:40:00+02:00", &[0.1]);
        draft
            .save_to(
                &backlog.join("session 1.md"),
                SaveOptions {
                    notes_root: Some(&backlog),
                    ..SaveOptions::default()
                },
            )
            .expect("save at the new path");

        // Edited at the new path: stopped, with nothing written.
        fs::write(backlog.join("session 1.md"), b"somebody typed this").expect("edit");
        let err = draft
            .save_to(
                &backlog.join("session 1.md"),
                SaveOptions {
                    notes_root: Some(&backlog),
                    ..SaveOptions::default()
                },
            )
            .expect_err("the guard");
        assert!(matches!(err, DraftError::SaveConflict { .. }), "{err:?}");
        assert_eq!(
            fs::read(backlog.join("session 1.md")).expect("read"),
            b"somebody typed this"
        );
    }

    #[test]
    fn a_clash_asks_and_keep_both_numbers_around_it() {
        // The invariant-4 heart of the drop: a taken name stops
        // the drop dead — nothing written, nothing tagged — and the only
        // resolution numbers the newcomer. There is no overwrite answer.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);
        fs::create_dir_all(&backlog).expect("target folder");
        fs::write(backlog.join("session 1.md"), b"someone else's note").expect("seed");

        let outcome = draft
            .move_to_project(drop_into(&ludo, &backlog, "Backlog"))
            .expect("a question, not a failure");
        assert_eq!(
            outcome,
            NoteMove::Clash {
                at: backlog.join("session 1.md"),
                free: Some(backlog.join("session 1 (2).md")),
            }
        );
        // Nothing at all happened: not the file, not the tag, not the binding.
        assert!(ludo.join("session 1.md").is_file(), "the file moved anyway");
        assert_eq!(draft.project(), Some("Ludo"));
        assert_eq!(draft.saved_path(), Some(Path::new("session 1.md")));
        assert_eq!(
            Draft::open(draft.dir()).expect("reopen").draft.project(),
            Some("Ludo"),
            "meta.json was written while a question was open"
        );

        // The answer.
        let outcome = draft
            .move_to_project(MoveOptions {
                clash: ClashChoice::KeepBoth,
                ..drop_into(&ludo, &backlog, "Backlog")
            })
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
            fs::read(backlog.join("session 1.md")).expect("read"),
            b"someone else's note",
            "keep both wrote over the note that was already there"
        );
        assert_eq!(draft.saved_path(), Some(Path::new("session 1 (2).md")));
    }

    #[test]
    fn dropping_a_missing_note_recreates_it_from_the_store() {
        // The note's file is unreachable — its binding is relative to
        // a project the config no longer has — so the drop renders a fresh one
        // from the lines the store still holds. Nothing stays missing after a
        // drop, and nothing that was on disk is touched.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Gone")).expect("create");
        append(
            &mut draft,
            "the fence clips",
            "2026-08-19T14:32:07+02:00",
            &[0.1],
        );

        let gone = tmp.path().join("Gone");
        save_in(&mut draft, &gone, &gone.join("note.md"), None);
        assert_eq!(draft.saved_path(), Some(Path::new("note.md")));

        // The project has left the config: no old root, so the binding cannot
        // be resolved at all.
        let backlog = tmp.path().join("Backlog");
        let outcome = draft
            .move_to_project(MoveOptions {
                project: Some("Backlog"),
                old_root: None,
                new_root: Some(&backlog),
                ..MoveOptions::default()
            })
            .expect("the rescue");
        assert_eq!(
            outcome,
            NoteMove::Recreated {
                to: backlog.join("note.md")
            }
        );

        assert_eq!(draft.project(), Some("Backlog"));
        assert_eq!(draft.saved_path(), Some(Path::new("note.md")));
        let written = fs::read_to_string(backlog.join("note.md")).expect("read");
        assert!(written.contains("the fence clips"), "{written}");
        // The old file — the one nothing could reach — is exactly where it was.
        assert!(gone.join("note.md").is_file(), "the old file was touched");

        // And it is a proper save: not dirty, hash recorded, so the guard works
        // at the new path from here on.
        assert!(!draft.dirty());
        assert!(draft.meta().last_save_hash.is_some());
    }

    #[test]
    fn a_note_whose_file_was_deleted_is_recreated_rather_than_refused() {
        // The same rescue, one step less broken: the binding resolves, there is
        // simply nothing at the end of it. Refusing here would strand a note
        // whose file the user deleted in a file manager.
        let (tmp, store) = store();
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);

        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);
        fs::remove_file(ludo.join("session 1.md")).expect("the user deleted it");

        let outcome = draft
            .move_to_project(drop_into(&ludo, &backlog, "Backlog"))
            .expect("the drop");
        assert_eq!(
            outcome,
            NoteMove::Recreated {
                to: backlog.join("session 1.md")
            }
        );
        assert!(backlog.join("session 1.md").is_file());
    }

    #[test]
    fn an_unbound_draft_and_a_drop_into_no_project_only_retag() {
        // There is nothing to move in either case, so nothing on disk moves.
        let (tmp, store) = store();
        let ludo = tmp.path().join("Ludo");
        let backlog = tmp.path().join("Backlog");

        let mut unsaved = store.create_draft(Some("Ludo")).expect("create");
        append(
            &mut unsaved,
            "a finding",
            "2026-08-19T14:32:07+02:00",
            &[0.1],
        );
        assert_eq!(
            unsaved
                .move_to_project(drop_into(&ludo, &backlog, "Backlog"))
                .expect("retag"),
            NoteMove::Retagged
        );
        assert_eq!(unsaved.project(), Some("Backlog"));
        assert_eq!(unsaved.saved_path(), None, "a binding was invented");
        assert!(!backlog.exists(), "the target folder was created anyway");

        // And out of every project: there is no folder to move into.
        let mut bound = store.create_draft(Some("Ludo")).expect("create");
        append(&mut bound, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);
        save_in(&mut bound, &ludo, &ludo.join("session 1.md"), None);
        assert_eq!(
            bound
                .move_to_project(MoveOptions {
                    project: None,
                    old_root: Some(&ludo),
                    new_root: None,
                    ..MoveOptions::default()
                })
                .expect("retag"),
            NoteMove::Retagged
        );
        assert_eq!(bound.project(), None);
        assert!(
            ludo.join("session 1.md").is_file(),
            "the file moved with no folder to move it to"
        );
        assert_eq!(
            bound.saved_path(),
            Some(ludo.join("session 1.md").as_path()),
            "the binding must name where the file actually is"
        );
    }

    #[test]
    fn dropping_a_note_into_the_folder_it_is_already_in_moves_no_file() {
        // Two projects sharing one folder. `fs::rename` onto itself is not a
        // gesture worth risking, and there is nothing to move.
        let (tmp, store) = store();
        let shared = tmp.path().join("shared");
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);
        save_in(&mut draft, &shared, &shared.join("session 1.md"), None);

        assert_eq!(
            draft
                .move_to_project(drop_into(&shared, &shared, "Backlog"))
                .expect("retag"),
            NoteMove::Retagged
        );
        assert_eq!(draft.project(), Some("Backlog"));
        assert_eq!(draft.saved_path(), Some(Path::new("session 1.md")));
        assert!(shared.join("session 1.md").is_file());
    }

    #[test]
    fn a_drop_onto_the_project_a_note_is_already_in_writes_nothing() {
        let (tmp, store) = store();
        let ludo = tmp.path().join("Ludo");
        let mut draft = store.create_draft(Some("Ludo")).expect("create");
        append(&mut draft, "a finding", "2026-08-19T14:32:07+02:00", &[0.1]);
        save_in(&mut draft, &ludo, &ludo.join("session 1.md"), None);

        assert_eq!(
            draft
                .move_to_project(drop_into(&ludo, &ludo, "Ludo"))
                .expect("no-op"),
            NoteMove::Unchanged
        );
    }

    #[test]
    fn the_cross_volume_fallback_verifies_before_it_deletes_anything() {
        // The copy path, exercised directly: a unit test cannot conjure a
        // second volume, and this is the ordering that makes a cross-volume
        // move lossless (invariant 4) — copy, fsync, read back, compare, and
        // only then remove the original.
        let dir = tempfile::tempdir().expect("temp dir");
        let from = dir.path().join("here").join("session 1.md");
        let to = dir.path().join("there").join("session 1.md");
        fs::create_dir_all(from.parent().expect("parent")).expect("source folder");
        fs::create_dir_all(to.parent().expect("parent")).expect("target folder");
        fs::write(&from, b"- 14:32 the fence clips\n").expect("seed");

        move_across_volumes(&from, &to).expect("the fallback");
        assert!(!from.exists(), "the original outlived a verified move");
        assert_eq!(
            fs::read(&to).expect("read"),
            b"- 14:32 the fence clips\n",
            "the copy is not the original"
        );
        // No temp file left behind: the copy goes through `write_atomic`.
        let leftovers: Vec<_> = fs::read_dir(to.parent().expect("parent"))
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "session 1.md")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_cross_volume_move_that_cannot_write_leaves_the_original_alone() {
        // The failure direction, which is the one that matters: the target
        // folder is a *file*, so the copy cannot be written — and the note is
        // still exactly where it was.
        let dir = tempfile::tempdir().expect("temp dir");
        let from = dir.path().join("session 1.md");
        fs::write(&from, b"- 14:32 the fence clips\n").expect("seed");
        let blocked = dir.path().join("not a folder");
        fs::write(&blocked, b"i am a file").expect("seed");

        let err = move_across_volumes(&from, &blocked.join("session 1.md"))
            .expect_err("nowhere to copy to");
        assert!(matches!(err, DraftError::Io { .. }), "{err:?}");
        assert_eq!(
            fs::read(&from).expect("read"),
            b"- 14:32 the fence clips\n",
            "the original was removed after a failed copy"
        );
    }

    #[test]
    fn a_case_only_rename_is_not_an_occupied_target() {
        // On Windows `to.exists()` finds the *source* through
        // the case-insensitive lookup, and refusing that made "Ludo.md" →
        // "ludo.md" impossible. On a case-sensitive filesystem the target
        // simply does not exist and the rename was always allowed — the same
        // assertions hold either way.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "a finding", "2026-08-15T14:32:07+02:00", &[0.1]);

        let root = tmp.path().join("notes");
        save_in(&mut draft, &root, &root.join("Ludo.md"), None);

        let report = draft.rename_note("ludo", Some(&root)).expect("rename");
        assert!(report.moved);
        assert_eq!(report.to, root.join("ludo.md"));
        assert_eq!(draft.saved_path(), Some(Path::new("ludo.md")));
        assert!(report.to.is_file(), "the renamed file is not there");
        // The content is the note's own, not something that was sitting there:
        // nothing was overwritten, because there was nothing else to overwrite.
        let written = fs::read_to_string(&report.to).expect("read back");
        assert!(written.contains("a finding"), "{written}");

        // And the refusal still stands for a target that is a different file.
        let occupied = root.join("theirs.md");
        fs::write(&occupied, b"somebody else's words").expect("write");
        let err = draft
            .rename_note("theirs", Some(&root))
            .expect_err("an occupied target");
        assert!(matches!(err, DraftError::NameTaken { .. }), "{err:?}");
        assert_eq!(
            fs::read(&occupied).expect("read back"),
            b"somebody else's words"
        );
    }

    #[test]
    fn a_save_adopts_a_projectless_draft_and_never_reassigns_one() {
        let (tmp, store) = store();
        let root = tmp.path().join("notes");

        // Dictated before any project existed: the save adopts it.
        let mut orphan = store.create_draft(None).expect("create");
        append(
            &mut orphan,
            "a finding",
            "2026-08-07T14:32:07+02:00",
            &[0.1],
        );
        assert_eq!(orphan.meta().project, None);
        save_in(&mut orphan, &root, &root.join("orphan.md"), Some("Ludo"));
        assert_eq!(orphan.meta().project.as_deref(), Some("Ludo"));
        // And it is on disk, in the same write as the binding.
        let reloaded = Draft::open(orphan.dir()).expect("reopen");
        assert_eq!(reloaded.draft.meta().project.as_deref(), Some("Ludo"));

        // A draft that already belongs somewhere is never moved by a save.
        let mut owned = store.create_draft(Some("Spreadsheet")).expect("create");
        append(&mut owned, "a finding", "2026-08-07T14:32:07+02:00", &[0.1]);
        save_in(&mut owned, &root, &root.join("owned.md"), Some("Ludo"));
        assert_eq!(owned.meta().project.as_deref(), Some("Spreadsheet"));

        // No adoption offered, no project invented.
        let mut still_none = store.create_draft(None).expect("create");
        append(
            &mut still_none,
            "a finding",
            "2026-08-07T14:32:07+02:00",
            &[0.1],
        );
        save_in(&mut still_none, &root, &root.join("none.md"), None);
        assert_eq!(still_none.meta().project, None);
    }

    #[test]
    fn saving_renders_the_lines_that_are_on_disk_now() {
        // An edit lands in `lines.jsonl` behind the handle's back; the save
        // must read the log rather than anything it remembers.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(
            &mut draft,
            "as transcribed",
            "2026-08-04T14:32:07+02:00",
            &[0.1],
        );
        append(
            &mut draft,
            "to be deleted",
            "2026-08-04T14:33:00+02:00",
            &[0.1],
        );

        let loaded = Draft::open(draft.dir()).expect("open");
        let mut edited = loaded.lines;
        edited[0].text = "as edited by the user".to_owned();
        edited[0].original = Some("as transcribed".to_owned());
        edited[1].deleted = true;
        let mut rewritten = Vec::new();
        for line in &edited {
            rewritten.extend_from_slice(&serde_json::to_vec(line).expect("encode"));
            rewritten.push(b'\n');
        }
        fs::write(draft.lines_path(), &rewritten).expect("rewrite lines");

        let target = tmp.path().join("notes.md");
        let report = draft.save_as(&target, None).expect("save");

        assert_eq!(report.lines, 1);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 14:32:07 — as edited by the user\n"
        );
    }

    #[test]
    fn saving_an_empty_draft_writes_an_empty_file_rather_than_failing() {
        // Rendering is total; whether to offer this is the UI's problem.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");

        let target = tmp.path().join("empty.md");
        let report = draft.save_as(&target, None).expect("save");

        assert_eq!(report.lines, 0);
        assert_eq!(report.bytes, 0);
        assert_eq!(fs::read_to_string(&target).expect("read"), "");
    }

    // -----------------------------------------------------------------------
    // Edits. The whole point of these is invariant 4: `lines.jsonl` only ever
    // grows, and every one of these tests checks the bytes as well as the fold.
    // -----------------------------------------------------------------------

    #[test]
    fn an_edit_is_appended_and_folds_over_the_base_line() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(
            &mut draft,
            "the mennu does nothing",
            "2026-08-05T10:00:00+02:00",
            &[0.1],
        );
        let before = fs::read(draft.lines_path()).expect("read lines");

        let edited = draft
            .edit_line(&spoken.id, "the menu does nothing")
            .expect("edit");

        assert_eq!(edited.text, "the menu does nothing");
        assert_eq!(edited.original.as_deref(), Some("the mennu does nothing"));
        assert_eq!(
            edited.spoken_at, spoken.spoken_at,
            "an edit is not a re-stamp"
        );
        assert_eq!(edited.audio, spoken.audio, "the wav is untouched");
        assert!(draft.dir().join(&spoken.audio).is_file());

        // Appended, never rewritten: the base record's bytes are still there,
        // in front, exactly as they were.
        let after = fs::read(draft.lines_path()).expect("read lines");
        assert!(after.starts_with(&before), "lines.jsonl was rewritten");
        assert!(after.len() > before.len());

        // And a re-read agrees with what the edit returned.
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(loaded.lines, vec![edited]);
        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(loaded.draft.line_count(), 1);
    }

    #[test]
    fn the_last_edit_of_a_field_wins_and_original_is_always_the_base_text() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(
            &mut draft,
            "first pass",
            "2026-08-05T10:00:00+02:00",
            &[0.1],
        );

        draft.edit_line(&spoken.id, "second pass").expect("edit");
        draft.edit_line(&spoken.id, "third pass").expect("edit");

        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(loaded.lines[0].text, "third pass");
        // Not "second pass": the base record is the original, forever.
        assert_eq!(loaded.lines[0].original.as_deref(), Some("first pass"));
        // Three records for one line, all of them still on disk.
        let raw = fs::read_to_string(draft.lines_path()).expect("read");
        assert_eq!(raw.lines().filter(|l| !l.trim().is_empty()).count(), 3);
    }

    #[test]
    fn editing_back_to_the_spoken_text_clears_original() {
        // The Undo path: reverting a line must leave it indistinguishable from
        // one that was never touched, even though two edits are on disk.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(&mut draft, "as spoken", "2026-08-05T10:00:00+02:00", &[0.1]);

        draft.edit_line(&spoken.id, "as tidied").expect("edit");
        let reverted = draft.edit_line(&spoken.id, "as spoken").expect("revert");

        assert_eq!(reverted.text, "as spoken");
        assert!(reverted.original.is_none());
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(loaded.lines[0].text, "as spoken");
        assert!(loaded.lines[0].original.is_none());
    }

    #[test]
    fn a_no_op_edit_writes_nothing() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(&mut draft, "unchanged", "2026-08-05T10:00:00+02:00", &[0.1]);
        let before = fs::read(draft.lines_path()).expect("read");

        let same = draft
            .edit_line(&spoken.id, "unchanged")
            .expect("no-op edit");
        assert_eq!(same, spoken);
        // Deleting something that is already deleted is the same non-event.
        draft.set_deleted(&spoken.id, false).expect("no-op delete");

        assert_eq!(
            fs::read(draft.lines_path()).expect("read"),
            before,
            "a no-op edit grew the file"
        );
    }

    #[test]
    fn a_soft_delete_hides_the_line_and_destroys_nothing() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "keep me", "2026-08-05T10:00:00+02:00", &[0.1]);
        let gone = append(&mut draft, "drop me", "2026-08-05T10:00:05+02:00", &[0.2]);
        let audio = draft.dir().join(&gone.audio);

        let deleted = draft.set_deleted(&gone.id, true).expect("delete");
        assert!(deleted.deleted);
        assert_eq!(draft.line_count(), 1);

        // The record, the text and the wav are all still there.
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(loaded.lines.len(), 2);
        assert_eq!(loaded.lines[1].text, "drop me");
        assert!(loaded.lines[1].deleted);
        assert!(audio.is_file(), "the audio of a deleted line was removed");
        assert_eq!(loaded.draft.line_count(), 1);

        // It simply stops being rendered (the save needed no change for this).
        let target = tmp.path().join("notes.md");
        let report = draft.save_as(&target, None).expect("save");
        assert_eq!(report.lines, 1);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 10:00:00 — keep me\n"
        );

        // Undelete brings it straight back, count and all.
        draft.set_deleted(&gone.id, false).expect("undelete");
        assert_eq!(draft.line_count(), 2);
        assert!(!Draft::open(draft.dir()).expect("reopen").lines[1].deleted);
    }

    #[test]
    fn a_save_renders_the_folded_text_of_edited_lines() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let first = append(
            &mut draft,
            "as transcribbed",
            "2026-08-05T14:32:07+02:00",
            &[0.1],
        );
        let second = append(
            &mut draft,
            "to be deleted",
            "2026-08-05T14:33:00+02:00",
            &[0.1],
        );

        draft
            .edit_line(&first.id, "as edited by the user")
            .expect("edit");
        draft.set_deleted(&second.id, true).expect("delete");

        let target = tmp.path().join("notes.md");
        let report = draft.save_as(&target, None).expect("save");

        assert_eq!(report.lines, 1);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 14:32:07 — as edited by the user\n"
        );
    }

    #[test]
    fn an_edit_marks_the_draft_dirty_again() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let line = append(&mut draft, "one", "2026-08-05T10:00:00+02:00", &[0.1]);
        draft
            .save_as(&tmp.path().join("notes.md"), None)
            .expect("save");
        assert!(!draft.dirty());

        draft.edit_line(&line.id, "one, tidied").expect("edit");

        assert!(draft.dirty());
        assert!(Draft::open(draft.dir()).expect("reopen").draft.meta().dirty);
    }

    #[test]
    fn editing_an_unknown_line_names_the_draft_and_the_id() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(
            &mut draft,
            "the only line",
            "2026-08-05T10:00:00+02:00",
            &[0.1],
        );
        let before = fs::read(draft.lines_path()).expect("read");

        match draft.edit_line("01JNOSUCHLINE", "nope") {
            Err(DraftError::NoSuchLine { id, path }) => {
                assert_eq!(id, "01JNOSUCHLINE");
                assert_eq!(path, draft.dir());
            }
            other => panic!("expected NoSuchLine, got {other:?}"),
        }
        assert!(matches!(
            draft.set_deleted("01JNOSUCHLINE", true),
            Err(DraftError::NoSuchLine { .. })
        ));
        assert_eq!(fs::read(draft.lines_path()).expect("read"), before);
    }

    #[test]
    fn an_edit_of_a_line_that_is_not_in_the_file_is_counted_and_kept() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(
            &mut draft,
            "the real line",
            "2026-08-05T10:00:00+02:00",
            &[0.1],
        );
        let dir = draft.dir().to_path_buf();
        drop(draft);

        // An edit whose base record is not here — a copied file, a hand-edit, a
        // line whose record was itself mangled.
        let orphan = br#"{"edit_of":"01JGHOST","at":"2026-08-05T10:01:00+02:00","text":"nowhere"}"#;
        let lines_path = dir.join(LINES_FILE);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&lines_path)
            .expect("open");
        file.write_all(orphan).expect("write");
        file.write_all(b"\n").expect("newline");
        drop(file);

        let loaded = Draft::open(&dir).expect("open");
        assert_eq!(loaded.lines.len(), 1);
        assert_eq!(loaded.lines[0].text, "the real line");
        assert_eq!(loaded.skipped_lines, 1);
        // Skipped means skipped, not deleted.
        let bytes = fs::read(&lines_path).expect("read");
        assert!(
            bytes.windows(orphan.len()).any(|w| w == orphan),
            "the orphan edit was removed from the file"
        );
    }

    #[test]
    fn a_draft_written_before_edits_existed_loads_byte_for_byte_the_same() {
        // No migration, no version bump: a file with only LineRecords in it has
        // to parse exactly as it did before this feature existed.
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00"}"#,
        )
        .expect("seed meta");
        let raw = b"{\"id\":\"01JLINEA\",\"spoken_at\":\"2026-08-03T10:00:00+02:00\",\
                    \"text\":\"first\",\"audio\":\"audio/01JLINEA.wav\"}\n\
                    {\"id\":\"01JLINEB\",\"spoken_at\":\"2026-08-03T10:00:05+02:00\",\
                    \"text\":\"second\",\"deleted\":true,\"audio\":\"audio/01JLINEB.wav\"}\n";
        fs::write(dir.join(LINES_FILE), raw).expect("seed lines");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(loaded.lines.len(), 2);
        assert_eq!(loaded.lines[0].text, "first");
        assert!(loaded.lines[0].original.is_none());
        assert!(loaded.lines[1].deleted);
        assert_eq!(loaded.draft.line_count(), 1);
        assert_eq!(fs::read(dir.join(LINES_FILE)).expect("read"), raw);
    }

    #[test]
    fn a_transcribe_ms_edit_records_the_duration_without_re_stamping_the_line() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(&mut draft, "mumbled", "2026-08-05T10:00:00+02:00", &[0.1]);

        let redone = draft
            .retranscribed_line(&spoken.id, "clear now", Some(730))
            .expect("retranscribe");

        assert_eq!(redone.text, "clear now");
        assert_eq!(redone.transcribe_ms, Some(730));
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(loaded.lines[0].transcribe_ms, Some(730));
        assert_eq!(loaded.lines[0].original.as_deref(), Some("mumbled"));
    }

    // -----------------------------------------------------------------------
    // Failed lines. A line the model refused keeps its audio, is
    // left out of the markdown until it has words, and is resolved by anything
    // that gives it some.
    // -----------------------------------------------------------------------

    #[test]
    fn a_failed_line_is_written_with_no_words_and_all_of_its_audio() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let samples: Vec<f32> = (0..1_600).map(|i| ((i as f32) / 800.0) - 1.0).collect();
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &samples);

        assert!(record.failed);
        assert!(record.text.is_empty());
        assert_eq!(record.transcribe_ms, None);
        // The whole point: the wav is there, exactly as an ordinary line's is.
        let wav = draft.line_audio_path(&record.id).expect("path");
        assert!(wav.is_file(), "a failed line kept no audio");
        // And it is not in the note, because it has nothing to say yet.
        assert_eq!(draft.line_count(), 0);
        assert_eq!(
            draft
                .preview_markdown(None, SessionDividers::Shown)
                .unwrap(),
            ""
        );
    }

    #[test]
    fn the_failed_flag_is_only_written_when_it_is_true() {
        // Old drafts have to stay byte-comparable, so an ordinary line must not
        // grow a `"failed":false` it never had.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "ordinary", "2026-08-12T10:00:00+02:00", &[0.1]);
        let ok = fs::read_to_string(draft.lines_path()).expect("read");
        assert!(!ok.contains("failed"), "{ok}");

        append_failed(&mut draft, "2026-08-12T10:00:05+02:00", &[0.1]);
        let both = fs::read_to_string(draft.lines_path()).expect("read");
        assert!(both.contains("\"failed\":true"), "{both}");
    }

    #[test]
    fn a_draft_written_before_failed_lines_existed_parses_and_renders_unchanged() {
        // The migration story: no version bump, no rewrite, and every line in a
        // file that predates the field reads `failed: false`.
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00"}"#,
        )
        .expect("seed meta");
        let raw = b"{\"id\":\"01JLINEA\",\"spoken_at\":\"2026-08-03T10:00:00+02:00\",\
                    \"text\":\"first\",\"audio\":\"audio/01JLINEA.wav\"}\n\
                    {\"id\":\"01JLINEB\",\"spoken_at\":\"2026-08-03T10:00:05+02:00\",\
                    \"text\":\"second\",\"audio\":\"audio/01JLINEB.wav\"}\n";
        fs::write(dir.join(LINES_FILE), raw).expect("seed lines");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.skipped_lines, 0);
        assert!(loaded.lines.iter().all(|line| !line.failed));
        assert_eq!(loaded.draft.line_count(), 2);
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 10:00:00 — first\n- 10:00:05 — second\n"
        );
        assert_eq!(fs::read(dir.join(LINES_FILE)).expect("read"), raw);
    }

    #[test]
    fn a_retry_that_produced_words_clears_the_failure_structurally() {
        // The fold rule: no second field on `EditRecord`, and no way for the
        // text and the state to disagree — a line with words is not failed.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &[0.2]);

        let redone = draft
            .retranscribed_line(&record.id, "the fence has no collision", Some(310))
            .expect("retranscribe");
        assert!(!redone.failed);
        // No "originally: " in the audit trail: the model never produced words
        // for this line, so there is no original to keep.
        assert_eq!(redone.original, None);

        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(!loaded.lines[0].failed);
        assert_eq!(loaded.lines[0].text, "the fence has no collision");
        assert_eq!(loaded.lines[0].original, None);
        assert_eq!(loaded.draft.line_count(), 1);
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 10:00:00 — the fence has no collision\n"
        );
    }

    #[test]
    fn typing_the_words_resolves_a_failed_line_exactly_as_a_retry_does() {
        // The manual resolve. It matters that this is the *same* path: a user
        // who types their finding must never be able to produce a line the
        // renderer then drops (invariant 4).
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &[0.2]);

        draft
            .edit_line(&record.id, "I said it myself")
            .expect("edit");

        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(!loaded.lines[0].failed);
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 10:00:00 — I said it myself\n"
        );
    }

    #[test]
    fn emptying_a_resolved_line_returns_it_to_the_failed_state() {
        // The undo of a typed resolve replays the inverse `line_edit`
        // straight through the command — it never passes the window's
        // empty-text guard — so if the fold only cleared `failed` one way, an
        // undo would leave an *empty ok* line: an empty bullet in the note,
        // with the failure and its Retry gone for good.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &[0.2]);

        draft
            .edit_line(&record.id, "I said it myself")
            .expect("edit");
        assert_eq!(draft.line_count(), 1);

        let back = draft.edit_line(&record.id, "").expect("undo the resolve");
        assert!(back.failed, "the line stopped being a failure permanently");
        assert!(back.text.is_empty());
        assert_eq!(draft.line_count(), 0, "and it left the note again");

        // The single-record fold above has to agree with a whole re-read.
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(loaded.lines[0].failed);
        assert_eq!(loaded.lines[0].original, None);
        assert_eq!(loaded.draft.line_count(), 0);
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "",
            "an undone resolve must not leave an empty bullet"
        );

        // And redo puts it back, so the round trip is symmetric both ways.
        draft
            .edit_line(&record.id, "I said it myself")
            .expect("redo");
        assert_eq!(draft.line_count(), 1);
        assert_eq!(
            Draft::open(draft.dir())
                .expect("reopen")
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 10:00:00 — I said it myself\n"
        );
    }

    #[test]
    fn an_empty_edit_of_an_unresolved_failed_line_leaves_it_exactly_as_it_was() {
        // Nothing to change: the text is already empty, so the no-op guard
        // writes no record and the state stands.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &[0.2]);
        let before = fs::read(draft.lines_path()).expect("read");

        let same = draft.edit_line(&record.id, "").expect("edit");

        assert!(same.failed);
        assert_eq!(draft.line_count(), 0);
        assert_eq!(fs::read(draft.lines_path()).expect("read"), before);
    }

    #[test]
    fn an_ordinary_line_edited_to_empty_is_still_an_ordinary_line() {
        // The other half of the symmetry: `failed` is the *base record's* flag
        // and nothing else can set it, so emptying a line the model
        // transcribed leaves an empty `ok` line — pre-existing behaviour, and
        // deliberately left alone.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(
            &mut draft,
            "said something",
            "2026-08-12T10:00:00+02:00",
            &[0.1],
        );

        let emptied = draft.edit_line(&spoken.id, "").expect("edit");

        assert!(!emptied.failed);
        assert_eq!(emptied.original.as_deref(), Some("said something"));
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(!loaded.lines[0].failed);
        assert_eq!(loaded.draft.line_count(), 1);
    }

    #[test]
    fn deleting_a_failed_line_leaves_it_failed_underneath() {
        // A delete says nothing about the text, so the state stands; what
        // changes is only which display wins, which is the window's business.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let record = append_failed(&mut draft, "2026-08-12T10:00:00+02:00", &[0.2]);

        draft.set_deleted(&record.id, true).expect("delete");

        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(loaded.lines[0].failed, "a delete must not resolve it");
        assert!(loaded.lines[0].deleted);
        // Excluded once for either reason; never a half-rendered bullet.
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            ""
        );
    }

    #[test]
    fn a_re_transcribe_of_an_ordinary_line_behaves_exactly_as_it_always_did() {
        // The regression guard on the fold: `failed` starts false, stays false,
        // and `original` still reports what was first said.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(&mut draft, "mumbled", "2026-08-12T10:00:00+02:00", &[0.1]);

        draft
            .retranscribed_line(&spoken.id, "clear now", Some(90))
            .expect("retranscribe");

        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert!(!loaded.lines[0].failed);
        assert_eq!(loaded.lines[0].text, "clear now");
        assert_eq!(loaded.lines[0].original.as_deref(), Some("mumbled"));
        assert_eq!(loaded.draft.line_count(), 1);
    }

    // -----------------------------------------------------------------------
    // Moves. Same contract as edits: `lines.jsonl` only grows, and
    // a move that cannot be folded leaves its line exactly where it was.
    // -----------------------------------------------------------------------

    /// The transcript as ids, which is all an ordering test cares about.
    fn order_of(draft: &Draft) -> Vec<String> {
        draft
            .read_lines()
            .expect("read")
            .into_iter()
            .map(|line| line.id)
            .collect()
    }

    /// Three lines, so "to the middle" is a distinct destination.
    fn three_lines(draft: &mut Draft) -> (String, String, String) {
        let a = append(draft, "first", "2026-08-05T10:00:00+02:00", &[0.1]);
        let b = append(draft, "second", "2026-08-05T10:00:05+02:00", &[0.1]);
        let c = append(draft, "third", "2026-08-05T10:00:09+02:00", &[0.1]);
        (a.id, b.id, c.id)
    }

    #[test]
    fn a_move_is_appended_and_folds_to_the_new_order() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, b, c) = three_lines(&mut draft);
        let before = fs::read(draft.lines_path()).expect("read lines");

        // "third" belongs right after "first".
        let folded = draft.move_line(&c, Some(&a)).expect("move");
        let ids: Vec<&str> = folded.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(ids, vec![a.as_str(), c.as_str(), b.as_str()]);

        // Appended, never rewritten.
        let after = fs::read(draft.lines_path()).expect("read lines");
        assert!(after.starts_with(&before), "lines.jsonl was rewritten");

        // And a re-read folds to the same thing, with every line intact.
        let loaded = Draft::open(draft.dir()).expect("reopen");
        assert_eq!(order_of(&loaded.draft), vec![a.clone(), c.clone(), b]);
        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(loaded.draft.line_count(), 3, "a move is not a delete");

        // Moves fold sequentially: the second one starts from the first's result.
        draft.move_line(&a, Some(&c)).expect("second move");
        assert_eq!(order_of(&draft)[0], c);
    }

    #[test]
    fn a_move_with_a_null_anchor_goes_to_the_top() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, b, c) = three_lines(&mut draft);

        let folded = draft.move_line(&c, None).expect("move to top");
        let ids: Vec<&str> = folded.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(ids, vec![c.as_str(), a.as_str(), b.as_str()]);
        assert_eq!(order_of(&draft), vec![c, a, b]);

        // `null` is written out, not omitted: absent must never be readable as
        // "to the top".
        let raw = fs::read_to_string(draft.lines_path()).expect("read");
        let last = raw.lines().last().expect("a record");
        assert!(last.contains(r#""after":null"#), "{last}");
    }

    #[test]
    fn a_no_op_move_writes_nothing() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, b, _c) = three_lines(&mut draft);
        let before = fs::read(draft.lines_path()).expect("read");

        // Already at the top; already directly after `a`; and after itself.
        draft.move_line(&a, None).expect("already at the top");
        draft.move_line(&b, Some(&a)).expect("already there");
        draft.move_line(&b, Some(&b)).expect("after itself");

        assert_eq!(
            fs::read(draft.lines_path()).expect("read"),
            before,
            "a no-op move grew the file"
        );
    }

    #[test]
    fn moving_an_unknown_line_or_to_an_unknown_anchor_names_the_missing_id() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, _b, _c) = three_lines(&mut draft);
        let before = fs::read(draft.lines_path()).expect("read");

        match draft.move_line("01JNOSUCHLINE", Some(&a)) {
            Err(DraftError::NoSuchLine { id, path }) => {
                assert_eq!(id, "01JNOSUCHLINE");
                assert_eq!(path, draft.dir());
            }
            other => panic!("expected NoSuchLine, got {other:?}"),
        }
        match draft.move_line(&a, Some("01JNOSUCHANCHOR")) {
            Err(DraftError::NoSuchLine { id, .. }) => assert_eq!(id, "01JNOSUCHANCHOR"),
            other => panic!("expected NoSuchLine, got {other:?}"),
        }
        assert_eq!(fs::read(draft.lines_path()).expect("read"), before);
    }

    #[test]
    fn a_soft_deleted_line_is_a_valid_anchor_and_holds_its_place() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, b, c) = three_lines(&mut draft);

        draft.set_deleted(&b, true).expect("delete the middle");
        // Deleted lines still hold a place in the transcript, so they can be
        // moved and anchored to.
        draft.move_line(&c, Some(&b)).expect("move after a deleted");
        assert_eq!(order_of(&draft), vec![a.clone(), b.clone(), c.clone()]);

        draft.move_line(&b, None).expect("move the deleted one");
        assert_eq!(order_of(&draft), vec![b, a, c]);
        assert_eq!(draft.line_count(), 2, "a move never changes the live count");
    }

    #[test]
    fn an_orphan_or_badly_anchored_move_is_counted_and_kept() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, b, _c) = three_lines(&mut draft);
        let dir = draft.dir().to_path_buf();
        drop(draft);

        // Three moves a hand-edit could produce: no such line, no such anchor,
        // and a line after itself. None of them may rearrange anything.
        let orphans = [
            format!(r#"{{"move_of":"01JGHOST","at":"2026-08-05T10:01:00+02:00","after":"{a}"}}"#),
            format!(r#"{{"move_of":"{a}","at":"2026-08-05T10:01:00+02:00","after":"01JGHOST"}}"#),
            format!(r#"{{"move_of":"{b}","at":"2026-08-05T10:01:00+02:00","after":"{b}"}}"#),
            // No `after` at all: required-but-nullable, so this is malformed.
            format!(r#"{{"move_of":"{b}","at":"2026-08-05T10:01:00+02:00"}}"#),
        ];
        let lines_path = dir.join(LINES_FILE);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&lines_path)
            .expect("open");
        for orphan in &orphans {
            file.write_all(orphan.as_bytes()).expect("write");
            file.write_all(b"\n").expect("newline");
        }
        drop(file);

        let loaded = Draft::open(&dir).expect("open");
        let ids: Vec<&str> = loaded.lines.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], a);
        assert_eq!(ids[1], b);
        // The self-move is a harmless nothing; the other three are skips. A
        // record missing `after` is malformed, not "to the top".
        assert_eq!(loaded.skipped_lines, 3);

        // Skipped means skipped, not deleted.
        let bytes = fs::read(&lines_path).expect("read");
        for orphan in &orphans {
            assert!(
                bytes
                    .windows(orphan.len())
                    .any(|window| window == orphan.as_bytes()),
                "a skipped move was removed from the file: {orphan}"
            );
        }
    }

    #[test]
    fn a_draft_written_before_moves_existed_folds_in_file_order() {
        // Old files parse byte-for-byte identically: the move fold is the
        // identity permutation when there is nothing to fold.
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00"}"#,
        )
        .expect("seed meta");
        let raw = b"{\"id\":\"01JLINEA\",\"spoken_at\":\"2026-08-03T10:00:00+02:00\",\
                    \"text\":\"first\",\"audio\":\"audio/01JLINEA.wav\"}\n\
                    {\"id\":\"01JLINEB\",\"spoken_at\":\"2026-08-03T10:00:05+02:00\",\
                    \"text\":\"second\",\"audio\":\"audio/01JLINEB.wav\"}\n\
                    {\"edit_of\":\"01JLINEA\",\"at\":\"2026-08-03T11:00:00+02:00\",\
                    \"text\":\"first, tidied\"}\n";
        fs::write(dir.join(LINES_FILE), raw).expect("seed lines");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.skipped_lines, 0);
        let ids: Vec<&str> = loaded.lines.iter().map(|line| line.id.as_str()).collect();
        assert_eq!(ids, vec!["01JLINEA", "01JLINEB"]);
        assert_eq!(loaded.lines[0].text, "first, tidied");
        assert_eq!(loaded.lines[0].original.as_deref(), Some("first"));
        assert_eq!(fs::read(dir.join(LINES_FILE)).expect("read"), raw);
    }

    #[test]
    fn a_save_renders_the_folded_order_with_the_original_timestamps() {
        // The accepted cost of reordering: the stamps
        // stop being chronological, because they are still the moments the
        // lines were spoken.
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let first = append(
            &mut draft,
            "spoken first",
            "2026-08-05T14:32:07+02:00",
            &[0.1],
        );
        let second = append(
            &mut draft,
            "spoken second",
            "2026-08-05T14:33:11+02:00",
            &[0.1],
        );
        append(
            &mut draft,
            "spoken third",
            "2026-08-05T14:35:00+02:00",
            &[0.1],
        );

        draft.move_line(&second.id, None).expect("move to top");
        draft.set_deleted(&first.id, true).expect("delete");

        let target = tmp.path().join("notes.md");
        let report = draft.save_as(&target, None).expect("save");

        assert_eq!(report.lines, 2);
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 14:33:11 — spoken second\n- 14:35:00 — spoken third\n"
        );
    }

    #[test]
    fn a_move_marks_the_draft_dirty_again() {
        let (tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let (a, _b, c) = three_lines(&mut draft);
        draft
            .save_as(&tmp.path().join("notes.md"), None)
            .expect("save");
        assert!(!draft.dirty());

        draft.move_line(&c, Some(&a)).expect("move");

        assert!(draft.dirty());
        assert!(Draft::open(draft.dir()).expect("reopen").draft.meta().dirty);
    }

    #[test]
    fn repositioning_is_the_same_arithmetic_everywhere() {
        // The one helper both the fold and the writer use, so a move on disk
        // and a move in the returned transcript cannot disagree.
        let mut items = vec!['a', 'b', 'c', 'd'];
        reposition(&mut items, 0, Some(2));
        assert_eq!(items, vec!['b', 'c', 'a', 'd']);
        reposition(&mut items, 3, Some(0));
        assert_eq!(items, vec!['b', 'd', 'c', 'a']);
        reposition(&mut items, 2, None);
        assert_eq!(items, vec!['c', 'b', 'd', 'a']);

        assert!(is_settled(0, None));
        assert!(!is_settled(1, None));
        assert!(is_settled(2, Some(1)));
        assert!(is_settled(2, Some(2)));
        assert!(!is_settled(1, Some(2)));
    }

    // -----------------------------------------------------------------------
    // Move to note… — two drafts, one line crossing between them,
    // and the same contract as everything above: both logs only ever grow.
    // What each of these pins is that the *arrival* folds to exactly what the
    // source read — text, `original`, stamp, failure, audio — which is the
    // whole promise of "its audio and history travel with it".
    // -----------------------------------------------------------------------

    /// A source and a destination draft, in one store.
    fn two_drafts() -> (tempfile::TempDir, DraftStore, Draft, Draft) {
        let (tmp, store) = store();
        let from = store.create_draft(Some("playtest")).expect("create source");
        let into = store
            .create_draft(Some("playtest"))
            .expect("create destination");
        (tmp, store, from, into)
    }

    /// The one line a destination holds after a move, folded off disk.
    fn only_line(draft: &Draft) -> LineRecord {
        let mut lines = Draft::open(draft.dir()).expect("reopen").lines;
        assert_eq!(lines.len(), 1, "expected exactly one line: {lines:?}");
        lines.remove(0)
    }

    #[test]
    fn an_edited_line_arrives_as_a_base_record_plus_one_edit_and_folds_identically() {
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let spoken = append(
            &mut from,
            "the date picker closes to soon",
            "2026-08-19T14:31:12+02:00",
            &[0.25, -0.25],
        );
        from.edit_line(&spoken.id, "the date picker closes too soon")
            .expect("edit");

        let moved = from.export_line(&spoken.id).expect("export");
        let arrived = into.import_line(&moved, true).expect("import");

        // The fold the writer reported and the fold a re-read reports are the
        // same fold — the property every correction path here is held to.
        let reread = only_line(&into);
        assert_eq!(reread, arrived);
        assert_eq!(reread.text, "the date picker closes too soon");
        assert_eq!(
            reread.original.as_deref(),
            Some("the date picker closes to soon"),
            "the edit history travelled: the base record carries what was spoken"
        );
        assert_eq!(reread.spoken_at, ts("2026-08-19T14:31:12+02:00"));
        assert!(!reread.deleted);
        assert!(!reread.failed);
        assert_eq!(reread.transcribe_ms, Some(42));

        // Structurally: one base record with the *original* words, and one edit
        // replaying the tidy-up. Two records, no new shape.
        let raw = fs::read_to_string(into.lines_path()).expect("read log");
        let records: Vec<&str> = raw.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            records.len(),
            2,
            "base + edit, and nothing else: {records:?}"
        );
        let base: LineRecord = serde_json::from_str(records[0]).expect("a base record");
        assert_eq!(base.text, "the date picker closes to soon");
        assert_eq!(base.original, None, "`original` is derived, never written");
        let edit: EditRecord = serde_json::from_str(records[1]).expect("an edit record");
        assert_eq!(edit.edit_of, base.id);
        assert_eq!(
            edit.text.as_deref(),
            Some("the date picker closes too soon")
        );
        assert_eq!(edit.deleted, None);
    }

    #[test]
    fn an_unedited_line_arrives_as_one_record_and_no_edit() {
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let spoken = append(
            &mut from,
            "the table redraws twice",
            "2026-08-19T14:31:40+02:00",
            &[0.1],
        );

        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        let raw = fs::read_to_string(into.lines_path()).expect("read log");
        let records: Vec<&str> = raw.lines().filter(|line| !line.is_empty()).collect();
        assert_eq!(
            records.len(),
            1,
            "a line with nothing to replay must not grow an edit: {records:?}"
        );
        let arrived = only_line(&into);
        assert_eq!(arrived.text, "the table redraws twice");
        assert_eq!(arrived.original, None);
        assert_eq!(into.line_count(), 1);
    }

    #[test]
    fn a_failed_line_moves_and_stays_failed() {
        // Its value *is* its audio, so it is exactly the kind of line
        // worth stitching into another note — and the flag has to travel or the
        // destination would show an empty ok line with no Retry.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let refused = append_failed(&mut from, "2026-08-19T14:32:00+02:00", &[0.4, -0.4, 0.4]);

        let moved = from.export_line(&refused.id).expect("export");
        into.import_line(&moved, true).expect("import");

        let arrived = only_line(&into);
        assert!(arrived.failed, "the failure travelled");
        assert!(arrived.text.is_empty());
        assert_eq!(arrived.original, None);
        assert_eq!(arrived.spoken_at, ts("2026-08-19T14:32:00+02:00"));
        assert_eq!(
            into.line_count(),
            0,
            "a failed line has no words, so it is not in the note — the same \
             rule an appended one follows"
        );
        // And its audio is what makes it worth having.
        assert!(into.dir().join(&arrived.audio).is_file());
    }

    #[test]
    fn the_wav_arrives_as_an_identical_byte_copy_under_a_fresh_id() {
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let samples: Vec<f32> = (0..400).map(|i| (i as f32 / 200.0) - 1.0).collect();
        let spoken = append(
            &mut from,
            "loud enough to matter",
            "2026-08-19T14:33:00+02:00",
            &samples,
        );

        let moved = from.export_line(&spoken.id).expect("export");
        assert!(moved.has_audio());
        into.import_line(&moved, true).expect("import");

        let arrived = only_line(&into);
        assert_ne!(
            arrived.id, spoken.id,
            "ids are never reused across drafts — `line_audio(draft, line)` \
             would be ambiguous while the line sits in both"
        );
        assert_eq!(
            arrived.audio,
            format!("{AUDIO_DIR}/{}.wav", arrived.id),
            "the stored path names the id it really has"
        );

        let source_bytes = fs::read(from.dir().join(&spoken.audio)).expect("source wav");
        let copied = fs::read(into.dir().join(&arrived.audio)).expect("copied wav");
        assert_eq!(copied, source_bytes, "no decode, no re-encode");
        // And the source's own wav is exactly where it was.
        assert!(from.dir().join(&spoken.audio).is_file());
    }

    #[test]
    fn a_line_whose_wav_is_gone_still_moves() {
        // The design rule: nothing is discarded because a dependency
        // failed. The words are the note; the missing audio is carried as
        // missing, exactly as the source had it.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let spoken = append(
            &mut from,
            "this one lost its audio",
            "2026-08-19T14:34:00+02:00",
            &[0.1],
        );
        fs::remove_file(from.dir().join(&spoken.audio)).expect("remove the wav");

        let moved = from.export_line(&spoken.id).expect("export");
        assert!(!moved.has_audio());
        let arrived = into.import_line(&moved, true).expect("import");

        assert_eq!(arrived.text, "this one lost its audio");
        assert_eq!(
            arrived.audio,
            format!("{AUDIO_DIR}/{}.wav", arrived.id),
            "the record names the wav it would have had, exactly as the source did"
        );
        assert!(
            !into.dir().join(&arrived.audio).exists(),
            "and no empty stand-in was invented for it"
        );
        assert_eq!(only_line(&into), arrived);
    }

    #[test]
    fn a_moved_line_leaves_the_source_soft_deleted_with_its_records_and_wav_intact() {
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let stays = append(&mut from, "stays put", "2026-08-19T14:30:00+02:00", &[0.1]);
        let goes = append(&mut from, "goes away", "2026-08-19T14:35:00+02:00", &[0.2]);
        let before = fs::read(from.lines_path()).expect("read the source log");

        let moved = from.export_line(&goes.id).expect("export");
        into.import_line(&moved, true).expect("import");
        // The caller's last step, and it is the ordinary soft delete — which is
        // what makes leaving the source one `line_set_deleted` to undo.
        from.set_deleted(&goes.id, true).expect("soft delete");

        let source = Draft::open(from.dir()).expect("reopen the source");
        assert_eq!(source.lines.len(), 2, "nothing was removed from the log");
        assert!(source.lines[1].deleted);
        assert_eq!(source.lines[1].text, "goes away");
        assert_eq!(source.draft.line_count(), 1);
        assert!(!source.lines[0].deleted);
        assert_eq!(source.lines[0].id, stays.id);
        // Append-only, byte for byte: the log grew and nothing in it moved.
        let after = fs::read(from.lines_path()).expect("read the source log");
        assert!(after.starts_with(&before), "the source log was rewritten");
        assert!(
            from.dir().join(&goes.audio).is_file(),
            "the source's wav is untouched — the delete is soft"
        );
    }

    #[test]
    fn a_destination_with_a_moved_line_in_it_still_parses_under_the_four_shape_rule() {
        // An older build has to read a file a newer one wrote: the move emits
        // only the record shapes that already existed, so every line of the
        // destination log still discriminates the same way.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-19T09:00:00+02:00",
            &[0.1],
        );
        into.mark_session().expect("marker");
        let spoken = append(&mut from, "spoken", "2026-08-19T14:36:00+02:00", &[0.3]);
        from.edit_line(&spoken.id, "spoken, then tidied")
            .expect("edit");
        let anchor = only_line(&into).id;

        let moved = from.export_line(&spoken.id).expect("export");
        let arrived = into.import_line(&moved, true).expect("import");
        into.move_line(&arrived.id, None).expect("reorder");

        let loaded = Draft::open(into.dir()).expect("reopen");
        assert_eq!(loaded.skipped_lines, 0, "every record still parsed");
        assert_eq!(
            loaded
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["spoken, then tidied", "already here"],
            "the arrival takes part in the ordinary fold, moves included"
        );
        assert_eq!(loaded.lines[1].id, anchor);
        assert_eq!(loaded.draft.line_count(), 2);
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Hidden)
                .expect("render"),
            "- 14:36:00 — spoken, then tidied\n- 09:00:00 — already here\n",
            "the arrival renders with the timestamp it was spoken at"
        );
    }

    #[test]
    fn an_import_marks_the_destination_dirty_and_never_touches_its_guard() {
        let (tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-19T09:00:00+02:00",
            &[0.1],
        );
        into.save_as(&tmp.path().join("destination.md"), None)
            .expect("save");
        assert!(!into.dirty());
        let guard = into.meta().last_save_hash.clone();
        assert!(guard.is_some());

        let spoken = append(&mut from, "arriving", "2026-08-19T14:37:00+02:00", &[0.1]);
        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        assert!(into.dirty(), "the note is behind its file again");
        assert!(Draft::open(into.dir()).expect("reopen").draft.meta().dirty);
        assert_eq!(
            into.meta().last_save_hash,
            guard,
            "the external-edit guard is about the markdown file, which a move \
             does not write"
        );
    }

    #[test]
    fn a_draft_refuses_to_import_its_own_line() {
        // The one-handle rule, where it cannot be forgotten: this is the shape
        // the worker's own refusal mirrors.
        let (_tmp, _store, mut from, _into) = two_drafts();
        let spoken = append(&mut from, "mine", "2026-08-19T14:38:00+02:00", &[0.1]);
        let moved = from.export_line(&spoken.id).expect("export");

        let err = from.import_line(&moved, true).expect_err("refused");
        assert!(matches!(err, DraftError::SameDraft { .. }), "{err:?}");
        assert_eq!(
            Draft::open(from.dir()).expect("reopen").lines.len(),
            1,
            "and nothing was appended"
        );
    }

    #[test]
    fn exporting_a_line_this_draft_does_not_have_names_it_and_writes_nothing() {
        let (_tmp, _store, from, _into) = two_drafts();
        let err = from
            .export_line("01JCPMOVENOTHINGHERE00")
            .expect_err("refused");
        match err {
            DraftError::NoSuchLine { id, path } => {
                assert_eq!(id, "01JCPMOVENOTHINGHERE00");
                assert_eq!(path, from.dir());
            }
            other => panic!("expected NoSuchLine, got {other:?}"),
        }
        assert_eq!(
            fs::read(from.lines_path()).unwrap_or_default(),
            Vec::<u8>::new(),
            "an export writes nothing"
        );
    }

    #[test]
    fn exporting_resolves_the_wav_from_the_id_and_refuses_a_crafted_one() {
        // The wav is found from the **id**, never from the record's `audio`
        // field, so a hand-written log cannot make a copy read some other file.
        let (_tmp, _store, mut from, _into) = two_drafts();
        let real = append(&mut from, "real", "2026-08-19T14:39:00+02:00", &[0.1]);
        fs::write(
            from.lines_path(),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&real).expect("encode"),
                r#"{"id":"..","spoken_at":"2026-08-19T14:40:00+02:00","text":"crafted","audio":"../../secrets.wav"}"#
            ),
        )
        .expect("write a hand-edited log");

        let err = from.export_line("..").expect_err("refused");
        assert!(matches!(err, DraftError::BadId { .. }), "{err:?}");
        // And the honest record beside it still moves.
        assert!(from.export_line(&real.id).is_ok());
    }

    // -----------------------------------------------------------------------
    // Arrivals land under a divider. The rule is decided from the
    // destination's log tail and lives in `import_line`, so no caller can
    // forget it; these pin it structurally, off the raw log, because a folded
    // transcript deliberately shows no markers at all.
    // -----------------------------------------------------------------------

    /// Every record in a draft's log as its shape, in file order.
    ///
    /// The four-shape rule, spelled the way the loader spells it — a marker is
    /// invisible in a folded transcript, so this is the only honest way to
    /// assert where one was written.
    fn shapes(draft: &Draft) -> Vec<&'static str> {
        let raw = fs::read_to_string(draft.lines_path()).expect("read log");
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                if serde_json::from_str::<LineRecord>(line).is_ok() {
                    "line"
                } else if serde_json::from_str::<EditRecord>(line).is_ok() {
                    "edit"
                } else if serde_json::from_str::<MoveRecord>(line).is_ok() {
                    "move"
                } else if serde_json::from_str::<SessionRecord>(line).is_ok() {
                    "marker"
                } else {
                    "junk"
                }
            })
            .collect()
    }

    #[test]
    fn an_empty_destination_takes_an_arrival_with_no_divider_at_all() {
        // A divider separates; an empty note has nothing to be separated from,
        // and a leading `---` is exactly what the renderer refuses to emit.
        // This is also the chooser's New note path end to end: a fresh draft
        // takes a whole batch and ends up with no marker anywhere in it.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        let one = append(
            &mut from,
            "the first one",
            "2026-08-20T10:00:00+02:00",
            &[0.1],
        );
        let two = append(
            &mut from,
            "the second one",
            "2026-08-20T10:01:00+02:00",
            &[0.1],
        );

        move_batch(&mut from, &mut into, &[&one.id, &two.id]);

        assert_eq!(shapes(&into), vec!["line", "line"]);
        assert_eq!(sessions_of(&into), vec![0, 0]);
    }

    #[test]
    fn an_arrival_into_a_note_that_has_lines_lands_under_one_new_marker() {
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        let spoken = append(
            &mut from,
            "stitched in",
            "2026-08-20T10:00:00+02:00",
            &[0.1],
        );

        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        assert_eq!(
            shapes(&into),
            vec!["line", "marker", "line"],
            "the marker goes above the arrival, not below it"
        );
        assert_eq!(
            sessions_of(&into),
            vec![0, 1],
            "the arrival is a sitting of its own, exactly as a resumed one is"
        );
    }

    #[test]
    fn a_destination_whose_log_already_ends_in_a_marker_gains_no_second_one() {
        // The other half of "a divider is never doubled": a note resumed but
        // not yet spoken into ends in an owed marker, and an arrival takes that
        // one rather than writing its twin.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        into.mark_session().expect("marker");
        let spoken = append(
            &mut from,
            "stitched in",
            "2026-08-20T10:00:00+02:00",
            &[0.1],
        );

        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        assert_eq!(shapes(&into), vec!["line", "marker", "line"]);
        assert_eq!(sessions_of(&into), vec![0, 1]);
    }

    #[test]
    fn a_torn_tail_is_not_a_record_so_the_last_complete_one_decides() {
        // The rule says *complete*: a half-written record is bytes the loader
        // skips and leaves on disk, and it must not be able to make a note grow
        // a second divider. The repair newline is the ordinary append path's.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        into.mark_session().expect("marker");
        let mut log = fs::read(into.lines_path()).expect("read log");
        log.extend_from_slice(br#"{"id":"01JCPTORN"#);
        fs::write(into.lines_path(), &log).expect("write a torn log");
        let mut into = Draft::open(into.dir()).expect("reopen").draft;

        let spoken = append(
            &mut from,
            "stitched in",
            "2026-08-20T10:00:00+02:00",
            &[0.1],
        );
        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        assert_eq!(
            shapes(&into),
            vec!["line", "marker", "junk", "line"],
            "no second marker, and the torn bytes are still there"
        );
        assert_eq!(sessions_of(&into), vec![0, 1]);
    }

    /// One batch: `divide` true on the first line only, exactly as the window
    /// sends it.
    fn move_batch(from: &mut Draft, into: &mut Draft, ids: &[&str]) {
        for (at, id) in ids.iter().enumerate() {
            let moved = from.export_line(id).expect("export");
            into.import_line(&moved, at == 0).expect("import");
        }
    }

    #[test]
    fn a_whole_batch_lands_under_one_marker_however_many_lines_it_holds() {
        // The divider belongs to the *move*, not to each line in it: three
        // stitched lines are one sitting, and three `---`s between them would
        // be exactly the doubling the rule forbids. Only the first call
        // divides, which is why the boundary travels in the command — after
        // the first line lands, this log's tail is a line record like any
        // other and nothing here could tell which move put it there.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        let one = append(&mut from, "one", "2026-08-20T10:00:00+02:00", &[0.1]);
        let two = append(&mut from, "two", "2026-08-20T10:01:00+02:00", &[0.1]);
        let three = append(&mut from, "three", "2026-08-20T10:02:00+02:00", &[0.1]);

        move_batch(&mut from, &mut into, &[&one.id, &two.id, &three.id]);

        assert_eq!(
            shapes(&into),
            vec!["line", "marker", "line", "line", "line"],
            "one marker, above the first arrival"
        );
        assert_eq!(
            sessions_of(&into),
            vec![0, 1, 1, 1],
            "the whole batch is one sitting, so the render draws one divider"
        );
    }

    #[test]
    fn a_later_batch_into_the_same_note_gets_a_divider_of_its_own() {
        // The other direction, and the reason the flag is not a "has this note
        // ever taken an arrival" latch: a second stitching pass is a second
        // sitting, and it says so itself.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        let one = append(&mut from, "one", "2026-08-20T10:00:00+02:00", &[0.1]);
        let two = append(&mut from, "two", "2026-08-20T10:01:00+02:00", &[0.1]);
        let later = append(&mut from, "later", "2026-08-20T11:00:00+02:00", &[0.1]);

        move_batch(&mut from, &mut into, &[&one.id, &two.id]);
        move_batch(&mut from, &mut into, &[&later.id]);

        assert_eq!(
            shapes(&into),
            vec!["line", "marker", "line", "line", "marker", "line"]
        );
        assert_eq!(sessions_of(&into), vec![0, 1, 1, 2]);
    }

    #[test]
    fn a_line_that_is_not_the_first_of_its_batch_never_writes_a_marker() {
        // `divide: false` is unconditional: not "unless the note looks like it
        // needs one". The tail rule is the first line's guard, and nothing
        // else's.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        let spoken = append(&mut from, "tail-ender", "2026-08-20T10:00:00+02:00", &[0.1]);

        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, false).expect("import");

        assert_eq!(
            shapes(&into),
            vec!["line", "line"],
            "a note with lines in it and no divider, because this was not a \
             batch's first line"
        );
        assert_eq!(sessions_of(&into), vec![0, 0]);
    }

    #[test]
    fn the_marker_is_always_written_and_only_the_setting_decides_the_render() {
        // Markers are written unconditionally, exactly as dictation writes
        // them; `session_dividers` is a *rendering* choice, which is what makes
        // turning it on and off safe for a file already bound to a draft.
        let (_tmp, _store, mut from, mut into) = two_drafts();
        append(
            &mut into,
            "already here",
            "2026-08-20T09:00:00+02:00",
            &[0.1],
        );
        let spoken = append(
            &mut from,
            "stitched in",
            "2026-08-20T10:00:00+02:00",
            &[0.1],
        );

        let moved = from.export_line(&spoken.id).expect("export");
        into.import_line(&moved, true).expect("import");

        assert_eq!(
            into.preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 09:00:00 — already here\n\n---\n\n- 10:00:00 — stitched in\n"
        );
        assert_eq!(
            into.preview_markdown(None, SessionDividers::Hidden)
                .expect("render"),
            "- 09:00:00 — already here\n- 10:00:00 — stitched in\n",
            "the same log, the same records, no divider"
        );
    }

    #[test]
    fn a_refused_import_writes_no_divider_either() {
        // The refusal is checked before anything of this line reaches disk, and
        // that includes the marker: a note must not grow a sitting because a
        // move was asked for and turned down.
        let (_tmp, _store, mut from, _into) = two_drafts();
        let spoken = append(&mut from, "mine", "2026-08-20T10:00:00+02:00", &[0.1]);
        let moved = from.export_line(&spoken.id).expect("export");

        from.import_line(&moved, true).expect_err("refused");

        assert_eq!(shapes(&from), vec!["line"]);
    }

    // -----------------------------------------------------------------------
    // Session markers. Same contract again: `lines.jsonl` only
    // grows, and a marker that cannot be read changes nothing.
    // -----------------------------------------------------------------------

    /// The session ordinals of a draft's lines, in rendered order.
    fn sessions_of(draft: &Draft) -> Vec<usize> {
        draft.read_transcript().expect("read").sessions
    }

    #[test]
    fn a_marker_splits_the_lines_after_it_into_a_later_sitting() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "first", "2026-08-05T10:00:00+02:00", &[0.1]);
        append(&mut draft, "second", "2026-08-05T10:01:00+02:00", &[0.1]);
        assert_eq!(sessions_of(&draft), vec![0, 0]);

        draft.mark_session().expect("marker");
        // The marker on its own changes no line.
        assert_eq!(sessions_of(&draft), vec![0, 0]);

        append(&mut draft, "third", "2026-08-06T09:00:00+02:00", &[0.1]);
        draft.mark_session().expect("second marker");
        append(&mut draft, "fourth", "2026-08-07T09:00:00+02:00", &[0.1]);

        assert_eq!(sessions_of(&draft), vec![0, 0, 1, 2]);
        // And it survives a reload: the ordinals are read off the file, never
        // remembered.
        let reopened = Draft::open(draft.dir()).expect("reopen").draft;
        assert_eq!(sessions_of(&reopened), vec![0, 0, 1, 2]);
        assert_eq!(reopened.line_count(), 4);
    }

    #[test]
    fn a_marker_marks_the_draft_dirty_because_it_changes_the_render() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "first", "2026-08-05T10:00:00+02:00", &[0.1]);
        let target = _tmp.path().join("note.md");
        draft.save_as(&target, None).expect("save");
        assert!(!draft.dirty());

        draft.mark_session().expect("marker");
        assert!(draft.dirty());
    }

    #[test]
    fn a_line_keeps_the_sitting_it_was_spoken_in_when_it_is_dragged() {
        // Which sitting a line belongs to is a fact about when it was written.
        // A drag moves where it *reads*, never when it was said.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let first = append(&mut draft, "monday", "2026-08-03T10:00:00+02:00", &[0.1]);
        draft.mark_session().expect("marker");
        let second = append(&mut draft, "tuesday", "2026-08-04T10:00:00+02:00", &[0.1]);
        assert_eq!(sessions_of(&draft), vec![0, 1]);

        draft.move_line(&second.id, None).expect("move to the top");
        let transcript = draft.read_transcript().expect("read");
        assert_eq!(
            transcript
                .lines
                .iter()
                .map(|l| l.id.clone())
                .collect::<Vec<_>>(),
            vec![second.id.clone(), first.id]
        );
        assert_eq!(transcript.sessions, vec![1, 0]);
    }

    #[test]
    fn a_malformed_marker_is_skipped_counted_and_left_on_disk() {
        // A marker with no readable timestamp is not a marker: silently
        // splitting the note somewhere nobody asked for would be worse than
        // reporting an unreadable line.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "before", "2026-08-05T10:00:00+02:00", &[0.1]);
        let dir = draft.dir().to_path_buf();
        drop(draft);

        let broken = br#"{"session_at":"not a timestamp"}"#;
        let lines_path = dir.join(LINES_FILE);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&lines_path)
            .expect("open");
        file.write_all(broken).expect("write");
        file.write_all(b"\n").expect("newline");
        drop(file);

        let mut draft = Draft::open(&dir).expect("open").draft;
        append(&mut draft, "after", "2026-08-06T10:00:00+02:00", &[0.1]);

        let loaded = Draft::open(&dir).expect("reopen");
        assert_eq!(loaded.skipped_lines, 1);
        assert_eq!(loaded.lines.len(), 2);
        // No split happened, because there was no readable marker.
        assert_eq!(sessions_of(&loaded.draft), vec![0, 0]);
        let bytes = fs::read(&lines_path).expect("read");
        assert!(
            bytes.windows(broken.len()).any(|w| w == broken),
            "the malformed marker was removed from the file"
        );
    }

    #[test]
    fn a_draft_written_before_markers_existed_loads_byte_for_byte_the_same() {
        // The third time this pin is made (edits, moves, now markers) and for
        // the same reason: no migration, no version bump, and every line in an
        // old draft is session 0.
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("01JDRAFT");
        fs::create_dir_all(dir.join(AUDIO_DIR)).expect("dirs");
        fs::write(
            dir.join(META_FILE),
            br#"{"version":1,"id":"01JDRAFT","created_at":"2026-08-03T10:00:00+02:00"}"#,
        )
        .expect("seed meta");
        let raw = b"{\"id\":\"01JLINEA\",\"spoken_at\":\"2026-08-03T10:00:00+02:00\",\
                    \"text\":\"first\",\"audio\":\"audio/01JLINEA.wav\"}\n\
                    {\"id\":\"01JLINEB\",\"spoken_at\":\"2026-08-03T10:00:05+02:00\",\
                    \"text\":\"second\",\"audio\":\"audio/01JLINEB.wav\"}\n";
        fs::write(dir.join(LINES_FILE), raw).expect("seed lines");

        let loaded = Draft::open(&dir).expect("open");

        assert_eq!(loaded.skipped_lines, 0);
        assert_eq!(sessions_of(&loaded.draft), vec![0, 0]);
        assert_eq!(fs::read(dir.join(LINES_FILE)).expect("read"), raw);

        // And with dividers on, an old draft renders exactly as it always did.
        assert_eq!(
            loaded
                .draft
                .preview_markdown(None, SessionDividers::Shown)
                .expect("render"),
            "- 10:00:00 — first\n- 10:00:05 — second\n"
        );
    }

    #[test]
    fn a_save_renders_the_divider_and_turning_it_off_renders_the_old_bytes() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "monday", "2026-08-03T10:00:00+02:00", &[0.1]);
        draft.mark_session().expect("marker");
        append(&mut draft, "tuesday", "2026-08-04T09:30:00+02:00", &[0.1]);

        let target = _tmp.path().join("note.md");
        draft
            .save_as_with(&target, None, SaveMode::Guarded, SessionDividers::Shown)
            .expect("save");
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 10:00:00 — monday\n\n---\n\n- 09:30:00 — tuesday\n"
        );

        // Off is the file as it was before markers existed, byte for byte. The
        // guard is re-armed by each save, so this is not a conflict with the
        // file above.
        draft
            .save_with(None, SaveMode::Guarded, SessionDividers::Hidden)
            .expect("save without dividers");
        assert_eq!(
            fs::read_to_string(&target).expect("read"),
            "- 10:00:00 — monday\n- 09:30:00 — tuesday\n"
        );
    }

    #[test]
    fn a_preview_is_exactly_what_a_save_would_write() {
        // The conflict dialog shows the preview beside the file on disk, so a
        // preview that differed from the save would be a lie about what
        // Overwrite is going to do.
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        append(&mut draft, "monday", "2026-08-03T10:00:00+02:00", &[0.1]);
        draft.mark_session().expect("marker");
        append(&mut draft, "tuesday", "2026-08-04T09:30:00+02:00", &[0.1]);

        let preview = draft
            .preview_markdown(Some("# Notes"), SessionDividers::Shown)
            .expect("preview");
        let target = _tmp.path().join("note.md");
        draft
            .save_as_with(
                &target,
                Some("# Notes"),
                SaveMode::Guarded,
                SessionDividers::Shown,
            )
            .expect("save");
        assert_eq!(fs::read_to_string(&target).expect("read"), preview);
    }

    #[test]
    fn line_audio_paths_refuse_anything_that_is_not_a_bare_id() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let spoken = append(&mut draft, "spoken", "2026-08-05T10:00:00+02:00", &[0.1]);

        let path = draft.line_audio_path(&spoken.id).expect("path");
        assert_eq!(path, draft.dir().join(&spoken.audio));
        assert!(path.is_file());

        for id in [
            "",
            ".",
            "..",
            ".hidden",
            "../../secrets",
            "audio/01JLINE",
            "/absolute",
            "01JLINE.wav",
            "01JLINE.",
        ] {
            match draft.line_audio_path(id) {
                Err(DraftError::BadId { .. }) => {}
                other => panic!("{id:?} was not rejected: {other:?}"),
            }
        }

        // The store-side form validates the draft id too: this is the pair the
        // playback command hands straight over from the frontend.
        let draft_id = draft.id().to_owned();
        assert_eq!(
            store.line_audio_path(&draft_id, &spoken.id).expect("path"),
            path
        );
        for (draft_id, line_id) in [
            ("../escape", spoken.id.as_str()),
            (".trash", spoken.id.as_str()),
            (draft_id.as_str(), "../../../etc/passwd"),
            (draft_id.as_str(), ""),
        ] {
            match store.line_audio_path(draft_id, line_id) {
                Err(DraftError::BadId { .. }) => {}
                other => panic!("{draft_id:?}/{line_id:?} was not rejected: {other:?}"),
            }
        }
    }

    #[test]
    fn default_drafts_dir_ends_in_drafts() {
        // `dirs` only reads environment conventions; nothing is created and
        // nothing leaves the machine.
        assert!(default_drafts_dir().ends_with("drafts"));
    }

    // -- Search -------------------------------------------------------------
    //
    // Pure tests over folded records: the matcher never opens anything, which
    // is the property that lets the command scan the *active* draft while the
    // worker is appending to it.

    /// A stored line, without going anywhere near a disk.
    fn line(id: &str, at: &str, text: &str) -> LineRecord {
        LineRecord {
            id: id.to_owned(),
            spoken_at: ts(at),
            text: text.to_owned(),
            original: None,
            deleted: false,
            audio: format!("{AUDIO_DIR}/{id}.wav"),
            transcribe_ms: None,
            failed: false,
        }
    }

    #[test]
    fn a_term_is_trimmed_and_an_empty_one_is_search_off() {
        assert_eq!(
            SearchTerm::parse("  table  ").expect("term").as_str(),
            "table"
        );
        // Interior whitespace is part of the literal — no tokenising.
        assert_eq!(
            SearchTerm::parse(" the table ").expect("term").as_str(),
            "the table"
        );
        // The off switch, and the only one.
        assert!(SearchTerm::parse("").is_none());
        assert!(SearchTerm::parse("   ").is_none());
        assert!(SearchTerm::parse("\t\n ").is_none());
    }

    #[test]
    fn matching_is_case_insensitive_literal_substring() {
        let term = SearchTerm::parse("Table").expect("term");
        assert!(term.matches_text("the table redraws"));
        assert!(term.matches_text("TABLE"));
        // The mock's own case: "Table" at the start of a sentence matches a
        // lowercase term and vice versa.
        assert!(SearchTerm::parse("table")
            .expect("term")
            .matches_text("Table rows lose their hover state"));
        // A substring, not a word: "table" is in "tables".
        assert!(term.matches_text("two tables"));
        assert!(!term.matches_text("tabel"));
        // No regex, no globbing: the term is literal.
        let dotted = SearchTerm::parse("t.ble").expect("term");
        assert!(!dotted.matches_text("the table"));
        assert!(dotted.matches_text("a t.ble"));
    }

    #[test]
    fn deleted_and_unresolved_failed_lines_never_match() {
        let term = SearchTerm::parse("table").expect("term");

        let mut deleted = line("01A", "2026-08-05T10:00:00+02:00", "the table flickers");
        deleted.deleted = true;
        assert!(!term.matches_line(&deleted));

        // A failed line has no words at all, and the folded flag is what says
        // so. Both halves are checked: the flag alone excludes it even if
        // something ever put text beside it.
        let mut failed = line("01B", "2026-08-05T10:00:01+02:00", "");
        failed.failed = true;
        assert!(!term.matches_line(&failed));
        let mut resolved = line("01C", "2026-08-05T10:00:02+02:00", "the table flickers");
        resolved.failed = false;
        assert!(term.matches_line(&resolved));

        // And the live one still does.
        assert!(term.matches_line(&line(
            "01D",
            "2026-08-05T10:00:03+02:00",
            "sorting the table"
        )));
    }

    #[test]
    fn a_matching_line_counts_once_however_often_the_term_occurs() {
        let term = SearchTerm::parse("table").expect("term");
        let lines = vec![
            line("01A", "2026-08-05T10:00:00+02:00", "table table table"),
            line("01B", "2026-08-05T10:00:01+02:00", "nothing here"),
            line("01C", "2026-08-05T10:00:02+02:00", "one Table"),
        ];
        // Indices, in transcript order, one per line.
        assert_eq!(matching_lines(&term, &lines), vec![0, 2]);
    }

    #[test]
    fn the_matcher_reads_a_torn_tail_draft_exactly_as_the_loader_left_it() {
        let (_tmp, store) = store();
        let mut draft = store.create_draft(None).expect("create");
        let dir = draft.dir().to_path_buf();
        append(
            &mut draft,
            "the table flickers",
            "2026-08-05T10:00:00+02:00",
            &[0.1],
        );
        append(
            &mut draft,
            "nothing to see",
            "2026-08-05T10:00:01+02:00",
            &[0.1],
        );
        drop(draft);

        // Exactly what a concurrent append looks like to a reader: half a
        // record, no newline. `Draft::open` skips it and rewrites nothing, so
        // the scan is safe to run over the note the worker is writing into.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join(LINES_FILE))
            .expect("open");
        file.write_all(br#"{"id":"01HALF","text":"the table ag"#)
            .expect("write");
        drop(file);

        let loaded = Draft::open(&dir).expect("open");
        let term = SearchTerm::parse("TABLE").expect("term");
        assert_eq!(matching_lines(&term, &loaded.lines), vec![0]);
        assert_eq!(loaded.skipped_lines, 1);
    }

    #[test]
    fn last_written_is_the_newest_live_line_or_nothing_at_all() {
        // No lines at all: nothing has ever been written, and the caller's
        // fallback (the draft's creation time) takes over.
        assert_eq!(last_written(&[]), None);

        let mut lines = vec![
            line("01A", "2026-08-05T10:00:00+02:00", "one"),
            line("01B", "2026-08-05T10:05:00+02:00", "two"),
            // Out of chronological order on purpose: a drag reorders the
            // transcript and every line keeps the time it was spoken at, so
            // "last" is a max and never "the one at the end".
            line("01C", "2026-08-05T10:02:00+02:00", "three"),
        ];
        assert_eq!(last_written(&lines), Some(ts("2026-08-05T10:05:00+02:00")));

        // A deleted or failed line is not something that was written into the
        // note, so it cannot be when the note was last written.
        lines[1].deleted = true;
        assert_eq!(last_written(&lines), Some(ts("2026-08-05T10:02:00+02:00")));
        lines[2].failed = true;
        assert_eq!(last_written(&lines), Some(ts("2026-08-05T10:00:00+02:00")));
        lines[0].deleted = true;
        assert_eq!(last_written(&lines), None);
    }
}
