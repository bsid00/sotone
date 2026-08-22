extern crate winapi;

// Sotone fork: no `grab`, no `simulate` — see README-SOTONE.md.
//
// `common` is gone too (2026-08-16, patch 4): every item in it was
// `WH_KEYBOARD_LL`/`WH_MOUSE_LL` plumbing — the `SetWindowsHookExA` installers,
// the `HHOOK` global and the `KBDLLHOOKSTRUCT`/`MSLLHOOKSTRUCT` decode. Its one
// caller was `listen`, which now decodes raw input in `raw` instead.
mod display;
mod keyboard;
mod keycodes;
mod listen;
mod raw;

pub use crate::windows::display::display_size;
pub use crate::windows::keyboard::Keyboard;
pub use crate::windows::listen::{listen, listen_scoped};
// The two pure decode functions. Public because they are the only part of the
// raw-input backend a unit test can reach without a device, a window or a
// thread, and Sotone's suite pins the key/button vocabulary through them.
pub use crate::windows::raw::{raw_keyboard_event, raw_mouse_events};
