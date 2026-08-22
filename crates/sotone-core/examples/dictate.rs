//! `dictate` — the whole core loop, in a console.
//!
//! Hold the push-to-talk key, say one finding, release; the line is transcribed
//! locally and appended to a durable draft. Press, speak, release, again. Or,
//! for a long finding, press the toggle key once to start and once more to
//! stop. The Tauri shell is the real frontend — this is the same
//! wiring without a window, which makes it the thing a script can drive and a
//! human can use before any UI exists.
//!
//! # One recording at a time
//!
//! The two modes share a single recording state: whichever
//! one started the recording is the only one that can end it, and the other's
//! key is ignored until it does. The state machine lives in the main loop
//! below, next to the hold timer, because it is the same shape the window needs.
//!
//! ```text
//! hotkey ──press/release──┐
//!                         ├─→ AudioEngine ─→ SessionWorker ─→ whisper ─→ draft
//!                         └─→ CuePlayer
//! ```
//!
//! # `--stdin`
//!
//! With `--stdin` the input hook is not installed at all; `begin`, `end`,
//! `toggle` and `quit` lines on stdin drive the very same code path instead —
//! `begin`/`end` are the push-to-talk pair, `toggle` is one press of the toggle
//! key. That is the
//! scripted-test hook (a rig can play a TTS phrase through the
//! speakers and assert on `lines.jsonl` afterwards), and it is emphatically
//! **not** synthetic input: it calls `AudioEngine::begin_utterance` /
//! `end_utterance` directly. Nothing here, or anywhere in Sotone, generates a
//! keystroke or a mouse event (invariant 1). The real hook path was verified by
//! a human.
//!
//! Nothing is downloaded and no window is created or focused (invariants 2, 3).
//!
//! Run with:
//! `cargo run --release -p sotone-core --features vulkan --example dictate -- [options]`
//!
//! Options:
//! `--model <path>`   ggml model file. Default: the first one in the models dir.
//! `--language <code>` whisper language, or `auto` (default).
//! `--mic <substring>` pin the microphone by name substring.
//! `--hotkey <token>`  push-to-talk binding, e.g. `F13`, `MouseX2`. Default F13.
//! `--toggle-hotkey <token>` toggle binding. Default F14.
//! `--no-ptt`          do not bind push-to-talk (console stand-in for the
//!                     Settings switch).
//! `--no-toggle`       do not bind the toggle key. Passing both is rejected,
//!                     exactly as the config layer rejects it.
//! `--drafts <dir>`    drafts root. Default: the app data drafts dir.
//! `--stdin`           read `begin` / `end` / `toggle` / `quit` instead of
//!                     using the hook.

use std::io::{self, BufRead};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};

use sotone_core::audio::{AudioEngine, EngineStatus};
use sotone_core::config::{
    default_models_dir, DEFAULT_HOTKEY, DEFAULT_LANGUAGE, DEFAULT_TOGGLE_HOTKEY,
};
use sotone_core::cue::{Cue, CuePlayer};
use sotone_core::draft::{default_drafts_dir, DraftStore};
use sotone_core::hotkey::{Binding, Bindings, HotkeyListener, PttEvent};
use sotone_core::model::scan_models_dir;
use sotone_core::session::{
    ReleaseInfo, SessionCommand, SessionConfig, SessionEvent, SessionWorker,
};
use sotone_core::transcribe::{Language, Transcriber};

const USAGE: &str = "usage: dictate [--model <path>] [--language <code>] [--mic <substring>]\n\
                     \x20              [--hotkey <token>] [--toggle-hotkey <token>]\n\
                     \x20              [--no-ptt] [--no-toggle] [--drafts <dir>] [--stdin]";

fn main() -> ExitCode {
    // Not `?` from `main`: Rust's termination path prints errors with `Debug`,
    // and every message in this crate is written to be read as `Display`.
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

/// What the main loop reacts to, whatever produced it.
///
/// The hotkey path and the `--stdin` path both funnel into this, so there is
/// exactly one implementation of "a press starts an utterance".
enum Input {
    Press,
    Release {
        /// When the key came up, from the hook where there is one.
        at: SystemTime,
    },
    /// One press of the toggle key: start if idle, stop if the toggle owns the
    /// live recording.
    Toggle {
        /// When the key went down, from the hook where there is one. A stopping
        /// press is the note's timestamp.
        at: SystemTime,
    },
    HookLost(String),
    Quit,
}

/// Which mode started the recording that is running.
///
/// The other mode's key is ignored until this one ends it: one recording state,
/// one source, no interleaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Ptt,
    Toggle,
}

