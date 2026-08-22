//! `cue_probe` — does an output stream coexist with the microphone stream?
//!
//! Opens the capture engine, opens the cue player, plays all three cues with
//! gaps, and exits. It answers the one question that is scriptable: whether
//! WASAPI shared mode on this machine will hand us an independent output stream
//! while an input stream is live, and whether the cpal error callback fires
//! while both are running. Exit 0 means the output stream opened, stayed open
//! and reported nothing; exit 1 means it did not.
//!
//! Whether the cues *sound* right — audible over a game, not startling, not
//! mistakable for each other — is the human's half, and no exit code can stand
//! in for it.
//!
//! Terminates on its own. No stdin, no loop, nothing to leave running.
//!
//! Run with:
//! `cargo run --release -p sotone-core --features vulkan --example cue_probe -- [mic substring]`

use std::error::Error;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use sotone_core::audio::{AudioEngine, EngineStatus};
use sotone_core::cue::{Cue, CuePlayer};

/// Silence between cues, so a human listening can tell them apart and a stuck
/// cue is obvious.
const GAP: Duration = Duration::from_millis(500);

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

    let mic = std::env::args().nth(1);

    // The input stream comes first deliberately: the question is whether the
    // output stream can be opened *while the microphone is already live*, which
    // is the order the app itself uses.
    let (engine, _utterances) = AudioEngine::start(mic.as_deref())?;
    println!(
        "Input:  {} ({} Hz, {} ch)",
        engine.device_name(),
        engine.input_sample_rate(),
        engine.input_channels()
    );

    let player = CuePlayer::start()?;
    println!(
        "Output: {} ({} Hz, {} ch)",
        player.device_name(),
        player.sample_rate(),
        player.channels()
    );
    println!();

    for (name, cue) in [
        ("begin", Cue::Begin),
        ("saved", Cue::Saved),
        ("error", Cue::Error),
    ] {
        println!("playing {name} ({} ms)", cue.duration().as_millis());
        player.play(cue);
        thread::sleep(cue.duration() + GAP);
    }

    // The error callback is asynchronous; give it the same grace the last cue
    // got before deciding nothing went wrong.
    thread::sleep(GAP);

    if let Some(reason) = player.failure() {
        return Err(format!("the cue output stream failed: {reason}").into());
    }
    if let EngineStatus::Dead { reason } = engine.status() {
        return Err(format!("the microphone stream died during the probe: {reason}").into());
    }

    println!("\nOK: input and output streams coexisted, no cpal error fired.");
    println!("Overruns on the input ring: {}", engine.overruns());
    Ok(())
}
