//! Model discovery: which files in a folder are whisper.cpp GGML models, and
//! for each one whether it is English-only or multilingual.
//!
//! The point of this module is *when* it fails. A bad file has to be
//! rejected **at scan time** with an explanation, never at transcribe time with
//! a crash, so every rejection here carries a sentence that can be shown to the
//! user verbatim. That is also why rejected files are returned alongside the
//! good ones instead of being dropped: a file that silently fails to appear in
//! the dropdown teaches the user nothing.
//!
//! Two rules about what this module touches:
//!
//! * **It writes nothing at all.** Discovery is pure — a rejected file stays
//!   exactly where the user put it, and so does a good one. This module once
//!   had two file moves (`add_model`, `remove_model`) for the in-app "Add
//!   model…" and "Remove" buttons; both buttons were retired — `models_dir` is
//!   the one place models live and the user manages it by hand — and the moves
//!   were deleted with them. Sotone therefore
//!   never creates, renames or deletes anything under `models_dir` —
//!   invariant 4 holds here by there being no write path left to get wrong.
//! * It never fetches anything. Model weights arrive by the user's own hand
//!   (none are bundled — the user downloads the file and drops it in the
//!   folder), so there is no network client here, no URL, and no download path
//!   (invariant 3).
//!
//! Only the fixed 48-byte header is read — the vocab and tensor data that
//! follow can be gigabytes and none of it is needed to answer these questions.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// `"ggml"` as a little-endian `u32`; on disk the bytes are `6c 6d 67 67`.
const GGML_MAGIC: u32 = 0x6767_6d6c;

/// GGUF is a different container that whisper.cpp does not accept. Its magic is
/// plain ASCII at offset 0, and it is the single most likely wrong file for a
/// user to pick, so it gets its own error rather than "bad magic".
const GGUF_MAGIC: [u8; 4] = *b"GGUF";

/// 4 magic bytes plus 11 `i32` hparams. Everything past this is
/// variable-length and out of scope.
const HEADER_LEN: usize = 48;

/// Number of `i32` hparams following the magic.
const HPARAM_COUNT: usize = 11;

/// Hparam names in file order, used for both indexing and error messages.
const HPARAM_NAMES: [&str; HPARAM_COUNT] = [
    "n_vocab",
    "n_audio_ctx",
    "n_audio_state",
    "n_audio_head",
    "n_audio_layer",
    "n_text_ctx",
    "n_text_state",
    "n_text_head",
    "n_text_layer",
    "n_mels",
    "ftype",
];

/// Mirrors whisper.cpp's `is_multilingual()`: `n_vocab >= 51865`. English-only
/// models are 51864 and large-v3 is 51866, so this must stay a threshold and
/// never become an equality test.
const MULTILINGUAL_MIN_VOCAB: i32 = 51865;

/// What languages a model can transcribe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// `n_vocab == 51864`. The `.en` models; asking these for another language
    /// produces confident nonsense.
    EnglishOnly,
    /// `n_vocab >= 51865`.
    Multilingual,
}

impl ModelKind {
    /// Short lowercase token for logs and the UI.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EnglishOnly => "english-only",
            Self::Multilingual => "multilingual",
        }
    }

    /// Whether a language other than English may be selected for this model.
    #[must_use]
    pub const fn is_multilingual(self) -> bool {
        matches!(self, Self::Multilingual)
    }
}

impl std::fmt::Display for ModelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A file that has been checked and is loadable by whisper.cpp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Where the file is. Models may live outside `models_dir`.
    pub path: PathBuf,
    /// English-only or multilingual, derived from `n_vocab`.
    pub kind: ModelKind,
    /// Vocabulary size from the header.
    pub n_vocab: i32,
    /// Mel bin count. 128 identifies the v3 family; nothing branches on it yet,
    /// but it is the cheapest way to tell v3 from v2 and throwing it away here
    /// would mean re-reading the header later.
    pub n_mels: i32,
    /// Size on disk, for a later "won't fit in VRAM" marking.
    pub size_bytes: u64,
}

