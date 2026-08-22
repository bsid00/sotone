//! Speech to one line of text, locally, via whisper.cpp.
//!
//! [`Transcriber`] owns one whisper context and one reused state, and turns a
//! finished utterance — 16 kHz mono `f32`, exactly what
//! [`audio::AudioEngine`](crate::audio::AudioEngine) delivers — into one
//! trimmed line. The API is blocking and synchronous: the caller owns the
//! thread.
//!
//! Three properties of this module are load-bearing:
//!
//! * **It is worker-thread-only.** [`Transcriber::transcribe`] blocks for as
//!   long as the model needs, which is seconds, not milliseconds. It must never
//!   be called from the `rdev` hook callback or the `cpal` data callback —
//!   blocking either one stutters the whole OS input or audio stack, including
//!   the app under test. Nothing in this module touches a callback; the caller
//!   puts it behind the audio worker thread, where blocking is the point.
//! * **Nothing leaves the machine.** whisper.cpp runs the model in-process and
//!   this module contains no I/O beyond opening the model file the user
//!   already has. There is no download path here; models arrive by the user's
//!   own hand via [`model::scan_models_dir`](crate::model::scan_models_dir) or
//!   a file picker.
//! * **No weights are bundled.** [`Transcriber::load`] takes a path. The unit
//!   tests below are pure logic and load no model, so nothing in the repo or
//!   the installer ever needs one.
//!
//! Whether the GPU was actually used is not something whisper-rs exposes as a
//! getter. `install_logging_hooks` routes whisper.cpp's own backend lines into
//! `tracing`, which is where the truth about a silent Vulkan-to-CPU fallback
//! shows up; under the `vulkan` feature `whisper_rs::vulkan::list_devices()`
//! can additionally enumerate the GPUs ggml sees. Neither is wired into the UI
//! here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState,
};

use crate::audio::TARGET_SAMPLE_RATE;

/// The spelling that means "let the model decide", in config and in the UI.
pub const AUTO_LANGUAGE: &str = "auto";

/// Length of the warm-up clip. One second of silence, warming the model on
/// load — long enough to clear
/// whisper.cpp's ~100 ms floor and force the real first-run costs (GPU upload,
/// Vulkan shader compilation) to be paid before the human speaks.
const WARM_UP_SAMPLES: usize = TARGET_SAMPLE_RATE as usize;

/// Longest utterance still decoded as a single segment.
///
/// whisper.cpp works in a fixed 30 s window, and `single_segment` tells it to
/// stop after the first one — so a longer clip is silently truncated rather
/// than reported as an error. Toggle mode makes long clips ordinary,
/// so the flag has to be conditional. 28 s leaves two seconds of
/// margin under the window, because the utterance also carries the 400 ms
/// pre-roll and 200 ms post-roll and the boundary is not worth cutting fine.
const SINGLE_SEGMENT_MAX_SAMPLES: usize = 28 * TARGET_SAMPLE_RATE as usize;

/// Which language the model is told to transcribe.
///
/// Config stores a string; this is that string after the one decision worth
/// making up front — auto-detect or not. Anything that is not `auto` is passed
/// to whisper.cpp verbatim. We deliberately do not keep our own list of valid
/// codes: it would go stale against whisper.cpp's table and reject languages a
/// newer model supports.
///
/// Note what whisper.cpp 1.8.3 actually does with a code it does not know: it
/// logs `unknown language 'xx'` and carries on with a bogus language token, so
/// the failure surfaces as a nonsense transcript in the log, not as an error
/// return. That is why the active language belongs in the overlay, and
/// why the whisper log lines are routed into `tracing`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Language {
    /// `None` is auto-detect. `Some` is a code handed to whisper unchanged.
    code: Option<String>,
}

impl Language {
    /// Auto-detect.
    #[must_use]
    pub const fn auto() -> Self {
        Self { code: None }
    }

