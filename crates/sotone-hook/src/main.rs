//! `sotone-hook` — Sotone's global input hook, in a process of its own.
//!
//! ```text
//! sotone-hook <push-to-talk binding|-> <toggle binding|->
//! sotone-hook --capture
//! ```
//!
//! # Why this exists
//!
//! Sotone used to install the low-level keyboard hook in its own process. With
//! Sotone's WebView2 window in the foreground, and no other process holding an
//! LL hook, that hook starves: keys vanish with no cue and no error
//! (tauri-apps/tauri#14770, reproduced in both directions on a real
//! machine). The only confirmed fix is to own the hook somewhere that has no
//! WebView2 window in it. That is this binary, and it does nothing else.
//!
//! # Behaviour
//!
//! * Two positional arguments in the config's own binding vocabulary (`F13`,
//!   `MouseX1`, `Space`, …), `-` for a mode that is switched off. A token
//!   neither of those exits 2 with one line on stderr.
//! * Every observed event goes out on stdout as one JSON line
//!   ([`sotone_core::hotkey::wire`]), **flushed immediately**: piped stdout on
//!   Windows is block-buffered, and an unflushed push-to-talk press is a lost
//!   recording rather than a late one.
//! * `HookLost` is written like any other event and then exits 1, so the parent
//!   can tell "the hook died" from "the helper died".
//!
//! # `--capture`: one press, then gone
//!
//! Settings rebinds by asking the user to press the key or button — nobody
//! types virtual key codes. That needs a hook, and an in-process hook
//! in the app is forbidden for good — so it happens here, in a second, briefly
//! lived helper:
//!
//! * install the hook, report the **first** press that names a binding as one
//!   `captured` line, flush, exit 0;
//! * a press that names no binding — a mouse move, a release, a key rdev has
//!   no name for — is ignored and capture continues, because the alternative is
//!   writing a `hotkey` the next launch cannot read;
//! * the lifetime rule is unchanged (stdin EOF kills it), and a hook that dies
//!   still writes `hook_lost` and exits 1, so a capture can never leave the
//!   parent waiting forever on a process that has given up.
//!
//! The two modes are exclusive: a capture helper never watches bindings and a
//! binding helper never captures.
//!
//! # Lifetime is the pipe
//!
//! A thread blocks reading stdin, which the parent holds open and never writes
//! to. EOF — the parent exited, crashed, or was killed — exits this process
//! immediately. A helper that outlives its parent would be an invisible process
//! holding a global hook and a lock on its own exe, which is exactly the stale
//! `hotkey_probe.exe` that once broke a release link.
//!
//! # Invariants
//!
//! * **1 — no synthetic input.** This process only *observes*. `simulate` and
//!   `grab` are deleted from the vendored rdev, so there is nothing here that
//!   could generate a keystroke even by mistake.
//! * **3 — nothing leaves the machine.** stdin/stdout are anonymous pipes to
//!   the parent. No socket is opened, no address is resolved.
//! * **5 — never block the input hook.** The rdev callback (inside `sotone-core`)
//!   sends and returns; the filtering happens on its consumer thread; the pipe
//!   writes happen here, on the main thread, behind an unbounded channel. No
//!   `write`, no `flush` and no allocation for JSON ever happens inside the OS
//!   hook callback.

use std::io::{self, Read, Write};
use std::process;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use sotone_core::hotkey::wire::HookMessage;
use sotone_core::hotkey::{
    capture_binding, Binding, Bindings, CaptureEvent, HotkeyListener, PttEvent,
};

/// The token for "this mode is switched off".
const UNBOUND: &str = "-";

/// The one-shot rebinding mode.
const CAPTURE_FLAG: &str = "--capture";

/// One line, printed on stderr — which the parent inherits — before exit 2.
const USAGE: &str = "usage: sotone-hook <push-to-talk binding|-> <toggle binding|->\n       \
                     sotone-hook --capture";