/// The single recording state both modes share.
enum Recording {
    Idle,
    Active {
        source: Source,
        /// `Instant`, not wall clock: the hold length must not depend on
        /// whether the clock was stepped mid-note.
        started: Instant,
    },
}

struct Args {
    model: Option<PathBuf>,
    language: String,
    mic: Option<String>,
    hotkey: String,
    toggle_hotkey: String,
    ptt_enabled: bool,
    toggle_enabled: bool,
    drafts: PathBuf,
    stdin: bool,
}

fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    let args = parse_args()?;
    let bindings = resolve_bindings(&args)?;
    let language = Language::new(&args.language);
    let model_path = resolve_model(args.model)?;

    let (engine, utterances) = AudioEngine::start(args.mic.as_deref())?;

    // Cues are an enhancement, never a blocker: a machine with no speakers still
    // dictates, it just does it quietly.
    let cues = match CuePlayer::start() {
        Ok(player) => Some(Arc::new(player)),
        Err(err) => {
            eprintln!("running without audio cues: {err}");
            None
        }
    };

    let transcriber = Transcriber::load(&model_path, language.clone())?;
    let store = DraftStore::new(args.drafts);
    let draft = store.create_draft(None)?;
    let draft_dir = draft.dir().to_path_buf();

    let (release_tx, release_rx) = mpsc::channel::<ReleaseInfo>();
    // The console runner has no draft list and no way to switch drafts: it
    // opens one draft up front and keeps it, which is exactly what it always
    // did. The command channel exists for the window; holding the
    // sender simply keeps it open for the life of the run.
    let (_draft_commands, command_rx) = mpsc::channel::<SessionCommand>();
    let (worker, events) = SessionWorker::spawn(
        utterances,
        release_rx,
        command_rx,
        SessionConfig {
            transcriber,
            draft: Some(draft),
            store: store.clone(),
            project: None,
            cues: cues.clone(),
        },
    )?;

    println!("Microphone: {}", engine.device_name());
    match &cues {
        Some(player) => println!("Cues:       {}", player.device_name()),
        None => println!("Cues:       (none)"),
    }
    println!("Model:      {}", model_path.display());
    println!("Backend:    {}", sotone_core::backend_name());
    println!("Language:   {language}");
    println!("Draft:      {}", draft_dir.display());
    match bindings.ptt {
        Some(binding) => println!("Push-to-talk: {binding} (hold)"),
        None => println!("Push-to-talk: (disabled)"),
    }
    match bindings.toggle {
        Some(binding) => println!("Toggle:       {binding} (press to start, press to stop)"),
        None => println!("Toggle:       (disabled)"),
    }
    if args.stdin {
        println!("\nCommands on stdin: 'begin', 'end', 'toggle', 'quit'.\n");
    } else {
        println!("\nType 'quit' (or close stdin) to stop.\n");
    }

    let lines = Arc::new(AtomicUsize::new(0));
    let printer = spawn_printer(events, Arc::clone(&lines));

    let (input_tx, input_rx) = mpsc::channel::<Input>();

    // The hook consumer thread must stay free: it is queued behind every input
    // event on the machine (invariant 5). It forwards and returns; the engine
    // and the cue player are driven from this thread, which owns them.
    // `AudioEngine` is deliberately not `Sync` — its command channel is not —
    // so this is also the only sound way to share it.
    let _listener = if args.stdin {
        None
    } else {
        let tx = input_tx.clone();
        Some(HotkeyListener::spawn(bindings, move |event| {
            let _ = tx.send(match event {
                PttEvent::Pressed => Input::Press,
                PttEvent::Released { time } => Input::Release { at: time },
                PttEvent::Toggled { time } => Input::Toggle { at: time },
                PttEvent::HookLost { reason } => Input::HookLost(reason),
            });
        })?)
    };

    let stdin_tx = input_tx.clone();
    let drives = args.stdin;
    thread::spawn(move || read_stdin(&stdin_tx, drives));
    drop(input_tx);

    let mut recording = Recording::Idle;
    let mut engine_dead = false;

    for input in input_rx {
        match input {
            Input::Press => match recording {
                Recording::Idle => {
                    recording = start_recording(&engine, cues.as_ref(), Source::Ptt);
                }
                // The toggle is holding the recording; ignoring the press is
                // what keeps one utterance to one source.
                Recording::Active { .. } => {
                    println!("  (ignored: a recording is already running)");
                }
            },
            Input::Release { at } => match recording {
                Recording::Active {
                    source: Source::Ptt,
                    started,
                } => {
                    recording = Recording::Idle;
                    stop_recording(&engine, &release_tx, started, at);
                }
                // A release with no press of ours: the hotkey filter drops the
                // unmatched ones, so this is a stray `end` on stdin or a
                // push-to-talk release while the toggle owns the recording.
                _ => println!(
                    "  (ignored: push-to-talk is not recording — is the toggle holding it?)"
                ),
            },
            Input::Toggle { at } => match recording {
                Recording::Idle => {
                    recording = start_recording(&engine, cues.as_ref(), Source::Toggle);
                }
                Recording::Active {
                    source: Source::Toggle,
                    started,
                } => {
                    // The stopping press is as close as toggle mode gets to a
                    // key release, so it is the line's timestamp.
                    recording = Recording::Idle;
                    stop_recording(&engine, &release_tx, started, at);
                }
                Recording::Active {
                    source: Source::Ptt,
                    ..
                } => println!("  (ignored: push-to-talk is recording)"),
            },
            Input::HookLost(reason) => {
                println!("  hotkey hook lost: {reason}");
                println!("  no further key events will arrive; type 'quit' to stop.");
            }
            Input::Quit => break,
        }

        // Cheap, and the alternative is a session that looks alive while the
        // microphone has been gone for ten minutes.
        if let EngineStatus::Dead { reason } = engine.status() {
            if !engine_dead {
                engine_dead = true;
                worker.report_engine_dead(reason);
            }
        }
    }

    // Order matters: stopping capture closes the utterance channel, which is
    // what lets the worker finish; the worker's own drop then joins it, and only
    // then does the event channel close and the printer end.
    drop(engine);
    drop(worker);
    let _ = printer.join();

    println!("\nDraft: {}", draft_dir.display());
    println!("Lines: {}", lines.load(Ordering::Relaxed));
    println!("Read them back before the next run.");
    Ok(())
}

