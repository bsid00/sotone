// The main window: title bar, project tree, transcript.
//
// No framework, no bundler, no npm: `withGlobalTauri` puts the two APIs this
// page needs on `window.__TAURI__`, and everything else here is the DOM.
//
// The page is a pure *view*. It flips one boolean over IPC (`set_armed`) and
// otherwise only renders what the control thread emits — it cannot start a
// recording, it cannot touch the microphone, and there is no API in this window
// that can generate a keystroke or a mouse event (invariant 1). Nothing is
// fetched from anywhere (invariant 3).
//
// It also drives its own window frame — minimize, maximize and
// close, from the caption buttons in our own title bar, because the window is
// undecorated. Those three and nothing more: no `setFocus`, no `show`, no
// raise-to-front anywhere in this file, and the capability file grants none of
// them either (invariant 2).
//
// The window is laid out to the design: the one-line
// top bar is gone, its indicator, key hint and Enabled/Disabled moved into the
// title bar, the sidebar became the design's 236px tree with a Settings footer,
// and the pane became header · transcript card · footer. The same was done
// to Settings: it is now a shell of its own — 190px of tabs and a pane of cards
// — swapped in below the title bar. Nothing about the *contracts* moved with
// either — every command is still send-only, every render still comes from an
// event, and every popup still tears down when its subject changes.
//
// The notices list is buried, and what it carried is split three ways:
//
// * A **condition** is a state that holds — the hook is dead, the microphone is
//   gone, a save was stopped by an external edit. It arrives on
//   `sotone://condition`, it is rendered and never *inferred*: nothing in this
//   file decides a condition from a notice's arrival, because that is the
//   stale-snapshot family and because a shim cannot catch a backend that
//   forgot to emit. One at a time, from one field, with the precedence resolved
//   in Rust.
// * A **refusal or a one-off failure** is one line in the pane footer's message
//   slot: latest wins, no history, no timer. It is invented surface — see
//   `setMessage` — and it exists because the two obvious alternatives are the
//   two things the design rules out by name.
// * Everything else — confirmations whose result is already on screen, skipped
//   lines, lifecycle traffic — goes only to the **debug log**, an in-memory
//   ring shown by an off-by-default toggle in Settings → About. Nothing about
//   it touches disk.
//
// The routing is the notice's `level` and nothing else: `warn`/`error` reach
// the user, `info`/`debug` reach the log. There is no list of message strings
// anywhere in this file deciding where something goes.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
// The window API: the main window is undecorated, so the three
// caption buttons in our own title bar are the only way to minimize, maximize
// or close it. Read lazily — see `appWindow()` — so a missing shim breaks three
// buttons rather than the whole page.
const windowApi = window.__TAURI__.window;

const EVENT = {
  status: "sotone://status",
  armed: "sotone://armed",
  recording: "sotone://recording",
  line: "sotone://line",
  lines: "sotone://lines",
  // Everything the worker is holding that is not on disk yet:
  // queued, being transcribed, or held because a write failed. A whole
  // snapshot every time, like `lines` — never a delta.
  pending: "sotone://pending",
  notice: "sotone://notice",
  // A state, not a message: the backend publishes which condition
  // holds and this window renders it.
  condition: "sotone://condition",
  drafts: "sotone://drafts",
  save: "sotone://save",
  saveAll: "sotone://save-all",
  // The one event in the table that is a *question*: a drop landed
  // on a name the target folder already has, and nothing was written. The
  // window's two answers are the same drop with `keepBoth`, or nothing.
  noteClash: "sotone://note-clash",
  projects: "sotone://projects",
  settings: "sotone://settings",
  // The one thing outside this window that can ask it to show a
  // view: the tray's Settings item. A request, not state — it is never in the
  // startup snapshot, so a reload cannot re-open Settings because the tray
  // asked ten minutes ago.
  view: "sotone://view",
};

// How many entries the debug log keeps. Big enough to hold a startup plus a
// dictation session, small enough that a window left open for a day cannot
// grow without bound. In memory only — nothing here is written to disk.
const MAX_LOG = 200;

// How long a "discard" click stays armed before it forgets it was clicked.
// Long enough to move the mouse a few pixels, short enough that a click found
// again ten minutes later is not still primed.
const CONFIRM_WINDOW_MS = 4000;

const el = (id) => document.getElementById(id);

const views = {
  loading: el("view-loading"),
  ready: el("view-ready"),
  // The first-run panel: a fourth top-level view, not a variation
  // of the fatal one. No model is a designed state, not a failure.
  empty: el("view-empty"),
  // The wizard: the fifth, and the *other* reading of a machine
  // with no model. The panel above is the repair surface for an install that
  // lost its weights; this is the seven steps a new install walks, in whatever
  // phase the launch landed in.
  onboarding: el("view-onboarding"),
  fatal: el("view-fatal"),
};

// What the notes shell's main area is showing: the transcript, one project's
// fields, or the new-project fields. The mechanism is `data-current`, never the
// `hidden` attribute, because the rule that shows a pane sets `display` and an
// author display rule beats the UA sheet's `[hidden] { display: none }`.
//
// Settings is not one of these: it is a whole shell of its own —
// see `showSettings` — because the design gives it its own sidebar.
const PANES = ["pane-session", "pane-project", "pane-new-project"];

let currentPane = "pane-session";

function showPane(id) {
  const leaving = currentPane !== id;
  currentPane = id;
  // A transient overlay does not survive a change of what is on screen.
  // Unconditional: `closeMenu` is silent when nothing is open.
  closeMenu();
  for (const pane of PANES) {
    el(pane).dataset.current = pane === id ? "yes" : "no";
  }
  // A pane switch tears an open rename field down, and it reverts:
  // the field's own footer statement lives in the pane that is going away.
  if (leaving) cancelRename();
  // Showing a notes pane *is* leaving Settings: every caller of this is a
  // "take me to the notes" act (a new note, a created project, a Back).
  showSettings(false);
}

// ---------------------------------------------------------------------------
// The settings view
//
// The design draws Settings as a separate window. It is an in-window view swap
// here, deliberately: every piece of state this app has arrives
// as an event into one webview, and a second window would duplicate every
// listener and the capability surface for no user gain — the drawn "Back to
// notes" row reads identically either way. The swap replaces the whole body
// below the title bar, sidebar included, exactly as the design's shell does.
//
// It is DOM only: nothing here shows, activates, raises or focuses a window,
// and the capability file grants none of those anyway (invariant 2). Because
// the notes shell is only `display`-swapped, the tree's collapse state, its
// active row and whichever pane was open all survive the round trip untouched;
// the one thing `display: none` does throw away is the scroll offset, which is
// why it is put back by hand below.
// ---------------------------------------------------------------------------

const TABS = ["general", "recording", "transcription", "overlay", "about"];

const TAB_NAME = {
  general: "General",
  recording: "Recording",
  transcription: "Transcription",
  overlay: "Overlay",
  about: "About",
};

// Which tab was last looked at. This app session only, deliberately: nothing
// about which settings tab you read belongs in storage, and a fresh launch
// opens on General the way a settings *window* would.
let currentTab = "general";

let settingsOpen = false;

// Where the tree was scrolled to when Settings took the screen.
let treeScroll = 0;

function showSettings(open) {
  if (open === settingsOpen) return;
  settingsOpen = open;
  // The view is swapping under it.
  closeMenu();
  // Leaving the notes shell tears an open rename field down, reverting it —
  // the field and the footer that states what it will change
  // are both in the view being swapped away.
  if (open) cancelRename();
  const tree = el("draft-list");
  if (open) treeScroll = tree.scrollTop;
  el("view-ready").dataset.view = open ? "settings" : "notes";
  if (open) {
    // Devices are enumerated on opening Settings, not at startup: a headset
    // plugged in five minutes ago has to be in the list.
    loadDevices();
  } else {
    // A hidden box has no scroll offset to keep, so the one it had is restored
    // rather than left at the top — coming back from Settings must land the
    // user where they were in the tree.
    tree.scrollTop = treeScroll;
  }
}

function showTab(name) {
  const previous = currentTab;
  currentTab = TABS.includes(name) ? name : "general";
  el("pane-settings").dataset.tab = currentTab;
  el("settings-tab-name").textContent = TAB_NAME[currentTab];
  for (const tab of el("settings-tabs").querySelectorAll(".tab")) {
    tab.dataset.selected = tab.dataset.tab === currentTab ? "yes" : "no";
  }
  // The models list is a directory read, not a subscription: the user
  // manages the folder by hand and there is no watcher, so a tab that lists
  // models re-reads it on arrival — the same reason `showSettings` enumerates
  // devices on opening rather than at startup. The Rescan button beside the
  // folder is still the guarantee; this only removes the common case of
  // needing it. Never on the boot call (`previous` is already `general`).
  if (currentTab === "transcription" && previous !== "transcription") {
    rescanModels();
  }
}

// Which kinds of live event have already arrived. The startup snapshot is
// fetched after the listeners are attached, so it may be older than an event
// that landed in between; it is only applied where nothing newer exists.
const seen = new Set();

let armButton = el("arm");
let userArmed = true;

// The last status, kept because two surfaces need more of it than the moment it
// arrived: the wizard's model step reads the phase and the reason, and its
// summary reads whether a restart is still coming.
let lastStatus = null;

function renderStatus(status) {
  lastStatus = status;
  el("status-detail").textContent = status.detail;
  // Routing, in precedence order: **a fatal wins** — a broken
  // install has to say so, and a wizard drawn over one would be a lie. Then the
  // wizard, in whatever phase the launch landed in. Then the phase, exactly as
  // before.
  //
  // Absence reads as onboarded: the wizard is the rare case, and a payload from
  // a backend that predates it must never put an existing user on step one.
  const wizard = status.phase !== "fatal" && status.onboarded === false;
  // Which half of the title bar is on screen. The indicator, the hotkey hint
  // and Enabled/Disabled describe a session, so they say nothing while Sotone is
  // starting, has no model, or has failed. A data attribute rather than the
  // `hidden` attribute, because the rules that show them set `display` — an
  // author display rule beats the UA sheet's `[hidden]`.
  el("titlebar").dataset.phase = status.phase;
  // …and nothing at all during the wizard, even on a machine that launched
  // ready: that screen has not asked about recording yet.
  el("titlebar").dataset.onboarding = wizard ? "yes" : "no";
  views.loading.hidden = wizard || status.phase !== "loading";
  views.ready.hidden = wizard || status.phase !== "ready";
  views.empty.hidden = wizard || status.phase !== "empty";
  views.fatal.hidden = status.phase !== "fatal";
  views.onboarding.hidden = !wizard;

  if (status.phase === "loading") {
    el("loading-step").textContent = status.detail;
  } else if (status.phase === "fatal") {
    el("fatal-message").textContent = status.detail;
  } else if (status.phase === "empty") {
    // The detail is the whole of "why am I looking at this": no model at all,
    // several with none chosen, or a configured one that is missing or was
    // rejected. It is also in the header line; repeated here because this is
    // the panel the user is reading.
    el("empty-reason").textContent = status.detail;
    // The list itself comes from `sotone://settings`, which the backend emits
    // immediately behind this status — and again after every Add.
    renderEmptyModels();
  } else if (status.phase === "ready" && status.ready) {
    renderReady(status.ready);
  }

  // Last, deliberately: the wizard's model step and its summary read the ready
  // readout, and the branch above is what sets it. Rendering the wizard before
  // that would draw one frame of a step that does not know what is loaded.
  showWizard(wizard);
}

// What this process is actually running: the model it loaded, the microphone it
// opened, the backend it was compiled for. Kept because Settings compares it
// against what the *config* now says, which is how a row knows it needs a
// restart — the backend does not guess at that, it publishes both sides.
let lastReady = null;

// The About tab's "This session" card: one row per fact, the value in mono at
// the right — mono is for paths, keys, counts and machine names only
// (the design's rule). Rebuilt whole rather than patched, like every
// other list in this window.
function factList(card, facts) {
  card.replaceChildren();
  for (const [label, value, title] of facts) {
    const { row, main, control } = setRow();
    rowLabel(main, label);
    control.classList.add("set-row__value");
    control.textContent = value;
    if (title) row.title = title;
    card.append(row);
  }
}

function renderReady(ready) {
  lastReady = ready;
  // The glance version, in the pane's footer: what got loaded, on
  // what backend. The long form — microphone, cues, bindings, language — is the
  // facts list in Settings → This session, which reads from this same object,
  // so there is one source and two lengths of it. It stays on screen because a
  // Vulkan build that quietly fell back to CPU is exactly the kind of thing a
  // glance should catch.
  const facts = el("pane-facts");
  facts.textContent = `${ready.model} · ${ready.backend}`;
  facts.title = [
    ready.model_path,
    ready.model_kind,
    `language ${ready.language}`,
    ready.device,
    `keys ${ready.bindings}`,
  ]
    .filter(Boolean)
    .join(" · ");

  const note = el("session-note");
  note.textContent = ready.note || "";
  note.hidden = !ready.note;

  // The readout block and the restart markers both read from this.
  renderSettings(lastSettings);
}

// `user_armed` is the whole of the armed contract — focus does not gate capture,
// so there is nothing here to combine.
//
// The button says the *state*, not the action: "Enabled" while armed, "Disabled"
// while not (a design rule — every surface that reports capture state also
// shows the key that changes it). It is a safety against accidental capture, not
// a record button; there is no record button anywhere in Sotone.
function renderArmed(armed) {
  userArmed = armed.user_armed;
  armButton.textContent = armed.user_armed ? "Enabled" : "Disabled";
  armButton.dataset.armed = armed.user_armed ? "yes" : "no";
  armButton.title = armed.user_armed
    ? "Sotone is listening for your capture keys. Click to turn that off."
    : "Your capture keys do nothing while this is off. Click to turn it on.";
  // One readout, in the title bar, and this is it. It is a function of armed
  // *and* recording, so both renderers drive it.
  renderIndicator();
}

// The elapsed clock, started when the capture event flips to recording. Frontend
// side on purpose: the backend publishes the transition, not a tick, and a
// second's worth of skew on a readout is not worth an event per second.
//
// A window that finished loading *during* a recording counts from the moment it
// heard about it — the snapshot carries `live`, not a start time. On record.
let recordingSince = null;
let elapsedTimer = null;

// One `setInterval` at 1Hz, and only while a recording is live: not a
// requestAnimationFrame loop, which would wake this window sixty times a second
// beside a game under test for a clock that changes once a second. The bars are
// CSS. Nothing here goes anywhere near the input hook (invariant 5).
function startElapsed() {
  recordingSince = Date.now();
  if (elapsedTimer === null) elapsedTimer = setInterval(renderIndicator, 1000);
}

function stopElapsed() {
  recordingSince = null;
  if (elapsedTimer !== null) {
    clearInterval(elapsedTimer);
    elapsedTimer = null;
  }
}

function elapsedLabel() {
  if (recordingSince === null) return "00:00";
  const seconds = Math.max(0, Math.floor((Date.now() - recordingSince) / 1000));
  const minutes = Math.floor(seconds / 60);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(minutes)}:${pad(seconds % 60)}`;
}

// The one indicator that carries all of it: armed and idle, recording, or
// disarmed. Derived from the states this window tracks rather than from the
// recording event alone.
//
// **The indicator tells the truth**: a failure that stops
// capture has to move it, not just raise a strip. So a condition that means
// "Sotone cannot record at all" reads `REC OFF` here whatever the armed flag
// says — for `noDevice` the backend has already disarmed, and for `hotkeyDead`
// it deliberately has not (the mic is fine; nothing can reach it), which is
// exactly the case that would otherwise sit there claiming to be armed.
//
// The order is the truth order: a live recording wins, because audio really is
// being captured — the hook can die mid-utterance and the clip is still real.
//
// The state travels as a data attribute, never as the `hidden` attribute: `.rec`
// sets `display`, and an author display rule wins over the UA sheet's
// `[hidden] { display: none }` — which is why the indicator used to sit there
// permanently no matter what this function set. CSS owns the appearance, the
// 160ms colour crossfade and the bars; this only names the state and the clock.
function renderIndicator() {
  const indicator = el("recording");
  const label = el("recording-label");

  if (recordingLive) {
    indicator.dataset.state = "recording";
    indicator.title = recordingSource
      ? `recording · ${recordingSource}`
      : "recording";
    label.textContent = `REC ${elapsedLabel()}`;
    return;
  }
  if (cannotRecord()) {
    indicator.dataset.state = "disarmed";
    indicator.title = conditionState.detail || "Sotone cannot record right now.";
    label.textContent = "REC OFF";
    return;
  }
  indicator.dataset.state = userArmed ? "armed" : "disarmed";
  indicator.title = userArmed
    ? "Ready. Press a capture key to start recording."
    : "Off. Your capture keys do nothing until Sotone is enabled.";
  label.textContent = userArmed ? "REC 00:00" : "REC OFF";
}

function renderRecording(recording) {
  recordingSource = recording.source;
  const changed = recordingLive !== recording.live;
  recordingLive = recording.live;
  if (changed) {
    if (recordingLive) startElapsed();
    else stopElapsed();
    // The sidebar's live dot is one attribute on the tree, not a per-row
    // render: CSS shows it on whichever row is active, so the dot cannot be
    // left behind on a row that stopped being the one lines land in.
    el("draft-list").dataset.recording = recordingLive ? "yes" : "no";
  }
  renderIndicator();

  // Editing is post-session. A live recording closes any open editor
  // and greys the controls rather than letting a line be retyped while the
  // user is still speaking into the same draft.
  if (changed) {
    if (recordingLive) cancelEdit();
    // The menu goes too: five of its items are about to become
    // refusals, and a menu that changes what it offers under the cursor is
    // worse than one that closes.
    if (recordingLive) closeMenu();
    // And an open dropdown, for the same reason and in the same breath:
    // several of the nine are about to be disabled with a reason in
    // their `title`, and a list still on screen over one of them is offering
    // what the control beneath it has stopped offering. It closes silently and
    // commits nothing.
    if (recordingLive) closeDrop();
    // The delete question goes the same way as the editor, and for the same
    // reason: it is an editing act, and editing is post-session.
    // The selection *stays* — it destroys nothing on its own, and Delete
    // N disables itself with the reason in its tooltip while the mic is live.
    if (recordingLive) pendingDelete = null;
    // An open rename field goes too, and it **reverts**: the
    // command behind it is refused while a recording runs, and a rename is
    // heavier than a text edit, so a teardown may never be a quiet commit.
    // `renderDrafts` below is what takes the field off the screen.
    if (recordingLive) renaming = null;
    // Same teardown rule as the conflict panel: a popup that
    // relaxes a guard has to retract when the state it was asked about moves.
    // A recording going live means the note it was asking about is growing.
    if (recordingLive) closeNoProject();
    // The clash question goes with it, and for a second reason as
    // well: the move behind it is refused while a recording runs, so Keep both
    // would answer a question with a notice. Retracting is honest; the drop is
    // one gesture away once the recording stops.
    if (recordingLive) closeNoteClash();
    // And the move chooser, for exactly that second reason: moving
    // lines is editing, and editing is post-session. It closes **silently** and
    // sends nothing — the selection stays, and the toolbar's own button is
    // already saying why it is greyed.
    if (recordingLive) closeMoveChooser();
    // **A recording no longer tears the conflict down** — the design's rule is
    // "recording still works — new lines queue behind the decision". What
    // happens instead is smaller and truer: Keep mine goes inert while the
    // recording runs — the structural mirror of the backend's own refusal —
    // and the diff, if it is open, marks its right-hand side as older than the
    // note once a line lands.
    renderCondition();
    renderTranscript(transcript);
    // The tree's per-project `+` is gated on the same rule (see `newNoteIn`),
    // so it has to be redrawn when the rule changes — otherwise it stays inert
    // after a hold too short to produce a line, which is the one path that
    // ends a recording without a drafts event behind it.
    renderDrafts(lastDrafts);
    // Saving is gated on the same rule, for the same reason: a note rendered
    // mid-utterance is missing the line being spoken.
    renderSaveState();
  }
}

// A line landed. The line itself is already on its way as a transcript event —
// this only moves the counter in the pane header, which is the readout the
// design gives that number. The last line is no longer echoed anywhere in this
// window: the transcript *is* the surface, and the overlay is what reads the
// newest line back while the user is looking at something else.
function renderLine(line) {
  el("line-count").textContent = countLabel(line.n);
}

// What to hold, named from the settings event. A hint that names the wrong key
// is worse than a generic one, so the fallback stays generic.
//
// One binding, on purpose: its callers are prose (the empty transcript, the
// invite) where a second sentence would be noise, and push-to-talk is the
// gesture Sotone is built around. The title-bar tooltip needs every enabled
// binding instead and composes its own — `hotkeyTooltip()` below.
function holdHint() {
  const ptt = lastSettings.ptt;
  if (ptt && ptt.enabled && ptt.label) {
    return `Hold ${ptt.label} · speak one finding · release`;
  }
  const toggle = lastSettings.toggle;
  if (toggle && toggle.enabled && toggle.label) {
    return `Press ${toggle.label} · speak one finding · press again`;
  }
  return "Hold your push-to-talk key · speak one finding · release";
}

// The tooltip on that hint: one sentence per *enabled* binding, each naming its
// own action. `holdHint()` cannot do this job — it prefers push-to-talk and
// returns one sentence, so with only toggle on it named the wrong gesture and
// with both on it told half the truth. Two bindings mean
// two sentences; a newline in a `title` renders as two lines.
function hotkeyTooltip() {
  const lines = [];
  const ptt = lastSettings.ptt;
  const toggle = lastSettings.toggle;
  if (ptt && ptt.enabled && ptt.label) {
    lines.push(`Hold ${ptt.label} · speak one finding · release`);
  }
  if (toggle && toggle.enabled && toggle.label) {
    const key = toggle.label;
    lines.push(`Press ${key} · speak · press ${key} again to stop`);
  }
  // Nothing bound, or nothing named: say so rather than name a default that may
  // be wrong.
  return lines.length > 0 ? lines.join("\n") : "No key is bound yet";
}

// The title bar's key hint: the shortest true form of the same thing. Both
// bindings can be on at once, and both are worth naming — the mono readout is
// the one place a user checks what the keys are without opening Settings.
function renderHotkeyHint() {
  const parts = [];
  const ptt = lastSettings.ptt;
  const toggle = lastSettings.toggle;
  if (ptt && ptt.enabled && ptt.label) parts.push(ptt.label);
  if (toggle && toggle.enabled && toggle.label) parts.push(toggle.label);
  const hint = el("hotkey-hint");
  hint.textContent = parts.join(" · ");
  hint.title = hotkeyTooltip();
}

// The pane header's span: the first and last timestamps in the note, and "now"
// while it is still being spoken into. Read from the transcript the backend
// last sent, never from the DOM.
function renderSpan() {
  const lines = transcript.lines;
  const span = el("line-time");
  if (lines.length === 0) {
    span.textContent = "";
    return;
  }
  const first = lines[0].spoken_at;
  const last = recordingLive ? "now" : lines[lines.length - 1].spoken_at;
  span.textContent = first === last ? first : `${first} → ${last}`;
}

// ---------------------------------------------------------------------------
// The transcript panel — post-session tidy-up
//
// The backend sends the whole folded transcript after anything that changes it,
// so this panel never patches a list: it re-renders from the snapshot it was
// given. Every mutation is a send-only command; the list that comes back is the
// only confirmation, which is why nothing here optimistically rewrites a line.
// ---------------------------------------------------------------------------

// The last transcript the backend sent.
let transcript = { draft_id: null, lines: [] };

// Whether soft-deleted lines are kept out of the transcript. Permanent
// per-line deletion was declined — the store is append-only and that is the
// promise — so what the clutter complaint gets instead is a *view* filter: it
// removes nothing, and what it writes is one word in the config.
// One global flag rather than a per-note one, and remembered across launches
// as `hide_deleted` in the config:
// this is the live copy, seeded from every settings payload and flipped
// optimistically by the toggle. It is still a view preference — no line, no
// note and no file on disk changes when it moves.
let hideDeleted = false;

// The last pending snapshot: rows for utterances that are not lines
// yet. Held straight as the backend sent it — the worker owns this queue, and
// nothing in this window adds to it, removes from it, or reorders it.
let pendingLines = [];

// The pending rows this note should be showing. Queued and transcribing rows
// carry no draft: they land in whatever note is active when the model gets to
// them, which is today's semantics made visible. A *held* row is pinned, so it
// waits for its own note rather than appearing under someone else's.
function visiblePending() {
  const mine = pendingLines.filter(
    (row) => !row.draft_id || row.draft_id === transcript.draft_id,
  );
  // `HH:MM:SS` sorts chronologically as a string, which is the whole reason
  // the backend formats it there and not here.
  mine.sort((a, b) => (a.spoken_at === b.spoken_at ? 0 : a.spoken_at < b.spoken_at ? -1 : 1));
  return mine;
}

// How many lines are spoken, transcribed, and still not in the note because the
// disk refused them. The footer counts these and Save says the file is behind
// by exactly this many.
function heldCount() {
  return visiblePending().filter((row) => row.status === "held").length;
}

function applyPending(payload) {
  pendingLines = Array.isArray(payload.pending) ? payload.pending : [];
  // The rows live in the transcript list, so this is a transcript render — and
  // the footer count and Save's `--warn` follow from the same snapshot.
  renderTranscript(transcript);
  renderSaveState();
}

// Undo/redo, one pair of stacks per draft id. Frontend-only: every
// inverse is one of the same commands the user could click by hand, so nothing
// new touches disk and undo can never delete a dictated line (invariant 4).
//
// A step is `{ undo, redo }`, each an `{ name, args }` invoke **or an array of
// them** (a selection delete is one action, and nobody presses
// Ctrl+Z three times to take one action back). Steps are pushed only once the
// forward command has been accepted, so a rejected edit leaves no phantom
// history. The stacks live for the app session only — a restart starts a fresh
// tidy-up pass.
const undoStacks = new Map();

function stacksFor(draftId) {
  let stacks = undoStacks.get(draftId);
  if (!stacks) {
    stacks = { undo: [], redo: [] };
    undoStacks.set(draftId, stacks);
  }
  return stacks;
}

// The line being dragged, or null. Nothing here reorders the list optimistically:
// this is only used to work out what to send.
let dragging = null;

// Lines waiting for a re-transcribe to come back.
const pendingRetranscribe = new Set();
// What those lines read *before* the re-transcribe, so the undo step can be
// decided when the result lands: a re-transcribe that produced the same words
// must not consume a Ctrl+Z.
const retranscribeWas = new Map();

// The line currently open for editing, and the text it had when it opened, so
// Escape can put it back.
let editing = null;
// A transcript that arrived while an editor was open. Applying it immediately
// would rip the contenteditable node out from under the caret.
let deferredTranscript = null;

let recordingLive = false;
// Which binding started the live recording ("hold", "toggle", …), kept so the
// readout can name it. Only shown while live.
let recordingSource = "";

// One context for the whole session, created on the first click: Web Audio,
// not a data: URL on an <audio> element, because the CSP is `default-src
// 'self'` and this keeps it that way — no data:, no blob:, no asset protocol.
// The bytes come over IPC from a local file and are decoded in this window.
// Nothing is fetched (invariant 3).
let audioContext = null;

function editingAllowed() {
  return !recordingLive;
}

// One step of history for the active draft, and the redo stack cleared: a new
// action after an undo abandons the redone future, exactly as an editor does.
function pushStep(draftId, step) {
  if (!draftId) return;
  const stacks = stacksFor(draftId);
  stacks.undo.push(step);
  stacks.redo.length = 0;
  renderUndoState();
}

// Both buttons follow the *active* draft's stacks, so switching away and back
// finds the same history waiting.
function renderUndoState() {
  const stacks = transcript.draft_id
    ? stacksFor(transcript.draft_id)
    : { undo: [], redo: [] };
  // `conflictHeld`: the card is out of service until the question is
  // answered, and the design puts every header action `--faint` with it. The
  // colour is CSS's; this is what makes it structural.
  const gated = !editingAllowed() || Boolean(editing) || conflictHeld();
  el("lines-undo").disabled = gated || stacks.undo.length === 0;
  el("lines-redo").disabled = gated || stacks.redo.length === 0;
}

// One side of a step, always as a list. Single-invoke steps are the shape they
// always were — every existing push site is untouched — and a compound one is
// simply longer.
function stepCalls(side) {
  return Array.isArray(side) ? side : [side];
}

// Run one step's command, or its commands in order. Popped first so a second
// Ctrl+Z cannot re-issue the same inverse while this one is in flight; put back
// if a command is rejected, so nothing is silently lost from the history.
//
// A failure part-way through a compound step re-pushes the whole step: its
// sub-operations are idempotent soft-delete flips, so pressing the same button
// again converges rather than double-applying.
async function runStep(from, to) {
  const stacks = transcript.draft_id ? stacksFor(transcript.draft_id) : null;
  if (!stacks || stacks[from].length === 0) return;
  const step = stacks[from].pop();
  renderUndoState();
  try {
    for (const call of stepCalls(step[from])) {
      await invoke(call.name, call.args);
    }
    stacks[to].push(step);
  } catch (err) {
    stacks[from].push(step);
    reportFailure(err);
  }
  renderUndoState();
}

const undoStep = () => runStep("undo", "redo");
const redoStep = () => runStep("redo", "undo");

// Where the caret is, for the shortcut gate: the contenteditable line editor,
// the sort dropdown, anything else that owns Ctrl+Z itself.
function isField(node) {
  if (!node || node.nodeType !== 1) return false;
  if (node.isContentEditable) return true;
  return ["INPUT", "SELECT", "TEXTAREA"].includes(node.tagName);
}

// Same gate as the mouse controls, plus focus: while a line editor is open the
// contenteditable's own native undo has to win, and while a recording is live
// nothing edits at all: editing is post-session.
function shortcutAllowed(target) {
  if (editing || !editingAllowed()) return false;
  return !isField(target) && !isField(document.activeElement);
}

function toolButton(label, title, onClick, extra) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = extra ? `line__tool ${extra}` : "line__tool";
  button.textContent = label;
  button.title = title;
  button.disabled = !editingAllowed();
  button.addEventListener("click", onClick);
  return button;
}

// Reordering a *filtered* list would drop a row between neighbours it cannot
// see, so no row is draggable while a term is active. The grip goes
// with it, in CSS. On record as a deviation from the mock's note that the rows
// under a search are the same ones.
function dragAllowed() {
  return editingAllowed() && !editing && !searching();
}

// Where the dragged line should land, as the `after` the backend takes: the id
// of whatever ends up in front of it, or null for the top of the note.
//
// Computed from the list as the backend last sent it, never from the DOM, so a
// transcript that arrived mid-drag cannot make this name a line that has moved.
function anchorFor(id, targetId, below) {
  const ids = transcript.lines.map((line) => line.id);
  const target = ids.indexOf(targetId);
  if (target < 0) return undefined;
  const insertAt = target + (below ? 1 : 0);
  const before = ids.slice(0, insertAt).filter((other) => other !== id);
  const after = before.length === 0 ? null : before[before.length - 1];

  // Already sitting there. The store refuses to record a no-op move anyway;
  // this just saves the round trip and the re-render.
  const at = ids.indexOf(id);
  const currently = at > 0 ? ids[at - 1] : null;
  return after === currently ? undefined : after;
}

function clearDropMarks() {
  for (const marked of document.querySelectorAll(".line[data-drop]")) {
    delete marked.dataset.drop;
  }
}

function endDrag() {
  dragging = null;
  clearDropMarks();
  for (const held of document.querySelectorAll('.line[data-dragging="yes"]')) {
    delete held.dataset.dragging;
  }
}

// Where a line sits *now*, in the same `after` form the backend takes: the
// inverse of the move about to be sent. Read from the transcript order at drop
// time, before anything is invoked.
function priorAnchor(id) {
  const ids = transcript.lines.map((line) => line.id);
  const at = ids.indexOf(id);
  if (at < 0) return undefined;
  return at > 0 ? ids[at - 1] : null;
}

// One `line_move` per drop, and then nothing: the panel waits for the
// transcript event, exactly as every other mutation in this file does.
//
// `after === undefined` is `anchorFor` saying the line is already there — no
// command, and so no undo step for a drag that changed nothing.
function moveLine(id, after) {
  if (after === undefined) return;
  const draftId = transcript.draft_id;
  const was = priorAnchor(id);
  invoke("line_move", { id, after })
    .then(() => {
      if (was === undefined) return;
      pushStep(draftId, {
        undo: { name: "line_move", args: { id, after: was } },
        redo: { name: "line_move", args: { id, after } },
      });
    })
    .catch(reportFailure);
}

function makeDraggable(item, id) {
  item.draggable = true;

  item.addEventListener("dragstart", (event) => {
    // The state can have changed since this row was rendered — a recording can
    // start, an editor can open — so the gate is checked here too.
    if (!dragAllowed()) {
      event.preventDefault();
      return;
    }
    // A drag is entering another of the mutually exclusive states, so the
    // selection ends here. The rows are un-marked **in place**:
    // rebuilding the list mid-dragstart would throw away the element being
    // lifted, and the drag would die with it.
    if (selection.size > 0 || selectionAnchor !== null) {
      forgetSelection();
      unmarkSelectedRows();
      renderSelection();
    }
    dragging = id;
    item.dataset.dragging = "yes";
    if (event.dataTransfer) {
      event.dataTransfer.effectAllowed = "move";
      // Some engines will not start a drag with an empty payload. It never
      // leaves this window (invariant 3).
      event.dataTransfer.setData("text/plain", id);
    }
  });

  item.addEventListener("dragover", (event) => {
    if (!dragging) return;
    // Only a preventDefault marks a valid drop target; without it the drop
    // never fires. Stopping here too: the list's own handler treats a hover as
    // "the gap under the last row" and would rub this indicator straight out.
    event.preventDefault();
    event.stopPropagation();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    const box = item.getBoundingClientRect();
    const where = event.clientY - box.top > box.height / 2 ? "below" : "above";
    if (item.dataset.drop !== where) {
      clearDropMarks();
      item.dataset.drop = where;
    }
  });

  item.addEventListener("dragleave", (event) => {
    // Crossing into one of the row's own spans is a dragleave on the row; only
    // a leave that really left it should drop the indicator.
    if (item.contains(event.relatedTarget)) return;
    delete item.dataset.drop;
  });

  item.addEventListener("drop", (event) => {
    if (!dragging) return;
    event.preventDefault();
    // The list has its own handler for the gap under the last row.
    event.stopPropagation();
    const held = dragging;
    const below = item.dataset.drop === "below";
    endDrag();
    moveLine(held, anchorFor(held, id, below));
  });

  item.addEventListener("dragend", endDrag);
}

// What a queued row says instead of guessing at the words: what was *heard*.
// The design is explicit that this is a duration and never a prediction.
function heardLabel(seconds) {
  const whole = Math.max(1, Math.round(Number(seconds) || 0));
  return `Audio captured, ${whole} ${whole === 1 ? "second" : "seconds"}`;
}

