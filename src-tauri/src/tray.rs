//! The tray icon: Sotone without its window open.
//!
//! Closing the window no longer quits the app — it hides it (see
//! [`crate::shell::on_close`]) — so for most of a session this glyph in the
//! notification area is the *only* thing Sotone shows. That is the whole design
//! constraint: **it must never lie about whether the mic
//! is open.**
//!
//! # One reducer, three surfaces
//!
//! The glyph, the tooltip and the menu's status line are three renderings of
//! one value. They are produced by [`view`], a pure function over [`TrayFacts`]
//! — and the facts are fed by exactly the events the two windows get
//! (`sotone://status`, `sotone://armed`, `sotone://recording`,
//! `sotone://condition`, `sotone://drafts`), never by reading anything back off
//! the app. Nothing here infers a recording, and nothing here can disagree with
//! the title bar's indicator or the overlay pill, because all three are
//! downstream of the same publisher in `shell.rs`.
//!
//! # Threading (invariant 5's discipline)
//!
//! ```text
//! shell.rs events ──┐
//!                   ├─→ sotone-tray thread ─→ set_menu / set_icon / set_tooltip
//! menu click ───────┘        (and every window call the menu can cause)
//! (event-loop thread: enqueue and return)
//! ```
//!
//! * The `on_menu_event` closure runs on the **tao event-loop thread**. It
//!   parses one id, sends one message and returns. Nothing else — the same rule
//!   the rdev callback lives under, for the same reason: work there stalls the
//!   whole UI, and `set_menu`/`set_icon` from that thread would deadlock
//!   outright (they post a task to the main thread and block for its result,
//!   and the main thread is the one that would have to run it).
//! * The **`sotone-tray` thread** owns everything else: the facts, the rebuilt
//!   menu, the 1 Hz recording animation, and the window calls the menu items
//!   ask for. It is the only place `set_icon`/`set_menu`/`set_tooltip` are
//!   called, exactly as `sotone-overlay` is the only place the pill's window is
//!   touched.
//! * It is spawned before startup and lives as long as the process, because the
//!   empty and fatal phases have a tray too — there is no control thread in
//!   either of them, which is also why menu actions are routed *here* rather
//!   than into `ShellInput`; the ones the session owns
//!   (arm, open a draft) are forwarded on to the control thread from here.
//!
//! # Invariant 2 — the two focus carve-outs that live in this file
//!
//! 1. **The menu takes focus while it is open — our window now, the OS's
//!    before.** Opening any notification-area menu has always stolen the
//!    foreground for as long as the menu was up: the native `tray-icon`
//!    called `SetForegroundWindow` on its own hidden helper window before
//!    `TrackPopupMenu` (Microsoft's documented shell pattern, not
//!    opt-outable). The glass menu keeps exactly that behaviour and no more:
//!    a **right-click** on the icon shows the `traymenu` window and focuses
//!    it — deliberately, because losing that focus IS the dismissal (the page
//!    closes on blur, the native menu's own semantics). User-initiated, one
//!    call site, in [`Tray::place_and_show`]. Nothing restores focus when the
//!    menu closes — the OS hands it back the way it always has.
//! 2. **`show()` + `set_focus()` on "Open Sotone", a recent note, and
//!    Settings.** The user asked for the window by clicking a menu item that
//!    says so; `show()` alone does not restore the foreground on Windows.
//!    A **double left-click on the icon** also enters
//!    this same carve-out: [`icon_input`] answers it with the very
//!    [`MenuAction::Open`] the "Open Sotone" row sends, so it is the existing
//!    path asked for without the menu — not a third focus site.
//!    `grep -rn set_focus src-tauri/` must find exactly **two** call sites,
//!    both in this file — [`open_window`] and [`Tray::place_and_show`] — and
//!    prose everywhere else. No capture path, no overlay path and no event
//!    handler can reach either.
//!
//! # Invariant 1
//!
//! Nothing here generates input. A menu click is an observation like a key
//! press: it arrives, it is enqueued, it is answered.
//!
//! # The glass menu
//!
//! The drawn menu is glass, and so is the shown menu: a third window
//! (`traymenu`, `ui/tray.html`) wearing the design's own grammar. A native
//! `HMENU` shipped first, on the thesis that "a custom popup is a focus
//! minefield" — but the native look was rejected (the same decision that
//! killed the app's other native popups), and that thesis died with the insight
//! that the menu may simply TAKE focus like the native one already did:
//! blur is then the dismissal, no global hook, no click-catcher. The flow:
//! **right-click** on the icon → [`TrayInput::OpenMenu`] (anchor = the icon's
//! physical rect; a double left-click opens the window instead and a single
//! left click does nothing — [`icon_input`])
//! → the thread emits the serialized [`TrayItem`] list + an open request →
//! the page renders, measures, answers `tray_menu_ready` → the thread sizes
//! (logical), places (physical, above the icon — [`menu_position`]), shows
//! and focuses. The page closes itself on blur/Escape/action via
//! `tray_menu_closed`. What a native menu could not do and this one does:
//! the design's row grammar, the line counts in a right-hand column, and a
//! recording clock that ticks while the menu is open.

use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, State};

use crate::shell::{self, Condition, DraftsEvent, Phase, ShellState};

// ---------------------------------------------------------------------------
// What the tray is told
// ---------------------------------------------------------------------------

/// One thing the tray reacts to.
///
/// Every variant except [`TrayInput::Menu`] is a fact that already travels to
/// the windows as an event; none of them is a second opinion.
#[derive(Debug, Clone)]
pub enum TrayInput {
    /// `sotone://status` moved. The hint is the ready readout's binding summary
    /// — the same string the session bar shows — and is empty until there is
    /// one.
    Phase {
        /// Loading, ready, empty or fatal.
        phase: Phase,
        /// Every enabled binding, named.
        hint: String,
    },
    /// `sotone://armed`.
    Armed(bool),
    /// `sotone://recording`, the `live` half. The tray does not need the source:
    /// its hint names every enabled binding in both states.
    Recording(bool),
    /// `sotone://condition` — the top condition, or none.
    Condition(Option<Condition>),
    /// `sotone://drafts`, reduced to what a menu can show.
    Notes(TrayNotes),
    /// A menu item was clicked. Sent from the command handlers, handled on
    /// the tray thread.
    Menu(MenuAction),
    /// The icon was right-clicked (right-click only, by design): open the
    /// glass menu against this anchor. Sent from
    /// the tray-icon event closure, decided by [`icon_input`].
    OpenMenu(MenuAnchor),
    /// The menu page rendered and measured itself (`tray_menu_ready`):
    /// logical CSS pixels, ready to be sized, placed and shown.
    MenuMeasured {
        /// The menu's width — 260 by construction, but the page is the one
        /// that laid it out, so the page's number is the one used.
        width: f64,
        /// The menu's rendered height.
        height: f64,
    },
    /// The page closed itself (blur, Escape, or after an action): hide the
    /// window.
    MenuClosed,
}

/// Where the tray icon is, in physical pixels — the anchor the menu is placed
/// against. Comes from the click event ([`TrayIconEvent::Click`]'s `rect`),
/// which reports it in the monitor's own pixels, the same unit the work-area
/// arithmetic in [`menu_position`] runs in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuAnchor {
    /// The icon rect's top edge.
    pub top: i32,
    /// The icon rect's bottom edge.
    pub bottom: i32,
    /// The icon rect's right edge.
    pub right: i32,
}

/// One note in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayNote {
    /// Draft ulid — what [`ShellState::open_draft`] takes.
    pub id: String,
    /// What the note is called, by the same rule the sidebar uses: the file it
    /// is bound to, else when it was started.
    pub label: String,
    /// Live lines, for the count the design draws beside the name.
    pub lines: usize,
}

/// The drafts list, as a menu can show it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayNotes {
    /// The five newest, newest first.
    pub recent: Vec<TrayNote>,
    /// The one lines are landing in, if any.
    pub active: Option<TrayNote>,
}

/// What a click on a menu item means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// Flip the arm switch — the menu's only capture control (design: recording
    /// is never started or stopped with the mouse).
    Toggle,
    /// Show the window.
    Open,
    /// Show the window on the settings view.
    Settings,
    /// Activate this draft and show the window.
    Note(String),
    /// The one true exit.
    Quit,
}