/// Open an utterance and cue the user that it is running.
///
/// Both modes start a recording identically — the begin cue is the only
/// confirmation the user gets, and a mode without it would feel broken.
fn start_recording(
    engine: &AudioEngine,
    cues: Option<&Arc<CuePlayer>>,
    source: Source,
) -> Recording {
    engine.begin_utterance();
    if let Some(player) = cues {
        player.play(Cue::Begin);
    }
    Recording::Active {
        source,
        started: Instant::now(),
    }
}

/// Close the utterance and hand the worker the timestamp and hold length it
/// pairs with the audio. There is no stop cue: `Saved` lands a moment later and
/// is the real confirmation.
fn stop_recording(
    engine: &AudioEngine,
    release_tx: &Sender<ReleaseInfo>,
    started: Instant,
    at: SystemTime,
) {
    engine.end_utterance();
    let _ = release_tx.send(ReleaseInfo {
        released_at: at,
        held: started.elapsed(),
    });
}

/// Drain [`SessionEvent`]s onto the console, one line each.
fn spawn_printer(
    events: mpsc::Receiver<SessionEvent>,
    lines: Arc<AtomicUsize>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for event in events {
            match event {
                SessionEvent::LineAdded { record, .. } => {
                    let number = lines.fetch_add(1, Ordering::Relaxed) + 1;
                    println!(
                        "{number:03} [{}] {}",
                        record.spoken_at.format("%H:%M:%S"),
                        if record.failed {
                            "(could not be transcribed — the audio is kept)"
                        } else {
                            record.text.as_str()
                        }
                    );
                }
                SessionEvent::Skipped { reason, .. } => println!("    skipped: {reason}"),
                // Capture never waits for the model, so the gap
                // between the two is reported rather than hidden.
                SessionEvent::Queued { seconds, .. } => {
                    println!("    queued: {seconds:.1}s of audio waiting for the model");
                }
                SessionEvent::Decoding { .. } => {}
                SessionEvent::Held { text, .. } => {
                    println!("    HELD (not written yet, kept in memory): {text}");
                }
                SessionEvent::Debug { message } => println!("    debug: {message}"),
                // The recording state below is deliberately left alone: the
                // engine is still capturing, and the user's next press is
                // still the press that stops it. Nothing to reset.
                SessionEvent::Capped { cap } => println!(
                    "    hit the {} cap — split into a new line, still recording",
                    cap_label(cap)
                ),
                SessionEvent::EngineDead { reason } => {
                    println!("    MICROPHONE GONE: {reason}");
                }
                // This runner never switches or discards drafts, so these can
                // only be the announcement of the one draft it opened itself.
                SessionEvent::DraftChanged { dir, .. } => {
                    if let Some(dir) = dir {
                        println!("    draft: {}", dir.display());
                    }
                }
                SessionEvent::DraftDiscarded { id, ok, message } => {
                    println!(
                        "    discarded {id}: {}",
                        if ok {
                            "moved to .trash".to_owned()
                        } else {
                            message.unwrap_or_else(|| "failed".to_owned())
                        }
                    );
                }
                // This runner has no editing surface (that is the window's
                // job), so a transcript snapshot is only worth its count here.
                SessionEvent::Transcript {
                    lines: transcript, ..
                } => {
                    println!("    transcript: {} line(s)", transcript.len());
                }
                // This runner has no Save button (that is the window's
                // job), so this can only arrive if something else sent the
                // command. Reported rather than ignored: a save outcome that
                // vanishes is the one thing a save must never do.
                SessionEvent::Saved { outcome } => println!("    save: {outcome:?}"),
                SessionEvent::SavedAll {
                    saved,
                    skipped,
                    conflicts,
                    errors,
                } => println!(
                    "    save all: {saved} saved, {skipped} skipped, {} stopped, {} failed",
                    conflicts.len(),
                    errors.len()
                ),
                // This runner has no rename surface either (that is the
                // window's job), so this can only arrive if something else
                // sent the command — and a refusal that vanished would read as
                // a rename that silently happened.
                SessionEvent::Renamed { errors } => {
                    if errors.is_empty() {
                        println!("    renamed");
                    } else {
                        println!("    rename refused: {}", errors.join("; "));
                    }
                }
                // Nor a drop surface (that too is the window's), on the same
                // terms: it can only arrive if something else sent the
                // command, and a move outcome that vanished would read as a
                // file that quietly went somewhere.
                SessionEvent::NoteMoved { outcome, .. } => match outcome {
                    Ok(moved) => println!("    moved: {moved:?}"),
                    Err(message) => println!("    move refused: {message}"),
                },
                SessionEvent::Notice { message } => println!("    {message}"),
                SessionEvent::Error { message } => println!("    ERROR: {message}"),
            }
        }
    })
}