// The five row states of the design's state model:
// `ok` and `failed` come off a stored line, `queued`,
// `transcribing` and `held` off the pending snapshot. One renderer for all
// five, because they are one grammar with different brightness.
function lineRow(line) {
  // The one row that is not a row: while its delete question is armed, the row
  // *is* the question (the design's rule). Nothing else in it is interactive
  // for as long as it is up.
  if (pendingDelete !== null && pendingDelete.id === line.id && !line.deleted) {
    return confirmRow(line);
  }

  const item = document.createElement("li");
  item.className = "line";
  item.dataset.id = line.id;
  // Reachable from the keyboard: Enter opens this line's editor
  // (the design's Keyboard line) and Space toggles its membership of the
  // selection. Every row is a tab stop, including the inert ones, so tabbing
  // through a transcript never skips a line the eye can see.
  item.tabIndex = 0;
  if (selection.has(line.id)) item.dataset.selected = "yes";
  const status = line.status || "ok";
  // CSS owns every appearance difference between the five (never the
  // `hidden` attribute, and never a colour set from JS).
  item.dataset.status = status;
  // Queued, transcribing and held rows are **not lines yet**: there is nothing
  // on disk for a command to name, so they carry no actions, no editor and no
  // drag. The backend would reject the id anyway; this is the near half of the
  // same rule.
  const waiting =
    status === "queued" || status === "transcribing" || status === "held";
  const failed = status === "failed";
  if (line.deleted) item.dataset.deleted = "yes";
  if (pendingRetranscribe.has(line.id)) item.dataset.pending = "yes";

  // `grip · 74px timestamp · line · actions`. The grip is
  // always in the grid, at zero opacity until the row is pointed at, so
  // hovering a row cannot shift the text sideways.
  const grip = document.createElement("span");
  grip.className = "line__grip";
  grip.setAttribute("aria-hidden", "true");
  for (let dot = 0; dot < 6; dot += 1) {
    grip.append(document.createElement("i"));
  }

  const time = document.createElement("span");
  time.className = "line__time";
  time.textContent = line.spoken_at;

  const body = document.createElement("span");
  body.className = "line__body";

  const text = document.createElement("span");
  text.className = "line__text";
  // A queued or transcribing row has no words and must not pretend to: it says
  // how much audio was heard. A failed row's text is left **empty** on purpose
  // — the sentence about it lives beside the text, so clicking through to the
  // editor opens an empty field rather than one the user has to clear before
  // they can type what they said.
  if (status === "queued" || status === "transcribing") {
    text.textContent = heardLabel(line.seconds);
  } else if (failed) {
    text.textContent = "";
  } else {
    // While a term is active every occurrence in the line is marked with a
    // `--sel` block. Built from text nodes, so a line whose words
    // happen to be markup is still just words.
    setSearchableText(text, line.text);
  }
  // Click the line and it becomes a field where it was, as the design draws
  // it. The old gutter `+` is gone: the text *is* the edit control, which
  // is the shortest path from "that word is wrong" to fixing it. A soft-deleted
  // line is on its way out of the note, so it does not open; nor does a row
  // that is not a line yet.
  //
  // A **modified** click is selection wherever it lands, the text included
  // (on record as a deviation from a strict reading of the design's
  // "click selects"): the row's own handler below takes it, and this one steps
  // aside rather than opening a field the user was trying to select.
  if (!line.deleted && !waiting) {
    text.addEventListener("click", (event) => {
      if (event.shiftKey || event.ctrlKey || event.metaKey) return;
      openEditorFor(line.id);
    });
  }
  body.append(text);

  if (failed) {
    // The one failure the design allows to be red, "because the line itself is
    // the failure". What the model said is in the debug log, not here: no error
    // codes, and one sentence that says what happened to the recording.
    const note = document.createElement("span");
    note.className = "line__failed";
    note.textContent = "Transcription failed — the audio is kept.";
    note.title =
      "Sotone could not transcribe this line. Its audio was kept: Retry runs " +
      "the model over it again, or type what you said and the line is yours. " +
      "The model's own message is in Settings → About → Debug log.";
    text.title = note.title;
    if (!line.deleted) {
      note.addEventListener("click", (event) => {
        if (event.shiftKey || event.ctrlKey || event.metaKey) return;
        openEditorFor(line.id);
      });
    }
    body.append(note);
  }

  if (line.original) {
    // The audit trail, said quietly beside the line: the markdown
    // stays clean, the original stays reachable as the title.
    const edited = document.createElement("span");
    edited.className = "line__edited";
    edited.textContent = "edited";
    edited.title = `originally: ${line.original}`;
    text.title = `originally: ${line.original}`;
    body.append(edited);
  }

  if (status === "transcribing" || pendingRetranscribe.has(line.id)) {
    // The design's "still being transcribed" row: `--muted` text and a mono
    // marker, no actions. The same marker for a fresh utterance the model has
    // started on and for a re-transcribe of a stored line — from the reader's
    // side they are the same fact.
    const pending = document.createElement("span");
    pending.className = "line__pending";
    pending.textContent = "transcribing…";
    body.append(pending);
  }

  const tools = document.createElement("span");
  tools.className = "line__tools";
  // `draggable="false"` here stops a press on a button from picking the whole
  // row up: the drag source is the nearest draggable ancestor, and this ends
  // that search.
  tools.draggable = false;

  if (waiting) {
    // No actions at all. What a queued or held row gets instead is the design's
    // mono `--faint` tag in the same column, so the row still says what state
    // it is in without offering anything that cannot be done to it.
    if (status !== "transcribing") {
      const tag = document.createElement("span");
      tag.className = "line__tag";
      tag.textContent = status === "held" ? "held" : "queued";
      tag.title =
        status === "held"
          ? "Transcribed, but the file would not take it yet. Sotone is keeping the words and the audio and will write them as soon as it can."
          : "Captured and waiting for the model. Recording never waits for transcription.";
      item.append(grip, time, body, tag);
    } else {
      item.append(grip, time, body, tools);
    }
    return item;
  }

  // Edit · Play · Re-transcribe · Delete. The design draws Edit and Delete;
  // the other two are Sotone realities and join the same hover group. A failed
  // line leads with **Retry** instead — the design's one row-level primary —
  // and keeps everything else, Edit included: a user typing the words is the
  // honest manual resolve, and it clears the state through the same fold a
  // successful Retry does.
  if (failed) {
    tools.append(
      toolButton(
        "Retry",
        "Run the model over this line's audio again",
        () => retranscribeLine(line.id),
        "line__tool--primary",
      ),
    );
  }

  if (!line.deleted) {
    tools.append(
      toolButton(
        "Edit",
        failed
          ? "Type what you said — the line stops being a failure the moment it has words"
          : "Edit this line's text",
        () => openEditorFor(line.id),
      ),
    );
  }

  // Playback is output only, so it stays available while a recording is live.
  const play = document.createElement("button");
  play.type = "button";
  play.className = "line__tool";
  play.textContent = "Play";
  play.title = failed
    ? "Play the audio Sotone kept for this line"
    : "Play the audio this line was transcribed from";
  play.disabled = !line.has_audio;
  play.addEventListener("click", () => playLine(line.id, play));
  tools.append(play);

  if (!failed) {
    tools.append(
      // Spelled out rather than "redo": that read as re-*record* to a reader,
      // and this button never touches the microphone.
      toolButton("Re-transcribe", "Transcribe this line's audio again", () =>
        retranscribeLine(line.id),
      ),
    );
  }

  // Delete **asks**: it arms the in-row confirm rather than doing
  // it. Restore stays a plain instant action — it is the undo of a
  // delete, not a destruction, and a question about putting a line back would
  // be a question about nothing.
  tools.append(
    toolButton(
      line.deleted ? "Restore" : "Delete",
      line.deleted
        ? "Put this line back in the note"
        : "Leave it out of the note — Sotone will ask first",
      () =>
        line.deleted ? setLineDeleted(line.id, false) : armDelete(line.id),
      line.deleted ? null : "line__tool--danger",
    ),
  );

  item.append(grip, time, body, tools);

  // Click grammar, as the design draws it. The tools and the grip are their own
  // controls, and a plain click on the text is still the editor — the design's
  // "the text is the edit control" stands. Everything else on the row
  // selects, and a modified click selects wherever it lands.
  item.addEventListener("click", (event) => {
    const modified = event.shiftKey || event.ctrlKey || event.metaKey;
    if (event.target.closest(".line__tools, .line__grip")) return;
    if (!modified && event.target.closest(".line__text, .line__failed, .line__hint")) {
      return;
    }
    selectLineFromClick(line.id, event);
  });

  // The design's Keyboard line, the half this screen owns: Enter edits the
  // focused line. Space toggles its membership — and takes the scroll default
  // with it, or the list would jump under the press. No other keys: Escape is
  // one document-level rule and arrow-key ranges are out of scope.
  item.addEventListener("keydown", (event) => {
    // A press inside the editor or on a tool button is that control's.
    if (event.target !== item) return;
    if (event.key === "Enter") {
      event.preventDefault();
      if (!line.deleted && !waiting) openEditorFor(line.id);
    } else if (event.key === " " || event.key === "Spacebar") {
      event.preventDefault();
      toggleLineSelected(line.id);
    }
  });

  // Deleted lines drag too: they hold a place in the transcript, and putting a
  // struck-through line back where it belongs is part of the same pass.
  if (dragAllowed()) makeDraggable(item, line.id);
  return item;
}

function renderTranscript(payload) {
  if (editing) {
    // Come back to it when the editor closes.
    deferredTranscript = payload;
    renderUndoState();
    return;
  }
  deferredTranscript = null;

  const switched = payload.draft_id !== transcript.draft_id;
  // A different draft's pending re-transcribes — and a drag that started in it
  // — are not this draft's business. Its undo history is: the stacks are keyed
  // by draft id and survive the switch.
  if (switched) {
    pendingRetranscribe.clear();
    retranscribeWas.clear();
    endDrag();
    // A selection and an armed question are both about *these* rows. Carrying
    // either into another note would name lines the user is no longer looking
    // at.
    forgetSelection();
    pendingDelete = null;
  }
  transcript = payload;
  // A re-render of the same draft re-marks the same ids and drops the ones
  // that are gone — including the armed row, if this payload moved it.
  pruneSelection();

  const list = el("line-list");
  // Newest is at the bottom, so stay pinned there when the user already was —
  // and do not fight their scroll otherwise.
  const atBottom =
    list.scrollHeight - list.scrollTop - list.clientHeight < 24;

  // While a term is active the pane lists only the matching lines, and the ids
  // are the backend's answer rather than a second match run here.
  // Pending rows are **not searched** — they are not on disk and have no words
  // yet — so they are not listed under a term either.
  const filtered = searchIdsFor(payload.draft_id);
  // …and minus the soft-deleted rows while the view toggle is on.
  const shown = shownLines(payload.draft_id, payload.lines);
  const waiting = filtered ? [] : visiblePending();

  list.replaceChildren();
  // "Nothing here yet" is only true when nothing is on its way either: a note
  // whose first utterance is still in the model has a row, and telling the user
  // it is empty underneath it would be a second, contradictory answer.
  if (shown.length === 0 && waiting.length === 0) {
    const empty = document.createElement("li");
    empty.className = "muted";
    // A note whose every line is deleted would otherwise read as a note nobody
    // has spoken into — the one place where hiding could pass for emptiness, so
    // it says which it is: a vanished row is never a mystery.
    const hidden = hiddenDeletedCount();
    empty.textContent = filtered
      ? `No lines in this note contain ${quoted(searchTerm())}.`
      : hidden > 0
        ? "Every line in this note is deleted, and deleted lines are hidden."
        : `No lines in this note yet. ${holdHint()}`;
    list.append(empty);
  }
  for (const line of shown) {
    list.append(lineRow(line));
  }
  // After the stored lines, in spoken order among themselves: nothing here is
  // in the note yet, so nothing here can sit between two lines that are.
  for (const line of waiting) {
    list.append(lineRow(line));
  }

  if (switched || atBottom) list.scrollTop = list.scrollHeight;

  renderUndoState();
  renderSpan();
  // The header's selection toolbar and the footer's count, from the same
  // snapshot the rows were drawn from — so the number on Delete N and the
  // number of filled rows can never disagree.
  renderSelection();
  // What the hide filter is keeping off screen, from that same snapshot.
  renderHiddenFact();
  // Which of the two the pane shows — the card or the invite — depends on
  // whether there is a note at all, and that is this payload's business too.
  renderNoteHead();
}

// ---------------------------------------------------------------------------
// Hiding the deleted lines
//
// A render filter, and one config key to remember it by. Flipping it
// draws the transcript again from the payload the backend already sent — the
// store never learns that a row left the screen, and no note is touched
// (invariant 4): the only write is `set_hide_deleted`, which puts one bool in
// the config file through the same atomic save every other setting uses. The
// rows are **absent** rather than dimmed, the same way a search-filtered row is
// absent, because "hidden but still in the list" is what the selection ranges
// and the armed confirm would then have to reason about separately.
// ---------------------------------------------------------------------------

// How many of the open note's lines the toggle is keeping off screen. Counted
// from the note rather than from the difference between two renders: the footer
// is stating a fact about the note, and it must read the same whether or not a
// search term is also filtering the list.
function hiddenDeletedCount() {
  if (!hideDeleted) return 0;
  return transcript.lines.filter((line) => line.deleted).length;
}

// The footer's quiet statement of it. The node is created here rather than
// living in the markup because it exists only while it has something to say —
// absent, like the rows it is reporting on, so the footer's gap never sits
// empty. Same `data-open="yes"` grammar as the counters beside it, and the
// `.foot__right` colour and mono face are inherited: nothing sets a colour from
// JS.
function renderHiddenFact() {
  const right = document.querySelector(".pane__footer .foot__right");
  if (!right) return;
  const existing = el("pane-hidden");
  const n = hiddenDeletedCount();
  if (n === 0) {
    if (existing) existing.remove();
    return;
  }
  const node = existing || document.createElement("span");
  node.id = "pane-hidden";
  node.className = "foot__hidden";
  node.dataset.open = "yes";
  node.textContent = n === 1 ? "1 deleted hidden" : `${n} deleted hidden`;
  node.title =
    "Deleted lines are hidden in this view. Nothing was removed — they are still in the note's history, and Show deleted lines brings them back.";
  if (!existing) right.insertBefore(node, el("note-state"));
}

// The toggle itself, from either context menu. One flip and one re-render:
// `renderTranscript` prunes the selection to the rows that are still visible
// and tears down an armed confirm whose row has gone, so every consequence of
// a row leaving the list is the one that already existed. Nothing here
// touches focus, DOM or window (invariant 2): the menu item's own click already
// put the menu away before this runs.
//
// The flip is local and instant; the invoke behind it only
// records the answer for the next launch. Fire-and-forget, with no
// snap-back branch, because there is nothing to snap back from: the backend
// never refuses this one, not even mid-recording, and a config write that
// somehow fails is a notice — not a reason to move rows the user just asked
// for back onto the screen.
function toggleHideDeleted() {
  hideDeleted = !hideDeleted;
  // The last payload is re-rendered from on its own (`renderReady` does it), so
  // it has to agree with the flag straight away: a *stale* settings object
  // replayed in the gap before our write comes back would otherwise adopt the
  // old value and put the rows back for a frame.
  lastSettings.hide_deleted = hideDeleted;
  renderTranscript(transcript);
  invoke("set_hide_deleted", { on: hideDeleted }).catch(reportFailure);
}

// The item both menus carry. It is **not** disabled while a recording is
// live: it writes nothing but a view preference, which the backend accepts
// mid-recording for exactly that reason — the same reason typing a search term
// stays allowed. The label is the state, because a menu item must say what
// clicking it does.
function hideDeletedMenuItem() {
  return {
    label: hideDeleted ? "Show deleted lines" : "Hide deleted lines",
    title: hideDeleted
      ? "List deleted lines in the transcript again"
      : "Keep deleted lines out of the transcript. Nothing is removed — this only changes what this window shows, and Sotone remembers it next time.",
    run: toggleHideDeleted,
  };
}

// Click the line (or its Edit action) and the text becomes a field where it
// was: same type, same position, so nothing jumps and the eye does not have to
// find the line again, as the design draws it. Enter saves, Escape
// reverts, and editing text never re-runs transcription and never touches the
// audio — which is what the hint under the field says out loud.
//
// `focus()` here moves the caret inside a window the user just clicked in. It
// is not window activation — nothing in this file raises, shows or activates a
// window (invariant 2).
function beginEdit(id, textEl) {
  if (!editingAllowed() || editing) return;
  editing = { id, was: textEl.textContent };
  // A searched row's text is split across mark spans; the field has to open on
  // the plain line the user is about to edit, not on a marked-up copy of it.
  // `textContent` already concatenated them, so this flattens the node.
  if (textEl.firstElementChild) textEl.textContent = editing.was;
  textEl.contentEditable = "plaintext-only";
  textEl.dataset.editing = "yes";
  textEl.focus();

  const hint = document.createElement("span");
  hint.className = "line__hint";
  const keys = document.createElement("span");
  keys.textContent = "⏎ save · esc revert";
  const untouched = document.createElement("span");
  untouched.textContent = "audio and timestamp unchanged";
  hint.append(keys, untouched);
  // Under the field, at the end of the row's body — never between the field and
  // the "edited" mark that may already be sitting beside it.
  if (textEl.parentElement) textEl.parentElement.append(hint);
  // The panel's Undo/Redo step aside while the editor is open: inside it,
  // Ctrl+Z belongs to the contenteditable.
  renderUndoState();

  const selection = window.getSelection();
  if (selection) {
    const range = document.createRange();
    range.selectNodeContents(textEl);
    selection.removeAllRanges();
    selection.addRange(range);
  }

  textEl.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey) {
      // One line, one finding: Enter commits rather than making a paragraph.
      event.preventDefault();
      textEl.blur();
    } else if (event.key === "Escape") {
      event.preventDefault();
      cancelEdit();
    }
  });
  // **Blur commits here, and only here.** The line editor's standing contract
  // is that an edit to a line's text is light, and losing the caret to the
  // next thing is how a person finishes typing. The rename field deliberately
  // does the opposite (`renameFieldNode` below reverts on blur) because a
  // rename changes a file's name on disk: the two edits are deliberately not
  // the same weight.
  textEl.addEventListener("blur", () => commitEdit(textEl));
}

function closeEditor(textEl) {
  textEl.contentEditable = "false";
  delete textEl.dataset.editing;
  const hint = textEl.parentElement
    ? textEl.parentElement.querySelector(".line__hint")
    : null;
  if (hint) hint.remove();
  editing = null;
  // Whatever arrived while the caret was in there is applied now; failing that,
  // re-render so the row's controls come back in their normal state.
  renderTranscript(deferredTranscript || transcript);
}

function commitEdit(textEl) {
  if (!editing) return;
  const { id, was } = editing;
  const text = textEl.textContent.trim();
  const draftId = transcript.draft_id;
  closeEditor(textEl);
  // An empty line is not an edit anyone means to make, and the backend would
  // dutifully store it. Put the text back instead. No command, so no undo step:
  // only an edit that actually changed the line is a step.
  if (text === "" || text === was) return;
  invoke("line_edit", { id, text })
    .then(() =>
      pushStep(draftId, {
        undo: { name: "line_edit", args: { id, text: was } },
        redo: { name: "line_edit", args: { id, text } },
      }),
    )
    .catch(reportFailure);
}

function cancelEdit() {
  if (!editing) return;
  const textEl = document.querySelector('.line__text[data-editing="yes"]');
  if (!textEl) {
    editing = null;
    return;
  }
  textEl.textContent = editing.was;
  closeEditor(textEl);
}

function setLineDeleted(id, deleted) {
  const draftId = transcript.draft_id;
  invoke("line_set_deleted", { id, deleted })
    .then(() =>
      pushStep(draftId, {
        undo: { name: "line_set_deleted", args: { id, deleted: !deleted } },
        redo: { name: "line_set_deleted", args: { id, deleted } },
      }),
    )
    .catch(reportFailure);
}

// ---------------------------------------------------------------------------
// Selection, and the in-row delete confirm
//
// Three view-only states live here — the selected
// set, the row whose delete question is armed, and the anchor a shift-click
// extends from — and the design's state model binds the first two to the
// editor: `selection`, `editingLineId` and `pendingDeleteLineId` are **all
// mutually exclusive; entering one clears the others**.
//
// **No new write path.** A bulk delete is N of the same `line_set_deleted`
// calls a row's own tool sends: still soft, still reversible, still nothing
// removed from disk (invariant 4) — and one compound undo step makes it *more*
// reversible than N separate deletes, not less. Copy is
// `navigator.clipboard.writeText`; nothing leaves the
// machine (invariant 3). Move to note… added the one command in
// this family, `line_move_to`, and it is the same act from this side: the
// source line is soft-deleted exactly as Delete N deletes it, and the append
// on the far side is the worker's. Every id in these sets is checked against
// the transcript the backend last sent before it is used, so a set can never
// name a line that has gone.
// ---------------------------------------------------------------------------

// The selected line ids. Order is never read from this set: transcript order is
// derived from the list the backend sent, so what Copy writes cannot depend on
// the order the user happened to click in.
const selection = new Set();

// Where a shift-extension starts from — the last plainly-clicked or ctrl-added
// row — and the set as it stood when that click landed. The base is what makes
// a second shift-click *replace* the previous extension rather than grow it,
// while leaving ctrl-added rows outside the range alone.
let selectionAnchor = null;
let selectionBase = new Set();

// The row whose delete question is armed, as `{ id, text }`, or null. Exactly
// one at a time — the question is a row, not a dialog, and two would be two
// questions. The text is kept so a transcript that *changes* that line tears
// the question down: the quote on screen is part of what was asked.
let pendingDelete = null;

// Which of a payload's stored lines the pane lists: the search filter and
// the hide-deleted filter applied in that order, over **one** path. Both
// are read-only view state, and putting them here rather than at each call site
// is what makes every rule that hangs off "which rows are visible" — ranges,
// pruning, the armed confirm's teardown — fall out for free for the second one.
function shownLines(draftId, lines) {
  const filtered = searchIdsFor(draftId);
  const shown = filtered ? lines.filter((line) => filtered.has(line.id)) : lines;
  // A deleted line never matches a term, so under a term this
  // filter is already a no-op — the two compose without either knowing about
  // the other.
  return hideDeleted ? shown.filter((line) => !line.deleted) : shown;
}

// The stored lines the pane is actually showing, in transcript order. Under a
// term that is the matching subset — a range that reached across
// rows the filter is hiding would select lines the user cannot see, the same
// reason reordering is refused there. Pending rows are never in it: they name
// nothing on disk, so they can be neither selected nor deleted.
function visibleLines() {
  return shownLines(transcript.draft_id, transcript.lines);
}

function lineById(id) {
  return transcript.lines.find((line) => line.id === id) || null;
}

// The selection in transcript order, dropping anything the pane is no longer
// showing.
function selectionIds() {
  return visibleLines()
    .filter((line) => selection.has(line.id))
    .map((line) => line.id);
}

// Which of those Delete N would actually change: a row already out of the note
// is skipped, so the number on the button is the number of lines it touches.
function deletableIds() {
  return selectionIds().filter((id) => {
    const line = lineById(id);
    return Boolean(line) && !line.deleted;
  });
}

// The selection's destructive half inherits the gates the header already has —
// it does not invent one. Copy stays enabled under both: it is read-only.
function deleteSelectionBlocked(count) {
  if (!editingAllowed()) {
    return "Not while a recording is running — editing is post-session";
  }
  if (conflictHeld()) {
    return "This note is out of service until the changed-on-disk question is answered";
  }
  if (count === 0) return "Every selected line is already out of the note";
  return "";
}

// Forget the set without drawing anything. The callers below each know whether
// they are about to render.
function forgetSelection() {
  selection.clear();
  selectionBase = new Set();
  selectionAnchor = null;
  // The move chooser acts on the set, so it cannot outlive it. One
  // teardown for every path that ends a selection — an editor opening, an
  // armed confirm, a drag, a search term, a note switch, Escape — rather than
  // a line in each of them to forget.
  closeMoveChooser();
}

// The set, and the rows that show it, in one go.
function clearSelection() {
  if (selection.size === 0 && selectionAnchor === null) return;
  forgetSelection();
  renderTranscript(transcript);
}

// Un-mark the rows in place, for the two paths that must not rebuild the list:
// a dragstart (the rebuild would throw away the element being lifted) and an
// editor opening on a node that has already been handed to `beginEdit`.
function unmarkSelectedRows() {
  for (const row of document.querySelectorAll('.line[data-selected="yes"]')) {
    delete row.dataset.selected;
  }
}

// Ids the sets hold that the pane is no longer showing are dropped: a
// re-render of the same draft re-marks the same rows and forgets the rest, and
// a draft switch clears everything (in `renderTranscript`).
//
// The armed confirm goes the same way — and also when its line's *words* moved
// under it, because the quote is half of what was asked.
function pruneSelection() {
  const showing = new Map(visibleLines().map((line) => [line.id, line]));
  for (const id of selection) if (!showing.has(id)) selection.delete(id);
  for (const id of selectionBase) if (!showing.has(id)) selectionBase.delete(id);
  if (selectionAnchor !== null && !showing.has(selectionAnchor)) {
    selectionAnchor = null;
  }
  if (pendingDelete !== null) {
    const line = showing.get(pendingDelete.id);
    if (!line || line.deleted || line.text !== pendingDelete.text) {
      pendingDelete = null;
    }
  }
}

// The row element for an id, and the text node inside it. Looked up from the
// list rather than remembered, because every state change here rebuilds it.
function rowFor(id) {
  for (const row of el("line-list").querySelectorAll(".line")) {
    if (row.dataset.id === id) return row;
  }
  return null;
}

function lineTextNode(id) {
  const row = rowFor(id);
  return row ? row.querySelector(".line__text") : null;
}

// Keyboard interaction rebuilds the list under the user's fingers, so the row
// they were on has to be handed the focus back. DOM focus inside a window that
// already has it — not window activation, which nothing in this file does
// (invariant 2).
function focusRow(id) {
  const row = rowFor(id);
  if (row) row.focus();
}

// -- Selecting ---------------------------------------------------------------

// Shift-click extends from the anchor to the clicked row, replacing whatever
// the last extension added and keeping the ctrl-added rows the anchor was set
// with — the familiar file-manager grammar.
function extendSelection(id) {
  const ids = visibleLines().map((line) => line.id);
  const from = ids.indexOf(selectionAnchor);
  const to = ids.indexOf(id);
  if (from < 0 || to < 0) return;
  const start = Math.min(from, to);
  const end = Math.max(from, to);
  selection.clear();
  for (const kept of selectionBase) selection.add(kept);
  for (let at = start; at <= end; at += 1) selection.add(ids[at]);
}

// One click, the whole grammar: click selects, clicking the sole
// selected row again clears, shift-click extends, ctrl/cmd-click adds.
// A row that is not a stored line — queued, transcribing, held — is refused
// silently: those rows are inert and there is nothing on disk for an
// action to name.
function selectLineFromClick(id, event) {
  if (!lineById(id)) return;
  // Entering the selection ends the other three states (the state model's
  // mutual exclusion). `cancelEdit` is the revert, never a silent commit — the
  // same rule search entry follows, and the rename field goes the same way.
  cancelEdit();
  pendingDelete = null;
  cancelRename();

  if (event.shiftKey && selectionAnchor !== null && lineById(selectionAnchor)) {
    extendSelection(id);
  } else if (event.ctrlKey || event.metaKey) {
    if (selection.has(id)) selection.delete(id);
    else selection.add(id);
    selectionAnchor = id;
    selectionBase = new Set(selection);
  } else if (!event.shiftKey && selection.size === 1 && selection.has(id)) {
    // By design: clicking the row that is the *only*
    // selected one clears the set — "the same empty space you clicked to
    // select it" unselects it. It goes through `forgetSelection`, the one
    // teardown Escape's `clearSelection` uses, so the move chooser and the
    // anchor die exactly as they do there; the render below is the render
    // `clearSelection` would have done. Nothing else in this grammar moves:
    // shift-click still extends from the anchor, ctrl/cmd-click still toggles
    // per row (including down to empty), `selectionBase`/`selectionAnchor` are
    // set in the same places as before, a plain click on the text still opens
    // the editor while a modified click anywhere is selection, inert rows
    // (queued, transcribing, held) are still refused by the `lineById` guard
    // above, and Space still toggles from the keyboard. A plain click on one
    // row of a *multi*-selection still collapses to that row alone — only the
    // click after that clears. The `!event.shiftKey` guard is why this is not
    // a change to shift-click: a shift-click whose anchor has been pruned away
    // has always fallen through to the collapse below, and still does.
    forgetSelection();
  } else {
    selection.clear();
    selection.add(id);
    selectionAnchor = id;
    selectionBase = new Set(selection);
  }
  renderTranscript(transcript);
  // The re-render replaced the row that was just clicked, and a click leaves
  // the focus on an element that no longer exists — so Enter and Space did
  // nothing until the user tabbed back in (re-review N1). Handing the focus to
  // the new row is the same thing `toggleLineSelected` already does, and it is
  // DOM focus inside a window that already has it (invariant 2).
  focusRow(id);
}

// Space on a focused row: the ctrl-click half of the grammar, from the
// keyboard.
function toggleLineSelected(id) {
  if (!lineById(id)) return;
  cancelEdit();
  pendingDelete = null;
  cancelRename();
  if (selection.has(id)) selection.delete(id);
  else selection.add(id);
  selectionAnchor = id;
  selectionBase = new Set(selection);
  renderTranscript(transcript);
  focusRow(id);
}

// The one way into the line editor from a row. It exists because the teardown
// has to happen **before** the field's node is looked up: opening a selection
// or an armed confirm rebuilds the list, and handing the caret to a node that
// rebuild has already thrown away is the trap this ordering avoids.
function openEditorFor(id) {
  if (!editingAllowed() || editing) return;
  // The fourth member of the exclusive family: a rename field open in
  // the tree or the breadcrumb reverts before the caret goes into a line.
  cancelRename();
  if (selection.size > 0 || selectionAnchor !== null || pendingDelete !== null) {
    forgetSelection();
    pendingDelete = null;
    renderTranscript(transcript);
  }
  const node = lineTextNode(id);
  if (node) beginEdit(id, node);
}

// -- The header's three actions ----------------------------------------------

// The selected lines' words, newline-joined, in transcript order. The folded
// text — what the note renders — not the original, and a line with no words (a
// failed one) contributes nothing rather than a blank line.
function selectionText() {
  return selectionIds()
    .map((id) => ((lineById(id) || {}).text || "").trim())
    .filter((words) => words !== "")
    .join("\n");
}

// Read-only, so it is offered under every gate the destructive half refuses —
// and it is the same `navigator.clipboard` the model-address button uses, with
// the same answer when the context has no clipboard at all: say so, rather
// than leave a button that did nothing visible.
async function copySelection() {
  const text = selectionText();
  if (text === "") {
    // A selection of nothing but failed rows has no words in it. Say so: a
    // button that did nothing visible is worse than a refusal.
    handleNotice({
      level: "warn",
      message: "nothing to copy — the selected lines have no words yet",
    });
    return;
  }
  const button = el("sel-copy");
  try {
    await navigator.clipboard.writeText(text);
    button.textContent = "Copied";
    setTimeout(() => {
      button.textContent = "Copy";
    }, COPY_FEEDBACK_MS);
  } catch (err) {
    handleNotice({
      level: "warn",
      message: `could not copy the selected lines (${err})`,
    });
  }
}

// N soft deletes, in transcript order, and **one** undo step over all of them.
// No second confirm: an explicit multi-select is already deliberate (the design
// draws "Delete 3" bare), and one Ctrl+Z reverses the whole act.
async function deleteSelection() {
  const doomed = deletableIds();
  if (deleteSelectionBlocked(doomed.length) !== "") return;
  const draftId = transcript.draft_id;

  // What is left selected is what the action did not touch: rows that were
  // already out of the note stay in the set, struck through, saying so.
  const skipped = selectionIds().filter((id) => !doomed.includes(id));
  forgetSelection();
  for (const id of skipped) selection.add(id);
  selectionBase = new Set(selection);
  renderTranscript(transcript);

  const done = [];
  try {
    for (const id of doomed) {
      await invoke("line_set_deleted", { id, deleted: true });
      done.push(id);
    }
  } catch (err) {
    // Whatever went through still has to be undoable, so the step below covers
    // exactly what happened rather than what was asked for.
    reportFailure(err);
  }
  if (done.length === 0) return;
  pushStep(draftId, {
    undo: done.map((id) => ({
      name: "line_set_deleted",
      args: { id, deleted: false },
    })),
    redo: done.map((id) => ({
      name: "line_set_deleted",
      args: { id, deleted: true },
    })),
  });
}

// -- Move to note… -----------------------------------------------------------
//
// The selection's third action, and the only one that asks a question first:
// Copy and Delete N know what they act on, this one needs a destination. The
// question is the `.popup` grammar, in-page, never a native dialog (invariant
// 2) — and Escape cancels it, because it is a picker the user opened rather
// than an answer the backend is waiting for.
//
// **One `line_move_to` per line, in transcript order**, which is the Copy
// precedent and keeps this a sibling of `line_set_deleted` rather than a
// second, wider write path. Only the first of those calls carries
// `first: true` — the destination's session divider goes above the batch, not
// above each line, and this is the only party that knows where a batch begins.
// The undo step is byte-identical in shape to a
// Delete N step, and that is the whole of what Undo covers: the lines come
// back here, and the copies stay where they were put. Undo must never reach
// into a note the user is not looking at — the stacks are per draft — so
// there is nothing honest it could do about the far side. On record: the rule
// is "leaving the source as one undo step", read literally.

// `{ draftId }` while the chooser is on screen, else null. The ids are
// deliberately **not** kept here: they are computed at confirm time from the
// transcript the backend last sent, so a line that left the note while the
// popup was open cannot be named by it.
let moveChooser = null;

// The chooser's one fixed option, and the reason "nowhere to move to" is not a
// state that exists any more: the destination can be made from
// here. The empty string is deliberately **not** an id — no draft can collide
// with it — and it is read back off `select.value`, which is also what an empty
// `<select>` reports, so "there was no previous choice" is read off the option
// count rather than off the value.
const NEW_NOTE = "";

// Every *existing* note this selection could go to: all of them but the open
// one. The active id is excluded as well as the transcript's, so the chooser
// cannot offer the note the worker is holding even if the two ever disagreed.
// New note is not in here — it is the fixed option the list always ends with.
function moveTargets() {
  const drafts = Array.isArray(lastDrafts.drafts) ? lastDrafts.drafts : [];
  return drafts.filter(
    (draft) => draft.id !== transcript.draft_id && draft.id !== lastDrafts.active_id,
  );
}

// The source note's project, as the last drafts payload reported it. A note
// stitched together from another one's lines is filed in the group those lines
// came from; `null` is the tree's "not in your projects" group and a real
// answer, not a missing argument.
function moveSourceProject() {
  const drafts = Array.isArray(lastDrafts.drafts) ? lastDrafts.drafts : [];
  const source = drafts.find((draft) => draft.id === transcript.draft_id);
  return (source && source.project) || null;
}

// Exactly the destructive half's gates, and no more. The fourth one
// — "there is no other note to move these lines to" — is gone with the decision
// that the chooser can make the note: nowhere-to-move is not a state that
// exists any more, so there is nothing left here to offer-and-refuse.
function moveSelectionBlocked(count) {
  return deleteSelectionBlocked(count);
}

// The chooser's list, rebuilt from the drafts payload each time it is shown or
// the tree changes underneath it. The current choice survives a rebuild when it
// is still there, exactly as the pick-or-create stop's picker does.
//
// It never retracts on an empty list any more: the list is never empty,
// because New note is always the last option in it.
function fillMoveChooser() {
  const targets = moveTargets();
  const select = el("move-note-select");
  // An empty `<select>` reports `value === ""`, which is now New note's own
  // value, so "there was no previous choice" has to be read off the options.
  const chosen = select.options.length === 0 ? null : select.value;
  select.replaceChildren();
  for (const draft of targets) {
    const option = document.createElement("option");
    option.value = draft.id;
    // The tree's own label, so a note is called the same thing in both places,
    // with its project beside it — two notes can share a name across projects.
    option.textContent = `${noteLabel(draft)} · ${draft.project || "no project"}`;
    select.append(option);
  }
  // Last, always, and after the existing notes: it is the way out of the list,
  // not the head of it, and a fresh chooser opens on the first real note.
  const fresh = document.createElement("option");
  fresh.value = NEW_NOTE;
  fresh.textContent = "New note";
  select.append(fresh);

  if (chosen === NEW_NOTE || targets.some((draft) => draft.id === chosen)) {
    select.value = chosen;
  }

  const n = deletableIds().length;
  el("move-note-detail").textContent =
    n === 1
      ? "Move 1 line into another note. Nothing is written until you choose."
      : `Move ${n} lines into another note. Nothing is written until you choose.`;
}

