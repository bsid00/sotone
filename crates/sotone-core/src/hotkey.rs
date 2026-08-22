//! Push-to-talk and toggle: two global bindings, watched process-wide.
//!
//! The user holds one physical control — a keyboard key or a mouse side button
//! — speaks, and releases. This module turns that into [`PttEvent::Pressed`]
//! and [`PttEvent::Released`], which the caller wires straight to
//! `AudioEngine::begin_utterance` / `end_utterance`.
//!
//! Alongside it there is a second, independently bound control for long
//! findings: press once to start, press again to stop.
//! That one produces [`PttEvent::Toggled`] on each *down* transition. This
//! module stays stateless about what is being recorded — it reports presses of
//! the controls it was given, and the caller owns the one recording state that
//! decides what a toggle press means.
//!
//! # Thread model: two threads, both for the life of the process
//!
//! On Windows `rdev::listen_scoped` creates a message-only window, registers
//! the keyboard — and the mouse only if a mouse button is actually bound, see
//! below — as Raw Input sinks, and then blocks forever pumping messages on the
//! calling thread; decoding and our callback run on that same thread. There is
//! no stop and no restart. So:
//!
//! * The **hook thread** exists to be blocked in `rdev::listen_scoped`. Its
//!   callback does one thing — `send` a [`RawEvent`] — and returns. It keeps
//!   the name from when it really did run a hook; it now runs a
//!   `WM_INPUT` message pump instead, and **nothing outside this process waits
//!   on it**. The enqueue-and-return discipline stands regardless (invariant
//!   5): what used to be system-wide stutter is now our own queue depth, and
//!   queue depth is latency — every millisecond spent in the callback is a
//!   millisecond of backlog in front of the next key.
//! * The **consumer thread** owns all the thinking: filtering to the bound
//!   controls, de-duplicating auto-repeat, and calling the caller's handler.
//!
//! Auto-repeat is why the split matters. Holding a key makes Windows deliver a
//! fresh `KeyPress` every few tens of milliseconds — raw input repeats the make
//! code just as the hook repeated its `WM_KEYDOWN`; only the first one starts
//! an utterance — and for the toggle key, only the first one may flip the
//! recording, or leaning on it would start and stop dozens of times a second.
//! Each binding therefore carries its own held flag. Deciding that is a state
//! read plus a compare — trivial work, but *any* work in the hook is the thing
//! invariant 5 forbids, so it lives on the consumer thread. Mouse buttons do
//! not auto-repeat, but they go through the same filter.
//!
//! Neither thread is ever joined or stopped; [`HotkeyListener`] is a handle,
//! not an owner. Both threads are detached and die with the process.
//!
//! # The mouse was a tax; it is now only our own queue
//!
//! Invariant 5 is about what the callback *does*, and this module has always
//! honoured it. There is another half of the bill: merely *having* a
//! `WH_MOUSE_LL` hook installed made Windows route every mouse input packet on
//! the machine — around a thousand a second from a gaming mouse — through this
//! process's hook thread before the foreground application received it. That is
//! a cross-process scheduling round trip per event, and under a CPU-saturating
//! fullscreen game the hook thread was starved: mouse deltas queued and
//! released in bursts, which the player feels as the pointer speeding up and
//! stalling. (Reproduced A/B in a heavy Minecraft modpack; quitting
//! Sotone fixed it instantly.)
//!
//! The first fix was to install the mouse hook only when a mouse button is bound,
//! which left a residual: bind `MouseX1`/`MouseX2` and the routing came back.
//! **That residual is gone entirely.** There is no hook of either kind
//! any more — the vendored rdev registers Raw Input sinks instead, and raw
//! input is delivered asynchronously to this process's own message queue.
//! Registering the mouse still means every movement packet arrives here (there
//! is no buttons-only subscription), but it arrives *after* the foreground
//! application has already had it, and a slow drain grows nothing but our
//! backlog. The vendored callback still discards motion on a flags compare
//! before decoding or allocating anything.
//!
//! The scope choice therefore stays, for a smaller reason: with keyboard-only
//! bindings the mouse usage serves nothing — [`Binding`] is a key or one of the
//! two side buttons, and moves and wheel events have no consumer anywhere — so
//! [`Bindings::needs_mouse`] decides the [`rdev::HookScope`] and keyboard-only
//! bindings register **no** mouse usage at all.
//!
//! [`capture_binding`] keeps both unconditionally: the answer to "which control
//! did you just press" may be a mouse button, and that helper lives for
//! seconds, not for the life of the app. Rebinding re-decides the scope for free —
//! a rebind respawns the helper process, so the scope is chosen
//! afresh at every start.
//!
//! # Where this runs
//!
//! The console runners (`examples/dictate.rs`, `examples/hotkey_probe.rs`) call
//! [`HotkeyListener::spawn`] directly. The **app does not**: it
//! runs this module inside the `sotone-hook` helper process and receives the
//! events over a pipe in the [`wire`] format, because an in-process low-level
//! hook starved while Sotone's own WebView2 window was foreground
//! (tauri-apps/tauri#14770). Nothing in this module knows the difference.
//!
//! The hook that starvation applied to is gone now, and raw input has no
//! equivalent mechanism — a message-only window's queue is nobody's to starve.
//! The helper process stays anyway: it also buys lifetime isolation (a rebind
//! respawns a process rather than trying to un-listen), and that starvation
//! history says we do not re-litigate the split on theory.
//!
//! # Elevated windows (UIPI): unchanged in the parts we know, unverified in the rest
//!
//! With low-level hooks an unelevated process received nothing while a window
//! belonging to an elevated process had focus — if the app under test ran as
//! administrator, Sotone went deaf while it was in the foreground. Raw input is
//! routed by the driver stack to every process that registered a sink, which is
//! a different path, so it **may** cross that line where hooks did not. This is
//! **unverified either way**: nobody has run Sotone against an elevated
//! foreground app under raw input, so assume the old limitation until someone
//! has. What is known not to change: the UAC secure desktop (the consent
//! dialog) delivers nothing to either mechanism.
//!
//! # Invariant 1: no synthetic input
//!
//! `rdev` also ships `simulate` and, behind the `unstable_grab` feature, an
//! event-swallowing API. Neither is referenced here and the feature is not
//! enabled in `Cargo.toml`. This module only *observes*; the keypress still
//! reaches the app under test untouched.

pub mod wire;

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::SystemTime;

use rdev::{Button, EventType, HookScope, Key};

// ---------------------------------------------------------------------------
// Binding token
// ---------------------------------------------------------------------------

/// The mouse side buttons as Windows reports them.
///
/// rdev has no named variants for side buttons, so it passes the platform's own
/// number through as `Button::Unknown(n)`. On Windows that number is 1 for
/// XBUTTON1 and 2 for XBUTTON2 — the high word of `WM_XBUTTONDOWN`'s
/// `mouseData` under the old low-level hook, and now `RI_MOUSE_BUTTON_4_*` /
/// `RI_MOUSE_BUTTON_5_*` in a raw mouse packet, deliberately decoded to the
/// same two numbers so a config written by any build keeps meaning the same
/// physical button. The mapping is still OS-dependent: on Linux and macOS the
/// same physical button may arrive as a different number, which is part of the
/// deferred platform-parity pass.
const XBUTTON1: u8 = 1;
/// See [`XBUTTON1`].
const XBUTTON2: u8 = 2;

/// Windows virtual-key code of F13. F13–F24 are contiguous, `0x7C..=0x87`.
///
/// rdev 0.5.3's `Key` enum stops at `F12`, so every key above it arrives as
/// `Key::Unknown(vk)`. Sotone's default binding is F13 precisely because no
/// normal application uses it, so these have to be expressible — we name them
/// here rather than making the user type a raw scan code.
const VK_F13: u32 = 0x7C;
/// How many extended function keys we name: F13 through F24.
const EXTENDED_FUNCTION_KEYS: u32 = 12;

