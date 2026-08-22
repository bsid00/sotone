//! The session-bar shell: startup, the control thread, and the IPC surface.
//!
//! This is `examples/dictate.rs` with a window instead of a console. The wiring
//! is deliberately identical — config → model → audio → whisper → draft — and
//! the recording state machine has the same shape, because the console runner
//! is the thing that was used in anger and it is the behaviour we are keeping.
//!
//! # Threads
//!
//! ```text
//! sotone-hook.exe ─pipe─→ hook reader thread ─send─┐
//! Tauri commands ─────────────────────────send────┼─→ control thread
//!                                                 │      ↓
//!                                                 │   AudioEngine → SessionWorker ─┐
//!                                                 └─ emit ──→ webview ←── emit ── drain thread
//!                                                       └──→ overlay thread (the pill)
//!                                                       └──→ tray thread (the glyph)
//! ```
//!
//! * The **control thread** is spawned from `setup`, does the whole (slow)
//!   startup itself so the event loop is never blocked, and then becomes the
//!   consumer loop. It owns the [`AudioEngine`] — which is deliberately not
//!   `Sync` — and is the only place `begin_utterance`/`end_utterance` are
//!   called.
//! * The **hook reader thread** owns the `sotone-hook` helper process: it reads
//!   its JSON lines, forwards them as [`ShellInput`]s, and restarts it if it
//!   dies. The hook itself is *not* in this process — see below.
//! * The **drain thread** turns [`SessionEvent`]s into frontend events.
//! * Nothing that observes input runs on the UI thread (invariant 5). Commands
//!   take the same route as key events: [`set_armed`] sends, it does not touch
//!   the engine.
//!
//! # Why the hook is a separate process
//!
//! An in-process `WH_KEYBOARD_LL` hook starves while Sotone's own WebView2
//! window is foreground and it is the only LL hook in the chain: keys vanish
//! silently (tauri-apps/tauri#14770; reproduced in both directions on one
//! running instance, by adding and removing a second process's hook while it
//! ran). The only confirmed fix is to own the hook in a process with no
//! WebView2 window in it, so `sotone-hook.exe` does that and streams what it saw
//! over a pipe. Its stdin is the parent's liveness signal: we hold the write
//! end open and never write, so if Sotone dies the helper sees EOF and exits.
//!
//! # Event contract (the whole of it, in one place)
//!
//! Payloads are the DTOs in this file — plain, `Serialize`, frontend-shaped.
//! Core types never cross the IPC boundary, so a change in `sotone-core` cannot
//! silently reshape the UI.
//!
//! | Event | Payload | When |
//! |---|---|---|
//! | `sotone://status` | [`StatusEvent`] `{ phase, detail, ready?, onboarded }` | every startup step, then ready — or empty, or fatal |
//! | `sotone://armed` | [`ArmedEvent`] `{ user_armed }` | the Arm/Disarm button |
//! | `sotone://recording` | [`RecordingEvent`] `{ live, source? }` | a recording starts or stops |
//! | `sotone://line` | [`LineEvent`] `{ n, spoken_at, text }` | a line was durably appended |
//! | `sotone://level` | [`LevelEvent`] `{ level }` | ~30 Hz **while a recording is live**, and never otherwise |
//! | `sotone://lines` | [`LinesEvent`] `{ draft_id, lines }` | the active draft's whole transcript, after anything that changes it |
//! | `sotone://pending` | [`PendingEvent`] `{ pending }` | every line not on disk yet — queued, transcribing, held |
//! | `sotone://notice` | [`NoticeEvent`] `{ level, message }` | one transient line: a refusal, a failure, or a `debug` entry for the log |
//! | `sotone://condition` | [`ConditionEvent`] `{ condition, detail }` | a *permanent* condition began or ended |
//! | `sotone://drafts` | [`DraftsEvent`] `{ drafts, rejected, active_id, default_save_dir }` | startup, and after anything that changes a draft |
//! | `sotone://save` | [`SaveEvent`] `{ outcome, path?, message?, disk_text?, pending_markdown? }` | a Save was attempted: written, stopped by an external edit, refused for want of a project, or failed |
//! | `sotone://save-all` | [`SaveAllEvent`] `{ saved, skipped, conflicts, errors }` | one Save all batch — every dirty note in the store, whatever project it belongs to — as one outcome, never one `sotone://save` per note |
//! | `sotone://note-clash` | [`NoteClashEvent`] `{ id, project, name, path, suggestion }` | a drop landed on a name the target folder already has — **nothing was written**, and the window asks |
//! | `sotone://projects` | [`ProjectsEvent`] `{ projects, active }` | startup, and after every project mutation |
//! | `sotone://settings` | [`SettingsEvent`] `{ ptt, toggle, mic_substring, audio_cues, overlay, theme, close_quits, hide_deleted, platform, version, active_model, models_dir, models, rejected_models, capture, capture_mode }` | startup, after every settings mutation, on every capture state change, and on every [`models_rescan`] (the models list is re-read from disk each time this is built) |
//! | `sotone://view` | [`ViewEvent`] `{ view }` | the user asked for a view from *outside* the window — currently only the tray's Settings item |
//!
//! Every one of those goes out with [`Emitter::emit`], which broadcasts to
//! every webview — so the overlay window is a second *subscriber* to this table
//! rather than an owner of anything in it. By design it reads one snapshot
//! from [`shell_status`] and then follows `sotone://recording` (the VU state),
//! `sotone://level` (the bars), `sotone://line` (the reveal), `sotone://status`
//! (whether there is a session at all) and `sotone://settings` (the palette, the
//! corner and the reveal duration). It still invokes exactly one command — the
//! snapshot — and there is no per-window emit anywhere in this file.
//!
//! **Search adds no row to that table**. It is a question the window
//! asks — [`search_notes`], answering with a value the way [`shell_status`]
//! does — and nothing in this file remembers it was asked, so there is no
//! search state that could go stale and nothing to carry on the snapshot. It is
//! also the only command that reads the whole store, and it is read-only from
//! end to end.
//!
//! **Renaming adds no row either**. A note's name *is* its file's
//! basename and a project's identity *is* its name, so `draft_rename`,
//! `project_rename` and `project_delete` change things the drafts and projects
//! payloads already carry — every label in the window, and the tray's recents,
//! re-derive from `saved_path` and `project` on the next emission of those two.
//! A new event would have been a second source of truth for a name.
//!
//! **Moving a note adds exactly one row, and only because it is a question**
//! Where a drop *succeeds* it still says nothing of its own: the
//! file is in the new folder, the row is in the new group, and `sotone://drafts`
//! carries both. `sotone://note-clash` exists for the one outcome the drafts
//! payload cannot express — "there is already one called that, and nothing has
//! been written" — which needs an answer before anything happens at all.
//!
//! **Moving *lines* between notes adds no row at all**. There is no
//! command table in this header and there never was one — commands are
//! documented where they are defined and registered in `main.rs` — so this is
//! the entry for [`line_move_to`], the third action in the selection toolbar:
//! it is one more send-only line command, the sibling of [`line_set_deleted`],
//! and both sides of what it does are already carried by rows above. The source
//! loses a line, which is `sotone://lines`; both notes' `line_count` and `dirty`
//! change, which is `sotone://drafts`, re-listed off that same transcript event.
//! A refusal — no such target, the target *is* the open note, a destination
//! that cannot be opened — is `sotone://notice` through the existing
//! [`SessionEvent::Error`] shape. Nothing here needed a question, because
//! nothing here can land on somebody else's file: both ends are appends inside
//! the drafts store.
//!
//! **Making the destination adds no row either**. The chooser's
//! "New note" is [`draft_create_detached`], and it is the one command in this
//! file that creates a draft without the worker: it must not be [`draft_new`],
//! which would open the fresh note as the *active* one and displace the note
//! the selection is being moved out of. It answers with the new id — the
//! window needs to name it in the `line_move_to` calls behind it — and fires
//! the ordinary `sotone://drafts` re-list on its way out, which is how the tree
//! learns about the row. The lines then arrive under a session divider of
//! their own; that marker is written by `Draft::import_line` on the worker,
//! and whether it is *rendered* is still the project's `session_dividers`
//! setting, so it adds nothing here either.
//!
//! It did add one **argument**, though: [`line_move_to`] carries `first`, true
//! on the first of a confirm's N calls. The batch is a fact about the window's
//! click, and nothing below the window can recover it — after the first line
//! lands, the destination's log tail is a line record whether this move put it
//! there or last week's did. So the boundary is declared rather than inferred,
//! and no state is kept anywhere between the N commands.
//!
//! `sotone://level` is the one row here that is *decoration*: the pill's state
//! comes from `sotone://recording`, exactly as the title bar's indicator does,
//! so the two can never disagree, which is what the design asks for.
//!
//! The **tray** is a third subscriber to the same table, one step
//! further removed: it is not a webview, so it is fed the same facts as
//! [`TrayInput`]s from the very functions that emit them — [`ShellState::set_status`],
//! [`ShellState::emit_armed`], [`ShellState::set_recording`],
//! [`ShellState::update_conditions`] and [`ShellState::set_drafts`]. It reads
//! nothing back and infers nothing, which is what stops the glyph from ever
//! disagreeing with the window about whether the microphone is open.
//! `crate::tray` has the rest of that story.
//!
//! # Conditions versus notices
//!
//! Those last two rows are the two halves of one rule, and getting them the
//! wrong way round is the defect this split exists to fix. Before it, a **permanent**
//! state — the hook is dead, there is no microphone, a save is stopped by an
//! external edit — was announced through the **transient** channel: one line in
//! a thirty-entry rolling list, and once it scrolled out nothing on screen said
//! the app was deaf.
//!
//! So:
//!
//! * A [`Condition`] is a state that *holds*. It lives in the snapshot, it is
//!   published on `sotone://condition` (and carried by [`shell_status`], so a
//!   webview that loads afterwards still sees it), and it ends only when the
//!   thing that caused it ends. At most one is published at a time —
//!   [`Conditions::top`] resolves the precedence, so no emission site has to.
//! * A [`NoticeEvent`] is a sentence about one *moment*: a refusal, a failure
//!   with no state to pin, or a `debug` line for the log. Nothing on screen is
//!   allowed to be inferred from one — a window that decided "the hook is dead"
//!   because a notice said so would be back in the same stale-snapshot family
//!   the conditions channel exists to prevent.
//!
//! [`NoticeLevel`] is the routing, and it is the whole routing: `warn` and
//! `error` reach the user (the pane footer's one-line message slot), `info` and
//! `debug` only reach the debug log. That is why several sentences changed
//! level without changing a word — a confirmation is `debug` because
//! visible state already carries it, and a refusal is `warn` because the design
//! calls a refusal `--warn` at full text brightness.
//!
//! The **`empty` phase** is the fourth value of `phase` and it is a
//! *designed* outcome, not a failure: no model could be resolved, so there is no
//! session — but the configuration is loaded, every settings command works, and
//! `sotone://settings` goes out immediately behind the status so the onboarding
//! panel can list what the models folder actually holds, rejects included. It
//! adds no event of its own. Nothing else has started: no microphone, no
//! whisper, no helper process, and the overlay stays hidden because
//! [`ShellState::is_ready`] is false.
//!
//! That phase draws **two different screens**, and the split is
//! the window's to make, not this file's: `onboarded` on the status says whether
//! the first-run wizard is behind this machine, so an `empty` first run is the
//! **wizard** and an `empty` launch on a configured machine is the
//! **repair panel** — the same phase, the same events, the same commands, two
//! surfaces. The wizard also runs in the `ready` phase, on a machine that was
//! seeded with a model before it ever started, and there every live-apply path
//! works throughout it, which is why finishing there needs no restart.
//! A fatal beats both: a broken install has to say so.
//!
//! `DraftsEvent.default_save_dir` is where a *first* save would land: the active
//! project's notes folder, recomputed from the **current** configuration on
//! every emission — there is no startup snapshot anywhere any more. It rides
//! on the drafts event rather than on `ReadyInfo` because it belongs with the
//! per-draft `saved_path`s the tooltip is choosing between.
//!
//! # Who owns the configuration
//!
//! This file does, after startup: one [`Config`] behind one mutex, held only for
//! the length of an edit-and-save. Every consumer — the save path, the drafts
//! list, a lazily created draft's project — reads it at the moment of use, so a
//! project created mid-session takes effect immediately.
//!
//! **When** the hand-over happens is load-bearing, not incidental.
//! It is the first thing [`init`] does after the file parses and the bindings
//! validate, and deliberately *before* the model step, because the model step's
//! failure is the empty phase: an empty phase whose configuration had never been
//! transferred would answer every settings command with "Sotone is still starting
//! up", so the one screen whose entire purpose is choosing a model would be the
//! one screen that could not. The rest of startup reads what it needs back out
//! of the state, or took a copy before the move.
//!
//! One key in there is read **before** the hand-over, and it is the only one:
//! `onboarded`. [`onboarding_state`] peeks at it on the main thread
//! during `setup`, read-only, because two decisions cannot wait for the control
//! thread — the wizard's 620×449 frame has to be set before the event loop ever
//! composites the window, and `user_armed` is constructed there. The peek writes
//! nothing and reports nothing: a config that will not parse is `init`'s error
//! to raise, with its path. Afterwards the key lives under the same lock as the
//! rest, and exactly two things write it — `init` consuming `"first-launch"`,
//! and [`onboarding_finish`].
//!
//! A hand-edit made while Sotone is running is *not* re-read (no hot-reload,
//! deferred) and is clobbered by the next UI mutation. On record as acceptable:
//! the UI is the editor over the file, and edits go back through
//! [`Config::save`], which round-trips comments and unknown keys byte for byte.
//!
//! [`shell_status`] returns a snapshot of all of it, so a webview that finishes
//! loading after the events fired still renders the truth. Call it once, then
//! trust the events.
//!
//! # Invariants
//!
//! * **1 — no synthetic input.** Nothing here generates input. The only key
//!   data that exists is what the hook *observed*; commands from JS can flip a
//!   bool and read a snapshot, and that is the entire surface.
//! * **2 — never steal focus.** No code path in this file calls a focus,
//!   activate or raise API on either window: there is no `set_focus`, no
//!   `set_always_on_top`, no `unminimize`, and not even a read of
//!   `is_focused`. Capture works regardless of which window is focused,
//!   including Sotone's own; focus-based auto-disarm was tried once and
//!   rejected.
//! * **2, amended for the overlay.** This file
//!   *does* call `show()`, `hide()`, `set_size()` and `set_position()` — on the
//!   overlay window, and all four from one thread ([`spawn_overlay`]). (There
//!   is one call on the *main* window too, and it is [`on_close`]'s `hide()`,
//!   which is the opposite of taking focus. There are also two ways for the X
//!   to *quit* instead, and a quit shows nothing either.) None of
//!   them can activate the overlay, by construction rather than by hope:
//!   - `focus: false` in `tauri.conf.json` sets tao's `MARKER_DONT_FOCUS` when
//!     the window is built, and nothing ever clears it — so every later
//!     `show()` is dispatched as `SW_SHOWNOACTIVATE`, never `SW_SHOW`. That
//!     flag, not `focusable`, is what makes a *runtime* show non-activating.
//!   - `focusable: false` puts `WS_EX_NOACTIVATE` on the window, so even a
//!     click on it cannot hand it activation.
//!   - [`fix_overlay_styles`] adds `WS_EX_TOOLWINDOW` and clears
//!     `WS_EX_APPWINDOW`, so it is absent from Alt-Tab and the taskbar — it
//!     cannot be *selected* into the foreground either. It runs **after every
//!     `show()`**, not once at setup: tao rewrites the whole extended-style
//!     word from its own stored flags when it processes a visibility change,
//!     which erased the setup-time version. [`restyle_after_show`]
//!     has the ordering.
//!   - `alwaysOnTop: true` is set once, in the config; the runtime toggle that
//!     would go through `SetWindowPos` is never called.
//!   - the size and position calls (the window hugs the pill's current
//!     state) are `SetWindowPos` writes that touch neither z-order nor focus,
//!     and they are made from the overlay thread rather than from an event
//!     handler, so nothing waits on the event loop for them (invariant 5).
//!   - a DPI probe against the real window reads those bits back off it
//!     — and the window's size with them — because a silently-ignored
//!     flag is the failure mode this whole arrangement has
//!     (tauri-apps/tauri#12055).
//! * **2, again, for the first-run frame.** [`set_main_frame`] calls
//!   `set_resizable`, `set_size` and `center` on the **main** window — once at
//!   launch when the wizard is about to run, once more if it finishes without a
//!   restart. None of the three can activate anything: two are `SetWindowPos`
//!   writes with no activation bit and the third is a style change, and there is
//!   still no `set_focus`, `show` or raise on this window anywhere in this file.
//!   The main window taking focus when the OS launches it is standing behaviour
//!   and is not touched; the overlay is not reached at all.
//! * **2, again, for the tray.** Two carve-outs, both in
//!   `crate::tray` and both user-initiated: the glass menu window taking
//!   focus for as long as it is open (blur is the dismissal, the
//!   native menu's own semantics), and the `show()` + `set_focus()` behind
//!   "Open Sotone", a recent note, Settings, the icon's double-click
//!   and an exe relaunch. `set_focus` appears in exactly two functions
//!   in this crate, `tray::open_window` and `Tray::place_and_show`, and
//!   nothing in *this* file calls either.
//!   [`on_close`] does the opposite — it hides the window, or quits
//!   without showing anything at all — and that `hide()` is the only
//!   visibility call the *main* window has ever had here.
//! * **2, again, for the two native surfaces.** The folder picker is
//!   an OS dialog and does take focus while it is open. It is permitted because
//!   it opens *only* from a click in Sotone's own UI — no code path opens it
//!   unprompted — and because capture is focus-independent, so a note can
//!   still be dictated while it is up. Nothing here calls a focus, raise or
//!   activate API on Sotone's own window, and every question Sotone asks the user
//!   is still in-page DOM.
//! * **3 — nothing leaves the machine.** No network client is constructed, no
//!   URL is opened, no updater is registered. The helper process talks over an
//!   anonymous pipe, which is not a network: no socket, no port, no address.
//!   `tauri-plugin-opener` is used for exactly one thing — revealing a local
//!   path Sotone resolved itself in the OS file manager; no URL-opening API is
//!   called and the command takes no path from the frontend.
//! * **3, again, for the first run.** The empty state *tells* the
//!   user where whisper weights live and lets the window copy that address to
//!   the clipboard. It opens no URL, fetches nothing, and adds no network code
//!   path: the browser that does the downloading is the user's own, which is
//!   exactly the carve-out in invariant 3. [`app_restart`] relaunches this same
//!   local executable and reaches no network at all.
//! * **1, again, for hotkey capture.** "Press the key you want" is
//!   an *observation*: the same helper process, the same read-only hook, one
//!   press reported as a token. Nothing added here can generate a keystroke or
//!   a mouse event, and the vendored rdev has `simulate` and `grab` deleted, so
//!   there is no such call to make.
//! * **5 — never block the input hook.** See the thread diagram above. The hook
//!   callback is not even in this process any more. Capture obeys the same
//!   rule: the control thread never *waits* for a press — the capture helper's
//!   reader thread turns the press into one more [`ShellInput`], so a capture
//!   in progress does not stop a single key event, command or event from being
//!   handled.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Error, Result};
use serde::Serialize;
use tauri::{
    AppHandle, CloseRequestApi, Emitter, LogicalSize, Manager, PhysicalPosition, State,
    WebviewWindow, Window,
};

use crate::tray::{self, TrayInput};

use sotone_core::audio::{list_input_devices, AudioEngine, EngineStatus, Reconnect};
use sotone_core::config::{
    clamp_reveal_seconds, default_config_path, default_notes_root, folder_plan,
    recording_mode_problem, Config, FolderKept, FolderPlan, Onboarded, OverlayCorner, Project,
    ProjectRenameError, Theme, DEFAULT_REVEAL_SECONDS,
};
use sotone_core::cue::{Cue, CuePlayer};
use sotone_core::draft::{
    default_drafts_dir, last_written, matching_lines, resolve_binding, ClashChoice, Draft,
    DraftScan, DraftStore, LineRecord, MoveOptions, NoteMove, SearchTerm, SessionDividers,
    TRASH_RETENTION,
};
use sotone_core::hotkey::wire::HookMessage;
use sotone_core::hotkey::{Binding, Bindings, PttEvent};
use sotone_core::model::{scan_models_dir, validate_model, ModelKind};
use sotone_core::savepath;
use sotone_core::session::{
    ProjectSaveContext, ReleaseInfo, SaveOutcome, SessionCommand, SessionConfig, SessionEvent,
    SessionWorker,
};
use sotone_core::template;
use sotone_core::transcribe::{language_options, Language, Transcriber, AUTO_LANGUAGE};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

/// Startup progress, the ready readout, and fatal failures.
pub const EVENT_STATUS: &str = "sotone://status";
/// Armed state, whenever the user flips it.
pub const EVENT_ARMED: &str = "sotone://armed";
/// Recording started or stopped.
pub const EVENT_RECORDING: &str = "sotone://recording";
/// A line landed in the draft.
pub const EVENT_LINE: &str = "sotone://line";
/// How loud the microphone is right now, while a recording is live.
///
/// **Decoration, not state.** It exists for the overlay pill's VU bars and
/// nothing may be inferred from it — a window that decided a recording was
/// running because a level arrived would be back in the stale-snapshot family. The
/// channel is silent between recordings, so an idle pill costs nothing, and
/// this deliberately does *not* ride on [`ShellStatus`]: there is no cold-load
/// claim to make about a number that was true 33 ms ago.
pub const EVENT_LEVEL: &str = "sotone://level";
/// The active draft's whole transcript, after anything that changed it.
pub const EVENT_LINES: &str = "sotone://lines";
/// Every line that is *not on disk yet* — queued, being transcribed, or held
/// because a write failed. A whole snapshot every time, never a
/// delta, exactly like [`EVENT_LINES`].
pub const EVENT_PENDING: &str = "sotone://pending";
/// One transient sentence: a refusal, a failure, or a `debug` log line.
pub const EVENT_NOTICE: &str = "sotone://notice";
/// A permanent condition began or ended. Never derived from a
/// notice: this is the state, and the window renders it rather than inferring
/// it.
pub const EVENT_CONDITION: &str = "sotone://condition";
/// The outstanding-drafts list, and which of them is active.
pub const EVENT_DRAFTS: &str = "sotone://drafts";
/// What one Save did — or refused to do.
pub const EVENT_SAVE: &str = "sotone://save";
/// What one Save all did, for the whole batch.
pub const EVENT_SAVE_ALL: &str = "sotone://save-all";
/// A drop found a note of that name already in the target folder.
///
/// The one event in this table that is a **question**. Nothing was written when
/// it is emitted — the drop is one atomic intent and it has not started — and
/// the window's only two answers are Keep both (the same command again with
/// `keep_both`) and Cancel (send nothing). There is no overwrite answer, here
/// or anywhere in the drop path (invariant 4).
pub const EVENT_NOTE_CLASH: &str = "sotone://note-clash";
/// The project list and which of them is active.
pub const EVENT_PROJECTS: &str = "sotone://projects";
/// Everything the Settings tab edits, and what capture is doing.
pub const EVENT_SETTINGS: &str = "sotone://settings";
/// The user asked for a view from outside the window.
pub const EVENT_VIEW: &str = "sotone://view";

/// The main window's label, as `tauri.conf.json` declares it.
pub(crate) const MAIN_LABEL: &str = "main";

/// The in-window settings view, as [`EVENT_VIEW`] names it.
///
/// The design draws Settings as a separate window; Sotone makes it a view swap
/// inside the one webview, so "open Settings" from outside is a request the
/// window answers by swapping, not a second window to create.
pub(crate) const VIEW_SETTINGS: &str = "settings";

// ---------------------------------------------------------------------------
// DTOs — the IPC surface
// ---------------------------------------------------------------------------

/// Which of the four startup outcomes the shell is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Startup is still running; `detail` is the current step.
    Loading,
    /// Running. `ready` carries the readout.
    Ready,
    /// No model to load, so there is no session — and nothing is broken.
    /// `detail` is the *specific* reason this machine is here, and
    /// the window answers it with the onboarding panel rather than an error.
    Empty,
    /// Startup failed. `detail` is the message, and it is the only thing the
    /// user has — it must read as a sentence, not as a Debug dump.
    Fatal,
}

impl Phase {
    /// The word this phase crosses the IPC boundary as.
    ///
    /// Kept beside the `rename_all = "lowercase"` above and pinned by a test,
    /// because the window switches on these four strings: a variant renamed
    /// without a thought for the derive would silently leave the frontend with
    /// no branch and a blank screen.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Ready => "ready",
            Self::Empty => "empty",
            Self::Fatal => "fatal",
        }
    }
}

/// What Sotone is doing, or why it is not doing it.
#[derive(Debug, Clone, Serialize)]
pub struct StatusEvent {
    /// Loading / ready / empty / fatal.
    pub phase: Phase,
    /// One line of prose for the phase.
    pub detail: String,
    /// Present only in [`Phase::Ready`].
    pub ready: Option<ReadyInfo>,
    /// Whether the first-run wizard is behind this machine.
    ///
    /// One bool, not the config's three-state marker: the window's question is
    /// only ever "do I draw the wizard", and `"first-launch"` is a *backend*
    /// arrangement about arming that is consumed before the window could act on
    /// it. It rides here rather than on an event of its own because the wizard
    /// is a view, and which view is on screen is what this event already
    /// decides.
    pub onboarded: bool,
}

impl StatusEvent {
    /// What the constructors below claim, until [`ShellState::set_status`]
    /// re-derives it from the configuration — the only place that can see it.
    ///
    /// `true` is the safe placeholder: it is the answer for every user who is
    /// not on their first run, and a loading screen that flashed the wizard at
    /// one of them would be worse than one that reaches it a frame late.
    const NOT_ONBOARDING: bool = true;

    fn loading(step: impl Into<String>) -> Self {
        Self {
            phase: Phase::Loading,
            detail: step.into(),
            ready: None,
            onboarded: Self::NOT_ONBOARDING,
        }
    }

    /// The error's whole `Display` chain: the causes are usually where the
    /// actionable part lives (a path, a device name, a validation reason).
    fn fatal(err: &anyhow::Error) -> Self {
        Self {
            phase: Phase::Fatal,
            detail: format!("{err:#}"),
            ready: None,
            onboarded: Self::NOT_ONBOARDING,
        }
    }

    fn ready(info: ReadyInfo) -> Self {
        Self {
            phase: Phase::Ready,
            detail: "listening".to_owned(),
            ready: Some(info),
            onboarded: Self::NOT_ONBOARDING,
        }
    }

    /// No model, therefore no session.
    ///
    /// Deliberately not built from an `anyhow::Error` the way [`StatusEvent::fatal`]
    /// is: this is not a failure being reported, it is a state being described,
    /// and the sentence was written for the panel rather than assembled from a
    /// chain of contexts.
    fn empty(detail: impl Into<String>) -> Self {
        Self {
            phase: Phase::Empty,
            detail: detail.into(),
            ready: None,
            onboarded: Self::NOT_ONBOARDING,
        }
    }
}

/// The session readout: everything the user needs to trust the setup at a
/// glance, as the session bar shows it.
#[derive(Debug, Clone, Serialize)]
pub struct ReadyInfo {
    /// Model file name, not the path — the path is the tooltip's job.
    pub model: String,
    /// Full path to the model, for the details line.
    pub model_path: String,
    /// `english-only` / `multilingual`, from the GGML header — `ModelKind`'s
    /// own spelling, verbatim. The overlay tests it for `multilingual` to
    /// decide what the *active* language really is, so the exact
    /// word matters on both sides of the IPC boundary.
    pub model_kind: String,
    /// Microphone, as the OS describes it.
    pub device: String,
    /// The cue output device, or `None` when the output stream could not be
    /// opened at all.
    ///
    /// This is the device, not the switch: the player is opened
    /// whether or not cues are switched on, so that the Settings checkbox can
    /// be live. Whether anything is actually played is `audio_cues` on
    /// [`SettingsEvent`].
    pub cues: Option<String>,
    /// Compiled backend intent (`vulkan` / `cpu` / `metal`).
    pub backend: String,
    /// Whisper language, or `auto`.
    pub language: String,
    /// Human-readable binding summary.
    pub bindings: String,
    /// An extra sentence worth saying once — currently only "the models dir
    /// held exactly one model, so that is the one running".
    pub note: Option<String>,
}

/// Whether a key press would actually start a recording.
///
/// One field, and it is the whole truth: nothing else gates capture. Focus does
/// not, so the frontend has nothing to combine and nothing to recompute.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ArmedEvent {
    /// The Arm/Disarm button's state.
    pub user_armed: bool,
}

/// The recording indicator.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RecordingEvent {
    /// A recording is running right now.
    pub live: bool,
    /// `"push-to-talk"` or `"toggle"` while live.
    pub source: Option<&'static str>,
}

impl RecordingEvent {
    const IDLE: Self = Self {
        live: false,
        source: None,
    };
}

/// A line as the session bar shows it.
#[derive(Debug, Clone, Serialize)]
pub struct LineEvent {
    /// 1-based position in this session.
    pub n: usize,
    /// Local wall-clock time of the key release, `HH:MM:SS`.
    pub spoken_at: String,
    /// The transcript.
    pub text: String,
}

/// How loud the microphone is right now.
///
/// One number, already shaped for bar heights — 0.0 is the rest position and
/// 1.0 is a bar at full height. The mapping from raw audio (dB-mapped RMS, see
/// `audio::capture`'s `level_from_mean_square`) lives in exactly one place, in
/// sotone-core, so the window never does audio arithmetic.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct LevelEvent {
    /// 0.0–1.0, clamped at the source.
    pub level: f32,
}

/// One line of the transcript panel — stored or still on its way there.
///
/// Frontend-shaped, like every DTO here: the core [`LineRecord`] carries a
/// relative audio path and an RFC3339 timestamp, neither of which the window
/// has any use for.
///
/// One shape for both halves, deliberately: the design's state
/// model says a line is `{ …, status }` with five values, and the window draws
/// all five with one row renderer. A second DTO for the pending three would be
/// a second renderer, and the two would drift.
#[derive(Debug, Clone, Serialize)]
pub struct LineDto {
    /// Line ulid — what every line command takes. For a pending row this is
    /// the worker's intake **token**, which names no line on disk: nothing has
    /// been written yet, and the window renders no actions on those rows.
    pub id: String,
    /// Local wall-clock time of the key release, `HH:MM:SS`.
    pub spoken_at: String,
    /// The text as it now reads, with any edits folded in. Empty for a queued
    /// or transcribing row — there are no words yet, and guessing at them is
    /// exactly what the design forbids.
    pub text: String,
    /// What the model first produced, when the text has since changed. The
    /// audit trail Sotone promises, surfaced as a muted affordance rather than as
    /// a second line.
    pub original: Option<String>,
    /// Soft-deleted: shown struck through, and absent from the markdown.
    pub deleted: bool,
    /// Whether there is audio to play. True for every stored line — the store
    /// writes a wav per line, including an empty one for an empty utterance, so
    /// this is never stat'ed here and a read failure surfaces when the button
    /// is actually clicked. False for pending rows: their audio is in memory,
    /// not in a file the playback command could name.
    pub has_audio: bool,
    /// Which of the design's five row states this line is in:
    /// `"ok"` · `"failed"` for stored lines, `"queued"` · `"transcribing"` ·
    /// `"held"` for pending ones. `failed` is the *folded* state — an edit that
    /// supplied text has already cleared it.
    pub status: &'static str,
    /// How much audio was heard, in seconds. Only on pending rows, which show
    /// a duration instead of words.
    pub seconds: Option<f32>,
    /// Which draft a **held** line is waiting for, so the window shows it only
    /// while that note is open. `None` on stored lines (the event names their
    /// draft) and on queued/transcribing ones, which land in whatever draft is
    /// active when they are decoded.
    pub draft_id: Option<String>,
}

impl LineDto {
    /// A stored line: everything that only pending rows carry is absent.
    fn stored(line: &LineRecord) -> Self {
        Self {
            id: line.id.clone(),
            spoken_at: line.spoken_at.format("%H:%M:%S").to_string(),
            text: line.text.clone(),
            original: line.original.clone(),
            deleted: line.deleted,
            has_audio: true,
            status: if line.failed { "failed" } else { "ok" },
            seconds: None,
            draft_id: None,
        }
    }

    /// A row for something the worker is still holding.
    fn pending(id: String, spoken_at: String, status: &'static str, seconds: f32) -> Self {
        Self {
            id,
            spoken_at,
            text: String::new(),
            original: None,
            deleted: false,
            has_audio: false,
            status,
            seconds: Some(seconds),
            draft_id: None,
        }
    }
}

/// The active draft's transcript, whole.
#[derive(Debug, Clone, Serialize)]
pub struct LinesEvent {
    /// Which draft these belong to, so a snapshot that crossed a draft switch
    /// can be recognised as stale.
    pub draft_id: String,
    /// Every parseable line, oldest first, deleted ones included.
    pub lines: Vec<LineDto>,
}

/// Everything the worker is holding that is not on disk.
///
/// A state contract, not a stream of deltas (the stale-snapshot family): the shell
/// keeps the list and re-publishes **all** of it after every change, so the
/// window's idea of what is waiting cannot drift from the worker's. The list is
/// ephemeral by nature — nothing here is persisted, because a queued utterance
/// only exists between the microphone and the model.
#[derive(Debug, Clone, Serialize)]
pub struct PendingEvent {
    /// Oldest first, in the order the worker took them in.
    pub pending: Vec<LineDto>,
}

/// How loudly to show a notice — and **where it goes**.
///
/// The level is the routing rule and the window has no second copy of it:
/// `warn` and `error` land in the pane footer's message slot where the user
/// will read them; `info` and `debug` land only in the debug log, which is off
/// by default. So the question at every emission site is not "how bad is this"
/// but "does the user have to be told, or does visible state already say it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeLevel {
    /// Lifecycle traffic and confirmations whose result is already on screen:
    /// helper spawns and their hook scope, reconnect outcomes, model swaps,
    /// "saved to …", "X is now active". The debug log and nowhere else.
    Debug,
    /// Something that happened to the user's *dictation* and needs no answer: a
    /// skipped silent utterance, a recording split at the cap. Log only too —
    /// the missing landed-cue and the two lines in the transcript already say
    /// it live, while the user's eyes are on the thing under test — but kept
    /// apart from `debug` because a reader of the log cares which is which.
    Info,
    /// "Sotone refused, or could not do, the thing you just asked." The design's
    /// `--warn`: grayscale at full text brightness.
    Warn,
    /// Something was lost or failed outright. Same slot as `warn`, same rules —
    /// the difference is in the sentence, not in a colour.
    Error,
}

/// One transient sentence for the footer slot and the debug log.
#[derive(Debug, Clone, Serialize)]
pub struct NoticeEvent {
    /// Severity, which is also the routing — see [`NoticeLevel`].
    pub level: NoticeLevel,
    /// Already-flattened prose. The frontend is not a log sink; anything
    /// structured goes to `tracing` instead.
    pub message: String,
}

/// A state that *holds* until something ends it.
///
/// The variant order **is** the precedence rule — `Ord` is derived and
/// [`Conditions::top`] is the only place that reads it — so "which strip is on
/// screen when two of these hold" has one answer, decided once, rather than at
/// each of the eight emission sites. Deafness beats everything; a conflict can
/// wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    /// A save was stopped by an external edit. Nothing was written.
    FileConflict,
    /// Nothing is capturing: the input device is gone. **The only condition
    /// that disarms Sotone by itself**, which is the design's rule.
    NoDevice,
    /// No key press can be seen at all — the app's whole purpose is gone.
    HotkeyDead,
}

impl Condition {
    /// The wire spelling, which is also the window's `data-condition` value.
    /// camelCase because it is read as a JS discriminant, not as a config key.
    const fn as_str(self) -> &'static str {
        match self {
            Self::FileConflict => "fileConflict",
            Self::NoDevice => "noDevice",
            Self::HotkeyDead => "hotkeyDead",
        }
    }
}

/// Which conditions hold right now, each with the sentence it would show.
///
/// Three slots rather than one value, because several genuinely can hold at
/// once — a helper that died while a save was stopped by an external edit — and
/// a single slot would either lose the second one or resurrect a stale first
/// one when the second cleared. Exactly one is ever *published*; the rest wait.
#[derive(Debug, Clone, Default)]
struct Conditions {
    hotkey_dead: Option<String>,
    no_device: Option<String>,
    file_conflict: Option<String>,
}

impl Conditions {
    /// The slot one condition owns, for setting or clearing.
    fn slot(&mut self, condition: Condition) -> &mut Option<String> {
        match condition {
            Condition::HotkeyDead => &mut self.hotkey_dead,
            Condition::NoDevice => &mut self.no_device,
            Condition::FileConflict => &mut self.file_conflict,
        }
    }

    /// The one to show: F > A > D, exactly the [`Condition`] ordering.
    fn top(&self) -> Option<(Condition, &str)> {
        self.hotkey_dead
            .as_deref()
            .map(|detail| (Condition::HotkeyDead, detail))
            .or_else(|| {
                self.no_device
                    .as_deref()
                    .map(|detail| (Condition::NoDevice, detail))
            })
            .or_else(|| {
                self.file_conflict
                    .as_deref()
                    .map(|detail| (Condition::FileConflict, detail))
            })
    }

    /// What the window is told: the top condition, or nothing at all.
    fn event(&self) -> ConditionEvent {
        self.top()
            .map_or_else(ConditionEvent::default, |(condition, detail)| {
                ConditionEvent {
                    condition: Some(condition.as_str()),
                    detail: detail.to_owned(),
                }
            })
    }
}

/// The condition the window should be showing, if any.
///
/// A flat pair rather than a tagged union, for the reason [`SaveEvent`] is
/// flat: the window switches on one string, and `null` is the whole of "nothing
/// is wrong".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ConditionEvent {
    /// `"hotkeyDead"`, `"noDevice"`, `"fileConflict"` — or `null`.
    pub condition: Option<&'static str>,
    /// One truthful sentence about *this* occurrence: which device, which file,
    /// how many times the helper stopped. Empty when there is no condition.
    pub detail: String,
}

/// One outstanding draft, as the list shows it.
#[derive(Debug, Clone, Serialize)]
pub struct DraftDto {
    /// Draft ulid — the id every draft command takes.
    pub id: String,
    /// Creation time, RFC3339 with the offset. The window formats it; the
    /// backend does not decide what "short" looks like in the user's locale.
    pub created_at: String,
    /// Owning project, if any.
    pub project: Option<String>,
    /// Unsaved changes. Displayed from meta as-is; cleared by a successful
    /// save, which is why the drain thread re-lists after one.
    pub dirty: bool,
    /// Live, non-deleted lines.
    pub line_count: usize,
    /// The markdown file this draft is bound to, once it has been saved,
    /// **resolved** to an absolute path against its project's current notes
    /// folder. `None` before the first save — and also when the
    /// binding is relative and its project is gone from the config, in which
    /// case there is no true path to show and the tooltip names the problem
    /// instead of a wrong file.
    pub saved_path: Option<String>,
}

/// One project, as the Projects tab shows it.
///
/// Strings, not paths and not `Option<PathBuf>`: the window puts these straight
/// into inputs, and an unset template is an empty field there.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectDto {
    /// Unique key, and the `{project}` token's value.
    pub name: String,
    /// Where this project's notes are written. Empty until one is chosen, and a
    /// save is refused while it is.
    pub notes_dir: String,
    /// Filename template, tokens unexpanded.
    pub filename_template: String,
    /// Header template, tokens unexpanded. Empty when there is none.
    pub header_template: String,
}

/// A view the user asked for from outside the window.
///
/// One string, because there is one thing outside the window that can ask: the
/// tray's Settings item. It is a *request*, not state — the window swaps and
/// forgets, and nothing here remembers that it was sent, which is why it is
/// deliberately absent from [`ShellStatus`]: a reload must not re-open Settings
/// because the tray asked for it ten minutes ago.
#[derive(Debug, Clone, Serialize)]
pub struct ViewEvent {
    /// Currently only [`VIEW_SETTINGS`].
    pub view: &'static str,
}

/// The project list, and which one is active.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProjectsEvent {
    /// Every project in the config, in file order.
    pub projects: Vec<ProjectDto>,
    /// The active project's name, if the config names one that exists.
    pub active: Option<String>,
}

// ---------------------------------------------------------------------------
// Settings
//
// The Settings tab is an editor over the config file, exactly as the Projects
// tab is: every one of these carries what the *file* says, and `ReadyInfo`
// carries what the running process has. Both are still published, because the
// two are still different questions ("what will the next launch do" versus
// "what is loaded right now") — but they cannot *disagree*: every
// row applies live, and the readout is corrected the moment it does. There is
// no restart-pending computation anywhere any more, in this file or in the
// window.
// ---------------------------------------------------------------------------

/// One recording mode's binding, as the Settings tab shows it.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HotkeyDto {
    /// `"ptt"` or `"toggle"` — the same word every hotkey command takes.
    pub mode: String,
    /// The canonical token, exactly as it is written in the config file.
    pub token: String,
    /// The same binding for a human to read.
    pub label: String,
    /// Whether this mode is switched on at all.
    pub enabled: bool,
}

/// One input device the user could pin the microphone to.
#[derive(Debug, Clone, Serialize)]
pub struct InputDeviceDto {
    /// The OS description, which is also exactly what `mic_substring` gets set
    /// to — the full string, not a fragment of it, so the pin cannot silently
    /// match a device the user did not choose.
    pub name: String,
    /// Whether the OS currently calls this the default input.
    pub is_default: bool,
}

/// One usable model in the models folder.
#[derive(Debug, Clone, Serialize)]
pub struct ModelDto {
    /// File name, which is also what `active_model` holds.
    pub name: String,
    /// Size on disk, for the "is this the big one" question.
    pub size_bytes: u64,
    /// From the GGML header: whether a language other than English is possible.
    pub multilingual: bool,
    /// Whether the config names this one.
    pub active: bool,
}

/// A `.bin` file in the models folder that is not a usable model.
///
/// Shown with its reason rather than hidden: the empty state is the
/// onboarding, and this list is where a bad download gets to explain itself
/// instead of just failing to appear.
#[derive(Debug, Clone, Serialize)]
pub struct RejectedModelDto {
    /// File name.
    pub name: String,
    /// Why it is not usable, in the words the validator uses.
    pub reason: String,
}

/// One entry of the language picker.
///
/// Shipped from Rust rather than written into the window, because the list is
/// whisper.cpp's own table read at runtime (`transcribe::language_options`): a
/// copy in JavaScript would go stale the first time whisper gains a language,
/// and this app has no hardcoded model list for the same reason.
#[derive(Debug, Clone, Serialize)]
pub struct LanguageDto {
    /// What the config stores and whisper is given: `auto`, or a code.
    pub code: String,
    /// What the row shows.
    pub label: String,
}

/// Everything the Settings tab edits, plus what capture is doing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SettingsEvent {
    /// Hold-to-talk.
    pub ptt: HotkeyDto,
    /// Press-once, press-again.
    pub toggle: HotkeyDto,
    /// The pinned microphone name, or empty for "whatever the system default
    /// is". Never an index.
    pub mic_substring: String,
    /// Whether cues are played. Live — no restart.
    pub audio_cues: bool,
    /// Whether the overlay is wanted.
    pub overlay: bool,
    /// Which corner the pill docks to: `"bottomLeft"` (the default),
    /// `"bottomRight"`, `"topLeft"` or `"topRight"`.
    ///
    /// The overlay window reads this too — it decides which way the pill grows
    /// and which edge the logo is pinned to — so the placement in Rust and the
    /// layout in CSS are two halves of one value, not two settings.
    pub overlay_corner: &'static str,
    /// How long a transcribed line stays on the pill, in seconds. Always within
    /// the configured range, because the clamp runs where the value is read.
    pub reveal_seconds: u32,
    /// `"dark"` or `"light"` — which palette the window draws in.
    /// Live: the window applies it the moment this event lands.
    pub theme: &'static str,
    /// Whether the window's X quits Sotone instead of hiding it to the tray.
    /// Off by default.
    ///
    /// A readout, exactly like the two above it: nothing in the window acts on
    /// this value, because the close decision is made in [`on_close`] from the
    /// configuration at the moment the X is pressed. This is only what the
    /// Settings row draws itself from.
    pub close_quits: bool,
    /// Whether soft-deleted lines are kept out of the transcript view.
    /// Off by default.
    ///
    /// Unlike the readout above it, the window *acts* on this one: it is the
    /// seed for the view filter the window keeps in a module-level flag, so a
    /// relaunch starts where the last session left off. Nothing in the store
    /// or on disk depends on it — the note keeps every line either way.
    pub hide_deleted: bool,
    /// `"windows"`, `"macos"` or `"linux"`, from the compile-time target.
    ///
    /// The window draws a different caption, corner radius and font stack per
    /// platform (the design specifies window chrome per platform), and it must
    /// not have to sniff a user agent to find out which: this is the answer,
    /// decided where the answer is actually known.
    pub platform: &'static str,
    /// What version of Sotone this is, for the About tab.
    ///
    /// From the backend rather than written into the page, for the same reason
    /// the platform is: the build knows, and a string in `index.html` would go
    /// stale the first time nobody remembered to edit it. See [`VERSION`].
    pub version: &'static str,
    /// The model the config names, if any.
    pub active_model: Option<String>,
    /// The model a swap is loading right now, if one is.
    ///
    /// A fact about this process, published like `capture` is and for the same
    /// reason: "loading…" is the state of a thread, not of the window, so a
    /// reload or a re-render can never strand a row in it — and a load that
    /// fails clears it by sending this event with `None`.
    pub model_loading: Option<String>,
    /// Where models are looked for.
    pub models_dir: String,
    /// Every usable model in there.
    pub models: Vec<ModelDto>,
    /// Every `.bin` in there that is not one, with the reason.
    pub rejected_models: Vec<RejectedModelDto>,
    /// The configured transcription language: a code, or `auto`. Live — no
    /// restart.
    ///
    /// This is what the config says. What is actually in force is
    /// [`ReadyInfo::language`], which additionally answers a project override
    /// and — through `model_kind` — an English-only model.
    pub language: String,
    /// Every language that can be chosen, auto first.
    pub languages: Vec<LanguageDto>,
    /// `"idle"` or `"capturing"`.
    pub capture: &'static str,
    /// Which mode is being rebound, while `capture` is `"capturing"`.
    pub capture_mode: Option<String>,
}

impl SettingsEvent {
    /// Idle, and empty of everything else. Startup's placeholder until the
    /// config has been read.
    ///
    /// The theme and the platform are still real here: this payload is what the
    /// window renders from while the config is being read, and a blank platform
    /// would paint the wrong chrome for a frame.
    fn idle() -> Self {
        Self {
            capture: CAPTURE_IDLE,
            theme: Theme::default().as_str(),
            platform: PLATFORM,
            // Real in the placeholder too, for the same reason the platform is:
            // the About tab can be open before the config has been read, and a
            // version that appears a moment later reads as a bug.
            version: VERSION,
            // Real for the same reason the theme is: the overlay page renders
            // from this payload before the config has been read, and a zero
            // reveal duration would be a drain that finished before it started.
            overlay_corner: OverlayCorner::default().as_str(),
            reveal_seconds: DEFAULT_REVEAL_SECONDS,
            language: AUTO_LANGUAGE.to_owned(),
            languages: languages(),
            ..Self::default()
        }
    }
}

/// whisper's language table, read once.
///
/// Read out of whisper.cpp itself (see `transcribe::language_options`), so it
/// cannot go stale — and cached, because the settings event is emitted after
/// every mutation and rebuilding a hundred owned strings each time would be
/// silly. The table cannot change while the process runs.
fn languages() -> Vec<LanguageDto> {
    static LANGUAGES: OnceLock<Vec<LanguageDto>> = OnceLock::new();
    LANGUAGES
        .get_or_init(|| {
            language_options()
                .into_iter()
                .map(|option| LanguageDto {
                    code: option.code,
                    label: option.label,
                })
                .collect()
        })
        .clone()
}

/// Which chrome the window draws, decided at compile time.
///
/// Not a runtime probe and not a user-agent sniff: the app is built for one
/// target and the window is told which. Every non-Windows, non-macOS target
/// gets the GNOME chrome, which is the design's third variant.
#[cfg(target_os = "windows")]
const PLATFORM: &str = "windows";
/// See [`PLATFORM`].
#[cfg(target_os = "macos")]
const PLATFORM: &str = "macos";
/// See [`PLATFORM`].
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const PLATFORM: &str = "linux";

/// This build's version, for the About tab.
///
/// `CARGO_PKG_VERSION` — the `sotone` crate's, which the workspace sets — rather
/// than Tauri's `PackageInfo`, for two reasons. It is a `&'static str` needing
/// no `AppHandle`, so the placeholder payload
/// ([`SettingsEvent::idle`], built before there is a config, let alone an app
/// handle) can carry the real version instead of an empty string; and the two
/// answers are the same one anyway, because
/// `the_version_agrees_with_the_bundle_manifest` fails the build's tests if
/// `tauri.conf.json` and `Cargo.toml` ever drift. That test is the important
/// half: the installer, and `release.yml`'s artifact name, come from
/// `tauri.conf.json`, so a version shown here that disagreed with it would be
/// this app lying about which build it is.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `capture` field's two values, named so the window and this file cannot
/// drift on the spelling.
const CAPTURE_IDLE: &str = "idle";
/// See [`CAPTURE_IDLE`].
const CAPTURE_LISTENING: &str = "capturing";

/// A directory under the drafts root that could not be read.
///
/// Shown rather than swallowed: a draft that silently fails to appear looks to
/// the user exactly like lost work.
#[derive(Debug, Clone, Serialize)]
pub struct RejectedDraftDto {
    /// Where it is, so the user can go and look.
    pub path: String,
    /// Why it did not load.
    pub reason: String,
}

/// The outstanding-drafts list.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DraftsEvent {
    /// Every loadable draft, oldest first.
    pub drafts: Vec<DraftDto>,
    /// Every directory that looked like a draft and was not.
    pub rejected: Vec<RejectedDraftDto>,
    /// Which one lines are landing in, if any. `None` on a fresh launch: no
    /// draft is created until a line needs somewhere to go.
    pub active_id: Option<String>,
    /// Where a first save would land: the active project's notes folder,
    /// recomputed from the current configuration on every emission. Empty when
    /// no project is active.
    pub default_save_dir: String,
}

/// What one Save did.
///
/// Flat rather than an externally-tagged enum: the window switches on one
/// string, and a shape the frontend cannot mis-destructure is worth more here
/// than a tidy union.
#[derive(Debug, Clone, Serialize)]
pub struct SaveEvent {
    /// `"saved"`, `"conflict"`, `"error"` or `"no_project"`.
    ///
    /// `no_project` carries no other field: notes exist only within projects,
    /// nothing was written, and the window answers it with the
    /// pick-or-create popup rather than with a message.
    pub outcome: &'static str,
    /// The file involved, on `saved` and `conflict`.
    pub path: Option<String>,
    /// Bullets written, on `saved`.
    pub lines: Option<usize>,
    /// Why not, on `error`.
    pub message: Option<String>,
    /// What is on disk now, on `conflict`. Empty when it could not be read.
    pub disk_text: Option<String>,
    /// What the save would have written, on `conflict`. **Nothing was written**
    /// — this is the right-hand side of the diff, not a report of a write.
    pub pending_markdown: Option<String>,
}

impl SaveEvent {
    /// Everything absent, so each constructor below only names what it has.
    const fn of(outcome: &'static str) -> Self {
        Self {
            outcome,
            path: None,
            lines: None,
            message: None,
            disk_text: None,
            pending_markdown: None,
        }
    }
}

/// What one Save all did.
///
/// Flat, and paths as display strings, for the same reason [`SaveEvent`] is:
/// the window shows one sentence built from three numbers, and a shape it
/// cannot mis-destructure is worth more than a tidy union.
///
/// There is deliberately **no** overwrite answer to this event. A batch cannot
/// show a diff, so it can never be given permission to discard what somebody
/// else wrote — every conflict listed here is resolved one note at a time
/// through the ordinary Save and its dialog (invariant 4).
#[derive(Debug, Clone, Serialize)]
pub struct SaveAllEvent {
    /// How many notes were written.
    pub saved: usize,
    /// How many dirty drafts had no project to be saved into.
    ///
    /// The batch is store-wide now, so it meets drafts that are not notes: no
    /// project, or one that is no longer configured, or one with no folder
    /// chosen. Nothing failed and nothing was written for them — but the window
    /// says the number, because a sentence that counts only what it saved while
    /// unsaved dots are still lit would read as a lie.
    pub skipped: usize,
    /// The notes an external edit stopped. Nothing was written for any of them.
    pub conflicts: Vec<String>,
    /// Anything else that went wrong, one message per note.
    pub errors: Vec<String>,
}

/// A drop that has a question to ask.
///
/// Everything the dialog needs to state the clash *and* to send the answer:
/// the answer is the same `draft_set_project` with `keep_both`, so the draft
/// and the destination have to come back with the question rather than being
/// remembered by the window. Flat display strings, like every other DTO here.
///
/// `suggestion` is what "keep both" would name the file **as of the moment the
/// question was asked**. The move recomputes the first free name when the
/// answer arrives, so a file that appeared in between changes the outcome and
/// not the promise; the footer's notice names what was actually written.
#[derive(Debug, Clone, Serialize)]
pub struct NoteClashEvent {
    /// The draft the drop was about.
    pub id: String,
    /// Where it was headed. `None` cannot clash — there is no folder — so this
    /// is always a real project in practice.
    pub project: Option<String>,
    /// The occupied file's name, as the folder lists it.
    pub name: String,
    /// The full occupied path, for the dialog's second line.
    pub path: String,
    /// What Keep both would call it, or `None` if even the numbering ran out.
    pub suggestion: Option<String>,
}

/// One matching line of one note.
///
/// Deliberately **not** a [`LineDto`]: a search result is a reading, not a row.
/// The window draws the pane's rows from the transcript it already has — that
/// is where `status`, `deleted` and the audio live — and uses these to know
/// *which* lines matched and to preview a note it has no transcript for.
#[derive(Debug, Clone, Serialize)]
pub struct SearchLineDto {
    /// Line ulid, so the window can intersect this with the transcript it holds.
    pub id: String,
    /// Local wall-clock time of the key release, `HH:MM:SS` — formatted exactly
    /// as [`LineDto::stored`] formats it, because the same rows show both.
    pub spoken_at: String,
    /// The line as it now reads, folded. What the preview shows and what the
    /// term is marked inside.
    pub text: String,
}

/// One note that contains the term.
#[derive(Debug, Clone, Serialize)]
pub struct SearchNoteDto {
    /// Draft ulid. The window already has this draft's name, project and dirty
    /// flag from `sotone://drafts`, so nothing here repeats them: two sources for
    /// one note's name is how a tree and a list end up disagreeing.
    pub draft_id: String,
    /// How many **lines** matched. Never a count of occurrences — a line that
    /// says "table" three times is one match, in the tree and in the pane alike.
    pub matches: usize,
    /// The matching lines themselves, in transcript order.
    pub lines: Vec<SearchLineDto>,
    /// When this note was last written to: the newest live line's `spoken_at`,
    /// or the draft's `created_at` if it has no live lines. RFC3339 with the
    /// offset, like [`DraftDto::created_at`] — what "today 14:39" looks like is
    /// the window's locale's business, not the backend's.
    pub last_written: String,
}

/// What one scan found.
///
/// A command answer, not an event: search is a question the window asks and
/// nothing in the backend remembers it was asked. There is no search state
/// anywhere in this file, which is also why nothing here can go stale.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchOutcome {
    /// The term as it was actually matched — trimmed and lowercased — so a
    /// window that raced two keystrokes can tell which answer it is holding.
    pub term: String,
    /// Every note with at least one match, in store order (ulid ascending).
    /// The window sorts them by whatever the sidebar sort says.
    pub notes: Vec<SearchNoteDto>,
    /// Total matching lines across every note.
    pub matches: usize,
}

/// What one create did.
///
/// A command answer like [`SearchOutcome`], and one word on the wire —
/// `"created"` / `"refused"` — for the reason [`SaveEvent`] carries one: the
/// window switches on a string, and a shape it cannot mis-destructure is worth
/// more than a tidy union.
///
/// It exists because a refusal resolves exactly as an acceptance does, so a
/// creation surface had no way to tell the two apart short of reading notices.
/// All three read the resolved promise as a yes: the Projects pane cleared its
/// form and left the pane, the pick-or-create popup closed and came back blank,
/// and the wizard stepped on to "ready" with no project.
///
/// Every refusal still emits its notice. The outcome is **in addition to** the
/// sentence, never instead of it — and it is deliberately not an `Err`, which
/// this window reports as a failure. A name already taken, a blank field, a
/// folder that could not be made and a live recording are ordinary answers with
/// a sentence of their own; only a configuration that cannot be written is a
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateOutcome {
    /// The project is in the configuration and it is the active one.
    Created,
    /// Nothing was written and nothing changed. A notice says why.
    Refused,
}

/// Everything the frontend would otherwise have had to catch live.
#[derive(Debug, Clone, Serialize)]
pub struct ShellStatus {
    /// Current status.
    pub status: StatusEvent,
    /// Current armed state.
    pub armed: ArmedEvent,
    /// Current recording state.
    pub recording: RecordingEvent,
    /// Lines in the active draft.
    pub lines: usize,
    /// The most recent line, if any.
    pub last_line: Option<LineEvent>,
    /// The outstanding drafts.
    pub drafts: DraftsEvent,
    /// The active draft's transcript, so a webview that finishes loading after
    /// the last `sotone://lines` fired still renders the lines it can edit.
    /// `None` when no draft is active.
    pub transcript: Option<LinesEvent>,
    /// What the worker is still holding, for the same reason the
    /// transcript is here: a window reloaded while three utterances are queued
    /// must not come back showing none of them.
    pub pending: PendingEvent,
    /// The projects, so the Projects tab renders on a cold load.
    pub projects: ProjectsEvent,
    /// The settings, for the same reason — including the capture state, so a
    /// window reloaded mid-capture shows "press a key…" rather than a Settings
    /// tab that looks idle while the helper is still listening.
    pub settings: SettingsEvent,
    /// The condition that holds right now, if one does.
    ///
    /// Carried here for the reason every other event is: a condition is
    /// permanent, so a webview that finished loading after `sotone://condition`
    /// fired must still find out the app is deaf. A reload that lost the strip
    /// would be the same bug the rolling notice list had.
    pub condition: ConditionEvent,
}

// ---------------------------------------------------------------------------
// Managed state
// ---------------------------------------------------------------------------

/// The mutable part of the snapshot. The armed flag lives outside it as an
/// atomic so the commands never wait on a lock held by an emitting thread.
struct Snapshot {
    status: StatusEvent,
    recording: RecordingEvent,
    lines: usize,
    last_line: Option<LineEvent>,
    drafts: DraftsEvent,
    transcript: Option<LinesEvent>,
    /// Queued, transcribing and held rows, in intake order. Ephemeral: it
    /// lives exactly as long as the worker's own queues do.
    pending: Vec<LineDto>,
    projects: ProjectsEvent,
    settings: SettingsEvent,
    /// Which conditions hold. Under the snapshot lock like everything else it
    /// sits beside, so "mutate and re-derive what to publish" is one critical
    /// section and two threads cannot interleave a set with a clear.
    conditions: Conditions,
}

struct Inner {
    /// The Arm/Disarm button. Starts armed: the user launched a dictation app.
    /// The only thing that gates capture.
    ///
    /// The one exception is a first run: a launch that still has to
    /// show the wizard — and the single launch straight after it — starts
    /// **disarmed**, because the wizard's last screen says so in as many words.
    /// See [`ShellState::new`]'s caller.
    user_armed: AtomicBool,
    /// To the control thread. `mpsc::Sender` is `Send` but not `Sync`, and
    /// managed state must be both — hence the mutex, which is only ever held
    /// for the duration of a send.
    inputs: Mutex<Sender<ShellInput>>,
    /// To the overlay thread, same shape and same reason.
    ///
    /// Separate from `inputs` because the overlay's work is *timed* — a reveal
    /// runs out on its own — and because a window call must never wait behind a
    /// model load or a save on the control thread's queue.
    overlay: Mutex<Sender<OverlayInput>>,
    /// To the tray thread, same shape again.
    ///
    /// Separate from `inputs` for a third reason on top of those two: the tray
    /// exists in the fatal phase, where there *is* no control thread to receive
    /// anything, and in the empty phase, where the one that exists answers a
    /// rebind and nothing else.
    tray: Mutex<Sender<TrayInput>>,
    /// Whether there is a tray icon at all.
    ///
    /// Load-bearing, not diagnostic: [`on_close`] hides the window instead of
    /// closing it, and hiding to a tray that failed to build would leave a
    /// running app with no way back to it. False until [`start`] says otherwise.
    tray_alive: AtomicBool,
    snapshot: Mutex<Snapshot>,
    /// Whether the last drafts re-scan failed. A latch, not a counter: the list
    /// is re-scanned after *every* appended line, every edit and every save, so
    /// an unreadable drafts folder used to produce one identical warning per
    /// call — a flood that pushed everything else out of the notice list.
    /// One sentence per episode, and a successful
    /// scan re-arms it.
    drafts_unreadable: AtomicBool,
    /// The drafts root, published by `init` once it exists. Holds no [`Draft`]
    /// handle — [`DraftStore`] is a path and nothing else — so the playback
    /// command can resolve a line's wav without going anywhere near the handle
    /// the worker owns.
    store: Mutex<Option<DraftStore>>,
    /// The configuration, and the file it came from. `None` until startup has
    /// read it.
    ///
    /// **One serialized owner**: every read and every mutation takes
    /// this lock, and a mutation saves the file while still holding it, so two
    /// commands cannot interleave a read-modify-write. The lock is never held
    /// across an `await` and never while emitting.
    config: Mutex<Option<ConfigOwner>>,
    /// The filename a *first* save of one draft will use, decided once and
    /// remembered as `(draft id, path)`. See [`ShellState::fallback_save_path`].
    pending_save: Mutex<Option<(String, PathBuf)>>,
    /// Whether cues are audible. An atomic rather than a read of the config,
    /// because the begin cue is played by the control thread on the recording
    /// path, and that path may not wait on the configuration lock (invariant 5
    /// in spirit). The worker keeps its own copy, sent as
    /// [`SessionCommand::SetCues`]; both are set from the same command.
    cues_enabled: AtomicBool,
    /// Which mode, if any, is being rebound right now. `None` is idle.
    ///
    /// Written only by the control thread, which owns the capture helper, and
    /// read by anything that needs the settings snapshot.
    capture: Mutex<Option<HotkeyMode>>,
    /// The model a swap is loading right now, if any.
    ///
    /// **One load at a time**, and this is what enforces it: a whisper model is
    /// hundreds of megabytes and seconds of work, and two loads in flight would
    /// mean two of them resident at once, in a race to decide which one the
    /// worker ends up with. The claim is taken under this lock, so two clicks
    /// cannot both win it.
    model_loading: Mutex<Option<String>>,
}

/// The configuration and where it lives.
struct ConfigOwner {
    path: PathBuf,
    config: Config,
}

/// Shared shell state: managed by Tauri for the commands, cloned into the
/// control and drain threads.
#[derive(Clone)]
pub struct ShellState {
    inner: Arc<Inner>,
}

/// A mutex we only ever hold for a field assignment cannot leave a broken
/// invariant behind, so poisoning is not interesting here — and losing the
/// session bar because a thread panicked elsewhere would be the worse bug.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl ShellState {
    fn new(
        inputs: Sender<ShellInput>,
        overlay: Sender<OverlayInput>,
        tray: Sender<TrayInput>,
        armed: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                // `armed` is false on a first run and on the one launch after
                // it, and true — as it always has been — on every other
                // launch.
                user_armed: AtomicBool::new(armed),
                inputs: Mutex::new(inputs),
                overlay: Mutex::new(overlay),
                tray: Mutex::new(tray),
                tray_alive: AtomicBool::new(false),
                snapshot: Mutex::new(Snapshot {
                    status: StatusEvent::loading("starting"),
                    recording: RecordingEvent::IDLE,
                    lines: 0,
                    last_line: None,
                    drafts: DraftsEvent::default(),
                    transcript: None,
                    pending: Vec::new(),
                    projects: ProjectsEvent::default(),
                    settings: SettingsEvent::idle(),
                    conditions: Conditions::default(),
                }),
                drafts_unreadable: AtomicBool::new(false),
                store: Mutex::new(None),
                config: Mutex::new(None),
                pending_save: Mutex::new(None),
                cues_enabled: AtomicBool::new(true),
                capture: Mutex::new(None),
                model_loading: Mutex::new(None),
            }),
        }
    }

    /// Publish the drafts root, once startup has one.
    fn set_store(&self, store: DraftStore) {
        *lock(&self.inner.store) = Some(store);
    }

    /// A clone of the drafts root, or `None` while startup is still running.
    fn store(&self) -> Option<DraftStore> {
        lock(&self.inner.store).clone()
    }

    /// Hand the configuration over to the shell, once startup has read it.
    fn set_config(&self, path: PathBuf, config: Config) {
        *lock(&self.inner.config) = Some(ConfigOwner { path, config });
    }

    /// Read something out of the configuration. `None` while startup is still
    /// running.
    ///
    /// The lock is held for the length of `read` and nothing else, so a slow
    /// caller cannot stall a save.
    fn with_config<R>(&self, read: impl FnOnce(&Config) -> R) -> Option<R> {
        lock(&self.inner.config)
            .as_ref()
            .map(|owner| read(&owner.config))
    }

    /// Mutate the configuration and write it back, under one lock.
    ///
    /// The save goes through [`Config::save`], which folds the change into the
    /// *same* `toml_edit` document the file was parsed into: comments, key
    /// order and keys this version has never heard of all survive byte for byte
    /// (invariant 4 in spirit — a config that loses the user's comments is a
    /// destroyed user file).
    ///
    /// An `edit` that returns `Err` is a refusal: nothing is written at all.
    fn edit_config<R>(
        &self,
        edit: impl FnOnce(&mut Config) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut guard = lock(&self.inner.config);
        let owner = guard
            .as_mut()
            .ok_or_else(|| "Sotone is still starting up".to_owned())?;
        let out = edit(&mut owner.config)?;
        owner
            .config
            .save(&owner.path)
            .map_err(|err| format!("{err}"))?;
        Ok(out)
    }

    /// The project list as the window sees it.
    fn projects_event(&self) -> ProjectsEvent {
        self.with_config(|config| ProjectsEvent {
            projects: config
                .projects
                .iter()
                .map(|project| ProjectDto {
                    name: project.name.clone(),
                    notes_dir: project.notes_dir.display().to_string(),
                    filename_template: project.filename_template.clone(),
                    header_template: project.header_template.clone().unwrap_or_default(),
                })
                .collect(),
            // The name is only reported when it still resolves: an
            // `active_project` naming a project that was hand-deleted is not an
            // active project.
            active: config.active_project().map(|p| p.name.clone()),
        })
        .unwrap_or_default()
    }

    /// Everything the Settings tab shows, read fresh from the configuration and
    /// the models folder.
    ///
    /// The models scan is real filesystem work — a directory listing plus a
    /// 48-byte header read per `.bin` — so this is only ever called from an
    /// async command, the control thread or the drain thread. The result is
    /// kept in the snapshot, which is what the (synchronous) `shell_status`
    /// hands back.
    fn settings_event(&self) -> SettingsEvent {
        let capture = *lock(&self.inner.capture);
        let model_loading = lock(&self.inner.model_loading).clone();

        let Some(mut settings) = self.with_config(|config| SettingsEvent {
            ptt: HotkeyDto {
                mode: HotkeyMode::Ptt.as_str().to_owned(),
                token: config.hotkey.clone(),
                label: binding_label(&config.hotkey),
                enabled: config.ptt_enabled,
            },
            toggle: HotkeyDto {
                mode: HotkeyMode::Toggle.as_str().to_owned(),
                token: config.toggle_hotkey.clone(),
                label: binding_label(&config.toggle_hotkey),
                enabled: config.toggle_enabled,
            },
            mic_substring: config.mic_substring.clone().unwrap_or_default(),
            audio_cues: config.audio_cues,
            overlay: config.overlay,
            overlay_corner: config.overlay_corner.as_str(),
            // Clamped on the way out as well as on the way in: the window's
            // control lists the durations it offers, and a hand-edited file
            // must not make it show one the pill would not honour.
            reveal_seconds: clamp_reveal_seconds(config.reveal_seconds),
            theme: config.theme.as_str(),
            close_quits: config.close_quits,
            hide_deleted: config.hide_deleted,
            platform: PLATFORM,
            version: VERSION,
            active_model: config.active_model.clone(),
            model_loading,
            models_dir: config.models_dir.display().to_string(),
            models: Vec::new(),
            rejected_models: Vec::new(),
            // The global, which is what the row edits. A project override — a
            // config-file-only field — is honoured by `effective_language`
            // wherever the language is actually *used*, and shows up in the
            // ready readout rather than in this control.
            language: config.language.clone(),
            languages: languages(),
            capture: CAPTURE_IDLE,
            capture_mode: None,
        }) else {
            return SettingsEvent::idle();
        };

        let models_dir = PathBuf::from(&settings.models_dir);
        let active_model = settings.active_model.clone();
        match scan_models_dir(&models_dir) {
            Ok(scan) => {
                settings.models = scan
                    .models
                    .into_iter()
                    .map(|info| {
                        let name = file_name(&info.path);
                        ModelDto {
                            active: active_model.as_deref() == Some(name.as_str()),
                            name,
                            size_bytes: info.size_bytes,
                            multilingual: info.kind.is_multilingual(),
                        }
                    })
                    .collect();
                settings.rejected_models = scan
                    .rejected
                    .into_iter()
                    .map(|(path, err)| RejectedModelDto {
                        name: file_name(&path),
                        reason: err.to_string(),
                    })
                    .collect();
            }
            // An unreadable models folder is not worth failing the whole panel
            // over: everything else in Settings still works, and the empty list
            // plus this reason is what the user needs to see.
            Err(err) => {
                settings.rejected_models.push(RejectedModelDto {
                    name: settings.models_dir.clone(),
                    reason: err.to_string(),
                });
            }
        }

        if let Some(mode) = capture {
            settings.capture = CAPTURE_LISTENING;
            settings.capture_mode = Some(mode.as_str().to_owned());
        }
        settings
    }

    /// Publish the settings. Called on startup and after every mutation and
    /// capture state change, so the window never has to infer one.
    fn refresh_settings(&self, app: &AppHandle) {
        let settings = self.settings_event();
        lock(&self.inner.snapshot).settings = settings.clone();
        emit(app, EVENT_SETTINGS, &settings);
    }

    /// Record which mode is being rebound. `None` is idle.
    fn set_capture(&self, mode: Option<HotkeyMode>) {
        *lock(&self.inner.capture) = mode;
    }

    /// Take the one model-load slot, or say who has it.
    ///
    /// Test-and-set under one lock, deliberately: the check and the claim have
    /// to be the same act, or two clicks a millisecond apart both find the slot
    /// free and both start loading.
    ///
    /// # Errors
    /// The sentence to show, when a load is already running.
    fn claim_model_load(&self, name: &str) -> Result<(), String> {
        let mut slot = lock(&self.inner.model_loading);
        if let Some(loading) = slot.as_deref() {
            return Err(loading_refusal(loading));
        }
        *slot = Some(name.to_owned());
        Ok(())
    }

    /// Give the slot back, whatever the load did.
    fn release_model_load(&self) {
        *lock(&self.inner.model_loading) = None;
    }

    /// Whether a rebind is in progress.
    fn capturing(&self) -> Option<HotkeyMode> {
        *lock(&self.inner.capture)
    }

    /// Whether cues are audible right now.
    fn cues_enabled(&self) -> bool {
        self.inner.cues_enabled.load(Ordering::Relaxed)
    }

    /// Set the control thread's half of the cue switch. The worker's half is
    /// [`SessionCommand::SetCues`], sent alongside.
    fn set_cues_enabled(&self, on: bool) {
        self.inner.cues_enabled.store(on, Ordering::Relaxed);
    }

    /// The bindings the configuration currently describes.
    ///
    /// The same two rules `resolve_bindings` applies at startup, restated as a
    /// refusal rather than a fatal error: by the time this is asked, the app is
    /// running, and the answer decides what a respawned helper watches.
    ///
    /// # Errors
    /// A sentence for the notice area, when the config names no usable binding.
    fn bindings(&self) -> Result<Bindings, String> {
        self.with_config(|config| {
            let parse = |token: &str| token.parse::<Binding>().map_err(|err| err.to_string());
            let ptt = config
                .ptt_enabled
                .then(|| parse(&config.hotkey))
                .transpose()?;
            let toggle = config
                .toggle_enabled
                .then(|| parse(&config.toggle_hotkey))
                .transpose()?;
            let bindings = Bindings { ptt, toggle };
            if bindings.is_empty() {
                return Err(
                    "no recording mode is enabled, so there is nothing to listen for".to_owned(),
                );
            }
            Ok(bindings)
        })
        .unwrap_or_else(|| Err("Sotone is still starting up".to_owned()))
    }

    /// The active project's name, for tagging a draft at creation.
    fn active_project_name(&self) -> Option<String> {
        self.with_config(|config| config.active_project().map(|p| p.name.clone()))
            .flatten()
    }

    /// The project that governs a save of a draft whose meta names
    /// `draft_project`.
    ///
    /// The draft's own project when the config still has one by that name —
    /// that is what keeps a note in the project it was dictated for — else the
    /// active project, which is the adoption path. `None` means there is no
    /// project at all, and the save is refused: notes exist only within
    /// projects.
    fn governing_project(&self, draft_project: Option<&str>) -> Option<Project> {
        self.with_config(|config| {
            draft_project
                .and_then(|name| config.project(name))
                .or_else(|| config.active_project())
                .cloned()
        })
        .flatten()
    }

    /// Every project a batch may write into, with its own folder and
    /// templates.
    ///
    /// Save all is about the *store*, not about a draft and — by design — not
    /// about the active project either, so this resolves neither
    /// [`ShellState::governing_project`] nor the active one:
    /// it hands over the whole configuration's worth of save rules and lets the
    /// worker match each dirty draft to the project it already names.
    ///
    /// Two things happen here rather than there, both for the same reason — the
    /// worker never reads configuration:
    ///
    /// * the header template is **expanded**, in text mode, because this is the
    ///   boundary that has the clock (the [`SessionCommand::Save`] rule);
    /// * a project with no folder chosen is left out entirely, so no batch can
    ///   resolve a note's path against an empty root. Its dirty drafts come
    ///   back counted as skipped, which is the honest answer: there is nowhere
    ///   for them to go until the user picks a folder.
    ///
    /// The filename template travels **unexpanded**: it is per-draft, and the
    /// worker expands it at each note's own save moment.
    fn save_contexts(&self) -> Vec<ProjectSaveContext> {
        self.with_config(|config| {
            config
                .projects
                .iter()
                .filter(|project| !project.notes_dir.as_os_str().is_empty())
                .map(|project| ProjectSaveContext {
                    project: project.name.clone(),
                    notes_root: project.notes_dir.clone(),
                    filename_template: project.filename_template.clone(),
                    header: project
                        .header_template
                        .as_deref()
                        .map(|template| template::expand_text_now(template, &project.name)),
                    dividers: project.session_dividers,
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// Every project's notes folder by name, for resolving relative bindings
    /// into the paths the window displays.
    fn project_dirs(&self) -> HashMap<String, PathBuf> {
        self.with_config(|config| {
            config
                .projects
                .iter()
                .map(|p| (p.name.clone(), p.notes_dir.clone()))
                .collect()
        })
        .unwrap_or_default()
    }

    /// The project a draft belongs to, as the last drafts listing reported it.
    fn draft_project(&self, draft_id: &str) -> Option<String> {
        lock(&self.inner.snapshot)
            .drafts
            .drafts
            .iter()
            .find(|draft| draft.id == draft_id)
            .and_then(|draft| draft.project.clone())
    }

    /// The file a draft is bound to, already resolved, as the last drafts
    /// listing reported it.
    fn draft_saved_path(&self, draft_id: &str) -> Option<String> {
        lock(&self.inner.snapshot)
            .drafts
            .drafts
            .iter()
            .find(|draft| draft.id == draft_id)
            .and_then(|draft| draft.saved_path.clone())
    }

    /// The path a *first* save of `draft_id` will write to, decided once.
    ///
    /// Stable while the draft is still unbound, and that is the point: a
    /// conflict dialog names a file, and the Overwrite that answers it has to
    /// mean *that* file. A filename recomputed from the clock on every click
    /// would send the overwrite at a fresh name and leave the file the user was
    /// shown sitting there untouched — a save that silently did something else.
    ///
    /// A draft that has a `saved_path` never reaches this: the worker ignores
    /// the fallback entirely once a draft is bound.
    fn fallback_save_path(&self, draft_id: &str, project: &Project) -> PathBuf {
        let mut slot = lock(&self.inner.pending_save);
        if let Some((id, path)) = slot.as_ref() {
            if id == draft_id {
                return path.clone();
            }
        }
        let path = savepath::resolve_now(project);
        *slot = Some((draft_id.to_owned(), path.clone()));
        path
    }

    /// Where a first save would land, as the tooltip shows it: the active
    /// project's notes folder. Empty when no project is active — which is now a
    /// real state, and the one the pick-or-create popup answers.
    fn default_save_dir(&self) -> String {
        self.with_config(|config| {
            config
                .active_project()
                .map(|p| p.notes_dir.display().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default()
    }

    /// Would a key press start a recording right now?
    fn capture_live(&self) -> bool {
        self.inner.user_armed.load(Ordering::Relaxed)
    }

    fn armed_event(&self) -> ArmedEvent {
        ArmedEvent {
            user_armed: self.capture_live(),
        }
    }

    fn snapshot(&self) -> ShellStatus {
        let snapshot = lock(&self.inner.snapshot);
        ShellStatus {
            status: snapshot.status.clone(),
            armed: self.armed_event(),
            recording: snapshot.recording,
            lines: snapshot.lines,
            last_line: snapshot.last_line.clone(),
            drafts: snapshot.drafts.clone(),
            transcript: snapshot.transcript.clone(),
            pending: PendingEvent {
                pending: snapshot.pending.clone(),
            },
            projects: snapshot.projects.clone(),
            settings: snapshot.settings.clone(),
            condition: snapshot.conditions.event(),
        }
    }

    /// Has startup finished successfully?
    ///
    /// The overlay's other precondition: it is shown only once there
    /// is a real session behind it, never during loading and never after a
    /// fatal. Read off the same snapshot the status event is driven from.
    fn is_ready(&self) -> bool {
        self.phase() == Phase::Ready
    }

    /// Which of the four outcomes the window is being told about right now.
    ///
    /// Read off the same snapshot the status event is driven from, so a command
    /// and the screen can never disagree about which phase they are in.
    fn phase(&self) -> Phase {
        lock(&self.inner.snapshot).status.phase
    }

    /// Is a recording running right now?
    ///
    /// Read off the same snapshot the indicator is driven from, which the
    /// control thread sets on every start and stop. The commands need it —
    /// editing a project mid-utterance would change where the line being spoken
    /// is about to be saved — and they cannot see the control thread's
    /// [`Machine`].
    fn recording_live(&self) -> bool {
        lock(&self.inner.snapshot).recording.live
    }

    /// Which draft lines are landing in right now, as far as the shell knows.
    /// Set only from the worker's own `DraftChanged`, so it cannot drift from
    /// the handle the worker actually holds.
    fn active_draft(&self) -> Option<String> {
        lock(&self.inner.snapshot).drafts.active_id.clone()
    }

    /// Record a draft activation. The line counter follows the draft: resuming
    /// a seven-line draft makes the next line the eighth, and the last-line
    /// readout belonged to the draft we just left.
    fn set_active_draft(&self, id: Option<String>, line_count: usize) {
        let mut snapshot = lock(&self.inner.snapshot);
        snapshot.drafts.active_id = id;
        snapshot.lines = line_count;
        snapshot.last_line = None;
        // The transcript belonged to the draft we just left. Dropped rather
        // than kept: the worker sends the new one immediately behind this, and
        // a snapshot fetched in between must not hand a webview someone else's
        // lines to edit.
        snapshot.transcript = None;
    }

    /// Record and publish the active draft's transcript.
    fn set_transcript(&self, app: &AppHandle, transcript: LinesEvent) {
        lock(&self.inner.snapshot).transcript = Some(transcript.clone());
        emit(app, EVENT_LINES, &transcript);
    }

    /// Change the pending list and publish **all** of it.
    ///
    /// One function, so there is no path that mutates without emitting and no
    /// path that emits a delta. The whole list every time is what keeps the
    /// window from having to reconstruct a queue it can only see one edge of.
    fn update_pending(&self, app: &AppHandle, edit: impl FnOnce(&mut Vec<LineDto>)) {
        let event = {
            let mut snapshot = lock(&self.inner.snapshot);
            edit(&mut snapshot.pending);
            PendingEvent {
                pending: snapshot.pending.clone(),
            }
        };
        emit(app, EVENT_PENDING, &event);
    }

    /// Retire the row for one intake token, whatever became of it.
    fn drop_pending(&self, app: &AppHandle, token: &str) {
        self.update_pending(app, |pending| {
            pending.retain(|row| row.id != token);
        });
    }

    fn set_drafts(&self, app: &AppHandle, drafts: DraftsEvent) {
        // The tray's recents are this list, cut to five and named the way the
        // sidebar names them — derived from the event rather than from a second
        // scan, so the menu and the tree can never list different notes.
        self.tray(TrayInput::Notes(tray::notes(&drafts)));
        lock(&self.inner.snapshot).drafts = drafts.clone();
        emit(app, EVENT_DRAFTS, &drafts);
    }

    /// Publish the project list. Called on startup and after every mutation, so
    /// the window never has to infer one.
    fn refresh_projects(&self, app: &AppHandle) {
        let projects = self.projects_event();
        lock(&self.inner.snapshot).projects = projects.clone();
        emit(app, EVENT_PROJECTS, &projects);
    }

    /// Whether the first-run wizard is behind this machine.
    ///
    /// `true` while the configuration has not been handed over yet — see
    /// [`StatusEvent::NOT_ONBOARDING`] for why that direction and not the other.
    fn onboarded(&self) -> bool {
        self.with_config(|config| config.onboarded.is_done())
            .unwrap_or(StatusEvent::NOT_ONBOARDING)
    }

    fn set_status(&self, app: &AppHandle, mut status: StatusEvent) {
        // Filled in here rather than at the twenty construction sites: this is
        // the one function that both publishes a status and can see the
        // configuration, so the flag cannot go out stale or be forgotten.
        status.onboarded = self.onboarded();
        self.tray_status(&status);
        lock(&self.inner.snapshot).status = status.clone();
        emit(app, EVENT_STATUS, &status);
    }

    /// Publish the current status again with the onboarding flag re-derived.
    ///
    /// The one thing that can change `onboarded` while the app runs is
    /// [`onboarding_finish`] on a seeded machine, where there is no restart to
    /// carry the news. Nothing else about the status moves, so this re-sends
    /// what is already in the snapshot rather than inventing a phase.
    fn refresh_status(&self, app: &AppHandle) {
        let status = lock(&self.inner.snapshot).status.clone();
        self.set_status(app, status);
    }

    /// Change one part of the ready readout and publish it again.
    ///
    /// The readout is what this *process* is actually running, and several of
    /// those facts can change without a restart: the bindings (a rebind), the
    /// microphone, the model and the language. Each
    /// of them would otherwise go on naming what startup opened until the next
    /// launch — a readout quietly wrong about the one thing the user just
    /// changed, on the About tab, in the footer and in the overlay at once.
    ///
    /// One place that mutates it and emits, so no caller can do the first
    /// without the second. Nothing happens before ready or after a fatal: there
    /// is no readout to correct.
    fn update_ready(&self, app: &AppHandle, edit: impl FnOnce(&mut ReadyInfo)) {
        let status = {
            let mut snapshot = lock(&self.inner.snapshot);
            let Some(ready) = snapshot.status.ready.as_mut() else {
                return;
            };
            edit(ready);
            snapshot.status.clone()
        };
        // The binding summary is the tray's key hint, so a rebind reaches the
        // menu by the same call that reaches the footer.
        self.tray_status(&status);
        emit(app, EVENT_STATUS, &status);
    }

    /// Update the ready readout's binding summary and publish it again.
    fn set_binding_readout(&self, app: &AppHandle, bindings: Bindings) {
        self.update_ready(app, |ready| {
            ready.bindings = describe_bindings(bindings);
        });
    }

    /// Publish the recording state — to both windows and to the overlay thread.
    ///
    /// One call, so the title bar's indicator, the pill's VU state and the
    /// window that has to be wide enough for it are three readings of the same
    /// moment. Nothing anywhere infers a recording from a level.
    fn set_recording(&self, app: &AppHandle, recording: RecordingEvent) {
        lock(&self.inner.snapshot).recording = recording;
        emit(app, EVENT_RECORDING, &recording);
        self.overlay(OverlayInput::Recording(recording.live));
        // Four readings of one moment now: the indicator, the pill, the window
        // that has to be wide enough for it, and the glyph in the tray.
        self.tray(TrayInput::Recording(recording.live));
    }

    fn emit_armed(&self, app: &AppHandle) {
        let armed = self.armed_event();
        self.tray(TrayInput::Armed(armed.user_armed));
        emit(app, EVENT_ARMED, &armed);
    }

    fn add_line(&self, app: &AppHandle, spoken_at: String, text: String) {
        let line = {
            let mut snapshot = lock(&self.inner.snapshot);
            snapshot.lines += 1;
            let line = LineEvent {
                n: snapshot.lines,
                spoken_at,
                text,
            };
            snapshot.last_line = Some(line.clone());
            line
        };
        emit(app, EVENT_LINE, &line);
        // The pill reveals exactly what this event carries, so the window is
        // grown by the same call that sends it. Its callers are already the
        // *fresh successful decodes only* filter: a failed line has no
        // words to reveal and a held line's words are minutes old.
        self.overlay(OverlayInput::Line);
    }

    fn notice(&self, app: &AppHandle, level: NoticeLevel, message: impl Into<String>) {
        let notice = NoticeEvent {
            level,
            message: message.into(),
        };
        match level {
            NoticeLevel::Error => tracing::warn!(message = %notice.message, "notice"),
            // A `debug` notice is log traffic on both sides of the boundary:
            // the frontend ring and `RUST_LOG=debug`. Promoting it to `info`
            // here would put the hook-scope line and every confirmation into
            // the default process log, which is the flood this level exists to
            // keep out.
            NoticeLevel::Debug => tracing::debug!(message = %notice.message, "notice"),
            _ => tracing::info!(message = %notice.message, "notice"),
        }
        emit(app, EVENT_NOTICE, &notice);
    }

    /// Mutate the conditions and publish the top one, under one lock.
    ///
    /// Emits only when what the window would *show* actually changed, so a
    /// conflict raised behind a dead hook is remembered without redrawing
    /// anything, and clearing the hook reveals it with no extra work at the
    /// site that cleared it.
    fn update_conditions(&self, app: &AppHandle, edit: impl FnOnce(&mut Conditions)) {
        let (before, after, top) = {
            let mut snapshot = lock(&self.inner.snapshot);
            let before = snapshot.conditions.event();
            edit(&mut snapshot.conditions);
            let after = snapshot.conditions.event();
            // The same resolved answer the window is given, as the enum rather
            // than as its wire spelling — one precedence rule, read once.
            let top = snapshot.conditions.top().map(|(condition, _)| condition);
            (before, after, top)
        };
        if before == after {
            return;
        }
        tracing::info!(condition = ?after.condition, detail = %after.detail, "condition");
        self.tray(TrayInput::Condition(top));
        emit(app, EVENT_CONDITION, &after);
    }

    /// Record that a condition holds, with the sentence for this occurrence.
    fn set_condition(&self, app: &AppHandle, condition: Condition, detail: impl Into<String>) {
        let detail = detail.into();
        self.update_conditions(app, move |conditions| {
            *conditions.slot(condition) = Some(detail);
        });
    }

    /// Record that a condition has ended. A no-op if it was not holding.
    fn clear_condition(&self, app: &AppHandle, condition: Condition) {
        self.update_conditions(app, |conditions| *conditions.slot(condition) = None);
    }

    /// Disarm because Sotone cannot record at all.
    ///
    /// The design's single exception — "no failure disarms the mic on its own
    /// except losing the input device" — so this has
    /// exactly one caller class: entering [`Condition::NoDevice`]. It routes
    /// through the same `Disarmed` input [`set_armed`] sends rather than
    /// touching the engine, so a live recording is ended by the state machine
    /// and not by a second copy of that rule. Re-arming is the user's: nothing
    /// in this file ever arms Sotone on their behalf.
    fn disarm(&self, app: &AppHandle) {
        if !self.inner.user_armed.swap(false, Ordering::Relaxed) {
            return;
        }
        self.send(ShellInput::Key(KeyInput::Disarmed {
            at: SystemTime::now(),
        }));
        self.emit_armed(app);
    }

    /// Send to the control thread, dropping the message if that thread is gone
    /// (a fatal startup leaves nobody listening, and that is not a new error to
    /// report — the fatal status already said everything). The empty phase
    /// *does* listen: it answers a hotkey capture, so the wizard's key step
    /// works before there is a session, and traces the rest away.
    fn send(&self, input: ShellInput) {
        let _ = lock(&self.inner.inputs).send(input);
    }

    /// Send to the overlay thread, dropped in the same way and for
    /// the same reason: the pill is optional, and a build without one is not an
    /// error to report to a user who cannot act on it.
    fn overlay(&self, input: OverlayInput) {
        let _ = lock(&self.inner.overlay).send(input);
    }

    /// Send to the tray thread, dropped in the same way.
    ///
    /// `pub(crate)` because the tray's own menu-event closure sends through it:
    /// that closure runs on the event-loop thread and this is the whole of what
    /// it is allowed to do (invariant 5's discipline).
    pub(crate) fn tray(&self, input: TrayInput) {
        let _ = lock(&self.inner.tray).send(input);
    }

    /// Tell the tray what `sotone://status` just said.
    ///
    /// One function, called from both places that publish a status, so the
    /// glyph cannot be told about a phase change without also being told the
    /// binding hint that goes with it.
    fn tray_status(&self, status: &StatusEvent) {
        self.tray(TrayInput::Phase {
            phase: status.phase,
            hint: status
                .ready
                .as_ref()
                .map(|ready| ready.bindings.clone())
                .unwrap_or_default(),
        });
    }

    /// Whether a tray icon exists. See [`Inner::tray_alive`].
    fn tray_alive(&self) -> bool {
        self.inner.tray_alive.load(Ordering::Relaxed)
    }

    /// Record whether the tray came up. Called once, from [`start`].
    fn set_tray_alive(&self, alive: bool) {
        self.inner.tray_alive.store(alive, Ordering::Relaxed);
    }

    /// Activate a draft on the user's behalf — the tray's recent-notes items.
    ///
    /// The same message the sidebar's click sends: the worker owns the active
    /// draft's handle, so this is a request to the control thread and never a
    /// second handle opened here.
    pub(crate) fn open_draft(&self, id: String) {
        self.send(ShellInput::Draft(DraftInput::Open(id)));
    }
}

/// Which of the two recording modes a settings command is about.
///
/// The wire spelling is `"ptt"` / `"toggle"` — the config's own words for the
/// two `*_enabled` keys, so there is one vocabulary from the file through the
/// commands to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HotkeyMode {
    /// Hold to record.
    Ptt,
    /// Press to start, press again to stop.
    Toggle,
}

impl HotkeyMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ptt => "ptt",
            Self::Toggle => "toggle",
        }
    }

    /// How the mode is named in a sentence to the user.
    const fn describe(self) -> &'static str {
        match self {
            Self::Ptt => "push-to-talk",
            Self::Toggle => "toggle",
        }
    }

    /// The mode a command named, or `None` for anything else. Refused rather
    /// than defaulted: a typo must not silently rebind the other key.
    fn parse(word: &str) -> Option<Self> {
        match word {
            "ptt" => Some(Self::Ptt),
            "toggle" => Some(Self::Toggle),
            _ => None,
        }
    }
}

/// A binding token as a sentence rather than as a token.
///
/// Only the mouse buttons need it: `F13` reads as itself, but `MouseX1` is not
/// what anybody calls the button under their thumb.
fn binding_label(token: &str) -> String {
    match token.trim() {
        "MouseX1" => "mouse side button 1".to_owned(),
        "MouseX2" => "mouse side button 2".to_owned(),
        other => other.to_owned(),
    }
}

/// A failed emit means the webview is gone, i.e. the app is closing. There is
/// no user left to tell.
fn emit<S: Serialize + Clone>(app: &AppHandle, event: &str, payload: S) {
    if let Err(err) = app.emit(event, payload) {
        tracing::debug!(event, error = %err, "event not delivered");
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Arm or disarm capture from the UI. Returns the new state so the button can
/// settle without waiting for the event round trip.
///
/// The button is reachable *during* a recording — clicking it no longer has to
/// focus anything, and focus no longer stops anything — so disarming has to end
/// a live recording itself, or a held push-to-talk key whose release will now be
/// refused would strand the audio. The stop is sent to the control thread like
/// any other input, stamped now: this command must not touch the engine
/// (`AudioEngine` is not `Sync`) and must not block (invariant 5).
///
/// Non-async on purpose: it takes one uncontended lock's worth of time, and a
/// command that cannot block does not need the async machinery.
#[tauri::command]
pub fn set_armed(armed: bool, app: AppHandle, state: State<'_, ShellState>) -> ArmedEvent {
    apply_armed(&app, &state, armed)
}

/// Arm or disarm, from wherever the user asked.
///
/// One implementation, two callers: this command and the tray's Enable/Disable
/// item. A second copy of "what arming means" is exactly how the
/// menu and the window would come to disagree about whether a held key's
/// release is still going to be honoured.
pub(crate) fn apply_armed(app: &AppHandle, state: &ShellState, armed: bool) -> ArmedEvent {
    state.inner.user_armed.store(armed, Ordering::Relaxed);
    if !armed {
        state.send(ShellInput::Key(KeyInput::Disarmed {
            at: SystemTime::now(),
        }));
    }
    state.emit_armed(app);
    state.armed_event()
}

/// Ask the window to show one of its views.
///
/// Sent, not stored: see [`ViewEvent`]. The window is shown and focused by the
/// caller in `crate::tray`; this only says *what* to show once it is up.
pub(crate) fn request_view(app: &AppHandle, view: &'static str) {
    emit(app, EVENT_VIEW, ViewEvent { view });
}

/// One snapshot of everything, for a webview that has just finished loading.
#[tauri::command]
pub fn shell_status(state: State<'_, ShellState>) -> ShellStatus {
    state.snapshot()
}

/// Start a new note. The current draft, if any, is simply left outstanding.
///
/// Send-only, like every command here: no filesystem work happens on the Tauri
/// command path, so a slow disk can never stall the IPC thread (invariant 5 in
/// spirit — the hook is a process away, but the same rule applies to every
/// callback-shaped thread in this app). The result comes back as
/// `sotone://drafts`.
#[tauri::command]
pub fn draft_new(state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::New));
}

/// Make a note **without** opening it, for a move to land in.
///
/// The chooser's "New note" option, and deliberately not [`draft_new`]: that
/// one routes to the worker and makes the fresh draft the *active* one, which
/// would displace the note the selection is being moved out of half way
/// through the batch. This creates the directory and walks away, so the source
/// stays open and the N `line_move_to` calls behind it still have somewhere to
/// come from.
///
/// It answers with the new id rather than an event, because the window has to
/// name it in the moves that follow — the `sotone://drafts` re-list it also
/// fires is how the *tree* learns about it, and it goes out before the id is
/// returned so the row exists by the time the first line lands in it.
///
/// `project` is the **source note's** project, so a stitched note is filed
/// where the lines came from; `null` is the tree's "not in your projects"
/// group and a real answer, not a missing argument. The note is unnamed until
/// it is saved or renamed — its label is its creation time, like every other
/// unsaved draft.
///
/// `async` for the `line_audio` reason: it touches the filesystem (one
/// directory, one `meta.json`, one empty log), and Tauri runs async commands
/// off the thread that services IPC so a slow disk cannot stall the window
/// (invariant 5 in spirit). The handle is dropped the moment the id is read
/// off it — the one-handle rule: the worker opens this draft again, once per
/// line, as each move arrives.
///
/// # Errors
/// A sentence for the chooser to report: a recording is running, startup has
/// not finished, or the draft directory could not be created. Nothing is
/// moved when this fails — the window keeps the selection and sends no
/// `line_move_to` at all.
#[tauri::command]
pub async fn draft_create_detached(
    project: Option<String>,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<String, String> {
    if state.recording_live() {
        // The standard refusal, as an `Err` rather than a notice: this command
        // owes its caller an id or a reason, and the chooser reports the reason
        // itself. A notice as well would say the same thing twice.
        return Err(recording_refusal("move lines into a new note"));
    }
    let store = state
        .store()
        .ok_or_else(|| "Sotone is still starting up".to_owned())?;

    let draft = store
        .create_draft(project.as_deref())
        .map_err(|err| format!("could not make a new note: {err}"))?;
    let id = draft.id().to_owned();
    // Explicit, not incidental: this handle exists only to read the id off,
    // and it must be gone before the worker opens the same directory for the
    // first arriving line.
    drop(draft);

    tracing::info!(draft = %id, project = ?project, "made a note for a move to land in");
    refresh_drafts(&app, &state, &store);
    Ok(id)
}

/// Resume an existing draft: new lines append to it, continuing its numbering.
#[tauri::command]
pub fn draft_open(id: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::Open(id)));
}

/// Discard a draft to `.trash/`.
///
/// Nothing is deleted: the directory is renamed, and the 30-day sweep at
/// startup is the only thing in this codebase that ever removes it
/// (invariant 4).
#[tauri::command]
pub fn draft_discard(id: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::Discard(id)));
}

/// Save the active draft to markdown.
///
/// Send-only like the rest: the render re-reads `lines.jsonl`, the guard hashes
/// whatever is at the path, and the write is atomic — all blocking I/O, and none
/// of it may run on the thread that services IPC (invariant 5). It goes to the
/// worker, which is the only thing holding the draft's handle.
///
/// `overwrite` is `false` everywhere except the conflict dialog's Overwrite
/// button. That is the single call site in the whole app that can discard what
/// someone else wrote (invariant 4), and the user has been shown the difference
/// before it is reachable.
///
/// The result comes back as `sotone://save`.
#[tauri::command]
pub fn draft_save(overwrite: bool, state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::Save { overwrite }));
}

/// Rename the note a draft is bound to.
///
/// A note's name **is** its file's basename — there is no name field anywhere
/// in the store — so this renames the `.md` file and updates the binding, and
/// every label in the window (the tree, the breadcrumb, the tray's recents)
/// follows the next drafts event for free.
///
/// Send-only like every other draft command: the existence check, the
/// `fs::rename` and the metadata write are blocking I/O, and none of it may run
/// on the thread that services IPC (invariant 5). Refused while a recording is
/// live, on the control thread, exactly as a save and a discard are.
///
/// The name arrives as the user typed it and is sanitized in `sotone-core`,
/// which is where the filename rules live. Nothing here can turn this into a
/// move: the target is always a sibling of the current file.
#[tauri::command]
pub fn draft_rename(id: String, name: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::Rename { id, name }));
}

/// Move a note into another project, or out of every project.
///
/// The one command the cross-project drag adds. `project: None` is the tree's
/// "no project" group, which is a real place a note can be: dictation is never
/// blocked by a missing project.
///
/// **Not** [`DraftInput::SetProject`], which tags the *next lazily created*
/// draft and touches nothing on disk. This moves one existing note: its `.md`
/// goes into the target project's notes folder, and the binding that says where
/// it is moves with it in the same atomic meta write — files move when you
/// move them. A note whose file is gone is re-rendered there instead,
/// so nothing stays in the "not in your projects" group after a drop.
///
/// `keep_both` is `false` everywhere except the clash dialog's Keep both
/// button, exactly as [`draft_save`]'s `overwrite` is. It is the *only*
/// resolution offered, because there is no overwrite answer to a name clash:
/// the second note is numbered, and the note already in the folder is never
/// written over (invariant 4). A first drop onto a taken name changes nothing
/// at all and comes back as `sotone://note-clash`.
///
/// Send-only like every other draft command: the move and the meta write are
/// blocking I/O and may not run on the thread that services IPC (invariant 5).
/// Refused while a recording is live, on the control thread, exactly as a save,
/// a discard and a rename are. The result comes back as `sotone://drafts`.
#[tauri::command]
pub fn draft_set_project(
    id: String,
    project: Option<String>,
    keep_both: bool,
    state: State<'_, ShellState>,
) {
    state.send(ShellInput::Draft(DraftInput::SetDraftProject {
        id,
        project,
        keep_both,
    }));
}

/// Save every dirty note in the store.
///
/// No arguments and no overwrite flag, both deliberately. Which projects exist
/// and where their notes live is read from the *config* on the control thread at
/// the moment the batch runs, not named by the window — and there is no batch
/// overwrite at all, so this command cannot be made to discard an external edit
/// however it is called (invariant 4).
///
/// Send-only like every other draft command: the whole batch is blocking I/O on
/// the worker thread, which is the only thing holding a draft handle
/// (invariant 5). The result comes back as `sotone://save-all`.
#[tauri::command]
pub fn draft_save_all(state: State<'_, ShellState>) {
    state.send(ShellInput::Draft(DraftInput::SaveAll));
}

// ---------------------------------------------------------------------------
// Projects
//
// These are the only commands in the app that write the configuration, and they
// are `async` for that reason: Tauri runs async commands off the thread that
// services IPC, so neither the (small) config write nor the native folder
// dialog can stall the window (invariant 5 in spirit).
//
// Every mutation is refused while a recording is live. `project_reveal` and
// `filename_preview` are the exceptions: neither writes anything.
//
// Refusals are notices, not `Err`s: they are ordinary answers ("that name is
// taken"), and the notice area is where this app says ordinary things. `Err` is
// reserved for a failure the user did not ask for, such as a config file that
// cannot be written.
// ---------------------------------------------------------------------------

/// Would a mutation land in the middle of an utterance?
///
/// The save precedent: a project edited mid-recording changes where the line
/// being spoken right now is about to be written.
///
/// `Warn`, not `Info`: this is the archetypal refusal, and a refusal
/// is the one thing the design gives `--warn` at full text brightness. The
/// level is what routes it to the footer slot where the user will read it.
fn refuse_while_recording(app: &AppHandle, state: &ShellState, what: &str) -> bool {
    if state.recording_live() {
        state.notice(app, NoticeLevel::Warn, recording_refusal(what));
        return true;
    }
    false
}

/// The one sentence this app refuses with while a recording is live.
///
/// Split out because [`draft_create_detached`] owes its caller a value
/// or a reason and so carries the refusal in its `Err` rather than in a notice.
/// Two copies of these words is how the footer and a popup come to disagree
/// about what was refused.
fn recording_refusal(what: &str) -> String {
    format!("not while a recording is running — stop it first, then {what}")
}

/// After any project mutation: persist-and-publish is already done, this is the
/// rest of the world catching up.
fn after_project_change(app: &AppHandle, state: &ShellState) {
    state.refresh_projects(app);
    // The active project decides `default_save_dir` and how every relative
    // binding resolves, so the drafts list is now stale.
    if let Some(store) = state.store() {
        refresh_drafts(app, state, &store);
    }
}

/// The one sentence a taken project name is refused with.
///
/// Split out for [`recording_refusal`]'s reason: `project_create` has an
/// early copy of the name check so the filesystem step below it is only ever
/// reached by a create that will be accepted, and two copies of these words is
/// how the early check and the config layer's backstop come to disagree about
/// the same refusal.
fn name_taken_refusal(name: &str) -> String {
    format!("there is already a project called \"{name}\"")
}

/// Everything [`project_create`] does before the configuration is written, in
/// the one order that keeps a refusal off the disk.
///
/// `name_taken` is read from the configuration by the caller, so this function
/// is a pure decision plus the one filesystem step — which is what makes the
/// *order* testable without an `AppHandle`.
///
/// **Eager, on purpose** — creating a project is the Godot dialog: the
/// folder is made now rather than at the first save, because a project the user
/// can see in the tree has a folder they can find in the file manager, and a
/// rename never meets a folder that is not there yet. Both halves of that have
/// been hit in practice.
///
/// **Invariant 4:** `create_dir_all` creates and never removes. There is no
/// `fs::remove_*` here or anywhere it reaches, so the worst this step can do to
/// an existing folder is nothing at all — which is exactly the create-subfolder
/// switch in its off position, pointing a project at a home that already
/// exists, arriving for free.
fn prepare_project_folder(name_taken: bool, name: &str, notes_dir: &Path) -> Result<(), String> {
    // Refusal first, filesystem second (`project_rename`'s precedent): a
    // duplicate name that was going to be refused must not have created a
    // folder on the way to being told so.
    if name_taken {
        return Err(name_taken_refusal(name));
    }
    std::fs::create_dir_all(notes_dir).map_err(|err| {
        format!(
            "could not create {}: {err} — nothing was written",
            notes_dir.display()
        )
    })
}

/// Create a project and make it active.
///
/// The notes folder is made to exist here, before the config write.
/// The path arrives composed: every creation surface pairs its folder picker
/// with the create-a-subfolder switch and sends the answer, exactly as the
/// wizard always has, so this command's contract is unchanged.
///
/// Answers with a [`CreateOutcome`] so the surface that sent this can tell
/// acceptance from refusal without reading notices — every refusal below is a
/// resolved promise, and three surfaces used to take that for a yes. The notice
/// each one emits is unchanged: the outcome is in addition to the sentence.
///
/// **Invariant 4:** the order that keeps a refusal off the disk is untouched.
/// Every refusal above [`prepare_project_folder`] returns before anything
/// reaches the filesystem, and that step creates and never removes.
///
/// # Errors
/// If the configuration file cannot be written, or if the app is still starting
/// up and has no configuration to read. A rejected name, a blank folder, a
/// folder that could not be created and a live recording are notices and
/// [`CreateOutcome::Refused`], not errors: `Err` is what the window reports as
/// a failure, and each of these is an answer with a sentence of its own.
#[tauri::command]
pub async fn project_create(
    name: String,
    notes_dir: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<CreateOutcome, String> {
    if refuse_while_recording(&app, &state, "create a project") {
        return Ok(CreateOutcome::Refused);
    }

    let name = name.trim().to_owned();
    let notes_dir = notes_dir.trim().to_owned();
    if name.is_empty() {
        state.notice(&app, NoticeLevel::Warn, "a project needs a name");
        return Ok(CreateOutcome::Refused);
    }
    if notes_dir.is_empty() {
        state.notice(
            &app,
            NoticeLevel::Warn,
            "pick the folder this project's notes go in",
        );
        return Ok(CreateOutcome::Refused);
    }

    // The name check the config write makes below, run early against the
    // configuration as it stands — the same shape `project_rename` uses, and
    // for the same reason: the folder step underneath it is the one thing here
    // that touches disk, and it must only ever be reached by a create that is
    // going to be accepted. The write's own check stays as the backstop for a
    // configuration that changes underneath us.
    let Some(name_taken) =
        state.with_config(|config| config.projects.iter().any(|p| p.name == name))
    else {
        return Err("Sotone is still starting up".to_owned());
    };
    if let Err(refusal) = prepare_project_folder(name_taken, &name, Path::new(&notes_dir)) {
        state.notice(&app, NoticeLevel::Warn, refusal);
        return Ok(CreateOutcome::Refused);
    }

    let created = state.edit_config(|config| {
        // Case-sensitive, because the name is the key `meta.project` stores and
        // the `{project}` token renders — two projects differing only in case
        // are two projects, and the config layer would reject neither.
        if config.projects.iter().any(|p| p.name == name) {
            return Ok(false);
        }
        config.projects.push(Project::new(&name, &notes_dir));
        config.active_project = Some(name.clone());
        Ok(true)
    })?;

    if !created {
        state.notice(&app, NoticeLevel::Warn, name_taken_refusal(&name));
        return Ok(CreateOutcome::Refused);
    }

    // The worker creates a draft the moment a line needs one, so it has to hear
    // about the new active project itself.
    state.send(ShellInput::Draft(DraftInput::SetProject(Some(
        name.clone(),
    ))));
    after_project_change(&app, &state);
    // A confirmation, so the log and nowhere else: the new group is in
    // the tree and the breadcrumb names it, which says this better than a
    // sentence does.
    state.notice(
        &app,
        NoticeLevel::Debug,
        format!("\"{name}\" is now active"),
    );
    Ok(CreateOutcome::Created)
}

/// Make a project active, or clear the active project with `null`.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn project_set_active(
    name: Option<String>,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "switch project") {
        return Ok(());
    }

    let known = state.edit_config(|config| {
        match &name {
            Some(name) if config.project(name).is_none() => return Ok(false),
            _ => {}
        }
        config.active_project = name.clone();
        Ok(true)
    })?;

    if !known {
        state.notice(
            &app,
            NoticeLevel::Warn,
            "there is no project by that name any more",
        );
        return Ok(());
    }

    state.send(ShellInput::Draft(DraftInput::SetProject(name)));
    after_project_change(&app, &state);
    Ok(())
}

/// Edit one project's folder or templates. Absent fields are left alone.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn project_update(
    name: String,
    notes_dir: Option<String>,
    filename_template: Option<String>,
    header_template: Option<String>,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "edit a project") {
        return Ok(());
    }

    let outcome = state.edit_config(|config| {
        let Some(project) = config.project_mut(&name) else {
            return Ok(Some("there is no project by that name any more"));
        };
        if let Some(dir) = &notes_dir {
            let dir = dir.trim();
            if dir.is_empty() {
                return Ok(Some("a project's notes folder cannot be empty"));
            }
            project.notes_dir = PathBuf::from(dir);
        }
        if let Some(template) = &filename_template {
            let template = template.trim();
            if template.is_empty() {
                return Ok(Some("a filename template cannot be empty"));
            }
            project.filename_template = template.to_owned();
        }
        if let Some(header) = &header_template {
            // An emptied field means "no header", which is the key's absence —
            // that is what `None` writes, and it is the only honest
            // representation of unset.
            project.header_template = (!header.is_empty()).then(|| header.clone());
        }
        Ok(None)
    })?;

    if let Some(refusal) = outcome {
        state.notice(&app, NoticeLevel::Warn, refusal);
        return Ok(());
    }
    after_project_change(&app, &state);
    Ok(())
}

/// What the rename's folder step actually does, once the filesystem has been
/// asked about the plan [`folder_plan`] drew from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FolderStep {
    /// Carry the folder along with the name.
    Move {
        /// Where it is now.
        from: PathBuf,
        /// The sibling it becomes.
        to: PathBuf,
    },
    /// Leave it where it is, with the reason the footer will say out loud.
    Keep(FolderKept),
}

/// The two filesystem probes a folder plan has to survive before `fs::rename`
/// is allowed near it.
///
/// Split out of [`project_rename`] so the *order* of the probes is testable:
/// `folder_plan` is pure by design and deliberately asks the configuration
/// nothing about disk, which leaves both questions here.
///
/// 1. **Occupied first** — a folder already sitting at the target is checked
///    before anything else, as it always has been. `fs::rename` would
///    replace a file and fail on a directory, and neither is an answer anyone
///    asked for.
/// 2. **Then a source that is not there.** There was a time when a
///    project's folder was only made at its first save, so a project created
///    and renamed before it was ever recorded into had nothing to rename —
///    and `fs::rename` failing on it aborted a rename that was otherwise
///    perfectly fine. The config rename still means something, so it goes
///    ahead and the footer says the folder half did not happen. Strictly more
///    conservative than what it replaces (invariant 4): it turns a
///    filesystem call into no filesystem call at all.
fn folder_step(plan: FolderPlan) -> FolderStep {
    match plan {
        FolderPlan::Rename { to: next, .. } if next.exists() => {
            FolderStep::Keep(FolderKept::Occupied)
        }
        FolderPlan::Rename { from: dir, .. } if !dir.exists() => {
            FolderStep::Keep(FolderKept::NoFolder)
        }
        FolderPlan::Rename {
            from: dir,
            to: next,
        } => FolderStep::Move {
            from: dir,
            to: next,
        },
        FolderPlan::Keep(reason) => FolderStep::Keep(reason),
    }
}

/// Rename a project — its name, its folder when that is safe, and every draft
/// that names it.
///
/// # The order, and why it is the order (invariant 4)
///
/// 1. **The folder move**, when [`folder_plan`] says the folder is safely this
///    project's own and [`folder_step`] finds it on disk with nothing already
///    sitting at the target. It goes first because it is the only step that can
///    fail for a reason outside our control — a file manager holding a handle
///    on it, a permission — and a failure here aborts the whole rename with
///    **nothing written anywhere**.
/// 2. **One config write**, moving `Project.name`, `notes_dir` (only when the
///    folder actually moved) and `active_project` (only when it matched)
///    together. One write, because a config in which those disagree is a
///    config where notes have quietly changed project.
/// 3. **The meta sweep, on the worker** — every draft whose `meta.project` is
///    the old name gets the new one, through the live handle for the active
///    draft and open→mutate→drop for the rest.
///
/// Both crash windows are non-destructive and neither loses a byte:
///
/// * after (1), the config still names the old folder, so relative bindings
///   resolve into a directory that is not there — and a save recreates it and
///   re-renders the note into it. Nothing is lost; the user sees a project
///   whose folder looks empty and re-points or re-renames it.
/// * after (2), the un-swept drafts sit in the "not in your projects" group
///   until they are re-saved (which adopts the active project) or the rename
///   is repeated. Visible and honest — the one thing that must never happen is
///   a draft that is *hidden*, and nothing here can hide one.
///
/// A rename **never merges**: a name another project already has is refused,
/// not folded into it.
///
/// # Errors
/// If the configuration file cannot be written. Every refusal — a blank name,
/// a taken name, a project that is gone — is a notice, and a folder that could
/// not be moved is an error notice with nothing written behind it.
#[tauri::command]
pub async fn project_rename(
    from: String,
    to: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "rename a project") {
        return Ok(());
    }

    let to = to.trim().to_owned();
    if to.is_empty() {
        state.notice(&app, NoticeLevel::Warn, "a project needs a name");
        return Ok(());
    }
    // Nothing to do, and saying so would be noise: the field simply closes.
    if to == from {
        return Ok(());
    }

    // **Refusals first** (§8), against the configuration as it stands: a
    // rename that is going to be refused must not have moved a folder first.
    // The config layer re-checks all of this inside the write, which is what
    // makes the check-then-write race harmless — this pass exists so the
    // *filesystem* step below is only ever reached by a rename that will be
    // accepted.
    let Some(refusal) = state.with_config(|config| {
        if !config.projects.iter().any(|p| p.name == from) {
            return Some(ProjectRenameError::NoSuchProject);
        }
        if config.projects.iter().any(|p| p.name == to) {
            return Some(ProjectRenameError::NameTaken);
        }
        None
    }) else {
        return Err("Sotone is still starting up".to_owned());
    };
    if let Some(refusal) = refusal {
        // A rename never merges (Never-list adjacency): the name is taken, and
        // folding one project into another is not what "rename" means.
        state.notice(&app, NoticeLevel::Warn, refusal.to_string());
        return Ok(());
    }

    let plan = state
        .with_config(|config| folder_plan(&config.projects, &from, &to))
        .unwrap_or(FolderPlan::Keep(FolderKept::NoFolder));
    let mut moved = None;
    let mut kept = None;
    match folder_step(plan) {
        FolderStep::Keep(reason) => kept = Some(reason),
        FolderStep::Move {
            from: dir,
            to: next,
        } => {
            // The one filesystem mutation in this command, and the only step
            // that can fail for a reason outside our control.
            if let Err(err) = std::fs::rename(&dir, &next) {
                state.notice(
                    &app,
                    NoticeLevel::Error,
                    format!(
                        "could not rename {} to {}: {err} — nothing was changed",
                        dir.display(),
                        next.display()
                    ),
                );
                return Ok(());
            }
            tracing::info!(from = %dir.display(), to = %next.display(), "renamed a project folder");
            moved = Some((dir, next));
        }
    }

    let renamed = state.edit_config(|config| {
        Ok(config
            .rename_project(&from, &to, moved.as_ref().map(|(_, next)| next.clone()))
            .map_err(|err| err.to_string()))
    })?;

    if let Err(refusal) = renamed {
        // Only reachable if the configuration changed underneath us between
        // the check above and this write. Nothing was written, so put the
        // folder back rather than leaving it ahead of the config: the rename
        // as a whole did nothing.
        if let Some((dir, next)) = &moved {
            let _ = std::fs::rename(next, dir);
        }
        state.notice(&app, NoticeLevel::Warn, refusal);
        return Ok(());
    }

    // Step three, on the thread that owns the draft handles.
    state.send(ShellInput::Draft(DraftInput::ProjectRenamed {
        from: from.clone(),
        to: to.clone(),
    }));
    after_project_change(&app, &state);

    match kept {
        // The honest half of it: the config changed and the folder did not, so
        // say which, at the level that reaches the user.
        Some(reason) => state.notice(
            &app,
            NoticeLevel::Warn,
            format!("renamed \"{from}\" to \"{to}\" — {}", reason.note()),
        ),
        // The new name is in the tree and in the breadcrumb, which says this
        // better than a sentence does.
        None => state.notice(
            &app,
            NoticeLevel::Debug,
            format!("renamed \"{from}\" to \"{to}\""),
        ),
    }
    Ok(())
}

/// Remove a project from the configuration.
///
/// **Config only, and grep-provably so: there is no `fs::remove_*` anywhere in
/// this command or anything it calls.** The folder and every file in it stay
/// exactly where they are; what goes is the entry in `config.toml` and the
/// `active_project` line if it named this one.
///
/// The drafts that reference it are left alone on purpose. They fall into the
/// window's "not in your projects" group, and the rule is that nothing may
/// ever hide a draft — and saving one adopts the active project. Recreating a
/// project of the same name brings the whole group back.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn project_delete(
    name: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "remove a project") {
        return Ok(());
    }

    let removed = state.edit_config(|config| Ok(config.remove_project(&name)))?;
    if !removed {
        state.notice(
            &app,
            NoticeLevel::Warn,
            "there is no project by that name any more",
        );
        return Ok(());
    }

    // The worker tags a lazily created draft with the active project, and there
    // may no longer be one.
    let active = state.active_project_name();
    state.send(ShellInput::Draft(DraftInput::SetProject(active)));
    after_project_change(&app, &state);
    state.notice(
        &app,
        NoticeLevel::Warn,
        format!(
            "removed \"{name}\" from your projects — its folder and every file in it are \
             untouched"
        ),
    );
    Ok(())
}

/// Ask the OS for a folder.
///
/// **The one native dialog in Sotone.** It takes focus while it is open, which
/// every other question this app asks deliberately does not (invariant 2) — a
/// folder simply cannot be chosen in-page, because the webview has no
/// filesystem access. It is permitted because it opens only from a click in
/// Sotone's own UI: no code path here opens it unprompted, on startup, or in
/// answer to an event. Capture is focus-independent, so a finding can
/// still be dictated while it is up.
///
/// `blocking_pick_folder` is safe here precisely because this command is
/// `async`: Tauri runs it off the event loop, and blocking the *event loop*
/// would deadlock the dialog's own message pump.
///
/// # Errors
/// If the chosen entry cannot be read back as a path.
#[tauri::command]
pub async fn project_pick_folder(
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<Option<String>, String> {
    if refuse_while_recording(&app, &state, "pick a folder") {
        return Ok(None);
    }
    let Some(chosen) = app.dialog().file().blocking_pick_folder() else {
        // Cancelled. Not an error, and not worth a notice either.
        return Ok(None);
    };
    let path = chosen.into_path().map_err(|err| format!("{err}"))?;
    Ok(Some(path.display().to_string()))
}

/// Show a folder or a saved note in the OS file manager.
///
/// `target` is `"notes_dir"` (with `name` naming the project), `"draft"` (with
/// `name` naming a draft) or `"models_dir"` (which takes no name — it is
/// whatever the configuration says). **Never a raw path from the frontend**: this
/// resolves every path itself, out of the configuration and the drafts listing
/// it already has, so the command cannot become an open-anything primitive.
///
/// Read-only, so it is allowed while a recording is live.
///
/// # Errors
/// If there is nothing to reveal, or the file manager could not be asked.
#[tauri::command]
pub async fn project_reveal(
    target: String,
    name: Option<String>,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    let path = match target.as_str() {
        "notes_dir" => {
            let project = state
                .with_config(|config| match &name {
                    Some(name) => config.project(name).cloned(),
                    None => config.active_project().cloned(),
                })
                .flatten()
                .ok_or_else(|| "there is no project by that name".to_owned())?;
            if project.notes_dir.as_os_str().is_empty() {
                return Err("that project has no notes folder yet".to_owned());
            }
            project.notes_dir
        }
        // The listing's `saved_path` is already resolved against the draft's
        // project (see `drafts_event`), so this is a path Sotone worked out, not
        // one the window handed back.
        "draft" => {
            let id = name.ok_or_else(|| "no note was named".to_owned())?;
            let path = state
                .draft_saved_path(&id)
                .ok_or_else(|| "that note has not been saved to a file yet".to_owned())?;
            PathBuf::from(path)
        }
        // Resolved from the configuration, so this stays a fixed set
        // of places Sotone knows about rather than a path the window chose.
        "models_dir" => {
            let dir = state
                .with_config(|config| config.models_dir.clone())
                .ok_or_else(|| "Sotone is still starting up".to_owned())?;
            // A fresh install has no models folder yet; showing the user an
            // empty folder is a better answer than an error, and it is where
            // they are about to put a model.
            if let Err(err) = std::fs::create_dir_all(&dir) {
                return Err(format!("could not open {}: {err}", dir.display()));
            }
            dir
        }
        other => return Err(format!("{other:?} is not something Sotone can reveal")),
    };

    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|err| format!("could not show {} : {err}", path.display()))
}

/// What a filename template would produce right now, for the live preview.
///
/// The expansion lives in Rust and only in Rust: a second implementation in JS
/// would be a second answer to "what will this file be called", and the two
/// would drift.
///
/// # Errors
/// Never in practice; the signature is `Result` because an async command
/// borrowing state has to have one.
#[tauri::command]
pub async fn filename_preview(
    template: String,
    project: String,
    _state: State<'_, ShellState>,
) -> Result<String, String> {
    Ok(template::expand_filename_now(&template, &project))
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// What [`SearchNoteDto::last_written`] says, including the fallback.
///
/// The rule in one place because it is the half the window cannot check: a note
/// with nothing live in it has never been written to, so the honest answer is
/// when it was *started*. Split from [`sotone_core::draft::last_written`], which
/// answers the other half (which line is the newest one that counts), so each
/// side is pinned by a test in the crate that owns it.
fn written_at(newest_live: Option<String>, created_at: String) -> String {
    newest_live.unwrap_or(created_at)
}

/// One note's result entry.
///
/// A constructor rather than a literal at the call site, because `matches` is a
/// count of **lines** and nothing else — deriving it from the vector here is
/// what stops it from ever becoming a count of occurrences (the tree's numbers
/// and the rows the pane lists have to be the same number).
fn search_note(draft_id: String, lines: Vec<SearchLineDto>, last_written: String) -> SearchNoteDto {
    SearchNoteDto {
        draft_id,
        matches: lines.len(),
        lines,
        last_written,
    }
}

/// The whole answer, with its total derived rather than accumulated.
fn search_outcome(term: &SearchTerm, notes: Vec<SearchNoteDto>) -> SearchOutcome {
    SearchOutcome {
        term: term.as_str().to_owned(),
        matches: notes.iter().map(|note| note.matches).sum(),
        notes,
    }
}

/// Every note in the store that contains `term`, with its matching lines.
///
/// **Read-only, from the first line to the last.** Nothing here opens a
/// writable path, and there is no branch in it that could: [`Draft::open`]
/// rewrites nothing (its own doc comment says so — the torn-tail repair belongs
/// to the next *append*, done by the draft's owner), and the only other call is
/// [`DraftStore::list_drafts`], which reads a directory. Invariant 4 is upheld
/// by absence, and a `grep` of this function is the proof.
///
/// **The active draft is scanned like every other one.** fsync-per-line means
/// disk is the truth, and the worst a concurrent append can do to a reader is
/// leave a torn tail — which the parser already treats as "skip the last
/// record". So there is no message to the control thread and no second handle:
/// the one-handle rule is about *writing*, and this writes nothing.
///
/// `async`, so Tauri runs it on the runtime pool rather than on the event-loop
/// thread — a disk walk on the thread that services IPC is the family of jank
/// invariant 5 exists to prevent. It touches neither the control thread nor the
/// worker: all it takes from the shared state is a clone of the drafts root,
/// which is a `PathBuf` and nothing else.
///
/// A draft that cannot be read is **skipped silently**: the
/// drafts-unreadable latch is already the one reporter for store trouble, and a
/// second one firing on every keystroke is the notice flood this app avoids. Same
/// for a root that cannot be listed at all — the answer is an empty result and
/// a `tracing` line, never an error the window has to say out loud.
///
/// # Errors
/// Never in practice; the signature is `Result` because an async command
/// borrowing state has to have one.
#[tauri::command]
pub async fn search_notes(
    term: String,
    state: State<'_, ShellState>,
) -> Result<SearchOutcome, String> {
    // An empty or all-whitespace term *is* search-off, so it answers with
    // nothing rather than with everything.
    let Some(needle) = SearchTerm::parse(&term) else {
        return Ok(SearchOutcome::default());
    };
    let Some(store) = state.store() else {
        // Startup has not published a root yet (or there is no session at all —
        // the empty phase). Nothing to search, and nothing wrong.
        return Ok(SearchOutcome::default());
    };

    let scan = match store.list_drafts() {
        Ok(scan) => scan,
        Err(err) => {
            tracing::warn!(reason = %err, "search could not list the drafts root");
            return Ok(SearchOutcome::default());
        }
    };

    let mut notes = Vec::new();

    for summary in scan.drafts {
        let loaded = match Draft::open(&summary.dir) {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::debug!(
                    dir = %summary.dir.display(),
                    reason = %err,
                    "search skipped an unreadable draft"
                );
                continue;
            }
        };
        let hits = matching_lines(&needle, &loaded.lines);
        if hits.is_empty() {
            continue;
        }
        let lines = hits
            .into_iter()
            .map(|index| {
                let line = &loaded.lines[index];
                SearchLineDto {
                    id: line.id.clone(),
                    spoken_at: line.spoken_at.format("%H:%M:%S").to_string(),
                    text: line.text.clone(),
                }
            })
            .collect();
        notes.push(search_note(
            summary.id,
            lines,
            written_at(
                last_written(&loaded.lines).map(|at| at.to_rfc3339()),
                summary.created_at.to_rfc3339(),
            ),
        ));
    }

    Ok(search_outcome(&needle, notes))
}

// ---------------------------------------------------------------------------
// Settings
//
// Same shape as the project commands, and for the same reasons: `async`,
// because they write the configuration file or open a native dialog and
// neither may happen on the thread that services IPC; refused while a
// recording is live, except the pure reads; and refusals are notices rather
// than `Err`s, because "you cannot turn off the only way to record" is an
// ordinary answer, not a failure.
//
// Everything writes through `edit_config`, so the user's comments, key order
// and unknown keys survive, and a key is only written once it departs its
// default. Two of the writes are additionally *validated first* against
// `recording_mode_problem`: Settings must never write a configuration that the
// next launch refuses to load, because the user would then have no way back in
// through the app.
// ---------------------------------------------------------------------------

/// The input devices the user could pin the microphone to.
///
/// Read fresh every call rather than cached: devices appear and disappear while
/// the app runs, and a Settings tab opened after plugging a headset in has to
/// offer it. Allowed while recording — it changes nothing.
///
/// # Errors
/// If the host refuses to enumerate its devices.
#[tauri::command]
pub async fn settings_devices(
    _state: State<'_, ShellState>,
) -> Result<Vec<InputDeviceDto>, String> {
    let devices = list_input_devices().map_err(|err| format!("{err}"))?;
    Ok(devices
        .into_iter()
        .map(|device| InputDeviceDto {
            name: device.name,
            is_default: device.is_default,
        })
        .collect())
}

/// Begin "press the key you want" for one mode.
///
/// Send-only: the control thread owns helper supervision, and this rebind is a
/// sequence of helper lifecycle steps — stop the one watching the bindings,
/// start a `--capture` one, wait for its single line — that nothing else may
/// interleave with. **Push-to-talk and toggle are dead while it runs**, on
/// purpose: there is exactly one hook, capture is seconds long, and the panel
/// says so.
#[tauri::command]
pub fn hotkey_capture_start(mode: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Settings(SettingsInput::CaptureStart(mode)));
}

/// Stop listening and put the previous bindings back.
#[tauri::command]
pub fn hotkey_capture_cancel(state: State<'_, ShellState>) {
    state.send(ShellInput::Settings(SettingsInput::CaptureCancel));
}

/// Switch one recording mode on or off.
///
/// Turning off the last one is refused: a config with both off is one
/// [`Config::load`] rejects, so writing it would leave the user with an app
/// that will not start and no way to fix it from inside the app.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_mode_enabled(
    mode: String,
    enabled: bool,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change the hotkeys") {
        // Re-published so the checkbox the user just clicked snaps back to what
        // the file says. Every refusal below does the same: this panel's
        // controls are only ever drawn from the event, so a refusal that said
        // nothing would leave a box ticked for a change that did not happen.
        state.refresh_settings(&app);
        return Ok(());
    }
    let Some(mode) = HotkeyMode::parse(&mode) else {
        return Err(format!("{mode:?} is not a recording mode"));
    };

    let refusal = state.edit_config(|config| {
        let (ptt, toggle) = match mode {
            HotkeyMode::Ptt => (enabled, config.toggle_enabled),
            HotkeyMode::Toggle => (config.ptt_enabled, enabled),
        };
        // The load rule, asked before the write rather than after it.
        if let Some(problem) =
            recording_mode_problem(&config.hotkey, &config.toggle_hotkey, ptt, toggle)
        {
            return Ok(Some(problem));
        }
        config.ptt_enabled = ptt;
        config.toggle_enabled = toggle;
        Ok(None)
    })?;

    if let Some(problem) = refusal {
        state.notice(&app, NoticeLevel::Warn, problem);
        state.refresh_settings(&app);
        return Ok(());
    }

    // Live, and in this order: the file is the truth a crash would leave
    // behind, so it is written before the running helper is changed to match.
    state.send(ShellInput::Settings(SettingsInput::Rehook));
    state.refresh_settings(&app);
    Ok(())
}

/// Pin the microphone to a device, or clear the pin with `null`. Live — no
/// restart.
///
/// Still refused while a recording is live, and that refusal is what protects
/// the clip: the engine tears its stream down to open the new device, and doing
/// that mid-utterance would truncate audio the user is still speaking.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_mic(
    device: Option<String>,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change the microphone") {
        state.refresh_settings(&app);
        return Ok(());
    }
    // The device's whole description, not a fragment the user typed: Sotone pins
    // by name substring, and the full name is the substring that cannot match
    // a device they did not choose.
    let device = device
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty());
    state.edit_config(|config| {
        config.mic_substring = device.clone();
        Ok(())
    })?;
    // File first, then the running engine — the same ordering as every other
    // live change here. A crash in between leaves a config that already says
    // what the user chose, so the next launch opens it.
    //
    // The reconnect itself happens on the control thread, which owns the
    // engine: `AudioEngine` is not `Sync`, and closing and opening a device is
    // blocking work that has no business on the thread servicing IPC
    // (invariant 5's spirit). It answers with its own notice and its own
    // readout update.
    state.send(ShellInput::Settings(SettingsInput::SetMic(device)));
    state.refresh_settings(&app);
    Ok(())
}

/// Choose the transcription language, live.
///
/// The cheapest true mechanism: whisper takes the language as a per-utterance
/// decoding parameter, so this is a field assignment on the running model
/// ([`Transcriber::set_language`]) rather than a reload. Nothing is loaded,
/// nothing is warmed, and the next utterance is decoded with it.
///
/// Refused while a recording is live, like every other mutation that reaches
/// the engine or the worker: the clip being spoken right now is about to be
/// decoded, and changing the language underneath it would answer a question the
/// user asked about the *next* line.
///
/// # Errors
/// If the code cannot be handed to whisper at all, or if the configuration file
/// cannot be written.
#[tauri::command]
pub async fn set_language(
    language: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change the language") {
        state.refresh_settings(&app);
        return Ok(());
    }

    // Interpreted before anything is written, so the file can never come to
    // hold a value the worker would then refuse (the `set_theme` precedent).
    // The list the window picks from is whisper's own table, so this only ever
    // catches a value that did not come from it.
    let chosen = Language::new(&language);
    if !chosen.is_ffi_safe() {
        return Err(format!(
            "{language:?} is not a language whisper can be given"
        ));
    }

    state.edit_config(|config| {
        config.language = chosen.as_str().to_owned();
        Ok(())
    })?;

    // What is actually in force, which is not always what was just written: a
    // project may override it (config-file-only, in the schema). The worker
    // and the readout both get *that*, so they cannot disagree with each other
    // or with the overlay.
    let effective = state
        .with_config(|config| Language::new(config.effective_language(config.active_project())))
        .unwrap_or(chosen);
    state.send(ShellInput::Settings(SettingsInput::SetLanguage(
        effective.clone(),
    )));
    state.update_ready(&app, |ready| {
        ready.language = effective.to_string();
    });
    state.refresh_settings(&app);
    Ok(())
}

/// Turn the audio cues on or off. Live.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_cues(
    on: bool,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    // Refused mid-recording, and not only for consistency with the rest: the
    // begin cue that already sounded is trimmed off the front of the stored
    // clip, and that trim is decided when the utterance *ends*. Switching cues
    // off in between would leave the beep in the clip — which is exactly the
    // contamination that made whisper hallucinate "Thank you" chains.
    if refuse_while_recording(&app, &state, "change the cues") {
        state.refresh_settings(&app);
        return Ok(());
    }
    state.edit_config(|config| {
        config.audio_cues = on;
        Ok(())
    })?;
    // File first, then the running session — the same ordering as every other
    // live change here.
    state.send(ShellInput::Settings(SettingsInput::SetCues(on)));
    state.refresh_settings(&app);
    Ok(())
}

/// Make the window's X quit Sotone rather than hide it to the tray, or put it
/// back.
///
/// There is no [`ShellInput`] here and nothing caches the answer: [`on_close`]
/// reads this key out of the configuration at the moment the X is pressed, so
/// the switch is live by construction and there is no second copy to keep in
/// step.
///
/// Refused while a recording is live like the rest of Settings — the view
/// preferences (the theme, and hide-deleted) are the exemptions,
/// and this is not one of them. Nothing about this toggle is urgent enough to
/// be worth widening that list.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_close_quits(
    on: bool,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change what closing the window does") {
        // The refusal has to put the switch back where the configuration says
        // it is: the window flipped it optimistically, as every toggle here
        // does, and this event is the correction.
        state.refresh_settings(&app);
        return Ok(());
    }
    state.edit_config(|config| {
        config.close_quits = on;
        Ok(())
    })?;
    state.refresh_settings(&app);
    Ok(())
}

/// Keep soft-deleted lines out of the transcript view, or list them again.
/// Live — the window re-renders from the event this fires.
///
/// **Deliberately accepted while a recording is live**, which makes it the
/// second exemption from the standing refusal, beside [`set_theme`] — and for
/// the same reason. What those refusals protect is the engine, the helper and
/// the draft mid-utterance, and this reaches none of the three: it writes one
/// word to the configuration and the window re-reads it. The toggle began as
/// a live-during-recording view filter with nothing behind it;
/// persisting it must not quietly take that away, because the surface it lives
/// on is a context menu the user opens *while* dictating.
///
/// The write goes through the same `edit_config` path as every other setting:
/// one lock, one atomic save, and no write code anywhere near a note
/// (invariant 4). No line is deleted, restored or rewritten by this — the
/// notes on disk keep every line, and only the view changes.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_hide_deleted(
    on: bool,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    state.edit_config(|config| {
        config.hide_deleted = on;
        Ok(())
    })?;
    // File first, then the window — the `set_theme` ordering. The window has
    // already flipped its own copy optimistically, so the event this fires
    // carries the value it just sent and lands as a no-op there.
    state.refresh_settings(&app);
    Ok(())
}

/// Show or hide the overlay, and remember the choice. Live — no restart.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_overlay(
    on: bool,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change the overlay setting") {
        state.refresh_settings(&app);
        return Ok(());
    }
    state.edit_config(|config| {
        config.overlay = on;
        Ok(())
    })?;
    // File first, then the window: a crash between the two leaves a config that
    // matches what the next launch will do (the capture precedent, and the
    // same ordering as `set_cues` above).
    //
    // Shown only if startup has reached ready, which is the other half of
    // requirement "ready AND wanted": there is nothing truthful to put on it
    // while the model is still loading, and a fatal leaves nothing at all.
    // Hiding is unconditional — there is no state in which a hidden overlay is
    // wrong.
    state.overlay(OverlayInput::Visible(on && state.is_ready()));
    state.refresh_settings(&app);
    Ok(())
}

/// Dock the overlay pill to a different corner, and remember it. Live — the
/// pill moves as soon as this returns.
///
/// Refused while a recording is live, like `set_overlay` and unlike
/// [`set_theme`]: the pill is the surface the user is *watching* while they
/// speak, and moving it out from under a glance mid-utterance is exactly the
/// kind of change the standing rule exists to postpone. The exemptions stay the
/// two view preferences — the theme, and [`set_hide_deleted`].
///
/// # Errors
/// If the word is not one of the four corners, or the configuration file cannot
/// be written.
#[tauri::command]
pub async fn set_overlay_corner(
    corner: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "move the overlay") {
        state.refresh_settings(&app);
        return Ok(());
    }
    // Parsed before anything is written, so the file can never come to hold a
    // word the next launch would refuse to load (the `set_theme` precedent).
    let corner: OverlayCorner = corner.parse()?;
    state.edit_config(|config| {
        config.overlay_corner = corner;
        Ok(())
    })?;
    // File first, then the window. The thread re-reads the corner out of the
    // configuration when it places, so this says *that something moved*, not
    // where to.
    state.overlay(OverlayInput::Moved);
    state.refresh_settings(&app);
    Ok(())
}

/// How long a transcribed line stays on the pill. Live — the next reveal uses
/// it.
///
/// Clamped rather than refused, in the same one place the configuration clamps
/// it: the control offers a list of durations, and a number from anywhere else
/// is still a wish about how long to read for.
///
/// # Errors
/// If the configuration file cannot be written.
#[tauri::command]
pub async fn set_reveal_seconds(
    seconds: u32,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change how long a line is shown") {
        state.refresh_settings(&app);
        return Ok(());
    }
    let seconds = clamp_reveal_seconds(seconds);
    state.edit_config(|config| {
        config.reveal_seconds = seconds;
        Ok(())
    })?;
    // Nothing to tell the overlay thread: it reads the duration when it starts
    // a reveal, so there is no copy of this anywhere to invalidate.
    state.refresh_settings(&app);
    Ok(())
}

/// Choose the palette the window draws in, and remember it. Live — no restart.
///
/// **Deliberately accepted while a recording is live**, which all but one of
/// the other mutations in this file refuse ([`set_hide_deleted`] is the second
/// exemption; the two of them are the view preferences). The rule
/// those refusals exist for is that a change must not reach the engine, the
/// helper or the draft mid-utterance — and a theme reaches none of them: it
/// writes one word to the config and the window re-reads it. Refusing it would
/// only mean a user who hits the wrong palette mid-session has to stop
/// dictating to fix it.
///
/// The write goes through the same `edit_config` path as every other setting:
/// one lock, one atomic save, no new write code anywhere near a note
/// (invariant 4).
///
/// # Errors
/// If the word is not one of the two palettes, or the configuration file
/// cannot be written.
#[tauri::command]
pub async fn set_theme(
    theme: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    // Parsed before anything is written, so the file can never come to hold a
    // word the next launch would refuse to load.
    let theme: Theme = theme.parse()?;
    state.edit_config(|config| {
        config.theme = theme;
        Ok(())
    })?;
    // The event is what actually flips the palette: the window optimistically
    // renders nothing, here as everywhere else in Settings.
    state.refresh_settings(&app);
    Ok(())
}

/// Load a different model and run every following line through it. Live — no
/// restart.
///
/// Validated before anything else, so a bad file is a notice here rather than a
/// failed load a second later. Then the work happens on a thread of its own —
/// see [`load_model`] for the ordering, which is the interesting part.
///
/// Refused while a recording is live, like every other mutation that reaches
/// the worker.
///
/// # Errors
/// If the name is not a file name at all, or the configuration is not readable
/// yet. A rejected file, and a second change while one is already loading, are
/// notices.
#[tauri::command]
pub async fn model_set_active(
    name: String,
    app: AppHandle,
    state: State<'_, ShellState>,
) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "change the model") {
        return Ok(());
    }

    let Some(models_dir) = state.with_config(|config| config.models_dir.clone()) else {
        return Err("Sotone is still starting up".to_owned());
    };
    // The name came from the window; resolve it against the models folder
    // ourselves rather than trusting it as a path.
    let path = models_dir.join(&name);
    if file_name(&path) != name {
        return Err(format!("{name:?} is not a model file name"));
    }
    if let Err(err) = validate_model(&path) {
        state.notice(&app, NoticeLevel::Warn, err.to_string());
        return Ok(());
    }

    // The empty phase is the one place this is still a restart, and
    // it has to be: there is no engine, no worker and no helper — startup
    // stopped before all three — so there is nothing to swap a model *into*.
    // The choice is written and the panel's Restart button, which is the whole
    // point of that screen, does the rest. Also covers the fatal phase, where
    // the same is true.
    if !state.is_ready() {
        state.edit_config(|config| {
            config.active_model = Some(name.clone());
            Ok(())
        })?;
        state.refresh_settings(&app);
        // The row itself now reads "will be used" and the Restart button is
        // live, so this is a confirmation of something already on screen: log
        // only.
        state.notice(
            &app,
            NoticeLevel::Debug,
            format!("{name} will be loaded when you restart Sotone"),
        );
        return Ok(());
    }

    // One at a time. A second click while the first model is still loading is
    // refused rather than queued: queueing would load two multi-hundred-megabyte
    // models at once to throw one of them away.
    if let Err(refusal) = state.claim_model_load(&name) {
        state.notice(&app, NoticeLevel::Warn, refusal);
        return Ok(());
    }
    // Published immediately, so the row can say "loading…" — the same
    // backend-owns-the-state arrangement the capture panel uses.
    state.refresh_settings(&app);

    // A model load blocks for seconds. It may not happen here — this is a Tauri
    // async command, so blocking it holds a runtime worker and every other
    // command behind it — and it may not happen on the session worker either,
    // where it would stall the transcription of lines the user has already
    // spoken. Hence a thread of its own, which owns the whole rest of the flow.
    let loader = app.clone();
    let loading_state = (*state).clone();
    if let Err(err) = thread::Builder::new()
        .name("sotone-model-load".to_owned())
        .spawn(move || {
            load_model(&loader, &loading_state, &name, &path);
            // Whatever happened: the slot is free and the window is told.
            loading_state.release_model_load();
            loading_state.refresh_settings(&loader);
        })
    {
        state.release_model_load();
        state.refresh_settings(&app);
        return Err(format!("could not start loading the model: {err}"));
    }
    Ok(())
}

/// Load a model, warm it, and hand it to the worker.
///
/// Runs on its own thread; everything here is allowed to block.
///
/// # The ordering, which is the whole design
///
/// **Load, then write the config, then hand over.** The load is the only step
/// that can fail for reasons nobody can predict — a corrupt body, a GPU that
/// will not allocate — so it happens *first*, while the configuration still
/// names the model that is actually running. That is a deliberate departure
/// from the write-then-act ordering [`set_cues`] and [`set_overlay`] use: those
/// act on things that cannot fail, so writing first makes the file the truth a
/// crash leaves behind. Here writing first would need a rollback on failure —
/// and a crash between the failure and the rollback would leave the file naming
/// a model that cannot load, which is a *fatal* on the next launch rather than
/// the empty phase (`resolve_model` only checks that the file validates).
///
/// So there is no rollback in this function, because there is nothing to roll
/// back: a failed load writes nothing at all, and the config and the running
/// model are in agreement at every instant.
///
/// The write is the ordinary [`ShellState::edit_config`] path — one lock, one
/// atomic `toml_edit` save that keeps the user's comments (invariant 4). If it
/// fails the loaded model is dropped and *not* handed over: a running model the
/// config disagrees with is the same lie in the other direction.
///
/// **Nothing spoken is lost.** Utterances recorded while this ran are sitting in
/// the worker's queue and are decoded after the swap, by the new model; an
/// utterance already being decoded finishes on the old one.
fn load_model(app: &AppHandle, state: &ShellState, name: &str, path: &Path) {
    // The language in force right now, so a swap does not quietly reset it.
    let language = state
        .with_config(|config| Language::new(config.effective_language(config.active_project())))
        .unwrap_or_default();

    // The warm-up (one second of silence) is inside `load`, so
    // it is paid here, on this thread, before the worker ever sees it — the
    // first real utterance after a swap must not be the cold start.
    let transcriber = match Transcriber::load(path, language) {
        Ok(transcriber) => transcriber,
        Err(err) => {
            state.notice(app, NoticeLevel::Error, swap_failure(name, &err));
            return;
        }
    };
    // Read off the loaded context rather than off the header we validated, and
    // spelled with `ModelKind` rather than by hand: the overlay tests this word
    // for `multilingual` to decide what the *active* language really is,
    // so both sides of the IPC boundary have to keep using the one vocabulary.
    let kind = if transcriber.is_multilingual() {
        ModelKind::Multilingual
    } else {
        ModelKind::EnglishOnly
    };

    if let Err(err) = state.edit_config(|config| {
        config.active_model = Some(name.to_owned());
        Ok(())
    }) {
        state.notice(
            app,
            NoticeLevel::Error,
            format!("{name} loaded, but the change could not be saved: {err} — Sotone is still running the previous model"),
        );
        return;
    }

    // Config first, then the running session (as everywhere else). The control
    // thread owns the channel to the worker, so the hand-over goes through it.
    state.send(ShellInput::Settings(SettingsInput::SetTranscriber(
        Box::new(transcriber),
    )));
    state.update_ready(app, |ready| {
        ready.model = name.to_owned();
        ready.model_path = path.display().to_string();
        ready.model_kind = kind.to_string();
        // A note from startup about how the model was *chosen* ("the only one
        // in the folder") is not true of this one: the user just chose it.
        ready.note = None;
    });
    // Confirmed by the row ("in use"), the footer facts and the About tab, all
    // of which this same swap has just corrected — so the sentence is log-only.
    // The *failures* above stay `Error`: those have no visible state.
    state.notice(
        app,
        NoticeLevel::Debug,
        format!("{name} is now transcribing — no restart needed"),
    );
}

/// The refusal when a model load is already running.
///
/// Its own function so the sentence is pinned by a test: this is the whole of
/// the one-load-at-a-time rule as the user meets it.
fn loading_refusal(loading: &str) -> String {
    format!("{loading} is still loading — wait for it to finish, then choose another")
}

/// What a failed model load says. The old model is still running, and saying so
/// is the point: the user needs to know the app still works.
fn swap_failure(name: &str, err: &dyn std::error::Error) -> String {
    let mut message = format!("could not load {name}: {err}");
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message.push_str(" — Sotone is still running the previous model");
    message
}

/// Read the models folder again and publish what is in it now.
///
/// The whole of Sotone's model management, together with the models-folder
/// reveal: there is deliberately no in-app "Add model…" or "Remove", so
/// adding a model is dropping the file into `models_dir` and this is how the
/// running app finds out. There is no
/// filesystem watcher — a rescan happens because the user asked, or because a
/// surface that lists models came on screen.
///
/// **It is a directory read and nothing else.** It does not touch the engine,
/// the worker or the active model: a model already loaded is in memory and
/// stays there, even if the file it came from has since been deleted, and the
/// configuration is not rewritten to agree with the folder. So there is nothing
/// here to refuse while a recording is live — unlike every mutation around it,
/// this command cannot change what the running session is doing. The scan is
/// real filesystem work (a listing plus a 48-byte header read per `.bin`),
/// which is why it is `async`: Tauri runs it off the event loop.
///
/// The answer is the ordinary `sotone://settings` event, so every surface that
/// lists models — Settings, the first-run panel, the wizard's model step —
/// follows it with no second channel. Invalid files travel in it too, with the
/// validator's own sentence: rejected at scan time, never at transcribe time.
///
/// # Errors
/// If the configuration has not been read yet.
#[tauri::command]
pub async fn models_rescan(app: AppHandle, state: State<'_, ShellState>) -> Result<(), String> {
    // Not "Sotone is starting up, try later" as flavour: without a config there
    // is no models folder to scan, and publishing an empty list over a real one
    // would be a worse answer than saying so.
    if state
        .with_config(|config| config.models_dir.clone())
        .is_none()
    {
        return Err("Sotone is still starting up".to_owned());
    }
    // `settings_event` does the scan; this is the same publish every settings
    // mutation ends with. No notice: the freshly drawn list is the answer, and
    // `scan_models_dir` already logs the counts (a confirmation whose
    // result is on screen is log traffic, not a message).
    state.refresh_settings(&app);
    Ok(())
}

/// Start Sotone again.
///
/// The honest version of the "restart Sotone to apply" line Settings prints,
/// and the way out of the empty phase: the microphone and the transcriber are
/// opened once at startup, so a model chosen now takes effect at the *next*
/// launch and this is that launch. It is a **process relaunch**, not an
/// in-process re-init — retrying startup without a restart is deliberately out
/// of scope, and pretending otherwise is how half-initialised sessions happen.
///
/// Refused while a recording is live. That is moot in the empty phase — nothing
/// can be recording when the engine was never opened — and cheap insurance
/// everywhere else, where restarting mid-utterance would throw away audio the
/// user is still speaking.
///
/// Nothing here touches focus, input or the network (invariants 1/2/3): it asks
/// this process to exit and the OS to start the same local executable again,
/// which then behaves like any other launch.
///
/// `AppHandle::restart` is `-> !` (tauri 2.11.5, `src/app.rs`): called off the
/// main thread — which is where Tauri runs an async command — it sets the
/// restart flag, requests the exit, and then parks the calling thread forever
/// rather than returning, so this future never completes and the frontend's
/// promise never settles. That is correct here: the answer to "restart" arrives
/// as a new process, not as a resolved promise.
///
/// # Errors
/// Never in practice; the one refusal is a notice.
#[tauri::command]
pub async fn app_restart(app: AppHandle, state: State<'_, ShellState>) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "restart Sotone") {
        return Ok(());
    }
    tracing::info!("restarting at the user's request");
    app.restart()
}

// ---------------------------------------------------------------------------
// The onboarding wizard
//
// The empty-phase panel stays exactly what it was — the *repair* surface for a
// that lost its model — and the wizard is the *first-run* surface. Which one is
// on screen is the window's decision, off `onboarded` and `phase`; the three
// commands below are the whole of what the backend adds for it, and none of
// them is new machinery: the finish writes one config key and puts the window
// back to its declared size, and the other two answer questions.
// ---------------------------------------------------------------------------

/// The wizard is done: record it, and put the window back.
///
/// Two endings, decided by the phase this launch landed in:
///
/// * **`empty`** — no model was resolvable, so there is no session at all: no
///   engine, no worker, no helper. Nothing exists for the wizard's choices to be
///   applied *to*, so the marker is written as `"first-launch"` and the window
///   calls the existing [`app_restart`] itself. This is the empty phase's
///   semantics unchanged; there is no new restart machinery here.
/// * **anything else** — the machine was already seeded with a model, so every
///   live-apply path has been working throughout the wizard and there is
///   nothing left to do but write `"yes"`, restore the frame and re-publish the
///   status. No restart.
///
/// Refused while a recording is live. Near-unreachable — the run is disarmed and
/// the wizard is not the notes view — and cheap insurance all the same.
///
/// # Errors
/// If the configuration file cannot be written. The refusal is a notice.
#[tauri::command]
pub async fn onboarding_finish(app: AppHandle, state: State<'_, ShellState>) -> Result<(), String> {
    if refuse_while_recording(&app, &state, "finish setting up") {
        return Ok(());
    }

    let restarting = state.phase() == Phase::Empty;
    state.edit_config(|config| {
        config.onboarded = if restarting {
            Onboarded::FirstLaunch
        } else {
            Onboarded::Yes
        };
        Ok(())
    })?;
    tracing::info!(restarting, "onboarding finished");

    if restarting {
        // Nothing else to do: the window asks for the restart, and the marker
        // is what makes the process it lands in start disarmed, as promised.
        return Ok(());
    }

    // `tauri.conf.json` still declares this shape; it was only ever overridden
    // at runtime for the wizard (§4), so this is putting it back rather than
    // deciding it. Focus is not touched (invariant 2).
    set_main_frame(&app, MAIN_WIDTH, MAIN_HEIGHT, true);
    // The one moment `onboarded` changes while the app is running. The window
    // swaps to its normal view on this.
    state.refresh_status(&app);
    Ok(())
}

/// The folder name a project would get, as the wizard echoes it while typing.
///
/// **The sanitizer stays in Rust and only in Rust.** `file_safe` is the one
/// place that knows what a legal name is — it is public precisely so a
/// second implementation would never be written — and a JS copy in the wizard
/// would be a second answer to "what will this folder be called", diverging the
/// moment either changed.
///
/// It is the *filename* sanitizer, so it appends `.md`; a folder wants the same
/// character rules without the extension, and only the one this call added is
/// taken off. The result is the project's name and not a lowercase-hyphen slug,
/// deliberately: a rename carries the folder to the new name whenever that name
/// is usable (`folder_plan`), so the echo has to show the
/// name a folder would actually get. A slug would be a second answer to that.
///
/// Blank in, blank out — the window renders no echo and offers no Create for a
/// name that is not a name yet.
///
/// Synchronous: this is string work on a short string, which is the one kind of
/// thing allowed on the thread that services IPC.
#[tauri::command]
#[must_use]
pub fn project_slug_preview(name: String) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let safe = template::file_safe(trimmed);
    if trimmed.to_ascii_lowercase().ends_with(".md") {
        // The extension was the user's, not ours.
        return safe;
    }
    safe.strip_suffix(".md").unwrap_or(&safe).to_owned()
}

/// Where the wizard offers to keep notes.
///
/// An invention, and flagged as one in `sotone_core::config::default_notes_root`:
/// there is no notes root in the configuration, so the wizard's folder step has
/// nothing to prefill unless something suggests one. It is only ever a
/// suggestion — nothing is written until the project step creates a project
/// under it, and the folder picker is one click away.
///
/// Synchronous for the same reason [`project_slug_preview`] is: one platform
/// directory lookup and a join.
#[tauri::command]
#[must_use]
pub fn onboarding_notes_root() -> String {
    default_notes_root().display().to_string()
}

/// Put the hotkey helper back, once, at the user's request.
///
/// The manual lever the restart limit never had. The supervisor gives up after
/// [`HELPER_RESTART_LIMIT`] stops in a row — deliberately, because retrying
/// forever hides the problem — and until now the only way past that was
/// relaunching the whole process. The counter lives in the supervisor thread,
/// so **a respawn is the reset**: [`respawn_helper`] starts a fresh helper with
/// a fresh supervisor at zero. There is no counter to poke here, and that is
/// why this command is one send and nothing else.
///
/// Send-only like the two capture commands, and for the same reason: the
/// control thread owns the helper lifecycle, and spawning a process is blocking
/// work that has no business on the thread servicing IPC (invariant 5's
/// spirit). The answer arrives as `sotone://condition` — cleared if the helper
/// came back, or set again with the fresh reason if it did not.
///
/// Not refused while a recording is live: with a dead hook there is no key
/// press left to end that recording, so refusing would be refusing the way out.
#[tauri::command]
pub fn hook_recheck(state: State<'_, ShellState>) {
    state.send(ShellInput::Settings(SettingsInput::Rehook));
}

// ---------------------------------------------------------------------------
// The overlay window (the capture pill)
//
// A second window, declared in `tauri.conf.json` and built hidden: a
// corner-docked glass pill that is a logo glyph when nothing is happening, VU
// bars and a clock while a recording is live, and the line that was just
// transcribed for a few seconds afterwards, as the design draws it. Everything
// here exists so that it can never take activation — the invariant-2 note in
// this file's header has the whole chain.
//
// It deliberately has **no `parent`**. Owning it from the main window would
// take it out of Alt-Tab for free, but Windows hides an owned window whenever
// its owner is minimized — and a minimized main window during a game is exactly
// the case the overlay exists for. Hence the hand-set `WS_EX_TOOLWINDOW` below
// instead.
//
// **Why the window is resized at all.** `set_ignore_cursor_events` is
// unreliable on Windows (tauri#11461, closed not-planned), so whatever area the
// window covers is area that eats clicks over the thing under test. The answer
// is for the window to hug the pill: three sizes, one per state, so an idle
// pill costs an 81×74 hole in the corner rather than a 561×74 one (72×66 and
// 456×66 before the pill was scaled by 1.25). The 260 ms growth itself is
// CSS *inside* a window that is already big enough — a native
// window animating its size every transition is jank, and every one of those
// resizes would be another chance to take activation.
// ---------------------------------------------------------------------------

/// The overlay's label, as `tauri.conf.json` declares it.
const OVERLAY_LABEL: &str = "overlay";

/// How far the *pill* sits in from its corner of the work area, in logical
/// pixels — the design's rule is "anchored to a screen corner, 18px inset".
///
/// Supersedes the earlier 16 px bottom-right card.
const OVERLAY_INSET: i32 = 18;

/// The transparent margin between the window's edge and the pill inside it, in
/// logical pixels — room for the CSS `--shadow` to fall into.
///
/// Deliberately equal to [`OVERLAY_INSET`], which is what makes
/// [`OVERLAY_OFFSET`] zero: the window sits flush in the work area's corner and
/// the *pill* lands exactly the 18 px in that the design draws. The alternative
/// — a wider margin — would push the window off the screen edge by the
/// difference. The price is that the softest few pixels of the shadow's bottom
/// tail are clipped; the alternative was a pill in the wrong place.
///
/// **Same number as `--pill-margin` in `ui/overlay.css`.** They are two halves
/// of one measurement and must move together.
const OVERLAY_SHADOW_MARGIN: i32 = 18;

/// Where the *window* goes, given where the pill has to end up.
const OVERLAY_OFFSET: i32 = OVERLAY_INSET - OVERLAY_SHADOW_MARGIN;

/// The pill's height in logical pixels, in every state.
///
/// **One height for all three states, deliberately** (a deviation from the
/// drawn canvas, where the three paddings imply 25/25/34 px): only the width
/// then changes, which is the growth the design actually describes ("grows
/// horizontally from its anchored logo"), and the anchored edge of a
/// bottom-docked window cannot drift because the window's height never changes.
///
/// Same number as `--pill-h` in `ui/overlay.css`.
///
/// **Scaled ×1.25** (30 → 38), with the three widths below and every font,
/// gap and padding in the sheet: in use the pill proved too
/// small to read at a glance from across a game. The factor is a judgment call:
/// these four numbers and the CSS that matches them are what to turn if it
/// needs dialling — the window size is computed from them, not stored.
const PILL_HEIGHT: u32 = 38;

/// The pill's width in each state, logical pixels. Same numbers as
/// `--pill-w-idle` / `--pill-w-rec` / `--pill-w-reveal` in `ui/overlay.css`;
/// the CSS animates between them inside a window this module has already sized
/// for the larger one.
const PILL_WIDTH_IDLE: u32 = 45;
/// See [`PILL_WIDTH_IDLE`]: glyph + VU + the elapsed clock.
const PILL_WIDTH_RECORDING: u32 = 115;
/// See [`PILL_WIDTH_IDLE`]: glyph + timestamp + the line itself.
const PILL_WIDTH_REVEAL: u32 = 525;

/// How long after the reveal timer runs out the window may shrink.
///
/// The CSS collapse is 260 ms (a design timing). Shrinking the window while it
/// is still running would clip the animation halfway; a little slack costs a
/// few hundred milliseconds of a slightly larger click hole and nothing else.
const PILL_COLLAPSE: Duration = Duration::from_millis(320);

/// Which of the pill's three shapes the window is currently sized for.
///
/// The *appearance* is the page's business — there are four drawn states, and
/// "recording ended, still waiting for the line" looks different from
/// "recording" — but only three of them have different sizes, so this is the
/// window's whole vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PillShape {
    /// The logo glyph, and nothing else.
    Idle,
    /// Bars and the elapsed clock. Also the shape of the wait between a
    /// release and the line arriving.
    Recording,
    /// The line, its timestamp, and the draining hairline.
    Reveal,
}

impl PillShape {
    /// The window's logical size for this shape: the pill plus the shadow
    /// margin on every side.
    const fn logical_size(self) -> (u32, u32) {
        let width = match self {
            Self::Idle => PILL_WIDTH_IDLE,
            Self::Recording => PILL_WIDTH_RECORDING,
            Self::Reveal => PILL_WIDTH_REVEAL,
        };
        let margin = 2 * OVERLAY_SHADOW_MARGIN as u32;
        (width + margin, PILL_HEIGHT + margin)
    }
}

/// The window's copy of the pill's state machine.
///
/// It exists because the window has to be *big enough before* the CSS grows and
/// may only shrink *after* the CSS collapses, and the page — which is read-only
/// and invokes nothing — cannot ask for either. Both sides are driven by the
/// same three facts (`sotone://recording`, `sotone://line`, the reveal duration),
/// which is what keeps them agreeing; the window is deliberately the more
/// generous of the two, so a disagreement can only ever mean a slightly larger
/// transparent margin for a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PillSizer {
    shape: PillShape,
    /// Is a recording live right now?
    live: bool,
    /// A line landed *during* a recording, so its reveal is owed once that
    /// recording ends.
    line_owed: bool,
    /// When the current shape may shrink back to the glyph.
    deadline: Option<Instant>,
}

/// What the pill sizer reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PillEvent {
    /// `sotone://recording` went live.
    RecordingStarted,
    /// `sotone://recording` went idle.
    RecordingStopped,
    /// `sotone://line` — a fresh, successful decode.
    LineLanded,
    /// The deadline passed.
    Elapsed,
}

impl PillSizer {
    const fn new() -> Self {
        Self {
            shape: PillShape::Idle,
            live: false,
            line_owed: false,
            deadline: None,
        }
    }

    /// Fold one event in and answer with the shape the window should now have.
    ///
    /// `reveal` is read at the moment it is needed rather than remembered, so a
    /// change to the setting takes effect on the next reveal with nothing to
    /// invalidate.
    fn apply(&mut self, event: PillEvent, now: Instant, reveal: Duration) -> PillShape {
        match event {
            // A new recording supersedes whatever was on the pill, reveal and
            // grace alike.
            PillEvent::RecordingStarted => {
                self.live = true;
                self.line_owed = false;
                self.deadline = None;
                self.shape = PillShape::Recording;
            }
            // A line that lands mid-recording does not interrupt the VU: the
            // pill and the indicator must never disagree about what is
            // happening *now*. It is revealed when the recording ends, and the
            // last one to land wins.
            PillEvent::LineLanded if self.live => self.line_owed = true,
            PillEvent::LineLanded => {
                self.line_owed = false;
                self.shape = PillShape::Reveal;
                self.deadline = Some(now + reveal + PILL_COLLAPSE);
            }
            // Nothing was live, so nothing ended. The shell publishes
            // `RecordingEvent::IDLE` once at startup — that is the indicator's
            // opening position, not the end of a recording — and treating it as
            // one opened the window to the recording width for a whole reveal
            // period at every launch (a probe measured 128 px of idle
            // pill and said so).
            PillEvent::RecordingStopped if !self.live => {}
            PillEvent::RecordingStopped => {
                self.live = false;
                if std::mem::take(&mut self.line_owed) {
                    self.shape = PillShape::Reveal;
                } else {
                    // Decode is still in flight: stay expanded with the bars at
                    // rest. The grace is the reveal duration, because a skipped
                    // utterance emits no line at all and the pill must not hang
                    // expanded forever waiting for one.
                    self.shape = PillShape::Recording;
                }
                self.deadline = Some(now + reveal + PILL_COLLAPSE);
            }
            PillEvent::Elapsed => {
                self.deadline = None;
                self.line_owed = false;
                self.shape = PillShape::Idle;
            }
        }
        self.shape
    }

    /// How long to wait for the next timeout, if one is pending.
    fn wait(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|at| at.saturating_duration_since(now))
    }
}

/// A monitor work area in physical pixels.
///
/// The origin is carried rather than assumed to be (0, 0): a taskbar docked to
/// the left or top edge moves it, and so does a second monitor placed left of
/// or above the primary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkArea {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// A window's outer size in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowSize {
    width: u32,
    height: u32,
}

/// Where the overlay window's top-left corner goes, so that the pill inside it
/// lands `offset + margin` in from `corner`.
///
/// `offset` is [`OVERLAY_OFFSET`] scaled for the monitor, passed in rather than
/// read here so the arithmetic is testable at any scale factor.
///
/// **The anchor rule** (design §2): the pill grows away from its corner and
/// never drifts back towards the middle. That is exactly what this expresses —
/// for a left corner the window's left edge is fixed and the width grows to the
/// right; for a right corner the *right* edge is fixed, so a wider window
/// starts further left. Recomputed at every state change for that reason:
/// keeping the old x while the width changed is precisely how an anchored edge
/// drifts.
///
/// Pure, so the arithmetic that decides whether the overlay lands on screen can
/// be pinned by tests rather than by looking at a monitor. Clamped to the work
/// area's origin: a window wider or taller than the area it is being placed in
/// should lose its far edge, never the near one the pill starts at.
fn overlay_position(
    area: WorkArea,
    window: WindowSize,
    corner: OverlayCorner,
    offset: i32,
) -> (i32, i32) {
    let x = if corner.is_left() {
        area.x + offset
    } else {
        area.x + area.width as i32 - window.width as i32 - offset
    };
    let y = if corner.is_top() {
        area.y + offset
    } else {
        area.y + area.height as i32 - window.height as i32 - offset
    };
    (x.max(area.x), y.max(area.y))
}

/// Give the overlay the size and position one shape asks for, and show or hide
/// it — the one place any of those four calls is made.
///
/// **Invariant 2.** None of these can activate the window, by construction:
/// `focus: false` means tao dispatches every `show()` as `SW_SHOWNOACTIVATE`,
/// `focusable: false` puts `WS_EX_NOACTIVATE` on it so even a click cannot hand
/// it activation, and `set_size`/`set_position` are `SetWindowPos` calls that
/// touch neither z-order nor focus. There is no `set_focus`, no raise and no
/// `set_always_on_top` anywhere in this file.
///
/// **Ordering.** Size before position before show: the position depends on the
/// size (a right-anchored window's x is a function of its width), and both must
/// be right *before* the window becomes visible or the pill would appear in the
/// old corner and jump. Every one of these is a message on the same event-loop
/// queue, in the order they are made here.
///
/// Callable from any thread, and it blocks on none of them (invariant 5).
fn apply_overlay(app: &AppHandle, shape: PillShape, visible: bool) {
    let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) else {
        // The window is declared in the bundled config, so this only happens in
        // a build whose config was edited. Not a notice: the setting is still
        // remembered, and there is nothing the user could act on.
        tracing::warn!("this build has no overlay window");
        return;
    };

    // One monitor read for both halves of the geometry: the pill is docked to
    // the primary monitor's work area, so *that* monitor's scale factor is the
    // one its size has to be measured in. See `overlay_scale` for why the
    // window's own is not asked.
    let monitor = match overlay.primary_monitor() {
        Ok(monitor) => monitor,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the primary monitor");
            None
        }
    };
    let scale = overlay_scale(monitor.as_ref());
    // Work area, not the full monitor: the taskbar is not somewhere to put a
    // window the user is meant to be able to read.
    let area = monitor.as_ref().map(|monitor| {
        let area = monitor.work_area();
        WorkArea {
            x: area.position.x,
            y: area.position.y,
            width: area.size.width,
            height: area.size.height,
        }
    });

    // Sized and placed even while hidden: the next show then has nothing left
    // to arrange, and a hidden window's geometry costs nothing.
    //
    // **Placed before it is sized.** Moving a window is what makes Windows
    // re-evaluate its DPI, and tao's `WM_DPICHANGED` re-derives the window's
    // size when it does — so doing the move first means the size lands at the
    // DPI the window is going to keep. Both are messages on one event-loop
    // queue, so the ordering is structural rather than timed. (This was once
    // believed to be *the* fix; the failure it was chasing turned out to be the
    // probe's ruler. It is kept because the ordering is still the right one and
    // costs nothing, not because anything is known to need it.)
    let size = expected_size(shape, scale);
    match area {
        Some(area) => place_overlay(&overlay, size, area, scale),
        // A headless RDP session is the case that really occurs. An overlay in
        // the wrong corner is worth more than no overlay.
        None => tracing::debug!("no primary monitor; leaving the overlay where it is"),
    }
    size_overlay(&overlay, shape, scale);

    if !visible {
        // Hiding clobbers the extended styles too — it is the same tao
        // `set_visible` — but a hidden window is in no Alt-Tab list to be
        // wrongly listed in, and the next show repairs it before anyone can
        // see it.
        if let Err(err) = overlay.hide() {
            tracing::warn!(error = %err, "could not hide the overlay");
        }
        return;
    }
    // Asked on *every* state change, not only on the first one, and that is
    // free: tao's `apply_diff` returns before touching the window when the flag
    // diff is empty (window_state.rs:321), so a show on an already-visible
    // window makes no `ShowWindow` call, no z-order change and no style write.
    // What it buys is that a *real* hidden→visible transition — whichever state
    // change happens to carry it — always ends with the fixup below.
    //
    // And every such transition is still non-activating: the `MARKER_DONT_FOCUS`
    // that turns `ShowWindow` into `SW_SHOWNOACTIVATE` is cleared only on a
    // *local copy* of the flags (`old_flags.apply_diff(...)` takes them by
    // value), so it survives in the stored state and the second show is as
    // quiet as the first (invariant 2, read off tao 0.35.3 rather than hoped).
    //
    // A resize, by the same early return, does **not** clobber the extended
    // styles: `set_inner_size` asks for MAXIMIZED to be cleared and it is
    // already clear, so nothing is written. (It was once claimed otherwise;
    // a trace disproved it — the second fixup found the word already
    // correct.) The show is what clobbers them, and the fixup is behind it.
    if let Err(err) = overlay.show() {
        tracing::warn!(error = %err, "could not show the overlay");
        return;
    }
    // Strictly after the show, and only when the show was accepted: this is the
    // repair for what the show itself breaks.
    restyle_after_show(&overlay);
}

/// The scale factor the pill's geometry is measured in: **the monitor's, not
/// the window's**.
///
/// Not because the window's is wrong — it never was;
/// the probe that appeared to catch it reading 1.0 was itself measuring through
/// a DPI-virtualized ruler. It is the monitor's because *this* is the number the
/// placement arithmetic below needs: the work area is that monitor's, in that
/// monitor's physical pixels, and mixing it with a scale factor read from
/// somewhere else is how the two halves of one geometry drift apart. It is also
/// a fresh read rather than a stored one, and it is by construction the DPI the
/// window has once it is sitting in that monitor's corner — which is the DPI
/// WebView2 rasterises the page's CSS pixels at, so the window and the pill
/// inside it agree.
///
/// With no monitor there is nothing to place against either, and 1.0 leaves the
/// window whatever size its logical request gives it.
fn overlay_scale(monitor: Option<&tauri::Monitor>) -> f64 {
    monitor
        .map(tauri::Monitor::scale_factor)
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0)
}

/// The physical size the window should end up with, for placement arithmetic.
///
/// Computed rather than read back: `set_size` is a message to the event loop
/// and `set_inner_size_physical` posts its `SetWindowPos` on from there with
/// `SWP_ASYNCWINDOWPOS`, so an `outer_size()` immediately afterwards can still
/// answer with the old one — and placing a window against the wrong width is
/// how an anchored edge drifts by exactly the difference.
const fn expected_size(shape: PillShape, scale: f64) -> WindowSize {
    let (width, height) = shape.logical_size();
    WindowSize {
        width: scaled(width, scale),
        height: scaled(height, scale),
    }
}

/// Resize the window to hug `shape`.
///
/// **Logical, not physical**, and the honest reason is not the one first
/// guessed at. An early diagnosis read a physical 90×83 request coming back as
/// 72×66 and blamed a rescale inside tao's `WM_DPICHANGED`; in fact the window
/// had been 90×83 all along and the *probe* was reading through a
/// DPI-virtualized ruler.
/// Both units work, and the app's own read-back proves this one does:
/// `logical 72×66 → actual 90×83` on a 125% display. (Those are the numbers
/// that were measured; the idle pill is 81×74 logical after the ×1.25
/// scaling.)
///
/// Logical is kept because it is the unit everything else here is already
/// written in — `tauri.conf.json`'s window size, the design's measurements, and
/// the CSS the page lays the pill out with — so there is one number per
/// dimension in the whole system rather than a physical translation of it that
/// has to be kept in step. Tauri and tao do the conversion once, at the layer
/// that also decides what a CSS pixel is.
///
/// The `ScaleFactorChanged` hook in `main.rs` re-applies the shape if the
/// display's scaling ever changes underneath it, so a stale conversion repairs
/// itself rather than persisting.
fn size_overlay(overlay: &WebviewWindow, shape: PillShape, scale: f64) {
    let (width, height) = shape.logical_size();
    if let Err(err) = overlay.set_size(LogicalSize::new(width, height)) {
        tracing::warn!(error = %err, "could not resize the overlay");
        return;
    }
    // What the window says it is *after* the resize has been queued. Purely
    // diagnostic — nothing here acts on it — but it is what turns the next
    // probe run into a measurement rather than another round of inference.
    let actual = overlay.outer_size().ok();
    tracing::debug!(
        shape = ?shape,
        logical_width = width,
        logical_height = height,
        scale,
        expected_width = scaled(width, scale),
        expected_height = scaled(height, scale),
        actual_width = actual.map(|size| size.width),
        actual_height = actual.map(|size| size.height),
        "overlay size"
    );
}

/// Logical pixels → physical, for whatever this monitor's scale factor is.
///
/// Used for the *placement* arithmetic only. The window's size is asked for in
/// logical units — see [`size_overlay`] for why that distinction is the fix.
const fn scaled(logical: u32, scale: f64) -> u32 {
    let physical = logical as f64 * scale;
    if physical < 1.0 {
        return 1;
    }
    (physical + 0.5) as u32
}

/// Put the overlay in the configured corner of the primary monitor's work area.
///
/// Recomputed at every show *and* at every state resize rather than remembered:
/// monitors get unplugged and resolutions change between sessions, a remembered
/// position is how an overlay ends up drawn off the side of a screen that no
/// longer exists — and a window that changes width without being re-placed
/// drags its anchored edge with it.
///
/// `area` and `size` are both in the same physical pixels — the monitor's, via
/// [`overlay_scale`] — which is the invariant that keeps the anchored edge
/// where it belongs.
fn place_overlay(overlay: &WebviewWindow, size: WindowSize, area: WorkArea, scale: f64) {
    let (x, y) = overlay_position(
        area,
        size,
        overlay_corner(overlay.app_handle()),
        // Signed, and usually zero — see OVERLAY_OFFSET.
        (f64::from(OVERLAY_OFFSET) * scale).round() as i32,
    );
    if let Err(err) = overlay.set_position(PhysicalPosition::new(x, y)) {
        tracing::warn!(error = %err, "could not place the overlay");
    }
}

/// The display's DPI changed under the overlay: re-place it.
///
/// Every number in the pill's geometry is derived from the monitor's scale
/// factor, so a display-settings change or a move between monitors of different
/// DPI invalidates all of them at once. This is one message on the same queue
/// as everything else here — it re-applies the *current* shape, so it can
/// neither change what the pill is showing nor make it visible.
pub fn on_overlay_scale_change(app: &AppHandle) {
    if let Some(state) = app.try_state::<ShellState>() {
        state.overlay(OverlayInput::Moved);
    }
}

/// The corner the configuration names, or the default if it cannot be read yet.
///
/// Read at the moment of use, like every other configuration value in this
/// file, so a change to the setting needs nothing invalidated: the next
/// placement simply reads the new one.
fn overlay_corner(app: &AppHandle) -> OverlayCorner {
    app.try_state::<ShellState>()
        .and_then(|state| state.with_config(|config| config.overlay_corner))
        .unwrap_or_default()
}

/// Re-apply the extended styles behind the `show()` that has just gone out.
///
/// **What clobbers them.** tao does not treat the extended-style word as ours:
/// `WindowState::set_window_flags` rewrites the whole word from the
/// `WindowFlags` *it* stores whenever it processes a visibility change. The
/// first implementation set the bits once at setup and they were gone by the
/// first show — the instrumented probe run read `0x08040118` off the live
/// window (`WS_EX_APPWINDOW` back, `WS_EX_TOOLWINDOW` gone), byte for byte the
/// value that had been logged as `before` at setup. `WS_EX_NOACTIVATE`
/// and `WS_EX_TOPMOST` survive only because tao holds those as flags of its
/// own; `WS_EX_TOOLWINDOW` is a bit it does not know about, so it is a bit it
/// erases.
///
/// **Why the ordering holds without sleeping.** `show()` and
/// `run_on_main_thread` are the same mechanism — `send_user_message` on one
/// event-loop queue. Called off the main thread, the show is a
/// `Message::Window(.., Show)` and this is a `Message::Task` queued behind it,
/// so tao's style write is processed first; called on the main thread, the show
/// has already run inline before this line executes. Either way the fixup lands
/// after the write it is repairing, structurally rather than by timing.
///
/// The closure captures the *window*, not its `HWND`: it resolves the handle on
/// the main thread at the moment it runs, and a destroyed window yields an
/// error there instead of a dangling pointer here. In practice the overlay
/// lives until [`quit`] destroys it, and both that destroy and this fixup are
/// messages processed on the main thread in the order they were queued, so the
/// two cannot interleave — capturing the window means not having to rely on
/// that. (The destroy once ran on the event-loop thread itself; now
/// it comes from the tray thread, which is the same queue by a longer road.)
///
/// The window carries `WS_EX_APPWINDOW` for the few milliseconds in between:
/// an Alt-Tab entry that appears and vanishes faster than the key combination
/// that would reveal it. The alternative is blocking a caller on the event
/// loop, which is worse (invariant 5).
#[cfg(windows)]
fn restyle_after_show(overlay: &WebviewWindow) {
    let window = overlay.clone();
    if let Err(err) = overlay.run_on_main_thread(move || fix_overlay_styles(&window)) {
        tracing::warn!(error = %err, "could not queue the overlay's style fixup");
    }
}

/// Nothing to re-apply elsewhere: the extended-style words are a Win32 concept,
/// and other platforms ship whatever Tauri's portable flags give them (parity
/// is a deferred item, not a silent gap).
#[cfg(not(windows))]
fn restyle_after_show(_overlay: &WebviewWindow) {}

/// The extended-style word the overlay should carry, given the one it has.
///
/// `skipTaskbar: true` only calls `ITaskbarList::DeleteTab`, which takes the
/// taskbar *button* away and nothing else; an unowned window still carries
/// `WS_EX_APPWINDOW`, and that is what keeps it in Alt-Tab
/// (tauri-apps/tauri#10422). `WS_EX_TOOLWINDOW` is the one flag that removes it
/// from both, and no Tauri or tao option sets it. `skipTaskbar` stays on as
/// well, belt and braces for the button.
///
/// Pure, and separate from the two Win32 calls, so the one thing here that can
/// be wrong arithmetically is pinned by a test against the real values a probe
/// run logged off the live window.
#[cfg(windows)]
fn overlay_ex_style(before: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};

    (before | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize)
}

/// Give the overlay the extended styles Tauri has no configuration for.
///
/// Runs on the main thread, posted by [`restyle_after_show`] — which is also
/// where the reason it has to run after every show is written down.
///
/// Neither call can activate anything (invariant 2): they read and write one
/// word of window state and touch neither z-order nor focus.
#[cfg(windows)]
fn fix_overlay_styles(overlay: &WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE,
    };

    let hwnd = match overlay.hwnd() {
        // Tauri's `HWND` is the `windows` crate's newtype; `windows-sys` uses
        // the bare pointer it wraps, and they are the same type underneath.
        Ok(handle) => handle.0,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the overlay's window handle");
            return;
        }
    };

    // SAFETY: `hwnd` was resolved from the window handle a moment ago on the
    // thread that owns the window, so it names a live window this process owns;
    // `GWL_EXSTYLE` is a valid index for both calls.
    let (before, after) = unsafe {
        let before = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let after = overlay_ex_style(before);
        if after != before {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, after);
        }
        (before, after)
    };
    tracing::debug!(
        before = format!("{before:#x}"),
        after = format!("{after:#x}"),
        "overlay extended style"
    );
}

/// What the overlay thread reacts to.
///
/// Every one of these is a fact that already travels to the window as an event;
/// nothing here is a second opinion. The two windows and this thread are three
/// readings of the same three facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayInput {
    /// Whether the overlay should be on screen at all: the config's switch AND
    /// a ready session.
    Visible(bool),
    /// `sotone://recording` moved.
    Recording(bool),
    /// `sotone://line` — a fresh decode is being revealed.
    Line,
    /// The corner setting changed; re-place whatever is showing.
    Moved,
}

/// The one thread that touches the overlay window.
///
/// **Why a thread of its own.** Two of the four things that resize the pill are
/// *timeouts* — the reveal running out, and the grace after a release that no
/// line ever answers — and neither the control thread (a blocking `for` over
/// its inbox) nor the drain thread (the same, over the worker's events) has
/// anywhere to put a deadline. It also means every window call in this file is
/// made from one place, in one order, which is worth more for invariant 2 than
/// the thread costs: it sleeps until something happens and does nothing else.
///
/// It holds no handle, no lock and no draft. When the channel closes — the app
/// is going away — it stops.
fn spawn_overlay(app: AppHandle, state: ShellState, inputs: Receiver<OverlayInput>) {
    let spawned = thread::Builder::new()
        .name("sotone-overlay".to_owned())
        .spawn(move || {
            let mut sizer = PillSizer::new();
            let mut visible = false;

            loop {
                let received = match sizer.wait(Instant::now()) {
                    Some(timeout) => match inputs.recv_timeout(timeout) {
                        Ok(input) => Some(input),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                    None => match inputs.recv() {
                        Ok(input) => Some(input),
                        Err(_) => break,
                    },
                };

                let now = Instant::now();
                let shape = match received {
                    // The deadline: the reveal is over and the CSS collapse has
                    // had its 260 ms, so the window may shrink to the glyph.
                    None => sizer.apply(PillEvent::Elapsed, now, reveal_duration(&state)),
                    Some(OverlayInput::Recording(true)) => {
                        sizer.apply(PillEvent::RecordingStarted, now, reveal_duration(&state))
                    }
                    Some(OverlayInput::Recording(false)) => {
                        sizer.apply(PillEvent::RecordingStopped, now, reveal_duration(&state))
                    }
                    Some(OverlayInput::Line) => {
                        sizer.apply(PillEvent::LineLanded, now, reveal_duration(&state))
                    }
                    Some(OverlayInput::Visible(wanted)) => {
                        visible = wanted;
                        // A pill that comes back comes back as the glyph: the
                        // state it was in belonged to a recording that is over.
                        if !wanted {
                            sizer = PillSizer::new();
                        }
                        sizer.shape
                    }
                    Some(OverlayInput::Moved) => sizer.shape,
                };

                // Applied unconditionally rather than only on a change: every
                // call underneath is idempotent, and a "has it changed" test
                // here would be a fourth copy of the state to get wrong. These
                // arrive a handful of times per utterance, not per frame.
                apply_overlay(&app, shape, visible);
            }
            tracing::debug!("the overlay thread has stopped");
        });
    if let Err(err) = spawned {
        // Not fatal, and not a notice: the overlay is optional by design, and
        // an app that refuses to start because a decoration could not would be
        // the wrong trade.
        tracing::warn!(error = %err, "running without the overlay");
    }
}

/// How long a revealed line stays on the pill, as the configuration has it.
fn reveal_duration(state: &ShellState) -> Duration {
    let seconds = state
        .with_config(|config| clamp_reveal_seconds(config.reveal_seconds))
        .unwrap_or(DEFAULT_REVEAL_SECONDS);
    Duration::from_secs(u64::from(seconds))
}

/// Turn the capture worker's mic levels into `sotone://level`.
///
/// A thread that does one thing: the channel is silent unless a recording is
/// live, so this sleeps through everything else, and when the engine goes away
/// the channel closes and it stops.
///
/// **Invariant 5.** Nothing here runs on a callback: the level was computed on
/// the capture worker (which had already drained those samples for its own
/// reasons) and this thread only serialises it. The emit is broadcast to every
/// webview, like every other event in this file — deliberately *not* gated on
/// `config.overlay`, because that switch already decides whether there is a
/// window to draw it, and a second copy of the answer is a second thing to get
/// out of step.
fn spawn_level_drain(app: AppHandle, levels: Receiver<f32>) {
    let spawned = thread::Builder::new()
        .name("sotone-level".to_owned())
        .spawn(move || {
            for level in levels {
                emit(&app, EVENT_LEVEL, LevelEvent { level });
            }
            tracing::debug!("the mic level feed has ended");
        });
    if let Err(err) = spawned {
        tracing::warn!(error = %err, "running without the overlay's level meter");
    }
}

// ---------------------------------------------------------------------------
// Line editing
//
// Every one of these is send-only, like the draft commands: the worker owns the
// active draft's handle, so an edit is a message to it and never a write from
// this thread. The result comes back as `sotone://lines`.
//
// They apply to the **active** draft. Editing some other draft would mean a
// second handle on a directory the worker is appending to; resuming the draft
// first is the flow: line editing is post-session tidy-up.
// ---------------------------------------------------------------------------

/// Replace one line's text.
#[tauri::command]
pub fn line_edit(id: String, text: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Line(LineInput::Edit { id, text }));
}

/// Soft-delete a line, or bring it back.
///
/// Nothing is destroyed: the record and its wav stay exactly where they are and
/// the line simply stops being rendered into the markdown (invariant 4).
#[tauri::command]
pub fn line_set_deleted(id: String, deleted: bool, state: State<'_, ShellState>) {
    state.send(ShellInput::Line(LineInput::SetDeleted { id, deleted }));
}

/// Move one line so it sits immediately after `after`, or to the top of the
/// transcript when `after` is `null`.
///
/// Nothing is rewritten: the worker appends one move record and the loader folds
/// it, so the order the lines were spoken in is still on disk (invariant 4).
/// `after` is required and nullable all the way down — "absent" is not a
/// destination.
#[tauri::command]
pub fn line_move(id: String, after: Option<String>, state: State<'_, ShellState>) {
    state.send(ShellInput::Line(LineInput::Move { id, after }));
}

/// Move one line out of the open note and into another one.
///
/// Send-only and **one line at a time**: the window confirms once and issues N
/// of these in transcript order, so this stays the sibling of
/// [`line_set_deleted`] rather than a second, wider write path. `target` is the
/// destination draft's id; the source is always the active draft, because a
/// selection only exists in the open transcript.
///
/// Nothing is destroyed on either side (invariant 4). The destination gains
/// appended records and a byte copy of the line's wav; the source gains the
/// ordinary soft delete, which is what makes leaving it one Undo away. The
/// worker does all of it, destination first — see `DraftSlot::move_line_to`.
///
/// `first` is the batch boundary, and it is the window's to declare:
/// `true` on the first of the confirm's N calls, `false` on the rest.
/// The whole batch lands under **one** session divider, and nothing downstream
/// could work out where a batch begins — once the first line is on disk the
/// destination's log tail is a line record like any other. It is IPC only; no
/// record on disk gains a field.
#[tauri::command]
pub fn line_move_to(id: String, target: String, first: bool, state: State<'_, ShellState>) {
    state.send(ShellInput::Line(LineInput::MoveTo { id, target, first }));
}

/// Run the model over one line's stored audio again.
///
/// The work happens on the worker thread, between utterances — it owns the
/// transcriber, and live speech is served first.
#[tauri::command]
pub fn line_retranscribe(id: String, state: State<'_, ShellState>) {
    state.send(ShellInput::Line(LineInput::Retranscribe(id)));
}

/// One line's stored audio, base64'd, for a local `Audio` element.
///
/// The only command in this file that touches the filesystem, hence `async`:
/// Tauri runs those off the thread that services IPC, so a slow disk cannot
/// stall the window (invariant 5 in spirit).
///
/// Both ids are validated inside `sotone-core` before anything is joined onto
/// the drafts root — they arrived from JavaScript, so they are input, not
/// facts. Nothing here reaches the network and no asset protocol is enabled:
/// the bytes go straight over IPC to an `Audio` element in the same window
/// (invariant 3).
///
/// # Errors
/// A message to show next to the play button: the ids were bad, the wav is
/// missing, or startup has not finished.
#[tauri::command]
pub async fn line_audio(
    draft_id: String,
    line_id: String,
    state: State<'_, ShellState>,
) -> Result<String, String> {
    let store = state
        .store()
        .ok_or_else(|| "Sotone is still starting up".to_owned())?;
    let path = store
        .line_audio_path(&draft_id, &line_id)
        .map_err(|err| err.to_string())?;
    let bytes =
        std::fs::read(&path).map_err(|err| format!("could not read {}: {err}", path.display()))?;
    Ok(base64(&bytes))
}

/// RFC 4648 base64, standard alphabet, padded.
///
/// Hand-rolled rather than adding a crate: this is the only encoder in the app,
/// it is a dozen lines, and every dependency here is one more thing in a build
/// that is deliberately local and small. Checked against the RFC's own vectors
/// in the tests below.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut triple = 0_u32;
        for (i, byte) in chunk.iter().enumerate() {
            triple |= u32::from(*byte) << (16 - 8 * i);
        }
        let symbol = |shift: u32| char::from(ALPHABET[(triple >> shift) as usize & 0x3f]);
        out.push(symbol(18));
        out.push(symbol(12));
        // Padding stands in for bytes that are not there, never for zero bytes:
        // a two-byte tail has three meaningful symbols, a one-byte tail two.
        out.push(if chunk.len() > 1 { symbol(6) } else { '=' });
        out.push(if chunk.len() > 2 { symbol(0) } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// Window events
// ---------------------------------------------------------------------------

/// What the X does with the main window.
///
/// Two outcomes and no third: either Sotone goes away through [`quit`] — the
/// one teardown, the one the tray's Exit uses — or the close is prevented and
/// the window is hidden while everything behind it keeps running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    /// Leave, through [`quit`].
    Quit,
    /// `prevent_close` + `hide`. Sotone lives on in the tray.
    Hide,
}

/// The whole close rule, as a function of the three facts that decide it.
///
/// Pure, and separate from [`on_close`], because the ordering *is* the rule and
/// a rule that can only be exercised by pressing a real X is a rule nothing
/// checks. The order, and why it is this one:
///
/// 1. **Not onboarded → quit.** A first launch that is closed should be gone
///    deliberately: there is no session to protect yet, and a
///    wizard that vanished into the notification area would be a first
///    impression of an app that would not close.
/// 2. **`close_quits` → quit.** The user asked for an app that is not
///    tray-resident, and the setting is the consent — no dialog, no
///    confirmation.
/// 3. **No tray → quit.** The escape hatch: hiding to a tray that failed
///    to build would strand a running app with no way back to it.
/// 4. **Otherwise hide.** The standing rule, and the mode Sotone is actually used
///    in.
fn close_action(onboarded: bool, close_quits: bool, tray_alive: bool) -> CloseAction {
    if !onboarded || close_quits || !tray_alive {
        CloseAction::Quit
    } else {
        CloseAction::Hide
    }
}

/// The X **hides the window**, except in the two cases carved
/// out of that rule.
///
/// Hiding is the mode the app is actually used in: dictation lives entirely in
/// the backend, so a window that is in the way should get out of the way
/// without taking the microphone with it. [`quit`] is the one true exit, the
/// tray's Exit item is the ordinary way to reach it, and every route here that
/// leaves goes through that same function — there is no second teardown.
///
/// The JS caption button takes this same path — `getCurrentWindow().close()`
/// raises `CloseRequested` exactly as the native X does — so
/// there is one close behaviour, not two, and the wizard's caption close needs
/// no code of its own.
///
/// [`close_action`] has the ordering and the reasons. The one thing decided
/// here rather than there is the state being missing at all: nothing can be
/// asked about a shell that does not exist, and a close that did nothing would
/// strand a running app — so it quits, which is the direction this has always
/// failed in.
///
/// `close_quits` is read from the configuration **at close time**. That is what
/// makes the setting live with no session command and no cached copy: there is
/// exactly one place the answer lives, and it is the file.
///
/// Nothing here focuses, activates or raises anything: `hide` is the opposite
/// motion and a quit shows nothing at all (invariant 2).
pub fn on_close(window: &Window, api: &CloseRequestApi) {
    let app = window.app_handle();
    // The overlay has no decorations, no close button and no way for a user to
    // ask it to close; if something ever does, it is not the main window and it
    // is not this rule.
    if window.label() != MAIN_LABEL {
        return;
    }

    let Some(state) = app.try_state::<ShellState>() else {
        tracing::warn!("no shell state, so closing the window quits Sotone");
        quit(app);
        return;
    };

    let onboarded = state.onboarded();
    // `false` when the configuration has not been handed over yet, which is the
    // standing rule and the right answer while startup is still running.
    let close_quits = state
        .with_config(|config| config.close_quits)
        .unwrap_or(false);
    let tray_alive = state.tray_alive();

    match close_action(onboarded, close_quits, tray_alive) {
        CloseAction::Quit => {
            if tray_alive {
                tracing::info!(onboarded, close_quits, "closing the window quits Sotone");
            } else {
                tracing::warn!("no tray icon, so closing the window quits Sotone");
            }
            quit(app);
        }
        CloseAction::Hide => {
            api.prevent_close();
            if let Err(err) = window.hide() {
                tracing::warn!(error = %err, "could not hide the window");
            }
        }
    }
}

/// Leave. The one true exit.
///
/// Called from the tray's Exit item and from nowhere else — except the escape
/// hatch in [`on_close`], for the case where there is no tray to leave to.
///
/// Best-effort orderly stop, as closing the window used to be: the control
/// thread is asked to stop, which drops the [`Session`] — helper first, then
/// the engine, which closes the utterance channel and lets the worker finish
/// its backlog — and the draft is fsync'd per line, so a hard exit loses
/// nothing either way. The helper cannot be orphaned even if the process goes
/// first: its stdin is our liveness signal and EOF makes it exit.
///
/// The overlay is destroyed rather than left to the runtime, for one reason:
/// nothing else in the app can ever close it, so it is torn down
/// explicitly wherever Sotone goes away. `destroy` rather than `close`, because
/// a close request would come back through [`on_close`].
pub fn quit(app: &AppHandle) {
    tracing::info!("quitting at the user's request");
    if let Some(state) = app.try_state::<ShellState>() {
        state.send(ShellInput::Quit);
    }
    if let Some(overlay) = app.get_webview_window(OVERLAY_LABEL) {
        if let Err(err) = overlay.destroy() {
            tracing::warn!(error = %err, "could not close the overlay");
        }
    }
    // Programmatic, so `RunEvent::ExitRequested` carries `Some(0)` and the
    // guard in `main.rs` — which exists to stop an *unasked-for* exit — lets it
    // through.
    app.exit(0);
}

// ---------------------------------------------------------------------------
// The recording state machine (pure)
// ---------------------------------------------------------------------------

/// Which mode owns the live recording.
///
/// The other mode's key is ignored until this one ends it: one recording state,
/// one source, no interleaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Held key.
    Ptt,
    /// Press to start, press to stop.
    Toggle,
}

impl Source {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Ptt => "push-to-talk",
            Self::Toggle => "toggle",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything that can change the recording state, whatever produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    /// The push-to-talk key went down.
    Press,
    /// The push-to-talk key came up, as the hook timed it.
    Release {
        /// Hook timestamp.
        at: SystemTime,
    },
    /// One press of the toggle key. A *stopping* press is the line's timestamp.
    Toggle {
        /// Hook timestamp.
        at: SystemTime,
    },
    /// The user disarmed capture from the UI.
    Disarmed {
        /// When the command arrived.
        at: SystemTime,
    },
}

/// Why an input did nothing, phrased for the notice area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoreReason {
    /// Capture is disarmed, so a start was refused.
    Disarmed,
    /// The other mode owns the live recording.
    Busy {
        /// The mode that owns it.
        source: Source,
    },
    /// A release with nothing running.
    NotRecording,
}

impl fmt::Display for IgnoreReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disarmed => f.write_str("ignored: capture is disarmed"),
            Self::Busy { source } => write!(f, "ignored: {source} is already recording"),
            Self::NotRecording => f.write_str("ignored: nothing was recording"),
        }
    }
}

/// What the control thread should do about an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing at all — not even worth a notice.
    None,
    /// Open an utterance.
    Start {
        /// The mode that started it.
        source: Source,
    },
    /// Close the utterance and hand the worker its timestamp.
    Stop {
        /// The mode that owned it.
        source: Source,
        /// The line's `spoken_at`.
        at: SystemTime,
        /// How long the key was held, measured monotonically.
        held: Duration,
    },
    /// The input was deliberately dropped; tell the user why.
    Ignored {
        /// The reason, ready to display.
        reason: IgnoreReason,
    },
}

/// The single recording state both modes share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RecordingState {
    #[default]
    Idle,
    Active {
        source: Source,
        /// `Instant`, not wall clock: the hold length must not depend on
        /// whether the clock was stepped mid-note.
        started: Instant,
    },
}

/// The decision logic, with no I/O in it at all.
///
/// Everything that makes the shell's behaviour arguable — the armed gating, the
/// disarm stop, which mode may end which recording — is decided here, so it can
/// be tested without a device, a model or a window.
#[derive(Debug, Clone, Copy, Default)]
pub struct Machine {
    state: RecordingState,
}

impl Machine {
    /// Fold one input into the state.
    ///
    /// `armed` is the Arm/Disarm button, evaluated by the caller at the moment
    /// the input is handled — the rdev callback stays a pure forwarder, so
    /// arming is always decided consumer-side. `now` is passed in rather than
    /// read here to keep this deterministic.
    pub fn apply(&mut self, input: KeyInput, armed: bool, now: Instant) -> Action {
        match input {
            KeyInput::Press => self.start(Source::Ptt, armed, now),
            KeyInput::Toggle { at } => match self.state {
                RecordingState::Idle => self.start(Source::Toggle, armed, now),
                // The stopping press is as close as toggle mode gets to a key
                // release, so it is the line's timestamp.
                RecordingState::Active {
                    source: Source::Toggle,
                    started,
                } => self.stop(Source::Toggle, at, now - started),
                RecordingState::Active {
                    source: Source::Ptt,
                    ..
                } => Action::Ignored {
                    reason: IgnoreReason::Busy {
                        source: Source::Ptt,
                    },
                },
            },
            // A release always stops, armed or not: disarming mid-hold must not
            // strand a recording, and the audio is already captured.
            KeyInput::Release { at } => match self.state {
                RecordingState::Active {
                    source: Source::Ptt,
                    started,
                } => self.stop(Source::Ptt, at, now - started),
                RecordingState::Active {
                    source: Source::Toggle,
                    ..
                } => Action::Ignored {
                    reason: IgnoreReason::Busy {
                        source: Source::Toggle,
                    },
                },
                RecordingState::Idle => Action::Ignored {
                    reason: IgnoreReason::NotRecording,
                },
            },
            // Disarming is the one stop the user makes with the mouse rather
            // than the key. Without it a recording live at that moment would run
            // to the cap with no input left that could end it: the next press or
            // release is refused precisely because capture is now disarmed.
            KeyInput::Disarmed { at } => match self.state {
                RecordingState::Active { source, started } => self.stop(source, at, now - started),
                RecordingState::Idle => Action::None,
            },
        }
    }

    fn start(&mut self, source: Source, armed: bool, now: Instant) -> Action {
        match self.state {
            RecordingState::Active { source: owner, .. } => Action::Ignored {
                reason: IgnoreReason::Busy { source: owner },
            },
            RecordingState::Idle if !armed => Action::Ignored {
                reason: IgnoreReason::Disarmed,
            },
            RecordingState::Idle => {
                self.state = RecordingState::Active {
                    source,
                    started: now,
                };
                Action::Start { source }
            }
        }
    }

    fn stop(&mut self, source: Source, at: SystemTime, held: Duration) -> Action {
        self.state = RecordingState::Idle;
        Action::Stop { source, at, held }
    }

    /// Is a recording running right now?
    ///
    /// Only the draft commands ask: discarding the draft a live recording is
    /// about to land in would throw away audio the user is still speaking.
    const fn is_recording(&self) -> bool {
        match self.state {
            RecordingState::Active { .. } => true,
            RecordingState::Idle => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Control thread
// ---------------------------------------------------------------------------

/// What the control thread reacts to.
///
/// `Debug` exists for the `RUST_LOG=debug` input trace: the second of the two
/// checkpoints that say how far a key event got.
#[derive(Debug)]
enum ShellInput {
    /// Something that can change the recording state.
    Key(KeyInput),
    /// The hook is gone; no further key events will arrive.
    HookLost(String),
    /// Something the user asked of the draft list.
    Draft(DraftInput),
    /// Something the user asked of a line in the active draft.
    Line(LineInput),
    /// Something from the Settings tab that only the control thread can do.
    Settings(SettingsInput),
    /// The window is closing.
    Quit,
}

/// The settings work that has to happen on the control thread.
///
/// Everything else Settings does — writing the config, scanning the models
/// folder, opening the file picker — happens in the async command itself. What
/// is here is the helper process lifecycle, which has exactly one owner.
#[derive(Debug)]
enum SettingsInput {
    /// Begin capture for a mode. The string is checked here rather than in the
    /// command so an unknown mode is one refusal in one place.
    CaptureStart(String),
    /// Stop capturing and put the previous bindings back.
    CaptureCancel,
    /// The capture helper reported the press. The token is a
    /// [`Binding`] spelling and is parsed before anything is written.
    Captured {
        /// What the helper said.
        token: String,
    },
    /// The capture helper stopped without reporting a press — it crashed, its
    /// hook died, or it could not start. Capture must never leave the app
    /// hookless, so this respawns the ordinary helper like a cancel does.
    CaptureEnded {
        /// Why, for the notice.
        reason: String,
    },
    /// The configuration's bindings changed; the running helper should now be
    /// watching the new ones.
    Rehook,
    /// Cues on or off, for the worker and for the begin cue this thread plays.
    SetCues(bool),
    /// Open a different microphone. Here rather than in the command
    /// because this thread owns the [`AudioEngine`] — it is not `Sync` — and
    /// because closing and opening a device is blocking work.
    SetMic(Option<String>),
    /// A model that is loaded and warmed, on its way to the worker.
    /// Only this thread has the channel to it.
    SetTranscriber(Box<Transcriber>),
    /// A language for the running model. Same routing, no load.
    SetLanguage(Language),
}

/// The five things the transcript panel can ask for. Undo is not among them:
/// it is the panel's own stack of these same commands, so a step back is
/// indistinguishable from the user making the correction by hand.
#[derive(Debug)]
enum LineInput {
    /// Replace one line's text.
    Edit {
        /// Line ulid.
        id: String,
        /// The new text.
        text: String,
    },
    /// Put one line somewhere else in the order.
    Move {
        /// Line ulid.
        id: String,
        /// The line it should now follow; `None` is the top.
        after: Option<String>,
    },
    /// Put one line in a different note altogether.
    MoveTo {
        /// Line ulid, in the active draft.
        id: String,
        /// The destination draft's ulid. Never the active one — the worker
        /// refuses that outright rather than opening a second handle.
        target: String,
        /// Whether this is the first of the confirm's N calls, and so the one
        /// the destination's session divider goes above. It becomes
        /// `SessionCommand::MoveLineTo::divide` one hop from here.
        first: bool,
    },
    /// Soft-delete or restore.
    SetDeleted {
        /// Line ulid.
        id: String,
        /// The state to move to.
        deleted: bool,
    },
    /// Transcribe the stored audio again.
    Retranscribe(String),
}

/// What the drafts panel can ask for.
#[derive(Debug)]
enum DraftInput {
    /// Start a fresh draft and make it active.
    New,
    /// Resume this draft.
    Open(String),
    /// Move this draft to `.trash/`.
    Discard(String),
    /// Render the active draft to its markdown file.
    Save {
        /// Only ever `true` from the conflict dialog.
        overwrite: bool,
    },
    /// Render every dirty note in the store, each under its own project's
    /// rules. Guarded, always.
    SaveAll,
    /// Rename the `.md` file this draft is bound to.
    Rename {
        /// Which draft.
        id: String,
        /// The new name, as typed. Sanitized in `sotone-core`.
        name: String,
    },
    /// Move one existing note into another project.
    ///
    /// Nothing to do with [`DraftInput::SetProject`] below, which tags the next
    /// draft the worker has to invent: this one moves a note that exists — its
    /// `.md` and its binding together.
    SetDraftProject {
        /// Which draft.
        id: String,
        /// The project it joins, or `None` for the tree's "no project" group.
        project: Option<String>,
        /// The user's answer to a name clash. Never `true` on a first drop.
        keep_both: bool,
    },
    /// A project was renamed; carry every draft that names it.
    /// Routed through the control thread because the worker is the only thing
    /// that may hold a draft handle, and the command that starts this is an
    /// async Tauri command with no channel of its own.
    ProjectRenamed {
        /// The name drafts still carry.
        from: String,
        /// The name they should carry.
        to: String,
    },
    /// The active project changed; the worker tags the next draft it has to
    /// invent with this one.
    SetProject(Option<String>),
}

/// Everything the running session owns.
///
/// Field order is the drop order, and it is deliberate: the hook helper goes
/// first, so no key event can arrive mid-teardown, and dropping the engine then
/// closes the utterance channel, which is what lets the worker finish its
/// backlog before its own `Drop` joins it.
struct Session {
    /// The helper process watching the bindings. Its `Drop` is the "no orphans"
    /// guarantee. `None` only while a capture is running or a respawn failed —
    /// and both of those are states the window is told about, because an app
    /// with no helper hears no key.
    hook: Option<HookProcess>,
    engine: AudioEngine,
    worker: SessionWorker,
    cues: Option<Arc<CuePlayer>>,
    release_tx: Sender<ReleaseInfo>,
    /// Draft switches and discards, to the thread that owns the handle. The
    /// shell never holds a second [`Draft`] on the active draft — it creates or
    /// opens one and hands it straight over.
    draft_tx: Sender<SessionCommand>,
    /// Clonable, holds no handle: used here to create/open/discard, and by the
    /// drain thread to re-list.
    store: DraftStore,
}

/// The rebind machinery, and it lives **outside** [`Session`] on purpose.
///
/// A capture is the one piece of helper lifecycle that has to work before there
/// is a session to own it: with no model set up, startup stops before the
/// microphone, whisper and the hook — the empty phase — and the wizard's key
/// step is on screen in exactly that phase. So the control thread owns this for
/// the app's whole life and hands it to the same four functions with or without
/// a session. One state machine, both phases, rather than a second copy of it
/// that would drift.
struct CaptureSlot {
    /// The one-shot `--capture` helper, while a rebind is in progress. Its
    /// `Drop` is the same "no orphans" guarantee the ordinary helper carries,
    /// and the control thread drops it before the session for the same reason
    /// `hook` is first in `Session`'s field order: no key event may arrive
    /// mid-teardown, and neither helper may outlive us.
    helper: Option<HookProcess>,
    /// Where `sotone-hook` lives, or why it could not be found — resolved once
    /// on the control thread *before* startup branches, so a respawn is not a
    /// second chance to get the path wrong and the empty phase has the path
    /// too. Kept as the outcome rather than spent as a fatal: a broken install
    /// with no model still has to reach the wizard, and [`init`] is what
    /// refuses to start a *session* without a helper.
    hook_path: Result<PathBuf, String>,
    /// Handed to each helper reader thread. The control thread's own receiver
    /// is what they feed.
    inputs: Sender<ShellInput>,
}

impl CaptureSlot {
    /// The helper to spawn, or the sentence to show instead of spawning it.
    ///
    /// # Errors
    /// The install is broken: [`helper_path`]'s message, which names the path
    /// it looked at.
    fn hook_path(&self) -> Result<&Path, String> {
        self.hook_path.as_deref().map_err(Clone::clone)
    }
}

/// The wizard's frame, in **logical** pixels, as the design draws it.
///
/// **The floor `tauri.conf.json` puts under the main window (`minWidth` 620,
/// `minHeight` 440) is this frame.** The wizard is the narrowest thing
/// the window is ever asked to hold, so it is the only honest place to put the
/// minimum — and a frame set *below* the floor here would be silently clamped up
/// by Windows rather than refused. Move one and check the other.
const WIZARD_WIDTH: f64 = 620.0;
/// The wizard's frame height. The design's own number.
const WIZARD_HEIGHT: f64 = 449.0;
/// The normal window, as `tauri.conf.json` declares it. Restated here because
/// finishing the wizard has to put it back, and the config is not readable from
/// Rust at runtime.
const MAIN_WIDTH: f64 = 900.0;
/// The normal window's height, for the same reason.
const MAIN_HEIGHT: f64 = 640.0;

/// Where this launch stands with the onboarding wizard.
///
/// A **read-only peek**, deliberately separate from the `Config::load` that
/// `init` does on the control thread: both answers this feeds are needed on the
/// main thread during `setup`, before the event loop has pumped a single
/// message — the first-run window size must land before the frame is ever
/// composited, and `user_armed` is built into the state there. Nothing is
/// written here; a missing file is a fresh install, and a file that cannot be
/// read or parsed is `init`'s error to report with its path, not this
/// function's to guess at (it answers `Yes`, so a broken install shows the fatal
/// view at its normal size rather than a wizard-shaped one).
fn onboarding_state() -> Onboarded {
    let Ok(path) = default_config_path() else {
        return Onboarded::Yes;
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Config::from_toml(&text, &path)
            .map(|config| config.onboarded)
            .unwrap_or(Onboarded::Yes),
        // No file at all is precisely a fresh install; `init` is about to
        // create one saying the same thing.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Onboarded::No,
        Err(_) => Onboarded::Yes,
    }
}

/// Size, centre and lock or unlock the **main** window.
///
/// `LogicalSize`, never `PhysicalSize`: the design's numbers are CSS pixels, and
/// 620 physical pixels on a 125% display is 496 of them (the DPI trap, on
/// record). Tauri converts with the window's own scale factor, which is
/// the only correct source for it.
///
/// **Nothing here can take focus** (invariant 2): `set_resizable` is a style
/// write, `set_size` and `center` are `SetWindowPos` calls with no activation
/// bit between them, and none of the three shows or raises anything. The main
/// window taking focus when the OS launches it is the standard launch's
/// business and is not touched. The overlay window is not reached at all.
///
/// Failures are logged, never fatal: a window that could not be resized is a
/// wizard drawn in a bigger frame, which is survivable — refusing to start over
/// it is not.
fn set_main_frame(app: &AppHandle, width: f64, height: f64, resizable: bool) {
    let Some(window) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    // Resizability first: on Windows it adds or removes the sizing border,
    // which moves the frame around the same client area — so setting it after
    // the size would leave the size subtly wrong.
    if let Err(err) = window.set_resizable(resizable) {
        tracing::warn!(error = %err, "could not change whether the window resizes");
    }
    if let Err(err) = window.set_size(LogicalSize::new(width, height)) {
        tracing::warn!(error = %err, width, height, "could not size the window");
    }
    if let Err(err) = window.center() {
        tracing::warn!(error = %err, "could not centre the window");
    }
}

/// Manage the shell state and start the control thread.
///
/// Returns as soon as the thread is spawned — `setup` must not block the event
/// loop, and whisper alone takes half a second to load.
///
/// # Errors
/// Only if the OS refuses a thread. Every *startup* failure after that point is
/// reported as a fatal status in the window instead, because an app that dies
/// before it can say why is the worst version of this.
pub fn start(app: &AppHandle) -> Result<()> {
    // No overlay style fixup here. It used to run at setup, while the window was
    // still hidden, and tao erased it at the first `show()` — see
    // `restyle_after_show`, which is where it lives now.

    // Two decisions that cannot wait for `init`: the window's
    // first-run size has to be set before the event loop composites it, and the
    // armed flag is built into the state on the next line. Both are read off the
    // same peek, which touches nothing — see [`onboarding_state`].
    let onboarding = onboarding_state();

    let (input_tx, input_rx) = mpsc::channel::<ShellInput>();
    let (overlay_tx, overlay_rx) = mpsc::channel::<OverlayInput>();
    let (tray_tx, tray_rx) = mpsc::channel::<TrayInput>();
    // The run ends — and starts — disarmed while the wizard is in play: "the
    // user turns recording on deliberately", as the design says. `Yes` is
    // every other launch, and behaves exactly as it always has.
    let state = ShellState::new(input_tx.clone(), overlay_tx, tray_tx, onboarding.is_done());
    app.manage(state.clone());

    // Only for the launch that actually *shows* the wizard. `FirstLaunch` is
    // the real app coming up for the first time — it starts disarmed, but it is
    // the ordinary 900×640 window, not the wizard's frame.
    if onboarding == Onboarded::No {
        set_main_frame(app, WIZARD_WIDTH, WIZARD_HEIGHT, false);
    }

    // Before startup rather than inside it: the overlay thread is
    // the only thing that touches that window, and `set_overlay` can be invoked
    // in the empty phase — where there is no session and `init` never returns
    // one — so it must exist for the app's whole life, not the session's.
    spawn_overlay(app.clone(), state.clone(), overlay_rx);

    // Same rule, one step stronger: the tray is built *here*, on the
    // main thread during `setup`, so the icon is in the notification area
    // before the first status event — the empty phase and a fatal startup both
    // have a tray, and both say so on it.
    let tray = tray::spawn(app.clone(), state.clone(), tray_rx);
    state.set_tray_alive(tray);

    let app = app.clone();
    thread::Builder::new()
        .name("sotone-control".to_owned())
        .spawn(move || run(&app, &state, input_tx, &input_rx))
        .context("could not start the Sotone control thread")?;
    Ok(())
}

/// Start everything, then become the consumer loop.
fn run(
    app: &AppHandle,
    state: &ShellState,
    input_tx: Sender<ShellInput>,
    input_rx: &Receiver<ShellInput>,
) {
    // Resolved here rather than inside `init`, and this one move is what makes
    // a rebind work in the wizard: startup stops before the helper when there
    // is no model, so the empty phase would otherwise have no path to spawn a
    // capture with. A broken install is still refused by `init`, with the same
    // message and at the same point in startup.
    let mut capture = CaptureSlot {
        helper: None,
        hook_path: helper_path().map_err(|err| format!("{err:#}")),
        inputs: input_tx.clone(),
    };

    // The empty phase is `None` rather than a return: the window stays up on
    // the wizard, and the wizard's key step is a surface this thread still
    // owns. Everything else the loop below does asks for a session first and
    // finds none, which is what a command sent into a dead channel used to get.
    let mut session = match init(app, state, input_tx, capture.hook_path()) {
        Ok(session) => session,
        Err(err) => {
            tracing::error!(error = ?err, "startup failed");
            state.set_status(app, StatusEvent::fatal(&err));
            // Returning rather than parking: the window stays up either way —
            // the fatal status is in the snapshot, so even a webview that loads
            // later renders the message — and a parked thread would just be a
            // leak with the same effect.
            return;
        }
    };

    // Both of these describe a session: whether it is armed, and that it is not
    // recording. The empty phase has neither to say, and the status it already
    // published is the whole truth about it.
    if session.is_some() {
        state.emit_armed(app);
        state.set_recording(app, RecordingEvent::IDLE);
    }

    let mut machine = Machine::default();
    let mut engine_dead = false;

    for input in input_rx {
        // Checkpoint 2 of the input trace: what actually reached the control
        // thread, and what it decided. With checkpoint 1 (the hotkey consumer,
        // in `init`) this brackets every piece of code Sotone owns between the
        // hook and the engine, so one repro press says which side lost the key.
        tracing::debug!(input = ?input, "control input");
        match input {
            // Every branch but the settings one needs the session, and the
            // empty phase has none: no hook to send a key, no worker to hand a
            // draft to. Nearly all of them arrive from surfaces that phase
            // never offers, so what happens without a session is the trace
            // above and nothing more.
            //
            // The one that does arrive is the wizard's step 6: `project_create`
            // sends `Draft(DraftInput::SetProject)`, and the `Draft` arm below
            // drops it. Harmless, because in that phase the config write *is*
            // the whole of a create — there is no worker yet holding a stale
            // active project to correct, and the session the wizard's finish
            // restarts into reads `active_project` back out of the file
            // (`init`, where the worker's copy is seeded).
            ShellInput::Key(key) => {
                let Some(session) = session.as_ref() else {
                    continue;
                };
                let action = machine.apply(key, state.capture_live(), Instant::now());
                tracing::debug!(action = ?action, "control action");
                perform(app, state, session, action);
            }
            // Not a notice any more: the hook being gone is permanent
            // until something puts it back, and a permanent state announced
            // through a rolling list stops being on screen the moment the list
            // rolls. The pane takeover says it until a respawn clears it.
            ShellInput::HookLost(reason) => {
                state.set_condition(
                    app,
                    Condition::HotkeyDead,
                    format!("the input hook could not be kept: {reason}"),
                );
            }
            ShellInput::Draft(request) => {
                let Some(session) = session.as_ref() else {
                    continue;
                };
                perform_draft(app, state, session, machine.is_recording(), request);
            }
            // The one request that is answered in both phases, which is why it
            // takes the capture slot and an *optional* session rather than
            // being written twice.
            ShellInput::Settings(request) => {
                perform_settings(
                    app,
                    state,
                    &mut capture,
                    session.as_mut(),
                    machine.is_recording(),
                    &mut engine_dead,
                    request,
                );
            }
            ShellInput::Line(request) => {
                // Straight through to the worker: it owns the handle, and this
                // thread must not open a second one. The frontend already
                // hides the editing controls while a recording is live, and
                // the worker applies these between utterances either way.
                let Some(session) = session.as_ref() else {
                    continue;
                };
                send_draft(
                    session,
                    match request {
                        LineInput::Edit { id, text } => SessionCommand::EditLine { id, text },
                        LineInput::Move { id, after } => SessionCommand::MoveLine { id, after },
                        // The one renamed field on this hop: the window says
                        // *first of the batch*, which is a fact about the
                        // confirm; the worker reads *divide*, which is what it
                        // does about it.
                        LineInput::MoveTo { id, target, first } => SessionCommand::MoveLineTo {
                            id,
                            target_id: target,
                            divide: first,
                        },
                        LineInput::SetDeleted { id, deleted } => {
                            SessionCommand::SetDeleted { id, deleted }
                        }
                        LineInput::Retranscribe(id) => SessionCommand::Retranscribe { id },
                    },
                );
            }
            ShellInput::Quit => break,
        }

        // Cheap, and the alternative is a session that looks alive while the
        // microphone has been gone for ten minutes. Nothing to poll without
        // one: the empty phase never opened a device.
        let Some(session) = session.as_mut() else {
            continue;
        };
        if !engine_dead {
            if let EngineStatus::Dead { reason } = session.engine.status() {
                engine_dead = true;
                session.worker.report_engine_dead(reason.clone());
                // The one failure allowed to disarm Sotone by itself, which is
                // the design's rule: with no input device it cannot record,
                // so the indicator has to go `--faint REC OFF` rather than sit
                // there claiming to be armed. Whatever had already been
                // captured is on its way through the worker — the utterance
                // ends when the device does, so the partial line is still
                // transcribed and saved.
                state.set_condition(
                    app,
                    Condition::NoDevice,
                    format!(
                        "the microphone stopped ({reason}). Anything already captured was still \
                         transcribed and saved."
                    ),
                );
                state.disarm(app);
            }
        }
    }

    // Spelled out, and in this order, now that the capture helper lives outside
    // the session: the one-shot goes first for the same reason the hook is
    // first inside `Session` — no key event may arrive mid-teardown, and
    // neither helper may outlive this thread.
    drop(capture);
    drop(session);
}

/// Carry out one decision. The only place the engine and the cue player are
/// touched.
fn perform(app: &AppHandle, state: &ShellState, session: &Session, action: Action) {
    match action {
        Action::None => {}
        Action::Start { source } => {
            session.engine.begin_utterance();
            // The begin cue is the only confirmation the user gets while they
            // are looking at something else — unless they turned cues off, in
            // which case the player is still open (the output stream is
            // persistent by design) and simply says nothing. The flag is an
            // atomic, not a config read: this is the recording path.
            if let Some(player) = &session.cues {
                if state.cues_enabled() {
                    player.play(Cue::Begin);
                }
            }
            state.set_recording(
                app,
                RecordingEvent {
                    live: true,
                    source: Some(source.as_str()),
                },
            );
        }
        Action::Stop { at, held, .. } => {
            session.engine.end_utterance();
            let _ = session.release_tx.send(ReleaseInfo {
                released_at: at,
                held,
            });
            // No stop cue: `Saved` lands a moment later and is the real one.
            state.set_recording(app, RecordingEvent::IDLE);
        }
        // "ignored: …" is the hotkey machine refusing a press, which is a
        // refusal like any other — `Warn`, so it reaches the footer
        // rather than a list nobody is looking at while they are mid-test.
        Action::Ignored { reason } => {
            state.notice(app, NoticeLevel::Warn, reason.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Drafts
// ---------------------------------------------------------------------------

/// Carry out one draft request.
///
/// Two rules shape this function. The worker owns the active [`Draft`] handle,
/// so anything touching it is sent there rather than done here; and a draft the
/// worker is *not* holding has no handle at all, so discarding it is a plain
/// rename this thread can do itself.
fn perform_draft(
    app: &AppHandle,
    state: &ShellState,
    session: &Session,
    recording: bool,
    request: DraftInput,
) {
    match request {
        // The project comes from the config *now*, not from startup: a project
        // created a moment ago has to be the one this note belongs to.
        DraftInput::New => match session
            .store
            .create_draft(state.active_project_name().as_deref())
        {
            Ok(draft) => send_draft(session, SessionCommand::SetDraft(Box::new(draft))),
            Err(err) => state.notice(
                app,
                NoticeLevel::Error,
                format!("could not start a new note: {err}"),
            ),
        },

        DraftInput::Open(id) => {
            match session
                .store
                .draft_path(&id)
                .and_then(|path| Draft::open(&path))
            {
                Ok(loaded) => {
                    if loaded.skipped_lines > 0 {
                        state.notice(
                            app,
                            NoticeLevel::Warn,
                            format!(
                                "{} line(s) in that draft could not be read and were left on \
                                 disk untouched",
                                loaded.skipped_lines
                            ),
                        );
                    }
                    send_draft(session, SessionCommand::SetDraft(Box::new(loaded.draft)));
                }
                // An unreadable draft is a notice, not a crash: the rest of the
                // list still works, and the message names the file.
                Err(err) => state.notice(
                    app,
                    NoticeLevel::Error,
                    format!("could not open that draft: {err}"),
                ),
            }
        }

        // The discard precedent: a save that lands mid-utterance would render a
        // note the user is still adding to, and the line they are speaking
        // right now would be missing from the file they just saved.
        DraftInput::Save { overwrite } => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then save",
                );
                return;
            }

            let active = state.active_draft().unwrap_or_default();
            // Notes exist only within projects. The draft's own
            // project wins when it still exists; otherwise the active one, which
            // is the adoption path. With neither — or with a project that has no
            // folder chosen — nothing is written and the window asks.
            let project = state
                .governing_project(state.draft_project(&active).as_deref())
                .filter(|project| !project.notes_dir.as_os_str().is_empty());
            let Some(project) = project else {
                tracing::info!("a save was asked for with no project to save into");
                emit(app, EVENT_SAVE, &SaveEvent::of("no_project"));
                return;
            };

            // The filename is decided the first time this draft is saved, and
            // then held, so an Overwrite lands on the file the conflict dialog
            // named. It is ignored once the draft is bound (Notepad
            // semantics).
            let fallback_path = state.fallback_save_path(&active, &project);
            // Expanded here, at the boundary that has the clock: the header is
            // file *content*, so it is text mode — colons in the time are fine
            // and nothing is sanitized.
            let header = project
                .header_template
                .as_deref()
                .map(|template| template::expand_text_now(template, &project.name));

            send_draft(
                session,
                SessionCommand::Save {
                    fallback_path,
                    notes_root: Some(project.notes_dir.clone()),
                    adopt_project: Some(project.name.clone()),
                    header,
                    dividers: project.session_dividers,
                    overwrite,
                },
            );
        }

        // Same refusal as a single save, for the same reason, times N: a batch
        // that lands mid-utterance renders notes the user is still adding to.
        DraftInput::SaveAll => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then save all",
                );
                return;
            }

            // **Every** project, not the active one: "Save all"
            // means every dirty note in the store, and a note saves under the
            // rules of the project it belongs to. So the whole configuration is
            // read *here* and travels in the command — the worker never reads
            // config, exactly as `RenameActive` and `SetDraftProject` carry
            // their notes roots rather than looking them up.
            //
            // A project with no folder chosen is left out: there is nowhere for
            // its notes to land, so the batch counts them as skipped rather
            // than writing into an empty path.
            let projects = state.save_contexts();
            if projects.is_empty() {
                // Nothing is configured that could take a note. Same answer as
                // a single save with no project: the pick-or-create popup,
                // rather than a batch that silently does nothing.
                tracing::info!("a save-all was asked for with no project to save into");
                emit(app, EVENT_SAVE, &SaveEvent::of("no_project"));
                return;
            }

            send_draft(session, SessionCommand::SaveAll { projects });
        }

        DraftInput::SetProject(name) => {
            send_draft(session, SessionCommand::SetProject { name });
        }

        // Same routing split as a discard, for the same reason (the
        // one-handle rule): the worker owns the *active* draft's only handle,
        // so it renames that one; every other draft is opened, renamed and
        // dropped right here, on the control thread, which never holds a
        // handle for longer than the call.
        DraftInput::Rename { id, name } => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then rename",
                );
                return;
            }

            // The draft's **own** project, never the active one: a relative
            // binding means "inside my project's notes folder", and resolving
            // it against somebody else's folder would rename a file in the
            // wrong place. With no project, only an absolute binding resolves —
            // and a relative one is refused by the store, which is correct.
            let notes_root = state
                .draft_project(&id)
                .and_then(|name| state.with_config(|config| config.project(&name).cloned()))
                .flatten()
                .map(|project| project.notes_dir);

            if state.active_draft().as_deref() == Some(id.as_str()) {
                send_draft(session, SessionCommand::RenameActive { name, notes_root });
                return;
            }

            match session
                .store
                .draft_path(&id)
                .and_then(|path| Draft::open(&path))
            {
                Ok(loaded) => {
                    // Explicit, not incidental: this handle exists for one
                    // rename and must be gone before anything else can open
                    // the same directory.
                    let mut draft = loaded.draft;
                    let outcome = draft.rename_note(&name, notes_root.as_deref());
                    drop(draft);
                    match outcome {
                        Ok(report) => {
                            tracing::info!(
                                draft = %id,
                                from = %report.from.display(),
                                to = %report.to.display(),
                                "renamed an outstanding note"
                            );
                            refresh_drafts(app, state, &session.store);
                        }
                        // Every refusal here is an ordinary answer — the name
                        // is taken, the note has never been saved — and none of
                        // them wrote anything.
                        Err(err) => state.notice(app, NoticeLevel::Warn, format!("{err}")),
                    }
                }
                Err(err) => state.notice(
                    app,
                    NoticeLevel::Error,
                    format!("could not open that draft: {err}"),
                ),
            }
        }

        // Moving a note between projects. The routing is the rename's
        // exactly — the worker's live handle for the active draft, an
        // open→mutate→drop here for any other — and the refusal is the second
        // layer under the command's own (the standing pattern).
        DraftInput::SetDraftProject {
            id,
            project,
            keep_both,
        } => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then move the note",
                );
                return;
            }

            // A target the config does not have would file the note into the
            // "not in your projects" group, which is not what the drop said.
            // Leaving *every* project (`None`) is a real destination and is
            // never refused.
            if let Some(name) = project.as_deref() {
                if state.with_config(|config| config.project(name).is_some()) != Some(true) {
                    state.notice(
                        app,
                        NoticeLevel::Warn,
                        "there is no project by that name any more",
                    );
                    return;
                }
            }

            // Two folders, and neither of them is the *active* project's (the
            // `notes_root` lesson): the one the binding is currently
            // relative to is the draft's **own** old project's, and the one the
            // file is moving into is the **drop target's**. An old project that
            // is no longer in the config gives `None`, and the store recreates
            // such a note in the target folder — moving a note out of the "not
            // in your projects" group is the repair, not an error.
            let notes_root_of = |name: &str| {
                state
                    .with_config(|config| config.project(name).cloned())
                    .flatten()
                    .map(|project| project.notes_dir)
                    .filter(|dir| !dir.as_os_str().is_empty())
            };
            let old_root = state
                .draft_project(&id)
                .and_then(|name| notes_root_of(&name));
            let target = project
                .as_deref()
                .and_then(|name| state.with_config(|config| config.project(name).cloned()))
                .flatten()
                .filter(|project| !project.notes_dir.as_os_str().is_empty());
            let new_root = target.as_ref().map(|project| project.notes_dir.clone());
            // Only read by the recreate branch, and expanded here for the same
            // reason a save's is: this is the boundary that has the clock, and
            // a header is file *content*, so nothing about it is sanitized.
            let header = target.as_ref().and_then(|project| {
                project
                    .header_template
                    .as_deref()
                    .map(|template| template::expand_text_now(template, &project.name))
            });
            // Only consulted by the recreate branch, which is only reachable
            // with a target folder, so "no target" falls back to the default
            // rather than inventing an answer.
            let dividers = target
                .as_ref()
                .map(|project| SessionDividers::when(project.session_dividers))
                .unwrap_or_default();
            let clash = if keep_both {
                ClashChoice::KeepBoth
            } else {
                ClashChoice::Ask
            };

            if state.active_draft().as_deref() == Some(id.as_str()) {
                send_draft(
                    session,
                    SessionCommand::SetDraftProject {
                        project,
                        old_root,
                        new_root,
                        clash,
                        header,
                        dividers,
                    },
                );
                return;
            }

            match session
                .store
                .draft_path(&id)
                .and_then(|path| Draft::open(&path))
            {
                Ok(loaded) => {
                    // Explicit, not incidental: this handle exists for one move
                    // and must be gone before anything else opens the same
                    // directory.
                    let mut draft = loaded.draft;
                    let outcome = draft.move_to_project(MoveOptions {
                        project: project.as_deref(),
                        old_root: old_root.as_deref(),
                        new_root: new_root.as_deref(),
                        clash,
                        header: header.as_deref(),
                        dividers,
                    });
                    drop(draft);
                    // The same reporting as the worker's route, through the
                    // same function: the two must not come to disagree about
                    // what a drop said.
                    report_note_move(
                        app,
                        state,
                        &session.store,
                        &id,
                        project.as_deref(),
                        outcome.map_err(|err| format!("{err}")),
                    );
                }
                Err(err) => state.notice(
                    app,
                    NoticeLevel::Error,
                    format!("could not open that draft: {err}"),
                ),
            }
        }

        // Step three of a project rename. Refused while recording
        // like every other config-adjacent mutation — the command that sends
        // this already refused, and this is the second layer (the standing
        // pattern).
        DraftInput::ProjectRenamed { from, to } => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then rename a project",
                );
                return;
            }
            send_draft(session, SessionCommand::RenameProject { from, to });
        }

        DraftInput::Discard(id) => {
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — stop it first, then discard",
                );
                return;
            }

            if state.active_draft().as_deref() == Some(id.as_str()) {
                // The worker holds the handle. It drops it and then renames, in
                // that order, and reports back as `DraftDiscarded`.
                send_draft(session, SessionCommand::DiscardActive);
                return;
            }

            match session.store.discard(&id) {
                Ok(path) => {
                    tracing::info!(trashed = %path.display(), "discarded a draft");
                    // One wording for both discard paths: this one used to
                    // name the `.trash` path and the worker's one used to name
                    // the retention, so the same act read as two different
                    // things depending on whether the draft was the active one.
                    state.notice(app, NoticeLevel::Debug, DISCARDED);
                    refresh_drafts(app, state, &session.store);
                }
                Err(err) => state.notice(
                    app,
                    NoticeLevel::Error,
                    format!("could not discard that draft: {err}"),
                ),
            }
        }
    }
}

/// Hand a command to the worker. A dead worker means the session is over and
/// the fatal status already said so.
fn send_draft(session: &Session, command: SessionCommand) {
    let _ = session.draft_tx.send(command);
}

/// What one drop is answered with — the same function for both routes.
///
/// The active draft's move happens on the worker and comes back as
/// [`SessionEvent::NoteMoved`]; every other draft's happens on the control
/// thread. Both land here, so the two cannot come to disagree about what a drop
/// said, in the way the two discard paths once did.
///
/// The routing per outcome, and why:
///
/// * **Moved / Recreated / Retagged** — the tree is the answer: the row
///   is in its new group and its label is the new file, which says it better
///   than a sentence. The sentence goes to the debug log, and the file it
///   *names* is the one on disk, never the one that was asked for.
/// * **Clash** — a question, so it goes to the window as
///   [`EVENT_NOTE_CLASH`] and nothing else happens. Nothing was written, so
///   there is nothing to re-list and nothing to apologize for.
/// * **A failure** — `warn`, in the footer's message slot, and the note is
///   still exactly where it was: every failure inside `move_to_project` happens
///   before the original is touched.
fn report_note_move(
    app: &AppHandle,
    state: &ShellState,
    store: &DraftStore,
    id: &str,
    project: Option<&str>,
    outcome: Result<NoteMove, String>,
) {
    let into = project.unwrap_or("no project");
    match outcome {
        Ok(NoteMove::Clash { at, free }) => {
            let name = at
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            tracing::info!(
                draft = %id,
                occupied = %at.display(),
                "a drop found that name taken — asking"
            );
            emit(
                app,
                EVENT_NOTE_CLASH,
                &NoteClashEvent {
                    id: id.to_owned(),
                    project: project.map(str::to_owned),
                    name,
                    path: at.display().to_string(),
                    suggestion: free.map(|path| {
                        path.file_name().map_or_else(
                            || path.display().to_string(),
                            |name| name.to_string_lossy().into_owned(),
                        )
                    }),
                },
            );
        }
        Ok(outcome) => {
            let said = match &outcome {
                NoteMove::Moved { to, copied, .. } => {
                    format!(
                        "moved that note into \"{into}\" — it is now {}{}",
                        to.display(),
                        if *copied {
                            " (copied across volumes)"
                        } else {
                            ""
                        }
                    )
                }
                NoteMove::Recreated { to } => format!(
                    "that note's file was gone, so \"{into}\" got a fresh one: {}",
                    to.display()
                ),
                NoteMove::Retagged => format!("filed that note under \"{into}\""),
                NoteMove::Unchanged => format!("that note is already in \"{into}\""),
                // Answered above; the compiler is what keeps that true.
                NoteMove::Clash { .. } => String::new(),
            };
            tracing::info!(draft = %id, project = into, outcome = ?outcome, "a note was dropped into a project");
            state.notice(app, NoticeLevel::Debug, said);
            refresh_drafts(app, state, store);
        }
        Err(message) => state.notice(app, NoticeLevel::Warn, message),
    }
}

/// Re-read the drafts root and emit the list.
///
/// Deliberately a full re-scan rather than a patched cache: the list is small,
/// the disk is the truth, and every cache-and-patch bug in a list like this
/// ends with the user seeing a draft that is not there. Called from the control
/// thread, the drain thread, and [`draft_create_detached`], which
/// is the one command that creates a draft itself and so is the one command
/// that has to re-list. Never from a callback, and never from a *synchronous*
/// command: this is a directory scan, and it may only run where blocking is
/// allowed (invariant 5).
/// What a discarded draft is told, from both paths.
///
/// One constant because there are two call sites — the store rename this thread
/// does for an inactive draft, and the worker's `DraftDiscarded` for the active
/// one — and they once said different things about the same act.
const DISCARDED: &str = "discarded that draft — it is in .trash for the next 30 days";

fn refresh_drafts(app: &AppHandle, state: &ShellState, store: &DraftStore) {
    match store.list_drafts() {
        Ok(scan) => {
            // Resolved against the configuration as it stands *now*:
            // a relative binding means "inside my project's notes folder", and
            // that folder can have moved since the last listing.
            let event = drafts_event(
                scan,
                state.active_draft(),
                state.default_save_dir(),
                &state.project_dirs(),
            );
            state.set_drafts(app, event);
            state
                .inner
                .drafts_unreadable
                .store(false, Ordering::Relaxed);
        }
        // Once per episode, not once per call: this function runs after every
        // appended line, every edit and every save, so an unreadable folder used
        // to emit the identical sentence dozens of times a minute and push
        // everything else out of the list (the notice flood). The
        // latch is released by the first scan that succeeds.
        Err(err) => {
            if !state.inner.drafts_unreadable.swap(true, Ordering::Relaxed) {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    format!("could not read the drafts folder: {err}"),
                );
            } else {
                tracing::debug!(error = %err, "the drafts folder is still unreadable");
            }
        }
    }
}

/// Core records → the transcript DTO.
///
/// The formatting decisions live here rather than in the window for the same
/// reason [`LineEvent`] formats its time here: one place decides what a line
/// looks like on screen.
fn lines_event(draft_id: String, lines: &[LineRecord]) -> LinesEvent {
    LinesEvent {
        draft_id,
        lines: lines.iter().map(LineDto::stored).collect(),
    }
}

/// Core scan → DTOs.
///
/// `project_dirs` is every project's notes folder by name, which is what turns
/// a draft's *relative* binding back into the absolute path the window shows. A
/// relative binding whose project is no longer in the config resolves to
/// `None`: there is no true path to display, and displaying a wrong one is how
/// a user opens the wrong file.
fn drafts_event(
    scan: DraftScan,
    active_id: Option<String>,
    default_save_dir: String,
    project_dirs: &HashMap<String, PathBuf>,
) -> DraftsEvent {
    DraftsEvent {
        drafts: scan
            .drafts
            .into_iter()
            .map(|draft| {
                let root = draft
                    .project
                    .as_deref()
                    .and_then(|name| project_dirs.get(name));
                let saved_path = draft
                    .saved_path
                    .as_deref()
                    .and_then(|stored| resolve_binding(stored, root.map(PathBuf::as_path)))
                    .map(|path| path.display().to_string());
                DraftDto {
                    id: draft.id,
                    created_at: draft.created_at.to_rfc3339(),
                    project: draft.project,
                    dirty: draft.dirty,
                    line_count: draft.line_count,
                    saved_path,
                }
            })
            .collect(),
        rejected: scan
            .rejected
            .into_iter()
            .map(|(path, err)| RejectedDraftDto {
                path: path.display().to_string(),
                reason: err.to_string(),
            })
            .collect(),
        active_id,
        default_save_dir,
    }
}

// ---------------------------------------------------------------------------
// Settings: hotkey capture and the helper lifecycle
//
// Hotkey capture is a sequence of helper lifecycle steps, and the control
// thread owns the helper — so it owns this. Nothing here waits: a capture is
// started, and the press arrives later as one more `ShellInput` from the
// capture helper's reader thread, so key events, commands and drain events keep
// being handled throughout (invariant 5).
//
// The rule that shapes every branch: **capture can never leave the app
// hookless.** Every way out of a capture — a press, a cancel, a crashed helper,
// a hook that died, a token that will not parse, a change the config layer
// refuses — ends by putting an ordinary helper back.
// ---------------------------------------------------------------------------

/// Carry out one settings request.
///
/// `session` is `None` in the empty phase, where there is no engine, no worker
/// and no hook — only a capture, which is why this takes an `Option` rather
/// than being written twice. The capture branches read as they always did,
/// minus a helper there was never anything to stop or put back; every other
/// branch asks for a session and says what it does without one.
///
/// `engine_dead` is the control loop's "already reported" latch, borrowed
/// because a device change is the one thing that can both *cause* it and
/// *clear* it.
fn perform_settings(
    app: &AppHandle,
    state: &ShellState,
    capture: &mut CaptureSlot,
    session: Option<&mut Session>,
    recording: bool,
    engine_dead: &mut bool,
    request: SettingsInput,
) {
    match request {
        SettingsInput::CaptureStart(mode) => {
            let Some(mode) = HotkeyMode::parse(&mode) else {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    format!("{mode:?} is not a recording mode"),
                );
                return;
            };
            start_capture(app, state, capture, session, recording, mode);
        }

        SettingsInput::CaptureCancel => {
            if capture.helper.take().is_none() {
                return;
            }
            state.set_capture(None);
            // Straight back to the bindings the config already had: cancelling
            // changed nothing, so nothing needs writing.
            respawn_helper(app, state, capture, session);
            state.refresh_settings(app);
        }

        SettingsInput::Captured { token } => {
            let Some(mode) = state.capturing() else {
                // A press that crossed a cancel. The helper is already gone and
                // the ordinary one is already back; the user's cancel wins.
                tracing::debug!(token, "a captured key arrived after the capture ended");
                return;
            };
            capture.helper = None;
            state.set_capture(None);
            apply_capture(app, state, capture, session, mode, &token);
        }

        SettingsInput::CaptureEnded { reason } => {
            if capture.helper.take().is_none() {
                return;
            }
            state.set_capture(None);
            state.notice(
                app,
                NoticeLevel::Warn,
                format!("the key capture stopped before you pressed anything ({reason})"),
            );
            respawn_helper(app, state, capture, session);
            state.refresh_settings(app);
        }

        SettingsInput::Rehook => {
            respawn_helper(app, state, capture, session);
            state.refresh_settings(app);
        }

        SettingsInput::SetCues(on) => {
            // The control thread's half of the switch is the atomic, and it is
            // set either way; the worker's half only exists with a session, and
            // without one the config the command has already written is what
            // the first session starts from.
            state.set_cues_enabled(on);
            if let Some(session) = session {
                send_draft(session, SessionCommand::SetCues(on));
            }
        }

        // The device change itself. The command already wrote the
        // config and already refused this while a recording was live; what is
        // left is the part only this thread can do.
        SettingsInput::SetMic(device) => {
            // No engine to reconnect to in the empty phase. The command has
            // already written the choice, so the device the file now names is
            // the one the first session opens.
            let Some(session) = session else {
                return;
            };
            // Belt and braces: a recording that went live between the command's
            // check and this message would otherwise lose its tail to the
            // teardown. The engine keeps capturing on the device it has.
            //
            // The config has already been written by then, so the message says
            // what is actually true rather than "nothing happened": the choice
            // is saved and will be opened at the next launch, and the way to
            // have it now is to ask again once the recording stops.
            if recording {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    "not while a recording is running — that microphone is saved, but Sotone is \
                     still using the current one; choose it again once you stop",
                );
                return;
            }
            let outcome = session.engine.reconnect(device.as_deref());
            let (level, message) = mic_notice(&outcome);
            // Whatever happened, the readout names the device that is really
            // open — so the About tab, the footer and the overlay stay truthful
            // even when the config now names something else.
            let running = ready_device(&outcome);
            state.update_ready(app, |ready| ready.device = running);
            state.notice(app, level, message.clone());
            // Setting the latch either way also re-arms it, so a device that
            // dies after a successful change is still reported.
            *engine_dead = matches!(outcome, Reconnect::Lost { .. });
            // The condition, which is the part that stays on screen. A
            // `Reverted` is *not* one: the previous device is open and Sotone can
            // still hear, so it is a refusal — one sentence in the footer — and
            // not a state. Only `Switched` clears: it is the only outcome that
            // proves something is capturing again.
            if *engine_dead {
                state.set_condition(app, Condition::NoDevice, message);
                state.disarm(app);
            } else if matches!(outcome, Reconnect::Switched { .. }) {
                state.clear_condition(app, Condition::NoDevice);
            }
            state.refresh_settings(app);
        }

        // Hand-over to the worker. Loaded and warmed by the loading thread —
        // nothing here blocks, and nothing here can fail.
        SettingsInput::SetTranscriber(transcriber) => {
            // Both of these are hand-overs to the worker, and the empty phase
            // has no worker: nothing there can have loaded a model or chosen a
            // language, so nothing there sends them.
            if let Some(session) = session {
                send_draft(session, SessionCommand::SetTranscriber(transcriber));
            }
        }

        SettingsInput::SetLanguage(language) => {
            if let Some(session) = session {
                send_draft(session, SessionCommand::SetLanguage(language));
            }
        }
    }
}

/// What one reconnect outcome is worth saying, and how loudly.
///
/// A free function so the three sentences can be read — and tested — without an
/// audio device. The rule they follow: **always name what is capturing now**. A
/// user who is told only that their choice failed does not know whether the app
/// can still hear them.
///
/// The levels are the notice routing: a successful switch is a confirmation the
/// About tab and the footer already show, so it is `Debug`; a revert is a
/// refusal the user has to read, so it is `Warn`; a loss is `Error` *and* the
/// sentence [`Condition::NoDevice`] carries — the same words in both places,
/// because there is only one true thing to say.
fn mic_notice(outcome: &Reconnect) -> (NoticeLevel, String) {
    match outcome {
        Reconnect::Switched { device } => (
            NoticeLevel::Debug,
            format!("now listening to {device} — no restart needed"),
        ),
        Reconnect::Reverted { device, error } => (
            NoticeLevel::Warn,
            format!("that microphone could not be opened ({error}) — still listening to {device}"),
        ),
        // `Debug`, not `Error` — and this is the whole point of the conditions
        // channel. The strip is the surface for this one: it says the same
        // sentence and it *stays*. Emitting it into the footer slot as well
        // would report one failure twice, which is exactly the "at most one
        // primary thing on screen" rule the design opens with.
        Reconnect::Lost {
            error,
            revert_error,
        } => (
            NoticeLevel::Debug,
            format!(
                "that microphone could not be opened ({error}), and neither could the previous one \
                 ({revert_error}) — Sotone cannot hear anything until a working device is chosen"
            ),
        ),
    }
}

/// The device the readout should now name.
///
/// A lost microphone is a *state*, not a missing value: the row keeps saying
/// something, and what it says is that there is nothing open.
fn ready_device(outcome: &Reconnect) -> String {
    match outcome {
        Reconnect::Switched { device } | Reconnect::Reverted { device, .. } => device.clone(),
        Reconnect::Lost { .. } => NO_MICROPHONE.to_owned(),
    }
}

/// What [`ReadyInfo::device`] says when nothing is open.
const NO_MICROPHONE: &str = "no microphone";

/// Shut the ordinary helper down and start a `--capture` one.
///
/// With no session there is no ordinary helper to shut down — the empty phase
/// never started one — so the first half is a no-op and the rest is unchanged.
/// That is the whole of the difference between a rebind in Settings and the
/// same rebind on the wizard's key step.
fn start_capture(
    app: &AppHandle,
    state: &ShellState,
    capture: &mut CaptureSlot,
    mut session: Option<&mut Session>,
    recording: bool,
    mode: HotkeyMode,
) {
    // Rebinding mid-utterance would take the key that is going to end the
    // recording out from under the user.
    if recording {
        state.notice(
            app,
            NoticeLevel::Warn,
            "not while a recording is running — stop it first, then rebind",
        );
        return;
    }
    if capture.helper.is_some() {
        state.notice(
            app,
            NoticeLevel::Warn,
            "Sotone is already waiting for a key — press one, or cancel",
        );
        return;
    }

    // There is one hook, so the helper watching the bindings has to go before
    // the capture helper arrives. Push-to-talk and toggle are dead from here
    // until the capture ends, deliberately, and the panel says so.
    stop_helper(session.as_deref_mut());

    // The install's own failure, carried since startup rather than asked again
    // here: with a session in hand this cannot be an error at all, because
    // `init` refused to build one without a helper.
    let outcome = capture
        .hook_path()
        .and_then(|path| spawn_capture_helper(path).map_err(|err| format!("{err:#}")));
    match outcome {
        Ok(child) => {
            capture.helper = Some(read_capture(&capture.inputs, child));
            state.set_capture(Some(mode));
            state.refresh_settings(app);
        }
        Err(err) => {
            state.notice(
                app,
                NoticeLevel::Error,
                format!("could not start the key capture: {err}"),
            );
            // Hookless is not a state this app is ever left in — and the empty
            // phase was hookless before this, which the window already says.
            respawn_helper(app, state, capture, session);
            state.refresh_settings(app);
        }
    }
}

/// Write a captured binding into the configuration and make it live.
///
/// "Live" is the session's half, and it is the half the empty phase does not
/// have: there the file *is* the whole of it, and the launch that finishes the
/// wizard reads the keys back out of it (see [`rebind`]).
fn apply_capture(
    app: &AppHandle,
    state: &ShellState,
    capture: &CaptureSlot,
    session: Option<&mut Session>,
    mode: HotkeyMode,
    token: &str,
) {
    // Parsed rather than trusted: the helper is our own process, but this
    // string is about to become a `hotkey` value, and a token the next launch
    // cannot read would be an app that will not start.
    let binding = match token.parse::<Binding>() {
        Ok(binding) => binding,
        Err(err) => {
            state.notice(app, NoticeLevel::Warn, format!("{err}"));
            respawn_helper(app, state, capture, session);
            state.refresh_settings(app);
            return;
        }
    };
    // Rendered from the parsed binding, so what is written is the canonical
    // spelling and not whatever came off the wire.
    let token = binding.to_string();

    let outcome = state.edit_config(|config| Ok(rebind(config, mode, &token)));

    match outcome {
        Ok(None) => {
            // Ordering, deliberately: the configuration file is written first
            // and the helper respawned second. A crash in between leaves a file
            // that already says what the user chose, so the next launch is
            // right — where the other order would leave a running helper
            // watching a key the file has never heard of.
            respawn_helper(app, state, capture, session);
            // The keycap in the row and the title bar's key hint both now read
            // the new binding, so the sentence is log-only.
            state.notice(
                app,
                NoticeLevel::Debug,
                format!("{} is now {}", mode.describe(), binding_label(&token)),
            );
        }
        Ok(Some(problem)) => {
            state.notice(app, NoticeLevel::Warn, problem);
            respawn_helper(app, state, capture, session);
        }
        Err(err) => {
            state.notice(
                app,
                NoticeLevel::Error,
                format!("could not save the new hotkey: {err}"),
            );
            respawn_helper(app, state, capture, session);
        }
    }
    state.refresh_settings(app);
}

/// Write one captured binding into a configuration, or say why it cannot be.
///
/// Split out of [`apply_capture`] so the rule can be read — and tested —
/// without a helper process or an `AppHandle`: this is the whole of what a
/// rebind changes, and the file it changes is what the next launch resolves its
/// bindings from. That matters most in the empty phase, where finishing the
/// wizard restarts the app and this write is the only trace the rebind leaves.
fn rebind(config: &mut Config, mode: HotkeyMode, token: &str) -> Option<String> {
    let (ptt, toggle) = match mode {
        HotkeyMode::Ptt => (token, config.toggle_hotkey.as_str()),
        HotkeyMode::Toggle => (config.hotkey.as_str(), token),
    };
    // The same rule `Config::load` applies: two enabled modes may not share a
    // binding. Asked *before* the write, because a settings screen that can
    // save a config the next launch refuses is a trap with no way out.
    if let Some(problem) =
        recording_mode_problem(ptt, toggle, config.ptt_enabled, config.toggle_enabled)
    {
        return Some(problem);
    }
    match mode {
        HotkeyMode::Ptt => config.hotkey = token.to_owned(),
        HotkeyMode::Toggle => config.toggle_hotkey = token.to_owned(),
    }
    None
}

/// Stop whatever helper is running, so there is exactly one hook at a time.
///
/// Nothing to stop without a session: the empty phase never started one.
fn stop_helper(session: Option<&mut Session>) {
    let Some(session) = session else {
        return;
    };
    if let Some(hook) = session.hook.take() {
        // The existing discipline: drop its stdin, wait, then kill. Its
        // supervisor thread sees `stopping` and does not restart it.
        hook.shutdown();
    }
}

/// Put an ordinary helper back, watching whatever the configuration now says.
///
/// Called from every way a capture can end, and from [`hook_recheck`]. A
/// failure here is not a notice any more but a [`Condition::HotkeyDead`]:
/// the app still runs and no key press will ever be seen, which is a state that
/// holds rather than a moment that passed.
///
/// With no session this does nothing at all, and that is what lets one state
/// machine serve both phases: "capture can never leave the app hookless" is
/// still true of a phase that was hookless before the capture started. Starting
/// a helper there would arm keys with no engine behind them, on a window whose
/// wizard is still saying the app is not set up yet.
fn respawn_helper(
    app: &AppHandle,
    state: &ShellState,
    capture: &CaptureSlot,
    session: Option<&mut Session>,
) {
    let Some(session) = session else {
        return;
    };
    stop_helper(Some(session));

    let bindings = match state.bindings() {
        Ok(bindings) => bindings,
        Err(err) => {
            state.set_condition(app, Condition::HotkeyDead, err);
            return;
        }
    };
    // Unreachable with a session in hand — `init` refuses to build one without
    // a helper — and still not worth a panic: the condition below is the state
    // a failed respawn leaves, said in the same place.
    let path = match capture.hook_path() {
        Ok(path) => path,
        Err(err) => {
            state.set_condition(app, Condition::HotkeyDead, err);
            return;
        }
    };

    match start_hook(app, state, capture.inputs.clone(), path, bindings) {
        Ok(hook) => {
            tracing::info!(bindings = %describe_bindings(bindings), "hotkey helper restarted");
            session.hook = Some(hook);
            state.set_binding_readout(app, bindings);
        }
        Err(err) => state.set_condition(
            app,
            Condition::HotkeyDead,
            format!("the hotkey helper could not be started: {err:#}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// The hotkey helper process
// ---------------------------------------------------------------------------

/// Windows `CREATE_NO_WINDOW`. Without it every launch — and every respawn —
/// flashes a console window over whatever the user is testing.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// How many times a helper that stops on its own is restarted in one session.
/// Beyond this it is not a hiccup, and retrying forever would just hide it.
const HELPER_RESTART_LIMIT: u32 = 5;
/// Pause before a restart, so a helper that dies instantly cannot spin.
const HELPER_RESTART_BACKOFF: Duration = Duration::from_secs(1);
/// How long a helper gets to notice its closed stdin before it is killed.
const HELPER_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// Polling interval for that grace period.
const HELPER_SHUTDOWN_POLL: Duration = Duration::from_millis(20);

/// A running `sotone-hook` helper, and the shutdown that guarantees no orphan.
///
/// The child handle is shared with the reader thread, which replaces it on a
/// restart; whichever of the two gets there first takes it out of the slot.
struct HookProcess {
    child: Arc<Mutex<Option<Child>>>,
    /// Set before shutdown so the reader thread reads the resulting EOF as
    /// "we are closing", not as "the helper crashed, restart it".
    stopping: Arc<AtomicBool>,
}

impl HookProcess {
    /// Close the pipe, give the helper a moment, then kill it.
    ///
    /// Dropping the write end of its stdin is the designed exit signal;
    /// `kill` is the fallback for a helper that is wedged. A helper left
    /// running would hold a global hook and a lock on its own exe — the stale
    /// `hotkey_probe.exe` that once broke a release link.
    fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        drop(child.stdin.take());

        let deadline = Instant::now() + HELPER_SHUTDOWN_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(error = %err, "could not wait for the hotkey helper");
                    break;
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(HELPER_SHUTDOWN_POLL);
        }

        tracing::debug!("the hotkey helper did not exit on its own; killing it");
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for HookProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Where `sotone-hook` lives: next to the running executable.
///
/// A missing helper is a broken install, and the message has to name the path
/// it looked at — "hotkeys do not work" with nothing else to go on is the
/// failure mode this whole arrangement exists to end.
fn helper_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not work out where Sotone is installed")?;
    let dir = exe
        .parent()
        .with_context(|| format!("{} has no parent directory", exe.display()))?;
    let name = if cfg!(windows) {
        "sotone-hook.exe"
    } else {
        "sotone-hook"
    };
    let path = dir.join(name);
    if !path.is_file() {
        bail!(
            "the hotkey helper is missing. Sotone expected to find {} next to itself; without it \
             no key press can be seen. Reinstall Sotone, or build it with `cargo build` so the \
             helper is built too.",
            path.display()
        );
    }
    Ok(path)
}

/// The helper's argument for one mode: the binding's config spelling, or `-`.
///
/// Every binding here was parsed from a config token, so its spelling parses
/// back. The one form that would not — `Unknown(n)`, for a key rdev has no name
/// for — cannot be produced by parsing, and capture refuses to report one
/// (`Binding::from_press`), so it cannot reach the config either.
fn helper_arg(binding: Option<Binding>) -> String {
    binding.map_or_else(|| "-".to_owned(), |binding| binding.to_string())
}

/// The command every helper is launched with, whichever mode it is in.
fn helper_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    command
        // Held open and never written: closing it is how the helper learns we
        // are gone, including when we are gone because we crashed.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Its tracing and its one-line failures land in ours.
        .stderr(Stdio::inherit());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// What the helper about to run will hook, as the debug log says it.
///
/// Read off `Bindings::needs_mouse` — the very call `sotone_core::hotkey`'s own
/// `hook_scope` makes to choose the rdev scope — so this readout cannot drift
/// from the decision it reports. That mattered enough to build: a `WH_MOUSE_LL`
/// hook is a tax on every mouse move on the machine (measured A/B on real
/// hardware), and before this readout the only way to know whether one was installed
/// was to feel the pointer stutter under a fullscreen game.
fn hook_scope_line(bindings: Bindings) -> String {
    let keys = describe_bindings(bindings);
    if bindings.needs_mouse() {
        format!("hook: keyboard + mouse ({keys})")
    } else {
        format!("hook: keyboard only ({keys})")
    }
}

/// Launch one helper process.
fn spawn_helper(path: &Path, bindings: Bindings) -> Result<Child> {
    helper_command(path)
        .arg(helper_arg(bindings.ptt))
        .arg(helper_arg(bindings.toggle))
        .spawn()
        .with_context(|| format!("could not start the hotkey helper {}", path.display()))
}

/// Launch the one-shot `--capture` helper.
///
/// It watches for one press, says which binding it was, and exits. Nothing here
/// asks it to *generate* a press — there is no such argument, and no such code
/// in the helper (invariant 1).
fn spawn_capture_helper(path: &Path) -> Result<Child> {
    helper_command(path)
        .arg("--capture")
        .spawn()
        .with_context(|| format!("could not start the hotkey helper {}", path.display()))
}

/// Read a capture helper's single line on a thread of its own.
///
/// The same reader-thread pattern the ordinary helper uses, and for the same
/// reason: the control thread must not sit waiting on a pipe while the user is
/// still deciding which key to press (invariant 5). Exactly one input is sent —
/// the press, or the reason there was not one — so the control thread's capture
/// state cannot be ended twice.
fn read_capture(input_tx: &Sender<ShellInput>, mut child: Child) -> HookProcess {
    let stdout = child.stdout.take();
    let slot = Arc::new(Mutex::new(Some(child)));
    let stopping = Arc::new(AtomicBool::new(false));
    let process = HookProcess {
        child: Arc::clone(&slot),
        stopping: Arc::clone(&stopping),
    };

    let reader_tx = input_tx.clone();
    let reader_stopping = Arc::clone(&stopping);
    let spawned = thread::Builder::new()
        .name("sotone-capture-reader".to_owned())
        .spawn(move || {
            let outcome = stdout.map_or_else(
                || Err("the key capture produced no output".to_owned()),
                capture_outcome,
            );
            // A cancel already put the ordinary helper back and killed this
            // process; the EOF that produced this outcome is that kill, not an
            // answer to anything.
            if reader_stopping.load(Ordering::SeqCst) {
                return;
            }
            let _ = reader_tx.send(ShellInput::Settings(match outcome {
                Ok(token) => SettingsInput::Captured { token },
                Err(reason) => SettingsInput::CaptureEnded { reason },
            }));
        });

    if let Err(err) = spawned {
        // No reader means no answer would ever arrive, so end the capture the
        // same way a crash would rather than leaving the panel listening.
        tracing::error!(error = %err, "could not read the key capture");
        let _ = input_tx.send(ShellInput::Settings(SettingsInput::CaptureEnded {
            reason: format!("{err}"),
        }));
    }

    process
}

/// The one thing a capture helper has to say, or why it said nothing.
fn capture_outcome(stdout: ChildStdout) -> Result<String, String> {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            break;
        };
        match HookMessage::from_line(&line) {
            Ok(HookMessage::Captured { token }) => return Ok(token),
            Ok(HookMessage::HookLost { reason }) => return Err(reason),
            // A recording event from a capture helper cannot happen — it was
            // started with no bindings to watch — and stray library output on
            // stdout is skipped here exactly as it is on the ordinary path.
            Ok(other) => tracing::warn!(?other, "unexpected message from the key capture"),
            Err(err) => {
                tracing::warn!(error = %err, line = %line, "unreadable line from the key capture");
            }
        }
    }
    Err("the key capture ended without reporting a press".to_owned())
}

/// Start the helper and the thread that reads it, and hand back the handle that
/// stops it again.
fn start_hook(
    app: &AppHandle,
    state: &ShellState,
    input_tx: Sender<ShellInput>,
    path: &Path,
    bindings: Bindings,
) -> Result<HookProcess> {
    let path = path.to_path_buf();
    let child = spawn_helper(&path, bindings)?;
    // A helper is running again, so whatever made the last one dead is over.
    // Cleared here rather than at each caller: this is the one place that knows
    // a process actually started.
    state.clear_condition(app, Condition::HotkeyDead);
    state.notice(app, NoticeLevel::Debug, hook_scope_line(bindings));

    let slot = Arc::new(Mutex::new(Some(child)));
    let stopping = Arc::new(AtomicBool::new(false));
    let hook = HookProcess {
        child: Arc::clone(&slot),
        stopping: Arc::clone(&stopping),
    };

    let app = app.clone();
    let state = state.clone();
    thread::Builder::new()
        .name("sotone-hook-reader".to_owned())
        .spawn(move || supervise(&app, &state, &input_tx, &path, bindings, &slot, &stopping))
        .context("could not start the hotkey helper reader thread")?;

    Ok(hook)
}

/// Read the helper until it stops, then decide whether to start another one.
///
/// A helper that reported `hook_lost` is not restarted for the same reason the
/// in-process listener never restarted: it said the hook is gone. A helper that
/// simply died — killed in Task Manager, crashed — *is* restarted, and that is
/// newly meaningful now the hook lives in a process of its own: rdev's
/// never-cleared global is poisoned per process, so a fresh process gets a
/// fresh hook.
fn supervise(
    app: &AppHandle,
    state: &ShellState,
    input_tx: &Sender<ShellInput>,
    path: &Path,
    bindings: Bindings,
    slot: &Mutex<Option<Child>>,
    stopping: &AtomicBool,
) {
    let mut restarts = 0_u32;

    loop {
        let Some(stdout) = lock(slot).as_mut().and_then(|child| child.stdout.take()) else {
            // Only reachable if shutdown got here first.
            return;
        };

        let hook_lost = read_helper(input_tx, stdout);

        if stopping.load(Ordering::SeqCst) {
            return;
        }
        // Reap it, so a dead helper is never left as a zombie handle.
        if let Some(mut child) = lock(slot).take() {
            let _ = child.wait();
        }
        if hook_lost {
            // The notice already went out through `ShellInput::HookLost`.
            return;
        }

        restarts += 1;
        if restarts > HELPER_RESTART_LIMIT {
            // Permanent until somebody acts, so a condition and not a
            // notice. Recheck is the way out: it respawns, and a fresh helper
            // gets a fresh supervisor whose count starts at zero.
            state.set_condition(
                app,
                Condition::HotkeyDead,
                format!("the hotkey helper stopped {HELPER_RESTART_LIMIT} times in a row"),
            );
            return;
        }
        state.notice(
            app,
            NoticeLevel::Warn,
            format!(
                "the hotkey helper stopped; restarting it (attempt {restarts} of \
                 {HELPER_RESTART_LIMIT})"
            ),
        );

        thread::sleep(HELPER_RESTART_BACKOFF);
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        match spawn_helper(path, bindings) {
            Ok(child) => {
                *lock(slot) = Some(child);
                // Shutdown may have run *during* the spawn, in which case it
                // found an empty slot and this one would be the orphan.
                if stopping.load(Ordering::SeqCst) {
                    if let Some(mut child) = lock(slot).take() {
                        drop(child.stdin.take());
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    return;
                }
                // The same two lines `start_hook` emits, because this is the
                // other place a helper process is actually born: the scope is
                // re-decided per spawn, so it is re-reported per spawn.
                state.clear_condition(app, Condition::HotkeyDead);
                state.notice(app, NoticeLevel::Debug, hook_scope_line(bindings));
            }
            Err(err) => {
                state.set_condition(
                    app,
                    Condition::HotkeyDead,
                    format!("the hotkey helper could not be started again: {err:#}"),
                );
                return;
            }
        }
    }
}

/// Forward one helper's output until its pipe ends. Returns whether it told us
/// the hook itself was lost.
fn read_helper(input_tx: &Sender<ShellInput>, stdout: ChildStdout) -> bool {
    let mut hook_lost = false;

    for line in BufReader::new(stdout).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                tracing::debug!(error = %err, "the hotkey helper's pipe ended");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let message = match HookMessage::from_line(&line) {
            Ok(message) => message,
            Err(err) => {
                // Anything a library printed to the helper's stdout lands here.
                // Skipping is right: a line we cannot read is not an event, and
                // a notice per line would drown the ones that matter.
                tracing::warn!(error = %err, line = %line, "unreadable line from the hotkey helper");
                continue;
            }
        };

        // A helper watching bindings has no capture to report, and a capture
        // helper is read somewhere else entirely — so this is a message from a
        // process that is not doing what we started it to do. Skipped, never
        // guessed at: a captured key must not be able to start a recording.
        let Some(event) = message.into_event() else {
            tracing::warn!(line = %line, "unexpected capture line from the hotkey helper");
            continue;
        };
        // Checkpoint 1 of the input trace, now one process removed: this is
        // what crossed the pipe. With checkpoint 2 on the control thread it
        // still brackets everything Sotone owns between the hook and the engine.
        tracing::debug!(event = ?event, "hotkey event");
        if matches!(event, PttEvent::HookLost { .. }) {
            hook_lost = true;
        }
        let _ = input_tx.send(match event {
            PttEvent::Pressed => ShellInput::Key(KeyInput::Press),
            PttEvent::Released { time } => ShellInput::Key(KeyInput::Release { at: time }),
            PttEvent::Toggled { time } => ShellInput::Key(KeyInput::Toggle { at: time }),
            PttEvent::HookLost { reason } => ShellInput::HookLost(reason),
        });
    }

    hook_lost
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// The whole of startup, in the order the user is told about it.
///
/// `Ok(None)` is the **empty phase**: no model could be resolved, so
/// there is no session to hand back — and that is not an error. The status and
/// the settings have already been published by the time it returns, exactly as
/// a fatal publishes its own status; the caller keeps its loop for the one
/// thing that still has an owner there, a hotkey capture (the wizard's key
/// step is on screen in exactly this phase).
///
/// `hook_path` is the control thread's one resolution of where `sotone-hook`
/// lives, passed in rather than made here: this is where it becomes fatal,
/// because a session without a helper is an app that can never record.
fn init(
    app: &AppHandle,
    state: &ShellState,
    input_tx: Sender<ShellInput>,
    hook_path: Result<&Path, String>,
) -> Result<Option<Session>> {
    state.set_status(app, StatusEvent::loading("reading the configuration"));
    let config_path = default_config_path().context("could not work out where the config lives")?;
    // A missing file writes defaults; a malformed one is an error and is never
    // silently replaced (the config is the user's file).
    let config = Config::load(&config_path)
        .with_context(|| format!("could not read {}", config_path.display()))?;
    // Still fatal, and deliberately so: only the *model* failures moved out of
    // that class and nothing else. A config naming an unreadable binding leaves
    // an app that can never record, and there is no panel that fixes it.
    let bindings = resolve_bindings(&config, &config_path)?;

    // The copies the rest of startup needs, taken while the config is still
    // local; everything after the hand-over below reads it back out of the
    // state at the moment of use.
    let project = config.active_project();
    let language = Language::new(config.effective_language(project));
    let project_name = project.map(|p| p.name.clone());
    let mic_substring = config.mic_substring.clone();
    let cues_enabled = config.audio_cues;
    tracing::info!(
        project = ?project_name,
        notes_dir = ?project.map(|p| p.notes_dir.display().to_string()),
        "active project at startup"
    );

    // **The hoist**, and the ordering is the reason it exists: the
    // shell takes the configuration here, *before* the model step, because the
    // model step's failure is the empty phase — and an empty phase whose
    // configuration had never been handed over would leave `model_set_active`,
    // `models_rescan` and the models-folder reveal all answering "Sotone is
    // still starting up". The one screen whose whole purpose is choosing a
    // model would be the one screen that could not.
    state.set_config(config_path, config);

    // The one-shot marker, consumed. This launch has already been
    // built disarmed from the same value — `start` read it before the state
    // existed — so all that is left is to spend it, and every launch from here
    // is an ordinary armed one. Immediately after the hand-over, so that no
    // status is published claiming a wizard that will not run.
    //
    // A failure to write is a warning and nothing more: the cost is one extra
    // disarmed launch, which is a great deal cheaper than refusing to start.
    if state.with_config(|config| config.onboarded) == Some(Onboarded::FirstLaunch) {
        if let Err(err) = state.edit_config(|config| {
            config.onboarded = Onboarded::Yes;
            Ok(())
        }) {
            tracing::warn!(error = %err, "could not record that the first launch happened");
        }
    }

    state.set_status(app, StatusEvent::loading("finding the model"));
    let Some(resolved) = state.with_config(resolve_model) else {
        bail!("the configuration went missing while Sotone was starting");
    };
    let model = match resolved {
        Ok(model) => model,
        Err(problem) => {
            let detail = problem.detail();
            tracing::info!(phase = Phase::Empty.as_str(), reason = %detail, "no model to run");
            // Status first, then the settings: the window switches view on the
            // status and fills the panel's list from the settings, so the other
            // order would paint one frame of a panel with nothing in it.
            state.set_status(app, StatusEvent::empty(detail));
            // No new event for the panel. `sotone://settings` already carries
            // `models_dir`, the usable models and every reject with its reason,
            // which is precisely "what your models folder holds right now".
            state.refresh_settings(app);
            // Startup stops here — before the microphone, before whisper and
            // before the helper process — so nothing is running that a restart
            // would have to tear down, and the overlay stays hidden for free
            // (`is_ready` is false).
            return Ok(None);
        }
    };

    state.set_status(app, StatusEvent::loading("opening the microphone"));
    let (mut engine, utterances) =
        AudioEngine::start(mic_substring.as_deref()).context("could not open the microphone")?;

    // The overlay pill's VU bars. Taken here, once, and drained on a
    // thread of its own; the feed is silent unless a recording is live, and it
    // survives a device change because the capture worker does.
    if let Some(levels) = engine.take_levels() {
        spawn_level_drain(app.clone(), levels);
    }

    // Cues are an enhancement, never a blocker: a machine with no speakers
    // still dictates, it just does it quietly.
    //
    // The player is opened whether or not cues are switched *on*:
    // the output stream is persistent by design, and
    // the Settings checkbox is live, so the alternative would be opening an
    // audio device from a click — or a restart marker on a row that has no
    // business needing one. Off simply means nothing is played.
    state.set_cues_enabled(cues_enabled);
    let cues = match CuePlayer::start() {
        Ok(player) => Some(Arc::new(player)),
        Err(err) => {
            tracing::warn!(error = %err, "running without audio cues");
            // Only worth saying when the user expected to hear something.
            if cues_enabled {
                state.notice(
                    app,
                    NoticeLevel::Warn,
                    format!("running without audio cues: {err}"),
                );
            }
            None
        }
    };

    state.set_status(
        app,
        StatusEvent::loading(format!("loading {} — this takes a moment", model.name)),
    );
    let transcriber = Transcriber::load(&model.path, language.clone())
        .with_context(|| format!("could not load the model {}", model.path.display()))?;

    // No draft is created here. Sotone opens on the drafts that are already
    // outstanding — Notepad semantics — and a draft per
    // launch would fill that list with empty notes nobody spoke into. The first
    // line the worker writes creates one.
    state.set_status(app, StatusEvent::loading("looking for outstanding drafts"));
    let store = DraftStore::new(default_drafts_dir());
    // Published for the playback command, which needs a path and must not have
    // a `Draft` handle. Set before anything can be clicked, since the window
    // only offers playback once lines exist.
    state.set_store(store.clone());

    // The 30-day sweep: the one permanent delete in this codebase, and never a
    // reason to fail startup.
    match store.sweep_trash(TRASH_RETENTION) {
        Ok(sweep) => tracing::info!(
            removed = sweep.removed,
            kept = sweep.kept,
            failed = sweep.failed,
            "swept discarded drafts"
        ),
        Err(err) => state.notice(
            app,
            NoticeLevel::Warn,
            format!("could not tidy the discarded drafts: {err}"),
        ),
    }

    let (release_tx, release_rx) = mpsc::channel::<ReleaseInfo>();
    let (draft_tx, draft_rx) = mpsc::channel::<SessionCommand>();
    let (worker, events) = SessionWorker::spawn(
        utterances,
        release_rx,
        draft_rx,
        SessionConfig {
            transcriber,
            draft: None,
            store: store.clone(),
            // The worker's own copy, kept current by `SessionCommand::SetProject`
            // whenever the active project changes.
            project: project_name,
            cues: cues.clone(),
        },
    )
    .context("could not start the transcription worker")?;

    // The worker starts with cues on, so it only has to be told when they are
    // off. Sent before the hook exists, so it is drained long before a key can
    // be pressed.
    if !cues_enabled {
        let _ = draft_tx.send(SessionCommand::SetCues(false));
    }

    state.set_status(app, StatusEvent::loading("listening for the hotkeys"));
    // Resolved once by the control thread before startup branched, so a respawn
    // after a rebind is not a second chance to get the path wrong. It is *spent*
    // here, where a broken install still fails startup with the message that
    // names the path it looked at — the empty phase, which never reaches this
    // line, keeps the outcome instead and refuses a capture with the same
    // sentence.
    let hook_path = hook_path.map_err(Error::msg)?;
    // The hook lives in `sotone-hook.exe`; this only starts it and reads its
    // pipe. Everything else — cues, engine calls, emits — happens on the
    // control thread, which owns them (`AudioEngine` is not `Sync`, so this is
    // the only sound way to share it anyway).
    let hook = start_hook(app, state, input_tx, hook_path, bindings)?;

    spawn_drain(app.clone(), state.clone(), store.clone(), events);

    let ready = ReadyInfo {
        model: model.name.clone(),
        model_path: model.path.display().to_string(),
        model_kind: model.kind,
        device: engine.device_name().to_owned(),
        cues: cues.as_ref().map(|p| p.device_name().to_owned()),
        backend: sotone_core::backend_name().to_owned(),
        language: language.to_string(),
        bindings: describe_bindings(bindings),
        note: model.note,
    };
    tracing::info!(
        model = %ready.model_path,
        device = %ready.device,
        backend = %ready.backend,
        drafts = %store.root().display(),
        bindings = %ready.bindings,
        "session ready"
    );
    state.set_status(app, StatusEvent::ready(ready));

    // After ready, so the panels are populated the moment the window has
    // somewhere to put them. The snapshot carries the same lists for a webview
    // that finishes loading later. Projects first: `default_save_dir` on the
    // drafts event is derived from the active project, and a window that had
    // the drafts list before it had the projects would render one frame of a
    // Projects tab that looks empty.
    state.refresh_projects(app);
    refresh_drafts(app, state, &store);
    // Last of the three, and the most expensive: it scans the models folder.
    state.refresh_settings(app);

    // The ready transition is the only moment the overlay may first appear:
    // before it there is nothing to be honest about, and a fatal never gets
    // here at all. Nothing to arrange first — the overlay thread sizes and
    // places it before the show, and the show repairs its own extended styles
    // behind it.
    if state.with_config(|config| config.overlay).unwrap_or(false) {
        state.overlay(OverlayInput::Visible(true));
    }

    Ok(Some(Session {
        hook: Some(hook),
        engine,
        worker,
        cues,
        release_tx,
        draft_tx,
        store,
    }))
}

/// Turn [`SessionEvent`]s into frontend events. Off the worker thread, so a
/// slow webview cannot delay the next transcription.
fn spawn_drain(
    app: AppHandle,
    state: ShellState,
    store: DraftStore,
    events: Receiver<SessionEvent>,
) {
    thread::spawn(move || {
        for event in events {
            match event {
                SessionEvent::LineAdded {
                    record,
                    token,
                    held_flush,
                } => {
                    // Whatever the outcome, the row that stood for this
                    // utterance is finished with: it is a line now.
                    if let Some(token) = token {
                        state.drop_pending(&app, &token);
                    }
                    // The overlay reveal and the counter are for **fresh
                    // successful decodes only**. A failed line has no words to
                    // reveal, and a held line's words were spoken minutes ago —
                    // reading them out as if they had just been said would be a
                    // lie about *now*, to a user whose eyes are on the thing
                    // under test.
                    if !record.failed && !held_flush {
                        let spoken_at = record.spoken_at.format("%H:%M:%S").to_string();
                        state.add_line(&app, spoken_at, record.text);
                    }
                    // The line changed the active draft's count, so the list is
                    // now stale. Re-scanning per line is a handful of small
                    // files and is what keeps the panel honest.
                    refresh_drafts(&app, &state, &store);
                }
                // Capture never waits for transcription, so the gap between the
                // two is a real state with rows of its own.
                SessionEvent::Queued {
                    token,
                    seconds,
                    spoken_at,
                } => {
                    let row = LineDto::pending(
                        token,
                        spoken_at.format("%H:%M:%S").to_string(),
                        "queued",
                        seconds,
                    );
                    state.update_pending(&app, |pending| pending.push(row));
                }
                SessionEvent::Decoding { token } => {
                    state.update_pending(&app, |pending| {
                        if let Some(row) = pending.iter_mut().find(|row| row.id == token) {
                            row.status = "transcribing";
                        }
                    });
                }
                // The write failed and the words are being kept in memory. No
                // footer message: the count in the footer *is* the surface, and
                // a sentence per held line would be a notice flood.
                SessionEvent::Held {
                    token,
                    draft_id,
                    text,
                    seconds,
                    spoken_at,
                } => {
                    state.update_pending(&app, |pending| {
                        let at = spoken_at.format("%H:%M:%S").to_string();
                        let row = match pending.iter_mut().find(|row| row.id == token) {
                            // In place, so a held line keeps the position its
                            // queued row had — which is its spoken order.
                            Some(row) => row,
                            None => {
                                pending.push(LineDto::pending(token, at.clone(), "held", seconds));
                                pending.last_mut().expect("just pushed")
                            }
                        };
                        row.status = "held";
                        row.text = text;
                        row.seconds = Some(seconds);
                        row.spoken_at = at;
                        row.draft_id = draft_id;
                    });
                }
                // Worker-side detail: the log and nowhere else. The model's own
                // error text arrives here, because the red row in the
                // transcript is what the user is meant to read.
                SessionEvent::Debug { message } => {
                    state.notice(&app, NoticeLevel::Debug, message);
                }
                // Every activation change arrives here, including the lazy
                // creation that a first line triggers — and it arrives *before*
                // that line, so the counter is set before it is incremented.
                SessionEvent::DraftChanged {
                    id,
                    dir,
                    line_count,
                } => {
                    tracing::info!(
                        draft = ?id,
                        dir = ?dir.as_ref().map(|d| d.display().to_string()),
                        line_count,
                        "active draft changed"
                    );
                    state.set_active_draft(id, line_count);
                    // The stale-dialog rule, made structural: a conflict is
                    // a question about one specific file, and the note the
                    // answer would be applied to has just changed. Retracted
                    // here as well as in the window, so a webview that reloads
                    // mid-switch cannot find the old question waiting.
                    state.clear_condition(&app, Condition::FileConflict);
                    refresh_drafts(&app, &state, &store);
                }
                SessionEvent::DraftDiscarded { id, ok, message } => {
                    if ok {
                        // The worker dropped this note's held lines with it
                        // (they would resurrect a note that went to `.trash`),
                        // so the rows that stood for them go too. Derived from
                        // an event that says what happened, not inferred from
                        // one that says something is wrong.
                        state.update_pending(&app, |pending| {
                            pending.retain(|row| row.draft_id.as_deref() != Some(id.as_str()));
                        });
                        state.notice(&app, NoticeLevel::Debug, DISCARDED);
                    } else {
                        state.notice(
                            &app,
                            NoticeLevel::Error,
                            format!(
                                "could not discard {id}: {} — the draft is still on disk",
                                message.unwrap_or_else(|| "unknown reason".to_owned())
                            ),
                        );
                    }
                    // No re-list here: the handle is dropped either way, so a
                    // `DraftChanged` always follows this event, and listing now
                    // would emit one frame still claiming the discarded draft
                    // is active.
                }
                // The transcript panel's whole contract: one snapshot per
                // change, never a delta, so the window's list cannot drift from
                // `lines.jsonl`.
                SessionEvent::Transcript { draft_id, lines } => {
                    state.set_transcript(&app, lines_event(draft_id, &lines));
                    // And re-list, because everything that produces a
                    // transcript — an edit, a move, a soft-delete — also marked
                    // the draft dirty on disk, and `dirty` only travels on the
                    // drafts event.
                    //
                    // Found the hard way: editing a line
                    // just after a save left Save greyed out, because the
                    // window's drafts snapshot still said `dirty: false` from
                    // the post-save re-list. Same stale-snapshot family as the
                    // fresh-note undo baseline — a second source of truth
                    // that nothing refreshed. The dirty dot in the list was
                    // wrong for exactly as long.
                    //
                    // This does re-list once more than strictly needed after a
                    // draft switch, which already re-lists on its
                    // `DraftChanged`. A handful of small files is the right
                    // price for a list that is never wrong.
                    refresh_drafts(&app, &state, &store);
                }
                // The one event that is answered by *two* things: the save
                // outcome for the dialog and the notice area, and — on success
                // only — a re-list, because the draft's dirty flag has just
                // been cleared on disk and the dot has to go.
                SessionEvent::Saved { outcome } => {
                    let saved = matches!(*outcome, SaveOutcome::Saved { .. });
                    let event = save_event(*outcome);
                    match (&event.message, event.outcome) {
                        (Some(message), _) => {
                            state.notice(&app, NoticeLevel::Error, message.clone());
                        }
                        // The footer already flips to "saved" and the tree's
                        // dirty dot already goes, so the sentence is
                        // log-only.
                        (None, "saved") => state.notice(
                            &app,
                            NoticeLevel::Debug,
                            format!(
                                "saved to {}",
                                event.path.as_deref().unwrap_or("the note's file")
                            ),
                        ),
                        // A conflict speaks through the strip. A notice as
                        // well would read as "something went wrong", and
                        // nothing did — nothing was written, on purpose.
                        (None, _) => {}
                    }
                    // The condition, which is what stays on screen until it is
                    // answered. A save that went through is the answer:
                    // Keep mine *is* an overwrite save, so the same branch
                    // clears it, and there is no second path to keep in step.
                    if event.outcome == "conflict" {
                        state.set_condition(
                            &app,
                            Condition::FileConflict,
                            conflict_detail(event.path.as_deref()),
                        );
                    } else if saved {
                        state.clear_condition(&app, Condition::FileConflict);
                    }
                    emit(&app, EVENT_SAVE, &event);
                    if saved {
                        refresh_drafts(&app, &state, &store);
                    }
                }
                // One batch, one event, one notice — never one per note: the
                // window answers a save outcome with a dialog or a notice, and
                // N of those for one click is what this design refuses. The
                // window builds the sentence; the re-list is unconditional
                // because a batch that saved nothing still may have been the
                // click that proved the dots are stale.
                SessionEvent::SavedAll {
                    saved,
                    skipped,
                    conflicts,
                    errors,
                } => {
                    let event = SaveAllEvent {
                        saved,
                        skipped,
                        conflicts: conflicts.iter().map(|p| p.display().to_string()).collect(),
                        errors,
                    };
                    emit(&app, EVENT_SAVE_ALL, &event);
                    refresh_drafts(&app, &state, &store);
                }
                // A note's file was renamed, or a renamed project's drafts were
                // carried across. One answer for both: re-list, so
                // every label — the tree, the breadcrumb, the tray's recents —
                // follows the store, because all of them ride `saved_path` and
                // `project` on the drafts payload.
                SessionEvent::Renamed { errors } => {
                    if !errors.is_empty() {
                        // A refusal is `warn` and reaches the footer's message
                        // slot; there is nothing to dismiss and no state to
                        // hold, because nothing was written.
                        state.notice(&app, NoticeLevel::Warn, errors.join("; "));
                    }
                    refresh_drafts(&app, &state, &store);
                }
                // The active draft's drop, from the one handle that may perform
                // it. Reported by the same function the control
                // thread's route uses.
                SessionEvent::NoteMoved {
                    draft,
                    project,
                    outcome,
                } => report_note_move(&app, &state, &store, &draft, project.as_deref(), outcome),
                SessionEvent::Notice { message } => {
                    state.notice(&app, NoticeLevel::Info, message);
                }
                // Log only, deliberately: the absence of the
                // per-line landed cue already signals a skip *live*, while the
                // user's eyes are on the thing under test, and a line in a
                // window they are not looking at added nothing. (Skips may one
                // day be promoted to the overlay pill if dictation feel wants
                // it.)
                SessionEvent::Skipped { reason, token } => {
                    // A skip that had a row (the model heard no words in it)
                    // takes the row away with it; the intake gates never made
                    // one, so there is nothing to retire.
                    if let Some(token) = token {
                        state.drop_pending(&app, &token);
                    }
                    state.notice(&app, NoticeLevel::Info, format!("skipped: {reason}"));
                }
                // The recording state is deliberately left alone: capture is
                // still running and the user's next press is still the press
                // that stops it. Log only for the same reason a skip is: the
                // split shows up as two lines in the transcript.
                SessionEvent::Capped { cap } => {
                    state.notice(
                        &app,
                        NoticeLevel::Info,
                        format!(
                            "hit the {} cap — split into a new line, still recording",
                            cap_label(cap)
                        ),
                    );
                }
                // The worker's own view of the same death the control thread
                // latches. It raises the condition too, because the two can
                // arrive in either order and neither may be the one that
                // decides: `set_condition` is idempotent on the detail, so the
                // second one through changes nothing.
                SessionEvent::EngineDead { reason } => {
                    // "Anything already *transcribed*", not "anything already
                    // captured": an utterance that was still queued for the
                    // model when the session stopped is lost, and the loss is
                    // reported below rather than contradicted here.
                    state.set_condition(
                        &app,
                        Condition::NoDevice,
                        format!(
                            "the microphone stopped ({reason}). Anything already transcribed \
                             was saved."
                        ),
                    );
                    state.disarm(&app);
                }
                SessionEvent::Error { message } => {
                    state.notice(&app, NoticeLevel::Error, message);
                }
            }
        }

        // The channel closed, so the worker is gone and so are its queues.
        // Every pending row left on screen is now a claim about a
        // model that no longer exists, and every held line was in that thread's
        // memory. Retract the rows and say what went with them: a row that goes
        // on promising a line nobody is working on is worse than the loss.
        //
        // Ordering: the worker's shutdown flush runs *before* it drops the
        // sender, so held lines that did reach disk have already retired their
        // rows through `LineAdded` by the time this runs. Whatever is still
        // here really was lost.
        let mut waiting = 0_usize;
        let mut held = 0_usize;
        state.update_pending(&app, |pending| {
            for row in pending.iter() {
                if row.status == "held" {
                    held += 1;
                } else {
                    waiting += 1;
                }
            }
            pending.clear();
        });
        if waiting > 0 {
            state.notice(
                &app,
                NoticeLevel::Warn,
                if waiting == 1 {
                    "1 recording was still waiting for the model when the session stopped, and \
                     was lost"
                        .to_owned()
                } else {
                    format!(
                        "{waiting} recordings were still waiting for the model when the session \
                         stopped, and were lost"
                    )
                },
            );
        }
        if held > 0 {
            state.notice(
                &app,
                NoticeLevel::Warn,
                if held == 1 {
                    "1 transcribed line was still held in memory when the session stopped, and \
                     was lost"
                        .to_owned()
                } else {
                    format!(
                        "{held} transcribed lines were still held in memory when the session \
                         stopped, and were lost"
                    )
                },
            );
        }
    });
}

/// Core save outcome → the DTO.
///
/// The one place the three outcomes are named for the frontend, so the window's
/// `switch` and this cannot drift.
fn save_event(outcome: SaveOutcome) -> SaveEvent {
    match outcome {
        SaveOutcome::Saved { path, lines, .. } => SaveEvent {
            path: Some(path.display().to_string()),
            lines: Some(lines),
            ..SaveEvent::of("saved")
        },
        SaveOutcome::Conflict {
            path,
            disk_text,
            pending_markdown,
        } => SaveEvent {
            path: Some(path.display().to_string()),
            disk_text: Some(disk_text),
            pending_markdown: Some(pending_markdown),
            ..SaveEvent::of("conflict")
        },
        SaveOutcome::Error { message } => SaveEvent {
            message: Some(message),
            ..SaveEvent::of("error")
        },
    }
}

/// The sentence [`Condition::FileConflict`] carries.
///
/// The design's own words for an error state, and the window pairs them with
/// the file name in the strip's title. Split out so the copy can be pinned by a
/// test without a draft store or a disk.
fn conflict_detail(path: Option<&str>) -> String {
    let what = path.map_or_else(|| "That file".to_owned(), |path| path.to_owned());
    format!(
        "{what} was edited by something else while Sotone held it. Sotone stopped rather than \
         overwrite the change, so nothing has been written."
    )
}

/// The cap as the user thinks of it: "2-minute", or seconds if it is not a
/// whole number of minutes. Derived from the event, never from a literal.
fn cap_label(cap: Duration) -> String {
    let seconds = cap.as_secs();
    if seconds >= 60 && seconds % 60 == 0 {
        format!("{}-minute", seconds / 60)
    } else {
        format!("{seconds}-second")
    }
}

/// A resolved model, plus whatever the user should be told about how it was
/// chosen.
struct ResolvedModel {
    path: PathBuf,
    name: String,
    kind: String,
    note: Option<String>,
}

/// Why there is no model to load, and therefore no session.
///
/// Its own type rather than an `anyhow::Error`, and that is the whole point:
/// every one of these is a *designed* first-run state — the empty state is
/// the onboarding — and giving them a type no `?` can turn into a startup
/// failure is what keeps the empty phase and the fatal phase apart
/// structurally. These were once `bail!`s, and a fresh machine met Sotone
/// as a red error panel with the tabs hidden.
///
/// Each variant carries what the sentence needs and nothing more, so the
/// wording lives in one place — [`NoModel::detail`] — and can be read without a
/// models folder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoModel {
    /// Nothing usable in the models folder. The expected state of a fresh
    /// install: no weights ship with Sotone.
    None {
        /// Where Sotone looked.
        dir: String,
    },
    /// Several, and nothing says which one to load.
    Several {
        /// Where Sotone looked.
        dir: String,
        /// What is in there, in scan order.
        names: Vec<String>,
    },
    /// The configuration names a model that is missing or is not a model.
    Rejected {
        /// The name as the configuration spells it.
        name: String,
        /// Where Sotone looked for it.
        dir: String,
        /// The validator's own sentence.
        why: String,
    },
    /// The models folder itself could not be read.
    Unreadable {
        /// Where Sotone looked.
        dir: String,
        /// The I/O error's sentence.
        why: String,
    },
}

impl NoModel {
    /// The line the panel puts at the top: the specific reason *this* machine
    /// has no session.
    ///
    /// No "then restart Sotone" tail any more — the panel carries a Restart
    /// button, and a sentence telling the user to do the thing the button in
    /// front of them does is how instructions stop being read.
    fn detail(&self) -> String {
        match self {
            Self::None { dir } => format!(
                "no model yet — Sotone ships without one. It needs a GGML whisper model (a .bin \
                 file) in {dir}."
            ),
            Self::Several { dir, names } => format!(
                "{dir} holds several models ({}) — pick the one to use.",
                names.join(", ")
            ),
            Self::Rejected { name, dir, why } => format!(
                "the configured model \"{name}\" should be a file in {dir}, and it was rejected: \
                 {why}"
            ),
            Self::Unreadable { dir, why } => {
                format!("the models folder {dir} could not be read: {why}")
            }
        }
    }
}

/// The model to load, from config alone — this is where the `--model` flag era
/// ends.
///
/// `active_model` (or a project's override) names a *file inside* `models_dir`,
/// and it is validated before whisper ever sees it, so a bad file fails here
/// with an explanation rather than at transcribe time.
/// With nothing set, exactly one model in the directory is unambiguous and is
/// used; zero or several are not, and the user is told which case they are in.
/// Nothing is ever written back to the config — the app does not decide for the
/// user.
///
/// Every way this can fail is a [`NoModel`], i.e. the empty phase. None of them
/// is fatal: the app comes up, says which of the four cases this machine is in,
/// and offers the controls that fix it.
fn resolve_model(config: &Config) -> Result<ResolvedModel, NoModel> {
    let dir = &config.models_dir;
    let dir_label = dir.display().to_string();

    if let Some(name) = config.effective_model(config.active_project()) {
        let path = dir.join(name);
        return match validate_model(&path) {
            Ok(info) => Ok(ResolvedModel {
                path,
                name: file_name(&info.path),
                kind: info.kind.to_string(),
                note: None,
            }),
            Err(err) => Err(NoModel::Rejected {
                name: name.to_owned(),
                dir: dir_label,
                why: err.to_string(),
            }),
        };
    }

    let scan = match scan_models_dir(dir) {
        Ok(scan) => scan,
        // A models folder that does not exist yet is *not* this case — the scan
        // reports that as an empty result, which is the fresh-install state.
        Err(err) => {
            return Err(NoModel::Unreadable {
                dir: dir_label,
                why: err.to_string(),
            })
        }
    };

    let mut models = scan.models;
    match models.len() {
        0 => Err(NoModel::None { dir: dir_label }),
        1 => {
            let info = models.remove(0);
            let name = file_name(&info.path);
            Ok(ResolvedModel {
                note: Some(format!(
                    "no active_model is set; {name} is the only model in {dir_label}"
                )),
                path: info.path,
                name,
                kind: info.kind.to_string(),
            })
        }
        _ => Err(NoModel::Several {
            names: models.iter().map(|m| file_name(&m.path)).collect(),
            dir: dir_label,
        }),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The bindings to watch, with the two rules the config layer already enforces
/// restated as fatal startup errors: at least one mode enabled, and two
/// distinct keys when both are.
fn resolve_bindings(config: &Config, config_path: &Path) -> Result<Bindings> {
    let ptt = config
        .ptt_enabled
        .then(|| {
            config.hotkey.parse::<Binding>().with_context(|| {
                format!(
                    "hotkey = \"{}\" in {}",
                    config.hotkey,
                    config_path.display()
                )
            })
        })
        .transpose()?;
    let toggle = config
        .toggle_enabled
        .then(|| {
            config.toggle_hotkey.parse::<Binding>().with_context(|| {
                format!(
                    "toggle_hotkey = \"{}\" in {}",
                    config.toggle_hotkey,
                    config_path.display()
                )
            })
        })
        .transpose()?;

    match (ptt, toggle) {
        (None, None) => bail!(
            "ptt_enabled and toggle_enabled are both false in {}, which leaves no way to \
             record; turn at least one back on.",
            config_path.display()
        ),
        // Resolved in favour of push-to-talk further down, which would leave
        // the toggle key looking broken instead of misconfigured.
        (Some(a), Some(b)) if a == b => bail!(
            "hotkey and toggle_hotkey are both {a} in {}; they need different bindings while \
             both modes are enabled.",
            config_path.display()
        ),
        _ => Ok(Bindings { ptt, toggle }),
    }
}

/// One line naming both bindings, for the session bar — and for the
/// tray's key hint, which is the same string read off the same readout.
fn describe_bindings(bindings: Bindings) -> String {
    let mut parts = Vec::new();
    if let Some(binding) = bindings.ptt {
        parts.push(format!("{binding} hold"));
    }
    if let Some(binding) = bindings.toggle {
        parts.push(format!("{binding} toggle"));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell state with both of its channels, and nothing else running.
    ///
    /// The receivers come back with it and must be held: dropping them would
    /// make every `send` fail, which is a state no live app is ever in.
    #[allow(clippy::type_complexity)]
    fn test_state() -> (
        ShellState,
        (
            Receiver<ShellInput>,
            Receiver<OverlayInput>,
            Receiver<TrayInput>,
        ),
    ) {
        let (input_tx, input_rx) = mpsc::channel::<ShellInput>();
        let (overlay_tx, overlay_rx) = mpsc::channel::<OverlayInput>();
        let (tray_tx, tray_rx) = mpsc::channel::<TrayInput>();
        (
            // Armed, which is every launch but a first run.
            ShellState::new(input_tx, overlay_tx, tray_tx, true),
            (input_rx, overlay_rx, tray_rx),
        )
    }

    /// A fixed pair of instants, so `held` is exact and no test sleeps.
    fn clock() -> (Instant, Instant) {
        let start = Instant::now();
        (start, start + Duration::from_millis(900))
    }

    fn when() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// Which mode, if any, owns the live recording.
    fn live(machine: &Machine) -> Option<Source> {
        match machine.state {
            RecordingState::Active { source, .. } => Some(source),
            RecordingState::Idle => None,
        }
    }

    #[test]
    fn push_to_talk_starts_and_stops() {
        let (t0, t1) = clock();
        let mut machine = Machine::default();

        assert_eq!(
            machine.apply(KeyInput::Press, true, t0),
            Action::Start {
                source: Source::Ptt
            }
        );
        assert_eq!(live(&machine), Some(Source::Ptt));
        assert_eq!(
            machine.apply(KeyInput::Release { at: when() }, true, t1),
            Action::Stop {
                source: Source::Ptt,
                at: when(),
                held: Duration::from_millis(900),
            }
        );
        assert_eq!(live(&machine), None);
    }

    #[test]
    fn toggle_starts_and_stops_on_two_presses() {
        let (t0, t1) = clock();
        let mut machine = Machine::default();

        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, true, t0),
            Action::Start {
                source: Source::Toggle
            }
        );
        // The stopping press is the line's timestamp.
        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, true, t1),
            Action::Stop {
                source: Source::Toggle,
                at: when(),
                held: Duration::from_millis(900),
            }
        );
    }

    #[test]
    fn the_other_mode_is_ignored_while_recording() {
        let (t0, _) = clock();
        let mut machine = Machine::default();

        machine.apply(KeyInput::Press, true, t0);
        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, true, t0),
            Action::Ignored {
                reason: IgnoreReason::Busy {
                    source: Source::Ptt
                }
            }
        );
        // Still push-to-talk's recording, and still running.
        assert_eq!(live(&machine), Some(Source::Ptt));

        let mut machine = Machine::default();
        machine.apply(KeyInput::Toggle { at: when() }, true, t0);
        assert_eq!(
            machine.apply(KeyInput::Press, true, t0),
            Action::Ignored {
                reason: IgnoreReason::Busy {
                    source: Source::Toggle
                }
            }
        );
        assert_eq!(
            machine.apply(KeyInput::Release { at: when() }, true, t0),
            Action::Ignored {
                reason: IgnoreReason::Busy {
                    source: Source::Toggle
                }
            }
        );
        assert_eq!(live(&machine), Some(Source::Toggle));
    }

    #[test]
    fn a_release_with_nothing_running_is_ignored() {
        let (t0, _) = clock();
        let mut machine = Machine::default();
        assert_eq!(
            machine.apply(KeyInput::Release { at: when() }, true, t0),
            Action::Ignored {
                reason: IgnoreReason::NotRecording
            }
        );
    }

    #[test]
    fn starts_are_refused_while_disarmed() {
        let (t0, _) = clock();
        let mut machine = Machine::default();

        assert_eq!(
            machine.apply(KeyInput::Press, false, t0),
            Action::Ignored {
                reason: IgnoreReason::Disarmed
            }
        );
        assert_eq!(live(&machine), None);
        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, false, t0),
            Action::Ignored {
                reason: IgnoreReason::Disarmed
            }
        );
        assert_eq!(live(&machine), None);
    }

    #[test]
    fn a_stop_while_disarmed_still_stops() {
        let (t0, t1) = clock();

        // Disarming mid-hold must not strand the recording that is already
        // capturing.
        let mut machine = Machine::default();
        machine.apply(KeyInput::Press, true, t0);
        assert_eq!(
            machine.apply(KeyInput::Release { at: when() }, false, t1),
            Action::Stop {
                source: Source::Ptt,
                at: when(),
                held: Duration::from_millis(900),
            }
        );
        assert_eq!(live(&machine), None);

        let mut machine = Machine::default();
        machine.apply(KeyInput::Toggle { at: when() }, true, t0);
        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, false, t1),
            Action::Stop {
                source: Source::Toggle,
                at: when(),
                held: Duration::from_millis(900),
            }
        );
        assert_eq!(live(&machine), None);
    }

    /// The Disarm button is reachable mid-recording, and after it nothing else
    /// could end the recording — so it has to.
    #[test]
    fn disarming_stops_a_live_recording_of_either_mode() {
        let (t0, t1) = clock();

        for source in [Source::Ptt, Source::Toggle] {
            let mut machine = Machine::default();
            match source {
                Source::Ptt => machine.apply(KeyInput::Press, true, t0),
                Source::Toggle => machine.apply(KeyInput::Toggle { at: when() }, true, t0),
            };
            assert_eq!(
                machine.apply(KeyInput::Disarmed { at: when() }, false, t1),
                Action::Stop {
                    source,
                    at: when(),
                    held: Duration::from_millis(900),
                }
            );
            assert_eq!(live(&machine), None);
        }
    }

    #[test]
    fn disarming_with_nothing_running_does_nothing() {
        let (t0, _) = clock();
        let mut machine = Machine::default();
        assert_eq!(
            machine.apply(KeyInput::Disarmed { at: when() }, false, t0),
            Action::None
        );
    }

    /// A disarm stop leaves the machine idle, not stuck: re-arming and pressing
    /// records again.
    #[test]
    fn recording_can_restart_after_a_disarm_stop() {
        let (t0, t1) = clock();
        let mut machine = Machine::default();

        machine.apply(KeyInput::Toggle { at: when() }, true, t0);
        machine.apply(KeyInput::Disarmed { at: when() }, false, t1);
        assert_eq!(
            machine.apply(KeyInput::Toggle { at: when() }, true, t1),
            Action::Start {
                source: Source::Toggle
            }
        );
    }

    /// The release of a push-to-talk key held across a disarm finds nothing
    /// running — the disarm already landed the line, so this is a notice, not a
    /// second line.
    #[test]
    fn a_release_after_a_disarm_stop_is_ignored() {
        let (t0, t1) = clock();
        let mut machine = Machine::default();

        machine.apply(KeyInput::Press, true, t0);
        machine.apply(KeyInput::Disarmed { at: when() }, false, t1);
        assert_eq!(
            machine.apply(KeyInput::Release { at: when() }, false, t1),
            Action::Ignored {
                reason: IgnoreReason::NotRecording
            }
        );
    }

    #[test]
    fn ignore_reasons_read_as_sentences() {
        assert!(IgnoreReason::Disarmed.to_string().contains("disarmed"));
        assert!(IgnoreReason::Busy {
            source: Source::Toggle
        }
        .to_string()
        .contains("toggle"));
    }

    #[test]
    fn cap_labels_use_minutes_only_when_they_are_whole() {
        assert_eq!(cap_label(Duration::from_secs(120)), "2-minute");
        assert_eq!(cap_label(Duration::from_secs(90)), "90-second");
        assert_eq!(cap_label(Duration::from_secs(30)), "30-second");
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        // The whole reason this is hand-rolled is that it is small enough to
        // pin exactly; these are the RFC's own test vectors, which cover all
        // three tail lengths.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn base64_covers_every_symbol_and_the_high_bytes_a_wav_is_made_of() {
        // A wav is arbitrary bytes, not text: 0x00 and 0xff have to survive.
        let all: Vec<u8> = (0..=255_u8).collect();
        let encoded = base64(&all);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.ends_with("/P3+/w=="), "{encoded}");
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYX"));
        // Padding only ever at the end, and never more than two.
        assert_eq!(encoded.matches('=').count(), 2);
        assert_eq!(base64(&[0x00]), "AA==");
        assert_eq!(base64(&[0xff, 0xff]), "//8=");
    }

    // -----------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------

    /// The mode word is the config's own vocabulary, and an unknown one is
    /// refused rather than defaulted — a typo must never rebind the other key.
    #[test]
    fn only_the_two_recording_modes_are_named() {
        assert_eq!(HotkeyMode::parse("ptt"), Some(HotkeyMode::Ptt));
        assert_eq!(HotkeyMode::parse("toggle"), Some(HotkeyMode::Toggle));
        for word in ["", "PTT", "Toggle", "push-to-talk", "ptt ", "both"] {
            assert_eq!(HotkeyMode::parse(word), None, "{word:?}");
        }
        assert_eq!(HotkeyMode::Ptt.as_str(), "ptt");
        assert_eq!(HotkeyMode::Toggle.as_str(), "toggle");
    }

    /// One sentence for "a recording is running", wherever it is carried.
    /// Every other mutation says it through a notice;
    /// `draft_create_detached` says it in an `Err`, because it owes its caller
    /// an id or a reason and a notice as well would say it twice. Two
    /// spellings is how the footer and a popup come to disagree about what was
    /// refused, so the words live in one function and this pins them.
    #[test]
    fn the_recording_refusal_is_one_sentence_wherever_it_is_carried() {
        let said = recording_refusal("move lines into a new note");
        assert_eq!(
            said,
            "not while a recording is running — stop it first, then move lines into a new note"
        );
        // And it is the same opening the notice path prints, which is what
        // makes the two indistinguishable to a reader.
        assert!(recording_refusal("save")
            .starts_with("not while a recording is running — stop it first, then "));
    }

    /// Settings must never write a configuration the next launch refuses to
    /// load: with both modes on, a capture that lands on the other mode's key
    /// is refused, and turning the last mode off is refused. Both go through
    /// `recording_mode_problem`, which is also what `Config::load` uses.
    #[test]
    fn settings_refuses_exactly_what_the_config_layer_refuses() {
        // A capture of F14 for push-to-talk while toggle is already F14.
        let clash = recording_mode_problem("F14", "F14", true, true)
            .expect("a shared binding must be refused");
        assert!(clash.contains("F14"), "{clash}");
        // The same capture is fine once the other mode is off.
        assert_eq!(recording_mode_problem("F14", "F14", true, false), None);
        // Turning off the last enabled mode.
        assert!(recording_mode_problem("F13", "F14", false, false)
            .expect("both off must be refused")
            .contains("at least one"));
        // And an ordinary rebind is allowed.
        assert_eq!(recording_mode_problem("MouseX1", "F14", true, true), None);
    }

    /// A rebind in the **empty phase** has nowhere to be applied: there is no
    /// helper to respawn and no session to tell, and finishing the wizard
    /// restarts the app. So the file is the whole of it, and this is the round
    /// trip that has to hold for the wizard's key step to mean anything — the
    /// same two functions the next launch calls, in the order it calls them.
    ///
    /// Filesystem, like the project group below and under the same guard: a
    /// config file this test alone owns, in the OS temp directory. No
    /// `AppHandle`, no helper process, no device.
    #[test]
    fn a_rebind_is_what_the_next_launch_resolves_its_bindings_from() {
        let tree = TempTree::new("rebind");
        let path = tree.path().join("config.toml");
        // A missing file writes the defaults, exactly as startup does.
        let mut config = Config::load(&path).expect("the defaults are written");
        assert_eq!(config.hotkey, "F13");

        assert_eq!(rebind(&mut config, HotkeyMode::Ptt, "MouseX1"), None);
        config.save(&path).expect("the rebind is written");

        let next = Config::load(&path).expect("the next launch reads its own file");
        let bindings = resolve_bindings(&next, &path).expect("both modes still resolve");
        let captured = "MouseX1"
            .parse::<Binding>()
            .expect("a captured token parses");
        assert_eq!(bindings.ptt, Some(captured));
        // The mode that was not rebound is untouched, in the file and in what
        // the helper would be told to watch.
        assert_eq!(next.toggle_hotkey, "F14");
        assert_eq!(
            bindings.toggle,
            Some("F14".parse::<Binding>().expect("the default parses"))
        );

        // And a refusal changes nothing: two enabled modes may not share a
        // binding, so the file never grows one the next load would reject.
        // "Changes", not "writes": production asks through
        // `edit_config(|c| Ok(rebind(..)))`, so a refusal comes back as
        // `Ok(Some(problem))` and the file is rewritten anyway — with the
        // content it already had. The content is the promise, and it is what
        // this asserts.
        let mut clash = next;
        assert!(rebind(&mut clash, HotkeyMode::Ptt, "F14").is_some());
        assert_eq!(clash.hotkey, "MouseX1", "a refused rebind changes nothing");
    }

    /// Mouse bindings are unreadable as tokens; everything else reads as
    /// itself. The token, not the label, is what is written to the file.
    #[test]
    fn bindings_are_labelled_for_a_human() {
        assert_eq!(binding_label("MouseX1"), "mouse side button 1");
        assert_eq!(binding_label("MouseX2"), "mouse side button 2");
        assert_eq!(binding_label("F13"), "F13");
        assert_eq!(binding_label(" Space "), "Space");
    }

    /// The settings snapshot's shape, as the window destructures it. The
    /// capture state is a fixed pair of spellings, so a refresh mid-capture
    /// cannot strand the panel in a state the window has no branch for.
    #[test]
    fn the_settings_event_has_the_shape_the_window_expects() {
        let idle = SettingsEvent::idle();
        assert_eq!(idle.capture, "idle");
        assert!(idle.capture_mode.is_none());
        assert!(idle.models.is_empty() && idle.rejected_models.is_empty());
        // The window seeds its view filter from this field, so the
        // placeholder payload has to carry the config's default rather than a
        // gap: a hide that flickered on before the config was read would take
        // rows off the screen for a frame.
        assert!(!idle.hide_deleted);

        let listening = SettingsEvent {
            capture: CAPTURE_LISTENING,
            capture_mode: Some(HotkeyMode::Toggle.as_str().to_owned()),
            ptt: HotkeyDto {
                mode: HotkeyMode::Ptt.as_str().to_owned(),
                token: "MouseX1".to_owned(),
                label: binding_label("MouseX1"),
                enabled: true,
            },
            ..SettingsEvent::idle()
        };
        assert_eq!(listening.capture, "capturing");
        assert_eq!(listening.capture_mode.as_deref(), Some("toggle"));
        assert_eq!(listening.ptt.mode, "ptt");
        assert_eq!(listening.ptt.token, "MouseX1");
        assert_eq!(listening.ptt.label, "mouse side button 1");
        assert!(listening.ptt.enabled);
        // The two states are the only two, and they are spelled once.
        assert_ne!(CAPTURE_IDLE, CAPTURE_LISTENING);
        // Empty rather than guessed: the window shows "(system default)".
        assert_eq!(listening.mic_substring, "");
        assert!(listening.active_model.is_none());
        // Nothing is loading in the placeholder, and the language row can be
        // drawn from it: the list is whisper's own table (a static one — no
        // model is loaded to read it), with auto first, and the window has no
        // copy of its own.
        assert!(idle.model_loading.is_none());
        assert_eq!(idle.language, AUTO_LANGUAGE);
        assert_eq!(
            idle.languages.first().map(|l| l.code.as_str()),
            Some("auto")
        );
        assert!(
            idle.languages.iter().any(|l| l.code == "en"),
            "whisper's table should name English"
        );
    }

    /// The two fields the window puts straight onto `<html>`. Both
    /// are closed sets, and both have to be right in the *placeholder* payload
    /// too — that is the one the window renders from while the config is still
    /// being read, and a blank platform there paints macOS chrome for a frame.
    #[test]
    fn the_settings_event_names_a_palette_and_a_platform() {
        let idle = SettingsEvent::idle();
        assert_eq!(idle.theme, "dark");
        assert!(matches!(idle.platform, "windows" | "macos" | "linux"));
        // Compiled for this machine, so this is the one the window will draw.
        assert_eq!(idle.platform, PLATFORM);

        // The palette words are the config's own spellings; the window matches
        // on "light" and treats everything else as dark.
        assert_eq!(Theme::Dark.as_str(), "dark");
        assert_eq!(Theme::Light.as_str(), "light");
    }

    /// The About tab's version comes from the build, and the placeholder
    /// payload carries it too — the About tab is reachable before the config
    /// has been read, and a version that pops in a moment later reads as a bug.
    #[test]
    fn the_settings_event_names_this_build() {
        let idle = SettingsEvent::idle();
        assert_eq!(idle.version, VERSION);
        assert!(!idle.version.is_empty());
        // A version, not a marketing string: the window prints it verbatim.
        assert!(
            idle.version.split('.').count() >= 2,
            "{} does not look like a version",
            idle.version
        );
    }

    /// **The two versions must agree.** `release.yml` reads
    /// `tauri.conf.json`'s `version` for the artifact name and the bundler
    /// stamps the installer with it, while the window shows
    /// `CARGO_PKG_VERSION`. Nothing in the toolchain ties the two together, so
    /// this does: a drift means the About tab names a build that is not the one
    /// the user downloaded.
    ///
    /// A string search rather than a JSON parse on purpose — `serde_json` is
    /// not a dependency of this crate, and adding one to assert a five-byte
    /// field would be the wrong trade.
    #[test]
    fn the_version_agrees_with_the_bundle_manifest() {
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json"),
        )
        .expect("tauri.conf.json sits beside this crate's Cargo.toml");
        let expected = format!("\"version\": \"{VERSION}\"");
        assert!(
            manifest.contains(&expected),
            "tauri.conf.json does not say {expected} — the installer and the About tab would disagree"
        );
    }

    #[test]
    fn binding_summaries_name_the_modes_that_are_on() {
        let ptt: Binding = "F13".parse().expect("F13 is a valid binding");
        let toggle: Binding = "F14".parse().expect("F14 is a valid binding");

        assert_eq!(
            describe_bindings(Bindings::new(ptt, toggle)),
            "F13 hold · F14 toggle"
        );
        assert_eq!(describe_bindings(Bindings::ptt_only(ptt)), "F13 hold");
        assert_eq!(
            describe_bindings(Bindings::toggle_only(toggle)),
            "F14 toggle"
        );
    }

    // -- The overlay's placement (four corners) ------------------------------
    //
    // The window itself is not testable without a display, but the arithmetic
    // that decides whether it lands on screen — and, since the pill, whether
    // its anchored edge stays put while it grows — is, and that is where the
    // bugs are.

    /// 1920×1080 with a 40 px taskbar along the bottom, at the origin.
    fn plain_work_area() -> WorkArea {
        WorkArea {
            x: 0,
            y: 0,
            width: 1920,
            height: 1040,
        }
    }

    /// The window size for one pill state, at scale factor 1.
    fn shape_size(shape: PillShape) -> WindowSize {
        let (width, height) = shape.logical_size();
        WindowSize { width, height }
    }

    /// The three states, largest last.
    const SHAPES: [PillShape; 3] = [PillShape::Idle, PillShape::Recording, PillShape::Reveal];

    const CORNERS: [OverlayCorner; 4] = [
        OverlayCorner::BottomLeft,
        OverlayCorner::BottomRight,
        OverlayCorner::TopLeft,
        OverlayCorner::TopRight,
    ];

    #[test]
    fn the_pill_sits_its_inset_in_from_the_corner_it_is_docked_to() {
        let area = plain_work_area();
        let window = shape_size(PillShape::Idle);
        // The window is flush in the corner and the *pill* is inset, because
        // the shadow margin is the inset — see OVERLAY_OFFSET.
        assert_eq!(OVERLAY_OFFSET, 0);

        let (x, y) = overlay_position(area, window, OverlayCorner::BottomLeft, OVERLAY_OFFSET);
        assert_eq!((x, y), (0, 1040 - window.height as i32));

        let (x, y) = overlay_position(area, window, OverlayCorner::TopRight, OVERLAY_OFFSET);
        assert_eq!((x, y), (1920 - window.width as i32, 0));
    }

    #[test]
    fn the_anchored_edge_never_moves_as_the_pill_grows() {
        // The design's rule (§2): the pill grows *away* from its corner. Four
        // corners × non-zero work-area origins × the three state sizes.
        for area in [
            plain_work_area(),
            // A taskbar docked to the left edge, and a second monitor above the
            // primary one, which puts the primary's origin at a negative y.
            WorkArea {
                x: 80,
                y: -160,
                width: 1840,
                height: 1080,
            },
        ] {
            for corner in CORNERS {
                let mut anchored: Option<(i32, i32)> = None;
                for shape in SHAPES {
                    let window = shape_size(shape);
                    let (x, y) = overlay_position(area, window, corner, OVERLAY_OFFSET);

                    // The edges that touch the corner, whichever ones those are.
                    let edge_x = if corner.is_left() {
                        x
                    } else {
                        x + window.width as i32
                    };
                    let edge_y = if corner.is_top() {
                        y
                    } else {
                        y + window.height as i32
                    };
                    match anchored {
                        None => anchored = Some((edge_x, edge_y)),
                        Some(first) => {
                            assert_eq!((edge_x, edge_y), first, "{corner:?} drifted at {shape:?}")
                        }
                    }

                    // And every state is inside the area it was placed in.
                    assert!(x >= area.x, "{corner:?} {shape:?}");
                    assert!(y >= area.y, "{corner:?} {shape:?}");
                    assert!(
                        x + window.width as i32 <= area.x + area.width as i32,
                        "{corner:?} {shape:?}"
                    );
                    assert!(
                        y + window.height as i32 <= area.y + area.height as i32,
                        "{corner:?} {shape:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_scaled_display_scales_the_inset_with_everything_else() {
        // The offset is a parameter precisely so this is testable: at 150% the
        // window still lands on the work area's corner, whatever the margin is.
        let area = plain_work_area();
        let window = WindowSize {
            width: 108,
            height: 99,
        };
        let (x, y) = overlay_position(area, window, OverlayCorner::BottomRight, 27);
        assert_eq!((x, y), (1920 - 108 - 27, 1040 - 99 - 27));
    }

    #[test]
    fn a_monitor_left_of_the_primary_one_does_not_drag_the_overlay_off_screen() {
        // Windows gives the primary monitor the origin, so a display placed to
        // its left has negative coordinates — the primary's own origin stays at
        // zero and the overlay must not follow the negative one.
        let (x, y) = overlay_position(
            plain_work_area(),
            shape_size(PillShape::Reveal),
            OverlayCorner::BottomRight,
            OVERLAY_OFFSET,
        );
        assert!(x > 0 && y > 0);
    }

    #[test]
    fn a_window_bigger_than_the_work_area_keeps_its_top_left_corner_on_screen() {
        // Cannot happen with the declared sizes, but a scaled-up display or a
        // tiny work area would otherwise produce a negative offset and push the
        // start of the line — which is the part worth reading — off the edge.
        let area = WorkArea {
            x: 12,
            y: 34,
            width: 200,
            height: 40,
        };
        let window = shape_size(PillShape::Reveal);
        for corner in CORNERS {
            let (x, y) = overlay_position(area, window, corner, 8);
            assert!(x >= area.x && y >= area.y, "{corner:?} left the work area");
        }
        // The right and bottom edges are where the subtraction would go
        // negative, so that is where the clamp does its work: the window keeps
        // its origin and loses its far edge instead.
        assert_eq!(
            overlay_position(area, window, OverlayCorner::BottomRight, 8),
            (area.x, area.y)
        );
        // A left/top corner never needed clamping — it is measured from the
        // near edge, so the inset is still honoured.
        assert_eq!(
            overlay_position(area, window, OverlayCorner::TopLeft, 8),
            (area.x + 8, area.y + 8)
        );
    }

    #[test]
    fn every_pill_state_is_the_pill_plus_its_shadow_margin() {
        // The numbers the CSS animates between, with room for the shadow. The
        // heights are equal on purpose: only the width changes, which is what
        // makes the anchored edge above provable rather than lucky.
        let margin = 2 * OVERLAY_SHADOW_MARGIN as u32;
        assert_eq!(
            PillShape::Idle.logical_size(),
            (PILL_WIDTH_IDLE + margin, PILL_HEIGHT + margin)
        );
        assert_eq!(
            PillShape::Recording.logical_size(),
            (PILL_WIDTH_RECORDING + margin, PILL_HEIGHT + margin)
        );
        assert_eq!(
            PillShape::Reveal.logical_size(),
            (PILL_WIDTH_REVEAL + margin, PILL_HEIGHT + margin)
        );
        for shape in SHAPES {
            assert_eq!(shape.logical_size().1, PillShape::Idle.logical_size().1);
        }
        // And the three are in the order the loop grows through them, which is
        // what "the window is never smaller than the pill" rests on.
        let widths: Vec<u32> = SHAPES.iter().map(|s| s.logical_size().0).collect();
        let mut sorted = widths.clone();
        sorted.sort_unstable();
        assert_eq!(widths, sorted);
    }

    #[test]
    fn physical_pixels_round_and_never_collapse_to_nothing() {
        assert_eq!(scaled(100, 1.0), 100);
        assert_eq!(scaled(100, 1.5), 150);
        assert_eq!(scaled(36, 1.25), 45);
        // A pathological scale factor must not produce a zero-sized window.
        assert_eq!(scaled(1, 0.0), 1);
    }

    // -- The pill's shape over one capture cycle -----------------------------

    /// The reveal duration the tests reason in.
    const REVEAL: Duration = Duration::from_secs(10);

    #[test]
    fn the_window_grows_for_the_recording_and_shrinks_after_the_reveal() {
        let mut sizer = PillSizer::new();
        let start = Instant::now();
        assert_eq!(sizer.shape, PillShape::Idle);
        assert_eq!(sizer.wait(start), None, "an idle pill has no deadline");

        assert_eq!(
            sizer.apply(PillEvent::RecordingStarted, start, REVEAL),
            PillShape::Recording
        );
        assert_eq!(sizer.wait(start), None, "a live recording never times out");

        // The release: the decode is still running, so the pill stays expanded
        // with its bars at rest rather than snapping back to the glyph.
        let released = start + Duration::from_secs(4);
        assert_eq!(
            sizer.apply(PillEvent::RecordingStopped, released, REVEAL),
            PillShape::Recording
        );

        // The line lands: reveal, and a deadline that outlasts the collapse.
        let landed = released + Duration::from_secs(1);
        assert_eq!(
            sizer.apply(PillEvent::LineLanded, landed, REVEAL),
            PillShape::Reveal
        );
        assert_eq!(sizer.wait(landed), Some(REVEAL + PILL_COLLAPSE));

        assert_eq!(
            sizer.apply(PillEvent::Elapsed, landed + REVEAL + PILL_COLLAPSE, REVEAL),
            PillShape::Idle
        );
        assert_eq!(sizer.wait(landed), None);
    }

    /// Found with a DPI probe against the real window: the shell publishes
    /// `RecordingEvent::IDLE` once at startup so the indicator has an opening
    /// position, and the pill treated it as a recording that had just ended —
    /// so every launch opened the window to the recording width, with a clock
    /// reading 00:00 on it, for a whole reveal period. The probe measured
    /// 128 px of "idle" pill and said so.
    #[test]
    fn the_indicators_opening_state_is_not_a_recording_that_ended() {
        let mut sizer = PillSizer::new();
        let start = Instant::now();
        assert_eq!(
            sizer.apply(PillEvent::RecordingStopped, start, REVEAL),
            PillShape::Idle
        );
        assert_eq!(
            sizer.wait(start),
            None,
            "nothing ended, so nothing is timed"
        );

        // And a stop that follows nothing is inert however often it arrives.
        sizer.apply(PillEvent::RecordingStopped, start, REVEAL);
        assert_eq!(sizer.shape, PillShape::Idle);

        // A real one still behaves: press, release, grace.
        sizer.apply(PillEvent::RecordingStarted, start, REVEAL);
        assert_eq!(
            sizer.apply(PillEvent::RecordingStopped, start, REVEAL),
            PillShape::Recording
        );
        assert_eq!(sizer.wait(start), Some(REVEAL + PILL_COLLAPSE));
    }

    /// The other half of the same bug: a stop arriving while a line is being
    /// revealed must not restart the reveal's timer or reopen anything.
    #[test]
    fn a_stray_stop_during_a_reveal_leaves_the_reveal_alone() {
        let mut sizer = PillSizer::new();
        let landed = Instant::now();
        sizer.apply(PillEvent::LineLanded, landed, REVEAL);
        let deadline = sizer.wait(landed);

        let later = landed + Duration::from_secs(4);
        assert_eq!(
            sizer.apply(PillEvent::RecordingStopped, later, REVEAL),
            PillShape::Reveal
        );
        assert_eq!(
            sizer.wait(landed),
            deadline,
            "the reveal timer was restarted by something that never started"
        );
    }

    #[test]
    fn a_skipped_utterance_cannot_leave_the_pill_expanded_forever() {
        // No line ever arrives (the utterance was too short, or silent): the
        // grace is the reveal duration, and then it collapses anyway.
        let mut sizer = PillSizer::new();
        let start = Instant::now();
        sizer.apply(PillEvent::RecordingStarted, start, REVEAL);
        sizer.apply(PillEvent::RecordingStopped, start, REVEAL);
        assert_eq!(sizer.shape, PillShape::Recording);
        assert_eq!(sizer.wait(start), Some(REVEAL + PILL_COLLAPSE));
        assert_eq!(
            sizer.apply(PillEvent::Elapsed, start + REVEAL + PILL_COLLAPSE, REVEAL),
            PillShape::Idle
        );
    }

    #[test]
    fn a_line_that_lands_mid_recording_waits_for_the_recording_to_end() {
        // The pill and the indicator must never disagree about *now*: a decode
        // finishing during the next utterance does not interrupt the VU.
        let mut sizer = PillSizer::new();
        let start = Instant::now();
        sizer.apply(PillEvent::RecordingStarted, start, REVEAL);
        assert_eq!(
            sizer.apply(PillEvent::LineLanded, start, REVEAL),
            PillShape::Recording
        );
        assert_eq!(
            sizer.wait(start),
            None,
            "still recording, still no deadline"
        );

        let stopped = start + Duration::from_secs(3);
        assert_eq!(
            sizer.apply(PillEvent::RecordingStopped, stopped, REVEAL),
            PillShape::Reveal,
            "the owed line is revealed the moment the recording ends"
        );
        assert_eq!(sizer.wait(stopped), Some(REVEAL + PILL_COLLAPSE));
    }

    #[test]
    fn a_new_recording_supersedes_a_reveal_and_a_grace_alike() {
        let start = Instant::now();
        for interrupted in [PillEvent::LineLanded, PillEvent::RecordingStopped] {
            let mut sizer = PillSizer::new();
            if interrupted == PillEvent::RecordingStopped {
                sizer.apply(PillEvent::RecordingStarted, start, REVEAL);
            }
            sizer.apply(interrupted, start, REVEAL);
            assert!(sizer.wait(start).is_some());

            let pressed = start + Duration::from_secs(2);
            assert_eq!(
                sizer.apply(PillEvent::RecordingStarted, pressed, REVEAL),
                PillShape::Recording
            );
            assert_eq!(
                sizer.wait(pressed),
                None,
                "the superseded reveal's deadline must go with it"
            );
        }
    }

    #[test]
    fn each_new_line_restarts_the_reveal_timer() {
        let mut sizer = PillSizer::new();
        let first = Instant::now();
        sizer.apply(PillEvent::LineLanded, first, REVEAL);

        let second = first + Duration::from_secs(6);
        assert_eq!(
            sizer.apply(PillEvent::LineLanded, second, REVEAL),
            PillShape::Reveal
        );
        // Measured from the new line, not the old one.
        assert_eq!(sizer.wait(second), Some(REVEAL + PILL_COLLAPSE));
    }

    #[test]
    fn the_window_shrinks_only_after_the_css_collapse_has_run() {
        // The motion table gives the collapse 260ms; shrinking the window under
        // it would clip the animation halfway.
        assert!(PILL_COLLAPSE >= Duration::from_millis(260));
        assert!(
            PILL_COLLAPSE < Duration::from_millis(600),
            "and not much more"
        );
    }

    #[test]
    fn a_reveal_duration_setting_is_honoured_by_the_window_too() {
        let mut sizer = PillSizer::new();
        let now = Instant::now();
        let short = Duration::from_secs(3);
        sizer.apply(PillEvent::LineLanded, now, short);
        assert_eq!(sizer.wait(now), Some(short + PILL_COLLAPSE));
    }

    // -- The overlay's extended styles ---------------------------------------

    /// The word the instrumented probe run read off the live overlay window,
    /// and the word the setup-time fixup had logged as its `before`: tao had
    /// rewritten it back. `WS_EX_APPWINDOW` (0x0004_0000) set, no
    /// `WS_EX_TOOLWINDOW` (0x80), with `WS_EX_NOACTIVATE` (0x0800_0000) and
    /// `WS_EX_TOPMOST` (0x8) still there because tao stores those itself.
    #[cfg(windows)]
    const CLOBBERED_EX_STYLE: isize = 0x0804_0118;

    #[test]
    #[cfg(windows)]
    fn the_fixup_adds_toolwindow_and_takes_appwindow_away() {
        // The exact pair from the diagnostic run, so a change to this
        // arithmetic has to argue with the machine that produced it.
        assert_eq!(overlay_ex_style(CLOBBERED_EX_STYLE), 0x0800_0198);
    }

    #[test]
    #[cfg(windows)]
    fn the_fixup_leaves_every_other_bit_alone() {
        // Only the two bits it is allowed to touch may differ — `set_visible`
        // brings back whatever tao believes, and the fixup must not undo the
        // parts of that which are correct (NOACTIVATE, TOPMOST, and the plain
        // window styles that share the word).
        let after = overlay_ex_style(CLOBBERED_EX_STYLE);
        let touched = after ^ CLOBBERED_EX_STYLE;
        assert_eq!(touched, 0x0004_0080);
    }

    #[test]
    #[cfg(windows)]
    fn the_fixup_is_idempotent_so_a_second_show_writes_nothing() {
        // The show path runs this after every show; a fixed word must be a
        // fixed point, or every show would be a real `SetWindowLongPtrW`.
        let once = overlay_ex_style(CLOBBERED_EX_STYLE);
        assert_eq!(overlay_ex_style(once), once);
    }

    // -----------------------------------------------------------------------
    // The first run
    //
    // `resolve_model` itself needs a models folder, so what is pinned here is
    // the part that decides what the user *reads*: which phase a machine is in
    // and what the panel says about it.
    // -----------------------------------------------------------------------

    /// The four words the window switches on. `Phase::as_str` is what the empty
    /// path logs and what these assertions read; it sits directly under the
    /// `rename_all = "lowercase"` derive it has to agree with, so a variant
    /// renamed without a thought for the frontend fails here.
    ///
    /// (A JSON round-trip would pin the derive itself, but that needs
    /// `serde_json` as a dev-dependency of this crate, which is not worth it
    /// for a four-word assertion.)
    #[test]
    fn every_phase_has_the_word_the_window_switches_on() {
        assert_eq!(Phase::Loading.as_str(), "loading");
        assert_eq!(Phase::Ready.as_str(), "ready");
        assert_eq!(Phase::Empty.as_str(), "empty");
        assert_eq!(Phase::Fatal.as_str(), "fatal");
        // Four distinct words: two phases sharing one would leave the window
        // rendering the wrong view with no error anywhere.
        let words = [
            Phase::Loading.as_str(),
            Phase::Ready.as_str(),
            Phase::Empty.as_str(),
            Phase::Fatal.as_str(),
        ];
        for (at, word) in words.iter().enumerate() {
            assert!(
                !words[at + 1..].contains(word),
                "{word:?} names more than one phase"
            );
        }
    }

    /// The empty status is a described state, not a reported failure: the phase
    /// is `Empty`, there is no `ready` payload, and the detail is the sentence
    /// it was given rather than an error chain.
    #[test]
    fn the_empty_status_carries_the_reason_and_no_readout() {
        let status = StatusEvent::empty(
            NoModel::None {
                dir: "C:\\models".to_owned(),
            }
            .detail(),
        );
        assert_eq!(status.phase, Phase::Empty);
        assert!(status.ready.is_none());
        assert!(status.detail.contains("C:\\models"), "{}", status.detail);
    }

    /// The split: every model problem is the *empty* phase, and
    /// each one says which of the four cases this machine is in. The details
    /// name the thing the user has to act on — the folder, the file names, the
    /// validator's reason — because the panel shows this line and nothing else
    /// explains why the machine is here.
    #[test]
    fn each_no_model_case_names_what_the_user_has_to_fix() {
        let none = NoModel::None {
            dir: "C:\\models".to_owned(),
        }
        .detail();
        assert!(none.contains("C:\\models"), "{none}");
        assert!(none.contains(".bin"), "{none}");

        let several = NoModel::Several {
            dir: "C:\\models".to_owned(),
            names: vec![
                "ggml-base.en.bin".to_owned(),
                "ggml-small.en.bin".to_owned(),
            ],
        }
        .detail();
        assert!(several.contains("ggml-base.en.bin"), "{several}");
        assert!(several.contains("ggml-small.en.bin"), "{several}");
        assert!(several.contains("pick"), "{several}");

        let rejected = NoModel::Rejected {
            name: "ggml-tiny.en.bin".to_owned(),
            dir: "C:\\models".to_owned(),
            why: "is in GGUF format, which whisper.cpp cannot load".to_owned(),
        }
        .detail();
        assert!(rejected.contains("ggml-tiny.en.bin"), "{rejected}");
        assert!(rejected.contains("GGUF"), "{rejected}");

        let unreadable = NoModel::Unreadable {
            dir: "C:\\models".to_owned(),
            why: "access is denied".to_owned(),
        }
        .detail();
        assert!(unreadable.contains("access is denied"), "{unreadable}");
    }

    /// None of these sentences may end in the old "then restart Sotone" tail:
    /// the panel carries a Restart button now, and telling the user in prose to
    /// do the thing the button in front of them does is how prose stops being
    /// read.
    #[test]
    fn no_model_sentences_leave_restarting_to_the_button() {
        for problem in [
            NoModel::None {
                dir: "C:\\models".to_owned(),
            },
            NoModel::Several {
                dir: "C:\\models".to_owned(),
                names: vec!["a.bin".to_owned(), "b.bin".to_owned()],
            },
            NoModel::Rejected {
                name: "a.bin".to_owned(),
                dir: "C:\\models".to_owned(),
                why: "not a model".to_owned(),
            },
            NoModel::Unreadable {
                dir: "C:\\models".to_owned(),
                why: "access is denied".to_owned(),
            },
        ] {
            let detail = problem.detail();
            assert!(!detail.contains("restart Sotone"), "{detail}");
        }
    }

    // -----------------------------------------------------------------------
    // The onboarding wizard
    //
    // Everything pinned here is a pure decision: which surface the window is
    // being told to draw, and what a folder would be called. No window is
    // created, no config file is read — `start` and `init` need both, and
    // neither is reachable without an `AppHandle`.
    // -----------------------------------------------------------------------

    /// The flag the window routes on, in the two directions that matter: the
    /// three-state marker never crosses the boundary, and `"first-launch"` —
    /// which exists only so the launch after a wizard starts disarmed — reads as
    /// **not done**, so nothing about arming can be inferred from `onboarded`.
    #[test]
    fn only_yes_tells_the_window_the_wizard_is_behind_it() {
        assert!(Onboarded::Yes.is_done());
        assert!(!Onboarded::No.is_done());
        assert!(!Onboarded::FirstLaunch.is_done());

        // The status carries one bool and nothing else about onboarding, and
        // every constructor leaves it for `set_status` to re-derive.
        let status = StatusEvent::empty("no model yet");
        assert!(status.onboarded);
        assert_eq!(status.phase, Phase::Empty);
    }

    /// Before the configuration exists, the answer is "not onboarding".
    ///
    /// The safe direction, and the reason is asymmetric: a first run that shows
    /// the loading view for one extra frame costs nothing, while an existing
    /// user who sees a flash of step one has been lied to about their own
    /// install.
    #[test]
    fn the_onboarding_flag_is_true_until_the_configuration_is_known() {
        let (state, _channels) = test_state();
        assert!(state.onboarded());
    }

    /// The folder echo under the project-name field. It is `file_safe` — the one
    /// sanitizer — with the extension it appends taken back off, and **not** a
    /// lowercase-hyphen slug: a rename carries the folder to whatever the new
    /// name sanitizes to (`folder_plan`), so the echo has to be that
    /// same name.
    #[test]
    fn the_slug_echo_is_the_one_sanitizer_without_the_extension() {
        assert_eq!(
            project_slug_preview("Checkout rebuild".to_owned()),
            "Checkout rebuild"
        );
        // The characters a Windows folder cannot have, replaced exactly as a
        // note's filename would have them replaced.
        assert_eq!(
            project_slug_preview("build 3/4: rc?".to_owned()),
            "build 3-4- rc-"
        );
        // Trailing dots and spaces go, because Windows would strip them at
        // creation time and the config would then name a folder that is not
        // there.
        assert_eq!(project_slug_preview("  Ludo . ".to_owned()), "Ludo");
        // A name that is nothing but forbidden characters still has to produce a
        // creatable folder; `file_safe`'s own fallback does it.
        assert_eq!(project_slug_preview("...".to_owned()), "note");
        // A project genuinely called "notes.md" keeps its name — only the
        // extension this call added is taken off.
        assert_eq!(project_slug_preview("notes.md".to_owned()), "notes.md");
        // Blank in, blank out: the window renders no echo and offers no Create.
        assert!(project_slug_preview("   ".to_owned()).is_empty());
        assert!(project_slug_preview(String::new()).is_empty());
    }

    /// The wizard's suggested notes folder is a suggestion and a real path.
    /// Nothing is created here — the project step's `project_create` is the
    /// first thing that touches the disk.
    #[test]
    fn the_suggested_notes_root_is_a_path_the_window_can_show() {
        let root = onboarding_notes_root();
        assert!(root.ends_with("Sotone"), "{root}");
    }

    // -----------------------------------------------------------------------
    // Live-apply. Pure decisions only: no device is opened, no
    // model is loaded, and nothing here touches whisper.
    // -----------------------------------------------------------------------

    /// The one-load-at-a-time rule. The claim and the check are the same act,
    /// so two clicks cannot both start a load — and the refusal names the model
    /// that is holding the slot, because "still loading" with no name is a
    /// message the user cannot act on.
    #[test]
    fn only_one_model_load_may_be_in_flight() {
        let (state, _channels) = test_state();

        state
            .claim_model_load("ggml-small.en.bin")
            .expect("the first load takes the slot");

        let refusal = state
            .claim_model_load("ggml-large-v3-turbo.bin")
            .expect_err("the second is refused");
        assert!(refusal.contains("ggml-small.en.bin"), "{refusal}");

        // And the slot is given back whatever the load did, so a failure does
        // not wedge the picker for the rest of the session.
        state.release_model_load();
        state
            .claim_model_load("ggml-large-v3-turbo.bin")
            .expect("the slot is free again");
    }

    /// Every reconnect outcome names what is capturing *now*. A user told only
    /// that their choice failed does not know whether the app can still hear
    /// them.
    #[test]
    fn every_microphone_outcome_says_what_is_listening() {
        use sotone_core::audio::AudioError;

        // A confirmation the About tab and the footer already carry, so it is
        // log traffic and not a footer message.
        let (level, message) = mic_notice(&Reconnect::Switched {
            device: "Yeti Stereo Microphone".to_owned(),
        });
        assert_eq!(level, NoticeLevel::Debug);
        assert!(message.contains("Yeti Stereo Microphone"), "{message}");
        // The banner is dead: a device change is applied, not deferred.
        assert!(message.contains("no restart needed"), "{message}");

        // A refusal: the user asked for a device and did not get it, and there
        // is no state to pin a strip to because the old one is still capturing.
        let (level, message) = mic_notice(&Reconnect::Reverted {
            device: "Stub Microphone".to_owned(),
            error: AudioError::NoInputDevice,
        });
        assert_eq!(level, NoticeLevel::Warn);
        // Both halves: why the choice failed, and what is running instead.
        assert!(message.contains("no audio input device"), "{message}");
        assert!(message.contains("Stub Microphone"), "{message}");

        // The one that becomes a condition. The strip says this sentence and it
        // stays, so putting it in the footer slot as well would report one
        // failure twice.
        let (level, message) = mic_notice(&Reconnect::Lost {
            error: AudioError::NoInputDevice,
            revert_error: AudioError::StreamThreadDied,
        });
        assert_eq!(level, NoticeLevel::Debug);
        assert!(message.contains("cannot hear"), "{message}");
    }

    // -----------------------------------------------------------------------
    // Conditions. Pure state: nothing here opens a device, spawns a
    // helper or touches a window.
    // -----------------------------------------------------------------------

    /// The precedence rule, which is the whole reason conditions are three
    /// slots and not one value: deafness beats everything, and a conflict can
    /// wait. Held simultaneously, exactly one is published.
    #[test]
    fn the_worst_condition_is_the_one_published() {
        let mut conditions = Conditions::default();
        assert_eq!(conditions.event(), ConditionEvent::default());

        conditions
            .slot(Condition::FileConflict)
            .replace("D".to_owned());
        assert_eq!(conditions.top(), Some((Condition::FileConflict, "D")));

        conditions.slot(Condition::NoDevice).replace("A".to_owned());
        assert_eq!(conditions.top(), Some((Condition::NoDevice, "A")));

        conditions
            .slot(Condition::HotkeyDead)
            .replace("F".to_owned());
        assert_eq!(conditions.top(), Some((Condition::HotkeyDead, "F")));
        assert_eq!(conditions.event().condition, Some("hotkeyDead"));

        // Clearing the top one reveals the next, rather than clearing the lot:
        // the helper coming back does not answer the question about the file.
        conditions.slot(Condition::HotkeyDead).take();
        assert_eq!(conditions.top(), Some((Condition::NoDevice, "A")));
        conditions.slot(Condition::NoDevice).take();
        assert_eq!(conditions.top(), Some((Condition::FileConflict, "D")));
        conditions.slot(Condition::FileConflict).take();
        assert_eq!(conditions.event(), ConditionEvent::default());
    }

    /// The wire spellings the window switches on. A drift here is a strip that
    /// never appears, which is exactly the class of bug a shim cannot catch.
    #[test]
    fn condition_names_match_the_window() {
        assert_eq!(Condition::HotkeyDead.as_str(), "hotkeyDead");
        assert_eq!(Condition::NoDevice.as_str(), "noDevice");
        assert_eq!(Condition::FileConflict.as_str(), "fileConflict");
    }

    /// Nothing is published until something holds, and `null` is the whole of
    /// "nothing is wrong" — a fresh shell must not raise a strip.
    #[test]
    fn a_fresh_shell_publishes_no_condition() {
        let (state, _channels) = test_state();
        assert_eq!(state.snapshot().condition, ConditionEvent::default());
    }

    /// A conflict says what happened to the work, names the file, and says
    /// plainly that nothing was written — the design's three requirements for
    /// every failure sentence.
    #[test]
    fn the_conflict_sentence_says_nothing_was_written() {
        let detail = conflict_detail(Some("C:\\notes\\session 04.md"));
        assert!(detail.contains("C:\\notes\\session 04.md"), "{detail}");
        assert!(detail.contains("nothing has been written"), "{detail}");
        // A save with no bound path yet still gets a readable sentence.
        assert!(conflict_detail(None).starts_with("That file"));
    }

    /// The readout that was once missing: whether a mouse hook is
    /// installed, in words, from the same `needs_mouse` that decides it. A
    /// keyboard-only pair must never claim the mouse path.
    #[test]
    fn the_hook_scope_line_names_what_is_hooked() {
        let f13: Binding = "F13".parse().expect("F13 parses");
        let side: Binding = "MouseX2".parse().expect("MouseX2 parses");

        let keyboard = hook_scope_line(Bindings::ptt_only(f13));
        assert!(keyboard.starts_with("hook: keyboard only"), "{keyboard}");
        assert!(keyboard.contains("F13"), "{keyboard}");

        let mouse = hook_scope_line(Bindings::new(f13, side));
        assert!(mouse.starts_with("hook: keyboard + mouse"), "{mouse}");
        assert!(mouse.contains("MouseX2"), "{mouse}");

        // Either binding being a mouse button is enough, which is the rule
        // `needs_mouse` applies — restated here so a change to it fails loudly.
        let toggle_only = hook_scope_line(Bindings::toggle_only(side));
        assert!(
            toggle_only.starts_with("hook: keyboard + mouse"),
            "{toggle_only}"
        );
    }

    /// The readout follows the *engine*, not the config: a revert leaves the
    /// old device named, and a total failure says plainly that there is none.
    #[test]
    fn the_device_readout_names_what_the_engine_ended_up_on() {
        use sotone_core::audio::AudioError;

        assert_eq!(
            ready_device(&Reconnect::Switched {
                device: "Yeti".to_owned()
            }),
            "Yeti"
        );
        assert_eq!(
            ready_device(&Reconnect::Reverted {
                device: "Stub".to_owned(),
                error: AudioError::NoInputDevice,
            }),
            "Stub"
        );
        assert_eq!(
            ready_device(&Reconnect::Lost {
                error: AudioError::NoInputDevice,
                revert_error: AudioError::NoInputDevice,
            }),
            NO_MICROPHONE
        );
    }

    /// A failed model load leaves the previous model running, and the message
    /// has to say so: the config was never written (the load happens first,
    /// precisely so there is nothing to roll back), so nothing about the app
    /// has changed and the user needs to know that.
    #[test]
    fn a_failed_model_load_says_the_previous_model_is_still_running() {
        let message = swap_failure(
            "ggml-large-v3-turbo.bin",
            &sotone_core::transcribe::TranscribeError::EmptyUtterance,
        );
        assert!(message.contains("ggml-large-v3-turbo.bin"), "{message}");
        assert!(
            message.contains("still running the previous model"),
            "{message}"
        );
    }

    // -----------------------------------------------------------------------
    // Search
    //
    // The file walk itself is `sotone_core::draft`'s (the matcher is pure and
    // tested there, over real torn-tail drafts). What is pinned here is the
    // shape the window destructures and the one rule that lives on this side
    // of the boundary.
    // -----------------------------------------------------------------------

    fn hit(id: &str, spoken_at: &str, text: &str) -> SearchLineDto {
        SearchLineDto {
            id: id.to_owned(),
            spoken_at: spoken_at.to_owned(),
            text: text.to_owned(),
        }
    }

    /// A note's `matches` is a count of matching **lines**, and the outcome's
    /// is their sum. A line that says the term three times is one match on both
    /// counters, which is what keeps the tree's per-note number and the rows
    /// the pane lists from disagreeing.
    #[test]
    fn search_counts_are_lines_and_the_total_is_their_sum() {
        let term = SearchTerm::parse("  Table ").expect("term");
        // Echoed normalised, so a window holding two answers can tell them
        // apart by the term they were for.
        assert_eq!(term.as_str(), "table");

        let one = search_note(
            "DRAFT-A".to_owned(),
            vec![
                hit("01A", "14:31:40", "the table redraws twice, table table"),
                hit("01B", "14:34:38", "third pass on the table"),
            ],
            "2026-08-05T14:34:38+02:00".to_owned(),
        );
        assert_eq!(one.matches, 2);
        let two = search_note(
            "DRAFT-B".to_owned(),
            vec![hit("01C", "11:20:00", "the table header sticks")],
            "2026-08-05T11:20:00+02:00".to_owned(),
        );
        assert_eq!(two.matches, 1);

        let outcome = search_outcome(&term, vec![one, two]);
        assert_eq!(outcome.term, "table");
        assert_eq!(outcome.matches, 3);
        assert_eq!(outcome.notes.len(), 2);
        assert_eq!(outcome.notes[0].draft_id, "DRAFT-A");
        assert_eq!(outcome.notes[0].lines[0].spoken_at, "14:31:40");
        assert_eq!(outcome.notes[0].lines[0].id, "01A");
    }

    /// Search-off, and a store that is not there yet, answer with the same
    /// empty shape — never with everything, and never with an error the window
    /// would have to say out loud.
    #[test]
    fn the_search_off_answer_is_an_empty_outcome() {
        assert!(SearchTerm::parse("   ").is_none());
        let empty = SearchOutcome::default();
        assert_eq!(empty.term, "");
        assert_eq!(empty.matches, 0);
        assert!(empty.notes.is_empty());
    }

    /// The fallback rule: a note with no live line has never been written to,
    /// so `last_written` is when it was started. The other half — which live
    /// line is the newest — is `sotone_core::draft::last_written`'s test.
    #[test]
    fn last_written_falls_back_to_the_drafts_creation_time() {
        let created = "2026-08-01T09:00:00+02:00".to_owned();
        let spoken = "2026-08-05T14:39:02+02:00".to_owned();

        assert_eq!(written_at(None, created.clone()), created);
        assert_eq!(written_at(Some(spoken.clone()), created), spoken);
    }

    /// The whole close rule as a truth table. Eight rows,
    /// because there are three facts and the ordering between them is the part
    /// that can be got wrong.
    #[test]
    fn the_x_hides_only_when_all_three_facts_allow_it() {
        use CloseAction::{Hide, Quit};

        // onboarded, close_quits, tray_alive → what the X does.
        let table = [
            (true, false, true, Hide),
            // The escape hatch: no tray to hide to.
            (true, false, false, Quit),
            // The setting is the consent, and a live tray does not override it.
            (true, true, true, Quit),
            (true, true, false, Quit),
            // The wizard. A first launch that is closed should be gone,
            // whatever the other two say — including a `close_quits` that a
            // seeded config file could already carry.
            (false, false, true, Quit),
            (false, false, false, Quit),
            (false, true, true, Quit),
            (false, true, false, Quit),
        ];

        for (onboarded, close_quits, tray_alive, want) in table {
            assert_eq!(
                close_action(onboarded, close_quits, tray_alive),
                want,
                "onboarded={onboarded} close_quits={close_quits} tray_alive={tray_alive}"
            );
        }
    }

    /// Hiding is the one outcome that needs every fact to line up; each of the
    /// three on its own is enough to leave. Stated separately from the table
    /// above because this is the sentence the rule makes, and a table can be edited
    /// into agreeing with a bug.
    #[test]
    fn any_one_reason_is_enough_to_quit() {
        assert_eq!(close_action(true, false, true), CloseAction::Hide);
        assert_eq!(close_action(false, false, true), CloseAction::Quit);
        assert_eq!(close_action(true, true, true), CloseAction::Quit);
        assert_eq!(close_action(true, false, false), CloseAction::Quit);
    }

    // -----------------------------------------------------------------------
    // Creating a project is the Godot dialog
    //
    // The one group of tests in this file that touches a filesystem, because
    // the whole rule is about what is on disk and when. Everything happens
    // under `std::env::temp_dir()` in a directory named after this process; no
    // config file is read, no `AppHandle` exists, and nothing outside the temp
    // tree is created, read or removed.
    // -----------------------------------------------------------------------

    /// A directory this test alone owns, gone when the guard drops.
    ///
    /// Hand-rolled rather than `tempfile`: this crate has no dev-dependencies
    /// and no new one is authorised. The `remove_dir_all` below is the
    /// only one in this file and it can only ever reach a path this constructor
    /// built under the OS temp directory — invariant 4 has nothing here.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(label: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "sotone-projdir-{}-{label}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // A previous run that was killed before its guard dropped.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the OS temp directory is writable");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// **Immediately**, which is the word the rule uses: the folder is there before
    /// the config write, never at the first save. A project the user can see in
    /// the tree has a folder they can find in the file manager.
    #[test]
    fn creating_a_project_makes_its_folder_before_anything_is_written() {
        let tree = TempTree::new("create");
        let dir = tree.path().join("Ludo");
        assert!(!dir.exists(), "nothing is there before the create");

        prepare_project_folder(false, "Ludo", &dir).expect("a fresh folder under a temp directory");

        assert!(
            dir.is_dir(),
            "the project's folder exists before the config write, not at the first save"
        );
    }

    /// The create-subfolder switch in its **off** position, arriving for free:
    /// the picked folder is an existing home, and pointing a project at it is a
    /// no-op — never a failure, and never a touch on what is already inside it
    /// (invariant 4).
    #[test]
    fn pointing_a_project_at_an_existing_folder_changes_nothing_in_it() {
        let tree = TempTree::new("existing");
        let dir = tree.path().join("Notes");
        std::fs::create_dir_all(dir.join("last month")).expect("seed a folder with history");
        let note = dir.join("session 1.md");
        std::fs::write(&note, "a line the user dictated").expect("seed a note");

        prepare_project_folder(false, "Notes", &dir).expect("an existing folder is a no-op");

        assert!(dir.is_dir());
        assert!(dir.join("last month").is_dir());
        assert_eq!(
            std::fs::read_to_string(&note).expect("the note is still there"),
            "a line the user dictated"
        );
    }

    /// The reason the name check moved ahead of the folder step: a create that
    /// is going to be refused must not have made a directory on its way to
    /// being told so, or every mistyped duplicate leaves an empty folder in the
    /// user's notes root.
    #[test]
    fn a_refused_duplicate_name_leaves_no_folder_behind() {
        let tree = TempTree::new("duplicate");
        let dir = tree.path().join("Ludo");

        let refusal =
            prepare_project_folder(true, "Ludo", &dir).expect_err("that name is already taken");

        assert_eq!(refusal, "there is already a project called \"Ludo\"");
        assert!(
            !dir.exists(),
            "the refusal was decided before the filesystem step"
        );
    }

    /// The two words the window switches on. A refusal resolves like an
    /// acceptance does, so this string is the only thing standing between a
    /// refused create and a form that clears itself — a variant renamed
    /// without a thought for the frontend fails here.
    ///
    /// Serialized through [`tauri::ipc::IpcResponse`], which is the JSON path
    /// this crate already has: it is the blanket impl `#[tauri::command]`
    /// itself hands the return value to, so this asserts the derive by the
    /// route the answer really travels. (`serde_json` is deliberately not a
    /// dependency here — the phase test records the same call.)
    #[test]
    fn the_create_outcome_is_the_word_the_window_switches_on() {
        use tauri::ipc::{InvokeResponseBody, IpcResponse};

        fn wire(outcome: CreateOutcome) -> String {
            match outcome.body().expect("a unit variant always serializes") {
                InvokeResponseBody::Json(json) => json,
                InvokeResponseBody::Raw(_) => {
                    panic!("a command answer is JSON, never a byte payload")
                }
            }
        }

        assert_eq!(wire(CreateOutcome::Created), "\"created\"");
        assert_eq!(wire(CreateOutcome::Refused), "\"refused\"");
    }

    /// A rename that meets no folder renames the configuration anyway and says
    /// which half did not happen. This once reached `fs::rename`, which
    /// failed and aborted a rename that had nothing wrong with it — older
    /// projects have no folder until their first save.
    ///
    /// Both halves are pinned here, because "renames the configuration anyway"
    /// is the half a reader has to take on trust otherwise.
    #[test]
    fn a_rename_whose_folder_is_missing_keeps_the_config_rename() {
        let tree = TempTree::new("rename-missing");

        assert_eq!(
            folder_step(FolderPlan::Rename {
                from: tree.path().join("Ludo"),
                to: tree.path().join("Ludo 2"),
            }),
            FolderStep::Keep(FolderKept::NoFolder)
        );

        // A `Keep` step is what makes the command pass `None`, so this is the
        // config half exactly as `project_rename` performs it.
        let mut config = Config::default();
        config
            .projects
            .push(Project::new("Ludo", tree.path().join("Ludo")));
        config
            .projects
            .push(Project::new("Studio", tree.path().join("Studio")));
        config
            .rename_project("Ludo", "Ludo 2", None)
            .expect("the project is there and the new name is free");

        let renamed = config
            .project("Ludo 2")
            .expect("the rename went through in the configuration");
        assert_eq!(
            renamed.notes_dir,
            tree.path().join("Ludo"),
            "no folder moved, so the binding must still point where it did"
        );
        assert!(
            config.project("Ludo").is_none(),
            "the old name is gone, not kept alongside the new one"
        );
        let other = config
            .project("Studio")
            .expect("the other project is untouched");
        assert_eq!(other.notes_dir, tree.path().join("Studio"));
        assert_eq!(config.projects.len(), 2, "a rename never adds a project");

        // And the sentence the footer gets for the half that did not happen.
        assert_eq!(
            FolderKept::NoFolder.note(),
            "folder kept — there is no folder of its own to rename"
        );
    }

    /// The order of the two probes. Occupied is still asked first, exactly as
    /// it always has been — a target that is already there is the more serious
    /// answer, and it must not be shadowed by a source that is not.
    #[test]
    fn an_occupied_target_is_answered_before_a_missing_source() {
        let tree = TempTree::new("rename-occupied");
        let next = tree.path().join("Ludo 2");
        std::fs::create_dir_all(&next).expect("something is already sitting at the target");

        assert_eq!(
            folder_step(FolderPlan::Rename {
                from: tree.path().join("Ludo"),
                to: next,
            }),
            FolderStep::Keep(FolderKept::Occupied)
        );
    }

    /// And the case the guard must not have broken: a folder that is really
    /// there, with a free target, still moves — and a plan that was already a
    /// `Keep` passes through with its reason intact.
    #[test]
    fn a_folder_that_is_there_still_travels_with_the_name() {
        let tree = TempTree::new("rename-move");
        let dir = tree.path().join("Ludo");
        std::fs::create_dir_all(&dir).expect("the project's own folder");
        let next = tree.path().join("Ludo 2");

        assert_eq!(
            folder_step(FolderPlan::Rename {
                from: dir.clone(),
                to: next.clone(),
            }),
            FolderStep::Move {
                from: dir,
                to: next
            }
        );
        assert_eq!(
            folder_step(FolderPlan::Keep(FolderKept::Shared)),
            FolderStep::Keep(FolderKept::Shared)
        );
    }
}