/// Bad invocation, or a hook that could not even be started.
const EXIT_USAGE: i32 = 2;
/// The hook was installed and then died. Reported as a message first.
const EXIT_HOOK_LOST: i32 = 1;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if is_capture(&args) {
        capture();
    }

    let bindings = match parse_bindings(&args) {
        Ok(bindings) => bindings,
        Err(err) => fail(&err),
    };

    // Started before the hook, so a parent that dies during startup is still
    // noticed.
    if let Err(err) = thread::Builder::new()
        .name("sotone-hook-parent".to_owned())
        .spawn(watch_parent)
    {
        fail(&format!("could not watch the parent process: {err}"));
    }

    let (tx, rx) = mpsc::channel::<PttEvent>();
    // The handler runs on sotone-core's consumer thread, not in the hook
    // callback; it does one send and returns (invariant 5).
    let _listener = match HotkeyListener::spawn(bindings, move |event| {
        let _ = tx.send(event);
    }) {
        Ok(listener) => listener,
        Err(err) => fail(&format!("could not install the hotkey hook: {err}")),
    };

    write_events(&rx);
}

/// One line on stderr and out, for the failures that happen before there is
/// anything to report on the wire.
fn fail(message: &str) -> ! {
    eprintln!("sotone-hook: {message}");
    process::exit(EXIT_USAGE)
}

/// Is this the one-shot capture invocation?
fn is_capture(args: &[String]) -> bool {
    args.len() == 1 && args[0] == CAPTURE_FLAG
}

/// `--capture`: watch for one press, say what it was, and stop.
///
/// Never returns. The parent shuts this down by closing stdin (cancel) or by
/// letting it exit on its own after the one line — either way there is no
/// second press to report and no reason to keep a global hook installed.
fn capture() -> ! {
    if let Err(err) = thread::Builder::new()
        .name("sotone-hook-parent".to_owned())
        .spawn(watch_parent)
    {
        fail(&format!("could not watch the parent process: {err}"));
    }

    let (tx, rx) = mpsc::channel::<CaptureEvent>();
    // Same discipline as the binding mode: the handler runs on sotone-core's
    // consumer thread, does one send, and returns (invariant 5).
    if let Err(err) = capture_binding(move |event| {
        let _ = tx.send(event);
    }) {
        fail(&format!("could not install the hotkey hook: {err}"));
    }

    let message = match rx.recv() {
        // The token, not a key code: the parent parses it back through the same
        // vocabulary the config file uses.
        Ok(CaptureEvent::Captured(binding)) => HookMessage::Captured {
            token: binding.to_string(),
        },
        Ok(CaptureEvent::HookLost { reason }) => HookMessage::HookLost { reason },
        // The consumer thread is gone without a word. Nothing to report but the
        // fact itself — the parent must never be left waiting on a capture that
        // will not arrive.
        Err(err) => HookMessage::HookLost {
            reason: format!("the capture hook stopped before any press was seen: {err}"),
        },
    };

    let lost = matches!(message, HookMessage::HookLost { .. });
    match message.to_line() {
        Ok(line) => {
            let mut out = io::stdout().lock();
            // Flushed for the same reason every other line is: piped stdout on
            // Windows is block-buffered, and an unflushed capture is a rebind
            // the user thinks did nothing.
            if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                process::exit(0);
            }
        }
        Err(err) => eprintln!("sotone-hook: could not encode the capture: {err}"),
    }

    process::exit(if lost { EXIT_HOOK_LOST } else { 0 })
}

/// The writer loop: one JSON line per event, flushed, forever.
///
/// This is the whole job of the main thread once the hook is up.
fn write_events(rx: &Receiver<PttEvent>) -> ! {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for event in rx {
        let lost = matches!(event, PttEvent::HookLost { .. });
        match HookMessage::from(event).to_line() {
            Ok(line) => {
                // A write error means the parent closed its read end, i.e. the
                // parent is gone. Nothing left to report to.
                if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                    process::exit(0);
                }
            }
            Err(err) => eprintln!("sotone-hook: could not encode an event: {err}"),
        }
        if lost {
            // Said out loud first, so the parent shows the reason rather than
            // inferring one from an exit code.
            process::exit(EXIT_HOOK_LOST);
        }
    }

    // rdev parks the listen callback — and a clone of the sender — in a
    // process-global it never clears, so the channel cannot actually disconnect
    // and this is unreachable in practice. If it ever is reached, the hook is
    // not coming back and there is no reason to linger.
    process::exit(0)
}

