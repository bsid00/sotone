//! Simple library to listen to keyboard and mouse events on MacOS, Windows and Linux
//! (x11).
//!
//! **This is Sotone's vendored fork of rdev 0.5.3.** The event-*sending* half of
//! upstream (`simulate`) and the event-*intercepting* half (`grab`,
//! `unstable_grab`) are deleted, not merely unused: Sotone promises it contains
//! no code path capable of generating or swallowing input. See
//! `README-SOTONE.md` for the full diff.
//!
//! # Listening to global events
//!
//! ```no_run
//! use rdev::{listen, Event};
//!
//! // This will block.
//! if let Err(error) = listen(callback) {
//!     println!("Error: {:?}", error)
//! }
//!
//! fn callback(event: Event) {
//!     println!("My callback {:?}", event);
//! }
//! ```
//!
//! ## OS Caveats:
//! When using the `listen` function, the following caveats apply:
//!
//! ## Mac OS
//! The process running the blocking `listen` function (loop) needs to be the parent process (no fork before).
//! The process needs to be granted access to the Accessibility API (ie. if you're running your process
//! inside Terminal.app, then Terminal.app needs to be added in
//! System Preferences > Security & Privacy > Privacy > Accessibility)
//! If the process is not granted access to the Accessibility API, MacOS will silently ignore rdev's
//! `listen` calleback and will not trigger it with events. No error will be generated.
//!
//! ## Linux
//! The `listen` function uses X11 APIs, and so will not work in Wayland or in the linux kernel virtual console
//!
//! # Main structs
//! ## Event
//!
//! `EventType` corresponds to a *physical* event, corresponding to QWERTY layout
//! `Event` corresponds to an actual event that was received.
//!
//! ```no_run
//! # use crate::rdev::EventType;
//! # use std::time::SystemTime;
//! /// When events arrive from the system we can add some information
//! /// time is when the event was received.
//! #[derive(Debug)]
//! pub struct Event {
//!     pub time: SystemTime,
//!     pub name: Option<String>,
//!     pub event_type: EventType,
//! }
//! ```
//!
//! Note: in this fork `Event::name` is always `None` on Windows — see
//! `windows/listen.rs` for why the OS-level character translation was removed
//! from the hook callback.
//!
//! ## EventType
//!
//! In order to manage different OS, the current EventType choices is a mix&match
//! to account for all possible events.
//! There is a safe mechanism to detect events no matter what, which are the
//! Unknown() variant of the enum which will contain some OS specific value.
//!
//! ```no_run
//! # use crate::rdev::{Key, Button};
//! /// In order to manage different OS, the current EventType choices is a mix&match
//! /// to account for all possible events.
//! #[derive(Debug)]
//! pub enum EventType {
//!     /// The keys correspond to a standard qwerty layout, they don't correspond
//!     /// To the actual letter a user would use, that requires some layout logic to be added.
//!     KeyPress(Key),
//!     KeyRelease(Key),
//!     /// Some mouse will have more than 3 buttons, these are not defined, and different OS will
//!     /// give different Unknown code.
//!     ButtonPress(Button),
//!     ButtonRelease(Button),
//!     /// Values in pixels
//!     MouseMove {
//!         x: f64,
//!         y: f64,
//!     },
//!     /// Note: On Linux, there is no actual delta, the actual values are ignored for delta_x
//!     /// and we only look at the sign of delta_y.
//!     Wheel {
//!         delta_x: i64,
//!         delta_y: i64,
//!     },
//! }
//! ```
//!
//!
//! # Getting the main screen size
//!
//! ```no_run
//! use rdev::{display_size};
//!
//! let (w, h) = display_size().unwrap();
//! assert!(w > 0);
//! assert!(h > 0);
//! ```
//!
//! # Keyboard state
//!
//! We can define a dummy Keyboard, that we will use to detect
//! what kind of EventType trigger some String. We get the currently used
//! layout for now !
//! Caveat : This is layout dependent. If your app needs to support
//! layout switching don't use this !
//! Caveat: On Linux, the dead keys mechanism is not implemented.
//! Caveat: Only shift and dead keys are implemented, Alt+unicode code on windows
//! won't work.
//!
//! ```no_run
//! use rdev::{Keyboard, EventType, Key, KeyboardState};
//!
//! let mut keyboard = Keyboard::new().unwrap();
//! let string = keyboard.add(&EventType::KeyPress(Key::KeyS));
//! // string == Some("s")
//! ```
//!
//! # Serialization
//!
//! Event data returned by the `listen` function can be serialized and de-serialized with
//! Serde if you install this library with the `serialize` feature.
mod rdev;
pub use crate::rdev::{
    Button, DisplayError, Event, EventType, HookScope, Key, KeyboardState, ListenError,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use crate::macos::Keyboard;
#[cfg(target_os = "macos")]
use crate::macos::{display_size as _display_size, listen as _listen, listen_scoped as _scoped};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use crate::linux::Keyboard;
#[cfg(target_os = "linux")]
use crate::linux::{display_size as _display_size, listen as _listen, listen_scoped as _scoped};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use crate::windows::Keyboard;
#[cfg(target_os = "windows")]
use crate::windows::{display_size as _display_size, listen as _listen, listen_scoped as _scoped};
/// Sotone fork (2026-08-16, patch 4): the Windows listener decodes Raw Input,
/// and these are the two pure functions it decodes with — no OS state, no
/// window, no device. They are public so the vocabulary they produce (which
/// `Key` a virtual-key code means, which `Button` a side-button flag means) can
/// be pinned by unit tests instead of only by pressing keys.
#[cfg(target_os = "windows")]
pub use crate::windows::{raw_keyboard_event, raw_mouse_events};

/// Listening to global events. Caveat: On MacOS, you require the listen
/// loop needs to be the primary app (no fork before) and need to have accessibility
/// settings enabled.
///
/// ```no_run
/// use rdev::{listen, Event};
///
/// fn callback(event: Event) {
///     println!("My callback {:?}", event);
/// }
/// fn main(){
///     // This will block.
///     if let Err(error) = listen(callback) {
///         println!("Error: {:?}", error)
///     }
/// }
/// ```
pub fn listen<T>(callback: T) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    _listen(callback)
}

/// Listening to global events, but only to the hooks the caller actually needs.
///
/// Sotone fork (2026-08-12, patch 3), additive: `listen(cb)` is exactly
/// `listen_scoped(cb, HookScope::KeyboardAndMouse)`, so nothing that already
/// names `listen` changes behaviour. Passing [`HookScope::Keyboard`] installs
/// no mouse hook, which is the only way to keep the process off the OS mouse
/// path — see [`HookScope`] for what that costs when the hook *is* installed.
///
/// ```no_run
/// use rdev::{listen_scoped, Event, HookScope};
///
/// fn callback(event: Event) {
///     println!("My callback {:?}", event);
/// }
/// fn main(){
///     // This will block.
///     if let Err(error) = listen_scoped(callback, HookScope::Keyboard) {
///         println!("Error: {:?}", error)
///     }
/// }
/// ```
pub fn listen_scoped<T>(callback: T, scope: HookScope) -> Result<(), ListenError>
where
    T: FnMut(Event) + 'static,
{
    _scoped(callback, scope)
}

/// Returns the size in pixels of the main screen.
/// This is useful to use with x, y from MouseMove Event.
///
/// ```no_run
/// use rdev::{display_size};
///
/// let (w, h) = display_size().unwrap();
/// println!("My screen size : {:?}x{:?}", w, h);
/// ```
pub fn display_size() -> Result<(u64, u64), DisplayError> {
    _display_size()
}
