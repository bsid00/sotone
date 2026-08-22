// The overlay pill: what capture is doing, over whatever is being tested.
// Idle it is the logo glyph and nothing else;
// while a recording is live it is four VU bars and an elapsed clock; when a
// line lands it opens far enough to show that line with its timestamp, drains a
// hairline for the configured number of seconds, and collapses back into the
// glyph.
//
// A pure read-only view, even more so than the main window: it invokes exactly
// one command — `shell_status`, for the snapshot — and then follows five events
// that the backend already publishes for other reasons. It flips no state, and
// there is no API in this window that can generate a keystroke or a mouse event
// (invariant 1), ask for focus (invariant 2), or fetch anything from anywhere
// (invariant 3). It does not even resize its own window: the shell does that,
// from the same two events this page draws from.
//
// **The pill and the title bar's indicator must never disagree.**
// Both are driven by `sotone://recording` and by nothing else. `sotone://level`
// is decoration — bar heights — and no state here is ever inferred from one
// arriving or not arriving (a whole family of bugs).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// All five are in shell.rs's event table. Only `sotone://level` was added for
// this page; the other four already existed for the main window.
const EVENT = {
  status: "sotone://status",
  recording: "sotone://recording",
  level: "sotone://level",
  line: "sotone://line",
  settings: "sotone://settings",
};

// The four states of the loop. `waiting` is the one the design leaves implicit:
// the key is up but the model has not answered yet, so the pill stays open with
// its bars at rest rather than collapsing and immediately growing again.
const STATE = {
  idle: "idle",
  recording: "recording",
  waiting: "waiting",
  reveal: "reveal",
  unavailable: "unavailable",
};

// How far each bar leans into the level. One microphone level drives four bars:
// there is no per-band data in the audio path (an FFT per buffer would be work
// nobody asked for, on the machine a game is already using), so the bands are
// fixed weights on one number rather than four measurements. Deliberate, and
// the deviation is on record.
const BAR_WEIGHTS = [0.82, 1, 0.9, 0.68];

// The floor of the VU animation in the design (`@keyframes vu`: .26 → 1).
const BAR_REST = 0.26;

const el = (id) => document.getElementById(id);

const pill = () => el("pill");
const bars = () => Array.from(document.querySelectorAll(".pill__glyph i"));

// The reveal duration, in seconds, as the settings event last reported it. The
// backend clamps it, so this is always a number the pill will honour.
let revealSeconds = 10;

// Is a recording live right now? From `sotone://recording` only.
let live = false;

// A line landed *during* a recording — its reveal is owed once that recording
// ends. Revealing it immediately would take the VU away from a user who is
// still speaking, and the indicator would go on saying the opposite.
let owed = false;

// The one collapse timer: a reveal running out, or the grace after a release
// that no line ever answers. Never two.
let collapse = null;

// The elapsed clock, which is this window's own.
let clock = null;
let startedAt = 0;

function setState(next) {
  pill().dataset.state = next;
}

function state() {
  return pill().dataset.state;
}

// -- The bars ---------------------------------------------------------------

function setBars(scaleOf) {
  bars().forEach((bar, index) => {
    bar.style.setProperty("--vu", scaleOf(index).toFixed(3));
  });
}

// Back to the logo's own heights. The 200ms ease to rest is the CSS
// transition's; this only says where to end up.
function restBars() {
  setBars(() => 1);
}

function renderLevel(payload) {
  // A level that arrives outside a recording is decoration for a state that is
  // not being drawn. Dropped rather than rendered — and never used to conclude
  // that a recording started.
  if (!live) return;
  const level = Math.min(Math.max(Number(payload.level) || 0, 0), 1);
  setBars((index) => BAR_REST + (1 - BAR_REST) * level * BAR_WEIGHTS[index]);
}

// -- The clock --------------------------------------------------------------

function paintClock() {
  const seconds = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
  const mm = String(Math.floor(seconds / 60)).padStart(2, "0");
  const ss = String(seconds % 60).padStart(2, "0");
  el("elapsed").textContent = `${mm}:${ss}`;
}

function startClock() {
  stopClock();
  startedAt = Date.now();
  paintClock();
  clock = setInterval(paintClock, 1000);
}

function stopClock() {
  if (clock !== null) clearInterval(clock);
  clock = null;
}

// -- The collapse timer -----------------------------------------------------

function clearCollapse() {
  if (collapse !== null) clearTimeout(collapse);
  collapse = null;
}

function collapseAfter(seconds) {
  clearCollapse();
  collapse = setTimeout(() => {
    collapse = null;
    setState(STATE.idle);
  }, seconds * 1000);
}

// The hairline is the timer, so it restarts with it. Removing the animation and
// forcing a reflow before putting it back is the only way to replay a running
// CSS animation from the start.
function restartDrain() {
  const drain = el("drain");
  drain.style.animation = "none";
  void drain.offsetWidth;
  drain.style.animation = "";
}