function openMoveChooser() {
  const doomed = deletableIds();
  if (moveSelectionBlocked(doomed.length) !== "") return;
  // Opening it does not touch the set: it acts on the selection, so the
  // selection has to still be there when it is answered.
  moveChooser = { draftId: transcript.draft_id };
  fillMoveChooser();
  el("move-note").dataset.open = "yes";
}

function closeMoveChooser() {
  if (moveChooser === null) return;
  moveChooser = null;
  el("move-note").dataset.open = "no";
}

// The answer. N invokes in transcript order over the ids that are **still** in
// the note at this moment, then one compound undo step over exactly what went
// through — the Delete N shape, and for the same reason: one act, one Ctrl+Z.
//
// New note adds exactly one call in front of those N: the destination is
// made first and the moves are the ordinary ones against the id it answers
// with. A create that fails moves **nothing** — the selection is still there to
// try again with, and no `line_move_to` was sent. A create that succeeds when
// every move afterwards fails leaves an empty note in the tree, which the user
// can discard; nothing here tries to roll a creation back, because that would
// be this app deleting a note by itself.
async function moveSelection() {
  if (moveChooser === null) return;
  const chosen = el("move-note-select").value;
  const draftId = transcript.draft_id;
  const doomed = deletableIds();
  const project = moveSourceProject();
  closeMoveChooser();
  // Re-checked here, not trusted from the moment the popup opened: a recording
  // can have started, or the note can have changed, while it was on screen.
  if (moveSelectionBlocked(doomed.length) !== "") return;
  if (chosen === draftId) return;

  let target = chosen;
  if (chosen === NEW_NOTE) {
    try {
      // Not `draft_new`: that one opens what it makes as the active note,
      // which would displace the note these lines are leaving, half way
      // through the batch.
      target = await invoke("draft_create_detached", { project });
    } catch (err) {
      reportFailure(err);
      return;
    }
    if (!target || target === draftId) return;
  }

  // What is left selected is what the action did not touch — the
  // reduction rule, so rows already out of the note stay in the set saying so.
  // With every selected row moved, that leaves the set empty and the header
  // goes back to being the note's own.
  const skipped = selectionIds().filter((id) => !doomed.includes(id));
  forgetSelection();
  for (const id of skipped) selection.add(id);
  selectionBase = new Set(selection);
  renderTranscript(transcript);

  const done = [];
  try {
    for (const [at, id] of doomed.entries()) {
      // `first` is the batch boundary, and this is the only place in the app
      // that knows where one is: the destination's divider goes
      // above the first arrival and nowhere else, and after that first line
      // has landed nothing on disk could tell this move from another one.
      await invoke("line_move_to", { id, target, first: at === 0 });
      done.push(id);
    }
  } catch (err) {
    // Whatever went through still has to be undoable, so the step below covers
    // exactly what happened rather than what was asked for.
    reportFailure(err);
  }
  if (done.length === 0) return;
  pushStep(draftId, {
    undo: done.map((id) => ({
      name: "line_set_deleted",
      args: { id, deleted: false },
    })),
    redo: done.map((id) => ({
      name: "line_set_deleted",
      args: { id, deleted: true },
    })),
  });
}

// -- The in-row confirm ------------------------------------------------------

// The row's Delete tool arms the question instead of answering it.
// Arming is entering one of the three mutually exclusive states, so the editor
// and the selection end here.
function armDelete(id) {
  const line = lineById(id);
  if (!line || line.deleted || !editingAllowed()) return;
  cancelEdit();
  cancelRename();
  forgetSelection();
  pendingDelete = { id, text: line.text };
  renderTranscript(transcript);
}

function disarmDelete() {
  if (pendingDelete === null) return;
  pendingDelete = null;
  renderTranscript(transcript);
}

// The answer: the existing single soft delete, with the existing undo step.
// Nothing about what this does is new — only when it is asked for.
function confirmDelete() {
  if (pendingDelete === null) return;
  const { id } = pendingDelete;
  pendingDelete = null;
  renderTranscript(transcript);
  setLineDeleted(id, true);
}

// The armed row, drawn as the question. The row itself becomes it:
// `--panel-high`, the line quoted so there is no doubt which one is going, the
// only red outline in the app on the destructive answer, and a quiet Cancel.
//
// **The copy is the truth, not the mock's.** The canvas says the audio goes
// with the line; in Sotone it does not — this delete is soft, the wav is
// untouched, the row offers Restore and Undo puts it back. Deviation from the
// drawn copy, on record.
function confirmRow(line) {
  const item = document.createElement("li");
  item.className = "line line--confirm";
  item.dataset.id = line.id;
  item.dataset.confirm = "yes";

  const main = document.createElement("span");
  main.className = "confirm__main";

  const title = document.createElement("span");
  title.className = "confirm__title";
  title.textContent = "Delete this line?";

  const note = document.createElement("span");
  note.className = "confirm__note";

  const quote = document.createElement("span");
  quote.className = "confirm__quote";
  const words = (line.text || "").trim();
  // A failed line has no words on purpose, so there is nothing to quote; say
  // which line it is instead of showing empty quotes.
  quote.textContent = words
    ? quoted(words)
    : "this line has no words — the model refused its audio";
  quote.title = words || "";

  const sep = document.createElement("span");
  sep.className = "confirm__sep";
  sep.textContent = "·";
  sep.setAttribute("aria-hidden", "true");

  const fact = document.createElement("span");
  fact.className = "confirm__fact";
  fact.textContent =
    "It leaves the note — nothing is deleted from disk, and Undo puts it back.";

  note.append(quote, sep, fact);
  main.append(title, note);

  const actions = document.createElement("span");
  actions.className = "confirm__actions";

  const go = document.createElement("button");
  go.type = "button";
  go.id = "confirm-delete";
  go.className = "confirm__go";
  go.textContent = "Delete";
  go.title = "Leave this line out of the note";
  go.addEventListener("click", confirmDelete);

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.id = "confirm-cancel";
  cancel.className = "confirm__quiet";
  cancel.textContent = "Cancel";
  cancel.title = "Keep this line (esc)";
  cancel.addEventListener("click", disarmDelete);

  actions.append(go, cancel);
  item.append(main, actions);
  return item;
}

// -- The chrome --------------------------------------------------------------

// The header's swap and the footer's count. Called from `renderTranscript`, the
// one function every path that changes "which rows, and which of them are in
// the set" already reaches.
function renderSelection() {
  const ids = selectionIds();
  const n = ids.length;
  el("pane-session").dataset.selection = n === 0 ? "none" : "some";

  el("sel-count").textContent = n === 1 ? "1 line selected" : `${n} lines selected`;

  const copy = el("sel-copy");
  copy.title =
    n === 1
      ? "Copy this line's text to the clipboard"
      : "Copy these lines' text to the clipboard, one per line";

  const doomed = deletableIds().length;

  // The same count Delete N acts on — a row already out of the note has
  // nothing to take anywhere — under exactly Delete N's gates: the
  // chooser can make the destination, so "nowhere to put them" is gone.
  const move = el("sel-move");
  const cannotMove = moveSelectionBlocked(doomed);
  move.disabled = cannotMove !== "";
  move.title =
    cannotMove ||
    `Move ${doomed === 1 ? "this line" : "these lines"} into another note — the audio and the edit history go too, and one Undo brings ${doomed === 1 ? "it" : "them"} back here`;

  const remove = el("sel-delete");
  remove.textContent = `Delete ${doomed}`;
  const blocked = deleteSelectionBlocked(doomed);
  remove.disabled = blocked !== "";
  remove.title =
    blocked ||
    `Leave ${doomed === 1 ? "this line" : "these lines"} out of the note — nothing is deleted from disk, and one Undo puts ${doomed === 1 ? "it" : "them all"} back`;

  const facts = el("sel-facts");
  facts.dataset.open = n === 0 ? "no" : "yes";
  facts.textContent = n === 0 ? "" : `${n} of ${transcript.lines.length} selected`;
  facts.title =
    n === 0 ? "" : "Selected lines, of the lines this note holds";
}

// ---------------------------------------------------------------------------
// Renaming a note or a project
//
// One field, four places: a note row in the tree, a
// project group row, and each of the two breadcrumb crumbs. **A note's name is
// its file's name** — there is no name field anywhere in the store — so
// renaming a note renames its `.md` and everything that shows a label follows
// the next drafts event for free.
//
// It is the fourth member of the state model's mutually exclusive family
// (selection · editor · armed confirm · rename): opening one clears the others,
// and everything that tears a rename down **reverts** it — including a blur
// — deliberately, reversing an earlier blur-commits rule. A
// rename is heavier than a text edit, so teardown is never a silent commit:
// **Enter is the only thing in this file that renames a file.**
//
// Nothing here sanitizes the typed name: `sotone-core`'s `file_safe` owns the
// filename rules, and a second sanitizer in JS would eventually disagree with
// the one that decides what is on disk. What comes back on the drafts event is
// the sanitized name, which is how the user sees what actually happened.
// ---------------------------------------------------------------------------

// The one open rename field, or null:
// `{ kind: "note"|"project", where: "row"|"crumb", key, was, value, caret, node }`
//
// `key` is the draft id or the project name; `was` is the label the field
// opened on, which is what "unchanged" is measured against and what tells us
// the row was renamed out from under the field by something else.
let renaming = null;

// Renaming is refused while a recording is live, in the backend and in both
// layers above it. The pencils are hidden rather than disabled for the reason
// §18 gives: an affordance that exists only to explain a refusal is the menu
// lying about what it can do.
function renameAllowed() {
  return !recordingLive;
}

function renamingHere(kind, where, key) {
  return (
    renaming !== null &&
    renaming.kind === kind &&
    renaming.where === where &&
    renaming.key === key
  );
}

// Whether what the field is about is still there, and still called what the
// field opened on. Either answer of "no" is a teardown: the question the field
// is asking has changed underneath it.
function renameSubjectGone() {
  if (!renaming) return false;
  if (renaming.kind === "note") {
    const draft = lastDrafts.drafts.find((d) => d.id === renaming.key);
    return !draft || noteLabel(draft) !== renaming.was;
  }
  return !lastProjects.projects.some((p) => p.name === renaming.key);
}

// Revert, and put the surfaces back. Silent when nothing is open, so every
// teardown site can call it unconditionally.
function cancelRename() {
  if (renaming === null) return;
  renaming = null;
  // The tree owns three of the four places a field can be, and this also
  // re-runs the header and the footer through `renderSaveState`.
  renderDrafts(lastDrafts);
}

// Enter, and nothing else, by design. Empty or unchanged is a
// quiet revert: no command, nothing to report, and no sentence about a rename
// that did not happen.
function commitRename() {
  if (renaming === null) return;
  const { kind, key, was, value } = renaming;
  const name = value.trim();
  renaming = null;
  renderDrafts(lastDrafts);
  if (name === "" || name === was) return;
  if (kind === "note") {
    invoke("draft_rename", { id: key, name }).catch(reportFailure);
  } else {
    invoke("project_rename", { from: key, to: name }).catch(reportFailure);
  }
}

// Opening a field is entering one of the mutually exclusive states, so the
// other three end here — the editor by **reverting**, exactly as a search term
// or a selection ends it.
function beginRename(kind, where, key, was) {
  if (!renameAllowed()) return;
  cancelEdit();
  forgetSelection();
  pendingDelete = null;
  // The tree's own armed "sure?" is a question about the same row.
  clearPending();
  renaming = { kind, where, key, was, value: was, caret: null, node: null };
  renderDrafts(lastDrafts);
}

// The field itself. Built once per render of the surface it sits in; the
// handlers close over the node so a *rebuilt* field's predecessor cannot act on
// the live rename as it is thrown away — its blur would otherwise revert the
// field that just replaced it.
function renameFieldNode() {
  const wrap = document.createElement("span");
  wrap.className = "rename";

  const input = document.createElement("input");
  input.type = "text";
  input.id = "rename-field";
  input.className = "rename__input";
  input.spellcheck = false;
  input.value = renaming.value;
  input.setAttribute(
    "aria-label",
    renaming.kind === "note" ? "New note name" : "New project name",
  );

  const mine = () => renaming !== null && renaming.node === input;

  input.addEventListener("input", () => {
    if (!mine()) return;
    renaming.value = input.value;
    renaming.caret = [input.selectionStart, input.selectionEnd];
  });
  input.addEventListener("keyup", () => {
    if (mine()) renaming.caret = [input.selectionStart, input.selectionEnd];
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commitRename();
    } else if (event.key === "Escape") {
      // Stays in the field: the document-level Escape ladder checks
      // `defaultPrevented` first, so this is one more rung rather than a
      // second answer to one press.
      event.preventDefault();
      cancelRename();
    }
  });
  input.addEventListener("blur", () => {
    // **Blur reverts**, deliberately and against an earlier rule.
    // A rename is heavier than a text edit — it changes a file's name on disk —
    // so the only act that commits one is the explicit Enter, and losing the
    // field to anything else puts the name back. This deliberately breaks with
    // the line editor's contract, which still commits on blur (see `textEl`
    // above), because the two edits are
    // not the same weight.
    if (mine()) cancelRename();
  });

  const hint = document.createElement("span");
  hint.className = "rename__hint";
  hint.textContent = "⏎ rename · esc cancel";

  wrap.append(input, hint);
  return wrap;
}

// After any render that may have rebuilt the field: take the caret back. DOM
// focus inside a window that already has it — never window activation, which
// nothing in this file does (invariant 2).
function restoreRename() {
  if (renaming === null) return;
  const input = el("rename-field");
  if (!input) return;
  const opening = renaming.node === null || renaming.node !== input;
  renaming.node = input;
  if (document.activeElement !== input) input.focus();
  if (renaming.caret) {
    input.setSelectionRange(renaming.caret[0], renaming.caret[1]);
  } else if (opening) {
    // The whole name selected, ready to be typed over: the rename convention
    // everywhere else.
    input.select();
  }
}

// The pencil, wherever it appears. `stopPropagation` because in the tree it
// sits inside a row whose own click opens the note.
function pencilButton(title, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "pencil";
  button.title = title;
  button.setAttribute("aria-label", title);
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("width", "12");
  svg.setAttribute("height", "12");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", "M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5L17 3z");
  svg.append(path);
  button.append(svg);
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    onClick();
  });
  return button;
}

// -- What the footer says while a field is open ------------------------------

// Whether a project rename would carry its folder along. **The same questions
// Rust's `folder_plan` asks, minus the one a browser cannot ask** — whether
// something is already sitting at the target — so this can say "renamed to
// match" where the backend then finds the name occupied and keeps the folder.
// The backend's own notice is the authority on what happened; this is the
// design's promise that nothing happens *silently*, made before the fact. On
// record.
//
// One question is taken *out* of this: a folder whose name is not the
// project's is carried along now, like any other (the rule is "files move when
// you move them"). One question is left that a browser can answer — does
// another project point at this folder — and the other two stay in Rust: is
// the target occupied (no filesystem here) and is the typed name usable as a
// folder name (`file_safe` is the one sanitizer and there is no JS copy of it).
// So the footer promises the common case and the notice tells the truth.
function folderFollowsRename(project) {
  if (!project || !project.notes_dir) return false;
  return !lastProjects.projects.some(
    (p) => p.name !== project.name && p.notes_dir === project.notes_dir,
  );
}

// The footer's two halves: what is being renamed, and the path that changes.
function renameFooter() {
  if (renaming === null) return { text: "", path: "" };
  if (renaming.kind === "note") {
    const draft = lastDrafts.drafts.find((d) => d.id === renaming.key);
    return {
      text: "renaming note · file renamed to match",
      path: (draft && draft.saved_path) || "",
    };
  }
  const project = lastProjects.projects.find((p) => p.name === renaming.key);
  const folder = folderFollowsRename(project)
    ? "folder renamed to match"
    : "folder kept";
  return {
    text: `renaming project · ${folder}`,
    path: (project && project.notes_dir) || "",
  };
}

// The header's two pencils, the crumb fields, and the footer. Called from
// `renderNoteHead`, which every path that changes "which note, and what is it
// called" already reaches.
function renderRename() {
  if (renaming !== null && renameSubjectGone()) {
    // Teardown, not a commit: the row the field was asking about is gone or
    // has been renamed by something else.
    renaming = null;
  }

  const draft = activeDraft();
  const project = draft
    ? lastProjects.projects.find((p) => p.name === draft.project)
    : lastProjects.projects.find((p) => p.name === lastProjects.active);

  // An unbound note has no file to rename and no name of its own — its label
  // is the date it was started — so it offers no pencil. Deviation from F,
  // on record: inventing a "pending name" is state the design does not draw.
  const noteShown = Boolean(renameAllowed() && draft && draft.saved_path);
  const projectShown = Boolean(renameAllowed() && project);
  el("rename-note").dataset.show = noteShown ? "yes" : "no";
  el("rename-project").dataset.show = projectShown ? "yes" : "no";
  el("rename-note").title = draft
    ? `Rename ${noteFileLabel(draft)}`
    : "Rename this note";
  el("rename-project").title = project
    ? `Rename "${project.name}"`
    : "Rename this project";

  for (const [kind, slotId] of [
    ["project", "slot-project"],
    ["note", "slot-note"],
  ]) {
    const slot = el(slotId);
    const open = renaming !== null && renaming.where === "crumb" && renaming.kind === kind;
    slot.dataset.renaming = open ? "yes" : "no";
    const existing = slot.querySelector(".rename");
    if (!open) {
      if (existing) existing.remove();
    } else if (!existing) {
      // Rebuilt only when it is not already there: the header is static
      // markup, so a field that survives a re-render keeps its caret without
      // anything having to restore it.
      slot.append(renameFieldNode());
    }
  }

  const { text, path } = renameFooter();
  const says = el("pane-rename");
  says.dataset.open = renaming === null ? "no" : "yes";
  says.textContent = text;
  const where = el("rename-path");
  where.dataset.open = renaming !== null && path !== "" ? "yes" : "no";
  where.textContent = path;
  where.title = path;

  restoreRename();
}

function retranscribeLine(id) {
  const line = transcript.lines.find((candidate) => candidate.id === id);
  pendingRetranscribe.add(id);
  if (line && transcript.draft_id) {
    retranscribeWas.set(id, { draftId: transcript.draft_id, was: line.text });
  }
  renderTranscript(transcript);
  invoke("line_retranscribe", { id }).catch(reportFailure);
}

// The other half of a re-transcribe: the model's answer is only an undo step if
// it actually reads differently. Decided here, from the transcript that came
// back, rather than at click time — the new text is not known until then, and
// the undo of a re-transcribe is a plain text edit either way (no second run of
// the model on redo).
function reconcileRetranscribe(payload) {
  if (retranscribeWas.size === 0) return;
  const byId = new Map(payload.lines.map((line) => [line.id, line]));
  for (const [id, entry] of retranscribeWas) {
    const line = byId.get(id);
    if (entry.draftId !== payload.draft_id || !line) continue;
    if (line.text === entry.was) continue;
    pushStep(entry.draftId, {
      undo: { name: "line_edit", args: { id, text: entry.was } },
      redo: { name: "line_edit", args: { id, text: line.text } },
    });
  }
  retranscribeWas.clear();
}

async function playLine(id, button) {
  if (!transcript.draft_id) return;
  button.disabled = true;
  try {
    const encoded = await invoke("line_audio", {
      draftId: transcript.draft_id,
      lineId: id,
    });
    const bytes = Uint8Array.from(atob(encoded), (c) => c.charCodeAt(0));
    audioContext = audioContext || new AudioContext();
    // Autoplay policy suspends a context created outside a gesture; this call
    // is inside a click, so this is only belt and braces.
    await audioContext.resume();
    const buffer = await audioContext.decodeAudioData(bytes.buffer);
    const source = audioContext.createBufferSource();
    source.buffer = buffer;
    source.connect(audioContext.destination);
    source.start();
  } catch (err) {
    // A missing or empty wav is one sentence in the footer, not a broken panel.
    reportFailure(`could not play that line: ${err}`);
  } finally {
    button.disabled = false;
  }
}

// Every rejected command in this file lands here. A rejection is a failure the
// user did not ask for, so it goes to the footer slot *and* the log — the same
// route a backend `error` notice takes, because from the user's side they are
// the same event.
function reportFailure(err) {
  handleNotice({ level: "error", message: `${err}` });
}

// ---------------------------------------------------------------------------
// Error states: conditions, the footer message slot, the debug log
//
// Three surfaces, one rule each, and no overlap:
//
// * `conditionState` is whatever the backend last published. It is *rendered*,
//   never derived: no code path in this file sets it from a notice, a save
//   outcome or a failed invoke. That is deliberate — the conditions this
//   replaces were announced through the rolling notice list, which is how "the
//   app is deaf" became a line that scrolled away.
// * The message slot is one line, latest wins. It never fades on a timer (the
//   design rules out toasts), and it is cleared by the next thing that works.
// * The debug log is an in-memory ring behind an off-by-default toggle.
//   Nothing about it is persisted; the *switch* is a display preference in
//   localStorage, like `sotone.tree_collapsed`.
// ---------------------------------------------------------------------------

// The published condition, exactly as `sotone://condition` (and the startup
// snapshot) carries it.
let conditionState = { condition: null, detail: "" };

// A condition the user dismissed, as `name|detail`. The strip is theirs to put
// away — the design says it stays "until the condition clears or the user
// dismisses it" — but a *different* occurrence, or a changed sentence, is a new
// question and shows again. The backend is not told: the only new command this
// task adds is `hook_recheck`, and a dismissal is a view preference about a
// state that genuinely still holds. So the footer readout and the mic row go on
// saying so, and a reload brings the strip back.
let dismissedCondition = null;

function conditionKey(state) {
  return state.condition ? `${state.condition}|${state.detail}` : null;
}

// The condition this window should be showing, or null. One value, because the
// backend publishes one — the precedence between them is Rust's.
function activeCondition() {
  const key = conditionKey(conditionState);
  if (key !== null && key === dismissedCondition) return null;
  return conditionState.condition;
}

// Can Sotone record at all right now? Both of these mean no, for different
// reasons: `noDevice` has nothing to capture with, `hotkeyDead` has no way to
// be told to. A dismissed strip does not make either of them untrue, so this
// reads the published state and not `activeCondition`.
function cannotRecord() {
  return (
    conditionState.condition === "noDevice" ||
    conditionState.condition === "hotkeyDead"
  );
}

// Is the transcript out of service pending a conflict decision? The one state
// that dims the card and greys the header actions — and the dismissal *does*
// count here, because "Leave the file" means "let me get on with it": the note
// stays dirty, and the guarded save will refuse again if they press Save.
function conflictHeld() {
  return activeCondition() === "fileConflict";
}

// What the footer says the cause is, plainly and in mono. Deliberately not the
// strip's sentence: the design's rule for the pane takeover is that the status
// bar *reports the cause* rather than repeating the message.
const CONDITION_CAUSE = {
  hotkeyDead: "Hotkeys: dead",
  noDevice: "no input device",
  fileConflict: "editing paused",
};

// The strip's title and its actions, per condition. `fileConflict`'s title
// names the file, so it is built rather than looked up.
function stripCopy(name) {
  if (name === "noDevice") {
    return {
      title: "Microphone disconnected",
      primary: "Open recording settings",
      dismiss: "Dismiss",
      diff: false,
    };
  }
  if (name === "fileConflict") {
    const file = conflictPayload
      ? fileNameOf(conflictPayload.path) || "That file"
      : "That file";
    return {
      title: `${file} changed on disk`,
      // The one command in this app that can discard what somebody else wrote,
      // and it is reachable only from here, only after the sentence above.
      primary: "Keep mine",
      dismiss: "Leave the file",
      // Offered only when there is something to show: after a reload the
      // condition survives but the two texts do not.
      diff: Boolean(conflictPayload),
    };
  }
  return null;
}

function fileNameOf(path) {
  if (!path) return "";
  return path.split(/[\\/]/).pop() || path;
}

// The backend's details are clauses, not always sentences: some end in a full
// stop, some end in an error string, some end in neither. Punctuation is
// presentation, so it is settled here rather than by making eight emission
// sites agree.
function endStop(text) {
  const trimmed = (text || "").trim();
  if (trimmed === "") return "";
  return /[.!?]$/.test(trimmed) ? trimmed : `${trimmed}.`;
}

function withoutStop(text) {
  return (text || "").trim().replace(/[.\s]+$/, "");
}

// The whole condition render. Called from the condition event, from the recording
// event (Keep mine is gated on it) and from the save outcome.
function renderCondition() {
  const name = activeCondition();
  const pane = el("pane-session");
  // CSS owns every appearance change that hangs off this: the dimmed card, the
  // faint header actions, the pane takeover. Never the `hidden` attribute.
  pane.dataset.condition = name || "none";

  // The pane takeover (F). Its copy is the condition's own detail, so it names
  // the real cause — the helper stopping N times, the hook that would not
  // install, no recording mode enabled — rather than a Windows `RegisterHotKey`
  // story Sotone does not have.
  el("hotkey-dead-note").textContent =
    name === "hotkeyDead"
      ? `${withoutStop(conditionState.detail)} — your capture keys do nothing until it is back.`
      : "";

  const copy = stripCopy(name);
  const strip = el("condition-strip");
  strip.dataset.open = copy ? "yes" : "no";
  if (copy) {
    el("strip-title").textContent = copy.title;
    el("strip-note").textContent = endStop(conditionState.detail);

    const primary = el("strip-primary");
    primary.dataset.show = "yes";
    primary.textContent = copy.primary;
    // Keep mine is refused mid-recording by the backend anyway (a note rendered
    // mid-utterance is missing the line being spoken). Disabling here is the
    // structural mirror of that rule, the same arrangement the project rows'
    // `+` uses — and it stays in place rather than vanishing under the cursor.
    primary.disabled = name === "fileConflict" && recordingLive;
    primary.title =
      name === "fileConflict"
        ? primary.disabled
          ? "Not while a recording is running — stop it first"
          : "Overwrite the file with this note, discarding the change on disk"
        : "Choose a microphone in Settings";

    const dismiss = el("strip-dismiss");
    dismiss.dataset.show = "yes";
    dismiss.textContent = copy.dismiss;
    dismiss.title =
      name === "fileConflict"
        ? "Leave the file alone — this note stays unsaved"
        : "Put this away; the microphone is still gone";

    const diff = el("strip-diff");
    diff.dataset.show = copy.diff ? "yes" : "no";
    diff.textContent = diffOpen ? "Hide what changed" : "Show what changed";
  } else {
    for (const id of ["strip-primary", "strip-dismiss", "strip-diff"]) {
      el(id).dataset.show = "no";
    }
  }

  const cause = el("pane-condition");
  cause.dataset.open = conditionState.condition ? "yes" : "no";
  cause.textContent = CONDITION_CAUSE[conditionState.condition] || "";
  cause.title = conditionState.detail || "";

  renderDiff();
  // The indicator and the header controls are both functions of the condition.
  renderIndicator();
  renderSaveState();
  renderUndoState();
}

function applyCondition(payload) {
  const arrivingKey = conditionKey(payload);
  // A dismissal covers the occurrence it was aimed at and nothing else.
  if (dismissedCondition !== null && dismissedCondition !== arrivingKey) {
    dismissedCondition = null;
  }
  conditionState = {
    condition: payload.condition || null,
    detail: payload.detail || "",
  };
  // The conflict's two texts belong to one save attempt. When the condition
  // ends they are stale, so they go with it.
  if (conditionState.condition !== "fileConflict") {
    forgetConflict();
  }
  renderCondition();
}

function dismissCondition() {
  dismissedCondition = conditionKey(conditionState);
  diffOpen = false;
  renderCondition();
}

// ---------------------------------------------------------------------------
// The footer message slot
//
// **Invented surface, on record.** The design draws the `--warn` treatment for
// a refusal but gives a one-off failure no specific home. One slot, latest
// wins, no history — history is the debug log — is this window's reading of
// "no toasts, no modals, say it plainly". It is rendered twice: in the session
// pane's footer, and in the first-run panel, which has no pane footer and where
// a rejected model file still has to be readable.
// ---------------------------------------------------------------------------

function setMessage(text) {
  // Three renderings of **one** slot: the pane footer, the repair panel's copy
  // and the wizard's. Latest wins in all three, because there is
  // only one message.
  for (const id of ["pane-message", "empty-message", "wiz-message"]) {
    const slot = el(id);
    slot.dataset.open = text ? "yes" : "no";
    slot.textContent = text || "";
    // The slot is one line and elides; the whole sentence is always reachable.
    slot.title = text || "";
  }
}

// The next thing that worked. Called from the paths that end the state the
// message was about — a save that went through, a note switch — rather than
// from a timer: a message that disappears on its own is the toast this design
// rules out.
function clearMessage() {
  setMessage("");
}

// ---------------------------------------------------------------------------
// The debug log
//
// The notice channel survived the funeral; only its UI died. Everything the
// backend emits is recorded here, and `warn`/`error` additionally reach the
// user through the slot above. The level is the whole routing rule — there is
// no list of messages anywhere deciding what is important.
// ---------------------------------------------------------------------------

// Whether the log is shown. A display preference, the same class as
// `sotone.tree_collapsed` and `sotone.draft_sort`: it lives in localStorage and
// deliberately never reaches the config file. The
// *entries* are never persisted anywhere — this is a debugging window, not
// telemetry, and nothing leaves the machine either way (invariant 3).
const DEBUG_KEY = "sotone.debug_log";

function loadDebugOpen() {
  try {
    return localStorage.getItem(DEBUG_KEY) === "yes";
  } catch {
    // Storage can be blocked or full; off is the right answer to either, and
    // it is the default anyway.
    return false;
  }
}

let debugOpen = loadDebugOpen();

// Newest first, capped. An array rather than DOM nodes so the cap is honest
// even while the log is hidden — a session that ran for an hour with the log
// off must not have kept an hour of `<li>`s.
const debugRing = [];

function handleNotice(notice) {
  const level = notice.level || "info";
  debugRing.unshift({
    at: new Date(),
    level,
    message: notice.message || "",
  });
  if (debugRing.length > MAX_LOG) debugRing.length = MAX_LOG;
  if (debugOpen) renderDebugLog();
  // The user-facing half. Everything else is log traffic: a confirmation whose
  // result is already on screen, or a skipped line whose absence the missing
  // cue already reported live.
  if (level === "warn" || level === "error") setMessage(notice.message);
}

function renderDebugLog() {
  const list = el("debug-list");
  list.replaceChildren();
  for (const entry of debugRing) {
    const item = document.createElement("li");
    item.dataset.level = entry.level;

    const time = document.createElement("span");
    time.className = "debuglog__time";
    time.textContent = entry.at.toLocaleTimeString();

    const level = document.createElement("span");
    level.className = "debuglog__level";
    level.textContent = entry.level;

    const text = document.createElement("span");
    text.className = "debuglog__text";
    text.textContent = entry.message;

    item.append(time, level, text);
    list.append(item);
  }
  el("debug-count").textContent =
    debugRing.length === 1 ? "1 entry" : `${debugRing.length} entries`;
}

function showDebugLog(open) {
  debugOpen = open;
  el("debug-log").dataset.open = open ? "yes" : "no";
  const toggle = el("debug-toggle");
  if (toggle) toggle.checked = open;
  try {
    localStorage.setItem(DEBUG_KEY, open ? "yes" : "no");
  } catch {
    // Not persisting a view preference is survivable; the session keeps it.
  }
  if (open) renderDebugLog();
}

// ---------------------------------------------------------------------------
// Saving — the first complete "dictate → tidy → save"
//
// The button sends and nothing else: the render, the external-edit guard and the
// atomic write all happen on the worker thread, and the only confirmation is the
// `sotone://save` event. Saving is deliberately *not* an undo step — the undo
// stacks are never touched here.
// ---------------------------------------------------------------------------

// The active draft, as the last drafts event described it. Save needs its
// `dirty` flag and its `saved_path`, neither of which the transcript carries.
function activeDraft() {
  return lastDrafts.drafts.find((d) => d.id === lastDrafts.active_id) || null;
}

// A save is only offered when there is a note, it has something unsaved in it,
// nobody is speaking into it right now, and no unanswered conflict is holding
// it. The third is the discard precedent: rendering a note mid-utterance writes
// a file that is missing the line being spoken. The fourth: while the
// strip is up the note is out of service, and Keep mine is the one way through
// — a Save that just conflicted again would be the header answering a question
// the strip is already asking.
function canSave() {
  const draft = activeDraft();
  // Held lines count as something to save: they are not in the log yet,
  // so the draft can read clean while the note is genuinely behind — and a Save
  // is one of the moments the worker flushes them. Saving what can be saved is
  // allowed and truthful, which is why this enables rather than blocks.
  const behind = heldCount() > 0;
  return Boolean(
    draft && (draft.dirty || behind) && editingAllowed() && !conflictHeld(),
  );
}

// Where, before there is a where. A first save that lands somewhere the user did
// not expect is the failure this tooltip exists to prevent — and the
// honest answer is sometimes "nowhere yet, Sotone will ask".
function saveTargetLabel(draft) {
  // `saved_path` arrives already resolved against the draft's project's current
  // notes folder. It is null both before the first save and when a relative
  // binding's project is gone — in which case naming a path would name the
  // wrong one.
  if (draft && draft.saved_path) return `Save to ${draft.saved_path}`;
  const orphan =
    draft &&
    draft.project &&
    !lastProjects.projects.some((p) => p.name === draft.project);
  if (orphan) {
    return `This note was dictated for "${draft.project}", which is not in your projects any more`;
  }
  if (lastDrafts.default_save_dir) {
    return `Will save to ${lastDrafts.default_save_dir}`;
  }
  return "Sotone will ask which project to save this note into";
}

// How many notes one Save all would take on: every dirty draft that belongs to
// a project the config still has, whichever project that is — deliberately
// store-wide, where it used to be the active project's only.
//
// A count from the same snapshot the dots are drawn from, deliberately: the
// button promises "3 notes" and the list shows three dots, and the backend
// re-lists after anything that changes a draft, so the snapshot is the
// honest number rather than a second source of truth. The backend re-reads the
// store itself before writing anything, so a stale count here can only mean the
// batch does slightly more or less than the label said — never that it writes
// something it should not.
//
// Drafts with no project — or naming one the config no longer has — are not
// counted, because they are not notes (notes exist only within projects)
// and the backend skips them. The button must not promise them.
function dirtyNotes() {
  return lastDrafts.drafts.filter((d) => d.dirty && savableProject(d.project))
    .length;
}

// The same test the backend applies when it builds the batch's contexts: a
// project it still has, with a folder chosen. A project whose `notes_dir` is
// empty has nowhere to put a note, so its drafts are skipped there and must not
// be counted here — the button would promise a save that could not happen.
function savableProject(name) {
  if (!name) return false;
  const project = lastProjects.projects.find((p) => p.name === name);
  return Boolean(project && project.notes_dir);
}

// Save all is offered when the store holds an unsaved note anywhere and nobody
// is speaking. Same recording gate as Save, for the same reason — rendering
// notes mid-utterance writes files missing the line being spoken. No active
// project is needed any more: a dirty note in *any* project is work this button
// can do.
function canSaveAll() {
  return editingAllowed() && !conflictHeld() && dirtyNotes() > 0;
}