/// One push-to-talk binding, in Sotone's own vocabulary.
///
/// This is the parsed form of the `hotkey` string in the config file. It is
/// deliberately a Sotone type rather than a re-exported rdev one: the config
/// spelling is a promise to the user and must not drift when rdev renames an
/// enum variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Binding {
    /// A keyboard key.
    Key(Key),
    /// The first mouse side button (`XBUTTON1`).
    MouseX1,
    /// The second mouse side button (`XBUTTON2`).
    MouseX2,
}

/// A `hotkey` string the config layer cannot make sense of.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingError {
    /// The token is not one Sotone knows.
    #[error(
        "'{token}' is not a hotkey Sotone recognises. Expected a key name such as \
         F13, Space, Escape or KeyA, or a mouse side button: MouseX1, MouseX2."
    )]
    UnknownToken {
        /// Exactly what the user wrote, so the message points at their file.
        token: String,
    },
}

impl Binding {
    /// The canonical config spelling of this binding.
    ///
    /// Equal to [`Display`](fmt::Display), but borrowed where possible.
    #[must_use]
    pub fn token(self) -> BindingToken {
        match self {
            Binding::MouseX1 => BindingToken::Static("MouseX1"),
            Binding::MouseX2 => BindingToken::Static("MouseX2"),
            Binding::Key(key) => key_token(key),
        }
    }

    /// The binding a keyboard key names.
    ///
    /// Total: every key is *some* binding, including the ones rdev has no name
    /// for. Whether it is one the config can hold is [`Binding::is_nameable`].
    #[must_use]
    pub const fn of_key(key: Key) -> Self {
        Binding::Key(key)
    }

    /// The binding a mouse button names, if Sotone has one for it.
    ///
    /// Only the two side buttons: left, right and middle are how the user
    /// operates everything else on their machine, and binding one of them would
    /// make Sotone record every time they clicked something.
    #[must_use]
    pub const fn of_button(button: Button) -> Option<Self> {
        match button {
            Button::Unknown(XBUTTON1) => Some(Binding::MouseX1),
            Button::Unknown(XBUTTON2) => Some(Binding::MouseX2),
            _ => None,
        }
    }

    /// The binding an observed **press** names, or `None` when that press is
    /// not something Sotone can bind to.
    ///
    /// This is the whole of Settings' "press to rebind": the helper
    /// runs the hook, this turns the one press it saw into a binding, and the
    /// binding's [`token`](Binding::token) is what crosses the pipe. Nothing
    /// here generates a press — it only names one that already happened
    /// (invariant 1).
    ///
    /// Two kinds of press deliberately answer `None`, and the capture UI treats
    /// both as "keep waiting" rather than as an error:
    ///
    /// * anything that is not a key or side-button *down* — mouse movement,
    ///   the wheel, the ordinary mouse buttons, and every release;
    /// * a key rdev has no name for, because its token would not parse back
    ///   (see [`BindingToken::Unnamed`]) and Settings must never write a
    ///   `hotkey` the next launch cannot read.
    ///
    /// The round trip is therefore a property of this function: whatever it
    /// returns renders to a token that parses back to the same binding.
    #[must_use]
    pub fn from_press(event_type: EventType) -> Option<Self> {
        let binding = match event_type {
            EventType::KeyPress(key) => Binding::of_key(key),
            EventType::ButtonPress(button) => Binding::of_button(button)?,
            _ => return None,
        };
        binding.is_nameable().then_some(binding)
    }

    /// Whether this binding has a canonical token that parses back.
    ///
    /// False only for a key rdev has no name for, which parsing can never
    /// produce — so a binding that came from the config is always nameable, and
    /// only a binding built straight from an observed event can fail this.
    #[must_use]
    pub fn is_nameable(self) -> bool {
        matches!(self.token(), BindingToken::Static(_))
    }
}

/// The rendered form of a [`Binding`], avoiding an allocation in the common
/// case where the token is a static string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingToken {
    /// A canonical, round-trippable token.
    Static(&'static str),
    /// A key rdev has no name for, rendered as its raw platform code. Not
    /// parseable; see [`Binding::token`].
    Unnamed(u32),
}

impl fmt::Display for BindingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindingToken::Static(name) => f.write_str(name),
            BindingToken::Unnamed(code) => write!(f, "Unknown({code})"),
        }
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.token(), f)
    }
}

impl FromStr for Binding {
    type Err = BindingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let token = s.trim();

        if token.eq_ignore_ascii_case("MouseX1") {
            return Ok(Binding::MouseX1);
        }
        if token.eq_ignore_ascii_case("MouseX2") {
            return Ok(Binding::MouseX2);
        }
        if let Some(key) = key_from_named_token(token) {
            return Ok(Binding::Key(key));
        }
        if let Some(key) = extended_function_key(token) {
            return Ok(Binding::Key(key));
        }

        Err(BindingError::UnknownToken {
            token: token.to_owned(),
        })
    }
}

/// Generates the two-way table between `rdev::Key` variants and their config
/// spellings.
///
/// The generated `match` has no wildcard arm, so adding a variant to
/// `rdev::Key` breaks this build rather than silently producing a key nobody
/// can bind.
macro_rules! key_tokens {
    ($($variant:ident),* $(,)?) => {
        /// The canonical config spelling of a key.
        fn key_token(key: Key) -> BindingToken {
            match key {
                $(Key::$variant => BindingToken::Static(stringify!($variant)),)*
                Key::Unknown(code) => match extended_function_token(code) {
                    Some(name) => BindingToken::Static(name),
                    // Reachable only when a caller builds `Binding::Key`
                    // straight from an rdev event with a key we have no name
                    // for. Parsing can never produce one, so this spelling is
                    // a diagnostic and deliberately does not parse back; the
                    // capture UI decides what to do with such keys.
                    None => BindingToken::Unnamed(code),
                },
            }
        }

        /// Parses a named-key token, case-insensitively.
        fn key_from_named_token(token: &str) -> Option<Key> {
            $(if token.eq_ignore_ascii_case(stringify!($variant)) {
                return Some(Key::$variant);
            })*
            None
        }

        #[cfg(test)]
        /// Every named token, for the round-trip test.
        const NAMED_TOKENS: &[&str] = &[$(stringify!($variant)),*];
    };
}

key_tokens!(
    Alt,
    AltGr,
    Backspace,
    CapsLock,
    ControlLeft,
    ControlRight,
    Delete,
    DownArrow,
    End,
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Home,
    LeftArrow,
    MetaLeft,
    MetaRight,
    PageDown,
    PageUp,
    Return,
    RightArrow,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    UpArrow,
    PrintScreen,
    ScrollLock,
    Pause,
    NumLock,
    BackQuote,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    Minus,
    Equal,
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    LeftBracket,
    RightBracket,
    SemiColon,
    Quote,
    BackSlash,
    IntlBackslash,
    Comma,
    Dot,
    Slash,
    Insert,
    KpReturn,
    KpMinus,
    KpPlus,
    KpMultiply,
    KpDivide,
    Kp0,
    Kp1,
    Kp2,
    Kp3,
    Kp4,
    Kp5,
    Kp6,
    Kp7,
    Kp8,
    Kp9,
    KpDelete,
    Function,
);

/// Parses `F13`..`F24` into the `Key::Unknown(vk)` rdev actually delivers.
///
/// These are the keys a mouse or keyboard macro layer emits precisely because
/// no application binds them, which makes them the right default for a
/// push-to-talk key that must not disturb the app under test.
fn extended_function_key(token: &str) -> Option<Key> {
    let digits = token.strip_prefix(['F', 'f'])?;
    // `parse` would accept "+13" and " 13"; the tokens are exactly two digits.
    if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u32 = digits.parse().ok()?;
    let index = number.checked_sub(13)?;
    (index < EXTENDED_FUNCTION_KEYS).then(|| Key::Unknown(VK_F13 + index))
}