/// Everything the tray renders, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayFacts {
    /// Whether there is a session at all.
    pub phase: Phase,
    /// The arm switch.
    pub armed: bool,
    /// Whether a recording is live right now.
    pub recording: bool,
    /// The condition that holds, if one does.
    pub condition: Option<Condition>,
    /// Every enabled binding, named.
    pub hint: String,
    /// The notes worth offering.
    pub notes: TrayNotes,
}

impl TrayFacts {
    /// What the tray shows before startup has said anything.
    ///
    /// `armed` matches `Inner::user_armed`, which starts true: the user
    /// launched a dictation app. It is corrected by the first `sotone://armed`
    /// either way.
    fn starting() -> Self {
        Self {
            phase: Phase::Loading,
            armed: true,
            recording: false,
            condition: None,
            hint: String::new(),
            notes: TrayNotes::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// The pure model
// ---------------------------------------------------------------------------

/// Which glyph is in the notification area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// Armed and idle: solid bars, `--dim`.
    Dim,
    /// Recording: solid bars, `--live`, animating.
    Live,
    /// Disabled, or a condition that stops capture: hollow bars, `--faint`.
    Hollow,
}

/// One row of the menu, before it becomes a native item.
///
/// The order of a [`Vec<TrayItem>`] **is** the design's fixed order: status and
/// the key hint · enable or disable · recents · open · settings · quit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayItem {
    /// The status line. Disabled — it is a readout, not an action — and the
    /// only row whose text changes without the menu being rebuilt.
    Status(String),
    /// The sentence under the status. Disabled.
    Hint(String),
    /// Enable or disable recording.
    Toggle {
        /// The action, not the state: "Disable recording" while armed.
        label: &'static str,
        /// False before there is a session to arm.
        enabled: bool,
    },
    /// A rule between blocks.
    Separator,
    /// A block label ("Recent notes"). Disabled.
    Section(&'static str),
    /// A note to open.
    Note {
        /// Draft ulid.
        id: String,
        /// The note's name (the sidebar's rule).
        label: String,
        /// Live lines — the right-hand mono column the design draws. A native
        /// item had no columns and folded this into the label; the glass
        /// menu has columns, so it is structural again.
        lines: usize,
    },
    /// Where the line being spoken is going. Disabled.
    Writing {
        /// The note's name.
        label: String,
        /// Its live lines, in the same column the recents use.
        lines: usize,
    },
    /// Show the window.
    Open,
    /// Show the window on the settings view.
    Settings,
    /// Leave, and say what that does.
    Quit(&'static str),
}

/// The whole tray, derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    /// The menu, in order.
    pub items: Vec<TrayItem>,
    /// The icon.
    pub glyph: Glyph,
    /// The hover text, which mirrors the status line.
    pub tooltip: String,
}

/// `mm:ss`, as the pill and the title bar spell it.
fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

/// The status line and the sentence under it.
///
/// The precedence is the point: **nothing can say "armed" while Sotone cannot
/// hear**, so the phase and the two capture-stopping conditions are answered
/// before the arm switch is even consulted.
///
/// [`Condition::FileConflict`] deliberately does not appear. It is a *save*
/// that was stopped, not a microphone that stopped; the mic is open, the keys
/// work, and the window's strip is where a file question is answered. Putting
/// it in the status slot would be the glyph's one forbidden move — saying
/// something about capture that is not about capture.
fn status(facts: &TrayFacts, elapsed: Duration) -> (String, Option<String>) {
    match facts.phase {
        Phase::Loading => return ("Sotone is starting…".to_owned(), None),
        Phase::Empty => {
            return (
                "No model yet — open Sotone".to_owned(),
                Some("nothing can be recorded until one is added".to_owned()),
            )
        }
        Phase::Fatal => {
            return (
                "Sotone could not start — open Sotone".to_owned(),
                Some("nothing is listening".to_owned()),
            )
        }
        Phase::Ready => {}
    }

    match facts.condition {
        Some(Condition::HotkeyDead) => {
            return (
                "Hotkeys are dead — open Sotone".to_owned(),
                Some("no key press reaches Sotone".to_owned()),
            )
        }
        Some(Condition::NoDevice) => {
            return (
                "No microphone — open Sotone".to_owned(),
                Some("nothing can be recorded until it is back".to_owned()),
            )
        }
        Some(Condition::FileConflict) | None => {}
    }

    if !facts.armed {
        return (
            "Recording disabled".to_owned(),
            Some("keys do nothing until enabled".to_owned()),
        );
    }

    let hint = (!facts.hint.is_empty()).then(|| facts.hint.clone());
    if facts.recording {
        return (format!("Recording {}", clock(elapsed)), hint);
    }
    ("Armed · not recording".to_owned(), hint)
}

/// Which glyph belongs to these facts.
///
/// Same rule as the status line, and derived from the same fields, so the icon
/// and the words cannot disagree: red only while a recording is live, hollow
/// whenever Sotone cannot record at all, dim otherwise.
fn glyph(facts: &TrayFacts) -> Glyph {
    if facts.phase != Phase::Ready {
        return Glyph::Hollow;
    }
    if matches!(
        facts.condition,
        Some(Condition::HotkeyDead | Condition::NoDevice)
    ) {
        return Glyph::Hollow;
    }
    if !facts.armed {
        return Glyph::Hollow;
    }
    if facts.recording {
        return Glyph::Live;
    }
    Glyph::Dim
}

/// The facts, rendered. The one function the glyph, the tooltip and the menu
/// all come out of.
///
/// `elapsed` is how long the live recording has been running *as of now*.
/// While a recording is live the tray thread ticks once a second anyway (the
/// icon animation), and each tick re-derives this view — so an open glass
/// menu shows a running clock, which the frozen native menu never could.
pub fn view(facts: &TrayFacts, elapsed: Duration) -> TrayView {
    let (status, hint) = status(facts, elapsed);
    let mut items = vec![TrayItem::Status(status.clone())];
    if let Some(hint) = hint {
        items.push(TrayItem::Hint(hint));
    }

    items.push(TrayItem::Toggle {
        label: if facts.armed {
            "Disable recording"
        } else {
            "Enable recording"
        },
        // Nothing to arm before there is a session: the empty phase has no
        // engine, no worker and no helper, so an enabled switch there would be
        // a control that does nothing.
        enabled: facts.phase == Phase::Ready,
    });

    // While a recording is live the recents give way to where the line being
    // spoken is going, as the design draws it. Nothing to show if no draft exists
    // yet — the draft is created when the first line needs somewhere to land.
    if facts.recording {
        if let Some(active) = &facts.notes.active {
            items.push(TrayItem::Separator);
            items.push(TrayItem::Section("Writing to"));
            items.push(TrayItem::Writing {
                label: active.label.clone(),
                lines: active.lines,
            });
        }
    } else if !facts.notes.recent.is_empty() {
        items.push(TrayItem::Separator);
        items.push(TrayItem::Section("Recent notes"));
        for note in &facts.notes.recent {
            items.push(TrayItem::Note {
                id: note.id.clone(),
                label: note.label.clone(),
                lines: note.lines,
            });
        }
    }

    items.push(TrayItem::Separator);
    items.push(TrayItem::Open);
    items.push(TrayItem::Settings);
    items.push(TrayItem::Separator);
    // Stated, never disabled: the menu says what leaving will do.
    items.push(TrayItem::Quit(if facts.recording {
        "stops recording"
    } else {
        "stops listening"
    }));

    TrayView {
        glyph: glyph(facts),
        tooltip: status,
        items,
    }
}

// (The `note_label` fold — name and count in one string — died with the
// native menu: the glass rows carry the count as their own mono column, the
// treatment the design drew all along.)

// ---------------------------------------------------------------------------
// The drafts list, reduced
// ---------------------------------------------------------------------------

/// How many notes the menu offers (the design's "five at most").
const RECENTS: usize = 5;

/// `sotone://drafts` → what the menu can show.
///
/// The drafts event lists oldest first, so "newest first, five at most" is the
/// tail, reversed. The active note is resolved separately because it is the one
/// the recording state shows, and it need not be among the five.
pub fn notes(drafts: &DraftsEvent) -> TrayNotes {
    let recent = drafts
        .drafts
        .iter()
        .rev()
        .take(RECENTS)
        .map(|draft| TrayNote {
            id: draft.id.clone(),
            label: draft_label(draft.saved_path.as_deref(), &draft.created_at),
            lines: draft.line_count,
        })
        .collect();

    let active = drafts.active_id.as_deref().and_then(|id| {
        drafts
            .drafts
            .iter()
            .find(|draft| draft.id == id)
            .map(|draft| TrayNote {
                id: draft.id.clone(),
                label: draft_label(draft.saved_path.as_deref(), &draft.created_at),
                lines: draft.line_count,
            })
    });

    TrayNotes { recent, active }
}

/// What a note is called, by the sidebar's rule (`noteLabel` in `ui/main.js`):
/// the file it is bound to, because that is the name the user gave it, and the
/// moment it was started before there is one.
fn draft_label(saved_path: Option<&str>, created_at: &str) -> String {
    if let Some(path) = saved_path {
        let base = path.rsplit(['\\', '/']).next().unwrap_or(path);
        return base.strip_suffix(".md").unwrap_or(base).to_owned();
    }
    when_label(created_at)
}

/// The months, for [`when_label`].
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// An RFC3339 timestamp as a short local label ("Aug 13, 14:31").
///
/// Sliced rather than parsed: the string was produced by the draft store from a
/// local-offset timestamp, so its own digits are already the local time, and a
/// second date library on the tray path would be a dependency for one label.
/// Anything that does not look like RFC3339 is shown as it came.
fn when_label(rfc3339: &str) -> String {
    let (Some(month), Some(day), Some(time)) =
        (rfc3339.get(5..7), rfc3339.get(8..10), rfc3339.get(11..16))
    else {
        return rfc3339.to_owned();
    };
    let Some(name) = month
        .parse::<usize>()
        .ok()
        .and_then(|month| MONTHS.get(month.wrapping_sub(1)))
    else {
        return rfc3339.to_owned();
    };
    let day = day.strip_prefix('0').unwrap_or(day);
    format!("{name} {day}, {time}")
}

// ---------------------------------------------------------------------------
// The glyph, painted
// ---------------------------------------------------------------------------

/// The icon's edge, in pixels.
///
/// Drawn at 32 and handed to Windows as raw RGBA (`CreateIcon` under
/// `tray_icon::Icon::from_rgba`) — no `.ico` file, no PNG decoder, no
/// `image-*` feature. The shell scales it to `SM_CXSMICON` (16 at 100 %, 20 at
/// 125 %, …), and 32 is the size every one of those divides into cleanly.
const ICON: u32 = 32;

/// Bar width, and the gap between bars: the design's 2 px on an 11 px glyph,
/// scaled ×2 into the 32 px cell.
const BAR_W: u32 = 4;
/// See [`BAR_W`].
const BAR_GAP: u32 = 4;
/// Where the first bar starts, so the four are centred: `32 - (4·4 + 3·4) = 4`,
/// halved.
const BAR_X: u32 = 2;

/// The logo's four bar heights (5/10/7/4), ×2.
const BARS: [u32; 4] = [10, 20, 14, 8];
/// The second recording frame. Two frames swapped at 1 Hz is the whole
/// animation: enough to read as alive from across the room, and a tray icon
/// that repaints faster is a tray icon that costs a game frames.
const BARS_LIVE: [u32; 4] = [16, 12, 20, 12];

/// `--dim` from the design tokens (dark).
const DIM: [u8; 4] = [0x8f, 0x8d, 0x85, 0xff];
/// `--live`. The only red in the app, and only while a recording is live.
const LIVE: [u8; 4] = [0xff, 0x5f, 0x56, 0xff];
/// `--faint`.
const FAINT: [u8; 4] = [0x75, 0x73, 0x69, 0xff];
/// `--sel` — the rounded square behind the bars while the menu is open.
const SEL: [u8; 4] = [0x39, 0x38, 0x35, 0xff];

/// One 32×32 RGBA frame of the glyph.
///
/// **Colours, not tinting.** The design asks for a template image the system
/// tints; Windows has no such thing for notification-area icons (that is a
/// macOS affordance), so the colours are ours to pick. The dark-taskbar values
/// above are the default because that is what Windows 11 ships and what the
/// most people run. On a light taskbar `--dim` and `--faint` are mid-greys on white:
/// legible, lower contrast than they are on dark. A per-taskbar-theme palette
/// would mean watching `SystemUsesLightTheme` for a repaint, which is a
/// registry watcher for a decoration — noted here rather than built.
pub fn paint(glyph: Glyph, frame: u8, open: bool) -> Vec<u8> {
    let mut pixels = vec![0u8; (ICON * ICON * 4) as usize];
    let heights = if glyph == Glyph::Live && frame % 2 == 1 {
        BARS_LIVE
    } else {
        BARS
    };
    let colour = match glyph {
        Glyph::Dim => DIM,
        Glyph::Live => LIVE,
        Glyph::Hollow => FAINT,
    };

    // The design's rule, "the icon takes a --sel rounded square while open":
    // the full cell, radius in the mock's 7-on-26 proportion, painted
    // before the bars so they sit on it.
    if open {
        const RADIUS: i32 = 9;
        let edge = ICON as i32;
        for y in 0..edge {
            for x in 0..edge {
                let dx = (x - RADIUS).min(0).max(x - (edge - 1 - RADIUS)).abs();
                let dy = (y - RADIUS).min(0).max(y - (edge - 1 - RADIUS)).abs();
                if dx * dx + dy * dy > RADIUS * RADIUS {
                    continue;
                }
                let at = ((y as u32 * ICON + x as u32) * 4) as usize;
                pixels[at..at + 4].copy_from_slice(&SEL);
            }
        }
    }

    for (bar, height) in heights.iter().enumerate() {
        let x0 = BAR_X + bar as u32 * (BAR_W + BAR_GAP);
        // Centred vertically, like the drawn glyph: the bars share a midline,
        // not a baseline.
        let y0 = (ICON - height) / 2;
        for y in y0..y0 + height {
            for x in x0..x0 + BAR_W {
                // A disabled bar is an outline. One pixel of it, not two: at
                // the 16 px the shell actually draws, a 2 px border leaves no
                // hole at all and the bar reads as solid.
                let interior = x > x0 && x + 1 < x0 + BAR_W && y > y0 && y + 1 < y0 + height;
                if glyph == Glyph::Hollow && interior {
                    continue;
                }
                let at = ((y * ICON + x) * 4) as usize;
                pixels[at..at + 4].copy_from_slice(&colour);
            }
        }
    }
    pixels
}

// ---------------------------------------------------------------------------
// The glass menu
// ---------------------------------------------------------------------------

/// The tray icon's id, so a future second icon cannot be confused with it.
const TRAY_ID: &str = "sotone";
/// The menu window's label, from `tauri.conf.json`.
const TRAYMENU_LABEL: &str = "traymenu";
/// The menu, serialized — emitted whenever the derived items change.
const EVENT_ITEMS: &str = "sotone://tray-items";
/// "The icon was clicked; render, measure, answer `tray_menu_ready`."
const EVENT_OPEN: &str = "sotone://tray-open";
/// The status line's id (a readout — [`action`] answers `None` for it).
const ID_STATUS: &str = "sotone:tray:status";
/// Enable / disable recording.
const ID_TOGGLE: &str = "sotone:tray:arm";
/// Show the window.
const ID_OPEN: &str = "sotone:tray:open";
/// Show the window on the settings view.
const ID_SETTINGS: &str = "sotone:tray:settings";
/// Leave.
const ID_QUIT: &str = "sotone:tray:quit";
/// One recent note; the draft ulid follows.
const ID_NOTE: &str = "sotone:tray:note:";

/// A clicked item id → what it means. `None` for anything with no action: the
/// status line, the hint, the section labels, the separators.
fn action(id: &str) -> Option<MenuAction> {
    match id {
        ID_TOGGLE => Some(MenuAction::Toggle),
        ID_OPEN => Some(MenuAction::Open),
        ID_SETTINGS => Some(MenuAction::Settings),
        ID_QUIT => Some(MenuAction::Quit),
        other => other
            .strip_prefix(ID_NOTE)
            .map(|draft| MenuAction::Note(draft.to_owned())),
    }
}

/// One row of the menu, as `ui/tray.js` receives it. A plain serialization of
/// [`TrayItem`] — the page renders it and invents nothing, so the drawn order
/// and content are still decided in exactly one place, [`view`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TrayRow {
    /// `status` | `hint` | `toggle` | `separator` | `section` | `note` |
    /// `writing` | `open` | `settings` | `quit`.
    kind: &'static str,
    /// The id a click sends back (`tray_menu_action`), on actionable rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// The row's text, where it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// The right-hand mono column: a note's live line count.
    #[serde(skip_serializing_if = "Option::is_none")]
    lines: Option<usize>,
    /// `false` on an offered-but-inert action (the empty phase's toggle).
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

/// The payload of [`EVENT_ITEMS`].
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TrayMenuPayload {
    /// `dim` | `live` | `hollow` — the page's status row shows the same truth
    /// the icon does.
    glyph: &'static str,
    /// The menu, in the drawn order.
    items: Vec<TrayRow>,
}

/// [`TrayView`] → what the page renders.
fn payload(view: &TrayView) -> TrayMenuPayload {
    let row = |kind| TrayRow {
        kind,
        id: None,
        label: None,
        lines: None,
        enabled: None,
    };
    let items = view
        .items
        .iter()
        .map(|item| match item {
            TrayItem::Status(text) => TrayRow {
                id: Some(ID_STATUS.to_owned()),
                label: Some(text.clone()),
                ..row("status")
            },
            TrayItem::Hint(text) => TrayRow {
                label: Some(text.clone()),
                ..row("hint")
            },
            TrayItem::Toggle { label, enabled } => TrayRow {
                id: Some(ID_TOGGLE.to_owned()),
                label: Some((*label).to_owned()),
                enabled: Some(*enabled),
                ..row("toggle")
            },
            TrayItem::Separator => row("separator"),
            TrayItem::Section(label) => TrayRow {
                label: Some((*label).to_owned()),
                ..row("section")
            },
            TrayItem::Note { id, label, lines } => TrayRow {
                id: Some(format!("{ID_NOTE}{id}")),
                label: Some(label.clone()),
                lines: Some(*lines),
                ..row("note")
            },
            TrayItem::Writing { label, lines } => TrayRow {
                label: Some(label.clone()),
                lines: Some(*lines),
                ..row("writing")
            },
            TrayItem::Open => TrayRow {
                id: Some(ID_OPEN.to_owned()),
                label: Some("Open Sotone".to_owned()),
                ..row("open")
            },
            TrayItem::Settings => TrayRow {
                id: Some(ID_SETTINGS.to_owned()),
                label: Some("Settings".to_owned()),
                ..row("settings")
            },
            TrayItem::Quit(note) => TrayRow {
                id: Some(ID_QUIT.to_owned()),
                label: Some(format!("Exit Sotone — {note}")),
                ..row("quit")
            },
        })
        .collect();

    TrayMenuPayload {
        glyph: match view.glyph {
            Glyph::Dim => "dim",
            Glyph::Live => "live",
            Glyph::Hollow => "hollow",
        },
        items,
    }
}

// ---------------------------------------------------------------------------
// Placing the menu
// ---------------------------------------------------------------------------

/// The gap between the icon and the menu, in logical pixels (scaled by the
/// caller): the design draws the flyout floating clear of the taskbar edge.
const MENU_GAP: i32 = 8;

/// A monitor's work area, in its own physical pixels. A local mirror of what
/// `Monitor::work_area()` answers — the overlay has its own; neither is shared
/// because each keeps the exact three fields its arithmetic needs.
#[derive(Debug, Clone, Copy, PartialEq)]
struct MenuArea {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// Where the menu window goes, in physical pixels.
///
/// The rule, in as many words (the native menu once
/// opened OVER the taskbar's overflow flyout): **above the icon**, right edge
/// aligned to the icon's right edge — so from a bottom taskbar, and from the
/// overflow flyout's row of icons, the menu rises clear of what was clicked.
/// A taskbar at the top of the screen leaves no room above; the menu drops
/// below the icon instead (the design's macOS/GNOME placement). Both results
/// are clamped into the work area, which is what keeps a menu taller than the
/// space it was given on screen at all.
fn menu_position(
    area: MenuArea,
    width: i32,
    height: i32,
    anchor: MenuAnchor,
    gap: i32,
) -> (i32, i32) {
    let x = (anchor.right - width).clamp(area.left, (area.right - width).max(area.left));

    let above = anchor.top - gap - height;
    let y = if above >= area.top {
        above
    } else {
        (anchor.bottom + gap).min((area.bottom - height).max(area.top))
    };
    (x, y)
}

// ---------------------------------------------------------------------------
// The icon's click grammar
// ---------------------------------------------------------------------------

/// One tray-icon event → what the tray thread is told about it, if anything.
///
/// The grammar is deliberate: it revised an earlier
/// either-button-opens-the-menu rule, under which a double left-click was
/// opening the menu twice:
///
/// | event                              | result                     |
/// |------------------------------------|----------------------------|
/// | right button, `Up`                 | [`TrayInput::OpenMenu`]     |
/// | left button, double-click          | `Menu(`[`MenuAction::Open`]`)` |
/// | single left click, any `Down`, middle, hover | nothing        |
///
/// **Why this needs no timer.** Windows delivers a double left-click as
/// DOWN, UP, DBLCLK, UP — so `tray-icon` forwards two ordinary left
/// `Click`/`Up` events *around* the `DoubleClick`, and there is no way to
/// know at the first one whether a second is coming. That is exactly why the
/// rule is implementable as a pure match: a single left click is defined to
/// do nothing, so the two stray `Click`s map to `None` and only the
/// `DoubleClick` speaks. No double-click interval is ever waited out, and
/// nothing here holds state between events.
///
/// The double-click answers with the same [`MenuAction::Open`] the menu's
/// "Open Sotone" row sends, so it reaches the window through [`act`] →
/// [`open_window`]: the existing invariant-2 carve-out entered without the
/// menu, not a new focus site.
///
/// Pure on purpose — the closure that calls it runs on the tao event-loop
/// thread (invariant 5), and a truth table is not something a taskbar can be
/// asked about in a unit test.
fn icon_input(event: &TrayIconEvent) -> Option<TrayInput> {
    match event {
        TrayIconEvent::Click {
            rect,
            button: MouseButton::Right,
            button_state: MouseButtonState::Up,
            ..
        } => {
            // tray-icon reports the icon's rect in physical pixels; the enum
            // wrapper is unwrapped here so the thread deals in one unit.
            // (`to_physical` with scale 1.0 is the identity on the
            // already-physical variant.)
            let position = rect.position.to_physical::<i32>(1.0);
            let size = rect.size.to_physical::<i32>(1.0);
            Some(TrayInput::OpenMenu(MenuAnchor {
                top: position.y,
                bottom: position.y + size.height,
                right: position.x + size.width,
            }))
        }
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => Some(TrayInput::Menu(MenuAction::Open)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The tray thread
// ---------------------------------------------------------------------------

/// How often the recording glyph changes frame, and the status line its clock.
const TICK: Duration = Duration::from_secs(1);

/// Build the tray icon and start the thread that owns it.
///
/// Returns whether there is a tray at all. That answer matters beyond
/// diagnostics: [`crate::shell::on_close`] only hides the window if this said
/// yes, because hiding to a tray that does not exist would leave the user with
/// a running app and no way back to it.
///
/// Called from `setup`, on the main thread, where every one of these
/// `run_on_main_thread` round trips is taken inline (`send_user_message`'s
/// same-thread fast path) — so the icon exists before the event loop runs, and
/// the empty phase has a tray exactly like a ready one.
pub fn spawn(app: AppHandle, state: ShellState, inputs: Receiver<TrayInput>) -> bool {
    let facts = TrayFacts::starting();
    let first = view(&facts, Duration::ZERO);

    let listener = state.clone();
    let glyph = first.glyph;
    let icon = TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::new_owned(paint(glyph, 0, false), ICON, ICON))
        .tooltip(&first.tooltip)
        // No `.menu()` at all: a right-click is answered with the glass
        // window. Which button means what is [`icon_input`]'s whole job —
        // right-click opens the menu, a double left-click opens Sotone, a
        // single left click does nothing (deliberate, revising an earlier
        // either-button rule).
        .on_tray_icon_event(move |_tray, event| {
            // **The tao event-loop thread.** Map, send, return — the mapping
            // is a pure function precisely so nothing else is tempted in
            // here. A window call would block the loop (invariant 5, the
            // rdev callback's rule in a different costume).
            if let Some(input) = icon_input(&event) {
                listener.tray(input);
            }
        })
        .build(&app);

    let icon = match icon {
        Ok(icon) => icon,
        Err(err) => {
            tracing::warn!(error = %err, "running without a tray icon");
            return false;
        }
    };

    let spawned = thread::Builder::new()
        .name("sotone-tray".to_owned())
        .spawn(move || {
            run(
                Tray {
                    app,
                    icon,
                    painted: (glyph, 0, false),
                    shown: first,
                    menu_open: false,
                    anchor: None,
                    last_closed: None,
                },
                state,
                facts,
                &inputs,
            );
        });

    if let Err(err) = spawned {
        // The icon is up but nothing will ever update it, which is worse than
        // no tray at all: it would sit there claiming to be starting up
        // forever, and the window could hide behind it.
        tracing::warn!(error = %err, "running without a tray icon");
        return false;
    }
    true
}

/// The icon, the menu window's state, and what is currently showing.
struct Tray {
    app: AppHandle,
    icon: TrayIcon,
    /// What is on screen, so nothing is re-applied for nothing.
    shown: TrayView,
    /// The glyph, frame and open-highlight currently painted.
    painted: (Glyph, u8, bool),
    /// Whether the glass menu is up (drives the icon's `--sel` highlight).
    menu_open: bool,
    /// The anchor of the click being answered. Kept while the menu is open so
    /// a re-measure (the recents changed under an open menu) re-places
    /// against the same icon.
    anchor: Option<MenuAnchor>,
    /// When the menu last closed. A click on the icon lands `blur` first —
    /// the page closes — and then this thread would reopen what the user was
    /// shutting; a click this soon after a close is that toggle, answered by
    /// staying closed.
    last_closed: Option<Instant>,
}

/// The toggle-shut window: an icon click within this of a close is the close's
/// own cause.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

/// The tray thread: facts in, menu and icon out.
///
/// It sleeps on the channel and wakes for one of two things: a fact changed, or
/// — while a recording is live — a second passed. Idle and disabled are static,
/// so a Sotone that is merely armed costs nothing at all.
fn run(mut tray: Tray, state: ShellState, mut facts: TrayFacts, inputs: &Receiver<TrayInput>) {
    let mut since: Option<Instant> = None;
    let mut frame = 0u8;

    loop {
        let received = if since.is_some() {
            match inputs.recv_timeout(TICK) {
                Ok(input) => Some(input),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match inputs.recv() {
                Ok(input) => Some(input),
                Err(_) => break,
            }
        };

        match received {
            // The tick: the next animation frame, and a fresh clock for
            // whoever opens the menu in the next second.
            None => frame ^= 1,
            Some(TrayInput::Phase { phase, hint }) => {
                facts.phase = phase;
                facts.hint = hint;
            }
            Some(TrayInput::Armed(armed)) => facts.armed = armed,
            Some(TrayInput::Recording(live)) => {
                facts.recording = live;
                // "The bars start and stop with the recording, never
                // mid-stride" — the design's motion rule.
                if live != since.is_some() {
                    since = live.then(Instant::now);
                    frame = 0;
                }
            }
            Some(TrayInput::Condition(condition)) => facts.condition = condition,
            Some(TrayInput::Notes(notes)) => facts.notes = notes,
            Some(TrayInput::Menu(action)) => {
                if !act(&tray.app, &state, &facts, action) {
                    break;
                }
            }
            Some(TrayInput::OpenMenu(anchor)) => tray.open_menu(anchor),
            Some(TrayInput::MenuMeasured { width, height }) => tray.place_and_show(width, height),
            Some(TrayInput::MenuClosed) => tray.close_menu(),
        }

        let elapsed = since.map(|started| started.elapsed()).unwrap_or_default();
        let next = view(&facts, elapsed);
        tray.apply(next, frame);
    }
    tracing::debug!("the tray thread has stopped");
}

impl Tray {
    /// Put a view on screen, touching only what changed.
    ///
    /// A changed menu is one emitted payload — the page re-renders wholesale,
    /// which is cheap where a native rebuild was not. The recording clock
    /// still only costs anything while a recording is live, because that is
    /// the only time the thread ticks.
    fn apply(&mut self, next: TrayView, frame: u8) {
        if self.shown.items != next.items || self.shown.glyph != next.glyph {
            self.emit_items(&next);
        }

        if self.painted != (next.glyph, frame, self.menu_open) {
            let image = Image::new_owned(paint(next.glyph, frame, self.menu_open), ICON, ICON);
            match self.icon.set_icon(Some(image)) {
                Ok(()) => self.painted = (next.glyph, frame, self.menu_open),
                Err(err) => tracing::warn!(error = %err, "could not repaint the tray icon"),
            }
        }

        if self.shown.tooltip != next.tooltip {
            if let Err(err) = self.icon.set_tooltip(Some(&next.tooltip)) {
                tracing::warn!(error = %err, "could not set the tray tooltip");
            }
        }

        self.shown = next;
    }

    /// Send the current menu to the page (hidden or not — the page always
    /// renders the latest payload it was given).
    fn emit_items(&self, view: &TrayView) {
        if let Err(err) = self.app.emit_to(TRAYMENU_LABEL, EVENT_ITEMS, payload(view)) {
            tracing::warn!(error = %err, "could not send the tray menu its items");
        }
    }

    /// The icon was clicked: start the open handshake, unless this click is
    /// the tail end of the user toggling the menu shut.
    fn open_menu(&mut self, anchor: MenuAnchor) {
        if let Some(closed) = self.last_closed {
            if closed.elapsed() < REOPEN_GUARD {
                return;
            }
        }
        self.anchor = Some(anchor);
        // Fresh items first, so the open can never show a stale menu, then
        // the request the page answers with its measurements.
        let shown = self.shown.clone();
        self.emit_items(&shown);
        if let Err(err) = self.app.emit_to(TRAYMENU_LABEL, EVENT_OPEN, ()) {
            tracing::warn!(error = %err, "could not ask the tray menu to open");
        }
    }

    /// The page measured itself: size, place, show, focus.
    ///
    /// Size is asked for in logical units and position in physical, on the
    /// primary monitor's scale — the overlay's split exactly, and for the
    /// overlay's reasons (one unit per number, and the work area is
    /// physical). **The second of the two focus carve-outs' call sites** (the
    /// module header has the story): the menu takes focus so that losing it
    /// can dismiss it.
    fn place_and_show(&mut self, width: f64, height: f64) {
        let Some(anchor) = self.anchor else {
            // A stray measurement — the page re-measured after a close won.
            return;
        };
        let Some(window) = self.app.get_webview_window(TRAYMENU_LABEL) else {
            tracing::warn!("this build has no tray menu window");
            return;
        };

        let monitor = match window.primary_monitor() {
            Ok(monitor) => monitor,
            Err(err) => {
                tracing::warn!(error = %err, "could not read the primary monitor");
                None
            }
        };
        let scale = monitor
            .as_ref()
            .map(tauri::Monitor::scale_factor)
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .unwrap_or(1.0);

        if let Err(err) = window.set_size(LogicalSize::new(width, height)) {
            tracing::warn!(error = %err, "could not size the tray menu");
            return;
        }

        if let Some(monitor) = monitor.as_ref() {
            let area = monitor.work_area();
            let area = MenuArea {
                left: area.position.x,
                top: area.position.y,
                right: area.position.x + area.size.width as i32,
                bottom: area.position.y + area.size.height as i32,
            };
            let physical = |logical: f64| (logical * scale).round() as i32;
            let (x, y) = menu_position(
                area,
                physical(width),
                physical(height),
                anchor,
                physical(f64::from(MENU_GAP)),
            );
            if let Err(err) = window.set_position(PhysicalPosition::new(x, y)) {
                tracing::warn!(error = %err, "could not place the tray menu");
                return;
            }
        } else {
            // Headless RDP again: a menu in the wrong place beats no menu.
            tracing::debug!("no primary monitor; leaving the tray menu where it is");
        }

        if let Err(err) = window.show() {
            tracing::warn!(error = %err, "could not show the tray menu");
            return;
        }
        if let Err(err) = window.set_focus() {
            tracing::warn!(error = %err, "could not focus the tray menu");
        }
        self.menu_open = true;
    }

    /// The page closed itself (blur, Escape, or after an action): hide.
    fn close_menu(&mut self) {
        self.menu_open = false;
        self.anchor = None;
        self.last_closed = Some(Instant::now());
        if let Some(window) = self.app.get_webview_window(TRAYMENU_LABEL) {
            if let Err(err) = window.hide() {
                tracing::warn!(error = %err, "could not hide the tray menu");
            }
        }
    }
}

/// Carry out one [`MenuAction`] — a menu click, or the icon's double
/// left-click, which is [`MenuAction::Open`] asked for without the menu.
/// Returns whether the tray thread should keep going.
///
/// All of it on the tray thread, never in the event handler: two of these
/// forward to the control thread, three of them call into a window, and one
/// ends the process.
fn act(app: &AppHandle, state: &ShellState, facts: &TrayFacts, action: MenuAction) -> bool {
    match action {
        // The arm switch, through the same path the window's control takes —
        // there is one place that decides what arming means, and this is not a
        // second copy of it.
        MenuAction::Toggle => {
            shell::apply_armed(app, state, !facts.armed);
        }
        MenuAction::Open => open_window(app, None),
        MenuAction::Settings => open_window(app, Some(shell::VIEW_SETTINGS)),
        MenuAction::Note(id) => {
            // The existing command path: the worker owns the draft handle, so
            // activating one is a message to the control thread, exactly as
            // clicking the note in the sidebar is.
            state.open_draft(id);
            open_window(app, None);
        }
        MenuAction::Quit => {
            shell::quit(app);
            return false;
        }
    }
    true
}

/// Show the main window because the user asked for it.
///
/// **The invariant-2 carve-out** (one of the two in this file; the other is
/// the menu window's own focus in [`Tray::place_and_show`]). The user clicked
/// an item that says "Open Sotone", or double left-clicked the icon, which
/// means the same thing ([`icon_input`]);
/// `show()` alone leaves the window behind whatever they were looking at on
/// Windows, which would make both look broken. Nothing on the capture path
/// and no overlay path can reach this function — it has exactly one caller,
/// [`act`], on the tray thread, and that caller runs only for a
/// [`MenuAction`] the user asked for by hand.
fn open_window(app: &AppHandle, view: Option<&'static str>) {
    let Some(window) = app.get_webview_window(shell::MAIN_LABEL) else {
        return;
    };
    if let Err(err) = window.show() {
        tracing::warn!(error = %err, "could not show the window");
        return;
    }
    if let Err(err) = window.set_focus() {
        tracing::warn!(error = %err, "could not bring the window forward");
    }
    if let Some(view) = view {
        shell::request_view(app, view);
    }
}

// ---------------------------------------------------------------------------
// The menu window's commands
// ---------------------------------------------------------------------------
//
// Three, all send-only into the tray channel, so every decision still happens
// on the tray thread — the page is a renderer with a wire, not a second
// implementation of the menu. Registered in main.rs.

/// A row was clicked. The id vocabulary is this file's own ([`action`]); an
/// id that means nothing (a forged or stale row) is dropped with a warning,
/// never guessed at.
#[tauri::command]
pub fn tray_menu_action(id: String, state: State<'_, ShellState>) {
    match action(&id) {
        Some(action) => state.tray(TrayInput::Menu(action)),
        None => tracing::warn!(id, "a tray menu click carried an unknown id"),
    }
}

/// The page rendered and measured itself: logical CSS pixels.
#[tauri::command]
pub fn tray_menu_ready(width: f64, height: f64, state: State<'_, ShellState>) {
    state.tray(TrayInput::MenuMeasured { width, height });
}

/// The page closed itself (blur, Escape, or after an action).
#[tauri::command]
pub fn tray_menu_closed(state: State<'_, ShellState>) {
    state.tray(TrayInput::MenuClosed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::DraftDto;

    fn ready() -> TrayFacts {
        TrayFacts {
            phase: Phase::Ready,
            armed: true,
            recording: false,
            condition: None,
            hint: "F13 hold · F8 toggle".to_owned(),
            notes: TrayNotes::default(),
        }
    }

    fn note(id: &str, label: &str, lines: usize) -> TrayNote {
        TrayNote {
            id: id.to_owned(),
            label: label.to_owned(),
            lines,
        }
    }

    fn draft(id: &str, created_at: &str, saved: Option<&str>, lines: usize) -> DraftDto {
        DraftDto {
            id: id.to_owned(),
            created_at: created_at.to_owned(),
            project: None,
            dirty: false,
            line_count: lines,
            saved_path: saved.map(ToOwned::to_owned),
        }
    }

    /// Every row as one string. Note and Writing rows fold their line count
    /// back in, so the expectations below keep asserting that the count
    /// travels — in the rendered menu it is a right-hand column.
    fn labels(view: &TrayView) -> Vec<String> {
        view.items
            .iter()
            .map(|item| match item {
                TrayItem::Status(text) | TrayItem::Hint(text) => text.clone(),
                TrayItem::Writing { label, lines } | TrayItem::Note { label, lines, .. } => {
                    match lines {
                        1 => format!("{label} · 1 line"),
                        n => format!("{label} · {n} lines"),
                    }
                }
                TrayItem::Toggle { label, .. } | TrayItem::Section(label) => (*label).to_owned(),
                TrayItem::Separator => "—".to_owned(),
                TrayItem::Open => "Open Sotone".to_owned(),
                TrayItem::Settings => "Settings".to_owned(),
                TrayItem::Quit(note) => format!("Exit Sotone — {note}"),
            })
            .collect()
    }

    // -- the reducer ------------------------------------------------------

    #[test]
    fn armed_and_idle_is_the_drawn_order() {
        let mut facts = ready();
        facts.notes.recent = vec![note("a", "session 04", 12), note("b", "table pass", 1)];

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            labels(&view),
            vec![
                "Armed · not recording",
                "F13 hold · F8 toggle",
                "Disable recording",
                "—",
                "Recent notes",
                "session 04 · 12 lines",
                "table pass · 1 line",
                "—",
                "Open Sotone",
                "Settings",
                "—",
                "Exit Sotone — stops listening",
            ]
        );
        assert_eq!(view.glyph, Glyph::Dim);
        assert_eq!(view.tooltip, "Armed · not recording");
    }

    #[test]
    fn recording_shows_the_clock_and_where_the_line_is_going() {
        let mut facts = ready();
        facts.recording = true;
        facts.notes.recent = vec![note("a", "session 04", 12)];
        facts.notes.active = Some(note("a", "session 04", 14));

        let view = view(&facts, Duration::from_secs(41));

        assert_eq!(
            labels(&view),
            vec![
                "Recording 00:41",
                "F13 hold · F8 toggle",
                "Disable recording",
                "—",
                "Writing to",
                "session 04 · 14 lines",
                "—",
                "Open Sotone",
                "Settings",
                "—",
                "Exit Sotone — stops recording",
            ]
        );
        assert_eq!(view.glyph, Glyph::Live);
    }

    #[test]
    fn a_recording_with_no_draft_yet_shows_no_note_block() {
        let mut facts = ready();
        facts.recording = true;

        let view = view(&facts, Duration::from_secs(3));

        assert!(!labels(&view).iter().any(|label| label == "Writing to"));
        assert_eq!(
            view.items[0],
            TrayItem::Status("Recording 00:03".to_owned())
        );
    }

    #[test]
    fn disabled_says_so_and_goes_hollow() {
        let mut facts = ready();
        facts.armed = false;
        facts.notes.recent = vec![note("a", "session 04", 12)];

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("Recording disabled".to_owned())
        );
        assert_eq!(
            view.items[1],
            TrayItem::Hint("keys do nothing until enabled".to_owned())
        );
        assert_eq!(
            view.items[2],
            TrayItem::Toggle {
                label: "Enable recording",
                enabled: true
            }
        );
        assert_eq!(view.glyph, Glyph::Hollow);
        // Recents survive being disabled: opening a note is not recording.
        assert!(labels(&view).iter().any(|label| label == "Recent notes"));
    }

    #[test]
    fn a_dead_hotkey_never_reads_as_armed() {
        let mut facts = ready();
        facts.condition = Some(Condition::HotkeyDead);

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("Hotkeys are dead — open Sotone".to_owned())
        );
        assert_eq!(view.glyph, Glyph::Hollow);
        assert_eq!(view.tooltip, "Hotkeys are dead — open Sotone");
    }

    #[test]
    fn a_lost_microphone_never_reads_as_armed() {
        let mut facts = ready();
        facts.condition = Some(Condition::NoDevice);
        // The engine dying disarms Sotone, but the tray must not need that to
        // have happened yet to tell the truth.
        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("No microphone — open Sotone".to_owned())
        );
        assert_eq!(view.glyph, Glyph::Hollow);
    }

    #[test]
    fn a_file_conflict_is_not_a_capture_condition() {
        let mut facts = ready();
        facts.condition = Some(Condition::FileConflict);

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("Armed · not recording".to_owned())
        );
        assert_eq!(view.glyph, Glyph::Dim);
    }

    #[test]
    fn the_empty_phase_has_a_tray_and_nothing_to_arm() {
        let mut facts = ready();
        facts.phase = Phase::Empty;

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("No model yet — open Sotone".to_owned())
        );
        assert_eq!(
            view.items[2],
            TrayItem::Toggle {
                label: "Disable recording",
                enabled: false
            }
        );
        assert_eq!(view.glyph, Glyph::Hollow);
    }

    #[test]
    fn startup_and_fatal_are_never_armed() {
        for phase in [Phase::Loading, Phase::Fatal] {
            let mut facts = ready();
            facts.phase = phase;
            let view = view(&facts, Duration::ZERO);
            assert_eq!(view.glyph, Glyph::Hollow, "{phase:?}");
            assert_ne!(
                view.items[0],
                TrayItem::Status("Armed · not recording".to_owned()),
                "{phase:?}"
            );
        }
    }

    #[test]
    fn no_bindings_means_no_hint_row() {
        let mut facts = ready();
        facts.hint = String::new();

        let view = view(&facts, Duration::ZERO);

        assert_eq!(
            view.items[0],
            TrayItem::Status("Armed · not recording".to_owned())
        );
        assert_eq!(
            view.items[1],
            TrayItem::Toggle {
                label: "Disable recording",
                enabled: true
            }
        );
    }

    #[test]
    fn the_clock_is_minutes_and_seconds() {
        assert_eq!(clock(Duration::ZERO), "00:00");
        assert_eq!(clock(Duration::from_secs(9)), "00:09");
        assert_eq!(clock(Duration::from_secs(119)), "01:59");
        assert_eq!(clock(Duration::from_secs(600)), "10:00");
    }

    // -- shape ------------------------------------------------------------

    #[test]
    fn a_ticking_clock_changes_the_items_so_an_open_menu_updates() {
        // The native menu froze while open; the glass one re-renders on every
        // emitted change, and the 1 Hz tick during a recording must BE a
        // change or the on-screen clock stops.
        let mut facts = ready();
        facts.recording = true;
        let before = view(&facts, Duration::from_secs(1));
        let after = view(&facts, Duration::from_secs(2));

        assert_ne!(before.items, after.items);
    }

    #[test]
    fn the_payload_is_the_items_verbatim_with_the_files_own_ids() {
        let mut facts = ready();
        facts.notes.recent = vec![note("01J0", "session 04", 12)];
        let sent = payload(&view(&facts, Duration::ZERO));

        assert_eq!(sent.glyph, "dim");
        let kinds: Vec<&str> = sent.items.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                "status",
                "hint",
                "toggle",
                "separator",
                "section",
                "note",
                "separator",
                "open",
                "settings",
                "separator",
                "quit",
            ]
        );