function saveAllLabel(count) {
  if (count === 0) return "Every note is saved";
  const notes = count === 1 ? "1 note" : `${count} notes`;
  // No project named: the batch spans all of them, and each note goes to its
  // own project's folder. The per-note Save tooltip is where a resolved target
  // is stated.
  return `Save ${notes} — every unsaved note, in every project`;
}

// Which note the transcript is showing, where it goes, and whether it has
// anything unsaved. All of it comes off the drafts payload — the same snapshot
// the tree's dots are drawn from — so the header, the footer and the sidebar can
// never disagree about a note's state.
//
// It also decides which of the two the pane shows: the transcript card, or the
// invite to make the first note. The invite is only ever offered when there is
// *no* active note, so nothing here can hide a note lines are landing in.
function renderNoteHead() {
  const draft = activeDraft();
  // A note that does not exist yet but already has an utterance in the model
  // counts: the invite would otherwise cover the queued row that says so, and
  // the note is one successful decode away from existing.
  const hasNote =
    Boolean(lastDrafts.active_id || transcript.draft_id) ||
    visiblePending().length > 0;

  // The note's *own* project, not the active one: a note dictated before any
  // project existed reads "no project" here exactly as it does in the tree, and
  // the footer below says where a first save would land. Naming the active
  // project here would promise a home this note does not have yet.
  const project = el("note-project");
  const named = draft ? draft.project || "" : lastProjects.active || "";
  project.textContent = named || "no project";
  project.title = named
    ? "The project this note is saved into"
    : "This note has no project yet — Sotone will ask where to save it";

  const title = el("note-title");
  title.textContent = draft ? noteFileLabel(draft) : "no note yet";
  title.title = draft
    ? draft.saved_path || "not saved to a file yet"
    : "The first line you speak starts one";

  // The footer: the resolved path this note is bound to, and how far behind the
  // file is. A first save must never be a surprise about where, so the folder
  // is named even before there is a file.
  const path = el("note-path");
  if (draft && draft.saved_path) {
    path.textContent = draft.saved_path;
  } else if (draft && lastDrafts.default_save_dir) {
    path.textContent = `${lastDrafts.default_save_dir} — not saved yet`;
  } else if (draft) {
    path.textContent = "not saved yet";
  } else {
    path.textContent = "";
  }
  path.title = path.textContent;

  const state = el("note-state");
  // `data-open`, not `hidden`: the rule that styles it keys off the attribute,
  // and the word beside it changes with the same flag.
  state.dataset.open = draft && draft.dirty ? "yes" : "no";
  state.textContent = draft ? (draft.dirty ? "unsaved changes" : "saved") : "";

  const pane = el("pane-session");
  pane.dataset.empty = hasNote ? "no" : "yes";
  if (!hasNote) renderInvite();

  // The two breadcrumb pencils, whichever crumb is mid-rename, and the
  // footer's statement of the path that is about to change. Same
  // reason as the line below: this is the one function every path that changes
  // "which note, and what is it called" already calls.
  renderRename();

  // Search's own chrome: the scope attribute, the counters and the
  // library-scope list. Driven from here because this is the one function every
  // path that changes "which note, and what is in it" already calls.
  renderSearch();
}

// The invite, when there is no note open. Two shapes: a project that has no
// notes yet (the design's second frame), and no active project at all — in
// which case the honest copy is that speaking makes the first note.
function renderInvite() {
  const project = lastProjects.active;
  const notesHere = project
    ? lastDrafts.drafts.filter((d) => d.project === project).length
    : 0;

  let title = "No note yet";
  if (project) {
    title = notesHere === 0 ? `${project} has no notes yet` : `No note open in ${project}`;
  }
  el("invite-title").textContent = title;

  el("invite-note").textContent = project
    ? `Create a note, then talk: ${holdHint()}. Every line you say is transcribed here and appended to it. Later notes are made from the + on the project in the sidebar.`
    : `Sotone saves notes inside a project — make one with the + above the tree, or just talk and Sotone will ask where the note goes. ${holdHint()}.`;

  const folder = project
    ? (lastProjects.projects.find((p) => p.name === project) || {}).notes_dir
    : "";
  el("invite-path").textContent = folder || "";
}

function renderSaveState() {
  const button = el("lines-save");
  button.disabled = !canSave();
  // The file is behind by lines that exist and are not in it — the design's
  // rule is "Save in the header goes `--warn` to show the file is behind".
  // A data attribute, so the colour is CSS's; the button stays *enabled*.
  const held = heldCount();
  button.dataset.behind = held > 0 ? "yes" : "no";
  button.title =
    held > 0
      ? `${held === 1 ? "1 line is" : `${held} lines are`} held in memory and not in the file yet — Sotone keeps trying to write them (Ctrl+S)`
      : `${saveTargetLabel(activeDraft())} (Ctrl+S)`;

  // The count itself, in the footer, at full text brightness while it is not
  // zero — the surface a held line actually has. No message per hold: the
  // number is the report.
  const behind = el("pane-held");
  behind.dataset.open = held > 0 ? "yes" : "no";
  behind.textContent =
    held === 0 ? "" : held === 1 ? "1 line unwritten" : `${held} lines unwritten`;
  behind.title =
    held === 0
      ? ""
      : "Sotone has these lines and their audio in memory. They go into the note as soon as the file will take them; nothing is discarded.";

  // No shortcut on the second button, deliberately, and no undo
  // step for either: a save is not an edit.
  const all = el("lines-save-all");
  all.disabled = !canSaveAll();
  all.title = saveAllLabel(dirtyNotes());

  // Driven from here because this is the one function every path that changes
  // "which note, and is it dirty" already calls.
  renderNoteHead();
}

function saveDraft(overwrite = false) {
  if (!overwrite && !canSave()) return;
  invoke("draft_save", { overwrite }).catch(reportFailure);
}

// One click, one command, no arguments: which projects exist and where their
// notes live is a question for the config, answered on the control thread and
// carried into the batch. There is no overwrite variant to pass.
function saveAllDrafts() {
  if (!canSaveAll()) return;
  invoke("draft_save_all").catch(reportFailure);
}

// Which draft the conflict on screen is about, or null when there is none.
//
// The strip is an in-flow block, not a modal — Sotone never takes focus and
// never blocks the rest of the window (invariant 2) — so the drafts list stays
// clickable beside it. That makes this id load-bearing rather than
// bookkeeping: Keep mine is the one command in the app that discards what
// somebody else wrote, and `draft_save` applies to whichever draft is *active
// now*. Left alone, switching drafts with the strip up would aim the user's
// "yes, overwrite the file I was just shown" at a different note entirely,
// bypassing that note's external-edit guard without ever showing its diff
// (invariant 4). So the strip is torn down the moment the active draft moves —
// in the backend as well — and Keep mine refuses anything it was not
// opened for.
let conflictFor = null;

// The save event the conflict came from: the file, what is on disk, and what
// the refused save would have written. Only this window has it, so a reload
// leaves the condition standing with no diff to offer — which is why "Show
// what changed" is conditional on it rather than always drawn.
let conflictPayload = null;

// Whether the diff is expanded. Collapsed by default (deliberately: a
// quiet control, not a second primary).
let diffOpen = false;

// A line landed after the comparison was taken, so the right-hand side is older
// than the note. Said out loud rather than silently redrawn: the pending text
// is what the *refused* save would have written, and this window cannot
// re-render the markdown itself.
let conflictStale = false;

// Forget the texts. The condition itself is the backend's to clear; this is the
// half that lives here.
function forgetConflict() {
  conflictFor = null;
  conflictPayload = null;
  conflictStale = false;
  diffOpen = false;
}

// A naive line diff: the longest common subsequence of the two line lists, with
// everything outside it marked. Deliberately naive — this is a "what
// am I about to lose" readout, not a merge tool. No dependency, no bundler.
//
// LCS is O(n·m), and the right-hand side is a Sotone note, but the left-hand side
// is whatever is on disk. Past the cap the panes simply show their text with
// nothing marked, which is still the information that matters: both sides.
const DIFF_CELL_CAP = 250000;

function commonLines(a, b) {
  if (a.length * b.length > DIFF_CELL_CAP) return null;
  // table[i][j] = length of the LCS of a[i..] and b[j..].
  const table = Array.from({ length: a.length + 1 }, () =>
    new Uint32Array(b.length + 1),
  );
  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i][j] =
        a[i] === b[j]
          ? table[i + 1][j + 1] + 1
          : Math.max(table[i + 1][j], table[i][j + 1]);
    }
  }

  // Walk it back into a per-line "is this one shared" for each side.
  const sharedA = new Array(a.length).fill(false);
  const sharedB = new Array(b.length).fill(false);
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      sharedA[i] = true;
      sharedB[j] = true;
      i += 1;
      j += 1;
    } else if (table[i + 1][j] >= table[i][j + 1]) {
      i += 1;
    } else {
      j += 1;
    }
  }
  return { sharedA, sharedB };
}

function fillPane(pane, lines, shared, mark) {
  pane.replaceChildren();
  for (const [at, text] of lines.entries()) {
    const row = document.createElement("span");
    row.className = "cdiff__line";
    if (shared && !shared[at]) row.dataset.mark = mark;
    // A genuinely empty line still has to occupy a row: as a flex item with no
    // content it would collapse to nothing and the two panes would stop lining
    // up, which is the one thing a side-by-side view has to get right.
    row.textContent = text === "" ? " " : text;
    pane.append(row);
  }
  if (lines.length === 0) {
    const row = document.createElement("span");
    row.className = "cdiff__line muted";
    row.textContent = "(empty)";
    pane.append(row);
  }
}

// `split` on a text ending in a newline leaves a trailing empty element;
// dropping it stops every pane gaining a phantom last line.
function diffLines(text) {
  const lines = (text || "").split("\n");
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

// The diff, expanded inside the dimmed card. Rebuilt from `conflictPayload` on
// every render rather than patched — the same rule the transcript follows — so
// a transcript that moved underneath cannot leave half a stale comparison on
// screen.
function renderDiff() {
  const box = el("conflict-diff");
  const open = conflictHeld() && diffOpen && Boolean(conflictPayload);
  box.dataset.open = open ? "yes" : "no";
  // The transcript gives up its bottom edge while this is open, so the two read
  // as one card. It cannot know that about itself, hence the flag on the pane.
  el("pane-session").dataset.diff = open ? "yes" : "no";
  if (!open) return;

  const disk = diffLines(conflictPayload.disk_text);
  const pending = diffLines(conflictPayload.pending_markdown);
  const common = commonLines(disk, pending);
  fillPane(el("cdiff-disk"), disk, common && common.sharedA, "removed");
  fillPane(el("cdiff-pending"), pending, common && common.sharedB, "added");
  el("cdiff-stale").dataset.open = conflictStale ? "yes" : "no";
}

function renderSaveOutcome(payload) {
  if (payload.outcome === "conflict") {
    // The *strip* comes from `sotone://condition`, which the backend emits
    // alongside this event. All that is kept here is what only this window can
    // hold: the two texts the diff needs, and which note the question is about.
    conflictPayload = payload;
    conflictFor = lastDrafts.active_id;
    conflictStale = false;
    diffOpen = false;
    renderCondition();
    return;
  }
  // Notes exist only within projects. Nothing was written and
  // there is nothing to report as an error — the note simply has nowhere to go
  // yet, and this is the question that gives it one.
  if (payload.outcome === "no_project") {
    openNoProject();
    return;
  }
  // Saved and error both close whatever question was up: it has been answered.
  closeNoProject();
  if (payload.outcome === "saved") {
    // The condition itself clears from the backend — Keep mine *is* an
    // overwrite save, so there is one path and no second one to keep in step.
    // This drops the texts behind it; the dirty dot clears with the drafts
    // event that follows.
    forgetConflict();
    // A save that went through is the next successful user action, so whatever
    // the footer slot was complaining about is over.
    clearMessage();
    renderSaveState();
  }
}

// One batch, one notice — and no dialog of any kind (invariant 2: this window
// never takes focus, and a batch has nothing to show a diff of anyway).
//
// Conflicts are named as a count and an instruction, not as a popup: the
// per-note Save conflict dialog is the resolution surface, and it is the only
// place in this app where "discard what someone else wrote" is reachable, which
// is exactly where that decision belongs (invariant 4).
function saveAllMessage(payload) {
  const saved = payload.saved || 0;
  const skipped = payload.skipped || 0;
  const conflicts = (payload.conflicts || []).length;
  const errors = (payload.errors || []).length;
  const parts = [saved === 1 ? "Saved 1 note" : `Saved ${saved} notes`];
  if (conflicts > 0) {
    parts.push(
      `${conflicts} stopped by an external edit — open each and Save to resolve`,
    );
  }
  // Said, not swallowed: the batch is store-wide now, so it meets
  // drafts that are not notes — no project, or one that is not in the config
  // any more. Nothing failed and nothing was written, but a count of what was
  // saved while unsaved dots are still lit would read as a lie.
  if (skipped > 0) {
    parts.push(
      `${skipped === 1 ? "1 note has" : `${skipped} notes have`} no project to save into — give ${skipped === 1 ? "it" : "them"} one and Save`,
    );
  }
  if (errors > 0) {
    parts.push(errors === 1 ? "1 failed" : `${errors} failed`);
  }
  return parts.join("; ");
}

function renderSaveAllOutcome(payload) {
  const conflicts = (payload.conflicts || []).length;
  const skipped = payload.skipped || 0;
  const errors = payload.errors || [];
  // A clean batch is a confirmation: the dots go out in the tree, which says it
  // better than a sentence does, so it is log traffic. A batch with
  // conflicts, skipped notes or failures is a *report the user has to act on* —
  // the one exception — so it takes the footer slot. Skipped counts as
  // one of those: a dot that stays lit after a Save all needs a
  // sentence saying why, or the button looks broken.
  const level =
    errors.length > 0 ? "error" : conflicts > 0 || skipped > 0 ? "warn" : "info";
  handleNotice({ level, message: saveAllMessage(payload) });
  // Each error names its note and why. They go to the log rather than fighting
  // the summary for the one slot: the summary is what says how many.
  for (const message of errors) {
    handleNotice({ level: "info", message });
  }
  // The drafts event that follows clears the dots; this settles the buttons in
  // case it does not change anything else.
  renderSaveState();
}

// ---------------------------------------------------------------------------
// Projects
//
// A project is a name, a notes folder and two templates. Every control here is
// an editor over the config file: a change is one `project_update` and the
// backend writes the file back with the user's comments and key order intact.
// Nothing is optimistically re-rendered — the `sotone://projects` event that
// comes back is the confirmation, exactly as the transcript panel works.
//
// There is no Projects tab: the sidebar tree *is* the project list,
// and one project's fields open in the main area from the edit affordance on its
// row. The panel is rebuilt from every projects event rather than patched, so a
// project renamed or removed under it cannot leave stale fields on screen.
//
// The filename preview is computed in Rust (`filename_preview`). Expanding
// tokens here as well would be a second answer to "what will this file be
// called", and the two would drift.
// ---------------------------------------------------------------------------

let lastProjects = { projects: [], active: null };

// Which project the per-project panel is showing, by name, or null.
let selectedProject = null;

function debounce(fn, ms) {
  let timer = null;
  return (...args) => {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}

function linkButton(label, title, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "linkish";
  button.textContent = label;
  button.title = title;
  button.addEventListener("click", onClick);
  return button;
}

// One labelled control. Returns the row and its input so the caller can wire it.
function fieldRow(label, value, placeholder) {
  const row = document.createElement("div");
  row.className = "field";
  const name = document.createElement("span");
  name.className = "field__label";
  name.textContent = label;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "field__input";
  input.spellcheck = false;
  input.value = value || "";
  if (placeholder) input.placeholder = placeholder;
  row.append(name, input);
  return { row, input };
}

function updateProject(name, changes) {
  invoke("project_update", { name, ...changes }).catch(reportFailure);
}

// The native folder dialog, and the only thing that opens it is this click.
async function pickFolder() {
  try {
    return await invoke("project_pick_folder");
  } catch (err) {
    reportFailure(err);
    return null;
  }
}

// One project's fields, as they go into the main area. Same controls as the old
// Projects-tab row — folder + Browse, filename template with the live preview,
// header template, make-active, reveal — and the same commands behind them.
function projectPanel(project, activeName) {
  const item = document.createElement("div");
  const active = project.name === activeName;

  const head = document.createElement("div");
  head.className = "projects__head";
  const name = document.createElement("h3");
  name.className = "projects__name";
  name.textContent = project.name;
  head.append(name);

  if (active) {
    const mark = document.createElement("span");
    mark.className = "projects__mark";
    mark.textContent = "active";
    head.append(mark);
  }

  const tools = document.createElement("span");
  tools.className = "projects__tools";
  if (!active) {
    tools.append(
      linkButton("make active", "New notes go into this project", () => {
        invoke("project_set_active", { name: project.name }).catch(reportFailure);
      }),
    );
  }
  tools.append(
    linkButton("open folder", "Show this project's notes folder", () => {
      invoke("project_reveal", { target: "notes_dir", name: project.name }).catch(
        reportFailure,
      );
    }),
  );
  head.append(tools);

  // Placeholders describe the field, never demonstrate a value, and are
  // sentence case.
  const dir = fieldRow("Notes folder", project.notes_dir, "Pick a folder");
  dir.input.addEventListener("change", () =>
    updateProject(project.name, { notesDir: dir.input.value }),
  );
  dir.row.append(
    linkButton("Browse…", "Choose this project's notes folder", async () => {
      const chosen = await pickFolder();
      if (!chosen) return;
      dir.input.value = chosen;
      updateProject(project.name, { notesDir: chosen });
    }),
  );

  const filename = fieldRow(
    "Filename",
    project.filename_template,
    "{project} {date} {time}.md",
  );
  const preview = document.createElement("span");
  preview.className = "field__preview";
  const refresh = debounce(async () => {
    try {
      preview.textContent = await invoke("filename_preview", {
        template: filename.input.value,
        project: project.name,
      });
    } catch (err) {
      preview.textContent = "";
      reportFailure(err);
    }
  }, 150);
  filename.input.addEventListener("input", refresh);
  filename.input.addEventListener("change", () =>
    updateProject(project.name, { filenameTemplate: filename.input.value }),
  );
  refresh();

  const previewRow = document.createElement("div");
  previewRow.className = "field";
  const previewLabel = document.createElement("span");
  previewLabel.className = "field__label";
  previewLabel.textContent = "Next note";
  previewRow.append(previewLabel, preview);

  const header = fieldRow("Header", project.header_template, "None");
  header.input.addEventListener("change", () =>
    updateProject(project.name, { headerTemplate: header.input.value }),
  );

  item.append(head, dir.row, filename.row, previewRow, header.row);
  item.append(projectDangerRow(project.name));
  return item;
}

// Removing a project. Two-step in place — the `note__discard`
// precedent, which is the only confirm shape this app has: first click arms,
// second click acts, and it disarms itself after four seconds. No dialog: a
// modal is a focus grab, and Sotone never takes focus (invariant 2).
//
// **The copy is the truth.** This removes an entry from a configuration file.
// The folder and every note in it stay exactly where they are, and the notes
// fall into the tree's "not in your projects" group — nothing can ever hide a
// draft. Recreating a project of the same name brings the
// group back.
let pendingProjectDelete = null;
let projectDeleteTimer = null;

function clearProjectDelete() {
  if (projectDeleteTimer !== null) clearTimeout(projectDeleteTimer);
  projectDeleteTimer = null;
  pendingProjectDelete = null;
}

function projectDangerRow(name) {
  const row = document.createElement("div");
  row.className = "projects__danger";

  const primed = pendingProjectDelete === name;
  const remove = linkButton(
    primed ? "sure? remove it" : "Delete project…",
    "Remove this project from Sotone. Nothing on disk is deleted.",
    () => {
      if (pendingProjectDelete === name) {
        clearProjectDelete();
        invoke("project_delete", { name }).catch(reportFailure);
        return;
      }
      clearProjectDelete();
      pendingProjectDelete = name;
      projectDeleteTimer = setTimeout(() => {
        clearProjectDelete();
        renderProjectPane();
      }, CONFIRM_WINDOW_MS);
      renderProjectPane();
    },
  );
  if (primed) remove.dataset.primed = "yes";

  const note = document.createElement("span");
  note.className = "projects__note";
  note.textContent =
    "Removes it from Sotone. The folder and every note in it stay on disk; its notes move to “not in your projects”, and recreating the project brings them back.";

  row.append(remove, note);
  return row;
}

// The per-project panel, rebuilt from the last projects event. Called on every
// one of them, so the panel is never older than the config.
function renderProjectPane() {
  const host = el("project-panel");
  host.replaceChildren();

  const project = selectedProject
    ? lastProjects.projects.find((p) => p.name === selectedProject) || null
    : null;
  if (!project) {
    const gone = document.createElement("p");
    gone.className = "muted";
    // Stale fields are worse than none: a folder edit committed against a
    // project that no longer exists would be a command aimed at nothing.
    gone.textContent = selectedProject
      ? `"${selectedProject}" is not one of your projects any more.`
      : "Pick a project in the notes tree to edit it.";
    host.append(gone);
    return;
  }
  host.append(projectPanel(project, lastProjects.active));
}

// The tree's per-project edit affordance. Same `data-current` switch the gear
// uses — one mechanism for what the main area is showing.
function openProjectPane(name) {
  selectedProject = name;
  renderProjectPane();
  showPane("pane-project");
}

function renderProjects(payload) {
  // A project menu is about a row in a tree this event re-renders.
  closeMenu();
  lastProjects = payload;

  // An armed "sure?" whose project is already gone is a question about
  // nothing; the same rule the tree's discard follows.
  if (
    pendingProjectDelete !== null &&
    !payload.projects.some((p) => p.name === pendingProjectDelete)
  ) {
    clearProjectDelete();
  }

  // Which project is active is read off the pane header's breadcrumb now (the
  // top bar that used to name it is gone) — `renderDrafts` below re-renders
  // the head, so there is nothing to set here.
  renderProjectPane();
  fillProjectPicker();
  // The wizard's project step and its summary both read this list — a created
  // project is what turns "Create project" into "Continue".
  if (wizardOpen) renderWizard();
  // The tree groups by project, and where a first save would land depends on the
  // active one, so both follow this event.
  renderDrafts(lastDrafts);
}

// ---------------------------------------------------------------------------
// Settings
//
// The same contract as the project panel: every control is an editor over the
// config file, every change is one command, and the `sotone://settings` event
// that comes back is the only confirmation — nothing here rewrites its own
// state optimistically. Every row sits in the design's card/row grammar
// and the view has its own tab sidebar; **not one command, event, payload
// field or refusal rule moved with it**, which is the point of listing them
// here rather than in the markup.
//
// Two things this view is careful about:
//
// * **Capture state comes from the backend.** "Press a key…" is a fact about a
//   helper process, not about this window, so a reload or a re-render cannot
//   strand the panel in it. While it is on, the rest of the pane is inert: one
//   question at a time.
// * **Nothing needs a restart, and nothing says it does.** The
//   microphone reconnects, a chosen model is loaded and swapped into the
//   worker, the language is a parameter on the model already loaded — so the
//   restart banner, the per-row restart markers and the pending-change
//   computation behind them are all gone. `model_loading` on the settings
//   event is the only state a change leaves behind, and like the capture state
//   it is the backend's fact, not this window's: a load that fails clears it by
//   sending the event again.
//
// And one it deliberately is *not* careful about: the refusal of every
// mutation while a recording is live belongs to the backend
// (`refuse_while_recording`), which answers with a notice. This window sends
// and re-renders from the event either way, so there is no second copy of that
// rule here to drift — the palette and the deleted-lines filter are the two
// commands the backend accepts mid-recording, and they are accepted there, not
// exempted here.
// ---------------------------------------------------------------------------

let lastSettings = {
  ptt: { mode: "ptt", token: "", label: "", enabled: true },
  toggle: { mode: "toggle", token: "", label: "", enabled: true },
  mic_substring: "",
  audio_cues: true,
  overlay: true,
  // Dark until the config says otherwise, and the chrome the markup already
  // ships with — the same two defaults index.html paints with.
  theme: "dark",
  platform: "windows",
  active_model: null,
  model_loading: null,
  models_dir: "",
  models: [],
  rejected_models: [],
  // The configured language, and the whole picker, both from the backend:
  // whisper's own table is read in Rust and a copy here would go stale.
  language: "auto",
  languages: [],
  capture: "idle",
  capture_mode: null,
};

// The input devices, as of the last time the Settings view was opened. `null`
// means "not asked yet" — devices come and go, so this is fetched on opening
// the pane rather than cached at startup.
let lastDevices = null;

const MODE_LABEL = { ptt: "Push-to-talk", toggle: "Toggle" };

// The "(system default)" entry's value. Empty string is the config's own
// spelling of "no preference", so the two cannot drift.
const SYSTEM_DEFAULT = "";

function capturing() {
  return lastSettings.capture === "capturing";
}

function sizeLabel(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "";
  const mib = bytes / (1024 * 1024);
  if (mib >= 1024) return `${(mib / 1024).toFixed(1)} GB`;
  return `${Math.round(mib)} MB`;
}

// Every control in the settings view except the ones that end a capture or
// leave it — the tabs included, so a rebind cannot be walked away from into
// another tab while the helper is still listening. The capture state came from
// the backend, so this cannot leave a control disabled with nothing listening:
// the next settings event turns it back on.
function setSettingsInert(inert) {
  // The attribute stays on the pane: it is what greys the rows, and the row
  // doing the asking is exempt from that.
  el("pane-settings").dataset.capturing = inert ? "yes" : "no";
  const shell = el("shell-settings");
  for (const control of shell.querySelectorAll("input, select, button")) {
    if (control.dataset.captureControl === "yes") continue;
    control.disabled = inert;
  }
}

// ---------------------------------------------------------------------------
// The design's row grammar, in one place
//
// A row is a 13px label, an optional 11.5px explanation under it, and the
// control pushed right. Everything rendered into a settings card goes through
// these three helpers so a row built in JS and a row written in index.html are
// the same object.
// ---------------------------------------------------------------------------

function setRow(tag = "div") {
  const row = document.createElement(tag);
  row.className = "set-row";
  const main = document.createElement("div");
  main.className = "set-row__main";
  const control = document.createElement("span");
  control.className = "set-row__control";
  row.append(main, control);
  return { row, main, control };
}

function rowLabel(main, text) {
  const label = document.createElement("span");
  label.className = "set-row__label";
  label.textContent = text;
  main.append(label);
  return label;
}

function rowNote(main, text, kind = "note") {
  const note = document.createElement("span");
  note.className = kind === "warn" ? "set-row__warn" : "set-row__note";
  if (kind === "warn") note.dataset.open = "yes";
  note.textContent = text;
  main.append(note);
  return note;
}

// A key is shown as a cap, never as an editable field: nobody types virtual
// key codes.
//
// Pass `onRebind` and the cap *is* the control — a real button that starts the
// capture, the way a game rebinds a control: click the key, then press the one
// you want. Leave it out and the cap is a read-only span: the question a
// running capture is asking is the only one built here today, and a readout
// that is only ever read (the title bar's hint) is markup, not a call. The form
// is decided by this argument and never by where the call came from, so a new
// caller has to say which of the two it wants.
function keycap(text, title, onRebind) {
  const cap = document.createElement(onRebind ? "button" : "span");
  cap.className = "keycap";
  cap.textContent = text;
  if (title) cap.title = title;
  if (onRebind) {
    // Focusable and Enter/Space-activated for free, which is the whole reason
    // this is a button and not a span with a click handler. No custom key
    // handling anywhere near it.
    cap.type = "button";
    cap.addEventListener("click", onRebind);
  }
  return cap;
}

function textAction(label, title, onClick) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "set-action";
  button.textContent = label;
  button.title = title;
  button.addEventListener("click", onClick);
  return button;
}

// One mode's row: what it does, the key it is bound to — which is also the
// control that rebinds it — and, in Settings, the switch that turns the mode
// off. Everything sends and nothing decides locally: the backend refuses to
// turn off the last mode, and the event it sends back is what redraws the row
// either way.
const MODE_NOTE = {
  ptt: "Records only while the key is held.",
  toggle: "Press once to start, again to stop.",
};

// What the cap says it will do, in its three states.
const REBIND_TITLE = "Click, then press the key or mouse button you want";
const REBIND_BUSY_TITLE = "Finish the key you are setting first";
const CANCEL_TITLE = "Stop listening and keep the key you have";

// The one key row, for Settings and for the wizard's key step. They differ in
// their words and in whether the mode can be switched off — nothing else, and
// least of all in how a capture reads, which is why this is one function.
function keyRow(hotkey, { label, note, modeSwitch = false }) {
  const { row, main, control } = setRow("li");
  rowLabel(main, label);
  if (note) rowNote(main, note);

  // Mid-rebind this one is the question: a dashed, empty slot rather than a
  // stale key that is about to stop being true — and not a button, because
  // there is nothing left to click here. Cancel is the way out.
  const awaiting = capturing() && lastSettings.capture_mode === hotkey.mode;
  // Another row is already asking. One question at a time, exactly as the
  // Settings pane enforces it — and said on the cap itself, because it is the
  // control now.
  const busy = capturing() && !awaiting;
  const cap = keycap(
    awaiting ? "Press a key…" : hotkey.label || "unset",
    awaiting ? "" : busy ? REBIND_BUSY_TITLE : REBIND_TITLE,
    awaiting
      ? null
      : () => {
          invoke("hotkey_capture_start", { mode: hotkey.mode }).catch(
            reportFailure,
          );
        },
  );
  if (awaiting) {
    cap.dataset.awaiting = "yes";
    // The row doing the asking: exempt from the pane's greying, and its cap
    // and Cancel exempt from `setSettingsInert`.
    row.dataset.awaiting = "yes";
    cap.dataset.captureControl = "yes";
  } else {
    // In Settings `setSettingsInert` disables this along with everything else;
    // the wizard has no such sweep, so the cap says it itself.
    cap.disabled = busy;
  }
  control.append(cap);
  if (awaiting) {
    const cancel = textAction("Cancel", CANCEL_TITLE, () => {
      invoke("hotkey_capture_cancel").catch(reportFailure);
    });
    cancel.dataset.captureControl = "yes";
    control.append(cancel);
  }
  // The wizard's rows stop here: a mode is switched off in Settings, which is
  // where the consequences of having none are explained.
  if (!modeSwitch) return row;

  const toggle = document.createElement("label");
  toggle.className = "toggle";
  const box = document.createElement("input");
  box.type = "checkbox";
  box.className = "toggle__box";
  box.checked = hotkey.enabled;
  box.setAttribute(
    "aria-label",
    `${MODE_LABEL[hotkey.mode] || hotkey.mode} enabled`,
  );
  // Named, not "this way of recording": the row's own label is the plainest
  // word for what the switch turns off.
  const modeName = MODE_LABEL[hotkey.mode] || hotkey.mode;
  box.title = hotkey.enabled
    ? `Turn ${modeName} off`
    : `Turn ${modeName} on`;
  box.addEventListener("change", () => {
    invoke("set_mode_enabled", {
      mode: hotkey.mode,
      enabled: box.checked,
    }).catch(reportFailure);
  });
  const knob = document.createElement("span");
  knob.className = "toggle__knob";
  knob.setAttribute("aria-hidden", "true");
  toggle.append(box, knob);

  control.append(toggle);
  return row;
}

// Settings' two rows: both modes, both switchable.
function hotkeyRow(hotkey) {
  return keyRow(hotkey, {
    label: MODE_LABEL[hotkey.mode] || hotkey.mode,
    note: MODE_NOTE[hotkey.mode],
    modeSwitch: true,
  });
}

function modelRow(model) {
  const { row, main, control } = setRow("li");
  if (model.active) row.dataset.active = "yes";

  rowLabel(main, model.name);
  rowNote(
    main,
    [
      sizeLabel(model.size_bytes),
      model.multilingual ? "multilingual" : "English only",
    ]
      .filter(Boolean)
      .join(" · "),
  );

  // A model this process is loading right now. The state is the backend's
  // (`model_loading` on the settings event), so a re-render cannot strand a row
  // in it and a load that fails clears it — quiet mono, not a spinner: it is a
  // fact, not an alarm.
  if (lastSettings.model_loading === model.name) {
    const loading = document.createElement("span");
    loading.className = "set-loading";
    loading.textContent = "loading…";
    loading.title = "Sotone is loading this model. It takes a moment.";
    control.append(loading);
    return row;
  }

  if (model.active) {
    const mark = document.createElement("span");
    mark.className = "set-mark";
    mark.textContent = "in use";
    control.append(mark);
  } else {
    // One action: removing a model is deleting the file in the
    // folder, which the user does themselves. Deliberately
    // still clickable while another model loads — the backend owns the
    // one-at-a-time rule and answers a second click with a notice, and a
    // control disabled by this window would be a second copy of that rule to
    // drift.
    control.append(
      textAction("Use this", "Load this model and use it from now on", () => {
        invoke("model_set_active", { name: model.name }).catch(reportFailure);
      }),
    );
  }

  return row;
}

function rejectedModelRow(rejected) {
  const { row, main } = setRow("li");
  row.classList.add("set-row--rejected");
  rowLabel(main, rejected.name);
  // The whole point of showing these: a bad download explains itself here
  // instead of silently failing to appear.
  rowNote(main, rejected.reason);
  return row;
}

// Two pickers, one list: Settings' row and the wizard's second step.
// The same command fills both and the same "(system default)" spelling means
// the same thing in both, because two implementations of "which microphone" is
// how the two screens would come to disagree.
function renderMicOptions() {
  for (const id of ["mic-select", "wiz-mic"]) fillMicOptions(el(id));
}

function fillMicOptions(select) {
  if (!select) return;
  select.replaceChildren();

  const fallback = document.createElement("option");
  fallback.value = SYSTEM_DEFAULT;
  fallback.textContent = "(system default)";
  select.append(fallback);

  const devices = lastDevices || [];
  for (const device of devices) {
    const option = document.createElement("option");
    option.value = device.name;
    option.textContent = device.is_default
      ? `${device.name} — system default`
      : device.name;
    select.append(option);
  }

  const pinned = lastSettings.mic_substring;
  // A pinned name that no device answers to is still the truth of the config,
  // so it stays on screen rather than silently reverting to the default.
  if (pinned && !devices.some((device) => device.name === pinned)) {
    const missing = document.createElement("option");
    missing.value = pinned;
    missing.textContent = `${pinned} — not connected`;
    select.append(missing);
  }
  select.value = pinned || SYSTEM_DEFAULT;
}

// The language picker. Options come from the settings event — whisper's
// own table, read in Rust — so this window keeps no list of its own, the same
// rule the model list follows.
function renderLanguageOptions() {
  const select = el("language-select");
  select.replaceChildren();

  const languages = lastSettings.languages || [];
  for (const language of languages) {
    const option = document.createElement("option");
    option.value = language.code;
    option.textContent = language.label;
    select.append(option);
  }

  // A configured code the running whisper does not know is still the truth of
  // the config, so it stays on screen rather than silently reverting — the same
  // rule the microphone row follows for a device that is not plugged in. Only
  // once a list has actually arrived, though: before the first settings event
  // there is nothing to be absent *from*, and accusing the default of being
  // unknown would be a lie for one frame.
  const chosen = lastSettings.language || "auto";
  const known = languages.some((language) => language.code === chosen);
  if (!known) {
    const unknown = document.createElement("option");
    unknown.value = chosen;
    unknown.textContent =
      languages.length > 0 ? `${chosen} — not a language whisper knows` : chosen;
    select.append(unknown);
  }
  select.value = chosen;

  // An English-only model ignores the setting entirely, and the active language
  // for one is "en" whatever the config says. The row stays usable — the
  // next model may be multilingual — and says what is true instead.
  const englishOnly = lastReady && lastReady.model_kind !== "multilingual";
  const note = el("language-note");
  note.dataset.open = englishOnly ? "yes" : "no";
  note.textContent = englishOnly
    ? `${lastReady.model} is an English-only model, so it transcribes English whatever this says.`
    : "";
}

function renderSettings(payload) {
  lastSettings = payload;

  // The two attributes on <html> that decide the palette and the chrome. First,
  // before anything below can paint in the wrong one.
  applyPlatform(payload.platform);
  applyTheme(payload.theme);

  // The transcript's hide-deleted filter is a config key, so
  // this payload is where the window learns it — at boot, and after anything
  // that rewrites the config.
  //
  // **Only if it changed**, which is what keeps this from fighting the toggle:
  // flipping the item re-renders locally and then invokes, and the settings
  // event that comes back carries the value we just wrote — so it lands here
  // as a no-op instead of drawing the same list a second time (or, mid-edit,
  // deferring one). `!!` because an older payload without the field must read
  // as the default — deleted lines are listed — rather than as `undefined`.
  const wantHidden = !!payload.hide_deleted;
  if (wantHidden !== hideDeleted) {
    hideDeleted = wantHidden;
    renderTranscript(transcript);
  }

  const hotkeys = el("hotkey-list");
  hotkeys.replaceChildren(hotkeyRow(payload.ptt), hotkeyRow(payload.toggle));

  // The question, in the same card as the two rows it is about. Its state is a
  // fact about the helper process and not about this window, so a reload or a
  // re-render cannot strand the view in it.
  const panel = el("capture-panel");
  panel.dataset.open = capturing() ? "yes" : "no";
  el("capture-label").textContent = capturing()
    ? `Press the key or button for ${
        MODE_LABEL[payload.capture_mode] || payload.capture_mode
      }…`
    : "";

  renderMicOptions();
  renderLanguageOptions();
  el("cue-device").textContent =
    (lastReady && lastReady.cues) || "no cue output device";
  el("cue-toggle").checked = payload.audio_cues;
  el("overlay-toggle").checked = payload.overlay;
  // `!!` because an older payload without the field must read as the
  // default — the X hides — rather than as `undefined`.
  el("close-quits-toggle").checked = !!payload.close_quits;

  // About → The app. From the backend like the platform above it:
  // the build knows its own version, and a number typed into index.html would
  // be wrong the first time nobody remembered to change it. Blank rather than
  // "unknown" if the field is missing — an older payload should leave a quiet
  // gap, not an alarming word.
  el("app-version").textContent = payload.version
    ? `Version ${payload.version}`
    : "";

  el("models-dir").textContent = payload.models_dir;
  const models = el("model-list");
  models.replaceChildren();
  if (payload.models.length === 0) {
    const { row, main } = setRow("li");
    rowLabel(main, "No models yet");
    // The empty state is the onboarding. No weights ship with Sotone.
    rowNote(
      main,
      "Sotone ships with none — put a GGML .bin file (the kind named like ggml-small.en.bin) in your models folder, then press Rescan below.",
    );
    models.append(row);
  }
  for (const model of payload.models) models.append(modelRow(model));
  for (const rejected of payload.rejected_models) {
    models.append(rejectedModelRow(rejected));
  }

  factList(
    el("settings-facts"),
    lastReady
      ? [
          ["Backend", lastReady.backend, "the backend this build was compiled for"],
          ["Microphone", lastReady.device, "the device this session opened"],
          ["Cue output", lastReady.cues || "unavailable", "cue output device"],
          ["Model", lastReady.model, lastReady.model_path],
          // The two rows the session pane's own facts list used to carry, moved
          // here rather than dropped when the transcript took that space.
          // The language that is actually in force is also in the overlay, which
          // is where it belongs; this is the long form.
          ["Language", lastReady.language, "whisper language"],
          ["Keys", lastReady.bindings, "push-to-talk and toggle bindings"],
        ]
      : [["Backend", "starting…", "Sotone is still starting up"]],
  );

  // Three readouts name the real binding — the title bar's key hint, the empty
  // transcript's line, and the invite — so all three follow this event.
  renderHotkeyHint();
  renderTranscript(transcript);
  // The first-run panel lists the same folder from the same payload, so it is
  // refreshed by the same event — a rescan that finds a new file updates it
  // with no restart. It lives outside `#shell-settings`, so the inerting below
  // never reaches it, and it has no capture control of its own: the repair
  // panel lists models and nothing else. The wizard's key step is where a
  // rebind happens in the empty phase, and it renders below.
  renderEmptyModels();
  // And the wizard, when it is the surface on screen: its mic picker, its key
  // caps and its model step are all drawn from this same event, which is what
  // makes a capture or a rescan show up there without a second channel.
  if (wizardOpen) renderWizard();
  // Last but one: it disables the controls the renders above just rebuilt.
  setSettingsInert(capturing());
  // And then the master switch, which only ever *adds* to that: the design's
  // rule is that a toggle owns the rows under it, so with the overlay off its
  // two rows go faint and stop taking input.
  renderOverlayRows(payload);
  // Last of all: the four steps above set values and flip `disabled`
  // in ways that leave no DOM mutation for the observer on each <select> to
  // see — `applyTheme` and `renderOverlayRows` assign a value, and the master
  // switch can re-disable a control that `setSettingsInert` has just enabled.
  // The render that owns all four says when it is finished rather than
  // anything watching for it.
  refreshPicks();
}

// Settings → Overlay's two subordinate rows.
//
// Both send and wait for the settings event like every other control here;
// neither decides anything locally, so a refusal on the backend — "not while a
// recording is running" — simply leaves them as they were.
function renderOverlayRows(payload) {
  const on = Boolean(payload.overlay);
  el("overlay-corner").value = payload.overlay_corner || "bottomLeft";
  renderRevealOptions(payload.reveal_seconds);

  for (const row of document.querySelectorAll('[data-subordinate="overlay"]')) {
    row.dataset.owned = on ? "yes" : "no";
    for (const control of row.querySelectorAll("select")) {
      // Never `= !on`: a rebind in progress has already disabled everything in
      // this view, and one question at a time outranks this one.
      control.disabled = control.disabled || !on;
    }
  }
}

// The reveal duration the config actually holds, which may not be one of the
// durations offered — the file is the source of truth and a hand-edit is
// allowed to say 7. The same rule the microphone and language rows use: show
// what is in force rather than the nearest thing on the list.
function renderRevealOptions(seconds) {
  const select = el("reveal-seconds");
  const value = String(Number(seconds) || 10);
  const options = () => Array.from(select.options);
  if (!options().some((option) => option.value === value)) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = `${value} seconds`;
    // In place, so the list still reads as a scale.
    const after = options().find(
      (existing) => Number(existing.value) > Number(value),
    );
    select.insertBefore(option, after || null);
  }
  select.value = value;
}

