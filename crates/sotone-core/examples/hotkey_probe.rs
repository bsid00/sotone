//! `hotkey_probe` — the human's runtime harness for the input hook.
//!
//! Binds a push-to-talk token (default `F13`) and a toggle token (default
//! `F14`), then prints a line per hotkey event with its timestamp. It answers
//! the questions a unit test cannot: does the mouse driver's F13 actually reach
//! us, do the side buttons arrive as X1/X2 on *this* mouse, and does anything
//! still arrive while a fullscreen game or an elevated window has focus.
//!
//! Pass `-` for either token to leave that mode unbound, which is how the
//! `--no-ptt` / `--no-toggle` cases are exercised.
//!
//! It only ever *listens*. Nothing here — and nothing anywhere in Sotone —
//! generates a keystroke or a mouse event, so the app under test sees exactly
//! the input the human gave it.
//!
//! Press Enter to quit. The listener threads are detached by design (rdev's
//! hook cannot be uninstalled) and die with the process.
//!
//! Run with:
//! `cargo run --release -p sotone-core --features vulkan --example hotkey_probe -- [ptt] [toggle]`

use std::error::Error;
use std::io::{self, BufRead};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use sotone_core::config::{DEFAULT_HOTKEY, DEFAULT_TOGGLE_HOTKEY};
use sotone_core::hotkey::{Binding, Bindings, HotkeyListener, PttEvent};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    // The config defaults, so the probe tests what a fresh install uses.
    let ptt_token = args.next().unwrap_or_else(|| DEFAULT_HOTKEY.to_owned());
    let toggle_token = args
        .next()
        .unwrap_or_else(|| DEFAULT_TOGGLE_HOTKEY.to_owned());

    let bindings = Bindings {
        ptt: parse(&ptt_token),
        toggle: parse(&toggle_token),
    };
    describe("Push-to-talk", bindings.ptt, &ptt_token);
    describe("Toggle", bindings.toggle, &toggle_token);

    // The handler runs on the listener's consumer thread, so it must not block.
    // Handing off to this channel and printing from main is the cheap way to
    // keep that true even though printing to a console is not cheap.
    let (tx, rx) = mpsc::channel();
    let listener = HotkeyListener::spawn(bindings, move |event| {
        let _ = tx.send(event);
    })?;

    println!(
        "Watching {} binding(s). Press Enter to quit.\n",
        usize::from(listener.bindings().ptt.is_some())
            + usize::from(listener.bindings().toggle.is_some())
    );

    // A second thread drains the events; the main thread waits on stdin so the
    // probe exits the moment the human asks, whatever the hotkey is doing.
    std::thread::spawn(move || {
        let mut count = 0u32;
        for event in rx {
            match event {
                PttEvent::Pressed => {
                    count += 1;
                    println!("#{count:03} PRESSED   at {}", now());
                }
                PttEvent::Released { time } => {
                    println!("      RELEASED  at {} (from the hook)", stamp(time));
                }
                PttEvent::Toggled { time } => {
                    count += 1;
                    // Start or stop is the app's state, not the hook's: this
                    // probe only proves the press arrived.
                    println!("#{count:03} TOGGLED   at {} (from the hook)", stamp(time));
                }
                PttEvent::HookLost { reason } => {
                    println!("      HOOK LOST: {reason}");
                    println!("      No further events will arrive. Press Enter to quit.");
                    return;
                }
            }
        }
    });

    let stdin = io::stdin();
    let _ = stdin.lock().lines().next();
    println!("Stopping.");
    Ok(())
}

/// Parse one token, or `None` for `-` (that mode switched off).
///
/// Not `?`: Rust's termination path prints the error with `Debug`, and the
/// whole point of `BindingError`'s message is that it tells the user which
/// spellings exist. This is the same text the app will show when
/// `config.hotkey` does not parse, so the probe must show it verbatim.
fn parse(token: &str) -> Option<Binding> {
    if token == "-" {
        return None;
    }
    match token.parse() {
        Ok(binding) => Some(binding),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

/// One line naming what a mode ended up bound to.
fn describe(label: &str, binding: Option<Binding>, token: &str) {
    match binding {
        Some(binding) => println!("{label}: {binding} (parsed from {token:?})"),
        None => println!("{label}: (disabled)"),
    }
}

/// Wall clock now, for comparing against the hook's own timestamp by eye.
fn now() -> String {
    stamp(SystemTime::now())
}

/// A `SystemTime` as seconds-and-milliseconds since the epoch.
///
/// Deliberately not `chrono`: this harness has no business pulling a date
/// library in, and the only thing being read here is the delta between the two
/// numbers on screen.
fn stamp(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => format!("{}.{:03}", since.as_secs(), since.subsec_millis()),
        Err(_) => "before the epoch".to_owned(),
    }
}