    /// Interpret a config string.
    ///
    /// `auto` in any casing, and the empty string, mean auto-detect — the empty
    /// string because whisper.cpp treats a zero-length language as auto anyway,
    /// and a config with `language = ""` should not behave differently from one
    /// with `language = "auto"`.
    ///
    /// Everything else is trimmed of surrounding whitespace and kept as
    /// written, including its casing: whisper.cpp's language table is matched
    /// exactly, so silently lower-casing would hide a config typo rather than
    /// let the log show it.
    #[must_use]
    pub fn new(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(AUTO_LANGUAGE) {
            Self::auto()
        } else {
            Self {
                code: Some(trimmed.to_owned()),
            }
        }
    }

    /// The code to pass to whisper, or `None` for auto-detect.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// Whether this is auto-detect.
    #[must_use]
    pub fn is_auto(&self) -> bool {
        self.code.is_none()
    }

    /// Whether this can cross the FFI boundary at all.
    ///
    /// whisper-rs's `FullParams::set_language` builds a `CString` and
    /// `expect`s, so an interior NUL is a **panic**, not an error. The config
    /// file is user-editable, so the answer is checked rather than risked —
    /// once, here, for everything that can set a language (load, live change,
    /// and the settings command that writes one).
    #[must_use]
    pub fn is_ffi_safe(&self) -> bool {
        !self.code.as_deref().is_some_and(|code| code.contains('\0'))
    }

    /// How to show it: the code, or `auto`. Round-trips through [`Self::new`].
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.code.as_deref().unwrap_or(AUTO_LANGUAGE)
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of the language picker: what to store, and what to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageOption {
    /// What goes in the config and into whisper — [`AUTO_LANGUAGE`] or a code.
    pub code: String,
    /// What a human reads.
    pub label: String,
}

/// Every language whisper.cpp itself knows, plus auto-detect, ready for a
/// picker.
///
/// **Read out of whisper, never kept here.** whisper-rs 0.16 re-exports three
/// standalone functions from `standalone.rs` — `get_lang_max_id()`,
/// `get_lang_str(id)` (`"de"`) and `get_lang_str_full(id)` (`"german"`) — which
/// walk whisper.cpp's own `g_lang` table. They take no context and load no
/// model, so this is safe to call before (or without) any model being loaded,
/// and it costs a handful of `CStr` reads.
///
/// That is why there is no list of codes anywhere in this repo: a hardcoded one
/// would go stale against whisper.cpp the first time it gains a language, and
/// the module already refuses to keep its own table of valid codes for exactly
/// that reason (see [`Language`]).
#[must_use]
pub fn language_options() -> Vec<LanguageOption> {
    let max = whisper_rs::get_lang_max_id();
    let entries = (0..=max).filter_map(|id| {
        let code = whisper_rs::get_lang_str(id)?;
        // The full name is the label; a language whisper can name only by code
        // still gets an entry, because it is still selectable.
        let name = whisper_rs::get_lang_str_full(id).unwrap_or(code);
        Some((code.to_owned(), name.to_owned()))
    });
    assemble_language_options(entries)
}

/// The picker's shape, without whisper: auto first, then everything else by
/// label.
///
/// Split out so the ordering and the labelling are testable with no model, no
/// GPU and no FFI (the repo's `cargo test` rule). whisper's own table is in
/// rough frequency order — `en`, `zh`, `de`, `es`, `ru` — which is a fine
/// default for a decoder and a poor one for a hundred-entry dropdown, so the
/// list is alphabetical by label and auto is pinned to the top.
fn assemble_language_options<I: IntoIterator<Item = (String, String)>>(
    entries: I,
) -> Vec<LanguageOption> {
    let mut options: Vec<LanguageOption> = entries
        .into_iter()
        .map(|(code, name)| LanguageOption {
            label: capitalize(&name),
            code,
        })
        .collect();
    options.sort_by(|a, b| a.label.cmp(&b.label));
    options.insert(
        0,
        LanguageOption {
            code: AUTO_LANGUAGE.to_owned(),
            // The honest name for it: whisper guesses per utterance, and a
            // guess is what the user is choosing.
            label: "Detect automatically".to_owned(),
        },
    );
    options
}

/// whisper spells its language names in lower case; a picker does not.
fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One utterance, transcribed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// The line, as the draft store will append it.
    ///
    /// Expected to be empty or near-empty when there was no speech: a clip
    /// under whisper's ~100 ms floor produces no segments at all, and silence
    /// should produce nothing once non-speech tokens are suppressed, which this
    /// module asks for. "Should" is the honest word — a model can still emit
    /// something from silence, so this is not a guarantee. Either way it is not
    /// an error, and what to do about an empty or junk line is the caller's
    /// decision.
    pub text: String,
    /// Wall time of the transcription call. The UI's honest answer to "how slow
    /// is this model on this machine".
    pub duration: Duration,
}