/// The `F13`..`F24` token for a raw key code, if it is one of them.
fn extended_function_token(code: u32) -> Option<&'static str> {
    const TOKENS: [&str; EXTENDED_FUNCTION_KEYS as usize] = [
        "F13", "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
    ];
    let index = code.checked_sub(VK_F13)?;
    TOKENS.get(index as usize).copied()
}

// ---------------------------------------------------------------------------
// The watched set
// ---------------------------------------------------------------------------

/// The controls a listener watches: hold-to-talk, press-again-to-stop, or both.
///
/// `None` means that mode is switched off in the config, and no event of its
/// kind will ever be delivered. Both `None` is rejected by
/// [`HotkeyListener::spawn`] — a listener that can never fire is a bug, and the
/// config layer rejects the same combination before it can get this far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bindings {
    /// Held to record, released to stop. Produces `Pressed` / `Released`.
    pub ptt: Option<Binding>,
    /// Pressed to start, pressed again to stop. Produces `Toggled`.
    pub toggle: Option<Binding>,
}

impl Bindings {
    /// Both modes bound.
    #[must_use]
    pub fn new(ptt: Binding, toggle: Binding) -> Self {
        Self {
            ptt: Some(ptt),
            toggle: Some(toggle),
        }
    }

    /// Only push-to-talk.
    #[must_use]
    pub fn ptt_only(ptt: Binding) -> Self {
        Self {
            ptt: Some(ptt),
            toggle: None,
        }
    }

    /// Only toggle.
    #[must_use]
    pub fn toggle_only(toggle: Binding) -> Self {
        Self {
            ptt: None,
            toggle: Some(toggle),
        }
    }

    /// Whether nothing at all is bound.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.ptt.is_none() && self.toggle.is_none()
    }

    /// Whether watching this set needs the OS mouse stream at all.
    ///
    /// True only when an *enabled* mode is bound to a mouse side button. A
    /// mode switched off (`None`) cannot justify the registration, however it
    /// was bound before — see the module docs for what watching the mouse
    /// costs when nothing needs it.
    #[must_use]
    pub fn needs_mouse(self) -> bool {
        self.ptt.is_some_and(is_mouse_binding) || self.toggle.is_some_and(is_mouse_binding)
    }

    /// Which mode owns `key`, if either.
    ///
    /// Push-to-talk wins a tie. A tie should be impossible — the config rejects
    /// two enabled modes on one binding — but the API cannot stop a caller
    /// building one, and silently emitting both a `Pressed` and a `Toggled` for
    /// one physical press would start two recordings from one key.
    fn mode_for_key(self, key: Key) -> Option<Mode> {
        if self.ptt.is_some_and(|bound| matches_key(bound, key)) {
            return Some(Mode::Ptt);
        }
        self.toggle
            .is_some_and(|bound| matches_key(bound, key))
            .then_some(Mode::Toggle)
    }

    /// Which mode owns `button`, if either. See [`Bindings::mode_for_key`].
    fn mode_for_button(self, button: Button) -> Option<Mode> {
        if self.ptt.is_some_and(|bound| matches_button(bound, button)) {
            return Some(Mode::Ptt);
        }
        self.toggle
            .is_some_and(|bound| matches_button(bound, button))
            .then_some(Mode::Toggle)
    }
}

/// True for the two mouse side buttons, false for any keyboard key.
///
/// A free function rather than a `Binding` method: which controls exist is
/// public vocabulary, but "does this control cost us the mouse stream" is this
/// module's own business.
fn is_mouse_binding(binding: Binding) -> bool {
    matches!(binding, Binding::MouseX1 | Binding::MouseX2)
}

/// The narrowest hook scope that can still see everything `bindings` names.
///
/// The whole point of the scope: this is the one place the mouse registration
/// is decided for a long-lived listener, and it says "no" for every
/// keyboard-only configuration.
fn scope_for(bindings: Bindings) -> HookScope {
    if bindings.needs_mouse() {
        HookScope::KeyboardAndMouse
    } else {
        HookScope::Keyboard
    }
}

/// Which of the two recording modes an input event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ptt,
    Toggle,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What the caller of [`HotkeyListener::spawn`] receives.
///
/// Named for push-to-talk, which is still the primary mode; `Toggled` is the
/// second binding's only event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttEvent {
    /// The push-to-talk control went down. Auto-repeat while it stays down
    /// produces no further `Pressed`.
    Pressed,
    /// The push-to-talk control came up.
    Released {
        /// When the release happened, taken in the hook callback rather than
        /// when the consumer got around to it. Note timestamps are pinned to
        /// key release so rapid-fire notes keep the order they were spoken.
        time: SystemTime,
    },
    /// The toggle control went down. Auto-repeat while it stays down produces
    /// no further `Toggled`, and its release produces nothing at all.
    ///
    /// Whether this starts or stops a recording is the caller's state, not
    /// ours. The timestamp travels with it because a *stopping* toggle press is
    /// the closest thing this mode has to a key release, and that is the note's
    /// `spoken_at`.
    Toggled {
        /// When the press happened, taken in the hook callback.
        time: SystemTime,
    },
    /// The listener is gone and no further events will arrive. Either it
    /// failed to start, or the message pump returned. Not recoverable
    /// in-process: rdev offers no restart.
    HookLost {
        /// Human-readable cause, for the UI and the log.
        reason: String,
    },
}

/// The payload the hook callback puts on the channel.
///
/// Deliberately *not* `rdev::Event`: that carries `name: Option<String>`, a
/// heap allocation rdev already made before calling us, and moving it across
/// the channel would keep it alive for no reason. This is `Copy` and small.
#[derive(Debug)]
enum RawEvent {
    /// One OS input event, exactly as rdev handed it over.
    Input {
        /// The event itself.
        event_type: EventType,
        /// `SystemTime::now()` as taken inside the hook.
        time: SystemTime,
    },
    /// The hook thread is finished. Sent explicitly rather than inferred from
    /// the channel disconnecting, because rdev parks our callback — and with
    /// it a clone of the sender — in a process-global that it never clears.
    /// The channel therefore never disconnects. The reason string is built
    /// after `rdev::listen` has returned, never in the callback.
    HookEnded {
        /// Human-readable cause.
        reason: String,
    },
}

/// Why a listener could not start.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    /// A thread could not be spawned.
    #[error("could not start the hotkey thread")]
    ThreadSpawn(#[source] std::io::Error),

    /// A listener is already running in this process.
    #[error("a hotkey listener is already running; rdev supports only one per process")]
    AlreadyRunning,

    /// Neither recording mode is bound, so the listener could never fire.
    ///
    /// The config layer rejects this combination at load time; this is the
    /// backstop for a caller that assembled [`Bindings`] by hand.
    #[error(
        "no hotkey is bound: at least one of push-to-talk and toggle must be enabled, \
         or there is no way to record"
    )]
    NoBindings,
}

/// rdev keeps its listen callback in a process-global, so a second `listen`
/// would race the first. Rebinding at runtime therefore is not a matter of
/// spawning again — it is out of scope here, and this flag makes the attempt
/// fail loudly instead of corrupting the first listener.
static LISTENER_RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// A running hotkey listener.
///
/// Dropping this does **not** stop anything: the threads it started run for the
/// life of the process (see the module docs). It exists to report what is bound
/// and to make the "one per process" rule visible at the type level.
#[derive(Debug)]
pub struct HotkeyListener {
    bindings: Bindings,
}