// Devices change while the app runs, so the list is fetched when the view is
// opened rather than cached at startup.
async function loadDevices() {
  try {
    lastDevices = await invoke("settings_devices");
  } catch (err) {
    lastDevices = [];
    reportFailure(err);
  }
  renderMicOptions();
}

// ---------------------------------------------------------------------------
// The first run — the empty phase
//
// The empty state is the onboarding. No model means no session, so this
// panel is the whole window: what a model is, where to get one, what is in the
// folder right now, and the three things that fix it — Open models folder,
// Rescan, Restart (there is no "Add model…": the user
// puts the file in the folder and Sotone reads it). It reuses the Settings
// commands rather than duplicating the pane; there is no navigation at all in
// this phase.
//
// Everything is in-page. The address is text plus a clipboard write — no link,
// no URL-opening command, no fetch (invariant 3): the browser that downloads
// the weights is the user's own.
// ---------------------------------------------------------------------------

// The canonical whisper.cpp model repository. Still under `ggerganov` — only
// the GitHub org moved to `ggml-org`, the Hugging Face repo did not.
const MODEL_URL = "https://huggingface.co/ggerganov/whisper.cpp";

// The whole of model management: open the folder,
// and read it again. Three surfaces offer both — Settings, the repair panel and
// the wizard's model step — and all three call these, because three copies of
// two `invoke`s is three places for one of them to drift.
//
// The reveal names a *target* Sotone resolves itself; this window never hands
// the backend a path, and there is no URL and no fetch anywhere near either of
// these (invariant 3). Neither writes anything: the file moves happen in the
// user's own file manager.
function revealModelsFolder() {
  invoke("project_reveal", { target: "models_dir", name: null }).catch(
    reportFailure,
  );
}

// A directory read that answers with `sotone://settings`, which is what redraws
// every list. Nothing optimistic happens here — as everywhere else in this
// window, the event that comes back is the render.
function rescanModels() {
  invoke("models_rescan").catch(reportFailure);
}

// How long the Copy button says "copied" before going back to itself.
const COPY_FEEDBACK_MS = 2000;

// What a restart would find, worked out from the settings payload alone.
//
// This mirrors `resolve_model` in shell.rs on purpose: a configured model wins
// when it is really in the folder, and otherwise exactly one model there is
// unambiguous. Anything else is still a question, and restarting into the same
// screen is not an answer.
function emptyState() {
  const models = lastSettings.models || [];
  const active = lastSettings.active_model || null;
  const chosen = active
    ? models.find((model) => model.name === active) || null
    : null;
  return {
    models,
    active,
    chosen,
    solved: Boolean(chosen) || (!active && models.length === 1),
  };
}

// What is still missing, for the disabled button's tooltip. A control that is
// dim for a reason has to be able to say the reason.
function restartBlockedBecause(state) {
  if (state.models.length === 0) {
    return "Put a model in your models folder first — Sotone has nothing to load.";
  }
  if (state.active && !state.chosen) {
    return `The chosen model "${state.active}" is not in this folder — pick one of the models above.`;
  }
  return "Choose which model to use first.";
}

// One model in the folder. Minimal on purpose: this is not the Settings view
// (no "in use" mark, no loading state), it is the shortest path from "I have a
// file" to "Sotone runs". It borrows Settings' row grammar the same way it
// borrows its commands — and the rejected rows beside it are literally the
// Settings renderer, so the two must read as one list.
function emptyModelRow(model, state) {
  const { row, main, control } = setRow("li");
  // The one a restart would load: the configured model, or the only model
  // there when nothing is configured.
  const willLoad =
    Boolean(state.chosen && state.chosen.name === model.name) ||
    (state.solved && !state.active);
  if (willLoad) row.dataset.active = "yes";

  rowLabel(main, model.name);
  rowNote(
    main,
    [
      sizeLabel(model.size_bytes),
      model.multilingual ? "multilingual" : "English only",
    ]
      .filter(Boolean)
      .join(" · "),
  );

  if (willLoad) {
    const mark = document.createElement("span");
    mark.className = "set-mark";
    mark.textContent = "will be used";
    control.append(mark);
  } else if (!state.solved) {
    // Offered only while the question is still open — with a model already
    // settled on, a second "use this one" would just be the Settings view again.
    control.append(
      textAction("Use this one", "Load this model when Sotone starts", () => {
        invoke("model_set_active", { name: model.name }).catch(reportFailure);
      }),
    );
  }

  return row;
}

// What the models folder holds right now, into whichever list is asking: the
// repair panel's, or the wizard's model step. One renderer, because
// the two screens are answering the same question about the same folder.
function fillModelList(list, state) {
  list.replaceChildren();
  const rejected = lastSettings.rejected_models || [];
  if (state.models.length === 0 && rejected.length === 0) {
    const nothing = document.createElement("li");
    nothing.className = "muted";
    nothing.textContent = "Nothing here yet.";
    list.append(nothing);
  }
  for (const model of state.models) {
    list.append(emptyModelRow(model, state));
  }
  // The same rows the Settings pane draws, and for the same reason: a file that
  // is not a usable model says so, with the validator's own words.
  for (const reject of rejected) {
    list.append(rejectedModelRow(reject));
  }
}

function renderEmptyModels() {
  const state = emptyState();
  el("empty-dir").textContent = lastSettings.models_dir || "";
  fillModelList(el("empty-models"), state);

  const restart = el("empty-restart");
  restart.disabled = !state.solved;
  restart.title = state.solved
    ? "Start Sotone again — a model is loaded at startup, so this is what puts your choice to work"
    : restartBlockedBecause(state);
}

// The model guide — the recommendation table, the footnotes and the address —
// exists once, in a `<template>`, and is cloned into both screens that have to
// explain what a speech model is. Called before anything wires a
// listener, because the Copy buttons it brings are wired by class.
function mountModelGuides() {
  const template = el("tmpl-model-guide");
  if (!template) return;
  for (const id of ["empty-guide", "wiz-model-guide"]) {
    const host = el(id);
    if (host) host.append(template.content.cloneNode(true));
  }
  for (const button of document.querySelectorAll(".model-url__copy")) {
    button.addEventListener("click", () => copyModelUrl(button));
  }
}

// The only clipboard write in this window, and it writes a constant from this
// file. Nothing is read from the clipboard and nothing is fetched.
async function copyModelUrl(button) {
  try {
    await navigator.clipboard.writeText(MODEL_URL);
    button.textContent = "copied";
    setTimeout(() => {
      button.textContent = "copy";
    }, COPY_FEEDBACK_MS);
  } catch (err) {
    // Some contexts refuse clipboard writes outright. The button did nothing
    // visible, so this has to be said — `warn`, which in the empty phase means
    // the panel's own copy of the message slot.
    handleNotice({
      level: "warn",
      message: `could not copy the address (${err}) — select it and copy it by hand`,
    });
  }
}

// ---------------------------------------------------------------------------
// The first-run wizard
//
// Seven steps in a 620×449 frame: welcome · microphone · notes folder · keys ·
// model · first project · ready. It runs in whatever phase the launch landed
// in, which decides how it *ends*: a machine seeded with a model launched
// `ready`, every live-apply path worked throughout, and finishing writes
// one config key; a fresh install launched `empty`, nothing exists to apply
// into, and finishing goes through `app_restart`.
//
// Two rules shape the code below:
//
// * **Nothing here decides anything the backend owns.** Every control sends a
//   command that already existed — `set_mic`, `project_pick_folder`,
//   `models_rescan`, `model_set_active`, `project_create` — and re-renders from
//   the event that comes back, exactly as Settings does. The two commands it adds
//   are questions, not decisions: the folder name a project would get, and the
//   notes folder to suggest.
// * **The sanitizer is not in this file.** "Creates <root>\<slug>\" comes from
//   `project_slug_preview` in Rust (the one-sanitizer rule). A JS copy would
//   be a second answer to what the folder is called, and the two would drift.
//
// The step index is in memory only: a reload lands on step one with every value
// re-derived from the settings, projects and status events. That is deliberate —
// the steps are idempotent, so revisiting one shows current truth rather than a
// remembered answer.
// ---------------------------------------------------------------------------

const WIZARD_STEPS = 7;

// The default filename template, as `config.rs` writes it into a fresh config
// and as `#new-project-name`'s sibling placeholder already spells it. Only ever
// handed *back* to Rust, which does the expanding: this is a string to preview
// with, not a second implementation of the template rules.
const WIZARD_TEMPLATE = "{project} {date} {time}.md";

// The project name the folder step's example path is drawn with, when there is
// no real one yet. Deliberately generic: an invented product name
// reads as something the user already owns, and the same rule that took those
// out of the placeholders takes them out of the sample path.
const WIZARD_EXAMPLE_PROJECT = "Project";

let wizardStep = 1;
let wizardOpen = false;
// Whether the two one-time questions have gone out (devices, the notes root).
let wizardAsked = false;
// The notes root: **wizard state, not configuration**. Nothing is written until
// the project step creates a project under it.
let wizardRoot = "";
// The folder name Rust says the typed project name would produce. Empty means
// there is no name yet, which is also what gates Create.
let wizardSlug = "";
// Whether the user has typed in the name field. A resumed wizard prefills it
// from the project that already exists, and must not then overwrite what is
// being typed on the next settings event.
let wizardNameTouched = false;

// Which separator this machine's paths use, read off the path itself rather
// than off `platform`: the root came from Rust already formatted, and a
// backslash in it is the only evidence that matters.
function pathSep(path) {
  return String(path).includes("\\") ? "\\" : "/";
}

function joinPath(root, name) {
  if (!root) return name;
  const sep = pathSep(root);
  return `${root.replace(/[\\/]+$/, "")}${sep}${name}`;
}

// Enter and leave. Idempotent: `renderStatus` calls it on every status event,
// and only the *transition* does the one-time work.
function showWizard(on) {
  if (on !== wizardOpen) {
    wizardOpen = on;
    // Windows 11 greys a caption button it cannot honour rather than hiding
    // it, and the wizard's window is not resizable — so maximize is disabled
    // and looks it. Restored the moment the wizard is behind us.
    el("win-maximize").disabled = on;
    if (on) askWizardQuestions();
  }
  if (on) renderWizard();
}

// The two things the wizard needs that no event carries. Once per run: devices
// change while the app runs, but this screen is seconds long, and the notes
// root is a constant of the machine.
async function askWizardQuestions() {
  if (wizardAsked) return;
  wizardAsked = true;
  // The same command, the same list and the same "(system default)" entry the
  // Settings picker uses.
  loadDevices();
  try {
    wizardRoot = await invoke("onboarding_notes_root");
  } catch (err) {
    reportFailure(err);
  }
  renderWizard();
  renderWizardExample();
}

// Move. `travel` is the direction the crossfade slides in, and the footer count
// moves with it (the design's motion table).
function wizardGoto(step, travel) {
  const next = Math.min(Math.max(step, 1), WIZARD_STEPS);
  if (next === wizardStep) return;
  wizardStep = next;
  views.onboarding.dataset.travel = travel;
  for (const section of el("wiz-steps").querySelectorAll(".wiz")) {
    section.dataset.current =
      Number(section.dataset.step) === wizardStep ? "yes" : "no";
  }
  // A step restarts its own animation by coming back from `display: none`; the
  // count never leaves the screen, so it needs the class taken off and put back
  // with a reflow in between.
  const count = el("wiz-count");
  count.classList.remove("is-moving");
  void count.offsetWidth;
  count.classList.add("is-moving");

  // The example path costs two commands, so it is refreshed on arrival rather
  // than on every render.
  if (wizardStep === 3) renderWizardExample();
  // Same shape for the model step: the user has very likely just been in
  // their file manager, and arriving on the step is the moment to re-read the
  // folder. The step's own Rescan button is what covers the case where they go
  // back to the file manager and return without leaving the step.
  if (wizardStep === 5) rescanModels();
  renderWizard();
  if (wizardStep === 6) {
    // DOM focus inside a window that already has it — never window activation
    // (invariant 2). The design draws this field focused with its caret shown.
    el("wiz-project").focus();
  }
}

function renderWizard() {
  el("wiz-root").textContent = wizardRoot;
  renderWizardKeys();
  renderWizardModel();
  renderWizardProject();
  renderWizardSummary();
  renderWizardFooter();
}

// Whether this launch has a session behind it. Only the model step turns on it
// now, and only because a model that is already loaded needs no restart — the
// keys step works in both phases (task 052: the capture helper lives outside
// the session, so a rebind in the empty phase spawns one of its own).
function wizardHasSession() {
  return Boolean(lastStatus) && lastStatus.phase === "ready";
}

function renderWizardFooter() {
  el("wiz-count").textContent = `${wizardStep} of ${WIZARD_STEPS}`;
  const back = el("wiz-back");
  // Back is a quiet text button from step two on, and never a chevron.
  back.dataset.open = wizardStep > 1 ? "yes" : "no";
  // The one thing that stops it is an open capture — a step's own requirement
  // is a reason not to go on, never a reason not to go back — and it stops it
  // in the grammar Next uses below: disabled, never hidden, carrying its
  // reason. Both buttons sit in `#view-empty`, which the Settings inert sweep
  // does not reach, so this render is the only thing holding them (task 054).
  const pinned = capturing() ? REBIND_BUSY_TITLE : "";
  back.dataset.live = pinned ? "no" : "yes";
  back.disabled = Boolean(pinned);
  back.title = pinned;

  const next = el("wiz-next");
  next.textContent = wizardPrimaryLabel();
  const blocked = wizardBlockedBecause();
  // Inert, never hidden and never removed: `data-live` carries the appearance
  // and `disabled` carries the behaviour, so a control that is dim for a reason
  // can also say the reason.
  next.dataset.live = blocked ? "no" : "yes";
  next.disabled = Boolean(blocked);
  next.title = blocked || "";
}

function wizardPrimaryLabel() {
  if (wizardStep === 1) return "Get started";
  if (wizardStep === WIZARD_STEPS) return "Open Sotone";
  // A resumed wizard — a reload or a relaunch after the project was made —
  // must not offer to create a second one of it.
  if (wizardStep === 6) return existingProject() ? "Continue" : "Create project";
  return "Continue";
}

// What is still missing on this step, as a sentence, or null when nothing is.
// Two steps have a requirement of their own — the model, and the name of the
// first project — and an open key capture holds every step there is.
function wizardBlockedBecause() {
  // Checked first, and without asking which step this is: a capture is the
  // backend's state, not the step's. Walk on while the question is open and the
  // row asking it leaves the screen with the helper still waiting for its one
  // press — the first key of the name typed on step 6 answers it instead, and
  // push-to-talk silently becomes that letter (task 054). Cancel, or press the
  // key you meant, is the way on. The busy cap's own sentence, from the one
  // constant, so the two readings of "wait" cannot drift apart.
  if (capturing()) return REBIND_BUSY_TITLE;
  if (wizardStep === 5) {
    if (wizardHasSession()) return null;
    const state = emptyState();
    // The rule is the repair panel's: at least one usable model, and
    // either exactly one of them or a choice already made.
    return state.solved ? null : restartBlockedBecause(state);
  }
  if (wizardStep === 6 && !existingProject() && !wizardSlug) {
    return "Type a name for your first project.";
  }
  return null;
}

// The project this wizard already has, if any — the active one, else the first.
function existingProject() {
  const projects = lastProjects.projects || [];
  const active = lastProjects.active;
  if (active && projects.some((project) => project.name === active)) {
    return active;
  }
  return projects.length > 0 ? projects[0].name : null;
}

// 2 · the microphone, and 4 · the keys ---------------------------------------

// The wizard's two key rows are the Settings rows: the same renderer, the same
// cap-is-the-control grammar, the same capture. What used to stand here was a
// second copy of the row with its Change disabled, because in the empty phase
// there was nothing to capture with — startup stopped before the helper
// process. Task 052 moved the capture helper out of the session, so the empty
// phase spawns its own one-shot helper and this row has nothing left to refuse.
function renderWizardKeys() {
  const keys = el("wiz-keys");
  if (!lastSettings.toggle || !lastSettings.ptt) {
    keys.replaceChildren();
    return;
  }
  keys.replaceChildren(
    keyRow(lastSettings.toggle, {
      label: "Toggle recording",
      note: "Press once to start, again to stop.",
    }),
    keyRow(lastSettings.ptt, {
      label: "Push to talk",
      note: "Records only while the key is held.",
    }),
  );
  // The question, in the card the two rows are in — the same row the Settings
  // pane raises, so a capture reads identically in both places.
  if (capturing()) {
    const { row, main, control } = setRow("li");
    row.classList.add("set-row--ask");
    row.dataset.open = "yes";
    rowLabel(
      main,
      `Press the key or button for ${
        MODE_LABEL[lastSettings.capture_mode] || lastSettings.capture_mode
      }…`,
    );
    control.append(
      textAction("Cancel", CANCEL_TITLE, () => {
        invoke("hotkey_capture_cancel").catch(reportFailure);
      }),
    );
    keys.append(row);
  }
}

// 3 · the notes folder -------------------------------------------------------

// What will exist on disk after the first recording: the project's folder under
// the root, and the note the default template would name. Both halves are
// rendered in Rust — the slug by `project_slug_preview`, the filename by
// `filename_preview` — because both are rules this window does not own.
async function renderWizardExample() {
  const name = existingProject() || WIZARD_EXAMPLE_PROJECT;
  try {
    const [slug, file] = await Promise.all([
      invoke("project_slug_preview", { name }),
      invoke("filename_preview", { template: WIZARD_TEMPLATE, project: name }),
    ]);
    el("wiz-example").textContent = `${joinPath(wizardRoot, slug)}${pathSep(
      wizardRoot,
    )}${file}`;
  } catch (err) {
    el("wiz-example").textContent = "";
    reportFailure(err);
  }
}

// 5 · the model --------------------------------------------------------------

function renderWizardModel() {
  const ready = wizardHasSession();
  const reason = el("wiz-model-reason");
  const empty = Boolean(lastStatus) && lastStatus.phase === "empty";
  reason.dataset.open = empty && lastStatus.detail ? "yes" : "no";
  reason.textContent = empty ? lastStatus.detail : "";

  // Already loaded: the step collapses to the one row that is worth saying,
  // with the folder actions still underneath — putting a second model in the
  // folder is allowed, it simply is not what this step is waiting for.
  el("wiz-model-ready").dataset.open = ready ? "yes" : "no";
  el("wiz-model-summary").textContent =
    ready && lastReady ? `${lastReady.model} · ready, offline` : "";
  el("wiz-model-guide").dataset.open = ready ? "no" : "yes";

  el("wiz-dir").textContent = lastSettings.models_dir || "";
  const list = el("wiz-models");
  if (ready) {
    // The collapse drops the *choice* — a model is already loaded — but not
    // the rejects: a `.bin` that is not a model has to explain itself on
    // every surface that lists the folder, or a rescan here would answer a
    // dropped-in file with silence.
    list.replaceChildren(
      ...(lastSettings.rejected_models || []).map((reject) =>
        rejectedModelRow(reject),
      ),
    );
  } else {
    fillModelList(list, emptyState());
  }
}

// 6 · the first project ------------------------------------------------------

function renderWizardProject() {
  const field = el("wiz-project");
  const existing = existingProject();
  // A resumed wizard shows the project that is already there rather than an
  // empty field that invites a duplicate.
  if (!wizardNameTouched && existing && field.value !== existing) {
    field.value = existing;
    refreshWizardSlug();
  }
  renderWizardSlug();
}

function renderWizardSlug() {
  el("wiz-slug").textContent = wizardSlug
    ? `Creates ${joinPath(wizardRoot, wizardSlug)}${pathSep(wizardRoot)}`
    : "";
}

// The echo under the field. Debounced like the filename preview it follows: a
// typed word is one round trip, not one per keystroke.
const refreshWizardSlug = debounce(async () => {
  const name = el("wiz-project").value;
  try {
    wizardSlug = await invoke("project_slug_preview", { name });
  } catch (err) {
    wizardSlug = "";
    reportFailure(err);
  }
  renderWizardSlug();
  // The Create button is gated on this, so the footer has to hear about it.
  renderWizardFooter();
}, 150);

async function createWizardProject() {
  const name = el("wiz-project").value.trim();
  if (!name || !wizardSlug) return;
  try {
    // `project_create` makes it active itself, and answers with
    // `sotone://projects` — which is what makes the button read Continue if the
    // user comes back to this step.
    const outcome = await invoke("project_create", {
      name,
      notesDir: joinPath(wizardRoot, wizardSlug),
    });
    // The step only advances on an acceptance. A refusal resolves too, and
    // taking that for a yes lands the user on "ready" with no project — which
    // is what a folder that could not be created (a read-only or unplugged
    // drive) does. The notice says why; the step stays where they can fix it.
    if (outcome !== "created") return;
  } catch (err) {
    reportFailure(err);
    return;
  }
  wizardGoto(7, "forward");
}

// 7 · ready ------------------------------------------------------------------

// Only what the user actually chose, read back. Nothing here is a control: the
// last screen is a statement, and the one action is the footer's.
function renderWizardSummary() {
  const rows = [
    ["Toggle key", keyLabel(lastSettings.toggle), true],
    ["Push to talk", keyLabel(lastSettings.ptt), true],
    ["Notes folder", wizardNotesFolder(), true],
    ["Microphone", wizardMicLabel(), false],
    ["Model", wizardModelLabel(), false],
  ];
  const list = el("wiz-summary");
  list.replaceChildren();
  for (const [label, value, mono] of rows) {
    const { row, main, control } = setRow("li");
    rowLabel(main, label);
    control.classList.add(mono ? "set-row__value" : "wiz-value");
    control.textContent = value;
    list.append(row);
  }
}

function keyLabel(hotkey) {
  if (!hotkey) return "unset";
  if (!hotkey.enabled) return "off";
  return hotkey.label || "unset";
}

// The folder the first note will land in. Once the project exists that is a
// fact in the config rather than a prediction, so it wins.
function wizardNotesFolder() {
  const name = existingProject();
  const project = (lastProjects.projects || []).find((p) => p.name === name);
  if (project && project.notes_dir) {
    return `${project.notes_dir.replace(/[\\/]+$/, "")}${pathSep(
      project.notes_dir,
    )}`;
  }
  if (!wizardSlug) return wizardRoot;
  return `${joinPath(wizardRoot, wizardSlug)}${pathSep(wizardRoot)}`;
}

function wizardMicLabel() {
  if (lastSettings.mic_substring) return lastSettings.mic_substring;
  // With nothing pinned, the honest answer on a running session is the device
  // that is actually open.
  if (lastReady && lastReady.device) return lastReady.device;
  return "system default";
}

function wizardModelLabel() {
  if (wizardHasSession() && lastReady) {
    return `${lastReady.model} · ready, offline`;
  }
  const state = emptyState();
  const name =
    (state.chosen && state.chosen.name) ||
    (state.solved && state.models[0] && state.models[0].name) ||
    "";
  return name ? `${name} · restarts to load` : "none yet";
}

// Leaving --------------------------------------------------------------------

function wizardNext() {
  if (wizardBlockedBecause()) return;
  if (wizardStep === 6 && !existingProject()) {
    createWizardProject();
    return;
  }
  if (wizardStep === WIZARD_STEPS) {
    finishWizard();
    return;
  }
  wizardGoto(wizardStep + 1, "forward");
}

// The two endings. Which one this is was decided at launch, not here: a machine
// that came up `empty` has no session for the choices to reach, so the marker
// the backend just wrote is spent by the *next* process — and this one asks for
// it. A machine that came up `ready` has been applying everything live all
// along, and the status that comes back is what swaps the view.
async function finishWizard() {
  const restarting = Boolean(lastStatus) && lastStatus.phase === "empty";
  try {
    await invoke("onboarding_finish");
  } catch (err) {
    reportFailure(err);
    return;
  }
  if (restarting) {
    // Never returns on success — the process is replaced — so there is nothing
    // to await and nothing to render afterwards.
    invoke("app_restart").catch(reportFailure);
  }
}

// ---------------------------------------------------------------------------
// The two create-a-project forms
//
// Creating a project is the Godot dialog: every creation surface pairs
// its folder picker with a switch saying whether the project gets a folder of
// its own inside the picked one — on by default — and an echo naming the exact
// folder the project will get. The rule was earned by a user picking their
// notes *root*, watching it become one project's home, and then renaming that
// project, which carried every other project's folder with it.
//
// Two rules, both older than these forms:
//
// * **The window composes; the backend's contract is unchanged.**
//   `project_create` takes `{name, notesDir}` and always has. The wizard has
//   always composed `root + slug` — these two forms are catching up with it,
//   not changing the command.
// * **The sanitizer is not in this file.** The folder name comes from
//   `project_slug_preview` in Rust (the one-sanitizer rule). A JS copy would
//   be a second answer to what the folder is called, and the two would drift.
//
// Both surfaces are DOM inside panes and popups that already existed: nothing
// here shows, raises or focuses a window (invariant 2).
// ---------------------------------------------------------------------------

// The file-safe folder name Rust says each form's typed name would produce.
// Empty means there is no usable name yet, which is also what leaves the echo
// blank.
const createSlug = { "new-project": "", "no-project": "" };

function createForm(prefix) {
  return {
    name: el(`${prefix}-name`),
    dir: el(`${prefix}-dir`),
    subfolder: el(`${prefix}-subfolder`),
    echo: el(`${prefix}-echo`),
  };
}

// The folder the project actually gets, and the *only* place this window works
// it out — the echo and the Create button read the same answer, so what the
// user was shown is what gets created.
//
// Switch on: a folder of the project's own name inside the picked one. Off: the
// picked folder itself, which is how a user points a project at a home that
// already exists. No slug yet means no answer yet, never the root as a
// fallback.
function composeNotesDir(picked, slug, subfolder) {
  if (!picked) return "";
  if (!subfolder) return picked;
  return slug ? joinPath(picked, slug) : "";
}

// The echo under the folder row, in the wizard's grammar word for word
// (`renderWizardSlug`). Empty until there is both a name and a folder: a path
// echoed before the user has said what the project is called is a promise about
// a folder they have not chosen yet.
function renderCreateEcho(prefix) {
  const { name, dir, subfolder, echo } = createForm(prefix);
  const picked = dir.value.trim();
  const notesDir = composeNotesDir(
    picked,
    createSlug[prefix],
    subfolder.checked,
  );
  echo.textContent =
    name.value.trim() && notesDir
      ? `${subfolder.checked ? "Creates" : "Uses"} ${notesDir}${pathSep(picked)}`
      : "";
}

// Debounced exactly like `refreshWizardSlug`, and for the same reason: a typed
// word is one round trip, not one per keystroke.
function makeSlugRefresh(prefix) {
  return debounce(async () => {
    try {
      createSlug[prefix] = await invoke("project_slug_preview", {
        name: el(`${prefix}-name`).value,
      });
    } catch (err) {
      createSlug[prefix] = "";
      reportFailure(err);
    }
    renderCreateEcho(prefix);
  }, 150);
}

const refreshCreateSlug = {
  "new-project": makeSlugRefresh("new-project"),
  "no-project": makeSlugRefresh("no-project"),
};

// Opening a form puts the switch back to the default. Never
// remembered: a switch that stayed off from the last create would make "bind
// the folder I picked, whatever it is" sticky, which is the defect the switch
// exists to prevent. The fields themselves are left alone — a create that failed
// leaves its values where the user can fix them, which is the pane form's
// existing rule.
function openCreateForm(prefix) {
  el(`${prefix}-subfolder`).checked = true;
  refreshCreateSlug[prefix]();
  renderCreateEcho(prefix);
}

// After an accepted create: the fields go back to empty and the switch back to
// on, together.
function clearCreateForm(prefix) {
  const { name, dir, subfolder } = createForm(prefix);
  name.value = "";
  dir.value = "";
  subfolder.checked = true;
  createSlug[prefix] = "";
  renderCreateEcho(prefix);
}

// What Create sends. The slug is re-asked for rather than read off the last
// debounce: a click landing inside those 150 ms must not send the picked folder
// as though the switch were off, which is precisely the accident this prevents.
async function createNotesDir(prefix) {
  const { name, dir, subfolder } = createForm(prefix);
  const picked = dir.value.trim();
  if (!picked || !subfolder.checked) return composeNotesDir(picked, "", false);
  createSlug[prefix] = await invoke("project_slug_preview", {
    name: name.value,
  });
  renderCreateEcho(prefix);
  return composeNotesDir(picked, createSlug[prefix], true);
}

// ---------------------------------------------------------------------------
// The pick-or-create stop
//
// Raised by a save that had no project to save into. In-page DOM, never a native
// dialog (invariant 2), and torn down when the thing it is asking about moves —
// on record: a popup whose confirm button relaxes a guard must not
// survive a change of subject.
// ---------------------------------------------------------------------------

// Whether the question is on screen, and which draft it is about. Two variables
// because the draft id is legitimately `null` — a save can be asked for before
// any draft exists — and "no draft" must not read as "nothing is open".
let noProjectOpen = false;
let noProjectFor = null;