impl Transcript {
    /// Whether the model produced no words at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Why transcription could not happen.
///
/// `WhisperError::InitError` — what a failed model load reports — carries no
/// detail at all: no file-not-found, no truncated-file, nothing. So every
/// variant here supplies the context whisper cannot, starting with the path.
#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    /// whisper.cpp refused to load the model file.
    #[error(
        "could not load the whisper model at {} ({}): {source}",
        .path.display(),
        describe_path(.path)
    )]
    LoadModel {
        /// The model file we were asked to load.
        path: PathBuf,
        /// whisper's own error, which is usually just "init failed".
        #[source]
        source: WhisperError,
    },

    /// The model loaded but no decoding state could be created for it.
    #[error("could not create a whisper state for the model at {}", .path.display())]
    CreateState {
        /// The model file in use.
        path: PathBuf,
        /// whisper's own error.
        #[source]
        source: WhisperError,
    },

    /// The warm-up pass failed, which means real utterances would fail too.
    #[error(
        "the whisper warm-up pass failed for the model at {} — this model cannot transcribe on \
         this machine",
        .path.display()
    )]
    WarmUp {
        /// The model file in use.
        path: PathBuf,
        /// whisper's own error.
        #[source]
        source: WhisperError,
    },

    /// whisper failed on a real utterance.
    #[error("whisper could not transcribe an utterance of {samples} samples")]
    Transcribe {
        /// How many samples were handed over, for correlating with the clip.
        samples: usize,
        /// whisper's own error.
        #[source]
        source: WhisperError,
    },

    /// The utterance had no samples. Guarded here rather than letting whisper
    /// report it, so the message names our pipeline and not their buffer.
    #[error("nothing to transcribe: the utterance contained no audio samples")]
    EmptyUtterance,

    /// A segment's text could not be read back out of whisper.
    #[error("whisper produced a segment whose text could not be read")]
    SegmentText(#[source] WhisperError),

    /// The configured language string cannot be handed to a C API.
    ///
    /// whisper-rs's `set_language` panics on an interior NUL byte, and a config
    /// file is user-editable, so this is checked rather than risked.
    #[error("the configured language {code:?} contains a NUL byte and cannot be used")]
    InvalidLanguage {
        /// The offending configured value.
        code: String,
    },
}

/// Says whether the file is actually there, since whisper's own load error
/// never distinguishes "missing" from "corrupt".
fn describe_path(path: &Path) -> &'static str {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => "the file exists, so it is unreadable or not a valid model",
        Ok(_) => "that path is a directory, not a model file",
        Err(_) => "no such file",
    }
}

/// A loaded model, ready to transcribe.
///
/// Holds the context and **one** state, reused across utterances — that reuse
/// is the entire reason whisper-rs separates the two. `full` takes `&mut self`
/// on the state, so one transcription runs at a time, which is exactly the
/// single-worker shape of the app.
///
/// Blocking. Worker thread only; see the module docs.
#[derive(Debug)]
pub struct Transcriber {
    /// Kept alive for its model metadata and to make ownership obvious; the
    /// state holds its own reference count on the underlying context.
    context: WhisperContext,
    state: WhisperState,
    language: Language,
    model_path: PathBuf,
}