impl HotkeyListener {
    /// Starts watching the whole system for `bindings`.
    ///
    /// `handler` runs on the consumer thread and **must be cheap and
    /// non-blocking**: it is in line behind every subsequent input event. The
    /// intended body is a pair of `AudioEngine::begin_utterance` /
    /// `end_utterance` calls, which are cheap by contract — they flip a flag
    /// and nudge a worker. Anything that waits on a lock, a file or the UI
    /// thread belongs behind a channel of its own.
    ///
    /// Only one listener may exist per process; a second call returns
    /// [`HotkeyError::AlreadyRunning`].
    ///
    /// # Errors
    ///
    /// Returns [`HotkeyError::NoBindings`] if neither mode is bound, and
    /// [`HotkeyError::ThreadSpawn`] if the OS refuses a thread. A hook that
    /// fails to *install* is reported asynchronously as
    /// [`PttEvent::HookLost`], because that failure happens on the hook thread
    /// after this function has already returned.
    pub fn spawn<F>(bindings: Bindings, handler: F) -> Result<Self, HotkeyError>
    where
        F: Fn(PttEvent) + Send + 'static,
    {
        // Checked before the running flag is claimed, so a rejected call leaves
        // the process able to try again with a usable set.
        if bindings.is_empty() {
            return Err(HotkeyError::NoBindings);
        }

        if LISTENER_RUNNING.swap(true, Ordering::SeqCst) {
            return Err(HotkeyError::AlreadyRunning);
        }

        let (tx, rx) = mpsc::channel::<RawEvent>();

        // Decided here, once, from the set we were handed: a listener runs for
        // the life of the process, so this is the choice between being on the
        // machine's mouse path all session or not being on it at all. A rebind
        // respawns the whole helper process, which re-runs this.
        let scope = scope_for(bindings);

        let spawned = thread::Builder::new()
            .name("sotone-hotkey-consumer".to_owned())
            .spawn(move || consume(bindings, &rx, handler))
            .and_then(|_consumer| {
                thread::Builder::new()
                    .name("sotone-hotkey-hook".to_owned())
                    .spawn(move || hook_thread(tx, scope))
            });

        if let Err(err) = spawned {
            // The consumer may already be running; it will exit on its own when
            // the sender drops. Releasing the flag lets a retry through.
            LISTENER_RUNNING.store(false, Ordering::SeqCst);
            return Err(HotkeyError::ThreadSpawn(err));
        }

        Ok(Self { bindings })
    }

    /// The bindings this listener watches.
    #[must_use]
    pub fn bindings(&self) -> Bindings {
        self.bindings
    }
}

/// What a capture run reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureEvent {
    /// The first press that named a binding. Exactly one of these is ever
    /// delivered; the caller is expected to stop after it.
    Captured(Binding),
    /// The hook is gone, so no press will ever be seen. Same meaning — and the
    /// same non-recoverability — as [`PttEvent::HookLost`].
    HookLost {
        /// Human-readable cause.
        reason: String,
    },
}

/// Watch for **one** press and report which binding it was.
///
/// This is the hook half of Settings' "press to rebind" (hotkeys are
/// captured by pressing the key or button; nobody types virtual key codes). It
/// is the same two threads [`HotkeyListener::spawn`] starts, and the same
/// enqueue-only callback — the only difference is what the consumer does with
/// what it sees: instead of filtering to a bound set it takes the first event
/// [`Binding::from_press`] can name, hands it over, and then reports nothing
/// further.
///
/// Presses that name no binding are **ignored, and capture continues**: a
/// stray mouse move, a modifier, or a key rdev has no name for must not end a
/// capture with a binding the config could not hold.
///
/// Like the listener, the threads this starts are detached and live for the
/// process — which is exactly why the app runs this inside a short-lived
/// `sotone-hook --capture` process rather than in itself (the whole point of the
/// helper: no global input listener in the app process, ever).
///
/// # Errors
///
/// [`HotkeyError::AlreadyRunning`] if this process already installed a hook,
/// and [`HotkeyError::ThreadSpawn`] if the OS refuses a thread. A hook that
/// fails to *install* arrives asynchronously as [`CaptureEvent::HookLost`].
pub fn capture_binding<F>(handler: F) -> Result<(), HotkeyError>
where
    F: Fn(CaptureEvent) + Send + 'static,
{
    if LISTENER_RUNNING.swap(true, Ordering::SeqCst) {
        return Err(HotkeyError::AlreadyRunning);
    }

    let (tx, rx) = mpsc::channel::<RawEvent>();

    let spawned = thread::Builder::new()
        .name("sotone-capture-consumer".to_owned())
        .spawn(move || consume_capture(&rx, handler))
        .and_then(|_consumer| {
            thread::Builder::new()
                .name("sotone-hotkey-hook".to_owned())
                // Both device classes, unlike the listener: the press being
                // captured may be a mouse side button, and there is no set of
                // bindings yet to narrow the scope from. Affordable because
                // this helper is alive for the few seconds of a rebind, not for
                // the session.
                .spawn(move || hook_thread(tx, HookScope::KeyboardAndMouse))
        });

    if let Err(err) = spawned {
        LISTENER_RUNNING.store(false, Ordering::SeqCst);
        return Err(HotkeyError::ThreadSpawn(err));
    }

    Ok(())
}

/// The capture consumer: first nameable press wins, then silence.
fn consume_capture<F>(rx: &Receiver<RawEvent>, handler: F)
where
    F: Fn(CaptureEvent),
{
    let mut reported = false;

    for raw in rx {
        match raw {
            RawEvent::Input { event_type, .. } => {
                // The timestamp is deliberately dropped: a capture is a question
                // about *which* control, never about when it was pressed.
                if reported {
                    continue;
                }
                if let Some(binding) = Binding::from_press(event_type) {
                    reported = true;
                    handler(CaptureEvent::Captured(binding));
                }
            }
            RawEvent::HookEnded { reason } => {
                // Reported even after a capture, because the caller may still be
                // waiting on the process to end; a caller that has already acted
                // simply ignores it.
                handler(CaptureEvent::HookLost { reason });
                return;
            }
        }
    }
}

/// The hook thread: start the listener, pump, and report the end.
///
/// `scope` decides whether the mouse is watched at all; see the module docs.
/// Everything expensive in here happens *outside* `rdev::listen_scoped` —
/// before it, or after it has already returned.
fn hook_thread(tx: Sender<RawEvent>, scope: HookScope) {
    let sender = tx.clone();

    // Only the `listen` family is ever named from rdev — and `simulate` and
    // `grab` are deleted from the vendored copy outright, so invariant 1 holds
    // by absence rather than by our restraint.
    let outcome = rdev::listen_scoped(
        move |event| {
            // THE HOT PATH. This runs while the vendored rdev is handling a
            // WM_INPUT message, for every keystroke on the machine (and every
            // mouse button when one is bound). Enqueue and return: no matching,
            // no dedup, no logging, no locking, no formatting. `send` on an
            // unbounded channel does one node allocation and one atomic swap,
            // and never blocks (invariant 5).
            //
            // With raw input no other process is waiting behind this call —
            // raw input is asynchronous — but the bar does not move: work here
            // is `WM_INPUT` backlog, and backlog is push-to-talk latency.
            let _ = sender.send(RawEvent::Input {
                event_type: event.event_type,
                time: event.time,
            });
        },
        scope,
    );

    // Past this point the listener is dead either way: `Err` means it never
    // started (window, registration or a broken message queue), `Ok` means the
    // message pump returned. Either way no further key will ever be seen, so
    // say so instead of going quiet — the app has to be able to tell the user
    // why push-to-talk stopped working.
    //
    // Formatting and logging here is fine: `listen` has returned, so this is no
    // longer the hook callback.
    let reason = match outcome {
        Ok(()) => "the global input hook stopped delivering events".to_owned(),
        // `ListenError` implements Debug but not Display, hence `{err:?}`.
        Err(err) => format!("the global input hook could not be installed: {err:?}"),
    };
    tracing::error!("{reason}");
    let _ = tx.send(RawEvent::HookEnded { reason });
}