function fillProjectPicker() {
  const select = el("no-project-select");
  const chosen = select.value;
  select.replaceChildren();
  for (const project of lastProjects.projects) {
    const option = document.createElement("option");
    option.value = project.name;
    option.textContent = project.name;
    select.append(option);
  }
  if (lastProjects.projects.some((p) => p.name === chosen)) select.value = chosen;
  else if (lastProjects.active) select.value = lastProjects.active;
  // Nothing to pick from is a real state on a fresh install; the create half of
  // the popup is the whole answer then. `data-empty`, not `hidden`: `.field`
  // sets `display`, and an author display rule beats `[hidden]`.
  el("no-project-pick").dataset.empty =
    lastProjects.projects.length === 0 ? "yes" : "no";
}

function openNoProject() {
  noProjectOpen = true;
  noProjectFor = lastDrafts.active_id;
  fillProjectPicker();
  // The create half starts from the default every time it opens.
  openCreateForm("no-project");
  el("no-project").dataset.open = "yes";
}

function closeNoProject() {
  noProjectOpen = false;
  noProjectFor = null;
  el("no-project").dataset.open = "no";
}

// ---------------------------------------------------------------------------
// The name-clash question
//
// A drop moves the note's `.md` into the target project's folder. When that
// folder already holds a note of that name the backend stops **before writing
// anything** and asks here, which is what makes "ask each time" a promise that
// no note is ever written over (invariant 4).
//
// Two answers and no third: Keep both re-sends the same drop with the answer
// attached, Cancel sends nothing. In-page DOM like every other question this
// app asks — no native dialog, no focus taken from whatever is being tested
// (invariant 2).
//
// The window remembers nothing about the drop: the event carries the draft and
// the destination back, so the answer is built from what the backend said
// rather than from a variable that could have gone stale while the question was
// on screen.
// ---------------------------------------------------------------------------

// The open question — the event's payload — or null.
let noteClash = null;

function openNoteClash(payload) {
  noteClash = payload;
  const where = payload.project || "that project";
  el("note-clash-detail").textContent =
    `"${payload.name}" is already in "${where}". Nothing has been moved.`;
  // What Keep both would call it, when the backend could name one. A folder
  // with no free number left says so instead, and the button goes with it:
  // offering an answer that cannot be carried out is its own kind of failure.
  const keep = el("note-clash-keep");
  if (payload.suggestion) {
    el("note-clash-keeps").textContent =
      `Keep both saves this one as "${payload.suggestion}".`;
    keep.disabled = false;
    keep.title = `Move it in as "${payload.suggestion}"`;
  } else {
    el("note-clash-keeps").textContent =
      "There is no free name left in that folder to number this one under.";
    keep.disabled = true;
    keep.title = "There is no free name left in that folder";
  }
  el("note-clash").dataset.open = "yes";
}

function closeNoteClash() {
  noteClash = null;
  el("note-clash").dataset.open = "no";
}

// ---------------------------------------------------------------------------
// The notes tree
//
// One collapsible group per project, one row per draft. It replaces both the
// flat drafts list and the Projects tab: notes exist only within projects,
// so a tree of projects is the notes list.
// ---------------------------------------------------------------------------

// The draft whose Discard button is primed, and the timer that un-primes it.
// One at a time: priming a second row cancels the first, so there is never more
// than one loaded gun on screen.
let pendingDiscard = null;
let pendingTimer = null;

// The last payload, so a purely local change (arming a discard) can re-render
// without inventing state the backend has not confirmed.
let lastDrafts = {
  drafts: [],
  rejected: [],
  active_id: null,
  default_save_dir: "",
};

function clearPending() {
  pendingDiscard = null;
  if (pendingTimer !== null) {
    clearTimeout(pendingTimer);
    pendingTimer = null;
  }
}

function countLabel(n) {
  return n === 1 ? "1 line" : `${n} lines`;
}

// Sort order is a view preference, not configuration: it lives in localStorage
// and deliberately never reaches the config file. Nothing leaves the machine
// either way (invariant 3).
const SORT_KEY = "sotone.draft_sort";

// Newest first. `created_at` is RFC3339 from the backend; the id is a ulid,
// which already sorts by creation time, so it serves as both the tiebreak and
// the fallback when a timestamp will not parse.
function byNewest(a, b) {
  const ta = Date.parse(a.created_at);
  const tb = Date.parse(b.created_at);
  if (!Number.isNaN(ta) && !Number.isNaN(tb) && ta !== tb) return tb - ta;
  if (a.id === b.id) return 0;
  return a.id < b.id ? 1 : -1;
}

// The design's four: newest · oldest · name · line count. The old "project"
// order died with the grouping — the tree already puts a project's notes
// together, which is the whole of what that option did. A stored preference
// naming it falls through `loadSort`'s check and lands on "newest".
const SORTS = {
  newest: byNewest,
  oldest: (a, b) => byNewest(b, a),
  // What the row actually says, so the order matches what the eye reads.
  name: (a, b) =>
    noteLabel(a).localeCompare(noteLabel(b), undefined, {
      sensitivity: "base",
      numeric: true,
    }) || byNewest(a, b),
  lines: (a, b) => b.line_count - a.line_count || byNewest(a, b),
};

// Which groups the user has folded shut. A view preference like the sort order,
// so it lives in localStorage and never reaches the config file.
// Keys are project names, and "" for the no-project group; absence
// means open, so a fresh install shows everything.
//
// The drafts *filter* dropdown that used to live beside the sort is gone:
// grouping answers the question it was asked. The rule it carried — the active
// note is visible whatever is selected, because a note lines are landing in must
// never look lost — is now carried by auto-expanding the group that holds it,
// below. The old `sotone.draft_filter` key is simply left where it is; nothing
// reads it, and clearing another app's storage is not this window's business.
const COLLAPSED_KEY = "sotone.tree_collapsed";

function loadCollapsed() {
  try {
    const stored = JSON.parse(localStorage.getItem(COLLAPSED_KEY) || "[]");
    if (!Array.isArray(stored)) return new Set();
    return new Set(stored.filter((key) => typeof key === "string"));
  } catch {
    // Storage can be unavailable or hold junk from an older shape; an
    // all-expanded tree is a fine answer to either.
    return new Set();
  }
}

let collapsed = loadCollapsed();

function saveCollapsed() {
  try {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsed]));
  } catch {
    // Not persisting a view preference is survivable; the session keeps it.
  }
}

function loadSort() {
  let stored = null;
  try {
    stored = localStorage.getItem(SORT_KEY);
  } catch {
    // Storage can be unavailable (blocked or full); a missing preference is not
    // worth failing the panel over.
  }
  return Object.prototype.hasOwnProperty.call(SORTS, stored) ? stored : "newest";
}

let sortMode = loadSort();

// Local and short. The backend sends RFC3339 with the offset; what "short"
// means is the user's locale's business, not the backend's.
function whenLabel(rfc3339) {
  const at = new Date(rfc3339);
  if (Number.isNaN(at.getTime())) return rfc3339;
  return at.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

// What a note is called in the tree: the file it is bound to, because that is
// the name the user gave it, and the moment it was started before there is one.
function noteLabel(draft) {
  if (draft.saved_path) {
    const base = draft.saved_path.split(/[\\/]/).pop() || draft.saved_path;
    return base.replace(/\.md$/i, "");
  }
  return whenLabel(draft.created_at);
}

// The same name with its extension, for the pane header's breadcrumb: there it
// stands for the file on disk (`project / note.md`, in mono), and the extension
// is part of what it is.
function noteFileLabel(draft) {
  if (draft.saved_path) {
    return draft.saved_path.split(/[\\/]/).pop() || draft.saved_path;
  }
  return whenLabel(draft.created_at);
}

function noteRow(draft, activeId) {
  const item = document.createElement("li");
  item.className = "note";
  // Kept for the same reason `.line` carries one: when a draft misbehaves, the
  // id is the only handle on it in the store.
  item.dataset.id = draft.id;
  const active = draft.id === activeId;
  if (active) item.dataset.active = "yes";

  // The drag affordance the design draws at the row's left. Cross-project drag
  // came later; this is the grip, drawn where that gesture will live.
  const grip = document.createElement("span");
  grip.className = "note__grip";
  grip.setAttribute("aria-hidden", "true");
  for (let dot = 0; dot < 6; dot += 1) {
    grip.append(document.createElement("i"));
  }
  item.append(grip);

  // While a term is active the row reports how many of its lines matched,
  // replacing the line count — the tree's numbers and the pane's
  // are then the same number, which is what stops them disagreeing.
  const count = document.createElement("span");
  count.className = "note__count";
  const found = searching() ? matchCount(draft.id) : null;
  count.textContent = String(found === null ? draft.line_count : found);
  count.title =
    found === null ? countLabel(draft.line_count) : matchLabel(found);

  // Mid-rename this row *is* the field: the name becomes
  // an input where it stood, with the hint under it. It replaces the row's
  // button rather than sitting inside it — a field inside a button is a field
  // whose keystrokes belong to the button — so while the field is open the row
  // opens nothing and offers no tools: it is one question, with one answer.
  if (renamingHere("note", "row", draft.id)) {
    const box = document.createElement("span");
    box.className = "note__open note__open--renaming";
    box.append(renameFieldNode(), count);
    item.append(box);
    return item;
  }

  const open = document.createElement("button");
  open.type = "button";
  open.className = "note__open";
  open.title = [
    whenLabel(draft.created_at),
    countLabel(draft.line_count),
    draft.saved_path || "not saved yet",
  ].join(" · ");

  // The live dot rides on every active row and is shown by CSS only while the
  // tree says a recording is on — so it cannot be left behind by a render that
  // happened before the recording stopped.
  const live = document.createElement("span");
  live.className = "note__live";
  live.title = "Sotone is recording into this note";
  open.append(live);

  const label = document.createElement("span");
  label.className = "note__label";
  label.textContent = noteLabel(draft);

  open.append(label, count);

  if (draft.dirty) {
    const dot = document.createElement("span");
    dot.className = "note__dot";
    dot.title = "unsaved changes";
    open.append(dot);
  }

  open.addEventListener("click", () => openNote(draft, active));

  const tools = document.createElement("span");
  tools.className = "note__tools";

  // A note's name is its file's name, so only a bound note has one to change.
  // An unbound draft's label is the date it was started; there is
  // no file to rename and no name to keep. Hidden while a recording is live,
  // like every other pencil.
  if (draft.saved_path && renameAllowed()) {
    tools.append(
      pencilButton(`Rename ${noteLabel(draft)}`, () =>
        beginRename("note", "row", draft.id, noteLabel(draft)),
      ),
    );
  }

  // Only a note that is actually on disk can be shown. The command takes the
  // draft's id, never a path from this window: the backend resolves the file
  // itself, so this cannot become an open-anything affordance.
  if (draft.saved_path) {
    tools.append(
      linkButton("show", `Show ${draft.saved_path} in your file manager`, () => {
        invoke("project_reveal", { target: "draft", name: draft.id }).catch(
          reportFailure,
        );
      }),
    );
  }

  const discard = document.createElement("button");
  discard.type = "button";
  discard.className = "note__discard linkish";
  const primed = pendingDiscard === draft.id;
  discard.textContent = primed ? "sure?" : "discard";
  if (primed) discard.dataset.primed = "yes";

  discard.addEventListener("click", async () => {
    if (pendingDiscard === draft.id) {
      clearPending();
      await invoke("draft_discard", { id: draft.id });
      return;
    }
    // First click only arms it. No confirm() anywhere: a modal dialog is a
    // focus grab, and Sotone never takes focus (invariant 2).
    armDiscard(draft.id);
  });
  tools.append(discard);

  item.append(open, tools);

  // The two gestures on a note row. The menu is a question and opens
  // whatever the state; the drag is gated, and both live in their own
  // sections below.
  item.addEventListener("contextmenu", (event) => {
    openRowMenu("note", draft.id, event, item);
  });
  makeNoteDraggable(item, draft);
  return item;
}

// Resuming a note, from the row's own button and from the menu's Open — one
// function so the two cannot come to mean different things.
function openNote(draft, active) {
  clearPending();
  renderDrafts(lastDrafts);
  // Clicking a note means "show me it", so the main area comes back from
  // Settings or a project's fields to the transcript.
  showPane("pane-session");
  if (active) return;
  invoke("draft_open", { id: draft.id }).catch(reportFailure);
}

// Arm the row's two-step discard: the first click on the row's own control, and
// the menu's `Discard…`, are the same act. **A menu never destroys anything on
// its own click** — it arms the question the row already asks, and
// the row's second click is still the only thing that sends `draft_discard`.
function armDiscard(id) {
  clearPending();
  pendingDiscard = id;
  pendingTimer = setTimeout(() => {
    clearPending();
    renderDrafts(lastDrafts);
  }, CONFIRM_WINDOW_MS);
  renderDrafts(lastDrafts);
}

function rejectedRow(rejected) {
  const item = document.createElement("li");
  item.className = "note note--rejected";
  const path = document.createElement("span");
  path.className = "note__label";
  path.textContent = rejected.path;
  const reason = document.createElement("span");
  reason.className = "note__meta";
  reason.textContent = rejected.reason;
  item.append(path, reason);
  return item;
}

// The groups the tree shows, in the order it shows them: the active project
// first (it is where the next line lands), then the rest of the config's
// projects in the order the config lists them, then any project a *draft* names
// that the config no longer has, then the no-project group if there is one.
//
// The two trailing kinds are not tidiness — they are the "never hide a draft"
// rule: a row that vanishes from this list is indistinguishable from lost work,
// so a draft whose project was hand-deleted from the config still gets a group
// to sit in, marked as what it is.
function treeGroups(drafts) {
  const names = lastProjects.projects.map((project) => project.name);
  const known = new Set(names);
  const ordered =
    lastProjects.active && known.has(lastProjects.active)
      ? [lastProjects.active, ...names.filter((n) => n !== lastProjects.active)]
      : names;

  const groups = new Map();
  for (const name of ordered) {
    groups.set(name, { key: name, label: name, kind: "project", notes: [] });
  }

  const missing = new Map();
  let none = null;
  for (const draft of drafts) {
    const key = draft.project || "";
    if (key === "") {
      none = none || { key: "", label: "no project", kind: "none", notes: [] };
      none.notes.push(draft);
    } else if (groups.has(key)) {
      groups.get(key).notes.push(draft);
    } else {
      if (!missing.has(key)) {
        missing.set(key, { key, label: key, kind: "missing", notes: [] });
      }
      missing.get(key).notes.push(draft);
    }
  }

  const all = [...groups.values(), ...missing.values()];
  if (none) all.push(none);
  // Sort inside each group. "project" degrades to
  // newest within a group by itself — every note in there has the same project —
  // which is exactly what it should do.
  for (const group of all) group.notes.sort(SORTS[sortMode]);
  return all;
}

// One group and its notes. Returns the active note's row alongside the node so
// the caller can scroll to it without going back through the DOM.
function groupNode(group, activeId) {
  const item = document.createElement("li");
  item.className = "group";
  item.dataset.kind = group.kind;
  if (group.kind === "project" && group.key === lastProjects.active) {
    item.dataset.active = "yes";
  }

  // A group holding the note lines are landing in stays open, whatever the
  // stored preference says: hiding the active note reads as lost work, and that
  // rule outranks a view preference (the rule the old filter carried, which this
  // tree inherits). The toggle says why rather than springing back open under
  // the cursor.
  const holdsActive = group.notes.some((draft) => draft.id === activeId);
  // Groups render **expanded** while a term is active, and `sotone.tree_collapsed`
  // is neither read nor written by search rendering — a filtered
  // tree hiding its own matches inside a folded group would be a lie about what
  // matched. Restoring on exit needs no code: the normal renderer re-reads the
  // preference the moment the term goes.
  const found = searching();
  const open = found || holdsActive || !collapsed.has(group.key);
  item.dataset.open = open ? "yes" : "no";

  const row = document.createElement("div");
  row.className = "group__row";

  // Mid-rename the row's label is the field: the caret
  // stays where it was, and the toggle button is left out entirely — an input
  // inside a button is an input whose keystrokes belong to the button, and
  // collapsing a group you are renaming is not a gesture anyone means to make.
  if (renamingHere("project", "row", group.key)) {
    const caret = document.createElement("span");
    caret.className = "group__caret";
    caret.setAttribute("aria-hidden", "true");
    row.append(caret, renameFieldNode());
    const body = document.createElement("div");
    body.className = "group__body";
    const notes = document.createElement("ul");
    notes.className = "group__notes";
    let renamingActiveRow = null;
    for (const draft of group.notes) {
      const node = noteRow(draft, activeId);
      if (draft.id === activeId) renamingActiveRow = node;
      notes.append(node);
    }
    body.append(notes);
    item.append(row, body);
    return { item, activeRow: renamingActiveRow };
  }

  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "group__toggle";
  toggle.setAttribute("aria-expanded", open ? "true" : "false");
  toggle.disabled = found || holdsActive;
  toggle.title = found
    ? "Groups stay open while a search is active"
    : holdsActive
      ? "This holds the note Sotone is writing into, so it stays open"
      : open
        ? `Collapse ${group.label}`
        : `Expand ${group.label}`;
  // A CSS triangle that turns 90° when the group opens — the caret and the
  // rows move in the same 180ms beat, which is why neither is drawn in JS.
  const caret = document.createElement("span");
  caret.className = "group__caret";
  caret.setAttribute("aria-hidden", "true");
  const name = document.createElement("span");
  name.className = "group__name";
  name.textContent = group.label;
  toggle.append(caret, name);
  toggle.addEventListener("click", () => {
    if (collapsed.has(group.key)) collapsed.delete(group.key);
    else collapsed.add(group.key);
    saveCollapsed();
    renderDrafts(lastDrafts);
  });
  row.append(toggle);

  const actions = document.createElement("span");
  actions.className = "group__actions";

  if (group.kind === "project") {
    // The pencil joins `edit` and `+` in the row's actions, revealed
    // by the same hover the other two are, and absent while a recording is
    // live. A "missing" group has no project to rename — its name is a string
    // some draft remembers — and neither has the no-project group.
    if (renameAllowed()) {
      actions.append(
        pencilButton(`Rename "${group.label}"`, () =>
          beginRename("project", "row", group.key, group.label),
        ),
      );
    }

    // Hover/focus affordance: this project's folder and templates, in the main
    // area, through the same `data-current` switch the gear uses.
    const edit = linkButton(
      "edit",
      `Notes folder and templates for "${group.label}"`,
      () => openProjectPane(group.key),
    );
    edit.classList.add("group__edit");
    actions.append(edit);

    // A note inside this project, the way a file is added inside a folder in an
    // editor. It makes that project active first, so the note is born where the
    // row says it is — both are refused while a recording is live, and the
    // refusal comes back as a notice rather than as a half-done chain.
    const add = document.createElement("button");
    add.type = "button";
    add.className = "group__new";
    add.textContent = "+";
    add.title = `New note in "${group.label}"`;
    add.setAttribute("aria-label", `New note in ${group.label}`);
    add.disabled = recordingLive;
    add.addEventListener("click", () => newNoteIn(group.key));
    actions.append(add);
  } else if (group.kind === "missing") {
    const gone = document.createElement("span");
    gone.className = "group__note";
    gone.textContent = "not in your projects";
    gone.title =
      "These notes name a project your config no longer has. They are still here; saving one adopts the active project.";
    actions.append(gone);
  }

  // The project's summed match count while a term is active, its note count
  // otherwise.
  const count = document.createElement("span");
  count.className = "group__count";
  const total = found
    ? group.notes.reduce((sum, draft) => sum + matchCount(draft.id), 0)
    : group.notes.length;
  count.textContent = total > 0 ? String(total) : "";
  count.title = found
    ? `${matchLabel(total)} in this project`
    : `${group.notes.length} in this project`;
  actions.append(count);
  row.append(actions);

  // The wrapper is what animates: a grid whose single row goes from 0fr to 1fr,
  // which is the only way to reach a content height without measuring it here.
  const body = document.createElement("div");
  body.className = "group__body";
  const notes = document.createElement("ul");
  notes.className = "group__notes";
  let activeRow = null;
  if (group.notes.length === 0) {
    const empty = document.createElement("li");
    empty.className = "muted";
    empty.textContent = "no notes yet";
    notes.append(empty);
  }
  for (const draft of group.notes) {
    const node = noteRow(draft, activeId);
    if (draft.id === activeId) activeRow = node;
    notes.append(node);
  }
  body.append(notes);

  // The gestures on a group row. **Only a project group has a menu**: a
  // "missing" group's name is a string some draft remembers and the "no
  // project" group is not a thing at all, so there is nothing behind either
  // header for a menu to act on. Their note rows keep the note menu.
  if (group.kind === "project") {
    row.addEventListener("contextmenu", (event) => {
      openRowMenu("project", group.key, event, toggle);
    });
  }
  // Drop targets are the project groups and the "no project" group — never a
  // "missing" one, because there is no project there to assign to (§8).
  makeGroupDropTarget(item, row, group);

  item.append(row, body);
  return { item, activeRow };
}

// The project row's `+`: make that project active, then create the note in it.
// Two existing commands in order, and the second only runs if the first was
// accepted — a note created against a project the backend refused to switch to
// would land somewhere the row did not promise.
async function newNoteIn(name) {
  // `project_set_active` is refused while a recording is live and *still*
  // resolves — the refusal comes back as a notice — so awaiting it is not
  // enough to know it happened. The row's `+` is disabled for the same reason;
  // this is the guard that makes the disable structural rather than cosmetic.
  if (recordingLive) return;
  clearPending();
  // The new note is about to become the active one, so show the transcript it
  // will land in rather than leaving Settings or a project's fields on screen.
  showPane("pane-session");
  try {
    if (lastProjects.active !== name) {
      await invoke("project_set_active", { name });
    }
    await invoke("draft_new");
  } catch (err) {
    reportFailure(err);
  }
}

// ---------------------------------------------------------------------------
// The context menu — one builder, two item sets
//
// The design's one menu grammar (the tray flyout) finally
// built as DOM: the tray could not take it, because a notification-area menu is
// drawn by the OS, and an in-window menu can. It is glass, 260px, `role="menu"`.
//
// Three rules hold it together:
//
// * **It adds no command and no second grammar.** Every item invokes exactly
//   what the surface beside it invokes — `draft_open`, `beginRename`,
//   `project_reveal`, `newNoteIn`, `project_set_active`, `openProjectPane` —
//   and `Discard…` arms the row's own two-step question rather than sending
//   anything. A menu never destroys anything on its own click.
// * **It lies about nothing.** Every item a live recording would refuse is
//   `disabled` with the reason in `title`, rather than being offered and then
//   answered with a notice. The one item with no control
//   beside it — `Hide deleted lines`, which is the *only* way to
//   reach that flag — is not an exception to the rule above but to its
//   premise: it invokes nothing at all, so there is no second way to do a
//   thing, and it stays live while a recording does, exactly as the search
//   field does.
// * **Opening it clears nothing.** A right-click is a question, not an action;
//   an *item* that enters one of the exclusive states runs that state's own
//   clearing, because it is the same function the other surface calls.
//
// Nothing here shows, raises or activates a window: it is a `position: fixed`
// element in the page, and the only focus calls move the caret between DOM
// nodes inside a window that already has it (invariant 2).
// ---------------------------------------------------------------------------

// The design's 8px inside margin, which is also what the menu is clamped to.
const MENU_EDGE = 8;

// What the open menu is about — `{ kind: "note"|"project", key, opener }` — or
// null. One at a time: opening another moves this one.
let menuOpen = null;

// The one sentence a refusal-while-recording gets in this menu. The backend
// refuses these too (`refuse_while_recording`); this is the near half of the
// same rule, said before the click rather than after it.
const MENU_LIVE = "Not while a recording is running — stop it first";

// Put the menu away. `giveFocusBack` returns the caret to the row that opened
// it (the keyboard path); a teardown that happens *during* a render must not,
// because the row it would focus is about to be thrown away.
function closeMenu(giveFocusBack = false) {
  if (menuOpen === null) return;
  const { opener } = menuOpen;
  menuOpen = null;
  const host = el("row-menu");
  host.dataset.open = "no";
  host.replaceChildren();
  if (giveFocusBack && opener && opener.isConnected) opener.focus();
}

function menuSeparator() {
  const line = document.createElement("div");
  line.className = "menu__sep";
  line.setAttribute("role", "separator");
  return line;
}

// One item. `disabled` + `title` is this app's existing grammar for "offered,
// but not now" (the selection toolbar's Delete N), and it is what makes the
// refusal readable before the click instead of after it.
function menuItemNode(item) {
  const node = document.createElement("button");
  node.type = "button";
  node.className = "menu__item";
  node.setAttribute("role", "menuitem");
  node.tabIndex = -1;
  node.textContent = item.label;
  node.title = item.title;
  node.disabled = Boolean(item.disabled);
  node.addEventListener("click", () => {
    if (node.disabled) return;
    // Closed *before* the action runs: every one of them re-renders something,
    // and a menu still on screen over a rebuilt tree is a menu about the row
    // that was there a moment ago.
    closeMenu();
    item.run();
  });
  return node;
}

// A note row's items. `Show in folder` follows the `note__tools` rule —
// only a note that is actually on disk can be shown — and `Rename` follows
// the rename rule: a note's name is its file's name, so an unbound one has none.
function noteMenuItems(draft) {
  const label = noteLabel(draft);
  const active = draft.id === lastDrafts.active_id;
  const items = [
    {
      label: "Open",
      title: active ? `${label} is already open` : `Open ${label}`,
      run: () => openNote(draft, active),
    },
    {
      label: "Rename",
      disabled: recordingLive || !draft.saved_path,
      title: recordingLive
        ? MENU_LIVE
        : draft.saved_path
          ? `Rename ${label}`
          : "This note has never been saved, so there is no file to rename",
      run: () => beginRename("note", "row", draft.id, label),
    },
  ];
  if (draft.saved_path) {
    items.push({
      label: "Show in folder",
      title: `Show ${draft.saved_path} in your file manager`,
      run: () =>
        invoke("project_reveal", { target: "draft", name: draft.id }).catch(
          reportFailure,
        ),
    });
  }
  // The view toggle sits above the separator the destructive tail already had
  // — the same item, the same global flag, in both menus.
  items.push(hideDeletedMenuItem());
  items.push({ separator: true });
  items.push({
    label: "Discard…",
    disabled: recordingLive,
    title: recordingLive
      ? MENU_LIVE
      : "Ask, in the row, before moving this note's draft to .trash",
    run: () => armDiscard(draft.id),
  });
  return items;
}

// A project group row's items (§3). Both ellipses are honest: `Discard…` arms
// the row's question and `Delete project…` opens the pane where the danger
// row's own two-step lives. Neither destroys anything from here.
function projectMenuItems(project) {
  const name = project.name;
  const active = name === lastProjects.active;
  return [
    {
      label: "New note",
      disabled: recordingLive,
      title: recordingLive ? MENU_LIVE : `New note in "${name}"`,
      run: () => newNoteIn(name),
    },
    {
      label: "Rename",
      disabled: recordingLive,
      title: recordingLive ? MENU_LIVE : `Rename "${name}"`,
      run: () => beginRename("project", "row", name, name),
    },
    {
      label: "Make active",
      disabled: recordingLive || active,
      // The recording reason wins while one is live, because that is what
      // would stop it *now*; otherwise an already-active project says so.
      title: recordingLive
        ? MENU_LIVE
        : active
          ? "This is the active project"
          : "New notes go into this project",
      run: () =>
        invoke("project_set_active", { name }).catch(reportFailure),
    },
    {
      label: "Edit project…",
      title: `Notes folder and templates for "${name}"`,
      run: () => openProjectPane(name),
    },
    {
      label: "Open folder",
      title: "Show this project's notes folder",
      run: () =>
        invoke("project_reveal", { target: "notes_dir", name }).catch(
          reportFailure,
        ),
    },
    // The same view toggle the note menu carries, flipping the same flag.
    // Above the separator, because it destroys nothing.
    hideDeletedMenuItem(),
    { separator: true },
    {
      label: "Delete project…",
      disabled: recordingLive,
      title: recordingLive
        ? MENU_LIVE
        : "Open this project's pane, where removing it asks first. Nothing on disk is deleted.",
      run: () => openProjectPane(name),
    },
  ];
}

// Inside the window, with the design's 8px margin, whatever the cursor was
// pointing at. A menu taller than the window pins to the top margin rather than
// hanging off the bottom.
function clampMenu(at, size, extent) {
  const most = Math.max(MENU_EDGE, extent - size - MENU_EDGE);
  return Math.min(Math.max(at, MENU_EDGE), most);
}

// The one entry point, from a note row's or a project row's `contextmenu`.
//
// `preventDefault` happens **here and nowhere else**: fields, the transcript and
// empty space keep the WebView's own menu, which is the right menu for text.
function openRowMenu(kind, key, event, opener) {
  // Built from the payload the window last heard, not from the render that
  // drew the row: what a menu offers is decided the moment it opens.
  let items = null;
  if (kind === "note") {
    const draft = lastDrafts.drafts.find((d) => d.id === key);
    if (draft) items = noteMenuItems(draft);
  } else {
    const project = lastProjects.projects.find((p) => p.name === key);
    if (project) items = projectMenuItems(project);
  }
  // A row whose subject has left the payload gets no menu at all — and no
  // `preventDefault`, so the native menu answers instead of nothing.
  if (!items || items.length === 0) return;

  event.preventDefault();
  // Whatever was open is replaced rather than stacked: one menu at a time.
  closeMenu();

  // A rename field open somewhere in the tree is settled **before** the menu
  // takes the focus, by its own standing contract — which is that a blur
  // **reverts** rather than commits. Not the menu
  // clearing a state — opening one clears nothing — but the same thing that
  // happens when the user clicks anywhere else: doing it here, in one
  // deliberate step, is what stops the field's blur from re-rendering the tree
  // underneath a menu that has just opened over it. A right-click is a
  // question, and a question must not rename a file.
  if (renaming !== null) cancelRename();

  const host = el("row-menu");
  host.replaceChildren();
  for (const item of items) {
    host.append(item.separator ? menuSeparator() : menuItemNode(item));
  }
  menuOpen = { kind, key, opener: opener || null };

  // Shown first, then measured, then corrected: the height depends on how many
  // items there are, and there is no way to know it without laying it out.
  host.style.left = `${event.clientX}px`;
  host.style.top = `${event.clientY}px`;
  host.dataset.open = "yes";
  const box = host.getBoundingClientRect();
  host.style.left = `${clampMenu(event.clientX, box.width, window.innerWidth)}px`;
  host.style.top = `${clampMenu(event.clientY, box.height, window.innerHeight)}px`;
  host.focus();
}

// Keyboard (§7). Attached once, to the static host.
function wireMenuKeys() {
  const host = el("row-menu");
  host.addEventListener("keydown", (event) => {
    if (menuOpen === null) return;
    const items = [...host.querySelectorAll(".menu__item:not(:disabled)")];
    const at = items.indexOf(document.activeElement);
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      if (items.length === 0) return;
      const step = event.key === "ArrowDown" ? 1 : -1;
      const next =
        at < 0
          ? step === 1
            ? 0
            : items.length - 1
          : (at + step + items.length) % items.length;
      items[next].focus();
    } else if (event.key === "Enter") {
      const focused = items[at] || items[0];
      if (!focused) return;
      // `preventDefault` so a real Enter on a focused button does not also
      // fire the browser's own activation: one press, one click.
      event.preventDefault();
      focused.click();
    } else if (event.key === "Escape") {
      // The menu is the topmost transient, so it answers Escape first — and
      // says so, which is what leaves the Escape ladder untouched (it checks
      // `defaultPrevented` before its first rung).
      event.preventDefault();
      closeMenu(true);
    } else if (event.key === "Tab") {
      // Focus is leaving; a menu the keyboard has walked out of is closed.
      closeMenu();
    }
  });
}

// ---------------------------------------------------------------------------
// Styled dropdowns
//
// The complaint: the open dropdowns "do not fit the design decisions of
// the app". They could not. A native <select>'s popup is drawn by the OS,
// outside this page's cascade, which is why three comments in style.css all
// said the same defeated thing. So the list moves in here, into the glass the
// context menu above already draws, and the <select>
// stops being something the pointer can reach — which is the whole of "the
// native popup never appears again": it only ever opens when the <select>
// itself is pressed.
//
// **Enhancement, not replacement.** The <select> stays in the DOM and stays
// the truth: the existing fill functions keep building its options, everything
// that reads `select.value` still reads it, and choosing a row sets that value
// and dispatches a DOM `change` — so every listener that was on it fires
// unchanged and not one of them moved for it.
//
// A `change` dispatched on our own element inside our own page is **not**
// synthetic input in the invariant-1 sense: that invariant forbids code that
// makes the *operating system* believe in a keystroke or a click (`SendInput`,
// `rdev::simulate`, `enigo`), and nothing here can produce one. This is the
// same DOM event the browser would have raised a microsecond later.
//
// Invariant 2 comes out of this stronger, by deletion: the
// popup this replaces was its own OS window and could take activation. A div
// cannot. And DOM focus never moves *into* the list at all — it stays on the
// button while it is open, so the one `focus()` below only ever restores what
// was already there, in answer to the user's own key or click.
// ---------------------------------------------------------------------------

// The nine. Written out rather than queried so that a tenth <select> is a
// deliberate act with a line in this list, not something that quietly inherits
// a component.
const DROP_IDS = [
  "wiz-mic",
  "draft-sort",
  "theme-select",
  "mic-select",
  "language-select",
  "overlay-corner",
  "reveal-seconds",
  "no-project-select",
  "move-note-select",
];

// Between the control and its list.
const DROP_GAP = 4;
// The tallest a list ever gets, room permitting. ~100 languages scroll.
const DROP_MAX = 320;
// And the shortest it is squeezed to before the clamp takes over instead.
const DROP_MIN = 64;
// How long a type-ahead prefix lives. Long enough to spell "ger" in the
// language list, short enough that a key pressed a minute later starts again.
const DROP_TYPE_MS = 800;

// Which control's list is open — `{ select, button }` — or null. One at a
// time, like the menu.
let dropOpen = null;
// Where the keyboard is in that list, as an index, or -1.
let dropActive = -1;
// The type-ahead prefix and when it was last added to.
let dropTyped = "";
let dropTypedAt = 0;

const dropHost = () => el("drop-menu");
const dropRows = () => [...dropHost().children];
const dropButton = (select) => el(`${select.id}-button`);

// What names this control on screen: its <label for> when it has one, and
// otherwise the settings row's own label — the design's rows carry their name
// in a `.set-row__label` span rather than in a <label> (the wizard's mic row).
function labelForSelect(select) {
  const tied = document.querySelector(`label[for="${select.id}"]`);
  if (tied) return tied;
  const row = select.closest(".set-row");
  return (row && row.querySelector(".set-row__label")) || null;
}