/// Block on stdin until the parent goes away, then take this process with it.
///
/// The parent never writes; any bytes that do arrive are ignored rather than
/// treated as a protocol, because this pipe has exactly one meaning: open or
/// closed.
fn watch_parent() {
    let mut stdin = io::stdin();
    let mut scratch = [0_u8; 64];
    loop {
        match stdin.read(&mut scratch) {
            // EOF, or a broken pipe: the parent exited, crashed or was killed.
            Ok(0) | Err(_) => process::exit(0),
            Ok(_) => {}
        }
    }
}

/// Parse the two positional binding tokens.
///
/// The vocabulary is deliberately the config's own — the parent renders the
/// bindings it parsed straight back into these arguments, so there is exactly
/// one spelling of a hotkey in the whole system.
fn parse_bindings(args: &[String]) -> Result<Bindings, String> {
    let [ptt, toggle] = args else {
        return Err(format!(
            "expected exactly two arguments, got {}. {USAGE}",
            args.len()
        ));
    };

    let bindings = Bindings {
        ptt: parse_slot("push-to-talk", ptt)?,
        toggle: parse_slot("toggle", toggle)?,
    };

    if bindings.is_empty() {
        return Err(format!(
            "both bindings are \"{UNBOUND}\", which leaves nothing to watch. {USAGE}"
        ));
    }
    Ok(bindings)
}

/// One argument: a binding, or `-` for a mode that is switched off.
fn parse_slot(mode: &str, token: &str) -> Result<Option<Binding>, String> {
    if token == UNBOUND {
        return Ok(None);
    }
    token
        .parse::<Binding>()
        .map(Some)
        .map_err(|err| format!("{mode}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests drive the argument parser only. Nothing here spawns a hook,
    /// touches stdin/stdout, or starts a process.
    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn two_tokens_bind_both_modes() {
        let bindings = parse_bindings(&args(&["F13", "F14"])).expect("both tokens are valid");
        assert_eq!(
            bindings,
            Bindings::new("F13".parse().expect("F13"), "F14".parse().expect("F14"))
        );
    }

    #[test]
    fn a_dash_switches_a_mode_off() {
        assert_eq!(
            parse_bindings(&args(&["-", "MouseX2"])).expect("toggle only"),
            Bindings::toggle_only(Binding::MouseX2)
        );
        assert_eq!(
            parse_bindings(&args(&["MouseX1", "-"])).expect("push-to-talk only"),
            Bindings::ptt_only(Binding::MouseX1)
        );
    }

    /// The parent rejects this at load time; the helper is the backstop, and a
    /// listener that could never fire must not look like it started fine.
    #[test]
    fn both_unbound_is_rejected() {
        let err = parse_bindings(&args(&["-", "-"])).expect_err("nothing to watch");
        assert!(err.contains(USAGE));
    }

    #[test]
    fn garbage_names_the_offending_mode_and_token() {
        let err = parse_bindings(&args(&["Frobnicate", "F14"])).expect_err("must not parse");
        assert!(err.contains("push-to-talk"), "{err}");
        assert!(err.contains("Frobnicate"), "{err}");

        let err = parse_bindings(&args(&["F13", "Unknown(124)"])).expect_err("must not parse");
        assert!(err.contains("toggle"), "{err}");
    }

    /// The two modes are exclusive, and the capture flag is recognised only on
    /// its own: `sotone-hook --capture F13` is a mistake, not a capture.
    #[test]
    fn capture_is_its_own_invocation() {
        assert!(is_capture(&args(&["--capture"])));
        for tokens in [
            vec![],
            vec!["F13", "F14"],
            vec!["--capture", "F13"],
            vec!["capture"],
            vec!["--Capture"],
        ] {
            assert!(!is_capture(&args(&tokens)), "{tokens:?}");
        }
        // And it is not a binding token either, so the ordinary path rejects it
        // by name rather than treating it as a key.
        let err = parse_bindings(&args(&["--capture", "F14"])).expect_err("not a binding");
        assert!(err.contains("--capture"), "{err}");
    }

    #[test]
    fn the_wrong_number_of_arguments_is_rejected() {
        for tokens in [vec![], vec!["F13"], vec!["F13", "F14", "F15"]] {
            let err = parse_bindings(&args(&tokens)).expect_err("wrong arity");
            assert!(err.contains(USAGE), "{err}");
        }
    }
}