/// The consumer thread: filter, dedup, deliver.
fn consume<F>(bindings: Bindings, rx: &Receiver<RawEvent>, handler: F)
where
    F: Fn(PttEvent),
{
    let mut filter = PttFilter::default();

    for raw in rx {
        match raw {
            RawEvent::Input { event_type, time } => {
                if let Some(event) = filter.apply(bindings, event_type, time) {
                    handler(event);
                }
            }
            RawEvent::HookEnded { reason } => {
                handler(PttEvent::HookLost { reason });
                return;
            }
        }
    }
}

/// Turns raw input events into hotkey events for the bound set.
///
/// Windows auto-repeat delivers a fresh `KeyPress` every few tens of
/// milliseconds while a key is held; only the first counts. Each binding keeps
/// its own held flag, so the two modes never de-duplicate each other. A
/// push-to-talk release with no matching press — the key was already down when
/// the hook installed, or the press went to an elevated window — is dropped,
/// because there is no utterance for it to end.
///
/// The toggle key's *release* is deliberately silent: the mode's whole point is
/// that the control is not held, so only down transitions carry meaning.
#[derive(Debug, Default)]
struct PttFilter {
    ptt_held: bool,
    toggle_held: bool,
}

impl PttFilter {
    fn apply(
        &mut self,
        bindings: Bindings,
        event_type: EventType,
        time: SystemTime,
    ) -> Option<PttEvent> {
        let (mode, down) = match event_type {
            EventType::KeyPress(key) => (bindings.mode_for_key(key)?, true),
            EventType::KeyRelease(key) => (bindings.mode_for_key(key)?, false),
            EventType::ButtonPress(button) => (bindings.mode_for_button(button)?, true),
            EventType::ButtonRelease(button) => (bindings.mode_for_button(button)?, false),
            // Mouse movement and wheel traffic. These are dropped
            // two layers earlier — the vendored rdev discards non-button mouse
            // packets before decoding them, and with keyboard-only bindings the
            // mouse is not watched at all — so this arm is a backstop rather
            // than the common case it used to be. On the raw-input Windows path
            // never builds a `MouseMove` or `Wheel` event at all; the arm stays
            // for the other platforms and for the types' sake.
            _ => return None,
        };

        match (mode, down) {
            (Mode::Ptt, true) => (!self.ptt_held).then(|| {
                self.ptt_held = true;
                PttEvent::Pressed
            }),
            (Mode::Ptt, false) => self.ptt_held.then(|| {
                self.ptt_held = false;
                PttEvent::Released { time }
            }),
            (Mode::Toggle, true) => (!self.toggle_held).then(|| {
                self.toggle_held = true;
                PttEvent::Toggled { time }
            }),
            (Mode::Toggle, false) => {
                // Only clears the auto-repeat latch; the recording, if any,
                // outlives the press by design.
                self.toggle_held = false;
                None
            }
        }
    }
}

/// True when `key` is the bound keyboard key.
///
/// Expressed through [`Binding::of_key`] rather than beside it: the listener's
/// idea of which control an event belongs to and the capture UI's idea of which
/// control was just pressed have to be the same idea, or a captured binding
/// could be one the listener then ignores.
fn matches_key(binding: Binding, key: Key) -> bool {
    binding == Binding::of_key(key)
}