// One control. Idempotent, so a second call is free.
function enhanceSelect(select) {
  if (!select || select.dataset.enhanced === "yes") return;
  select.dataset.enhanced = "yes";
  // Focusable from script, never by Tab: the button below is the tab stop now,
  // and leaving the <select> in the order would be one invisible stop the user
  // cannot see or explain.
  select.tabIndex = -1;
  // And not announced twice. It is still the value and still what `change`
  // fires on; it is simply not the control any more.
  select.setAttribute("aria-hidden", "true");

  const button = document.createElement("button");
  button.type = "button";
  button.id = `${select.id}-button`;
  // The <select>'s own class comes with it, so the closed control keeps
  // exactly the box it always had — `.pick__select` inside `.pick`,
  // `.sort__select` lying invisible over the sort glyph, `.field__input` in a
  // popup's row. Only what *opening* looks like changes.
  button.className = `drop__button ${select.className}`.trim();
  button.dataset.for = select.id;
  button.dataset.open = "no";
  // The select-only combobox pattern: the button is the control, `#drop-menu`
  // is the listbox it owns, and the row the keyboard is on is named rather
  // than focused (`aria-activedescendant`, set on open).
  button.setAttribute("role", "combobox");
  button.setAttribute("aria-haspopup", "listbox");
  button.setAttribute("aria-controls", "drop-menu");
  button.setAttribute("aria-expanded", "false");

  const label = labelForSelect(select);
  if (label) {
    if (!label.id) label.id = `${select.id}-label`;
    // Both, in this order — "Theme", then "Dark": the row's name and the value
    // in force, which together are what the <select> used to announce.
    button.setAttribute("aria-labelledby", `${label.id} ${button.id}`);
    // A <label for> pointing at a control the pointer can no longer reach is a
    // dead label, so it points at the one that took its place.
    if (label.htmlFor === select.id) label.htmlFor = button.id;
  } else if (select.getAttribute("aria-label")) {
    button.setAttribute("aria-label", select.getAttribute("aria-label"));
  }

  // The value goes in its own span — see `.drop__label`: a button's text is
  // laid out in an anonymous box that `text-overflow` does not reliably reach,
  // and "Microphone (Realtek(R) Audio) — system default" has to end in an
  // ellipsis rather than a cut-off glyph.
  const value = document.createElement("span");
  value.className = "drop__label";
  button.append(value);

  select.insertAdjacentElement("afterend", button);

  button.addEventListener("click", () => toggleDrop(select, button));
  button.addEventListener("keydown", (event) =>
    dropKeydown(event, select, button),
  );

  // The caret and the box's own padding are part of the one control the design
  // draws, so a press on either opens what the button opens — aiming at the
  // caret and getting nothing is the defect this prevents, in miniature.
  // The button's own click bubbles through here and is ignored.
  const box = button.closest(".pick, .sort");
  if (box) {
    box.addEventListener("click", (event) => {
      if (event.target instanceof Node && button.contains(event.target)) return;
      toggleDrop(select, button);
    });
  }

  // The label follows the value wherever the value comes from — including a
  // driver, or any future caller, that sets it and dispatches the event by
  // hand. The <select> is the truth; this only reads it.
  select.addEventListener("change", () => refreshPick(select));

  // Options rebuilt by a fill function, `disabled` flipped by
  // `setSettingsInert` or by the overlay's master switch, a `title` rewritten
  // by the sort control: every one of those is a mutation *on the <select>*,
  // and watching for them is what keeps the button honest without a single
  // existing render function being rewired. An observer fires on the mutation
  // itself — it is not a poll.
  new MutationObserver(() => {
    refreshPick(select);
    // A list on screen built from options that have just been thrown away is a
    // list about the control that was there a moment ago (§7). It closes, and
    // commits nothing.
    if (dropOpen !== null && dropOpen.select === select) closeDrop();
  }).observe(select, {
    childList: true,
    subtree: true,
    characterData: true,
    attributes: true,
    attributeFilter: ["disabled", "title"],
  });

  refreshPick(select);
}

// The closed control, re-read from the <select>: its label, its inert state,
// and its reason for being inert.
function refreshPick(select) {
  const button = dropButton(select);
  if (!button) return;
  const label = button.querySelector(".drop__label");
  const chosen = select.selectedOptions[0] || null;
  if (label) label.textContent = chosen ? chosen.textContent : "";
  button.disabled = select.disabled;
  button.dataset.value = select.value;
  // The refusal a live recording puts on a control lives in its `title`; a
  // button that dropped it would refuse without saying why.
  if (select.title) button.title = select.title;
  else button.removeAttribute("title");
}

// The paths that set a value without touching the DOM — `applyTheme`,
// `renderOverlayRows`, `renderRevealOptions` — and `setSettingsInert`, which
// disables the <select> and the button in the same sweep but can leave them
// disagreeing when the overlay's own switch re-disables one of them
// afterwards. All four are inside one render, so that render says when it is
// finished rather than anything here watching for it.
function refreshPicks() {
  for (const id of DROP_IDS) {
    const select = el(id);
    if (select && select.dataset.enhanced === "yes") refreshPick(select);
  }
}

function toggleDrop(select, button) {
  if (dropOpen !== null && dropOpen.button === button) closeDrop(true);
  else openDrop(select);
}

function openDrop(select) {
  const button = dropButton(select);
  if (!button || button.disabled || select.disabled || !select.isConnected) {
    return;
  }
  // Whatever was open is replaced rather than stacked.
  closeDrop();

  const host = dropHost();
  host.replaceChildren();
  const label = labelForSelect(select);
  host.setAttribute(
    "aria-label",
    (label && label.textContent.trim()) ||
      select.getAttribute("aria-label") ||
      "Options",
  );
  // Built from the <select>'s options at open time, not cached: what a list
  // offers is decided the moment it opens, the same rule the row menu follows.
  [...select.options].forEach((option, index) => {
    const row = document.createElement("div");
    row.className = "drop__option";
    row.id = `drop-option-${index}`;
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", option.selected ? "true" : "false");
    // Offered, dim and inert — the <select>'s own word for it, kept.
    if (option.disabled) row.setAttribute("aria-disabled", "true");
    row.dataset.value = option.value;
    row.textContent = option.textContent;
    // A device name can be longer than the list is wide; the row shortens and
    // says the whole of it on hover rather than spilling past the glass.
    row.title = option.title || option.textContent;
    host.append(row);
  });
  host.dataset.for = select.id;

  // `box` is the styled control as the user sees it — the `.pick` or `.sort`
  // the button sits in, or the button itself. Kept because a press anywhere on
  // it is the toggle, and the click-away below has to know that.
  dropOpen = { select, button, box: button.closest(".pick, .sort") || button };
  button.setAttribute("aria-expanded", "true");
  button.dataset.open = "yes";
  placeDrop(button);
  // The value in force is where the list opens: a hundred languages with the
  // chosen one off screen is a list that has not answered the question.
  setDropActive(select.selectedIndex, true);
}

// Under the control, inside the window, above it when there is no room below —
// the context menu's own 8px clamp doing the same job for a box that has an
// anchor rather than a cursor.
function placeDrop(button) {
  const host = dropHost();
  // The styled box, not the button inside it: `.pick` and `.sort` carry the
  // border the list should line up with.
  const box = button.closest(".pick, .sort") || button;
  const anchor = box.getBoundingClientRect();
  // At least as wide as the control it belongs to — but never wider than the
  // window, because `min-width` beats the sheet's `max-width` and a narrow
  // window would otherwise get a list it cannot show.
  host.style.minWidth = `${Math.round(
    Math.min(anchor.width, window.innerWidth - 2 * MENU_EDGE),
  )}px`;
  host.style.maxHeight = `${DROP_MAX}px`;
  host.style.left = `${anchor.left}px`;
  host.style.top = `${anchor.bottom + DROP_GAP}px`;
  // Shown first, then measured, then corrected, for the reason the menu is:
  // how tall a list is depends on what is in it, and nothing can know that
  // without laying it out.
  host.dataset.open = "yes";

  const below = window.innerHeight - MENU_EDGE - (anchor.bottom + DROP_GAP);
  const above = anchor.top - DROP_GAP - MENU_EDGE;
  // Downwards unless that would cut the list off and upwards would not: a
  // control near the bottom edge opens over itself rather than off the screen.
  const flip = host.getBoundingClientRect().height > below && above > below;
  host.dataset.flip = flip ? "up" : "down";
  const room = Math.min(DROP_MAX, Math.max(DROP_MIN, flip ? above : below));
  host.style.maxHeight = `${room}px`;

  const size = host.getBoundingClientRect();
  const top = flip
    ? anchor.top - DROP_GAP - size.height
    : anchor.bottom + DROP_GAP;
  host.style.top = `${clampMenu(top, size.height, window.innerHeight)}px`;
  host.style.left = `${clampMenu(anchor.left, size.width, window.innerWidth)}px`;
}

// Scrolled by hand rather than with `scrollIntoView`, which walks up the
// ancestor chain and can end up scrolling the pane *behind* the list — which
// is one of the events that tears it down.
function scrollRowIntoView(row, centre) {
  const host = dropHost();
  const top = row.offsetTop - host.clientTop;
  const view = host.clientHeight;
  if (centre) host.scrollTop = Math.max(0, top - (view - row.offsetHeight) / 2);
  else if (top < host.scrollTop) host.scrollTop = top;
  else if (top + row.offsetHeight > host.scrollTop + view) {
    host.scrollTop = top + row.offsetHeight - view;
  }
}

function setDropActive(index, centre = false) {
  if (dropOpen === null) return;
  const rows = dropRows();
  for (const row of rows) delete row.dataset.active;
  const row = index >= 0 ? rows[index] : null;
  dropActive = row ? index : -1;
  if (!row) {
    dropOpen.button.removeAttribute("aria-activedescendant");
    return;
  }
  row.dataset.active = "yes";
  // Said to a screen reader without moving DOM focus off the button, which is
  // ARIA's own answer for a listbox popup — and this window's answer to "who
  // has the focus when a list is open": the control does, throughout.
  dropOpen.button.setAttribute("aria-activedescendant", row.id);
  scrollRowIntoView(row, centre);
}

function dropEnabled(row) {
  return row.getAttribute("aria-disabled") !== "true";
}

// One step, skipping what the <select> disabled. **No wrap**, deliberately: a
// native dropdown stops at its ends, and a language list that silently jumps
// from Zulu back to Auto is a list nobody can count their way through.
function stepDrop(delta) {
  const rows = dropRows();
  let at = dropActive < 0 ? (delta > 0 ? -1 : rows.length) : dropActive;
  for (;;) {
    at += delta;
    if (at < 0 || at >= rows.length) return;
    if (dropEnabled(rows[at])) {
      setDropActive(at);
      return;
    }
  }
}

// Home and End: the first and last row the <select> will actually accept.
function endDrop(last) {
  const rows = dropRows();
  const order = last ? [...rows.keys()].reverse() : [...rows.keys()];
  for (const at of order) {
    if (dropEnabled(rows[at])) {
      setDropActive(at);
      return;
    }
  }
}

// First-letter type-ahead, without which ~100 languages are unusable. It moves
// the keyboard's place and commits nothing — Enter is still the answer.
function typeAheadDrop(char) {
  const now = Date.now();
  if (now - dropTypedAt > DROP_TYPE_MS) dropTyped = "";
  dropTypedAt = now;
  // The same letter again is not a longer word, it is "the next one like this".
  const repeat =
    dropTyped.length === 1 && dropTyped.toLowerCase() === char.toLowerCase();
  if (!repeat) dropTyped += char;
  const term = dropTyped.toLowerCase();
  const rows = dropRows();
  if (rows.length === 0) return;
  // One letter looks for the *next* match and wraps, so pressing it again walks
  // the Es; a longer prefix refines what is already under the cursor rather
  // than jumping away from it.
  const step = term.length === 1 ? 1 : 0;
  const from = dropActive < 0 ? 0 : dropActive + step;
  for (let i = 0; i < rows.length; i += 1) {
    const at = (from + i) % rows.length;
    const row = rows[at];
    if (!dropEnabled(row)) continue;
    if (row.textContent.trim().toLowerCase().startsWith(term)) {
      setDropActive(at);
      return;
    }
  }
}

// The whole keyboard, on the button — which is where DOM focus is whether the
// list is open or not.
function dropKeydown(event, select, button) {
  const open = dropOpen !== null && dropOpen.button === button;
  const key = event.key;
  const opens =
    key === "Enter" || key === " " || key === "ArrowDown" || key === "ArrowUp";
  if (!open) {
    if (opens) {
      // `preventDefault` so the button's own activation does not also fire:
      // one press, one open (the menu's Enter rule).
      event.preventDefault();
      openDrop(select);
    }
    return;
  }
  if (key === "Escape") {
    // An open list is the topmost transient, and says so: the document's
    // Escape ladder checks
    // `defaultPrevented` before its first rung, so this press closes the list
    // and **only** the list. A popup hosting a dropdown takes two presses,
    // which is one question answered per press.
    event.preventDefault();
    closeDrop(true);
  } else if (key === "Tab") {
    // Focus is leaving. No `preventDefault`: the Tab still moves on, and a
    // list the keyboard has walked out of is closed.
    closeDrop();
  } else if (key === "Enter" || key === " ") {
    event.preventDefault();
    commitDrop(dropActive);
  } else if (key === "ArrowDown" || key === "ArrowUp") {
    event.preventDefault();
    stepDrop(key === "ArrowDown" ? 1 : -1);
  } else if (key === "Home" || key === "End") {
    event.preventDefault();
    endDrop(key === "End");
  } else if (
    key.length === 1 &&
    !event.ctrlKey &&
    !event.altKey &&
    !event.metaKey
  ) {
    event.preventDefault();
    typeAheadDrop(key);
  }
}

// The answer. The <select> is set and told, and everything that was listening
// to it hears exactly what it heard before the list was styled.
function commitDrop(index) {
  if (dropOpen === null) return;
  const { select } = dropOpen;
  const row = dropRows()[index];
  const option = select.options[index];
  // A row that is no longer backed by the option it was built from, or one the
  // <select> refuses, answers nothing and leaves the list open.
  if (!row || !option || option.disabled || row.dataset.value !== option.value) {
    return;
  }
  // Put away *before* the change runs: a listener re-renders something, and a
  // list still on screen over a rebuilt control is the menu's §7 problem in a
  // different coat.
  closeDrop(true);
  // Same as the native control: re-picking what was already picked is not a
  // change and raises no event, so no command is sent for it either.
  if (select.value !== option.value) {
    select.value = option.value;
    // The bridge, and the reason no listener in this file had to move. A DOM
    // event on our own element — not OS input synthesis, which this codebase
    // has no path to at all (invariant 1).
    select.dispatchEvent(new Event("change", { bubbles: true }));
  }
  refreshPick(select);
}

// `giveFocusBack` is the same contract the menu's `closeMenu` has: the user's
// own key or click puts the caret back on the control, and a teardown — a
// click somewhere else, a re-render, a recording starting — leaves it wherever
// the user just put it. Nothing here activates or raises a window
// (invariant 2); this is DOM focus inside a window that already has it.
function closeDrop(giveFocusBack = false) {
  if (dropOpen === null) return;
  const { button } = dropOpen;
  dropOpen = null;
  dropActive = -1;
  dropTyped = "";
  const host = dropHost();
  host.dataset.open = "no";
  host.replaceChildren();
  host.scrollTop = 0;
  delete host.dataset.for;
  delete host.dataset.flip;
  button.dataset.open = "no";
  button.setAttribute("aria-expanded", "false");
  button.removeAttribute("aria-activedescendant");
  if (giveFocusBack && button.isConnected && !button.disabled) button.focus();
}

// The host's own handlers and the teardowns that are events on something other
// than the list (§7). Attached once, like the menu's.
function wireDrops() {
  const host = dropHost();
  // Focus stays on the button, which is what makes "the caret is back on the
  // control afterwards" true without anything having to put it there — and a
  // press inside the list is the one thing that would take it away.
  host.addEventListener("mousedown", (event) => event.preventDefault());
  host.addEventListener("click", (event) => {
    const row =
      event.target instanceof Element
        ? event.target.closest(".drop__option")
        : null;
    if (!row) return;
    commitDrop(dropRows().indexOf(row));
  });

  // Click-away, in capture and on both events, for the menu's reason: a real
  // pointer dismisses a popup on the press, and a page driven programmatically
  // only ever sees the `click`.
  for (const kind of ["pointerdown", "click"]) {
    document.addEventListener(
      kind,
      (event) => {
        if (dropOpen === null) return;
        const target = event.target instanceof Element ? event.target : null;
        // The trigger's own press is the toggle: closing it here would leave
        // the control's handler re-opening what this just closed.
        if (
          target &&
          (target.closest("#drop-menu") || dropOpen.box.contains(target))
        ) {
          return;
        }
        closeDrop();
      },
      true,
    );
  }

  // The anchor moving out from under an open list: any scroll in the window
  // that is not the list's own, and any resize of it.
  document.addEventListener(
    "scroll",
    (event) => {
      if (dropOpen === null) return;
      if (
        event.target instanceof Element &&
        event.target.closest("#drop-menu")
      ) {
        return;
      }
      closeDrop();
    },
    true,
  );
  window.addEventListener("resize", () => closeDrop());
}

// ---------------------------------------------------------------------------
// Dragging a note into another project
//
// The tree's own small mirror of the transcript drag, deliberately not a
// generalization of it: the transcript reorders lines within one note and this
// re-files one note between projects, and the only thing the two share is the
// HTML drag API and the `data-drop` mark.
//
// **No optimistic move.** The drop invokes `draft_set_project` and stops; the
// row re-renders into its new group when `sotone://drafts` says so, which is the
// same rule every other mutation in this file follows.
// ---------------------------------------------------------------------------

// The note being dragged, or null. Only ever used to work out what to send.
let draggingNote = null;

// The transcript's gate plus two: a rename field and an open menu are both
// questions about a row, and dragging that row out from under one is not an
// answer to it. Under a search the tree is a filtered answer, so
// nothing in it drags at all.
function noteDragAllowed() {
  return (
    editingAllowed() && !editing && !searching() && renaming === null && menuOpen === null
  );
}

// Which groups take a drop: a real project, or "no project" — never a
// "missing" group, because there is no project behind it to assign to.
function droppableGroup(group) {
  return group.kind === "project" || group.kind === "none";
}

function clearNoteDropMarks() {
  for (const marked of document.querySelectorAll(".group__row[data-drop]")) {
    delete marked.dataset.drop;
  }
}

function endNoteDrag() {
  draggingNote = null;
  clearNoteDropMarks();
  for (const held of document.querySelectorAll('.note[data-dragging="yes"]')) {
    delete held.dataset.dragging;
  }
}

// One `draft_set_project` per drop, and then nothing: the panel waits for the
// drafts event. Dropping a note on the group it is already in is a quiet no-op
// — there is no order in a group to change, so there is nothing to send.
//
// `keepBoth` is `false` here and `true` in exactly one other place — the clash
// dialog's own button — which is the same shape `draft_save`'s `overwrite` has,
// and for the same reason: the one argument that can invent a second file is
// only ever set by the user answering a question about it.
function moveNoteToProject(id, project) {
  const draft = lastDrafts.drafts.find((d) => d.id === id);
  if (!draft) return;
  if ((draft.project || null) === project) return;
  invoke("draft_set_project", { id, project, keepBoth: false }).catch(
    reportFailure,
  );
}

function makeNoteDraggable(item, draft) {
  // Not draggable at all under the gates, rather than draggable-and-refused:
  // the grip goes with it in CSS, exactly as the transcript's does.
  if (!noteDragAllowed()) return;
  item.draggable = true;

  item.addEventListener("dragstart", (event) => {
    // The menu goes first, and before the gate rather than after it: a press
    // on the row is a click-away that has already closed it with a real
    // pointer, and this is the same answer for a drag that begins without one
    // (§6 — a drag start is a teardown, not a thing the menu refuses).
    closeMenu();
    // The state can have moved since this row was rendered, so the gate is
    // checked here too.
    if (!noteDragAllowed()) {
      event.preventDefault();
      return;
    }
    // A drag enters one of the mutually exclusive states, so the
    // selection ends here. The rows are un-marked **in place**: rebuilding the
    // list mid-dragstart would throw away the element being lifted.
    if (selection.size > 0 || selectionAnchor !== null) {
      forgetSelection();
      unmarkSelectedRows();
      renderSelection();
    }
    draggingNote = draft.id;
    item.dataset.dragging = "yes";
    if (event.dataTransfer) {
      event.dataTransfer.effectAllowed = "move";
      // Some engines will not start a drag with an empty payload. It never
      // leaves this window (invariant 3).
      event.dataTransfer.setData("text/plain", draft.id);
    }
  });

  item.addEventListener("dragend", endNoteDrag);
}

// The whole group takes the drop — its row and its body, which is what makes a
// note row inside it a way of dropping *into that group* rather than a target
// of its own. There is no reordering here: the tree sorts by `SORTS`.
function makeGroupDropTarget(item, row, group) {
  const accepts = () =>
    draggingNote !== null && noteDragAllowed() && droppableGroup(group);

  item.addEventListener("dragover", (event) => {
    if (!accepts()) return;
    // Only a preventDefault marks a valid drop target; without it the drop
    // never fires.
    event.preventDefault();
    event.stopPropagation();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    if (row.dataset.drop !== "into") {
      clearNoteDropMarks();
      row.dataset.drop = "into";
    }
  });

  item.addEventListener("dragleave", (event) => {
    // Crossing into one of the group's own children is a dragleave on the
    // group; only a leave that really left it drops the mark.
    if (item.contains(event.relatedTarget)) return;
    delete row.dataset.drop;
  });

  item.addEventListener("drop", (event) => {
    if (!accepts()) return;
    event.preventDefault();
    event.stopPropagation();
    const held = draggingNote;
    endNoteDrag();
    moveNoteToProject(held, group.kind === "none" ? null : group.key);
  });
}

// `forceScroll` is for the one case that is not an activation change: the user
// picked a different sort, so the active row moved under them and has to be
// found again.
function renderDrafts(payload, forceScroll = false) {
  // The tree is about to be rebuilt, so the row the menu is about is about to
  // be thrown away with it. No focus return: the row it would hand
  // the caret back to is one of the elements going.
  closeMenu();
  const switched = payload.active_id !== lastDrafts.active_id;
  if (switched) {
    // The transcript belonged to the draft we just left. Empty it now; the
    // worker's own `sotone://lines` for the new draft lands a moment later and
    // fills it in. Showing the old draft's lines under the new draft's name —
    // editable — is the one thing this panel must never do.
    if (payload.active_id !== transcript.draft_id) {
      renderTranscript({ draft_id: payload.active_id, lines: [] });
    }
    // A conflict is a question about one specific note, and the note it was
    // asking about is no longer the one Keep mine would write to. Retract it:
    // an unanswered question left on screen after its subject changed is how a
    // "yes" ends up applied to the wrong file (invariant 4). Resuming the
    // other draft and pressing Save again asks it afresh. The backend clears
    // the condition on the same `DraftChanged`; this drops the texts, and
    // `renderCondition` below settles the screen either way.
    if (conflictFor !== null && conflictFor !== payload.active_id) {
      forgetConflict();
      renderCondition();
    }
    // Switching notes is a fresh start: the last refusal was about the note
    // being left behind.
    clearMessage();
    // A rename field open on the note being left behind goes with it, and it
    // reverts. Nulled rather than `cancelRename()`d because
    // this *is* the render that takes it off the screen — calling back into
    // `renderDrafts` from inside `renderDrafts` would recurse.
    renaming = null;
    // The pick-or-create question is about one note, and it is no longer the
    // note a save would write. Same rule, same reason.
    if (noProjectOpen && noProjectFor !== payload.active_id) {
      closeNoProject();
    }
  }
  // A clash question about a note that is no longer in the tree is a question
  // about nothing — and Keep both would send a drop for a draft the backend
  // does not list. Retracted whatever else changed, because unlike the two
  // above it is not about the *active* note.
  if (noteClash !== null && !payload.drafts.some((d) => d.id === noteClash.id)) {
    closeNoteClash();
  }
  lastDrafts = payload;

  // A rename field whose row has left the tree — or whose name changed under
  // it, which means something else renamed it first — is a question about
  // something that is no longer there. Reverted here, before the tree is
  // rebuilt without it.
  if (renaming !== null && renameSubjectGone()) renaming = null;
  // The tree is about to be rebuilt, so whichever field node is in it is about
  // to be thrown away. Forgetting the node first is what stops a *removed*
  // element's blur — should a browser ever fire one — from reverting the field
  // that replaces it out from under the user mid-type: the handler only acts
  // for the field that is still the open one, and `restoreRename` hands the
  // identity to its replacement at the end of this render.
  if (renaming !== null) renaming.node = null;

  const list = el("draft-list");
  list.replaceChildren();

  // Group and sort a copy: `lastDrafts` keeps the backend's own creation order
  // and its whole list, so a later re-render re-derives from the truth rather
  // than from an already-shuffled one. Every draft in the payload lands in
  // exactly one group — nothing is filtered out of the tree.
  //
  // While a term is active the tree filters to what matched, keeping its
  // hierarchy: notes with no matches are gone and so are the projects left
  // holding none. **The active-note-always-visible rule is
  // suspended** for the duration — a filter that showed a non-matching note
  // would lie about matches — and it comes back with the term, because this
  // filter is the only thing doing it.
  const listed = searching()
    ? payload.drafts.filter((draft) => matchCount(draft.id) > 0)
    : payload.drafts;
  const groups = treeGroups(listed).filter(
    (group) => !searching() || group.notes.length > 0,
  );
  let activeRow = null;
  for (const group of groups) {
    const node = groupNode(group, payload.active_id);
    if (node.activeRow) activeRow = node.activeRow;
    list.append(node.item);
  }

  if (groups.length === 0 && payload.rejected.length === 0) {
    const empty = document.createElement("li");
    empty.className = "muted";
    // No projects and no drafts: a fresh install, one keypress from its first
    // note. Making a project is the `+` above this list. Under a term the same
    // row says why the tree is empty instead — never a blank sidebar.
    empty.textContent = searching()
      ? `No note contains ${quoted(searchTerm())}.`
      : "No notes yet — hold your key and speak.";
    list.append(empty);
  }

  for (const rejected of payload.rejected) {
    list.append(rejectedRow(rejected));
  }

  // A new draft can land below the fold of the scrolled tree, so "New note"
  // looked like it did nothing. Only scroll when the active draft actually
  // changed (or the order just did): the tree re-renders on every appended
  // line, and yanking the scroll position each time would fight the user's own
  // scrolling. scrollIntoView moves the scroll box only — it never activates a
  // window (invariant 2).
  if ((switched || forceScroll) && activeRow) {
    activeRow.scrollIntoView({ block: "nearest" });
  }

  // The line counter belongs to the active draft, not to the launch: resuming
  // a seven-line draft has to read "7 lines", not "0".
  const active = payload.drafts.find((d) => d.id === payload.active_id);
  el("line-count").textContent = countLabel(active ? active.line_count : 0);

  // The chooser's list *is* this payload, so it follows the tree
  // rather than the snapshot it was built from: a note added or renamed while
  // it is open relabels in place, and the choice survives. It no longer gates
  // anything — the list always holds New note — so this is a
  // re-render and never a retraction. Only while there is a set: with none, the
  // toolbar is not on screen at all.
  if (selection.size > 0) renderSelection();
  if (moveChooser !== null) fillMoveChooser();

  // The dirty flag and the bound path both live on this payload, so this is
  // the event that decides whether Save is offered and what it promises.
  renderSaveState();
}

// ---------------------------------------------------------------------------
// Search
//
// One field, two behaviours, and everything about it is read-only. With a note
// open the term filters the tree and the transcript to matches; with none the
// pane lists the notes that contain it. An empty (or all-whitespace) term IS
// search-off — there is no other switch anywhere in this window.
//
// **Which lines matched is the backend's answer, never this window's.** The
// command walks the store and returns per-note results; this file marks the
// term, filters the rows it already has to the ids that came back, and counts.
// A second matcher here would be a second answer to "does this note contain
// the term", and the tree and the pane would eventually disagree.
//
// Nothing here writes anything, asks for anything to be written, or touches a
// window: `search_notes` opens no writable path (invariant 4), and the only
// focus call below moves the caret inside a field the user just clicked
// (invariant 2 is about window activation, which nothing in this file does).
// ---------------------------------------------------------------------------

// The `filename_preview` precedent. Long enough that a typed word is one scan,
// short enough that the list keeps up with the typing.
const SEARCH_DEBOUNCE_MS = 150;

// `term` is the field verbatim; `seq` is the stale-answer token (the last term
// typed wins, whatever order the answers come back in); `result` is the last
// `SearchOutcome` — and `null` is the whole of "search is off", which is why
// every renderer below asks `searching()` rather than looking at the term.
const search = { term: "", seq: 0, result: null };

function searchTerm() {
  return search.term.trim();
}

function searching() {
  return search.result !== null;
}

// Whether there is a note on screen at all. The design's `scope` is implied by
// this and nothing else. A pending row is deliberately not a note: it names
// nothing on disk, so there is nothing to filter.
function hasOpenNote() {
  return Boolean(lastDrafts.active_id || transcript.draft_id);
}

// `no` · `note` · `library` — the value of `#view-ready[data-searching]`.
function searchScope() {
  if (!searching()) return "no";
  return hasOpenNote() ? "note" : "library";
}

function searchNote(draftId) {
  if (!search.result || !draftId) return null;
  return search.result.notes.find((note) => note.draft_id === draftId) || null;
}

// How many lines of one note matched, as the tree's rows and groups report it.
function matchCount(draftId) {
  const note = searchNote(draftId);
  return note ? note.matches : 0;
}

// The ids of the open note's matching lines, or `null` when nothing is being
// filtered. An *empty* set is a real answer — "this note has no matches" — and
// the reason this is not just `Set | undefined`.
function searchIdsFor(draftId) {
  if (searchScope() !== "note" || !draftId) return null;
  const note = searchNote(draftId);
  return new Set(note ? note.lines.map((line) => line.id) : []);
}

// The totals the three counters show. Projects are counted from the *drafts*
// payload the tree is drawn from, not from a second list in the answer, so the
// footer and the tree cannot disagree about which projects are involved.
function searchTotals() {
  if (!search.result) return { matches: 0, notes: 0, projects: 0 };
  const projects = new Set();
  for (const note of search.result.notes) {
    const draft = lastDrafts.drafts.find((d) => d.id === note.draft_id);
    if (draft) projects.add(draft.project || "");
  }
  return {
    matches: search.result.matches,
    notes: search.result.notes.length,
    projects: projects.size,
  };
}

function matchLabel(n) {
  return n === 1 ? "1 match" : `${n} matches`;
}

function noteCountLabel(n) {
  return n === 1 ? "1 note" : `${n} notes`;
}

// The term as the user typed it, in the design's quotes. Never the lowercased
// echo from the backend: what is on screen has to be what was typed.
function quoted(term) {
  return `“${term}”`;
}

// What the sidebar's sort is set to, in words. The header echoes it capitalised
// and the footer in a sentence; both are readouts, and the control stays the
// one <select> in the sidebar.
const SORT_LABEL = {
  newest: "newest first",
  oldest: "oldest first",
  name: "name",
  lines: "line count",
};

function sortLabel() {
  return SORT_LABEL[sortMode] || SORT_LABEL.newest;
}

// -- Marking ----------------------------------------------------------------

// Where the term sits inside a line, as `[start, end)` offsets into the
// original string.
//
// Lowercasing can change a string's *length* — U+0130 is one code point and two
// after `toLowerCase` — so a naive `indexOf` on a folded copy would mark the
// wrong characters in exactly the notes least able to complain about it. The
// table maps every folded offset back to the offset it came from.
function markRanges(text, term) {
  const needle = term.toLowerCase();
  if (needle === "") return [];

  let folded = "";
  const origin = [];
  let offset = 0;
  for (const ch of text) {
    const lower = ch.toLowerCase();
    for (let k = 0; k < lower.length; k += 1) origin.push(offset);
    folded += lower;
    offset += ch.length;
  }
  // One past the end, so a match that runs to the end of the line can close.
  origin.push(text.length);

  const ranges = [];
  let at = folded.indexOf(needle);
  while (at !== -1) {
    const start = origin[at];
    const end = origin[at + needle.length];
    // A term that matched only half of one character's folded form covers no
    // original characters; there is nothing honest to draw a block behind.
    if (end > start) ranges.push({ start, end });
    at = folded.indexOf(needle, at + needle.length);
  }
  return ranges;
}

// The term marked with a `--sel` block behind it (design: "never a colour").
//
// **Split text nodes and spans, never an HTML string.** A line is user text —
// a finding that says `<b>table</b>` is a finding about markup, and it renders
// as those exact characters here because nothing in this function parses
// anything.
function markInto(node, text, term) {
  const ranges = markRanges(text, term);
  if (ranges.length === 0) {
    node.textContent = text;
    return;
  }
  const parts = [];
  let at = 0;
  for (const { start, end } of ranges) {
    if (start > at) parts.push(document.createTextNode(text.slice(at, start)));
    const mark = document.createElement("span");
    mark.className = "mark";
    mark.textContent = text.slice(start, end);
    parts.push(mark);
    at = end;
  }
  if (at < text.length) parts.push(document.createTextNode(text.slice(at)));
  node.replaceChildren(...parts);
}

// A line's text, marked while a term is active. One place, so the transcript
// row and the result preview cannot mark differently.
function setSearchableText(node, text) {
  if (searching()) markInto(node, text, searchTerm());
  else node.textContent = text;
}

// -- Running one --------------------------------------------------------------

// Two results are "the same" when the same notes matched the same term the same
// number of times. Used only to decide whether the 180ms crossfade is owed: a
// re-scan that changed nothing must not blink the tree.
function resultSignature(result) {
  if (!result) return "";
  const notes = result.notes
    .map((note) => `${note.draft_id}:${note.matches}`)
    .join(",");
  return `${result.term}|${notes}`;
}

// "Search filter — 180ms ease-out", from the design's motion table. The animation
// is CSS's; this only re-arms it, by removing the attribute and forcing a
// reflow before setting it again.
function fadeSearch() {
  for (const id of ["draft-list", "line-list", "search-results"]) {
    const node = el(id);
    if (!node) continue;
    node.removeAttribute("data-search-fade");
    void node.offsetWidth;
    node.dataset.searchFade = "yes";
  }
}

function applySearchResult(outcome) {
  const changed = resultSignature(search.result) !== resultSignature(outcome);
  search.result = outcome;
  // The tree and the pane both read `search`, so both are re-derived from the
  // payloads this window already has — nothing is re-fetched for a filter.
  renderDrafts(lastDrafts);
  renderTranscript(transcript);
  if (changed) fadeSearch();
}

async function dispatchSearch() {
  const term = searchTerm();
  // Bumped whatever happens: an answer for an older term must never land on a
  // newer one, and clearing the field is the same kind of event as typing.
  const seq = (search.seq += 1);
  if (term === "") {
    applySearchResult(null);
    return;
  }
  try {
    const outcome = await invoke("search_notes", { term });
    if (seq !== search.seq) return;
    applySearchResult(outcome);
  } catch (err) {
    reportFailure(err);
  }
}

const queueSearch = debounce(dispatchSearch, SEARCH_DEBOUNCE_MS);

// The one way the term ever changes: typing, Escape, the keycap, "Show whole
// note", and nothing else.
function setSearchTerm(value) {
  const before = searchTerm();
  search.term = value;
  const now = searchTerm();
  const field = el("search");
  if (field.value !== value) field.value = value;
  el("search-clear").dataset.open = now === "" ? "no" : "yes";
  if (now === before) return;

  if (now === "") {
    // Clearing is immediate and never debounced: Escape puts everything back
    // at once, and the bumped token drops an answer that is still in flight.
    search.seq += 1;
    applySearchResult(null);
    return;
  }

  if (before === "") {
    // Entering a term. The pane is about to swap to matches, and an edit
    // mid-flight must not be silently committed by that — so it is
    // **reverted**, exactly as Escape does. The armed discard goes with it,
    // because the row it was armed on may be about to leave the tree.
    cancelEdit();
    // And the selection and the armed delete, for the same reason twice over:
    // the state model makes them exclusive with a search, and a set the filter
    // has hidden is a set the user can no longer see.
    if (selection.size > 0 || selectionAnchor !== null || pendingDelete !== null) {
      forgetSelection();
      pendingDelete = null;
      renderTranscript(transcript);
    }
    if (pendingDiscard !== null) {
      clearPending();
      renderDrafts(lastDrafts);
    }
    // And an open rename field, reverted like everything else here: the tree
    // is about to filter, and a field on a row the filter is removing is a
    // question about something the user can no longer see.
    cancelRename();
  }
  queueSearch();
}