impl Transcriber {
    /// Load a model, create its state, and warm it up.
    ///
    /// The warm-up is not optional: without it the first real utterance pays
    /// the GPU upload and shader compilation, and that is the one utterance the
    /// user is most likely to be timing.
    ///
    /// GPU use is not forced or forbidden here. `WhisperContextParameters`
    /// defaults `use_gpu` from whisper-rs's own GPU feature, which our `vulkan`
    /// and `metal` features turn on and our `cpu` feature does not — so a CPU
    /// build asks for CPU without this code having to know which build it is.
    /// If no GPU is found at runtime whisper.cpp falls back to CPU silently;
    /// the log lines routed by `install_logging_hooks` are where that shows.
    ///
    /// # Errors
    /// [`TranscribeError::LoadModel`] if whisper cannot read the file,
    /// [`TranscribeError::CreateState`] if no state can be made, and
    /// [`TranscribeError::WarmUp`] if the warm-up pass itself fails.
    pub fn load(model_path: &Path, language: Language) -> Result<Self, TranscribeError> {
        // Idempotent and permanent: the first call wins and there is no way to
        // uninstall it. Doing it here means no caller can create a context
        // before the hooks exist and lose whisper's startup lines to stdout.
        whisper_rs::install_logging_hooks();

        if !language.is_ffi_safe() {
            return Err(TranscribeError::InvalidLanguage {
                code: language.as_str().to_owned(),
            });
        }

        let load_started = Instant::now();
        let context =
            WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
                .map_err(|source| TranscribeError::LoadModel {
                    path: model_path.to_path_buf(),
                    source,
                })?;
        let load_time = load_started.elapsed();

        let state_started = Instant::now();
        let state = context
            .create_state()
            .map_err(|source| TranscribeError::CreateState {
                path: model_path.to_path_buf(),
                source,
            })?;
        let state_time = state_started.elapsed();

        let mut transcriber = Self {
            context,
            state,
            language,
            model_path: model_path.to_path_buf(),
        };

        let warm_up_time = transcriber.warm_up()?;

        // The only latency ground truth this project gets. No numbers are
        // claimed anywhere else; they are measured here and logged.
        tracing::info!(
            model = %transcriber.model_path.display(),
            backend = crate::backend_name(),
            multilingual = transcriber.is_multilingual(),
            language = %transcriber.language,
            load_ms = load_time.as_millis(),
            state_ms = state_time.as_millis(),
            warm_up_ms = warm_up_time.as_millis(),
            "whisper model ready"
        );

        Ok(transcriber)
    }

    /// One dummy pass over silence. There is no official warm-up API; this is
    /// the standard trick. The result is discarded — a model that hallucinates
    /// words out of digital silence is a real phenomenon, and none of it is
    /// user data.
    fn warm_up(&mut self) -> Result<Duration, TranscribeError> {
        let silence = vec![0.0_f32; WARM_UP_SAMPLES];
        let started = Instant::now();
        self.run(&silence)
            .map_err(|source| TranscribeError::WarmUp {
                path: self.model_path.clone(),
                source,
            })?;
        Ok(started.elapsed())
    }

    /// Transcribe one utterance: 16 kHz mono `f32`, one call per utterance.
    ///
    /// Blocks for the length of the decode. **Worker thread only** — never a
    /// `cpal` or `rdev` callback.
    ///
    /// Zero segments is success with empty text, not an error: a clip under
    /// whisper's ~100 ms floor lands there, as silence usually does, and
    /// deciding what an empty line means belongs to the caller, not here.
    ///
    /// # Errors
    /// [`TranscribeError::EmptyUtterance`] for an empty slice,
    /// [`TranscribeError::InvalidLanguage`] if the configured code cannot cross
    /// the FFI boundary, [`TranscribeError::Transcribe`] if the decode fails,
    /// and [`TranscribeError::SegmentText`] if a produced segment's text cannot
    /// be read back.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcript, TranscribeError> {
        if samples.is_empty() {
            return Err(TranscribeError::EmptyUtterance);
        }

        let started = Instant::now();
        self.run(samples)
            .map_err(|source| TranscribeError::Transcribe {
                samples: samples.len(),
                source,
            })?;
        let duration = started.elapsed();

        let mut segments = Vec::new();
        for segment in self.state.as_iter() {
            // Lossy on purpose: one stray invalid byte in a multi-second
            // utterance must not cost the user the whole line.
            let text = segment
                .to_str_lossy()
                .map_err(TranscribeError::SegmentText)?;
            segments.push(text);
        }