/// True when `button` is the bound mouse side button. See [`matches_key`].
fn matches_button(binding: Binding, button: Button) -> bool {
    Binding::of_button(button) == Some(binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A distinguishable, non-`now` timestamp so a test can prove the release
    /// time is carried through rather than re-taken.
    fn stamp() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn every_named_token_round_trips() {
        for token in NAMED_TOKENS {
            let binding: Binding = token.parse().expect("named token must parse");
            assert_eq!(binding.to_string(), *token, "round trip for {token}");
        }
    }

    #[test]
    fn extended_function_tokens_round_trip() {
        for number in 13..=24 {
            let token = format!("F{number}");
            let binding: Binding = token.parse().expect("F13..F24 must parse");
            assert_eq!(binding.to_string(), token);
        }
    }

    #[test]
    fn f13_is_the_key_windows_actually_delivers() {
        // rdev 0.5.3 has no F13 variant; the hook reports VK 0x7C as Unknown.
        assert_eq!(
            "F13".parse::<Binding>(),
            Ok(Binding::Key(Key::Unknown(0x7C)))
        );
        assert_eq!(
            "F24".parse::<Binding>(),
            Ok(Binding::Key(Key::Unknown(0x87)))
        );
    }

    #[test]
    fn mouse_side_buttons_round_trip() {
        assert_eq!("MouseX1".parse::<Binding>(), Ok(Binding::MouseX1));
        assert_eq!("MouseX2".parse::<Binding>(), Ok(Binding::MouseX2));
        assert_eq!(Binding::MouseX1.to_string(), "MouseX1");
        assert_eq!(Binding::MouseX2.to_string(), "MouseX2");
    }

    #[test]
    fn parsing_ignores_case_and_surrounding_space() {
        assert_eq!(
            "f13".parse::<Binding>(),
            Ok(Binding::Key(Key::Unknown(0x7C)))
        );
        assert_eq!("  space  ".parse::<Binding>(), Ok(Binding::Key(Key::Space)));
        assert_eq!("MOUSEX2".parse::<Binding>(), Ok(Binding::MouseX2));
        assert_eq!("keya".parse::<Binding>(), Ok(Binding::Key(Key::KeyA)));
    }

    #[test]
    fn the_default_config_hotkeys_parse_and_differ() {
        // The config defaults must never be tokens this module rejects, and the
        // two modes must not land on one physical key.
        let ptt: Binding = crate::config::DEFAULT_HOTKEY
            .parse()
            .expect("default push-to-talk token");
        let toggle: Binding = crate::config::DEFAULT_TOGGLE_HOTKEY
            .parse()
            .expect("default toggle token");
        assert_ne!(ptt, toggle);
    }

    #[test]
    fn garbage_names_the_bad_token() {
        let err = "Frobnicate".parse::<Binding>().expect_err("must not parse");
        assert_eq!(
            err,
            BindingError::UnknownToken {
                token: "Frobnicate".to_owned()
            }
        );
        assert!(err.to_string().contains("Frobnicate"));
        assert!(err.to_string().contains("MouseX1"));
    }

    #[test]
    fn unnameable_and_out_of_range_tokens_are_rejected() {
        for token in ["", "F0", "F25", "F1234", "Unknown(124)", "MouseX3", "F+3"] {
            assert!(
                token.parse::<Binding>().is_err(),
                "{token} must not parse into a binding"
            );
        }
    }

    /// The default pair: F13 held, F14 toggled. Both arrive from rdev as
    /// `Key::Unknown`, which is what the real hook delivers on Windows.
    fn default_bindings() -> Bindings {
        Bindings::new(
            Binding::Key(Key::Unknown(0x7C)),
            Binding::Key(Key::Unknown(0x7D)),
        )
    }

    #[test]
    fn auto_repeat_produces_one_press() {
        // Synthetic *data*, not synthetic OS input: these structs never touch
        // the input stack, they only drive the pure filter.
        let bindings = Bindings::ptt_only(Binding::Key(Key::Unknown(0x7C)));
        let mut filter = PttFilter::default();

        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7C)), stamp()),
            Some(PttEvent::Pressed)
        );
        for _ in 0..20 {
            assert_eq!(
                filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7C)), stamp()),
                None,
                "auto-repeat must not restart the utterance"
            );
        }
        assert_eq!(
            filter.apply(bindings, EventType::KeyRelease(Key::Unknown(0x7C)), stamp()),
            Some(PttEvent::Released { time: stamp() })
        );
        // And the next press starts a fresh one.
        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7C)), stamp()),
            Some(PttEvent::Pressed)
        );
    }

    #[test]
    fn toggle_auto_repeat_produces_one_event_per_down_transition() {
        // Leaning on the toggle key must not start and stop a recording dozens
        // of times a second.
        let bindings = default_bindings();
        let mut filter = PttFilter::default();
        let toggle = EventType::KeyPress(Key::Unknown(0x7D));

        assert_eq!(
            filter.apply(bindings, toggle, stamp()),
            Some(PttEvent::Toggled { time: stamp() })
        );
        for _ in 0..20 {
            assert_eq!(filter.apply(bindings, toggle, stamp()), None);
        }
        // The release itself says nothing — the recording is still running.
        assert_eq!(
            filter.apply(bindings, EventType::KeyRelease(Key::Unknown(0x7D)), stamp()),
            None
        );
        // The second press is the stop, and carries its own hook timestamp.
        let stop = stamp() + Duration::from_secs(42);
        assert_eq!(
            filter.apply(bindings, toggle, stop),
            Some(PttEvent::Toggled { time: stop })
        );
    }

    #[test]
    fn the_two_modes_have_independent_latches() {
        // Holding push-to-talk must not swallow a toggle press, and vice versa;
        // deciding what that *means* is the caller's state machine, not ours.
        let bindings = default_bindings();
        let mut filter = PttFilter::default();

        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7C)), stamp()),
            Some(PttEvent::Pressed)
        );
        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7D)), stamp()),
            Some(PttEvent::Toggled { time: stamp() })
        );
        assert_eq!(
            filter.apply(bindings, EventType::KeyRelease(Key::Unknown(0x7D)), stamp()),
            None
        );
        assert_eq!(
            filter.apply(bindings, EventType::KeyRelease(Key::Unknown(0x7C)), stamp()),
            Some(PttEvent::Released { time: stamp() })
        );
    }

    #[test]
    fn a_disabled_mode_is_deaf_to_its_own_key() {
        // `--no-toggle` / `toggle_enabled = false`: the key is still pressed on
        // the machine, it just means nothing to us.
        let bindings = Bindings::ptt_only(Binding::Key(Key::Unknown(0x7C)));
        let mut filter = PttFilter::default();
        for event in [
            EventType::KeyPress(Key::Unknown(0x7D)),
            EventType::KeyRelease(Key::Unknown(0x7D)),
        ] {
            assert_eq!(filter.apply(bindings, event, stamp()), None);
        }

        let bindings = Bindings::toggle_only(Binding::Key(Key::Unknown(0x7D)));
        let mut filter = PttFilter::default();
        for event in [
            EventType::KeyPress(Key::Unknown(0x7C)),
            EventType::KeyRelease(Key::Unknown(0x7C)),
        ] {
            assert_eq!(filter.apply(bindings, event, stamp()), None);
        }
        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Unknown(0x7D)), stamp()),
            Some(PttEvent::Toggled { time: stamp() })
        );
    }

    #[test]
    fn a_release_without_a_press_is_ignored() {
        let mut filter = PttFilter::default();
        assert_eq!(
            filter.apply(
                Bindings::ptt_only(Binding::Key(Key::Space)),
                EventType::KeyRelease(Key::Space),
                stamp()
            ),
            None
        );
    }

    #[test]
    fn other_keys_and_mouse_traffic_are_ignored() {
        let bindings = default_bindings();
        let mut filter = PttFilter::default();
        let noise = [
            EventType::KeyPress(Key::KeyA),
            EventType::KeyRelease(Key::KeyA),
            EventType::ButtonPress(Button::Left),
            EventType::ButtonRelease(Button::Left),
            EventType::ButtonPress(Button::Unknown(XBUTTON1)),
            EventType::MouseMove { x: 12.0, y: 34.0 },
            EventType::Wheel {
                delta_x: 0,
                delta_y: 1,
            },
        ];
        for event_type in noise {
            assert_eq!(filter.apply(bindings, event_type, stamp()), None);
        }
    }

    #[test]
    fn mouse_side_buttons_drive_the_filter() {
        let bindings = Bindings::ptt_only(Binding::MouseX2);
        let mut filter = PttFilter::default();
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonPress(Button::Unknown(XBUTTON2)),
                stamp()
            ),
            Some(PttEvent::Pressed)
        );
        // The other side button is not the bound one.
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
                stamp()
            ),
            None
        );
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonRelease(Button::Unknown(XBUTTON2)),
                stamp()
            ),
            Some(PttEvent::Released { time: stamp() })
        );
    }

    #[test]
    fn a_side_button_can_be_the_toggle_while_a_key_holds() {
        let bindings = Bindings::new(Binding::Key(Key::Space), Binding::MouseX1);
        let mut filter = PttFilter::default();
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonPress(Button::Unknown(XBUTTON1)),
                stamp()
            ),
            Some(PttEvent::Toggled { time: stamp() })
        );
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
                stamp()
            ),
            None
        );
    }

    #[test]
    fn one_key_bound_to_both_modes_only_ever_speaks_for_push_to_talk() {
        // The config rejects this; the filter still must not emit two events
        // for one physical press.
        let binding = Binding::Key(Key::Space);
        let bindings = Bindings::new(binding, binding);
        let mut filter = PttFilter::default();
        assert_eq!(
            filter.apply(bindings, EventType::KeyPress(Key::Space), stamp()),
            Some(PttEvent::Pressed)
        );
        assert_eq!(
            filter.apply(bindings, EventType::KeyRelease(Key::Space), stamp()),
            Some(PttEvent::Released { time: stamp() })
        );
    }

    #[test]
    fn no_bindings_at_all_is_rejected_rather_than_watched() {
        assert!(Bindings::default().is_empty());
        let err = HotkeyListener::spawn(Bindings::default(), |_| {}).expect_err("must be rejected");
        assert!(matches!(err, HotkeyError::NoBindings));
        // No hook was installed, so a later real listener is still possible.
        assert!(!LISTENER_RUNNING.load(Ordering::SeqCst));
    }

    #[test]
    fn the_release_timestamp_comes_from_the_event() {
        let bindings = Bindings::ptt_only(Binding::MouseX1);
        let mut filter = PttFilter::default();
        let press = stamp();
        let release = press + Duration::from_millis(1_234);

        filter.apply(
            bindings,
            EventType::ButtonPress(Button::Unknown(XBUTTON1)),
            press,
        );
        assert_eq!(
            filter.apply(
                bindings,
                EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
                release
            ),
            Some(PttEvent::Released { time: release })
        );
    }

    // -----------------------------------------------------------------------
    // Hook scope
    // -----------------------------------------------------------------------

    /// The truth table in full. A `true` here means Sotone sits on the machine's
    /// mouse path for the whole session, so every row is worth pinning.
    #[test]
    fn needs_mouse_is_true_exactly_when_an_enabled_mode_is_a_mouse_button() {
        let key = Binding::Key(Key::Unknown(0x7C));
        let other_key = Binding::Key(Key::Unknown(0x7D));

        // Keyboard only, in every shape a config can take.
        assert!(!Bindings::ptt_only(key).needs_mouse());
        assert!(!Bindings::toggle_only(key).needs_mouse());
        assert!(!Bindings::new(key, other_key).needs_mouse());
        // Not a configuration `spawn` accepts, but the answer must still be
        // "nothing needs the hook" rather than a panic or a `true`.
        assert!(!Bindings::default().needs_mouse());

        // Either slot, either side button, alone or beside a key.
        for button in [Binding::MouseX1, Binding::MouseX2] {
            assert!(Bindings::ptt_only(button).needs_mouse());
            assert!(Bindings::toggle_only(button).needs_mouse());
            assert!(Bindings::new(button, other_key).needs_mouse());
            assert!(Bindings::new(key, button).needs_mouse());
            assert!(Bindings::new(button, button).needs_mouse());
        }

        // A mode switched off cannot justify the hook, whatever it holds: this
        // is the F4/F8 case of a user who had once bound a side button.
        assert!(!Bindings {
            ptt: Some(key),
            toggle: None,
        }
        .needs_mouse());
    }

    /// The listener's scope is derived from that truth table and nothing else.
    #[test]
    fn the_hook_scope_follows_the_bindings() {
        assert_eq!(
            scope_for(Bindings::new(
                Binding::Key(Key::Unknown(0x7C)),
                Binding::Key(Key::Unknown(0x7D))
            )),
            HookScope::Keyboard,
            "keyboard-only bindings must install no mouse hook"
        );
        assert_eq!(
            scope_for(Bindings::ptt_only(Binding::Key(Key::Space))),
            HookScope::Keyboard
        );
        assert_eq!(
            scope_for(Bindings::new(Binding::Key(Key::Space), Binding::MouseX1)),
            HookScope::KeyboardAndMouse
        );
        assert_eq!(
            scope_for(Bindings::toggle_only(Binding::MouseX2)),
            HookScope::KeyboardAndMouse
        );
    }

    /// `HookScope::Keyboard` is the whole point; if the vendored fork ever
    /// loses the distinction this fails rather than quietly re-installing the
    /// mouse hook for everyone.
    #[test]
    fn only_the_mouse_scope_includes_the_mouse_hook() {
        assert!(!HookScope::Keyboard.includes_mouse());
        assert!(HookScope::KeyboardAndMouse.includes_mouse());
    }

    // -----------------------------------------------------------------------
    // Capture
    // -----------------------------------------------------------------------

    /// Every event type the hook can deliver, as *data*. None of this touches
    /// the input stack — it drives a pure function (invariant 1).
    fn every_event_kind() -> Vec<EventType> {
        let mut events = vec![
            EventType::MouseMove { x: 1.0, y: 2.0 },
            EventType::Wheel {
                delta_x: 0,
                delta_y: 3,
            },
            EventType::ButtonPress(Button::Left),
            EventType::ButtonPress(Button::Right),
            EventType::ButtonPress(Button::Middle),
            EventType::ButtonPress(Button::Unknown(XBUTTON1)),
            EventType::ButtonPress(Button::Unknown(XBUTTON2)),
            EventType::ButtonPress(Button::Unknown(7)),
            EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
            EventType::KeyPress(Key::Unknown(9_999)),
            EventType::KeyRelease(Key::Space),
        ];
        for token in NAMED_TOKENS {
            let binding: Binding = token.parse().expect("named token must parse");
            if let Binding::Key(key) = binding {
                events.push(EventType::KeyPress(key));
            }
        }
        for number in 13..=24 {
            let binding: Binding = format!("F{number}").parse().expect("F13..F24 parse");
            if let Binding::Key(key) = binding {
                events.push(EventType::KeyPress(key));
            }
        }
        events
    }

    /// The property the whole capture path rests on: whatever a press is
    /// reported as must parse back to the same binding. Settings writes that
    /// token into the config file, and a token the next launch cannot read
    /// would be an app that will not start because of a key the user pressed.
    #[test]
    fn every_captured_press_round_trips_through_its_token() {
        for event in every_event_kind() {
            let Some(binding) = Binding::from_press(event) else {
                continue;
            };
            let token = binding.token().to_string();
            assert_eq!(
                token.parse::<Binding>(),
                Ok(binding),
                "captured {event:?} rendered as {token}, which does not parse back"
            );
            // And the token is stable: rendering the parsed binding again gives
            // the same string, which is what the config file will hold.
            assert_eq!(binding.to_string(), token);
        }
    }

    #[test]
    fn capture_names_the_two_side_buttons_and_no_other_mouse_button() {
        assert_eq!(
            Binding::from_press(EventType::ButtonPress(Button::Unknown(XBUTTON1))),
            Some(Binding::MouseX1)
        );
        assert_eq!(
            Binding::from_press(EventType::ButtonPress(Button::Unknown(XBUTTON2))),
            Some(Binding::MouseX2)
        );
        // Left/right/middle are how the user works everything else on the
        // machine; binding one would record on every click.
        for button in [Button::Left, Button::Right, Button::Middle] {
            assert_eq!(Binding::from_press(EventType::ButtonPress(button)), None);
        }
    }

    #[test]
    fn capture_ignores_releases_movement_and_unnameable_keys() {
        for event in [
            EventType::KeyRelease(Key::Space),
            EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
            EventType::MouseMove { x: 4.0, y: 5.0 },
            EventType::Wheel {
                delta_x: 1,
                delta_y: 0,
            },
            // No token for this one, so capture must keep waiting rather than
            // hand Settings something it cannot write.
            EventType::KeyPress(Key::Unknown(9_999)),
        ] {
            assert_eq!(Binding::from_press(event), None, "{event:?}");
        }
    }

    /// The listener's "is this my key" and capture's "which key is this" must
    /// be one table. If they ever diverge, Settings could capture a binding the
    /// listener then ignores — a rebind that silently does nothing.
    #[test]
    fn capture_and_the_listener_agree_on_every_event() {
        for event in every_event_kind() {
            let Some(captured) = Binding::from_press(event) else {
                continue;
            };
            let mut filter = PttFilter::default();
            assert_eq!(
                filter.apply(Bindings::ptt_only(captured), event, stamp()),
                Some(PttEvent::Pressed),
                "the listener does not recognise the binding capture reported for {event:?}"
            );
        }
    }

    #[test]
    fn unnamed_keys_render_as_a_diagnostic_that_does_not_parse() {
        let binding = Binding::Key(Key::Unknown(9_999));
        assert_eq!(binding.to_string(), "Unknown(9999)");
        assert!(binding.to_string().parse::<Binding>().is_err());
    }

    // -----------------------------------------------------------------------
    // Raw input decode
    // -----------------------------------------------------------------------

    /// The vendored rdev's Windows listener receives Raw Input,
    /// and these drive its two pure decode functions directly: raw *numbers*
    /// in, `EventType` out. No window, no device, no thread — and nothing that
    /// generates input, only arithmetic on the fields a `WM_INPUT` packet would
    /// have carried (invariant 1).
    ///
    /// They exist because the move off `WH_KEYBOARD_LL` changed what the OS
    /// hands us — generic modifiers instead of sided ones, button flags instead
    /// of window messages — while the vocabulary above, every `Binding` token a
    /// user may already have written in their config, had to stay identical.
    #[cfg(windows)]
    mod raw_decode {
        use super::*;
        use rdev::{raw_keyboard_event, raw_mouse_events};

        // `RAWKEYBOARD::Flags`.
        const MAKE: u16 = 0x00;
        const BREAK: u16 = 0x01;
        const E0: u16 = 0x02;

        // The generic modifier virtual-key codes raw input reports, where the
        // low-level hook reported the sided ones.
        const VK_SHIFT: u16 = 0x10;
        const VK_CONTROL: u16 = 0x11;
        const VK_MENU: u16 = 0x12;
        // A few ordinary keys, which need no normalisation.
        const VK_A: u16 = 0x41;
        const VK_F13: u16 = 0x7C;
        const VK_F14: u16 = 0x7D;

        // `RAWKEYBOARD::MakeCode` (set 1 scan codes).
        const SCAN_LEFT_SHIFT: u16 = 0x2A;
        const SCAN_RIGHT_SHIFT: u16 = 0x36;
        const SCAN_CONTROL: u16 = 0x1D;
        const SCAN_MENU: u16 = 0x38;
        const SCAN_A: u16 = 0x1E;
        const SCAN_F13: u16 = 0x64;
        const SCAN_F14: u16 = 0x65;
        /// The driver's "I lost track of the keys" packet, not a key.
        const KEYBOARD_OVERRUN: u16 = 0xFF;

        // `RAWMOUSE::usButtonFlags`.
        const LEFT_DOWN: u16 = 0x0001;
        const LEFT_UP: u16 = 0x0002;
        const X1_DOWN: u16 = 0x0040;
        const X1_UP: u16 = 0x0080;
        const X2_DOWN: u16 = 0x0100;
        const X2_UP: u16 = 0x0200;
        const WHEEL: u16 = 0x0400;

        /// All six sided modifiers, which is the one place raw input would
        /// silently change the vocabulary if nobody normalised it: `VK_SHIFT` /
        /// `VK_CONTROL` / `VK_MENU` would land on `Key::Unknown(16|17|18)` and
        /// every `ShiftLeft`-style binding in every config would stop matching.
        #[test]
        fn generic_modifiers_are_sided_the_way_the_hook_sided_them() {
            let table = [
                // (vkey, make code, flags, key, config token)
                (VK_SHIFT, SCAN_LEFT_SHIFT, MAKE, Key::ShiftLeft, "ShiftLeft"),
                (
                    VK_SHIFT,
                    SCAN_RIGHT_SHIFT,
                    MAKE,
                    Key::ShiftRight,
                    "ShiftRight",
                ),
                (
                    VK_CONTROL,
                    SCAN_CONTROL,
                    MAKE,
                    Key::ControlLeft,
                    "ControlLeft",
                ),
                (
                    VK_CONTROL,
                    SCAN_CONTROL,
                    E0,
                    Key::ControlRight,
                    "ControlRight",
                ),
                (VK_MENU, SCAN_MENU, MAKE, Key::Alt, "Alt"),
                (VK_MENU, SCAN_MENU, E0, Key::AltGr, "AltGr"),
            ];

            for (vkey, make_code, flags, key, token) in table {
                assert_eq!(
                    raw_keyboard_event(vkey, make_code, flags),
                    Some(EventType::KeyPress(key)),
                    "vkey {vkey:#x} make {make_code:#x} flags {flags:#x}"
                );
                // And the side survives all the way to the config spelling.
                assert_eq!(Binding::Key(key).token().to_string(), token);
                assert_eq!(token.parse::<Binding>(), Ok(Binding::Key(key)));
            }
        }

        /// Ordinary keys are passed through untouched, sided or not.
        #[test]
        fn everything_else_flows_through_the_existing_table() {
            assert_eq!(
                raw_keyboard_event(VK_A, SCAN_A, MAKE),
                Some(EventType::KeyPress(Key::KeyA))
            );
            // Already-sided codes must not be re-sided into something else.
            assert_eq!(
                raw_keyboard_event(0xA1, SCAN_RIGHT_SHIFT, MAKE),
                Some(EventType::KeyPress(Key::ShiftRight))
            );
        }

        /// `RI_KEY_BREAK` is the only thing that says "release" — there is no
        /// separate message type any more.
        #[test]
        fn the_break_flag_is_what_makes_a_release() {
            assert_eq!(
                raw_keyboard_event(VK_F13, SCAN_F13, MAKE),
                Some(EventType::KeyPress(Key::Unknown(u32::from(VK_F13))))
            );
            assert_eq!(
                raw_keyboard_event(VK_F13, SCAN_F13, BREAK),
                Some(EventType::KeyRelease(Key::Unknown(u32::from(VK_F13))))
            );
            // The extended flag rides along with a break without confusing it.
            assert_eq!(
                raw_keyboard_event(VK_CONTROL, SCAN_CONTROL, BREAK | E0),
                Some(EventType::KeyRelease(Key::ControlRight))
            );
        }

        /// Microsoft's own sample drops both of these; a listener that turned
        /// them into keys would deliver presses nobody made.
        #[test]
        fn unmapped_keys_and_overrun_packets_are_discarded() {
            assert_eq!(raw_keyboard_event(0xFF, SCAN_A, MAKE), None);
            assert_eq!(raw_keyboard_event(0x100, SCAN_A, MAKE), None);
            assert_eq!(raw_keyboard_event(VK_A, KEYBOARD_OVERRUN, MAKE), None);
        }

        /// Both edges of both side buttons, in the exact spelling `Binding`
        /// parses `MouseX1`/`MouseX2` into.
        #[test]
        fn side_buttons_keep_their_numbers() {
            assert_eq!(
                raw_mouse_events(X1_DOWN),
                vec![EventType::ButtonPress(Button::Unknown(XBUTTON1))]
            );
            assert_eq!(
                raw_mouse_events(X1_UP),
                vec![EventType::ButtonRelease(Button::Unknown(XBUTTON1))]
            );
            assert_eq!(
                raw_mouse_events(X2_DOWN),
                vec![EventType::ButtonPress(Button::Unknown(XBUTTON2))]
            );
            assert_eq!(
                raw_mouse_events(X2_UP),
                vec![EventType::ButtonRelease(Button::Unknown(XBUTTON2))]
            );
        }

        /// One raw packet can carry several transitions at once — a click and
        /// its release in the same report, or two buttons together. All of them
        /// are decoded, presses first, or a consumer's held flag could latch
        /// the wrong way round.
        #[test]
        fn a_multi_flag_packet_decodes_presses_before_releases() {
            assert_eq!(
                raw_mouse_events(X1_UP | X1_DOWN),
                vec![
                    EventType::ButtonPress(Button::Unknown(XBUTTON1)),
                    EventType::ButtonRelease(Button::Unknown(XBUTTON1)),
                ]
            );
            assert_eq!(
                raw_mouse_events(LEFT_DOWN | X2_DOWN | LEFT_UP),
                vec![
                    EventType::ButtonPress(Button::Left),
                    EventType::ButtonPress(Button::Unknown(XBUTTON2)),
                    EventType::ButtonRelease(Button::Left),
                ]
            );
        }

        /// The common case by three orders of magnitude: a movement packet,
        /// which carries no button flag at all. Nothing is built for it.
        #[test]
        fn movement_and_wheel_packets_decode_to_nothing() {
            assert!(raw_mouse_events(0).is_empty());
            assert!(raw_mouse_events(WHEEL).is_empty());
        }

        /// The regression pin. Sotone's default bindings are F13 and F14, which
        /// rdev has no name for and which arrive as raw virtual-key codes; the
        /// side buttons arrive as flags. If any of the four ever decoded to
        /// something else, an existing config would silently stop working —
        /// so drive them from raw fields all the way through `Binding`'s own
        /// token parse and the listener's match.
        #[test]
        fn the_default_bindings_survive_the_trip_from_raw_fields_to_a_token() {
            let cases = [
                (raw_keyboard_event(VK_F13, SCAN_F13, MAKE), "F13"),
                (raw_keyboard_event(VK_F14, SCAN_F14, MAKE), "F14"),
                (raw_mouse_events(X1_DOWN).first().copied(), "MouseX1"),
                (raw_mouse_events(X2_DOWN).first().copied(), "MouseX2"),
            ];

            for (decoded, token) in cases {
                let event = decoded.expect("the packet must decode to a press");
                let binding = Binding::from_press(event).expect("the press must name a binding");
                assert_eq!(binding.token().to_string(), token);
                assert_eq!(token.parse::<Binding>(), Ok(binding));

                // And the listener recognises what capture just named, from a
                // press decoded out of raw fields.
                let mut filter = PttFilter::default();
                assert_eq!(
                    filter.apply(Bindings::ptt_only(binding), event, stamp()),
                    Some(PttEvent::Pressed),
                    "{token} decoded from a raw packet must drive the filter"
                );
            }
        }
    }
}