// A `sotone://lines` or `sotone://drafts` event re-runs the scan through the same
// debounce, so a line that lands mid-search appears if it matches. Typing is
// the only other trigger: no event, no re-scan.
function rescanIfSearching() {
  if (searchTerm() !== "") queueSearch();
}

// -- Rendering ----------------------------------------------------------------

// One result row: name · project · "N matches · when", then the first matching
// line as a one-line ellipsised preview with the term marked.
function resultRow(draft, note) {
  const item = document.createElement("li");
  item.className = "result";
  item.dataset.id = draft.id;
  // Reachable from the keyboard, like every other row-sized target here.
  item.tabIndex = 0;

  const head = document.createElement("div");
  head.className = "result__head";

  const name = document.createElement("span");
  name.className = "result__name";
  // The sidebar's label rule, so a note is called the same thing in both
  // places.
  name.textContent = noteLabel(draft);

  const project = document.createElement("span");
  project.className = "result__project";
  const known =
    draft.project && lastProjects.projects.some((p) => p.name === draft.project);
  project.textContent = draft.project || "no project";
  project.title = draft.project
    ? known
      ? "The project this note belongs to"
      : "This note names a project your config no longer has"
    : "This note has no project yet";

  const meta = document.createElement("span");
  meta.className = "result__meta";
  // `last_written` is the backend's answer — the newest live line's time, or
  // the draft's creation time when it has never been written to. What "short"
  // looks like is this window's locale's business, as everywhere else.
  meta.textContent = `${matchLabel(note.matches)} · ${whenLabel(note.last_written)}`;

  head.append(name, project, meta);

  const preview = document.createElement("div");
  preview.className = "result__preview";
  const first = note.lines[0];
  if (first) markInto(preview, first.text, searchTerm());

  item.append(head, preview);

  const open = async () => {
    // The term stays: opening a result switches to the note-scoped behaviour
    // for that note, which is the design's own sentence.
    showPane("pane-session");
    if (draft.id === lastDrafts.active_id) return;
    try {
      await invoke("draft_open", { id: draft.id });
    } catch (err) {
      reportFailure(err);
    }
  };
  item.addEventListener("click", open);
  item.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    open();
  });
  return item;
}

// The matching notes in the sidebar's own sort order, paired with the drafts
// payload they are drawn from. A result whose draft is not in that payload is
// dropped rather than half-drawn: it was discarded between the scan and now.
function searchResultRows() {
  if (!search.result) return [];
  const rows = [];
  for (const note of search.result.notes) {
    const draft = lastDrafts.drafts.find((d) => d.id === note.draft_id);
    if (draft) rows.push({ draft, note });
  }
  const order = SORTS[sortMode] || SORTS.newest;
  rows.sort((a, b) => order(a.draft, b.draft));
  return rows;
}

function renderSearchResults() {
  const list = el("search-results");
  list.replaceChildren();
  if (searchScope() !== "library") return;

  const rows = searchResultRows();
  if (rows.length === 0) {
    const empty = document.createElement("li");
    empty.className = "muted";
    // Never a blank pane: zero matches is an answer, and it is said out loud.
    empty.textContent = `Nothing contains ${quoted(searchTerm())}.`;
    list.append(empty);
    return;
  }
  for (const { draft, note } of rows) list.append(resultRow(draft, note));
}

// The chrome: the scope attribute every CSS swap hangs off, the three counters,
// and the results list. Called from `renderNoteHead`, which every render path
// already reaches — and it calls nothing that would come back here.
function renderSearch() {
  const scope = searchScope();
  el("view-ready").dataset.searching = scope;
  el("search-clear").dataset.open = searchTerm() === "" ? "no" : "yes";

  const found = el("search-count");
  const facts = el("search-facts");
  if (scope === "no") {
    found.dataset.open = "no";
    found.textContent = "";
    facts.dataset.open = "no";
    facts.textContent = "";
    el("search-results").replaceChildren();
    return;
  }

  const term = searchTerm();
  const totals = searchTotals();

  // The tree's heading: "6 matches · 3 notes".
  found.dataset.open = "yes";
  found.textContent = `${matchLabel(totals.matches)} · ${noteCountLabel(totals.notes)}`;
  found.title = `Notes containing ${quoted(term)}`;

  if (scope === "note") {
    // Counted from the rows the pane is actually showing — the intersection of
    // the transcript this window holds and the ids the scan returned — so the
    // header can never claim a match the card is not listing.
    const shown = searchIdsFor(transcript.draft_id);
    const here = shown
      ? transcript.lines.filter((line) => shown.has(line.id)).length
      : 0;
    const matches = el("pane-matches");
    matches.textContent = here === 1 ? "1 matching line" : `${here} matching lines`;
    matches.title = `Lines in this note containing ${quoted(term)}`;
    facts.dataset.open = "yes";
    const projects =
      totals.projects === 1 ? "1 project" : `${totals.projects} projects`;
    facts.textContent = `${matchLabel(totals.matches)} · ${noteCountLabel(totals.notes)} · ${projects}`;
  } else {
    el("search-title").textContent = `Notes containing ${quoted(term)}`;
    el("search-meta").textContent =
      `${noteCountLabel(totals.notes)} · ${totals.matches === 1 ? "1 line" : `${totals.matches} lines`}`;
    const echo = el("search-sort");
    // Capitalised, as the mock has it. Text, not a second sort control.
    echo.textContent = sortLabel().charAt(0).toUpperCase() + sortLabel().slice(1);
    echo.title = "The order the sidebar's sort is set to";
    facts.dataset.open = "yes";
    facts.textContent = `sorted by ${sortLabel()}`;
  }

  renderSearchResults();
}

// ---------------------------------------------------------------------------
// The window itself: chrome and theme
//
// `decorations: false`, so the strip at the top of index.html is the caption.
// Three things live here and nothing else:
//
// * the three caption buttons, which call `minimize` / `toggleMaximize` /
//   `close` and nothing else. There is no `setFocus`, no `show` and no raise
//   anywhere in this file, and the capability file grants none of them
//   (invariant 2). `close()` rather than `destroy()` so the Rust
//   `CloseRequested` handler — which tears the overlay down — still runs.
// * the maximized flag, which decides between the maximise and restore glyphs.
//   Read from the window (`isMaximized`) and refreshed on resize, never
//   guessed at from the click: Win+arrow and a double-click on the bar both
//   change it without going through our buttons.
// * `data-theme` and `data-platform` on <html>, both applied from the settings
//   event. The theme is a config value; the platform is a compile-time `cfg`
//   the backend publishes, so nothing here sniffs a user agent.
// ---------------------------------------------------------------------------

// The three platforms the design draws chrome for. Anything else is treated as
// Linux, which is what the backend sends for every non-Windows, non-macOS
// target anyway.
const PLATFORMS = ["windows", "macos", "linux"];

// This window's handle, or `null` if the API is not there at all.
function appWindow() {
  return windowApi ? windowApi.getCurrentWindow() : null;
}

// Every caption button goes through here: one place that catches, one place
// that reports. A rejected window call is worth saying out loud — a dead
// caption button with a silent console is exactly the kind of thing that
// survives to release.
function captionAction(name, run) {
  const win = appWindow();
  if (!win) {
    handleNotice({
      level: "warn",
      message: `the window controls are unavailable, so ${name} did nothing`,
    });
    return;
  }
  Promise.resolve()
    .then(() => run(win))
    .catch(reportFailure);
}

async function refreshMaximized() {
  const win = appWindow();
  if (!win) return;
  try {
    const maximized = await win.isMaximized();
    el("titlebar").dataset.maximized = maximized ? "yes" : "no";
  } catch (err) {
    // Not worth a notice: the glyph is cosmetic and the button still works.
    reportFailure(err);
  }
}

// Absence is dark (tokens.css treats a missing `data-theme` as the dark
// palette), so light is the only value ever written. Anything the backend
// cannot name is dark, which is also the config's default.
function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === "light") root.dataset.theme = "light";
  else delete root.dataset.theme;
  const select = el("theme-select");
  if (select) select.value = theme === "light" ? "light" : "dark";
}

function applyPlatform(platform) {
  if (!PLATFORMS.includes(platform)) return;
  document.documentElement.dataset.platform = platform;
}

function wireChrome() {
  el("win-minimize").addEventListener("click", () => {
    captionAction("minimize", (win) => win.minimize());
  });
  el("win-maximize").addEventListener("click", () => {
    captionAction("maximize", (win) => win.toggleMaximize());
  });
  el("win-close").addEventListener("click", () => {
    // `close()`, never `destroy()`: this is the same path the native X took,
    // so the backend's teardown still runs.
    captionAction("close", (win) => win.close());
  });

  el("theme-select").addEventListener("change", () => {
    // Like every other control in Settings this only sends; the settings event
    // that comes back is what actually flips the palette.
    invoke("set_theme", { theme: el("theme-select").value }).catch(reportFailure);
  });

  const win = appWindow();
  if (!win) return;
  refreshMaximized();
  // Maximize is not only ours to trigger: the drag region's double-click, the
  // Win+arrow snap and a drag to the top edge all change it. `onResized` is the
  // one signal that covers every route.
  win.onResized(refreshMaximized).catch(reportFailure);
}

async function main() {
  // Listeners first: a status event fired between the snapshot and the listen
  // would otherwise be lost, and a missed "ready" leaves a permanent spinner.
  await listen(EVENT.status, (event) => {
    seen.add("status");
    renderStatus(event.payload);
  });
  await listen(EVENT.armed, (event) => {
    seen.add("armed");
    renderArmed(event.payload);
  });
  await listen(EVENT.recording, (event) => {
    seen.add("recording");
    renderRecording(event.payload);
  });
  await listen(EVENT.line, (event) => {
    seen.add("line");
    renderLine(event.payload);
  });
  await listen(EVENT.lines, (event) => {
    seen.add("lines");
    // Whatever was waiting on a re-transcribe has now been answered — this is
    // the answer, and the only place that can tell whether it changed anything.
    reconcileRetranscribe(event.payload);
    pendingRetranscribe.clear();
    // The note the conflict is about has moved, so the diff's right-hand side —
    // what the *refused* save would have written — is now older than the note.
    // Marked rather than silently redrawn: this window has the pending text,
    // not the ability to render the markdown again.
    if (conflictFor !== null && event.payload.draft_id === conflictFor) {
      conflictStale = true;
    }
    renderTranscript(event.payload);
    renderCondition();
    // A line landing mid-search appears if it matches. Through the
    // same debounce, so a burst of appends is one scan.
    rescanIfSearching();
  });
  // The rows for utterances that are not lines yet. A state contract
  // like the transcript: the whole list arrives every time, and this window
  // never adds to it or takes from it on its own.
  await listen(EVENT.pending, (event) => {
    seen.add("pending");
    applyPending(event.payload);
  });
  await listen(EVENT.notice, (event) => {
    // A re-transcribe that produced nothing reports a notice and no new
    // transcript, so this is the other end of the pending state. `debug`
    // notices are excluded: they are lifecycle traffic that can
    // arrive at any moment — a helper respawn, a model swap — and any one of
    // them would otherwise end a spinner that is still waiting for its answer.
    if (pendingRetranscribe.size > 0 && event.payload.level !== "debug") {
      pendingRetranscribe.clear();
      // No transcript came back, so nothing changed and there is nothing to
      // undo: drop the captured text rather than leave a dead step waiting.
      retranscribeWas.clear();
      renderTranscript(transcript);
    }
    handleNotice(event.payload);
  });
  // A state, not a message. Rendered as it arrives and never inferred from
  // anything else that lands on the way.
  await listen(EVENT.condition, (event) => {
    seen.add("condition");
    applyCondition(event.payload);
  });
  await listen(EVENT.save, (event) => {
    renderSaveOutcome(event.payload);
  });
  await listen(EVENT.saveAll, (event) => {
    renderSaveAllOutcome(event.payload);
  });
  // The drop's question. Rendered, never inferred: a clash is
  // something the backend found on disk, and this window has no filesystem to
  // find it with.
  await listen(EVENT.noteClash, (event) => {
    openNoteClash(event.payload);
  });
  await listen(EVENT.projects, (event) => {
    seen.add("projects");
    renderProjects(event.payload);
  });
  await listen(EVENT.settings, (event) => {
    seen.add("settings");
    renderSettings(event.payload);
  });
  // The tray's Settings item, arriving as an event because the tray
  // cannot reach into this document. The window is already shown and focused by
  // the time this lands — the tray did that, deliberately, on a click — so all
  // that is left here is the in-window swap Settings is. Not added
  // to `seen`: there is no snapshot field to fall back to, by design.
  await listen(EVENT.view, (event) => {
    if (event.payload.view === "settings") showSettings(true);
  });
  await listen(EVENT.drafts, (event) => {
    seen.add("drafts");
    // A list that arrived while a discard was armed is a list that may have
    // renumbered under it; forget the arming rather than fire it at a row the
    // user is no longer looking at.
    clearPending();
    renderDrafts(event.payload);
    // Same rule as the transcript's: the drafts moved, so what matched may
    // have too.
    rescanIfSearching();
  });

  // The model guide, cloned into the repair panel and the wizard's model step
  // before anything below wires a listener, because the Copy
  // buttons it brings are wired by class rather than by id — there are two of
  // them once this has run.
  mountModelGuides();

  // The caption buttons and the theme control. Before the snapshot below, so a
  // settings payload that arrives with it finds the select already wired.
  wireChrome();

  // Reflect the stored preference before the first render, so the control and
  // the list cannot disagree on the very first paint.
  const sortSelect = el("draft-sort");
  sortSelect.value = sortMode;
  // The control is a glyph (the field took the label's width), so the
  // order it is set to has to be readable somewhere that is not the open menu.
  const nameSort = () => {
    sortSelect.title = `Sorted by ${sortLabel()}`;
  };
  nameSort();
  sortSelect.addEventListener("change", () => {
    sortMode = Object.prototype.hasOwnProperty.call(SORTS, sortSelect.value)
      ? sortSelect.value
      : "newest";
    try {
      localStorage.setItem(SORT_KEY, sortMode);
    } catch {
      // Not persisting a view preference is survivable; the session keeps it.
    }
    nameSort();
    renderDrafts(lastDrafts, true);
    // The library-scope list follows the same sort, and the header and footer
    // echo it, so a change here has to reach all three.
    renderSearch();
  });

  // Search. The field is the whole switch: typing runs a debounced
  // scan, an empty term is search-off, and Escape — or the keycap, or "Show
  // whole note" — empties it. Nothing here is persisted and nothing is written.
  const searchField = el("search");
  searchField.addEventListener("input", () => setSearchTerm(searchField.value));
  searchField.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    event.preventDefault();
    setSearchTerm("");
  });
  el("search-clear").addEventListener("click", () => {
    setSearchTerm("");
    // The caret goes back where the user was typing. DOM focus inside a window
    // they just clicked in — not window activation, which nothing in this file
    // does (invariant 2).
    searchField.focus();
  });
  // "Show whole note" **clears the search**: one way out, never a third state
  // where the tree is filtered and the pane is not.
  el("search-exit").addEventListener("click", () => setSearchTerm(""));

  // The gear is a toggle, so the way out of Settings is the way in — and the
  // row pinned to the bottom of the settings sidebar is the design's own way
  // back. Neither touches the notes shell's state: it is still there, mid-swap,
  // exactly as it was left.
  el("open-settings").addEventListener("click", () => {
    showSettings(!settingsOpen);
  });
  el("settings-close").addEventListener("click", () => showSettings(false));

  // The tabs. Which one is selected is this window's business alone — no
  // command, no event, nothing persisted.
  for (const tab of el("settings-tabs").querySelectorAll(".tab")) {
    tab.addEventListener("click", () => showTab(tab.dataset.tab));
  }
  showTab(currentTab);

  // The breadcrumb's two pencils. Each opens the field in the
  // crumb it belongs to; what they can rename is decided in `renderRename`,
  // which is also what hides them while a recording is live.
  el("rename-note").addEventListener("click", () => {
    const draft = activeDraft();
    if (!draft || !draft.saved_path) return;
    beginRename("note", "crumb", draft.id, noteLabel(draft));
  });
  el("rename-project").addEventListener("click", () => {
    const draft = activeDraft();
    const name = (draft && draft.project) || lastProjects.active;
    if (!name || !lastProjects.projects.some((p) => p.name === name)) return;
    beginRename("project", "crumb", name, name);
  });

  el("project-close").addEventListener("click", () => showPane("pane-session"));
  el("new-project-close").addEventListener("click", () =>
    showPane("pane-session"),
  );
  el("project-new-open").addEventListener("click", () => {
    showPane("pane-new-project");
    // Every (re)open resets the create-subfolder switch.
    openCreateForm("new-project");
  });

  // Settings controls. Each one sends and waits for the settings event; none
  // of them decides anything locally, so a refusal on the backend (the last
  // recording mode, the model in use) simply leaves the control as it was.
  el("capture-cancel").addEventListener("click", () => {
    invoke("hotkey_capture_cancel").catch(reportFailure);
  });
  el("mic-select").addEventListener("change", () => {
    const device = el("mic-select").value;
    invoke("set_mic", { device: device === SYSTEM_DEFAULT ? null : device }).catch(
      reportFailure,
    );
  });
  el("language-select").addEventListener("change", () => {
    invoke("set_language", { language: el("language-select").value }).catch(
      reportFailure,
    );
  });
  el("cue-toggle").addEventListener("change", () => {
    invoke("set_cues", { on: el("cue-toggle").checked }).catch(reportFailure);
  });
  el("overlay-toggle").addEventListener("change", () => {
    invoke("set_overlay", { on: el("overlay-toggle").checked }).catch(reportFailure);
  });
  // Nothing in this window acts on the answer: the backend decides
  // what the X does, reading the config at the moment it is pressed, and the
  // settings event that comes back is what this row is drawn from.
  el("close-quits-toggle").addEventListener("change", () => {
    invoke("set_close_quits", {
      on: el("close-quits-toggle").checked,
    }).catch(reportFailure);
  });
  el("overlay-corner").addEventListener("change", () => {
    invoke("set_overlay_corner", { corner: el("overlay-corner").value }).catch(
      reportFailure,
    );
  });
  el("reveal-seconds").addEventListener("change", () => {
    invoke("set_reveal_seconds", {
      seconds: Number(el("reveal-seconds").value),
    }).catch(reportFailure);
  });
  el("models-reveal").addEventListener("click", revealModelsFolder);
  el("models-rescan").addEventListener("click", rescanModels);

  // The first-run panel. Two of the three controls are the same
  // commands the Settings pane has; the third relaunches the process, which is
  // the only way a newly chosen model gets loaded. The Copy button is not here:
  // it arrives with the cloned model guide and is wired by class in
  // `mountModelGuides`, because there are two of it.
  el("empty-reveal").addEventListener("click", revealModelsFolder);
  el("empty-rescan").addEventListener("click", rescanModels);
  el("empty-restart").addEventListener("click", () => {
    // The command never returns on success — the process is replaced — so
    // there is nothing to await and nothing to render afterwards.
    invoke("app_restart").catch(reportFailure);
  });

  // The wizard. Every one of these sends a command that already
  // existed, except the two questions it adds — and none of them decides
  // anything locally: the events that come back are what redraw the step.
  el("wiz-back").addEventListener("click", () => {
    wizardGoto(wizardStep - 1, "back");
  });
  el("wiz-next").addEventListener("click", wizardNext);
  el("wiz-mic").addEventListener("change", () => {
    const device = el("wiz-mic").value;
    invoke("set_mic", {
      device: device === SYSTEM_DEFAULT ? null : device,
    }).catch(reportFailure);
  });
  el("wiz-root-pick").addEventListener("click", async () => {
    const chosen = await pickFolder();
    if (!chosen) return;
    // Wizard state, not configuration: nothing is written until the project
    // step creates a project under it.
    wizardRoot = chosen;
    renderWizard();
    renderWizardExample();
  });
  el("wiz-model-reveal").addEventListener("click", revealModelsFolder);
  el("wiz-model-rescan").addEventListener("click", rescanModels);
  el("wiz-project").addEventListener("input", () => {
    wizardNameTouched = true;
    refreshWizardSlug();
  });
  el("wiz-project").addEventListener("keydown", (event) => {
    // Enter in the one field the wizard has does what the footer's button
    // does. A listener on the field itself — it never reaches the document's
    // keydown ladder, which the wizard does not touch at all.
    if (event.key !== "Enter") return;
    event.preventDefault();
    wizardNext();
  });

  // The Godot dialog's three controls: the picker, the switch and
  // the echo. Setting `.value` from script fires no input event, so the browse
  // answer redraws the echo itself.
  el("new-project-name").addEventListener("input", () =>
    refreshCreateSlug["new-project"](),
  );
  el("new-project-subfolder").addEventListener("change", () =>
    renderCreateEcho("new-project"),
  );
  el("new-project-dir").addEventListener("input", () =>
    renderCreateEcho("new-project"),
  );
  el("new-project-browse").addEventListener("click", async () => {
    const chosen = await pickFolder();
    if (!chosen) return;
    el("new-project-dir").value = chosen;
    renderCreateEcho("new-project");
  });
  el("new-project-create").addEventListener("click", async () => {
    const name = el("new-project-name").value.trim();
    const picked = el("new-project-dir").value.trim();
    if (!name || !picked) {
      handleNotice({
        level: "warn",
        message: "a project needs a name and a notes folder",
      });
      return;
    }
    try {
      // **The window composes the path, exactly as the wizard always has**
      // — `project_create` still takes `{name, notesDir}`, and what
      // goes in it is the folder the echo just promised — the picked folder
      // with the project's own folder inside it, or the picked folder itself
      // with the switch off.
      const notesDir = await createNotesDir("new-project");
      // Acceptance is the answer, not the resolved promise: every refusal
      // — a taken name, a folder that could not be created — resolves as well,
      // carrying its reason in a notice. Anything but `"created"` leaves the
      // name, the folder and the switch exactly as the user typed them, on the
      // pane they are already looking at.
      const outcome = await invoke("project_create", { name, notesDir });
      if (outcome !== "created") return;
      clearCreateForm("new-project");
      // Back to the notes, where the new group appearing in the tree is the
      // confirmation. Deliberately not straight into the new project's panel:
      // the projects event that would fill it has not necessarily arrived yet,
      // and a panel that says "no such project" for a frame is worse than none.
      showPane("pane-session");
    } catch (err) {
      reportFailure(err);
    }
  });

  // The pick-or-create answers. Both end with the same `draft_save` the Save
  // button sends — the note is unchanged, it simply has somewhere to go now.
  el("no-project-use").addEventListener("click", async () => {
    const name = el("no-project-select").value;
    if (!name) return;
    closeNoProject();
    try {
      await invoke("project_set_active", { name });
      saveDraft(false);
    } catch (err) {
      reportFailure(err);
    }
  });
  // The same three controls as the pane form. Same grammar, same
  // composer, same default — two creation surfaces that disagree about what
  // folder a project gets is the whole defect.
  el("no-project-name").addEventListener("input", () =>
    refreshCreateSlug["no-project"](),
  );
  el("no-project-subfolder").addEventListener("change", () =>
    renderCreateEcho("no-project"),
  );
  el("no-project-dir").addEventListener("input", () =>
    renderCreateEcho("no-project"),
  );
  el("no-project-browse").addEventListener("click", async () => {
    const chosen = await pickFolder();
    if (!chosen) return;
    el("no-project-dir").value = chosen;
    renderCreateEcho("no-project");
  });
  el("no-project-create").addEventListener("click", async () => {
    const name = el("no-project-name").value.trim();
    const picked = el("no-project-dir").value.trim();
    if (!name || !picked) {
      handleNotice({
        level: "warn",
        message: "a project needs a name and a notes folder",
      });
      return;
    }
    // Composed while the fields are still on screen — the popup now closes
    // after the answer, not before it — and `project_create`'s contract is
    // unchanged.
    let notesDir;
    try {
      notesDir = await createNotesDir("no-project");
    } catch (err) {
      reportFailure(err);
      return;
    }
    try {
      const outcome = await invoke("project_create", { name, notesDir });
      // The popup closes on an acceptance and on nothing else. Closing first
      // was the bug: a refusal still resolves, so `saveDraft` ran, answered
      // `no_project`, and reopened this popup blank — the user's name and
      // folder gone with the reason they were refused. Returning leaves it
      // open, untouched. Deliberately not `openCreateForm`/`openNoProject`:
      // both put the create-a-subfolder switch back to on.
      if (outcome !== "created") return;
      closeNoProject();
      clearCreateForm("no-project");
      saveDraft(false);
    } catch (err) {
      // A failure leaves it open too — there is still no project to save into.
      reportFailure(err);
    }
  });
  // Cancel means nothing was written, which is already true.
  el("no-project-cancel").addEventListener("click", closeNoProject);

  // The clash question's two answers. Keep both is the *only* place
  // in this window that sends `keepBoth: true`, exactly as the conflict
  // dialog's Keep mine is the only place that sends `overwrite: true`.
  el("note-clash-keep").addEventListener("click", () => {
    if (noteClash === null) return;
    const { id, project } = noteClash;
    // Closed first: the answer re-renders the tree, and a question left on
    // screen over a rebuilt tree is a question about the row that was there.
    closeNoteClash();
    invoke("draft_set_project", { id, project, keepBoth: true }).catch(
      reportFailure,
    );
  });
  // Cancel sends nothing at all — and nothing was written when the question was
  // asked, so the drop simply did not happen (invariant 4).
  el("note-clash-cancel").addEventListener("click", closeNoteClash);

  // The move chooser's two answers. Cancel — and Escape, and a
  // recording starting — send nothing: until "Move them" is clicked, no line
  // has been asked to go anywhere.
  el("move-note-go").addEventListener("click", moveSelection);
  el("move-note-cancel").addEventListener("click", closeMoveChooser);

  // One step at a time, like an editor. Undo
  // issues the inverse of the last correction — never anything that could
  // remove a dictated line.
  el("lines-undo").addEventListener("click", undoStep);
  el("lines-redo").addEventListener("click", redoStep);
  el("lines-save").addEventListener("click", () => saveDraft(false));
  el("lines-save-all").addEventListener("click", saveAllDrafts);

  // The selection's three actions. Copy is read-only
  // and always offered; Move to note… and Delete N carry the header's own
  // gates and say why when they refuse. None of the three is a new write path:
  // Copy is the clipboard, Delete N is N of the soft delete a row's own tool
  // already sends, and Move to note… is N `line_move_to` — the same soft
  // delete on this side, with an append on the other.
  el("sel-copy").addEventListener("click", copySelection);
  el("sel-move").addEventListener("click", openMoveChooser);
  el("sel-delete").addEventListener("click", deleteSelection);

  // The context menu's keyboard, and the three teardowns that are events on
  // something other than the menu itself.
  wireMenuKeys();

  // Every dropdown in the window, styled. After the sort control
  // above, whose value and `title` are set before this runs and are read off
  // it here; before the snapshot below, so a settings payload arriving with it
  // finds nine controls already enhanced and nine `change` listeners already
  // where they have always been.
  wireDrops();
  for (const id of DROP_IDS) enhanceSelect(el(id));
  // Click-away. Capture, so it runs before whatever was clicked does its own
  // work, and `pointerdown` as well as `click` because a native menu goes away
  // on the press — a page driven programmatically only ever sees the second.
  for (const kind of ["pointerdown", "click"]) {
    document.addEventListener(
      kind,
      (event) => {
        if (menuOpen === null) return;
        if (event.target instanceof Element && event.target.closest("#row-menu")) {
          return;
        }
        closeMenu();
      },
      true,
    );
  }
  // A right-click anywhere else. **No `preventDefault` here**: fields, the
  // transcript and empty space keep the WebView's own menu, which is the right
  // menu for text. A row's own handler has already prevented it and opened
  // ours, and this is how that is told apart.
  document.addEventListener("contextmenu", (event) => {
    if (event.defaultPrevented) return;
    closeMenu();
  });
  // Scrolling the tree moves the row out from under it.
  el("draft-list").addEventListener("scroll", () => closeMenu());

  // The strip's three controls. What they mean depends on which
  // condition raised it, and the strip only ever shows one.
  el("strip-primary").addEventListener("click", () => {
    if (activeCondition() === "noDevice") {
      // The in-window view swap, which is what "open settings" means here.
      // Nothing shows, activates or focuses a window anywhere in this file
      // (invariant 2) — the design's Settings *window* is a shell inside this
      // one, deliberately.
      showSettings(true);
      showTab("recording");
      return;
    }
    if (activeCondition() !== "fileConflict") return;
    // Belt and braces on top of the teardown in `renderDrafts`: a drafts event
    // can land between the mousedown and this handler, so the answer is checked
    // against the question one last time before the one command in this app
    // that can discard somebody else's file is sent.
    const asked = conflictFor;
    if (asked === null || asked !== lastDrafts.active_id) return;
    // And the same recording gate the button's `disabled` shows, because the
    // state can move between the render and the click.
    if (recordingLive) return;
    forgetConflict();
    renderCondition();
    saveDraft(true);
  });
  // "Leave the file" / "Dismiss". Nothing is written either way: the backend
  // stopped before a byte, and this only puts the strip away.
  el("strip-dismiss").addEventListener("click", dismissCondition);
  el("strip-diff").addEventListener("click", () => {
    diffOpen = !diffOpen;
    renderCondition();
  });

  // The hotkey-dead pane's two actions. Restart is the existing command;
  // Recheck is the newer one, and it is the only lever that gets past the
  // supervisor's restart limit without relaunching the process.
  el("hotkey-restart").addEventListener("click", () => {
    invoke("app_restart").catch(reportFailure);
  });
  el("hotkey-recheck").addEventListener("click", () => {
    invoke("hook_recheck").catch(reportFailure);
  });

  // The debug log's switch. A display preference: it is read from localStorage
  // at load and written back here, and it reaches no command and no event.
  el("debug-toggle").addEventListener("change", () => {
    showDebugLog(el("debug-toggle").checked);
  });
  showDebugLog(debugOpen);

  // Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y / Ctrl+S and Escape, on real keystrokes
  // inside this window: a DOM listener, never injected input, and nothing here
  // can generate one (invariant 1). Gated so it never fights a control that
  // owns those keys itself, and `preventDefault` only on the ones actually
  // handled.
  document.addEventListener("keydown", (event) => {
    // Escape, in one place and in one order: an open **editor**
    // consumes it first and reverts, else an armed **confirm** cancels, else a
    // **selection** clears. The search field's Escape stays in the field.
    //
    // The two listeners that own the key on their own element — the line
    // editor's and the search field's — run first, in the target phase, and
    // both `preventDefault`. Checking that here is what makes this a *ladder*
    // rather than two listeners racing over one press: whoever answered first
    // has said so, and this stops.
    if (event.key === "Escape") {
      if (event.defaultPrevented) return;
      // The context menu is the topmost transient and answers first.
      // With focus inside it its own handler has already said so through
      // `defaultPrevented`; this is the same rung for the press that lands
      // anywhere else while it is open. Either way it is **one** answer: the
      // ladder below is not reached, so a selection survives the Escape that
      // closed the menu.
      if (menuOpen !== null) {
        event.preventDefault();
        closeMenu(true);
      } else if (moveChooser !== null) {
        // The move chooser is the next transient down: it is a
        // picker the user opened, not a question the backend is waiting on, so
        // walking away is a real answer and it sends nothing. One rung, one
        // answer — the selection it acts on survives the press that closed it.
        event.preventDefault();
        closeMoveChooser();
      } else if (editing) {
        event.preventDefault();
        cancelEdit();
      } else if (pendingDelete !== null) {
        event.preventDefault();
        disarmDelete();
      } else if (selection.size > 0) {
        event.preventDefault();
        clearSelection();
      }
      return;
    }
    if (!event.ctrlKey || event.altKey || event.metaKey) return;
    const key = event.key.toLowerCase();
    const undo = key === "z" && !event.shiftKey;
    const redo = (key === "z" && event.shiftKey) || key === "y";
    const save = key === "s";
    if (!undo && !redo && !save) return;
    if (!shortcutAllowed(event.target)) return;
    // Even when the gate passes, Ctrl+S with nothing to save does nothing —
    // but it must still not fall through to the webview's own Save dialog.
    event.preventDefault();
    if (save) saveDraft(false);
    else if (undo) undoStep();
    else redoStep();
  });

  // The gap under the last row: a drop there means "to the end". Rows stop
  // their own drops from reaching this.
  const lineList = el("line-list");
  lineList.addEventListener("dragover", (event) => {
    if (!dragging) return;
    event.preventDefault();
    if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    clearDropMarks();
  });
  lineList.addEventListener("drop", (event) => {
    if (!dragging) return;
    event.preventDefault();
    const held = dragging;
    endDrag();
    const last = transcript.lines[transcript.lines.length - 1];
    if (last) moveLine(held, anchorFor(held, last.id, true));
  });

  // The invite's button, for the first note in a project — every later one is
  // made from the `+` on the project's row in the tree.
  el("draft-new").addEventListener("click", async () => {
    clearPending();
    // The new note is about to become the active one, so show the transcript it
    // will land in rather than leaving Settings on screen.
    showPane("pane-session");
    await invoke("draft_new");
  });

  armButton.addEventListener("click", async () => {
    armButton.disabled = true;
    try {
      renderArmed(await invoke("set_armed", { armed: !userArmed }));
    } finally {
      armButton.disabled = false;
    }
  });

  const snapshot = await invoke("shell_status");
  if (!seen.has("status")) renderStatus(snapshot.status);
  if (!seen.has("armed")) renderArmed(snapshot.armed);
  if (!seen.has("recording")) renderRecording(snapshot.recording);
  // Both renderers above are guarded, and the readout depends on both states —
  // so settle it once here, after the snapshot has been folded in. Without this
  // a launch where only one of the two was applied could leave the readout on
  // the markup's default.
  renderIndicator();
  // Projects before drafts: the filter and the tooltip both read the project
  // list, and `renderProjects` re-renders the drafts itself.
  if (!seen.has("projects") && snapshot.projects) {
    renderProjects(snapshot.projects);
  }
  // Including the capture state: a window that finished loading mid-capture
  // shows "press a key…", because that is a fact about the helper process and
  // not about this page.
  if (!seen.has("settings") && snapshot.settings) {
    renderSettings(snapshot.settings);
  }
  if (!seen.has("drafts")) renderDrafts(snapshot.drafts);
  if (!seen.has("line") && snapshot.last_line) renderLine(snapshot.last_line);
  // A webview that finished loading after the last transcript event still has
  // to be able to render — and edit — the active draft's lines.
  // Pending before the transcript, so the one render below draws both halves:
  // a window reloaded with three utterances in the model must come back showing
  // three queued rows, not none (the same reason the condition is in the
  // snapshot at all).
  if (!seen.has("pending") && snapshot.pending) {
    pendingLines = Array.isArray(snapshot.pending.pending)
      ? snapshot.pending.pending
      : [];
  }
  if (!seen.has("lines") && snapshot.transcript) {
    renderTranscript(snapshot.transcript);
  } else if (pendingLines.length > 0) {
    renderTranscript(transcript);
  }
  // A condition is permanent, so a window that finished loading after
  // `sotone://condition` fired still has to find out the app is deaf — which is
  // the whole defect it exists to fix, one level up. Guarded like every other
  // snapshot field: a live event that landed first wins.
  if (!seen.has("condition") && snapshot.condition) {
    applyCondition(snapshot.condition);
  } else {
    // `applyCondition` renders; without it the strip, the footer readout and
    // the indicator would still be on the markup's defaults.
    renderCondition();
  }
  // Save depends on the drafts payload *and* the recording state, and either
  // of those renderers can have been skipped above — so settle it once here,
  // the same way the recording readout is settled.
  renderSaveState();
}

main().catch((err) => {
  // The only thing left that can be said, and saying it beats a blank window.
  el("status-detail").textContent = `the main window failed to start: ${err}`;
});