/// Why a file is not a usable model.
///
/// Every variant's `Display` is written to be shown to the user unedited, so
/// each one names the file and says what to do about it.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// The file could not be opened or read.
    #[error("could not read model file {}: {source}", .path.display())]
    Io {
        /// Path we tried to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Smaller than the fixed header, so there is nothing to validate.
    #[error(
        "{} is only {size_bytes} bytes, but a whisper model starts with a 48-byte header — \
         the file is truncated or an interrupted download",
        .path.display()
    )]
    TooShort {
        /// Path of the offending file.
        path: PathBuf,
        /// How many bytes the file actually has.
        size_bytes: u64,
    },

    /// A GGUF file. Common mistake, specific advice.
    #[error(
        "{} is in GGUF format, which whisper.cpp cannot load — Sotone needs a GGML model, \
         the kind named like ggml-base.en.bin",
        .path.display()
    )]
    GgufFormat {
        /// Path of the offending file.
        path: PathBuf,
    },

    /// Not a GGML file at all.
    #[error(
        "{} is not a whisper GGML model: it begins with {} instead of the GGML marker \"ggml\"",
        .path.display(),
        describe_magic(.found)
    )]
    BadMagic {
        /// Path of the offending file.
        path: PathBuf,
        /// The four bytes actually found at offset 0.
        found: [u8; 4],
    },

    /// Right magic, impossible header values — garbage or corruption.
    #[error(
        "{} looks like a GGML model but its header is corrupt: {field} is {value}, \
         which is not a possible value — re-download the file",
        .path.display()
    )]
    BadHparams {
        /// Path of the offending file.
        path: PathBuf,
        /// Which hparam is wrong.
        field: &'static str,
        /// The value read.
        value: i32,
    },
}

impl ModelError {
    /// The file the error is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Io { path, .. }
            | Self::TooShort { path, .. }
            | Self::GgufFormat { path }
            | Self::BadMagic { path, .. }
            | Self::BadHparams { path, .. } => path,
        }
    }
}

/// Renders the four leading bytes as hex plus a printable rendering, because
/// "bad magic" without the bytes is unactionable when someone has handed the
/// picker a zip or a text file.
fn describe_magic(found: &[u8; 4]) -> String {
    let hex = found
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let printable: String = found
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '.' })
        .collect();
    format!("{hex} (\"{printable}\")")
}

/// The result of scanning a directory: what is usable, and what was rejected
/// and why.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Usable models, sorted by file name.
    pub models: Vec<ModelInfo>,
    /// `.bin` files that failed validation, sorted by file name, each with the
    /// reason to show the user.
    pub rejected: Vec<(PathBuf, ModelError)>,
}