        let text = join_segments(&segments);

        tracing::info!(
            samples = samples.len(),
            audio_ms = (samples.len() as u64 * 1000) / u64::from(TARGET_SAMPLE_RATE),
            transcribe_ms = duration.as_millis(),
            segments = segments.len(),
            chars = text.len(),
            "transcribed utterance"
        );

        Ok(Transcript { text, duration })
    }

    /// The decode itself. Shared by the warm-up so the warm-up exercises the
    /// exact code path a real utterance takes; anything less is not a warm-up.
    fn run(&mut self, samples: &[f32]) -> Result<(), WhisperError> {
        // Owned copy so the params borrow a local rather than `self`, which is
        // mutably borrowed by `full` on the next line.
        let language = self.language.code().map(str::to_owned);
        let params = full_params(
            language.as_deref(),
            samples.len() <= SINGLE_SEGMENT_MAX_SAMPLES,
        );
        self.state.full(params, samples)
    }

    /// The model file backing this transcriber.
    #[must_use]
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    /// The language this transcriber is currently decoding in.
    #[must_use]
    pub fn language(&self) -> &Language {
        &self.language
    }

    /// Change the language, live.
    ///
    /// **This is the whole mechanism, and it is a field assignment.** whisper's
    /// language is a *decoding parameter*, not a property of the context or the
    /// state: [`full_params`] is built fresh for every utterance (it has to be —
    /// `FullParams` borrows the language string), so the next `full` call
    /// simply reads the new value. Nothing is reloaded, no state is recreated,
    /// and the model file on disk is untouched. Swapping the whole
    /// [`Transcriber`] for a language change would cost seconds of model load
    /// for a value that is passed per call anyway.
    ///
    /// Worker-thread only, like everything else on this type: it takes
    /// `&mut self`, so it cannot run while a decode is in flight.
    ///
    /// An English-only model ignores this entirely (see
    /// [`Transcriber::is_multilingual`]); the setting is still stored, because
    /// it is the user's configuration and the next model may be multilingual.
    ///
    /// # Errors
    /// [`TranscribeError::InvalidLanguage`] if the code cannot cross the FFI
    /// boundary — the same check [`Transcriber::load`] makes, so a language that
    /// would panic in whisper-rs never reaches it.
    pub fn set_language(&mut self, language: Language) -> Result<(), TranscribeError> {
        if !language.is_ffi_safe() {
            return Err(TranscribeError::InvalidLanguage {
                code: language.as_str().to_owned(),
            });
        }
        tracing::info!(
            from = %self.language,
            to = %language,
            multilingual = self.is_multilingual(),
            "transcription language changed"
        );
        self.language = language;
        Ok(())
    }

    /// Whether the loaded model can do anything other than English. An
    /// English-only model ignores the language setting entirely.
    #[must_use]
    pub fn is_multilingual(&self) -> bool {
        self.context.is_multilingual()
    }
}

/// Decoding parameters for one dictated utterance.
///
/// Built fresh per call because `FullParams` borrows the language string;
/// building it once and storing it would tangle lifetimes for no gain, as
/// construction costs nothing next to a decode.
fn full_params(language: Option<&str>, single_segment: bool) -> FullParams<'_, '_> {
    // Greedy with best_of 1: one short utterance, decoded once. Beam search
    // multiplies latency for accuracy that a note-taking line does not need.
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

    // One utterance is one line, so a clip that fits whisper's 30 s
    // window is asked for as one segment rather than stitched back together
    // afterwards. Past that the flag would cost the caller everything after the
    // first window without saying so, so a long toggle-mode recording is
    // decoded as several segments and `join_segments` folds them into the one
    // line the draft store appends.
    params.set_single_segment(single_segment);

    // Each press is independent. Carrying decoder context across utterances
    // makes the model finish the previous sentence instead of hearing this one.
    params.set_no_context(true);

    // Transcribe, never translate. Sotone logs what was said.
    params.set_translate(false);

    // Suppress non-speech tokens (default is off). Sotone is a dictation logger:
    // a note line must contain the user's words and nothing else, and whisper's
    // sound annotations — `[Music]`, `(applause)` and friends — are not words
    // anyone said. Observed on this machine: base.en on a near-silent clip
    // produced the single line "[ Silence ]", which would have been appended to
    // the user's markdown verbatim.
    //
    // The same mask also blocks a `-` or `'` at the start of a word, which is
    // whisper.cpp's own definition of the non-speech set, not an extra we chose;
    // mid-word apostrophes (don't, it's) are unaffected.
    params.set_suppress_nst(true);

    // `set_language` must always be called: the default is "en", so a
    // multilingual model would silently transcribe every language as English.
    //
    // Note that on a `Some` code whisper-rs leaks the CString it builds here
    // (it uses `into_raw` and has no `Drop`). It is a handful of bytes per
    // utterance and not worth working around with unsafe.
    params.set_language(language);

    // whisper.cpp's own printf output. Nothing here is ours, and the console
    // is a user-facing surface in a probe.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    params
}