        let row = sent.items.iter().find(|row| row.kind == "note").unwrap();
        // The id round-trips through `action` — the page echoes it verbatim.
        assert_eq!(row.id.as_deref(), Some("sotone:tray:note:01J0"));
        assert_eq!(
            action(row.id.as_deref().unwrap()),
            Some(MenuAction::Note("01J0".to_owned()))
        );
        // The count is structural — the right-hand column the design draws.
        assert_eq!(row.lines, Some(12));
        assert_eq!(row.label.as_deref(), Some("session 04"));

        let toggle = sent.items.iter().find(|row| row.kind == "toggle").unwrap();
        assert_eq!(toggle.enabled, Some(true));
        // (A JSON round-trip would pin the serde derive too; that needs
        // `serde_json` as a dev-dependency this crate deliberately does not
        // have — the same call the phase test records.)
    }

    // ------------------------------------------------------------------
    // The icon's click grammar
    // ------------------------------------------------------------------

    /// The icon's rect as the shell reports it: 32px square, near the
    /// bottom-right corner of a 2560×1440 screen with a 48px taskbar.
    fn icon_rect() -> tauri::Rect {
        tauri::Rect {
            position: tauri::Position::Physical(PhysicalPosition::new(2448, 1398)),
            size: tauri::Size::Physical(tauri::PhysicalSize::new(32, 32)),
        }
    }

    fn click(button: MouseButton, button_state: MouseButtonState) -> TrayIconEvent {
        TrayIconEvent::Click {
            id: tauri::tray::TrayIconId::new(TRAY_ID),
            position: PhysicalPosition::default(),
            rect: icon_rect(),
            button,
            button_state,
        }
    }

    fn double_click(button: MouseButton) -> TrayIconEvent {
        TrayIconEvent::DoubleClick {
            id: tauri::tray::TrayIconId::new(TRAY_ID),
            position: PhysicalPosition::default(),
            rect: icon_rect(),
            button,
        }
    }

    #[test]
    fn right_click_opens_the_menu_double_left_click_opens_sotone_nothing_else_speaks() {
        // Right button, on the release: the glass menu, anchored on the
        // icon's own physical rect.
        match icon_input(&click(MouseButton::Right, MouseButtonState::Up)) {
            Some(TrayInput::OpenMenu(anchor)) => assert_eq!(
                anchor,
                MenuAnchor {
                    top: 1398,
                    bottom: 1430,
                    right: 2480,
                }
            ),
            other => panic!("right-click Up must open the menu, got {other:?}"),
        }

        // A double left-click is the menu's own "Open Sotone" said without the
        // menu — the same action id, so the same window path and the same
        // focus carve-out.
        assert!(matches!(
            icon_input(&double_click(MouseButton::Left)),
            Some(TrayInput::Menu(MenuAction::Open))
        ));

        // Everything else is silence: a single left click above all, every
        // press half, the middle button, and a
        // double-click with a button that has no double-click meaning.
        for event in [
            click(MouseButton::Left, MouseButtonState::Up),
            click(MouseButton::Left, MouseButtonState::Down),
            click(MouseButton::Right, MouseButtonState::Down),
            click(MouseButton::Middle, MouseButtonState::Up),
            click(MouseButton::Middle, MouseButtonState::Down),
            double_click(MouseButton::Right),
            double_click(MouseButton::Middle),
        ] {
            assert!(
                icon_input(&event).is_none(),
                "{event:?} must do nothing at all"
            );
        }
    }

    #[test]
    fn the_windows_double_click_sequence_asks_once_and_never_opens_the_menu() {
        // What the OS actually delivers for one double left-click:
        // DOWN, UP, DBLCLK, UP. The stray singles are why the rule needs no
        // timer — they map to nothing.
        let delivered = [
            click(MouseButton::Left, MouseButtonState::Down),
            click(MouseButton::Left, MouseButtonState::Up),
            double_click(MouseButton::Left),
            click(MouseButton::Left, MouseButtonState::Up),
        ];

        let inputs: Vec<_> = delivered.iter().filter_map(icon_input).collect();

        assert_eq!(inputs.len(), 1, "one double-click, one request: {inputs:?}");
        assert!(matches!(inputs[0], TrayInput::Menu(MenuAction::Open)));
    }

    // ------------------------------------------------------------------
    // Placing the menu
    // ------------------------------------------------------------------

    /// A 2560×1440 monitor at the origin with a 48px bottom taskbar.
    fn desk() -> MenuArea {
        MenuArea {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1392,
        }
    }

    #[test]
    fn the_menu_rises_above_a_bottom_tray_icon_right_aligned() {
        // An icon in the notification area: 32px square near the corner.
        let anchor = MenuAnchor {
            top: 1398,
            bottom: 1430,
            right: 2480,
        };
        let (x, y) = menu_position(desk(), 325, 300, anchor, 10);
        assert_eq!(x, 2480 - 325);
        // Bottom edge 10px above the icon's top — which also clears the
        // overflow flyout row the icon sits in, which is the point.
        assert_eq!(y, 1398 - 10 - 300);
    }

    #[test]
    fn an_overflow_flyout_icon_anchors_the_menu_above_the_flyout_row() {
        // The overflow flyout floats above the taskbar, so its icons' rects
        // sit higher; the rule is the same and the menu clears the row.
        let anchor = MenuAnchor {
            top: 1280,
            bottom: 1312,
            right: 2400,
        };
        let (_, y) = menu_position(desk(), 325, 300, anchor, 10);
        assert_eq!(y, 1280 - 10 - 300);
    }

    #[test]
    fn a_top_taskbar_drops_the_menu_below_the_icon() {
        let anchor = MenuAnchor {
            top: 8,
            bottom: 40,
            right: 2480,
        };
        let area = MenuArea {
            left: 0,
            top: 48,
            right: 2560,
            bottom: 1440,
        };
        let (_, y) = menu_position(area, 325, 300, anchor, 10);
        assert_eq!(y, 40 + 10);
    }

    #[test]
    fn the_menu_never_leaves_the_work_area() {
        // An icon at the far left with a menu wider than the space to its
        // left, and a menu taller than the room above it on a short screen.
        let anchor = MenuAnchor {
            top: 700,
            bottom: 732,
            right: 100,
        };
        let area = MenuArea {
            left: 0,
            top: 0,
            right: 800,
            bottom: 736,
        };
        let (x, y) = menu_position(area, 325, 720, anchor, 10);
        assert_eq!(x, 0);
        // No room above (700 - 10 - 720 < 0): below, clamped to the bottom.
        assert_eq!(y, 736 - 720);
    }

    // -- recents ----------------------------------------------------------

    #[test]
    fn recents_are_the_five_newest_first() {
        let drafts = DraftsEvent {
            drafts: (0..7)
                .map(|n| draft(&format!("d{n}"), "2026-08-13T14:31:02+02:00", None, n))
                .collect(),
            active_id: Some("d2".to_owned()),
            ..DraftsEvent::default()
        };

        let notes = notes(&drafts);

        assert_eq!(
            notes
                .recent
                .iter()
                .map(|note| note.id.as_str())
                .collect::<Vec<_>>(),
            vec!["d6", "d5", "d4", "d3", "d2"]
        );
        assert_eq!(notes.active.map(|note| note.id), Some("d2".to_owned()));
    }

    #[test]
    fn an_active_note_outside_the_five_is_still_resolved() {
        let drafts = DraftsEvent {
            drafts: (0..7)
                .map(|n| draft(&format!("d{n}"), "2026-08-13T14:31:02+02:00", None, n))
                .collect(),
            active_id: Some("d0".to_owned()),
            ..DraftsEvent::default()
        };

        let notes = notes(&drafts);

        assert!(!notes.recent.iter().any(|note| note.id == "d0"));
        assert_eq!(notes.active.map(|note| note.id), Some("d0".to_owned()));
    }

    #[test]
    fn a_bound_note_is_named_by_its_file() {
        assert_eq!(
            draft_label(
                Some(r"C:\Users\me\notes\perf pass — filters.md"),
                "2026-08-13T14:31:02+02:00"
            ),
            "perf pass — filters"
        );
        assert_eq!(
            draft_label(Some("/home/me/notes/session 04.md"), ""),
            "session 04"
        );
    }

    #[test]
    fn an_unbound_note_is_named_by_when_it_started() {
        assert_eq!(
            draft_label(None, "2026-08-13T14:31:02.123+02:00"),
            "Aug 13, 14:31"
        );
        assert_eq!(draft_label(None, "2026-01-05T09:07:00Z"), "Jan 5, 09:07");
        // Anything that is not a timestamp is shown as it came, never guessed
        // at.
        assert_eq!(draft_label(None, "not a date"), "not a date");
        assert_eq!(
            draft_label(None, "2026-99-13T14:31:02Z"),
            "2026-99-13T14:31:02Z"
        );
    }

    // -- ids --------------------------------------------------------------

    #[test]
    fn menu_ids_round_trip_to_actions() {
        assert_eq!(action(ID_TOGGLE), Some(MenuAction::Toggle));
        assert_eq!(action(ID_OPEN), Some(MenuAction::Open));
        assert_eq!(action(ID_SETTINGS), Some(MenuAction::Settings));
        assert_eq!(action(ID_QUIT), Some(MenuAction::Quit));
        assert_eq!(
            action("sotone:tray:note:01J0"),
            Some(MenuAction::Note("01J0".to_owned()))
        );
        assert_eq!(action(ID_STATUS), None);
        assert_eq!(action("anything else"), None);
    }

    // -- the glyph --------------------------------------------------------

    /// Every lit pixel's column, and the rows lit in one column.
    fn lit_columns(pixels: &[u8]) -> Vec<u32> {
        let mut columns = Vec::new();
        for x in 0..ICON {
            if (0..ICON).any(|y| pixels[((y * ICON + x) * 4 + 3) as usize] != 0) {
                columns.push(x);
            }
        }
        columns
    }

    fn lit_rows(pixels: &[u8], x: u32) -> Vec<u32> {
        (0..ICON)
            .filter(|y| pixels[((y * ICON + x) * 4 + 3) as usize] != 0)
            .collect()
    }

    #[test]
    fn the_glyph_is_four_bars_of_the_logos_geometry() {
        let pixels = paint(Glyph::Dim, 0, false);

        assert_eq!(pixels.len(), (ICON * ICON * 4) as usize);
        assert_eq!(
            lit_columns(&pixels),
            vec![2, 3, 4, 5, 10, 11, 12, 13, 18, 19, 20, 21, 26, 27, 28, 29]
        );
        // 10 / 20 / 14 / 8 tall, centred on one midline.
        assert_eq!(lit_rows(&pixels, 2).len(), 10);
        assert_eq!(lit_rows(&pixels, 10).len(), 20);
        assert_eq!(lit_rows(&pixels, 18).len(), 14);
        assert_eq!(lit_rows(&pixels, 26).len(), 8);
        assert_eq!(lit_rows(&pixels, 2)[0], 11);
        assert_eq!(lit_rows(&pixels, 10)[0], 6);
    }

    #[test]
    fn each_state_has_its_own_colour() {
        let at = |pixels: &[u8], x: u32, y: u32| {
            let at = ((y * ICON + x) * 4) as usize;
            [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
        };

        assert_eq!(at(&paint(Glyph::Dim, 0, false), 2, 11), DIM);
        assert_eq!(at(&paint(Glyph::Live, 0, false), 2, 11), LIVE);
        assert_eq!(at(&paint(Glyph::Hollow, 0, false), 2, 11), FAINT);
        // Nothing is painted outside the bars.
        assert_eq!(at(&paint(Glyph::Dim, 0, false), 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn the_disabled_glyph_is_hollow() {
        let pixels = paint(Glyph::Hollow, 0, false);
        // The tallest bar spans x 10..13. Its two edge columns are solid…
        assert_eq!(lit_rows(&pixels, 10).len(), 20);
        assert_eq!(lit_rows(&pixels, 13).len(), 20);
        // …and the two between them carry only the top and bottom edges.
        assert_eq!(lit_rows(&pixels, 11), vec![6, 25]);
        assert_eq!(lit_rows(&pixels, 12), vec![6, 25]);
        // The same bar is solid when it is not hollow.
        assert_eq!(lit_rows(&paint(Glyph::Dim, 0, false), 11).len(), 20);
    }

    #[test]
    fn recording_has_two_frames_and_nothing_else_animates() {
        assert_ne!(paint(Glyph::Live, 0, false), paint(Glyph::Live, 1, false));
        assert_eq!(paint(Glyph::Live, 0, false), paint(Glyph::Live, 2, false));
        assert_eq!(paint(Glyph::Dim, 0, false), paint(Glyph::Dim, 1, false));
        assert_eq!(
            paint(Glyph::Hollow, 0, false),
            paint(Glyph::Hollow, 1, false)
        );
    }

    #[test]
    fn an_open_menu_puts_the_sel_square_behind_the_bars() {
        let at = |pixels: &[u8], x: u32, y: u32| {
            let at = ((y * ICON + x) * 4) as usize;
            [pixels[at], pixels[at + 1], pixels[at + 2], pixels[at + 3]]
        };
        let open = paint(Glyph::Dim, 0, true);
        let closed = paint(Glyph::Dim, 0, false);
        // A corner pixel stays transparent (the square is rounded), the
        // centre of an edge is --sel, and the bars still paint their colour.
        assert_eq!(at(&open, 0, 0), [0, 0, 0, 0]);
        assert_eq!(at(&open, 16, 0), SEL);
        assert_eq!(at(&open, 2, 11), DIM);
        assert_ne!(open, closed);
    }
}