/// The cap as the user thinks of it: "2-minute", or seconds if it is not a
/// whole number of minutes.
///
/// Derived from the `Duration` the event carries, never from a literal — the
/// constant has already moved twice.
fn cap_label(cap: Duration) -> String {
    let seconds = cap.as_secs();
    if seconds >= 60 && seconds % 60 == 0 {
        format!("{}-minute", seconds / 60)
    } else {
        format!("{seconds}-second")
    }
}

/// Read commands until stdin closes.
///
/// `drives` is `--stdin` mode: `begin`, `end` and `toggle` become utterance
/// boundaries. Without it only `quit` means anything, so the console can still
/// be used to stop a hotkey session cleanly instead of killing it.
///
/// The commands are not gated on `--no-ptt` / `--no-toggle`: those switch off
/// *hook bindings*, and in `--stdin` mode no hook exists. The caller drives
/// the state machine directly.
fn read_stdin(tx: &Sender<Input>, drives: bool) {
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let command = line.trim().to_ascii_lowercase();
        let input = match command.as_str() {
            "quit" | "q" | "exit" => Input::Quit,
            "begin" | "start" if drives => Input::Press,
            // `now()` here rather than in the worker: this is as close to the
            // "release" as a piped command ever gets.
            "end" | "stop" if drives => Input::Release {
                at: SystemTime::now(),
            },
            // One press of the toggle key. Start or stop is the state
            // machine's decision, exactly as it is for the real hook.
            "toggle" | "t" if drives => Input::Toggle {
                at: SystemTime::now(),
            },
            "" => continue,
            other => {
                eprintln!("unknown command: {other:?}");
                continue;
            }
        };
        let quit = matches!(input, Input::Quit);
        if tx.send(input).is_err() || quit {
            return;
        }
    }
    // EOF: the caller closed the pipe, or the human pressed Ctrl-Z/Ctrl-D.
    let _ = tx.send(Input::Quit);
}

