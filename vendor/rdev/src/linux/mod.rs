extern crate libc;
extern crate x11;

// Sotone fork: no `grab`, no `simulate` — see README-SOTONE.md.
mod common;
mod display;
mod keyboard;
mod keycodes;
mod listen;

pub use crate::linux::display::display_size;
pub use crate::linux::keyboard::Keyboard;
pub use crate::linux::listen::{listen, listen_scoped};
