//! `reconnect_probe` — does `AudioEngine::reconnect` leave a working stream?
//!
//! The microphone is live: changing it tears the cpal stream down
//! and opens the new device, keeping the worker thread and — the part that
//! matters — the *same* utterance channel. That is exactly the kind of claim a
//! unit test cannot check, because it needs a real device, and exactly the kind
//! a script can: capture, reconnect, capture again, compare.
//!
//! So it does three things and judges them by exit code:
//!
//! 1. opens the engine on the default microphone (or the first argument as a
//!    name substring) and records a short utterance,
//! 2. reconnects to the same device and prints the outcome,
//! 3. records a second utterance **on the receiver it was given at the start** —
//!    the one handed back by `AudioEngine::start`, never re-fetched.
//!
//! Exit 0 means the reconnect switched *and* both captures produced samples,
//! i.e. the second stream is really feeding the same channel. Anything else is
//! exit 1 with the reason printed.
//!
//! What it cannot judge: whether the audio is any *good*. Sample counts prove
//! the pipeline is connected; a human with a voice proves the rest.
//!
//! No stdin and no hotkey: capture is driven through the engine's own
//! `begin_utterance` / `end_utterance`, the same calls the control thread makes,
//! so nothing here reads or produces input events (invariant 1). It terminates
//! on its own and leaves nothing running.
//!
//! Run with:
//! `cargo run --release -p sotone-core --example reconnect_probe -- [mic substring]`

use std::error::Error;
use std::process::ExitCode;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use sotone_core::audio::{AudioEngine, EngineStatus, Reconnect, Utterance};

/// How long each capture runs. Comfortably longer than the worker's 100 ms
/// drain pass, so a capture that produces nothing is a real failure and not a
/// race with the poll interval.
const CAPTURE: Duration = Duration::from_millis(700);

/// Time given to a freshly opened stream before it is asked for audio. The
/// first few hundred milliseconds after a WASAPI open are unreliable — that is
/// the whole reason opening a device per keypress is forbidden — and this probe
/// is testing the reconnect, not that rule.
const SETTLE: Duration = Duration::from_millis(400);

/// Long enough that a slow first resample cannot be mistaken for a hang.
const UTTERANCE_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    // Not `?` from `main`: Rust's termination path prints errors with `Debug`,
    // and every message in this crate is written to be read as `Display`.
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            let mut source = err.source();
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // The same substring for both opens: reconnecting to the device already in
    // use is the honest test of the teardown-then-open order, because a host
    // that hands the device out exclusively would refuse the second open and
    // that is precisely what this probe should catch.
    let mic = std::env::args().nth(1);

    let (mut engine, utterances) = AudioEngine::start(mic.as_deref())?;
    println!(
        "opened: {} ({} Hz, {} ch native)",
        engine.device_name(),
        engine.input_sample_rate(),
        engine.input_channels()
    );

    let before = capture(&engine, &utterances, "before")?;
    report(&engine);

    println!("reconnecting to {:?}...", mic.as_deref());
    let outcome = engine.reconnect(mic.as_deref());
    println!("  outcome: {}", describe(&outcome));
    println!(
        "  now open: {} ({} Hz, {} ch native)",
        engine.device_name(),
        engine.input_sample_rate(),
        engine.input_channels()
    );
    report(&engine);

    // The receiver is the original one. If a reconnect had replaced the
    // channel, this call would block until the timeout — which is the failure
    // this probe exists to make visible.
    let after = capture(&engine, &utterances, "after")?;
    report(&engine);

    // The verdict, spelled out rather than implied by the exit code alone.
    if !matches!(outcome, Reconnect::Switched { .. }) {
        return Err(format!("the reconnect did not switch: {}", describe(&outcome)).into());
    }
    if before == 0 || after == 0 {
        return Err(format!(
            "a capture produced no samples (before: {before}, after: {after}) — the utterance \
             channel is not being fed"
        )
        .into());
    }

    println!("PASS: switched, and both captures produced samples ({before} then {after})");
    Ok(())
}

/// Drive one utterance the way the control thread does, and count what came
/// back.
///
/// A missing utterance is a failure of the probe's whole point, so it is an
/// error rather than a printed shrug: the channel either delivered or it did
/// not.
fn capture(
    engine: &AudioEngine,
    utterances: &Receiver<Utterance>,
    label: &str,
) -> Result<usize, Box<dyn Error>> {
    thread::sleep(SETTLE);

    println!("capturing ({label})...");
    engine.begin_utterance();
    thread::sleep(CAPTURE);
    engine.end_utterance();

    match utterances.recv_timeout(UTTERANCE_TIMEOUT) {
        Ok(utterance) => {
            let peak = utterance
                .samples
                .iter()
                .fold(0.0f32, |acc, sample| acc.max(sample.abs()));
            println!(
                "  {} samples, {:.2} s, peak {peak:.3}",
                utterance.samples.len(),
                utterance.duration().as_secs_f32()
            );
            Ok(utterance.samples.len())
        }
        Err(err) => Err(format!("no utterance arrived {label} the reconnect: {err}").into()),
    }
}

/// One reconnect outcome as a line a script's output can be read for.
fn describe(outcome: &Reconnect) -> String {
    match outcome {
        Reconnect::Switched { device } => format!("Switched to {device}"),
        Reconnect::Reverted { device, error } => {
            format!("Reverted to {device} ({error})")
        }
        Reconnect::Lost {
            error,
            revert_error,
        } => format!("Lost: {error}; the revert also failed: {revert_error}"),
    }
}

fn report(engine: &AudioEngine) {
    match engine.status() {
        EngineStatus::Running => println!("  status: running, overruns: {}", engine.overruns()),
        EngineStatus::Dead { reason } => println!("  status: DEAD ({reason})"),
    }
}