/// Check one file and describe it, or say precisely why it is unusable.
///
/// Reads at most the 48-byte fixed header. Checks run in the order that yields
/// the most specific message: size, then GGUF, then GGML magic, then hparams.
///
/// # Errors
/// Returns [`ModelError`] if the file cannot be read, is shorter than the
/// header, is GGUF, does not carry the GGML magic, or has a header value that
/// cannot be real.
pub fn validate_model(path: &Path) -> Result<ModelInfo, ModelError> {
    let mut file = File::open(path).map_err(|source| ModelError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let size_bytes = file
        .metadata()
        .map_err(|source| ModelError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();

    let mut header = [0_u8; HEADER_LEN];
    let filled = read_header(&mut file, &mut header).map_err(|source| ModelError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if filled < HEADER_LEN {
        return Err(ModelError::TooShort {
            path: path.to_path_buf(),
            // Trust the bytes we could actually read over the reported length:
            // on a file being written right now they can disagree.
            size_bytes: size_bytes.min(filled as u64),
        });
    }

    let magic: [u8; 4] = [header[0], header[1], header[2], header[3]];
    if magic == GGUF_MAGIC {
        return Err(ModelError::GgufFormat {
            path: path.to_path_buf(),
        });
    }
    if u32::from_le_bytes(magic) != GGML_MAGIC {
        return Err(ModelError::BadMagic {
            path: path.to_path_buf(),
            found: magic,
        });
    }

    let mut hparams = [0_i32; HPARAM_COUNT];
    for (i, slot) in hparams.iter_mut().enumerate() {
        let start = 4 + i * 4;
        let bytes: [u8; 4] = [
            header[start],
            header[start + 1],
            header[start + 2],
            header[start + 3],
        ];
        *slot = i32::from_le_bytes(bytes);
    }

    // `ftype` legitimately encodes 0 (f32); every other hparam is a count and
    // must be positive. A zero or negative one means we are looking at random
    // bytes that happened to start with the right four.
    for (i, &value) in hparams.iter().enumerate() {
        let is_ftype = i == HPARAM_COUNT - 1;
        let ok = if is_ftype { value >= 0 } else { value > 0 };
        if !ok {
            return Err(ModelError::BadHparams {
                path: path.to_path_buf(),
                field: HPARAM_NAMES[i],
                value,
            });
        }
    }

    let n_vocab = hparams[0];
    let n_mels = hparams[9];
    let kind = if n_vocab >= MULTILINGUAL_MIN_VOCAB {
        ModelKind::Multilingual
    } else {
        ModelKind::EnglishOnly
    };

    Ok(ModelInfo {
        path: path.to_path_buf(),
        kind,
        n_vocab,
        n_mels,
        size_bytes,
    })
}

/// Fill `header` from `file`, returning how many bytes were available. Short
/// reads are normal on some filesystems, so this loops rather than assuming one
/// `read` delivers 48 bytes; it never asks for more than the header.
fn read_header(file: &mut File, header: &mut [u8; HEADER_LEN]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < HEADER_LEN {
        match file.read(&mut header[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Scan `dir` for usable models, non-recursively.
///
/// Only regular files with a `.bin` extension (case-insensitive) are
/// considered; a models folder may reasonably hold READMEs, checksums and
/// subdirectories, and complaining about those would be noise. A missing or
/// empty directory is an empty result rather than an error — a fresh install
/// has no models and that empty state is the onboarding.
///
/// # Errors
/// Returns [`ModelError::Io`] if `dir` exists but cannot be listed.
pub fn scan_models_dir(dir: &Path) -> Result<ScanResult, ModelError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            tracing::info!(dir = %dir.display(), "models dir does not exist yet; no models");
            return Ok(ScanResult::default());
        }
        Err(source) => {
            return Err(ModelError::Io {
                path: dir.to_path_buf(),
                source,
            })
        }
    };

    let mut result = ScanResult::default();

    for entry in entries {
        let entry = entry.map_err(|source| ModelError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();

        if !has_bin_extension(&path) {
            continue;
        }
        // Follows symlinks deliberately: a symlinked model is a model. A
        // directory that happens to end in `.bin` is not, and is not worth
        // reporting to the user as a rejection.
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            _ => continue,
        }

        match validate_model(&path) {
            Ok(info) => result.models.push(info),
            Err(err) => {
                tracing::warn!(path = %path.display(), reason = %err, "ignoring file in models dir");
                result.rejected.push((path, err));
            }
        }
    }

    // Sorted by file name so the dropdown does not reshuffle between launches;
    // directory iteration order is not guaranteed on any platform.
    result.models.sort_by_key(|info| sort_key(&info.path));
    result.rejected.sort_by_key(|(path, _)| sort_key(path));

    tracing::info!(
        dir = %dir.display(),
        models = result.models.len(),
        rejected = result.rejected.len(),
        "scanned models dir"
    );

    Ok(result)
}

/// File name if there is one, whole path otherwise, so the ordering is total
/// even for odd paths.
fn sort_key(path: &Path) -> std::ffi::OsString {
    path.file_name().map_or_else(
        || path.as_os_str().to_os_string(),
        std::ffi::OsStr::to_os_string,
    )
}

fn has_bin_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Header of a plausible `base` model, with `n_vocab` and `n_mels` free to
    /// vary since those are the two fields anything reads.
    fn hparams(n_vocab: i32, n_mels: i32) -> [i32; HPARAM_COUNT] {
        [
            n_vocab, 1500, 512, 8, 6, // audio side
            448, 512, 8, 6, // text side
            n_mels, 1, // n_mels, ftype (1 = f16)
        ]
    }

    fn write_header(path: &Path, magic: [u8; 4], hparams: &[i32]) {
        let mut bytes = Vec::with_capacity(HEADER_LEN);
        bytes.extend_from_slice(&magic);
        for value in hparams {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        // A little payload past the header, as a real model has.
        bytes.extend_from_slice(&[0xAB; 32]);

        let mut file = File::create(path).expect("create test file");
        file.write_all(&bytes).expect("write test file");
    }

    fn ggml_magic_bytes() -> [u8; 4] {
        GGML_MAGIC.to_le_bytes()
    }

    fn write_model(dir: &Path, name: &str, n_vocab: i32) -> PathBuf {
        let path = dir.join(name);
        write_header(&path, ggml_magic_bytes(), &hparams(n_vocab, 80));
        path
    }

    #[test]
    fn english_only_vocab_classifies_as_english_only() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_model(dir.path(), "ggml-base.en.bin", 51864);

        let info = validate_model(&path).expect("valid model");

        assert_eq!(info.kind, ModelKind::EnglishOnly);
        assert!(!info.kind.is_multilingual());
        assert_eq!(info.n_vocab, 51864);
        assert_eq!(info.n_mels, 80);
        assert_eq!(info.path, path);
        assert!(info.size_bytes > HEADER_LEN as u64);
    }

    #[test]
    fn multilingual_vocab_classifies_as_multilingual() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = write_model(dir.path(), "ggml-base.bin", 51865);

        let info = validate_model(&path).expect("valid model");

        assert_eq!(info.kind, ModelKind::Multilingual);
    }

    #[test]
    fn large_v3_vocab_still_classifies_as_multilingual() {
        // 51866, not 51865: the threshold must not be an equality test.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("ggml-large-v3.bin");
        write_header(&path, ggml_magic_bytes(), &hparams(51866, 128));

        let info = validate_model(&path).expect("valid model");

        assert_eq!(info.kind, ModelKind::Multilingual);
        assert_eq!(info.n_mels, 128);
    }

    #[test]
    fn quantized_ftype_zero_is_accepted() {
        // ftype legitimately reaches 0; only the counts must be positive.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("ggml-small-q5_0.bin");
        let mut params = hparams(51865, 80);
        params[HPARAM_COUNT - 1] = 0;
        write_header(&path, ggml_magic_bytes(), &params);

        assert!(validate_model(&path).is_ok());
    }

    #[test]
    fn file_shorter_than_the_header_is_too_short() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("truncated.bin");
        fs::write(&path, &ggml_magic_bytes()[..]).expect("seed");

        match validate_model(&path) {
            Err(ModelError::TooShort { size_bytes, .. }) => assert_eq!(size_bytes, 4),
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn empty_file_is_too_short() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("empty.bin");
        fs::write(&path, b"").expect("seed");

        assert!(matches!(
            validate_model(&path),
            Err(ModelError::TooShort { size_bytes: 0, .. })
        ));
    }

    #[test]
    fn gguf_file_gets_its_own_message() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("model.bin");
        write_header(&path, GGUF_MAGIC, &hparams(51865, 80));

        let err = validate_model(&path).expect_err("gguf must be rejected");

        assert!(matches!(err, ModelError::GgufFormat { .. }));
        let message = err.to_string();
        assert!(message.contains("GGUF"), "message was: {message}");
        assert!(message.contains("GGML"), "message was: {message}");
    }

    #[test]
    fn foreign_magic_is_bad_magic_and_reports_the_bytes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("notes.bin");
        write_header(&path, *b"PK\x03\x04", &hparams(51865, 80));

        let err = validate_model(&path).expect_err("must be rejected");

        assert!(matches!(err, ModelError::BadMagic { .. }));
        let message = err.to_string();
        assert!(message.contains("50 4b 03 04"), "message was: {message}");
    }

    #[test]
    fn zero_hparam_is_bad_hparams() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("zeroed.bin");
        write_header(&path, ggml_magic_bytes(), &hparams(0, 80));

        match validate_model(&path) {
            Err(ModelError::BadHparams { field, value, .. }) => {
                assert_eq!(field, "n_vocab");
                assert_eq!(value, 0);
            }
            other => panic!("expected BadHparams, got {other:?}"),
        }
    }

    #[test]
    fn negative_hparam_is_bad_hparams() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("garbage.bin");
        write_header(&path, ggml_magic_bytes(), &hparams(51865, -1));

        match validate_model(&path) {
            Err(ModelError::BadHparams { field, value, .. }) => {
                assert_eq!(field, "n_mels");
                assert_eq!(value, -1);
            }
            other => panic!("expected BadHparams, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_is_an_io_error_naming_the_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("nope.bin");

        let err = validate_model(&path).expect_err("must fail");

        assert!(matches!(err, ModelError::Io { .. }));
        assert_eq!(err.path(), path);
    }

    #[test]
    fn scan_splits_valid_from_rejected_and_ignores_non_bin_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();

        write_model(root, "ggml-base.en.bin", 51864);
        write_model(root, "ggml-medium.bin", 51865);
        write_header(&root.join("broken.bin"), *b"junk", &hparams(51865, 80));
        fs::write(root.join("README.md"), b"not a model").expect("seed readme");
        fs::write(root.join("checksums.txt"), b"deadbeef").expect("seed checksums");
        fs::create_dir(root.join("subdir")).expect("seed subdir");
        write_model(&root.join("subdir"), "ggml-tiny.bin", 51865);

        let result = scan_models_dir(root).expect("scan");

        let names: Vec<_> = result
            .models
            .iter()
            .map(|m| {
                m.path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["ggml-base.en.bin", "ggml-medium.bin"]);

        assert_eq!(result.rejected.len(), 1);
        assert_eq!(
            result.rejected[0].0.file_name().expect("name"),
            "broken.bin"
        );
        assert!(matches!(result.rejected[0].1, ModelError::BadMagic { .. }));
    }

    #[test]
    fn scan_matches_the_bin_extension_case_insensitively() {
        let dir = tempfile::tempdir().expect("temp dir");
        write_model(dir.path(), "GGML-BASE.EN.BIN", 51864);

        let result = scan_models_dir(dir.path()).expect("scan");

        assert_eq!(result.models.len(), 1);
    }

    #[test]
    fn scan_ignores_a_directory_named_like_a_model() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("weights.bin")).expect("seed dir");

        let result = scan_models_dir(dir.path()).expect("scan");

        assert!(result.models.is_empty());
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn scan_of_a_missing_dir_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("no-such-folder");

        let result = scan_models_dir(&missing).expect("missing dir is not an error");

        assert!(result.models.is_empty());
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn scan_of_an_empty_dir_is_empty() {
        let dir = tempfile::tempdir().expect("temp dir");

        let result = scan_models_dir(dir.path()).expect("scan");

        assert!(result.models.is_empty());
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn scan_order_is_by_file_name_regardless_of_creation_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();

        for name in ["zeta.bin", "alpha.bin", "mid.bin"] {
            write_model(root, name, 51865);
        }
        for name in ["z-bad.bin", "a-bad.bin"] {
            write_header(&root.join(name), *b"junk", &hparams(51865, 80));
        }

        let result = scan_models_dir(root).expect("scan");

        let models: Vec<_> = result
            .models
            .iter()
            .map(|m| {
                m.path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(models, vec!["alpha.bin", "mid.bin", "zeta.bin"]);

        let rejected: Vec<_> = result
            .rejected
            .iter()
            .map(|(p, _)| p.file_name().expect("name").to_string_lossy().into_owned())
            .collect();
        assert_eq!(rejected, vec!["a-bad.bin", "z-bad.bin"]);
    }

    #[test]
    fn error_messages_name_the_file() {
        // These strings go straight into the rejected row the user reads.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("weird.bin");
        write_header(&path, *b"junk", &hparams(51865, 80));

        let err = validate_model(&path).expect_err("must fail");

        assert!(err.to_string().contains("weird.bin"));
    }

    // -----------------------------------------------------------------------
    // Rescan
    //
    // The user manages `models_dir` by hand now, by design,
    // so the whole of Sotone's model management is this function
    // run again. That makes "a second scan sees what changed on disk" a
    // promise rather than an implementation detail: these two tests are the
    // core seam behind the Rescan button, and they need no window and no
    // engine to state it.
    // -----------------------------------------------------------------------

    #[test]
    fn a_second_scan_sees_a_model_dropped_in_since_the_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        write_model(root, "ggml-base.en.bin", 51864);

        let first = scan_models_dir(root).expect("scan");
        assert_eq!(first.models.len(), 1);

        // What the user does now: drops two files into the folder by hand, one
        // of them a bad download.
        write_model(root, "ggml-medium.bin", 51865);
        write_header(&root.join("half.bin"), *b"GGUF", &hparams(51865, 80));

        let second = scan_models_dir(root).expect("rescan");

        let names: Vec<_> = second
            .models
            .iter()
            .map(|m| {
                m.path
                    .file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["ggml-base.en.bin", "ggml-medium.bin"]);
        // And the bad one is listed with its reason rather than dropped: it can
        // never be selected, and it explains itself here instead of at
        // transcribe time.
        assert_eq!(second.rejected.len(), 1);
        assert!(second.rejected[0].1.to_string().contains("GGUF"));
    }

    #[test]
    fn a_second_scan_no_longer_lists_a_model_deleted_since_the_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path();
        let doomed = write_model(root, "ggml-base.en.bin", 51864);
        write_model(root, "ggml-medium.bin", 51865);
        assert_eq!(scan_models_dir(root).expect("scan").models.len(), 2);

        // The user deletes it in their file manager — Sotone never does this,
        // and has no code left that could (invariant 4).
        fs::remove_file(&doomed).expect("the user's own delete");

        let second = scan_models_dir(root).expect("rescan");

        assert_eq!(second.models.len(), 1);
        assert_eq!(
            second.models[0].path.file_name().expect("name"),
            "ggml-medium.bin"
        );
        assert!(
            second.rejected.is_empty(),
            "a file that is gone is not a reject"
        );
    }
}