// -- The states -------------------------------------------------------------

function reveal() {
  owed = false;
  setState(STATE.reveal);
  restartDrain();
  collapseAfter(revealSeconds);
}

function renderRecording(payload) {
  const wasLive = live;
  live = Boolean(payload.live);
  if (live) {
    // A new recording supersedes whatever was on the pill — a reveal, a grace,
    // an owed line. What is happening now is what the pill shows.
    clearCollapse();
    owed = false;
    setState(STATE.recording);
    startClock();
    return;
  }

  if (!wasLive) {
    // Nothing was live, so nothing ended. The shell publishes an idle recording
    // state once at startup — that is the indicator's opening position, not the
    // end of a recording — and without this guard the pill sat open showing a
    // 00:00 clock for a whole reveal period at every launch (a probe against
    // the real window measured it still at the recording width).
    return;
  }

  stopClock();
  // Whatever the last level said, the microphone is closed: the bars ease back
  // to the logo over 200ms rather than freezing mid-stride.
  restBars();
  if (owed) {
    reveal();
    return;
  }
  // The decode is still running. Stay open, bars at rest, until the line
  // arrives — or until the grace runs out, because a skipped utterance produces
  // no line at all and the pill must not hang open waiting for one.
  setState(STATE.waiting);
  collapseAfter(revealSeconds);
}

function renderLine(payload) {
  // Rendered whatever the state: the reveal, whenever it happens, shows the
  // line that landed last.
  el("stamp").textContent = payload.spoken_at;
  el("line").textContent = payload.text;
  if (live) {
    owed = true;
    return;
  }
  reveal();
}

// The overlay is only ever *shown* in the ready phase — the shell hides it
// otherwise — but this page loads long before then and a session can end under
// it. Anything that is not a running session puts the pill back to the glyph,
// so a window that reappears never reappears mid-reveal.
function renderStatus(status) {
  if (status.phase === "ready") return;
  clearCollapse();
  stopClock();
  restBars();
  live = false;
  owed = false;
  setState(STATE.idle);
}

// Palette, dock corner and reveal duration all ride on the settings event, so
// this page needs no command of its own to read any of them.
function renderSettings(settings) {
  const root = document.documentElement;
  if (settings.theme === "light") root.dataset.theme = "light";
  else delete root.dataset.theme;
  if (settings.platform) root.dataset.platform = settings.platform;

  if (settings.overlay_corner) pill().dataset.corner = settings.overlay_corner;

  const seconds = Number(settings.reveal_seconds);
  if (Number.isFinite(seconds) && seconds > 0) {
    revealSeconds = seconds;
    // The hairline drains over exactly the duration the timer runs for: one
    // number, two consumers.
    pill().style.setProperty("--reveal-dur", `${seconds}s`);
  }
}

// The known silent-failure class, made loud: a missing ACL grant for this
// window makes `listen()` reject, and the page would otherwise sit there
// looking alive while never updating again. The pill is 45px wide with nothing
// happening and cannot ask for more room, so it says it the only way it can —
// hollow bars, the design's own "this glyph is not reporting" treatment.
function renderFailure(err) {
  setState(STATE.unavailable);
  pill().title = `Sotone overlay: no events (${err}). Restart Sotone.`;
}

// Which kinds of live event have already arrived, so the snapshot — which is
// fetched after the listeners are attached and may therefore be older than
// something that landed in between — is only applied where nothing newer
// exists. The main window does the same, for the same reason.
const seen = new Set();

async function main() {
  // Listeners first: a recording that started between the snapshot and the
  // listen would otherwise be lost, and this window has no second chance.
  await listen(EVENT.recording, (event) => {
    seen.add("recording");
    renderRecording(event.payload);
  });
  await listen(EVENT.level, (event) => renderLevel(event.payload));
  await listen(EVENT.line, (event) => {
    seen.add("line");
    renderLine(event.payload);
  });
  await listen(EVENT.status, (event) => {
    seen.add("status");
    renderStatus(event.payload);
  });
  await listen(EVENT.settings, (event) => {
    seen.add("settings");
    renderSettings(event.payload);
  });

  const snapshot = await invoke("shell_status");
  if (!seen.has("settings") && snapshot.settings) {
    renderSettings(snapshot.settings);
  }
  if (!seen.has("status")) renderStatus(snapshot.status);
  if (!seen.has("recording") && snapshot.recording) {
    renderRecording(snapshot.recording);
  }
  // The last line is deliberately *not* revealed on a cold load. A reveal is a
  // claim about what was just said; a window that reloads mid-session and opens
  // showing a line from ten minutes ago would be lying about now, to a user
  // whose eyes are on the thing under test.
}

main().catch(renderFailure);
