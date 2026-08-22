//! `transcribe_probe` — one-shot runtime harness for the whisper path.
//!
//! Takes a model file and a wav file, loads the model, warms it up, transcribes
//! the wav once, prints the timings and the line, and exits. That is the whole
//! program: no loop, no stdin, so it is scriptable and can never be left
//! running.
//!
//! It answers what a unit test cannot: does this machine's Vulkan build
//! actually load a model, how long do the load, warm-up and decode really take
//! here, and does a real recording from `mic_probe` come back as sensible text.
//!
//! No model ships with Sotone, so the model path is a required argument.
//! Nothing is downloaded, here or anywhere else in the crate.
//!
//! Run with:
//! `cargo run --release -p sotone-core --features vulkan --example transcribe_probe -- <model.bin> <clip.wav> [language]`

use std::error::Error;
use std::path::Path;
use std::process::ExitCode;

use sotone_core::audio::TARGET_SAMPLE_RATE;
use sotone_core::model::validate_model;
use sotone_core::transcribe::{Language, Transcriber};

fn main() -> ExitCode {
    // Not `?` from `main`: Rust's termination path prints errors with `Debug`,
    // and every message in this crate is written to be read by a human as
    // `Display`. Same fix hotkey_probe carries.
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
    // Without this, every timing this probe is supposed to report — and every
    // whisper.cpp/ggml line that whisper-rs routes into `tracing` — goes
    // nowhere. `RUST_LOG` still overrides it for a quieter or noisier run.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let mut args = std::env::args().skip(1);
    let (Some(model_arg), Some(wav_arg)) = (args.next(), args.next()) else {
        return Err(
            "usage: transcribe_probe <model.bin> <clip.wav> [language]\n\
             language defaults to auto; the wav must be 16 kHz mono, as mic_probe writes"
                .into(),
        );
    };
    let language = Language::new(&args.next().unwrap_or_else(|| "auto".to_owned()));

    let model_path = Path::new(&model_arg);
    let wav_path = Path::new(&wav_arg);

    // Pre-flight the header ourselves: whisper's own load failure says nothing
    // at all, and this is exactly the check the "Add model…" dialog will run.
    let info = validate_model(model_path)?;
    println!(
        "Model: {} ({}, n_vocab {}, n_mels {}, {:.1} MiB)",
        info.path.display(),
        info.kind,
        info.n_vocab,
        info.n_mels,
        info.size_bytes as f64 / (1024.0 * 1024.0)
    );
    println!("Backend (compiled): {}", sotone_core::backend_name());
    println!("Language requested: {language}");

    let samples = read_wav_mono_16k(wav_path)?;
    println!(
        "Clip: {} ({} samples, {:.2} s)",
        wav_path.display(),
        samples.len(),
        samples.len() as f32 / TARGET_SAMPLE_RATE as f32
    );

    // Load, state-create and warm-up are timed inside `Transcriber::load` and
    // emitted as one tracing line, so they print through the subscriber above
    // rather than being measured a second time here.
    println!("\nLoading and warming up...");
    let wall = std::time::Instant::now();
    let mut transcriber = Transcriber::load(model_path, language)?;
    println!(
        "Ready in {:.2} s (load + state + warm-up, wall clock)",
        wall.elapsed().as_secs_f32()
    );
    println!("Model is multilingual: {}", transcriber.is_multilingual());

    let transcript = transcriber.transcribe(&samples)?;
    println!(
        "\nTranscribed in {:.2} s",
        transcript.duration.as_secs_f32()
    );
    if transcript.is_empty() {
        println!("(no speech found — empty line, which is not an error)");
    } else {
        println!("> {}", transcript.text);
    }

    Ok(())
}

/// Read a wav as the 16 kHz mono `f32` whisper wants.
///
/// Accepts what `mic_probe` writes (16-bit int) and plain f32 wavs. Anything
/// else is rejected loudly rather than resampled: this probe exists to test the
/// whisper path, and quietly converting the input would make a bad result
/// impossible to attribute.
fn read_wav_mono_16k(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();

    if spec.channels != 1 {
        return Err(format!(
            "{} has {} channels; transcribe_probe wants mono \
             (mic_probe's probe-NNN.wav, not probe-NNN-native.wav)",
            path.display(),
            spec.channels
        )
        .into());
    }
    if spec.sample_rate != TARGET_SAMPLE_RATE {
        return Err(format!(
            "{} is {} Hz; whisper needs {TARGET_SAMPLE_RATE} Hz",
            path.display(),
            spec.sample_rate
        )
        .into());
    }

    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => {
            reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?
        }
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|sample| sample.map(|value| f32::from(value) / f32::from(i16::MAX)))
            .collect::<Result<Vec<_>, _>>()?,
        (format, bits) => {
            return Err(format!(
                "{} is {bits}-bit {format:?}; transcribe_probe reads 16-bit int or 32-bit float",
                path.display()
            )
            .into())
        }
    };

    if samples.is_empty() {
        return Err(format!("{} contains no samples", path.display()).into());
    }

    Ok(samples)
}