/// Fold segment texts into the single line the draft store appends.
///
/// Whisper hands back text with a leading space and, on longer clips, embedded
/// newlines; a line with a newline in it would break the one-utterance-per-line
/// guarantee that is the file format's only promise. Collapsing every run of
/// whitespace handles the leading space, the newlines, the carriage returns and
/// the double spaces in one pass.
///
/// Free function taking strings rather than a method on the state, so the
/// joining rules are testable without a model.
fn join_segments<I, S>(segments: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for segment in segments {
        for word in segment.as_ref().split_whitespace() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nothing in here loads a model, opens a device or touches a GPU. That is
    // deliberate: no model weights exist in this repo to load.

    #[test]
    fn auto_in_any_casing_is_auto_detect() {
        for raw in ["auto", "AUTO", "Auto", " auto ", ""] {
            let language = Language::new(raw);
            assert!(language.is_auto(), "{raw:?} should be auto");
            assert_eq!(language.code(), None);
            assert_eq!(language.as_str(), "auto");
        }
    }

    #[test]
    fn a_code_passes_through_unchanged() {
        for raw in ["en", "tr", "zh", "yue"] {
            let language = Language::new(raw);
            assert!(!language.is_auto());
            assert_eq!(language.code(), Some(raw));
            assert_eq!(language.as_str(), raw);
        }
    }

    #[test]
    fn a_code_is_trimmed_but_not_recased() {
        // Trimming is a config-file courtesy; re-casing would hide a typo that
        // whisper.cpp's exact-match table will reject.
        let language = Language::new("  EN  ");
        assert_eq!(language.code(), Some("EN"));
    }

    #[test]
    fn language_round_trips_through_its_display_form() {
        for raw in ["auto", "en", "tr"] {
            let language = Language::new(raw);
            assert_eq!(Language::new(&language.to_string()), language);
        }
    }

    #[test]
    fn default_language_is_auto() {
        assert_eq!(Language::default(), Language::auto());
    }

    #[test]
    fn a_language_with_a_nul_byte_is_refused_rather_than_handed_to_whisper() {
        // whisper-rs `expect`s on the CString, so this check is the difference
        // between a refusal and a panic. Auto and ordinary codes pass.
        assert!(Language::auto().is_ffi_safe());
        assert!(Language::new("tr").is_ffi_safe());
        assert!(!Language::new("t\0r").is_ffi_safe());
    }

    #[test]
    fn the_language_picker_puts_auto_first_and_the_rest_in_alphabetical_order() {
        // whisper's own table order — frequency, not alphabet.
        let entries = [
            ("en".to_owned(), "english".to_owned()),
            ("zh".to_owned(), "chinese".to_owned()),
            ("de".to_owned(), "german".to_owned()),
        ];
        let options = assemble_language_options(entries);

        assert_eq!(options[0].code, AUTO_LANGUAGE);
        assert_eq!(options[0].label, "Detect automatically");
        // Every code survives verbatim: it is what goes into the config and
        // into whisper.
        assert_eq!(
            options[1..]
                .iter()
                .map(|option| (option.code.as_str(), option.label.as_str()))
                .collect::<Vec<_>>(),
            vec![("zh", "Chinese"), ("en", "English"), ("de", "German"),]
        );
    }

    #[test]
    fn every_picker_code_round_trips_through_language() {
        let options = assemble_language_options([
            ("en".to_owned(), "english".to_owned()),
            ("yue".to_owned(), "cantonese".to_owned()),
        ]);
        for option in options {
            let language = Language::new(&option.code);
            assert_eq!(language.as_str(), option.code, "{option:?}");
            assert!(language.is_ffi_safe());
        }
    }

    #[test]
    fn segments_join_with_one_space_and_lose_the_leading_one() {
        // Whisper's own shape: every segment starts with a space.
        let joined = join_segments([" Hello there.", " Second thought."]);
        assert_eq!(joined, "Hello there. Second thought.");
    }

    #[test]
    fn newlines_and_carriage_returns_become_spaces() {
        // One utterance is one line; a newline here would corrupt the markdown.
        let joined = join_segments([" first\nsecond\r\nthird"]);
        assert_eq!(joined, "first second third");
        assert!(!joined.contains('\n'));
        assert!(!joined.contains('\r'));
    }

    #[test]
    fn runs_of_whitespace_collapse() {
        let joined = join_segments(["  a  \t b  ", "   c   "]);
        assert_eq!(joined, "a b c");
    }

    #[test]
    fn whitespace_only_segments_contribute_nothing() {
        assert_eq!(join_segments([" ", "\n", "\t"]), "");
        assert_eq!(join_segments([" ", " word ", "\n"]), "word");
    }

    #[test]
    fn no_segments_is_an_empty_line_not_a_panic() {
        let empty: [&str; 0] = [];
        assert_eq!(join_segments(empty), "");
    }

    #[test]
    fn joining_accepts_owned_and_borrowed_text() {
        // The real caller passes `Cow<str>` out of whisper.
        use std::borrow::Cow;
        let owned = vec![String::from(" one"), String::from(" two")];
        assert_eq!(join_segments(&owned), "one two");
        let cows = vec![Cow::Borrowed(" one"), Cow::Owned(String::from(" two"))];
        assert_eq!(join_segments(cows), "one two");
    }

    #[test]
    fn empty_utterance_error_names_the_problem_in_our_words() {
        let message = TranscribeError::EmptyUtterance.to_string();
        assert!(message.contains("no audio samples"), "message: {message}");
    }

    #[test]
    fn load_error_reports_a_missing_file_as_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("ggml-nope.bin");
        let err = TranscribeError::LoadModel {
            path: missing.clone(),
            source: WhisperError::InitError,
        };
        let message = err.to_string();
        assert!(message.contains("ggml-nope.bin"), "message: {message}");
        assert!(message.contains("no such file"), "message: {message}");
    }

    #[test]
    fn load_error_distinguishes_an_existing_but_bad_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("ggml-broken.bin");
        std::fs::write(&path, b"not a model").expect("seed");
        let message = TranscribeError::LoadModel {
            path,
            source: WhisperError::InitError,
        }
        .to_string();
        assert!(message.contains("the file exists"), "message: {message}");
    }

    #[test]
    fn transcriber_can_be_moved_onto_a_worker_thread() {
        // Compile-time only, no model involved. The session worker owns the transcriber
        // from the audio worker thread, so losing `Send` would be a silent
        // architectural regression.
        fn assert_send<T: Send>() {}
        assert_send::<Transcriber>();
    }

    #[test]
    fn warm_up_uses_one_second_of_audio() {
        assert_eq!(WARM_UP_SAMPLES, TARGET_SAMPLE_RATE as usize);
    }

    #[test]
    fn the_single_segment_threshold_is_28_seconds_at_16k() {
        // Pins the intent, not the expression: below whisper's 30 s window,
        // with margin, so nothing a long toggle recording contains is dropped.
        assert_eq!(SINGLE_SEGMENT_MAX_SAMPLES, 448_000);
        assert_eq!(
            SINGLE_SEGMENT_MAX_SAMPLES as u32 / TARGET_SAMPLE_RATE,
            28,
            "the threshold must stay under the 30 s decode window"
        );
    }
}