/// The model to load: the one asked for, or the first one the user has.
fn resolve_model(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let dir = default_models_dir();
    let scan = scan_models_dir(&dir)?;
    match scan.models.into_iter().next() {
        Some(info) => Ok(info.path),
        None => {
            // No weights ship with Sotone, so an empty models dir is the
            // expected state of a fresh machine, not a bug.
            bail!(
                "no ggml model found in {}. Put one there, or pass --model <path>.",
                dir.display()
            )
        }
    }
}

/// The bindings to watch, with the same two rules the config layer enforces:
/// at least one mode enabled, and two distinct keys when both are.
///
/// Rejecting a shared binding here rather than letting it through matters
/// because the filter resolves the tie in favour of push-to-talk, which would
/// leave the toggle key looking broken instead of misconfigured.
fn resolve_bindings(args: &Args) -> Result<Bindings> {
    let ptt = args
        .ptt_enabled
        .then(|| {
            args.hotkey
                .parse::<Binding>()
                .with_context(|| format!("--hotkey {}", args.hotkey))
        })
        .transpose()?;
    let toggle = args
        .toggle_enabled
        .then(|| {
            args.toggle_hotkey
                .parse::<Binding>()
                .with_context(|| format!("--toggle-hotkey {}", args.toggle_hotkey))
        })
        .transpose()?;

    match (ptt, toggle) {
        (None, None) => bail!(
            "--no-ptt and --no-toggle together leave no way to record; at least one of \
             push-to-talk and toggle must stay enabled"
        ),
        (Some(a), Some(b)) if a == b => bail!(
            "--hotkey and --toggle-hotkey are both {a}; push-to-talk and toggle need \
             different bindings while both are enabled"
        ),
        _ => Ok(Bindings { ptt, toggle }),
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        model: None,
        language: DEFAULT_LANGUAGE.to_owned(),
        mic: None,
        hotkey: DEFAULT_HOTKEY.to_owned(),
        toggle_hotkey: DEFAULT_TOGGLE_HOTKEY.to_owned(),
        ptt_enabled: true,
        toggle_enabled: true,
        drafts: default_drafts_dir(),
        stdin: false,
    };

    let mut raw = std::env::args().skip(1);
    while let Some(flag) = raw.next() {
        match flag.as_str() {
            "--stdin" => args.stdin = true,
            "--model" => args.model = Some(PathBuf::from(value(&flag, &mut raw)?)),
            "--language" => args.language = value(&flag, &mut raw)?,
            "--mic" => args.mic = Some(value(&flag, &mut raw)?),
            "--hotkey" => args.hotkey = value(&flag, &mut raw)?,
            "--toggle-hotkey" => args.toggle_hotkey = value(&flag, &mut raw)?,
            "--no-ptt" => args.ptt_enabled = false,
            "--no-toggle" => args.toggle_enabled = false,
            "--drafts" => args.drafts = PathBuf::from(value(&flag, &mut raw)?),
            "-h" | "--help" => bail!("{USAGE}"),
            other => bail!("unknown option {other:?}\n{USAGE}"),
        }
    }

    Ok(args)
}

/// The value belonging to a flag, or a message naming the flag that is missing
/// one.
fn value(flag: &str, raw: &mut impl Iterator<Item = String>) -> Result<String> {
    raw.next()
        .with_context(|| format!("{flag} needs a value\n{USAGE}"))
}
