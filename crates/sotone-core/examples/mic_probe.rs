//! `mic_probe` — the human's runtime harness for the audio pipeline.
//!
//! Lists input devices, opens the engine, and records utterances on Enter,
//! writing each one to `probe-NNN.wav` (16 kHz mono) in the current directory
//! so the pre-roll can actually be listened to.
//!
//! It also keeps the untouched native capture and writes it to
//! `probe-NNN-native.wav`, and prints per-channel levels and inter-channel
//! correlation. That pair answers the only question worth asking about a bad
//! recording: was it already bad when it left the device, or did we break it?
//!
//! Input here is `stdin`. There is deliberately no `rdev` and no hook: this
//! crate contains no code that reads or produces keyboard events, and the
//! hotkey lives elsewhere.
//!
//! Run with:
//! `cargo run --release -p sotone-core --features vulkan --example mic_probe -- [mic substring]`

use std::error::Error;
use std::io::{self, BufRead, Write};
use std::time::Duration;

use sotone_core::audio::{
    channel_stats, list_input_devices, AudioEngine, CaptureOptions, EngineStatus,
};

/// Long enough that a slow first resample cannot be mistaken for a hang.
const UTTERANCE_TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> Result<(), Box<dyn Error>> {
    let wanted = std::env::args().nth(1);
    let needle = wanted.as_deref().map(str::to_lowercase);

    println!("Input devices:");
    for device in list_input_devices()? {
        let default = if device.is_default { " [default]" } else { "" };
        let matched = match &needle {
            Some(n) if device.name.to_lowercase().contains(n) => " <- matches argument",
            _ => "",
        };
        println!("  {}{default}{matched}", device.name);
    }
    println!();

    // The whole point of this harness is diagnosing the pipeline, so it pays the
    // memory for the native capture that production deliberately does not.
    let (engine, utterances) = AudioEngine::start_with_options(
        wanted.as_deref(),
        CaptureOptions {
            retain_native: true,
        },
    )?;
    println!(
        "Recording from: {} ({} Hz, {} ch native)",
        engine.device_name(),
        engine.input_sample_rate(),
        engine.input_channels()
    );
    report(&engine);
    println!("Enter starts an utterance, Enter again ends it. 'q' then Enter quits.\n");

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut index = 0u32;

    loop {
        if !prompt("Enter to START (or q to quit): ", &mut lines)? {
            break;
        }
        engine.begin_utterance();

        if !prompt("  recording... Enter to STOP: ", &mut lines)? {
            engine.end_utterance();
            break;
        }
        engine.end_utterance();

        match utterances.recv_timeout(UTTERANCE_TIMEOUT) {
            Ok(utterance) => {
                index += 1;
                let path = format!("probe-{index:03}.wav");
                let peak = utterance
                    .samples
                    .iter()
                    .fold(0.0f32, |acc, s| acc.max(s.abs()));
                println!(
                    "  {} samples, {:.2} s, peak {peak:.3}",
                    utterance.samples.len(),
                    utterance.duration().as_secs_f32()
                );
                println!(
                    "  normalize gain: x{:.2} ({})",
                    utterance.normalize_gain,
                    decibels(utterance.normalize_gain)
                );
                write_wav(&path, &utterance.samples, utterance.sample_rate, 1)?;
                println!("  wrote {path}");

                if let Some(native) = &utterance.native {
                    report_native(&native.samples, native.sample_rate, native.channels);
                    let native_path = format!("probe-{index:03}-native.wav");
                    write_wav(
                        &native_path,
                        &native.samples,
                        native.sample_rate,
                        native.channels,
                    )?;
                    println!("  wrote {native_path}");
                }
            }
            Err(err) => println!("  no utterance arrived: {err}"),
        }

        report(&engine);
        println!();
    }

    println!("Stopping.");
    Ok(())
}

/// Prints a prompt and waits for a line. Returns false when the user quits or
/// stdin closes.
fn prompt(text: &str, lines: &mut io::Lines<io::StdinLock<'_>>) -> Result<bool, Box<dyn Error>> {
    print!("{text}");
    io::stdout().flush()?;
    match lines.next() {
        Some(line) => Ok(!line?.trim().eq_ignore_ascii_case("q")),
        None => Ok(false),
    }
}

fn report(engine: &AudioEngine) {
    match engine.status() {
        EngineStatus::Running => println!("  status: running, overruns: {}", engine.overruns()),
        EngineStatus::Dead { reason } => println!("  status: DEAD ({reason})"),
    }
}

/// Prints what the device actually delivered, before any of our processing.
fn report_native(samples: &[f32], sample_rate: u32, channels: u16) {
    let stats = channel_stats(samples, channels);
    println!(
        "  native: {sample_rate} Hz, {channels} ch, {} frames",
        stats.frames
    );
    for (channel, (peak, rms)) in stats.peaks.iter().zip(&stats.rms).enumerate() {
        println!(
            "    ch{channel}: peak {peak:.4} rms {rms:.4} ({})",
            decibels(*rms)
        );
    }
    if let Some(correlation) = stats.correlation {
        // The reading that matters: |r| near 1 at a nonzero lag means channel
        // two is a delayed copy, and averaging the two comb-filters the speech.
        println!(
            "    inter-channel: r = {:+.3} at lag {:+} samples",
            correlation.coefficient, correlation.lag
        );
    }
}

/// A linear ratio as dB, for eyes used to reading levels that way.
fn decibels(ratio: f32) -> String {
    if ratio <= 0.0 {
        return "-inf dB".to_owned();
    }
    format!("{:+.1} dB", 20.0 * ratio.log10())
}

fn write_wav(
    path: &str,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<(), Box<dyn Error>> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for sample in samples {
        // Clamp before scaling: whisper wants f32, but a 16-bit wav is what a
        // human can double-click, and an out-of-range sample would wrap.
        let clamped = sample.clamp(-1.0, 1.0);
        writer.write_sample((clamped * f32::from(i16::MAX)) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}
