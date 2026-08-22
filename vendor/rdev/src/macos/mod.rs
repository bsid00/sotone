// Sotone fork: no `grab`, no `simulate` — see README-SOTONE.md.
mod common;
mod display;
mod keyboard;
mod keycodes;
mod listen;

pub use crate::macos::display::display_size;
pub use crate::macos::keyboard::Keyboard;
pub use crate::macos::listen::{listen, listen_scoped};
