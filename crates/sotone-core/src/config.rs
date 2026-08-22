//! Typed configuration over a format-preserving TOML document.
//!
//! The config file is the source of truth;
//! the UI is an editor over it. That is why this module carries the parsed
//! [`toml_edit::DocumentMut`] around inside [`Config`] instead of serialising
//! the struct back out: a save mutates the *same* document, so comments, key
//! order, blank lines and keys this version of Sotone has never heard of all
//! come back out byte-for-byte. Only values we actually changed are rewritten,
//! and even then the surrounding whitespace and trailing comment survive.
//!
//! Two consequences worth stating, because they are easy to "simplify" away:
//!
//! * A malformed file is an error, never a silent reset to defaults. Replacing
//!   a hand-edited file because of a typo destroys the user's work.
//! * Saves go through [`crate::fsutil::write_atomic`], so an interrupted save
//!   leaves the previous config intact.
//!
//! Nothing here touches the filesystem or the environment at import time —
//! paths are always passed in explicitly, which is also what makes the tests
//! device-free.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::fsutil::write_atomic;

/// Language token meaning "let whisper decide": `auto` as an honest
/// fallback.
pub const DEFAULT_LANGUAGE: &str = "auto";

/// Placeholder push-to-talk binding. The token format belongs to the hotkey
/// layer; here it is an opaque string. F13 is the default because no keyboard emits
/// it by accident and no game binds it.
pub const DEFAULT_HOTKEY: &str = "F13";

/// Default toggle-to-talk binding: press once to start, again to stop.
/// F14 for the same reason F13 is the push-to-talk default —
/// adjacent on a macro layer, equally unused by real applications — and
/// distinct from [`DEFAULT_HOTKEY`], because the two modes may not share a
/// binding while both are enabled.
pub const DEFAULT_TOGGLE_HOTKEY: &str = "F14";

/// Default note filename template. Token rendering happens elsewhere; this
/// layer only stores the string.
pub const DEFAULT_FILENAME_TEMPLATE: &str = "{project} {date} {time}.md";

/// Directory name used under the platform config and data directories.
const APP_DIR: &str = "sotone";

/// The skeleton written on first run.
///
/// Kept as text rather than built up through the document API so the header
/// comment, the commented-out optional keys and the spacing are all visible
/// here exactly as the user will first see them. Values are filled in by
/// [`Config::sync_into_document`] before the first save, so the placeholders
/// below only have to be type-correct.
const DEFAULT_DOCUMENT: &str = r#"# Sotone configuration.
#
# Sotone edits this file, but it is yours: comments, key order, blank lines and
# keys Sotone does not recognise are all preserved. Only values you change in
# the app get rewritten. If this file is ever unparseable, Sotone stops and says
# so rather than replacing it.

# Where Sotone looks for GGML model files. No model weights ship with the app.
models_dir = ""

# Transcription language, or "auto".
language = "auto"

# Push-to-talk binding: hold to record. Capture it in Settings rather than
# typing key codes.
hotkey = "F13"

# Toggle binding: press once to start a long finding, press again to stop.
# The two modes need different bindings, and at least one of them must stay
# enabled.
toggle_hotkey = "F14"
ptt_enabled = true
toggle_enabled = true

# The microphone is pinned by a substring of its name, never by index, because
# device indices move when you plug something in. Uncomment to pin one:
# mic_substring = "Yeti"

# Audio cues for recording / saved / error, and the always-on-top overlay.
audio_cues = true
overlay = true

# Which screen corner the overlay pill docks to — "bottomLeft" (the default),
# "bottomRight", "topLeft" or "topRight" — and how long a transcribed line
# stays on it, in seconds (3 to 60). Uncomment to change either without
# opening Settings.
# overlay_corner = "bottomRight"
# reveal_seconds = 10

# Window palette: "dark" or "light". Uncomment to switch without opening
# Settings; anything else is refused at startup rather than guessed at.
# theme = "light"

# Closing the window normally hides Sotone to the notification area, where it
# keeps listening, and the tray's "Exit Sotone" is the way out. Uncomment if you
# would rather the X quit outright.
# close_quits = true

# Deleted lines are normally still listed in the transcript, struck through,
# with a Restore beside them. Uncomment to keep them out of that view — the
# notes on disk are untouched either way, and every line stays in the note's
# history.
# hide_deleted = true

# Whether Sotone's first-run setup is behind you: "no" while it still has to
# run, "first-launch" for the one launch straight after it, "yes" afterwards.
# A config file without this key is treated as "yes" — this one is new, so it
# says "no".
onboarded = "no"

# Projects are appended below as [[projects]] tables.
"#;

/// Everything that can go wrong loading or saving the config.
///
/// Every variant carries the path, because "invalid TOML" with no filename is
/// useless when the user has several projects open.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but is not valid TOML. The file is left untouched.
    #[error("config file {} is not valid TOML: {source}", .path.display())]
    Parse {
        /// Path of the offending file.
        path: PathBuf,
        /// The underlying `toml_edit` parse error, line and column included.
        #[source]
        source: toml_edit::TomlError,
    },

    /// The file parses as TOML but does not describe a config Sotone can use —
    /// a key of the wrong type, or a project without a unique name.
    #[error("config file {} is not usable: {detail}", .path.display())]
    Invalid {
        /// Path of the offending file.
        path: PathBuf,
        /// What is wrong, in the user's terms.
        detail: String,
    },

    /// The file could not be read.
    #[error("could not read config file {}: {source}", .path.display())]
    Read {
        /// Path we tried to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The file could not be written. The previous contents are intact.
    #[error("could not write config file {}: {source}", .path.display())]
    Write {
        /// Path we tried to write.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The platform reported no config directory at all.
    #[error("this platform reports no user config directory; pass a config path explicitly")]
    NoConfigDir,
}

impl ConfigError {
    /// The file the error is about, when there is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Parse { path, .. }
            | Self::Invalid { path, .. }
            | Self::Read { path, .. }
            | Self::Write { path, .. } => Some(path),
            Self::NoConfigDir => None,
        }
    }
}

/// Which palette the window draws in.
///
/// Two values and no third: the design ships exactly two complete palettes and
/// every token name exists in both, so nothing in the UI branches on more than
/// this. Dark is the default, and — like every other default in this file — it
/// is not written to the config just to restate itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// The default. `data-theme` is simply absent in the window.
    #[default]
    Dark,
    /// `data-theme="light"` on the root element.
    Light,
}

impl Theme {
    /// The word this is written as, in the config file and in the DTO.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

impl std::str::FromStr for Theme {
    /// A sentence for whoever asked — the config loader turns it into a
    /// [`ConfigError::Invalid`], the Settings command hands it straight to the
    /// window.
    type Err = String;

    /// Case- and whitespace-insensitive, like the hotkey tokens: `"Light"` in a
    /// hand-edited file is not a typo worth refusing to start over. Anything
    /// else *is* refused rather than silently defaulted, because a silent
    /// default means the next save overwrites what the user typed.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            other => Err(format!(
                "`theme` should be \"dark\" or \"light\", but it is \"{other}\""
            )),
        }
    }
}

/// Which screen corner the overlay pill docks to.
///
/// Four values, spelled exactly as the window and the design name them. The
/// default is the design's: bottom-left, 18 px in. Like [`Theme`], an
/// unrecognised word is refused rather than guessed at, and the default is
/// never written to the file just to restate itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayCorner {
    /// The design's default.
    #[default]
    BottomLeft,
    /// Where an earlier status card used to sit.
    BottomRight,
    /// Top left.
    TopLeft,
    /// Top right.
    TopRight,
}

impl OverlayCorner {
    /// The word this is written as, in the config file and in the DTO.
    ///
    /// camelCase in a TOML file is unusual, and deliberate: the same four words
    /// travel to the window as a JavaScript discriminant, and one vocabulary
    /// from the file to the DOM attribute is worth more than TOML convention.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BottomLeft => "bottomLeft",
            Self::BottomRight => "bottomRight",
            Self::TopLeft => "topLeft",
            Self::TopRight => "topRight",
        }
    }

    /// Whether the pill hugs the left edge of the work area.
    #[must_use]
    pub const fn is_left(self) -> bool {
        matches!(self, Self::BottomLeft | Self::TopLeft)
    }

    /// Whether the pill hugs the top edge of the work area.
    #[must_use]
    pub const fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight)
    }
}

impl std::str::FromStr for OverlayCorner {
    /// A sentence for whoever asked, exactly as [`Theme`] does it.
    type Err = String;

    /// Case- and whitespace-insensitive: `"BottomLeft"` in a hand-edited file
    /// is not a typo worth refusing to start over. Anything else is refused,
    /// because a silent default would overwrite what the user typed.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "bottomleft" => Ok(Self::BottomLeft),
            "bottomright" => Ok(Self::BottomRight),
            "topleft" => Ok(Self::TopLeft),
            "topright" => Ok(Self::TopRight),
            other => Err(format!(
                "`overlay_corner` should be \"bottomLeft\", \"bottomRight\", \"topLeft\" or \
                 \"topRight\", but it is \"{other}\""
            )),
        }
    }
}

/// Whether the onboarding wizard still has to run.
///
/// Three states rather than a bool, and the third one is the interesting one:
/// the wizard's last screen promises "Sotone starts off", and on a fresh install
/// finishing it goes through a **restart** (there is no session to live-apply
/// into when no model was ever loaded). A bool would be true by then, so the
/// process the user actually lands in would come up armed and break that
/// promise the moment it started. [`Onboarded::FirstLaunch`] is the marker that
/// survives the restart: the next launch starts disarmed, consumes it, and every
/// launch after that is an ordinary one.
///
/// There is deliberately **no `Default` impl**, because the two defaults differ
/// and picking one would silently be wrong somewhere: a *missing* key means
/// [`Onboarded::Yes`] (every config that predates the wizard belongs to a user
/// who has been running Sotone for weeks, and must never be shown it), while
/// [`Config::default`] — which is what a fresh install writes — means
/// [`Onboarded::No`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Onboarded {
    /// The wizard has never been finished. It runs at this launch.
    No,
    /// The wizard finished and a restart followed. This launch is the first
    /// real one: it starts disarmed and then rewrites the key to
    /// [`Onboarded::Yes`].
    FirstLaunch,
    /// Ordinary. Every launch from here behaves as it always has.
    Yes,
}

/// What an absent `onboarded` key means: an existing user, never the wizard.
pub const ONBOARDED_WHEN_ABSENT: Onboarded = Onboarded::Yes;

impl Onboarded {
    /// The word this is written as, in the config file.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::FirstLaunch => "first-launch",
            Self::Yes => "yes",
        }
    }

    /// Whether the wizard is finished with — the one bit the window is told.
    #[must_use]
    pub const fn is_done(self) -> bool {
        matches!(self, Self::Yes)
    }
}

impl std::str::FromStr for Onboarded {
    /// A sentence for whoever asked, exactly as [`Theme`] does it.
    type Err = String;

    /// Case- and whitespace-insensitive, and an unrecognised word is refused
    /// rather than guessed at — the rule the whole module follows, and here it
    /// also stops a typo from quietly re-running the wizard over a configured
    /// machine.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "no" => Ok(Self::No),
            "first-launch" => Ok(Self::FirstLaunch),
            "yes" => Ok(Self::Yes),
            other => Err(format!(
                "`onboarded` should be \"no\", \"first-launch\" or \"yes\", but it is \"{other}\""
            )),
        }
    }
}

/// How long a transcribed line stays on the overlay pill, in seconds.
pub const DEFAULT_REVEAL_SECONDS: u32 = 10;

/// The shortest reveal the setting allows.
///
/// Below three seconds a line is gone before a glance can land on it, which is
/// the one thing the pill exists for.
pub const MIN_REVEAL_SECONDS: u32 = 3;

/// The longest. A minute of a line sitting over the thing under test is already
/// generous; forever is what the main window is for.
pub const MAX_REVEAL_SECONDS: u32 = 60;

/// One project: a notes folder plus the overrides that apply while it is
/// active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Unique key, and the `{project}` filename token.
    pub name: String,
    /// Arbitrary folder the rendered markdown is saved into.
    pub notes_dir: PathBuf,
    /// Filename template, tokens unrendered.
    pub filename_template: String,
    /// Optional header prepended to a rendered note, tokens unrendered.
    pub header_template: Option<String>,
    /// Model override; falls back to [`Config::active_model`].
    pub model: Option<String>,
    /// Language override; falls back to [`Config::language`].
    pub language: Option<String>,
    /// Words whisper keeps getting wrong for this project.
    pub vocabulary: Vec<String>,
    /// Whether a resumed session gets a divider in the markdown.
    pub session_dividers: bool,
}

impl Project {
    /// A project with the given name and notes folder, everything else default.
    #[must_use]
    pub fn new(name: impl Into<String>, notes_dir: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            notes_dir: notes_dir.into(),
            filename_template: DEFAULT_FILENAME_TEMPLATE.to_owned(),
            header_template: None,
            model: None,
            language: None,
            vocabulary: Vec::new(),
            session_dividers: true,
        }
    }
}

/// Why a project could not be renamed.
///
/// Not a `ConfigError`: none of these is a failure to read or write the file,
/// they are ordinary answers the window puts in the footer's message slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProjectRenameError {
    /// The name was empty or all whitespace.
    #[error("a project needs a name")]
    Blank,
    /// Nothing in the configuration is called that any more.
    #[error("there is no project by that name any more")]
    NoSuchProject,
    /// Another project already has the new name. **A rename never merges two
    /// projects**: silently folding one into another would move every note of
    /// one project into the other's folder rules without asking.
    #[error("there is already a project with that name")]
    NameTaken,
}

/// What a project rename should do about the folder on disk.
///
/// **The folder follows the name whenever that is safe** — files move when you
/// move them: renaming a project renames its notes folder,
/// *including a folder the user picked*, because that is what a VS Code user
/// expects a rename to mean. An earlier gate ("only a folder already called
/// what the project is called") is gone; it surprised people in the wild.
///
/// What is left refusing are the three cases where renaming would touch
/// something that is not this rename's to touch: a folder another project
/// shares, a target name already in use, and a new name a folder cannot be
/// called. Those are invariant 4, and they stay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderPlan {
    /// The folder is this project's own and moves with the name.
    Rename {
        /// Where it is now.
        from: PathBuf,
        /// The sibling it becomes.
        to: PathBuf,
    },
    /// The configuration changes and the folder stays where it is, with the
    /// reason said out loud rather than left for the user to discover.
    Keep(FolderKept),
}

/// Why a project rename left the folder alone.
///
/// "The folder is not called what the project is called" is no longer a reason
/// to keep it, so what was left of that case split into the two honest halves:
/// a rename with no folder to carry at all, and a new name that cannot be a
/// folder name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKept {
    /// There is nothing to rename: the project is not in the configuration, or
    /// its notes folder is blank or a filesystem root. Structural, and not
    /// normally reachable from the window — the rename command refuses an
    /// unknown project before it ever asks for a plan.
    NoFolder,
    /// Another project points at the same folder. Renaming it would move that
    /// project's notes as a side effect of renaming this one.
    Shared,
    /// A folder of the new name is already there. Never merged into, never
    /// numbered around — `fs::rename` would replace or fail, and neither is an
    /// answer a user asked for.
    Occupied,
    /// The new project name is not usable as a folder name (separators,
    /// control characters, a trailing dot). The folder could not end up called
    /// what the project is called, which is the whole point of carrying it.
    UnsafeName,
}

impl FolderKept {
    /// The footer's second half, in the app's voice. Kept beside the reason so
    /// the window cannot invent a fifth wording.
    #[must_use]
    pub const fn note(self) -> &'static str {
        match self {
            Self::NoFolder => "folder kept — there is no folder of its own to rename",
            Self::Shared => "folder kept — another project uses it",
            Self::Occupied => "folder kept — a folder of that name is already there",
            Self::UnsafeName => "folder kept — that name cannot be a folder name",
        }
    }
}

/// Whether a project's folder can be carried along by a rename.
///
/// Pure: the caller checks whether the target exists on disk and downgrades to
/// [`FolderKept::Occupied`] itself. Split that way because everything *here* is
/// a question about the configuration, and a filesystem probe in a function
/// like this is how a unit test ends up needing a temp directory.
#[must_use]
pub fn folder_plan(projects: &[Project], from: &str, to: &str) -> FolderPlan {
    let Some(project) = projects.iter().find(|p| p.name == from) else {
        return FolderPlan::Keep(FolderKept::NoFolder);
    };
    let dir = &project.notes_dir;

    // **No name check on the folder itself.** A folder the user
    // picked is still this project's home — "open folder" is the model — and
    // the rename carries it, deliberately. The
    // three refusals below are the ones that are about *someone else's* files.
    if projects
        .iter()
        .any(|other| other.name != from && other.notes_dir == *dir)
    {
        return FolderPlan::Keep(FolderKept::Shared);
    }
    // No parent means a filesystem root (or a blank path): there is no sibling
    // to rename it to, so there is nothing this plan can carry.
    let Some(parent) = dir.parent() else {
        return FolderPlan::Keep(FolderKept::NoFolder);
    };
    // A project name may contain anything; a folder name may not. If the new
    // name is not usable as written, the folder cannot end up called what the
    // project is called — which is the whole point of moving it — so it stays
    // where it is and the footer says so.
    if !crate::template::is_file_safe(to) {
        return FolderPlan::Keep(FolderKept::UnsafeName);
    }
    FolderPlan::Rename {
        from: dir.clone(),
        to: parent.join(to),
    }
}

/// Global settings plus the project list.
///
/// Clone-able and cheap to read; the embedded document is only consulted on
/// save.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory scanned for GGML models on launch.
    pub models_dir: PathBuf,
    /// Filename of the model in use, if one has been chosen. Validation is
    /// done elsewhere; here it is just a string.
    pub active_model: Option<String>,
    /// Global language, or `"auto"`.
    pub language: String,
    /// Opaque push-to-talk token (the hotkey layer interprets it).
    pub hotkey: String,
    /// Opaque toggle-to-talk token, same vocabulary as [`Config::hotkey`].
    pub toggle_hotkey: String,
    /// Whether the hold-to-talk mode is bound at all.
    pub ptt_enabled: bool,
    /// Whether the press-once/press-again mode is bound at all.
    pub toggle_enabled: bool,
    /// Substring matched against input device names. Never an index.
    pub mic_substring: Option<String>,
    /// Play the recording / saved / error cues.
    pub audio_cues: bool,
    /// Show the always-on-top overlay.
    pub overlay: bool,
    /// Which corner the overlay pill docks to.
    pub overlay_corner: OverlayCorner,
    /// How long a transcribed line stays on the pill, in seconds. Always
    /// between [`MIN_REVEAL_SECONDS`] and [`MAX_REVEAL_SECONDS`].
    pub reveal_seconds: u32,
    /// Which palette the window draws in.
    pub theme: Theme,
    /// Whether the window's close button quits Sotone instead of hiding it to
    /// the tray. Off by default: the X hides, and the tray's Exit is
    /// the one true way out.
    pub close_quits: bool,
    /// Whether soft-deleted lines are kept out of the transcript view.
    /// Off by default: a deleted line is listed struck through, with a
    /// Restore beside it.
    ///
    /// A *view* preference, like [`Config::theme`] — nothing on disk changes
    /// when it flips, and the note keeps every line it ever had.
    pub hide_deleted: bool,
    /// Whether the onboarding wizard still has to run. Absent from
    /// the file means [`ONBOARDED_WHEN_ABSENT`]; a fresh install writes
    /// [`Onboarded::No`], because that is what [`Config::default`] holds.
    pub onboarded: Onboarded,
    /// Name of the project currently selected, if any.
    pub active_project: Option<String>,
    /// All known projects, in file order.
    pub projects: Vec<Project>,

    /// The parsed file. Private: callers edit the typed fields, and `save`
    /// folds those edits back into this document so formatting survives.
    doc: DocumentMut,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            models_dir: default_models_dir(),
            active_model: None,
            language: DEFAULT_LANGUAGE.to_owned(),
            hotkey: DEFAULT_HOTKEY.to_owned(),
            toggle_hotkey: DEFAULT_TOGGLE_HOTKEY.to_owned(),
            ptt_enabled: true,
            toggle_enabled: true,
            mic_substring: None,
            audio_cues: true,
            overlay: true,
            overlay_corner: OverlayCorner::BottomLeft,
            reveal_seconds: DEFAULT_REVEAL_SECONDS,
            theme: Theme::Dark,
            close_quits: false,
            hide_deleted: false,
            // **Not** [`ONBOARDED_WHEN_ABSENT`]. `Config::default()` is what
            // [`Config::load`] writes when there is no file at all, which is
            // precisely a fresh install — so the wizard's "no" falls out of the
            // first save with no extra code. A file that exists without the key
            // is a different situation entirely and reads as "yes".
            onboarded: Onboarded::No,
            active_project: None,
            projects: Vec::new(),
            doc: default_document(),
        }
    }
}

impl Config {
    /// Load the config at `path`.
    ///
    /// If the file does not exist this returns defaults *and writes them*, so
    /// a fresh install has something to hand-edit. Any other failure is
    /// reported: a malformed file is never replaced with defaults.
    ///
    /// # Errors
    /// [`ConfigError::Parse`] for invalid TOML, [`ConfigError::Invalid`] for a
    /// key of the wrong type or a duplicate project name, [`ConfigError::Read`]
    /// or [`ConfigError::Write`] for I/O failures.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, path),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let mut config = Self::default();
                config.save(path)?;
                Ok(config)
            }
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parse `text` as a config. `path` is only used for error messages.
    ///
    /// # Errors
    /// As [`Config::load`], minus the I/O variants.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let doc: DocumentMut = text.parse().map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        let root = doc.as_table();
        let defaults = Self::default();

        let projects = read_projects(root, path)?;
        for (i, project) in projects.iter().enumerate() {
            if projects[..i].iter().any(|p| p.name == project.name) {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    detail: format!(
                        "two projects are named \"{}\"; project names are the key Sotone uses",
                        project.name
                    ),
                });
            }
        }

        // Read the recording-mode keys first: they are validated against each
        // other, and a config that binds no way to record at all must fail here
        // rather than start an app whose hotkeys do nothing.
        let hotkey = read_string(root, "hotkey", path)?.unwrap_or(defaults.hotkey);
        let toggle_hotkey =
            read_string(root, "toggle_hotkey", path)?.unwrap_or(defaults.toggle_hotkey);
        let ptt_enabled = read_bool(root, "ptt_enabled", path)?.unwrap_or(defaults.ptt_enabled);
        let toggle_enabled =
            read_bool(root, "toggle_enabled", path)?.unwrap_or(defaults.toggle_enabled);
        check_recording_modes(&hotkey, &toggle_hotkey, ptt_enabled, toggle_enabled, path)?;

        Ok(Self {
            models_dir: read_path(root, "models_dir", path)?.unwrap_or(defaults.models_dir),
            active_model: read_string(root, "active_model", path)?,
            language: read_string(root, "language", path)?.unwrap_or(defaults.language),
            hotkey,
            toggle_hotkey,
            ptt_enabled,
            toggle_enabled,
            mic_substring: read_string(root, "mic_substring", path)?,
            audio_cues: read_bool(root, "audio_cues", path)?.unwrap_or(defaults.audio_cues),
            overlay: read_bool(root, "overlay", path)?.unwrap_or(defaults.overlay),
            overlay_corner: read_overlay_corner(root, path)?.unwrap_or(defaults.overlay_corner),
            // Clamped rather than refused, unlike every *word* in this file: a
            // number outside the range still says plainly what the user wanted
            // (as long as possible, as short as possible), and the nearest
            // legal value is the honest reading of it. A value of the wrong
            // *type* is still an error.
            reveal_seconds: read_u32(root, "reveal_seconds", path)?
                .map_or(defaults.reveal_seconds, clamp_reveal_seconds),
            theme: read_theme(root, path)?.unwrap_or(defaults.theme),
            close_quits: read_bool(root, "close_quits", path)?.unwrap_or(defaults.close_quits),
            hide_deleted: read_bool(root, "hide_deleted", path)?.unwrap_or(defaults.hide_deleted),
            // Deliberately *not* `defaults.onboarded`: this file exists, so
            // whoever wrote it has been using Sotone, and the wizard is behind
            // them.
            onboarded: read_onboarded(root, path)?.unwrap_or(ONBOARDED_WHEN_ABSENT),
            active_project: read_string(root, "active_project", path)?,
            projects,
            doc,
        })
    }

    /// Fold the typed fields back into the document and write it atomically.
    ///
    /// Takes `&mut self` because the document is updated in place: two saves
    /// in a row produce identical bytes.
    ///
    /// # Errors
    /// [`ConfigError::Write`] if the file cannot be written. On failure the
    /// previous file is untouched (see [`crate::fsutil::write_atomic`]).
    pub fn save(&mut self, path: &Path) -> Result<(), ConfigError> {
        let rendered = self.to_toml_string();
        write_atomic(path, rendered.as_bytes()).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The exact bytes [`Config::save`] would write.
    ///
    /// Useful for previews and for tests that do not want a filesystem.
    pub fn to_toml_string(&mut self) -> String {
        self.sync_into_document();
        self.doc.to_string()
    }

    /// The project named `name`, if it exists.
    #[must_use]
    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    /// Mutable access to the project named `name`.
    pub fn project_mut(&mut self, name: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.name == name)
    }

    /// Rename a project, and everything in this file that names it.
    ///
    /// One mutation, not three, because the three have to land together: the
    /// project's own `name`, its `notes_dir` when the folder moved with it, and
    /// `active_project` when it was the active one. A config in which two of
    /// those agree and the third does not is a config where the user's notes
    /// have quietly changed project.
    ///
    /// `notes_dir` is `Some` only when the caller actually renamed the folder
    /// on disk **first** — the folder move is the step that can fail, and it
    /// fails before anything is written here.
    ///
    /// Nothing about drafts happens here: their `meta.project` is swept on the
    /// worker, because that is the thread that holds draft handles.
    ///
    /// # Errors
    /// [`ProjectRenameError`] for the three refusals, none of which mutates
    /// anything. In particular a rename onto an existing name is refused rather
    /// than merging the two projects.
    pub fn rename_project(
        &mut self,
        from: &str,
        to: &str,
        notes_dir: Option<PathBuf>,
    ) -> Result<(), ProjectRenameError> {
        if to.trim().is_empty() {
            return Err(ProjectRenameError::Blank);
        }
        if !self.projects.iter().any(|p| p.name == from) {
            return Err(ProjectRenameError::NoSuchProject);
        }
        // Case-sensitive, exactly as `project_create` is: the name is the key
        // `meta.project` stores and the `{project}` token renders, so two names
        // differing only in case are two projects.
        if to != from && self.projects.iter().any(|p| p.name == to) {
            return Err(ProjectRenameError::NameTaken);
        }

        for project in &mut self.projects {
            if project.name == from {
                project.name = to.to_owned();
                if let Some(dir) = notes_dir {
                    project.notes_dir = dir;
                }
                break;
            }
        }
        if self.active_project.as_deref() == Some(from) {
            self.active_project = Some(to.to_owned());
        }
        Ok(())
    }

    /// Remove a project from the configuration.
    ///
    /// **Config only.** The folder and every file in it stay exactly where they
    /// are, and no draft is touched: drafts that name this project fall into
    /// the "not in your projects" group, which is visible and honest, and
    /// recreating the project by name brings them back. Nothing in this
    /// codebase deletes a user's notes.
    ///
    /// Returns whether there was a project of that name.
    pub fn remove_project(&mut self, name: &str) -> bool {
        let before = self.projects.len();
        self.projects.retain(|project| project.name != name);
        if before == self.projects.len() {
            return false;
        }
        // An `active_project` naming a project that is gone is already treated
        // as "no active project" by every reader (see `active_project`), but
        // leaving the string in the file would resurrect it the moment someone
        // recreates that name by hand.
        if self.active_project.as_deref() == Some(name) {
            self.active_project = None;
        }
        true
    }

    /// The project [`Config::active_project`] points at, if it still exists.
    #[must_use]
    pub fn active_project(&self) -> Option<&Project> {
        self.active_project
            .as_deref()
            .and_then(|name| self.project(name))
    }

    /// Language actually used for a note: project override, else global.
    ///
    /// An override that is present but blank is treated as absent — an empty
    /// string in the file is a user mistake, not a request to transcribe in
    /// the "" language.
    #[must_use]
    pub fn effective_language<'a>(&'a self, project: Option<&'a Project>) -> &'a str {
        project
            .and_then(|p| p.language.as_deref())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.language)
    }

    /// Model actually used for a note: project override, else global.
    #[must_use]
    pub fn effective_model<'a>(&'a self, project: Option<&'a Project>) -> Option<&'a str> {
        project
            .and_then(|p| p.model.as_deref())
            .filter(|s| !s.trim().is_empty())
            .or(self.active_model.as_deref())
            .filter(|s| !s.trim().is_empty())
    }

    /// Write every typed field into the document, touching only the values
    /// that actually differ. This is the whole round-trip guarantee in one
    /// function.
    fn sync_into_document(&mut self) {
        let root = self.doc.as_table_mut();

        set_string_unless_default(
            root,
            "models_dir",
            &path_to_string(&self.models_dir),
            &path_to_string(&default_models_dir()),
        );
        set_optional_string(root, "active_model", self.active_model.as_deref());
        set_string_unless_default(root, "language", &self.language, DEFAULT_LANGUAGE);
        set_string_unless_default(root, "hotkey", &self.hotkey, DEFAULT_HOTKEY);
        set_string_unless_default(
            root,
            "toggle_hotkey",
            &self.toggle_hotkey,
            DEFAULT_TOGGLE_HOTKEY,
        );
        set_bool_unless_default(root, "ptt_enabled", self.ptt_enabled, true);
        set_bool_unless_default(root, "toggle_enabled", self.toggle_enabled, true);
        set_optional_string(root, "mic_substring", self.mic_substring.as_deref());
        set_bool_unless_default(root, "audio_cues", self.audio_cues, true);
        set_bool_unless_default(root, "overlay", self.overlay, true);
        set_string_unless_default(
            root,
            "overlay_corner",
            self.overlay_corner.as_str(),
            OverlayCorner::default().as_str(),
        );
        set_u32_unless_default(
            root,
            "reveal_seconds",
            clamp_reveal_seconds(self.reveal_seconds),
            DEFAULT_REVEAL_SECONDS,
        );
        set_string_unless_default(
            root,
            "theme",
            self.theme.as_str(),
            Theme::default().as_str(),
        );
        set_bool_unless_default(root, "close_quits", self.close_quits, false);
        set_bool_unless_default(root, "hide_deleted", self.hide_deleted, false);
        // The default compared against is the *absent* meaning, not
        // `Config::default()`'s: a file with no `onboarded` key belongs to a
        // user the wizard is behind, and it must not grow one just to say so.
        // A fresh install is the other side of the same rule — its value is
        // "no", which is not that default, so the key appears in the very first
        // file written.
        set_string_unless_default(
            root,
            "onboarded",
            self.onboarded.as_str(),
            ONBOARDED_WHEN_ABSENT.as_str(),
        );
        set_optional_string(root, "active_project", self.active_project.as_deref());

        sync_projects(root, &self.projects);
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Where the config lives when the user has not said otherwise:
/// `<platform config dir>/sotone/config.toml`.
///
/// # Errors
/// [`ConfigError::NoConfigDir`] if the platform reports no config directory,
/// which is the caller's cue to ask for a path rather than guess.
pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    dirs::config_dir()
        .map(|dir| dir.join(APP_DIR).join("config.toml"))
        .ok_or(ConfigError::NoConfigDir)
}

/// `<platform data dir>/sotone/models`, or a relative `models` folder if the
/// platform reports no data directory. Never panics: a missing data dir must
/// not stop the app from starting.
#[must_use]
pub fn default_models_dir() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("models"),
        |dir| dir.join(APP_DIR).join("models"),
    )
}

/// Where the onboarding wizard offers to keep notes: `<documents>/Sotone`.
///
/// **An invention**: there is no notes root in the
/// configuration — a project's `notes_dir` is an arbitrary folder the user
/// picked, and the create-a-project surface has no default at all. The
/// wizard needs *something* prefilled so its folder step can be passed with
/// Continue, so this is it, and it is only ever a suggestion: nothing is written
/// here until the wizard's project step actually creates a project under it.
///
/// Never panics, exactly like [`default_models_dir`]: a platform that reports no
/// documents folder falls back to the home directory, and one that reports
/// neither gets a relative path rather than stopping the app.
#[must_use]
pub fn default_notes_root() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .map_or_else(|| PathBuf::from("Sotone"), |dir| dir.join("Sotone"))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Parse the first-run skeleton.
///
/// The `expect` is a programmer error, not a user-facing one: the input is a
/// compile-time constant and `default_document_is_parseable` in the tests
/// below fails the build if it ever stops being valid TOML.
fn default_document() -> DocumentMut {
    DEFAULT_DOCUMENT
        .parse()
        .expect("DEFAULT_DOCUMENT is a constant and must parse")
}

// ---------------------------------------------------------------------------
// Reading typed values out of the document
//
// A key that is present with the wrong type is an error rather than a silent
// fallback: falling back would mean the next save quietly overwrites what the
// user typed.
// ---------------------------------------------------------------------------

fn type_error(path: &Path, key: &str, expected: &str, item: &Item) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        detail: format!(
            "`{key}` should be {expected}, but it is {}",
            item.type_name()
        ),
    }
}

fn read_string(table: &Table, key: &str, path: &Path) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None | Some(Item::None) => Ok(None),
        Some(item) => item
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| type_error(path, key, "a string", item)),
    }
}

fn read_bool(table: &Table, key: &str, path: &Path) -> Result<Option<bool>, ConfigError> {
    match table.get(key) {
        None | Some(Item::None) => Ok(None),
        Some(item) => item
            .as_bool()
            .map(Some)
            .ok_or_else(|| type_error(path, key, "true or false", item)),
    }
}

/// `theme`, if the file names one.
///
/// A value that is not one of the two palettes is a load error, exactly like a
/// key of the wrong type: the alternative is starting in a palette the file
/// does not describe and then overwriting the word the user wrote.
fn read_theme(table: &Table, path: &Path) -> Result<Option<Theme>, ConfigError> {
    read_string(table, "theme", path)?
        .map(|text| {
            text.parse::<Theme>()
                .map_err(|detail| ConfigError::Invalid {
                    path: path.to_path_buf(),
                    detail,
                })
        })
        .transpose()
}

/// `overlay_corner`, if the file names one.
///
/// Refused rather than defaulted for the same reason `theme` is: the next save
/// would otherwise overwrite the word the user wrote.
fn read_overlay_corner(table: &Table, path: &Path) -> Result<Option<OverlayCorner>, ConfigError> {
    read_string(table, "overlay_corner", path)?
        .map(|text| {
            text.parse::<OverlayCorner>()
                .map_err(|detail| ConfigError::Invalid {
                    path: path.to_path_buf(),
                    detail,
                })
        })
        .transpose()
}

/// `onboarded`, if the file names it.
///
/// Refused rather than defaulted for the same reason `theme` is — and with one
/// more reason of its own: a value nobody can parse must not fall back to "the
/// wizard has not run", which would put a configured machine back on step one.
fn read_onboarded(table: &Table, path: &Path) -> Result<Option<Onboarded>, ConfigError> {
    read_string(table, "onboarded", path)?
        .map(|text| {
            text.parse::<Onboarded>()
                .map_err(|detail| ConfigError::Invalid {
                    path: path.to_path_buf(),
                    detail,
                })
        })
        .transpose()
}

/// A non-negative whole number, or an error naming the key.
///
/// TOML integers are `i64`; anything negative or absurd is a type error here
/// rather than a silent wrap, because a wrapped duration would be a setting the
/// user cannot explain.
fn read_u32(table: &Table, key: &str, path: &Path) -> Result<Option<u32>, ConfigError> {
    match table.get(key) {
        None | Some(Item::None) => Ok(None),
        Some(item) => item
            .as_integer()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| type_error(path, key, "a whole number of seconds", item)),
    }
}

/// The reveal duration the app will actually honour.
///
/// One function, called both when the file is read and when it is written, so
/// the value in memory, the value on disk and the value the pill uses cannot
/// disagree.
#[must_use]
pub const fn clamp_reveal_seconds(seconds: u32) -> u32 {
    if seconds < MIN_REVEAL_SECONDS {
        MIN_REVEAL_SECONDS
    } else if seconds > MAX_REVEAL_SECONDS {
        MAX_REVEAL_SECONDS
    } else {
        seconds
    }
}

fn read_path(table: &Table, key: &str, path: &Path) -> Result<Option<PathBuf>, ConfigError> {
    Ok(read_string(table, key, path)?.map(PathBuf::from))
}

fn read_string_list(table: &Table, key: &str, path: &Path) -> Result<Vec<String>, ConfigError> {
    let Some(item) = table.get(key).filter(|i| !i.is_none()) else {
        return Ok(Vec::new());
    };
    let array = item
        .as_array()
        .ok_or_else(|| type_error(path, key, "a list of strings", item))?;

    array
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ConfigError::Invalid {
                    path: path.to_path_buf(),
                    detail: format!(
                        "`{key}` should contain only strings, but it has a {}",
                        v.type_name()
                    ),
                })
        })
        .collect()
}

fn read_projects(root: &Table, path: &Path) -> Result<Vec<Project>, ConfigError> {
    let Some(item) = root.get("projects").filter(|i| !i.is_none()) else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or_else(|| type_error(path, "projects", "a list of [[projects]] tables", item))?;

    let defaults = Project::new(String::new(), PathBuf::new());

    tables
        .iter()
        .map(|table| {
            let name = read_string(table, "name", path)?.ok_or_else(|| ConfigError::Invalid {
                path: path.to_path_buf(),
                detail: "every [[projects]] table needs a `name`".to_owned(),
            })?;

            Ok(Project {
                name,
                notes_dir: read_path(table, "notes_dir", path)?.unwrap_or_default(),
                filename_template: read_string(table, "filename_template", path)?
                    .unwrap_or_else(|| defaults.filename_template.clone()),
                header_template: read_string(table, "header_template", path)?,
                model: read_string(table, "model", path)?,
                language: read_string(table, "language", path)?,
                vocabulary: read_string_list(table, "vocabulary", path)?,
                session_dividers: read_bool(table, "session_dividers", path)?
                    .unwrap_or(defaults.session_dividers),
            })
        })
        .collect()
}

/// The two rules the recording modes have to obey, as a sentence to show the
/// user — or `None` when the combination is fine.
///
/// Public, and the single statement of the rule, because there are now two
/// callers with the same question. [`check_recording_modes`] asks it at load
/// time; the Settings tab asks it *before* writing a change, because
/// a settings screen that can save a config the next launch refuses to load
/// would be a trap with no way out inside the app. Two implementations of this
/// rule would eventually disagree, and the disagreement would be exactly that
/// trap.
///
/// The token comparison is case- and whitespace-insensitive because
/// [`Binding`](crate::hotkey::Binding) parses that way — `"f13"` and `" F13 "`
/// are the same physical key, so treating them as different bindings here would
/// let a real collision through. It stays a string compare rather than a call
/// into the hotkey layer: an unparseable token is that layer's error to report,
/// with its list of valid spellings.
#[must_use]
pub fn recording_mode_problem(
    hotkey: &str,
    toggle_hotkey: &str,
    ptt_enabled: bool,
    toggle_enabled: bool,
) -> Option<String> {
    if !ptt_enabled && !toggle_enabled {
        return Some(
            "at least one of push-to-talk and toggle must be enabled; set \
             `ptt_enabled` or `toggle_enabled` to true"
                .to_owned(),
        );
    }

    if ptt_enabled && toggle_enabled && hotkey.trim().eq_ignore_ascii_case(toggle_hotkey.trim()) {
        return Some(format!(
            "`hotkey` and `toggle_hotkey` are both \"{}\"; push-to-talk and toggle need \
             different bindings while both are enabled",
            hotkey.trim()
        ));
    }

    None
}

/// [`recording_mode_problem`] as a load-time failure.
///
/// Hard, in keeping with the rest of this module: a wrong config is reported,
/// never silently corrected. Silently re-enabling a mode the user switched off,
/// or silently preferring one of two identically bound modes, would leave the
/// user with a key that does something other than what their file says.
fn check_recording_modes(
    hotkey: &str,
    toggle_hotkey: &str,
    ptt_enabled: bool,
    toggle_enabled: bool,
    path: &Path,
) -> Result<(), ConfigError> {
    match recording_mode_problem(hotkey, toggle_hotkey, ptt_enabled, toggle_enabled) {
        Some(detail) => Err(ConfigError::Invalid {
            path: path.to_path_buf(),
            detail,
        }),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Writing typed values back into the document
// ---------------------------------------------------------------------------

/// Replace `key`'s value, keeping the decor — the whitespace before it and any
/// trailing `# comment` on the same line. Untouched if the value is already
/// equal, which is what keeps unchanged parts of the file byte-identical.
fn set_value(table: &mut Table, key: &str, new: Value) {
    if let Some(item) = table.get_mut(key) {
        if let Some(existing) = item.as_value() {
            if values_equal(existing, &new) {
                return;
            }
            let decor = existing.decor().clone();
            let mut replacement = new;
            *replacement.decor_mut() = decor;
            *item = Item::Value(replacement);
            return;
        }
    }
    table.insert(key, Item::Value(new));
}

/// Compare by meaning, not by representation: `'a'` and `"a"` are the same
/// string and must not trigger a rewrite that would reformat the user's file.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.value() == y.value(),
        (Value::Boolean(x), Value::Boolean(y)) => x.value() == y.value(),
        // `0x0a` and `10` are the same number, and rewriting one as the other
        // would reformat a file nobody asked to have reformatted.
        (Value::Integer(x), Value::Integer(y)) => x.value() == y.value(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b))
        }
        _ => false,
    }
}

/// Is `key` actually written in the file?
fn present(table: &Table, key: &str) -> bool {
    table.get(key).is_some_and(|item| !item.is_none())
}

fn set_string(table: &mut Table, key: &str, val: &str) {
    set_value(table, key, Value::from(val));
}

fn set_bool(table: &mut Table, key: &str, val: bool) {
    set_value(table, key, Value::from(val));
}

/// Set `key`, but do not *introduce* it just to restate a default.
///
/// A user who deleted `session_dividers` from their file did so on purpose;
/// growing the file back on every save turns a minimal hand-written config
/// into boilerplate. Once the value stops being the default it gets written,
/// because at that point it is information.
fn set_string_unless_default(table: &mut Table, key: &str, val: &str, default: &str) {
    if !present(table, key) && val == default {
        return;
    }
    set_string(table, key, val);
}

fn set_bool_unless_default(table: &mut Table, key: &str, val: bool, default: bool) {
    if !present(table, key) && val == default {
        return;
    }
    set_bool(table, key, val);
}

fn set_u32_unless_default(table: &mut Table, key: &str, val: u32, default: u32) {
    if !present(table, key) && val == default {
        return;
    }
    set_value(table, key, Value::from(i64::from(val)));
}

/// `None` means the key should not be in the file. Removing it is the only
/// honest representation of "unset", and it is our own key, not user prose.
fn set_optional_string(table: &mut Table, key: &str, val: Option<&str>) {
    match val {
        Some(v) => set_string(table, key, v),
        None => {
            if table.get(key).is_some_and(|i| i.is_value()) {
                table.remove(key);
            }
        }
    }
}

fn set_string_list(table: &mut Table, key: &str, values: &[String]) {
    if !present(table, key) && values.is_empty() {
        return;
    }
    let array: toml_edit::Array = values.iter().map(String::as_str).collect();
    set_value(table, key, Value::Array(array));
}

/// Sync the project list into `[[projects]]` positionally, so an untouched
/// project keeps its comments and key order. Only the tail is added or
/// removed.
fn sync_projects(root: &mut Table, projects: &[Project]) {
    if projects.is_empty() {
        if root.get("projects").is_some_and(Item::is_array_of_tables) {
            root.remove("projects");
        }
        return;
    }

    let item = root
        .entry("projects")
        .or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        *item = Item::ArrayOfTables(ArrayOfTables::new());
    }
    let Some(tables) = item.as_array_of_tables_mut() else {
        return;
    };

    while tables.len() > projects.len() {
        tables.remove(tables.len() - 1);
    }
    while tables.len() < projects.len() {
        tables.push(Table::new());
    }

    let defaults = Project::new(String::new(), PathBuf::new());

    for (table, project) in tables.iter_mut().zip(projects) {
        set_string(table, "name", &project.name);
        set_string_unless_default(
            table,
            "notes_dir",
            &path_to_string(&project.notes_dir),
            &path_to_string(&defaults.notes_dir),
        );
        set_string_unless_default(
            table,
            "filename_template",
            &project.filename_template,
            &defaults.filename_template,
        );
        set_optional_string(table, "header_template", project.header_template.as_deref());
        set_optional_string(table, "model", project.model.as_deref());
        set_optional_string(table, "language", project.language.as_deref());
        set_string_list(table, "vocabulary", &project.vocabulary);
        set_bool_unless_default(
            table,
            "session_dividers",
            project.session_dividers,
            defaults.session_dividers,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    /// A file that looks the way a hand-edited one does: comments everywhere,
    /// odd spacing, literal strings, a key Sotone has never heard of.
    const HAND_EDITED: &str = r#"# my sotone config
# do not lose this comment

models_dir = 'C:\models'      # where the ggml files live
language   =    "en"
hotkey = "MouseButton4"
audio_cues = false
overlay = true

# something a future version added
experimental_thing = 42

[[projects]]
# the game I'm testing this week
name = "Ludo"
notes_dir = 'C:\notes\ludo'
vocabulary = ["hitbox", "traversal"]
session_dividers = false
"#;

    #[test]
    fn default_document_is_parseable() {
        let doc = default_document();
        assert!(doc.as_table().contains_key("models_dir"));
        assert!(doc.to_string().starts_with("# Sotone configuration."));
    }

    #[test]
    fn defaults_match_the_spec() {
        let config = Config::default();
        assert_eq!(config.language, "auto");
        assert_eq!(config.hotkey, "F13");
        assert_eq!(config.toggle_hotkey, "F14");
        assert!(config.ptt_enabled);
        assert!(config.toggle_enabled);
        assert!(config.audio_cues);
        assert!(config.overlay);
        // The design's corner and reveal duration. Bottom-left
        // supersedes the earlier bottom-right card.
        assert_eq!(config.overlay_corner, OverlayCorner::BottomLeft);
        assert_eq!(config.reveal_seconds, 10);
        assert_eq!(config.theme, Theme::Dark);
        // The X hides to the tray unless the user says otherwise.
        assert!(!config.close_quits);
        // Deleted lines are listed, struck through, until the user
        // asks for them to be hidden.
        assert!(!config.hide_deleted);
        // The value a *fresh install* writes, which is the opposite of
        // what an absent key means.
        assert_eq!(config.onboarded, Onboarded::No);
        assert!(config.projects.is_empty());
        assert_eq!(config.active_model, None);
        assert_eq!(config.mic_substring, None);

        let project = Project::new("p", "dir");
        assert_eq!(project.filename_template, DEFAULT_FILENAME_TEMPLATE);
        assert!(project.session_dividers);
        assert!(project.vocabulary.is_empty());
    }

    #[test]
    fn theme_reads_both_palettes_and_refuses_a_third() {
        let path = Path::new("config.toml");

        // Absent is dark, and dark is not written back just to say so.
        let mut config = Config::from_toml("theme = \"light\"\n", path).expect("parse");
        assert_eq!(config.theme, Theme::Light);
        config.theme = Theme::Dark;
        assert_eq!(config.to_toml_string(), "theme = \"dark\"\n");

        // Spelling is forgiving, like the hotkey tokens.
        let config = Config::from_toml("theme = \" Light \"\n", path).expect("parse");
        assert_eq!(config.theme, Theme::Light);

        // Anything else stops the load rather than defaulting silently.
        let err = Config::from_toml("theme = \"solarized\"\n", path).expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("solarized"), "{message}");
        assert!(message.contains("\"dark\""), "{message}");

        // A key of the wrong type is the same kind of error it always was.
        assert!(Config::from_toml("theme = 3\n", path).is_err());
    }

    #[test]
    fn the_overlay_corner_reads_all_four_and_refuses_a_fifth() {
        let path = Path::new("config.toml");

        for (text, expected) in [
            ("bottomLeft", OverlayCorner::BottomLeft),
            ("bottomRight", OverlayCorner::BottomRight),
            ("topLeft", OverlayCorner::TopLeft),
            ("topRight", OverlayCorner::TopRight),
            // Forgiving about spelling, like `theme` and the hotkey tokens.
            (" TOPRIGHT ", OverlayCorner::TopRight),
        ] {
            let config =
                Config::from_toml(&format!("overlay_corner = \"{text}\"\n"), path).expect("parse");
            assert_eq!(config.overlay_corner, expected, "{text}");
        }

        // Anything else stops the load rather than defaulting silently.
        let err = Config::from_toml("overlay_corner = \"middle\"\n", path).expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("middle"), "{message}");
        assert!(message.contains("bottomLeft"), "{message}");

        // And a key of the wrong type is the same error it always was.
        assert!(Config::from_toml("overlay_corner = 3\n", path).is_err());
    }

    #[test]
    fn the_reveal_duration_is_clamped_rather_than_refused() {
        let path = Path::new("config.toml");

        let config = Config::from_toml("reveal_seconds = 25\n", path).expect("parse");
        assert_eq!(config.reveal_seconds, 25);

        // Out of range is the user's meaning, read at the nearest legal value.
        let config = Config::from_toml("reveal_seconds = 0\n", path).expect("parse");
        assert_eq!(config.reveal_seconds, MIN_REVEAL_SECONDS);
        let config = Config::from_toml("reveal_seconds = 6000\n", path).expect("parse");
        assert_eq!(config.reveal_seconds, MAX_REVEAL_SECONDS);
        // Negative is not a duration at all: that is a type error, not a clamp.
        assert!(Config::from_toml("reveal_seconds = -4\n", path).is_err());
        assert!(Config::from_toml("reveal_seconds = \"ten\"\n", path).is_err());

        // What is written back is what is honoured — the clamp runs on save too,
        // so the file can never keep a number the pill ignores.
        let mut config = Config::from_toml("reveal_seconds = 25\n", path).expect("parse");
        config.reveal_seconds = 900;
        assert_eq!(
            config.to_toml_string(),
            format!("reveal_seconds = {MAX_REVEAL_SECONDS}\n")
        );
    }

    #[test]
    fn a_minimal_file_does_not_grow_the_overlay_pill_keys() {
        // The theme rule, restated for the pill's two keys: a default is not
        // information, and a file that grows every key on every save stops
        // being the user's.
        let text = "hotkey = \"MouseButton4\"\n";
        let mut config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert_eq!(config.to_toml_string(), text);

        config.overlay_corner = OverlayCorner::TopRight;
        config.reveal_seconds = 4;
        let out = config.to_toml_string();
        assert!(out.contains("overlay_corner = \"topRight\""), "{out}");
        assert!(out.contains("reveal_seconds = 4"), "{out}");

        // And back to the defaults: once a key exists it keeps being written,
        // because at that point the file has an opinion.
        config.overlay_corner = OverlayCorner::BottomLeft;
        config.reveal_seconds = DEFAULT_REVEAL_SECONDS;
        let out = config.to_toml_string();
        assert!(out.contains("overlay_corner = \"bottomLeft\""), "{out}");
        assert!(out.contains("reveal_seconds = 10"), "{out}");
    }

    #[test]
    fn an_existing_config_never_sees_the_wizard_and_a_fresh_one_always_does() {
        let path = Path::new("config.toml");

        // The whole point of the three states: a file that predates the wizard
        // has no `onboarded` key, and its owner has been using Sotone for weeks.
        let mut config = Config::from_toml(HAND_EDITED, path).expect("parse");
        assert_eq!(config.onboarded, Onboarded::Yes);
        assert!(config.onboarded.is_done());
        // …and it is not written back just to restate what absence means.
        assert_eq!(config.to_toml_string(), HAND_EDITED);

        // A fresh install is the other side of the same rule.
        let fresh = Config::default();
        assert_eq!(fresh.onboarded, Onboarded::No);
        assert!(!fresh.onboarded.is_done());

        // All three words round-trip, spelling-forgivingly like `theme`.
        for (text, expected) in [
            ("no", Onboarded::No),
            ("first-launch", Onboarded::FirstLaunch),
            ("yes", Onboarded::Yes),
            (" First-Launch ", Onboarded::FirstLaunch),
        ] {
            let config =
                Config::from_toml(&format!("onboarded = \"{text}\"\n"), path).expect("parse");
            assert_eq!(config.onboarded, expected, "{text}");
        }

        // Anything else stops the load rather than putting a configured machine
        // back on step one.
        let err = Config::from_toml("onboarded = \"maybe\"\n", path).expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("maybe"), "{message}");
        assert!(message.contains("first-launch"), "{message}");
        assert!(Config::from_toml("onboarded = true\n", path).is_err());
    }

    #[test]
    fn the_first_launch_marker_is_written_and_then_consumed() {
        let dir = temp_dir();
        let path = dir.path().join("config.toml");

        // What a fresh install writes: the key is there, saying "no", because
        // "no" is not what absence means.
        let mut config = Config::load(&path).expect("first run");
        assert_eq!(config.onboarded, Onboarded::No);
        let text = fs::read_to_string(&path).expect("read back");
        assert!(text.contains("onboarded = \"no\""), "{text}");

        // The wizard finishing on the empty path: a restart follows, so the
        // marker has to survive it.
        config.onboarded = Onboarded::FirstLaunch;
        config.save(&path).expect("save");
        let reloaded = Config::load(&path).expect("relaunch");
        assert_eq!(reloaded.onboarded, Onboarded::FirstLaunch);
        assert!(!reloaded.onboarded.is_done());

        // That launch consumes it. Every launch after this one is ordinary.
        let mut consumed = reloaded;
        consumed.onboarded = Onboarded::Yes;
        consumed.save(&path).expect("save");
        let after = Config::load(&path).expect("next launch");
        assert_eq!(after.onboarded, Onboarded::Yes);
        // The key stays in the file once it is there — at that point it is
        // information, exactly like every other key this module writes.
        let text = fs::read_to_string(&path).expect("read back");
        assert!(text.contains("onboarded = \"yes\""), "{text}");
    }

    #[test]
    fn the_wizards_notes_root_is_a_suggestion_under_a_real_folder() {
        let root = default_notes_root();
        // The app's name and nothing more: this folder holds the projects, and
        // "Notes" on the end read as a second word for the same thing.
        assert!(root.ends_with("Sotone"), "{}", root.display());
        // Absolute on any machine that reports a documents or home folder,
        // relative only on one that reports neither — never a panic.
        assert!(root.is_absolute() || root == Path::new("Sotone"));
    }

    #[test]
    fn a_minimal_file_does_not_grow_a_theme_key() {
        let text = "hotkey = \"MouseButton4\"\n";
        let mut config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert_eq!(config.to_toml_string(), text);

        config.theme = Theme::Light;
        let out = config.to_toml_string();
        assert!(out.contains("theme = \"light\""), "{out}");
    }

    #[test]
    fn hand_edited_file_survives_a_load_set_save_cycle() {
        let path = Path::new("config.toml");
        let mut config = Config::from_toml(HAND_EDITED, path).expect("parse");

        assert_eq!(config.language, "en");
        assert_eq!(config.hotkey, "MouseButton4");
        assert_eq!(config.models_dir, PathBuf::from(r"C:\models"));
        assert!(!config.audio_cues);

        // Change exactly one value.
        config.language = "de".to_owned();
        let out = config.to_toml_string();

        // Everything except that one value is untouched, byte for byte.
        let expected = HAND_EDITED.replace(r#"language   =    "en""#, r#"language   =    "de""#);
        assert_eq!(out, expected);
    }

    #[test]
    fn saving_without_changes_rewrites_nothing() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        assert_eq!(config.to_toml_string(), HAND_EDITED);
    }

    #[test]
    fn a_minimal_file_does_not_grow_with_restated_defaults() {
        let text = "hotkey = \"MouseButton4\"\n";
        let mut config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert_eq!(config.to_toml_string(), text);

        // A value that stops being the default is information, so it lands.
        config.overlay = false;
        let out = config.to_toml_string();
        assert!(out.contains("overlay = false"));
        assert!(!out.contains("audio_cues"));
        // The same rule from the other side: `close_quits` is false
        // here because nothing said otherwise, so the file must not grow a key
        // restating it.
        assert!(!out.contains("close_quits"));
    }

    /// An absent key means "the X hides to the tray"; the setting
    /// only reaches the file once the user turns it on, and it survives the
    /// trip back.
    #[test]
    fn close_quits_is_absent_until_it_is_true() {
        let path = Path::new("config.toml");
        let mut config = Config::from_toml("hotkey = \"MouseButton4\"\n", path).expect("parse");
        assert!(!config.close_quits);

        config.close_quits = true;
        let out = config.to_toml_string();
        assert!(out.contains("close_quits = true"), "{out}");

        let reloaded = Config::from_toml(&out, path).expect("reparse");
        assert!(reloaded.close_quits);
    }

    /// Off again is still information: the key exists, so the value written
    /// into it is the one that stays there.
    #[test]
    fn close_quits_turned_off_again_stays_in_a_file_that_has_it() {
        let path = Path::new("config.toml");
        let mut config = Config::from_toml("close_quits = true\n", path).expect("parse");
        assert!(config.close_quits);

        config.close_quits = false;
        let out = config.to_toml_string();
        assert_eq!(out, "close_quits = false\n");
        assert!(!Config::from_toml(&out, path).expect("reparse").close_quits);
    }

    #[test]
    fn close_quits_of_the_wrong_type_is_refused() {
        let err = Config::from_toml("close_quits = \"yes\"\n", Path::new("config.toml"))
            .expect_err("a string is not a bool");
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err:?}");
    }

    /// The same rule as `close_quits`, for the view filter that used
    /// to live only in the window: an absent key means "deleted lines are
    /// listed", the key appears once the toggle is turned on, and the answer
    /// survives the trip back — which is the whole point of moving it here.
    #[test]
    fn hide_deleted_is_absent_until_it_is_true() {
        let path = Path::new("config.toml");
        let mut config = Config::from_toml("hotkey = \"MouseButton4\"\n", path).expect("parse");
        assert!(!config.hide_deleted);

        config.hide_deleted = true;
        let out = config.to_toml_string();
        assert!(out.contains("hide_deleted = true"), "{out}");

        let reloaded = Config::from_toml(&out, path).expect("reparse");
        assert!(reloaded.hide_deleted);
    }

    /// Off again is still information: the key exists, so the value written
    /// into it is the one that stays there.
    #[test]
    fn hide_deleted_turned_off_again_stays_in_a_file_that_has_it() {
        let path = Path::new("config.toml");
        let mut config = Config::from_toml("hide_deleted = true\n", path).expect("parse");
        assert!(config.hide_deleted);

        config.hide_deleted = false;
        let out = config.to_toml_string();
        assert_eq!(out, "hide_deleted = false\n");
        assert!(!Config::from_toml(&out, path).expect("reparse").hide_deleted);
    }

    #[test]
    fn hide_deleted_of_the_wrong_type_is_refused() {
        let err = Config::from_toml("hide_deleted = \"yes\"\n", Path::new("config.toml"))
            .expect_err("a string is not a bool");
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err:?}");
    }

    #[test]
    fn unknown_keys_and_comments_survive_a_project_edit() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        config.projects[0].session_dividers = true;
        let out = config.to_toml_string();

        assert!(out.contains("# do not lose this comment"));
        assert!(out.contains("experimental_thing = 42"));
        assert!(out.contains("# the game I'm testing this week"));
        assert!(out.contains(r#"vocabulary = ["hitbox", "traversal"]"#));
        assert!(out.contains("session_dividers = true"));
    }

    #[test]
    fn inline_comment_on_a_changed_value_survives() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        config.models_dir = PathBuf::from(r"D:\models");
        let out = config.to_toml_string();

        // The trailing comment belongs to the key, not to the old value.
        assert!(out.contains("# where the ggml files live"));

        // Quoting style is toml_edit's business; the value must survive a
        // Windows path with backslashes intact.
        let reloaded = Config::from_toml(&out, Path::new("config.toml")).expect("reparse");
        assert_eq!(reloaded.models_dir, PathBuf::from(r"D:\models"));
    }

    #[test]
    fn added_and_removed_projects_round_trip() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        config
            .projects
            .push(Project::new("Spreadsheet", PathBuf::from("/tmp/notes")));
        let out = config.to_toml_string();

        let reloaded = Config::from_toml(&out, Path::new("config.toml")).expect("reparse");
        assert_eq!(reloaded.projects.len(), 2);
        assert_eq!(reloaded.projects[1].name, "Spreadsheet");
        assert_eq!(
            reloaded.projects[1].filename_template,
            DEFAULT_FILENAME_TEMPLATE
        );
        assert!(reloaded.projects[1].session_dividers);
        assert_eq!(reloaded.projects[0].vocabulary, ["hitbox", "traversal"]);

        let mut trimmed = reloaded;
        trimmed.projects.clear();
        let out = trimmed.to_toml_string();
        assert!(!out.contains("[[projects]]"));
        assert!(out.contains("# do not lose this comment"));
    }

    // -----------------------------------------------------------------------
    // Renaming and removing a project
    // -----------------------------------------------------------------------

    #[test]
    fn renaming_a_project_moves_its_name_folder_and_active_flag_together() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        config.active_project = Some("Ludo".to_owned());

        config
            .rename_project(
                "Ludo",
                "Checkout rebuild",
                Some(PathBuf::from(r"C:\notes\Checkout rebuild")),
            )
            .expect("rename");

        assert_eq!(config.projects[0].name, "Checkout rebuild");
        assert_eq!(
            config.projects[0].notes_dir,
            PathBuf::from(r"C:\notes\Checkout rebuild")
        );
        assert_eq!(config.active_project.as_deref(), Some("Checkout rebuild"));

        // And it survives the round trip with the user's file intact.
        let out = config.to_toml_string();
        assert!(out.contains("# do not lose this comment"), "{out}");
        assert!(out.contains("# the game I'm testing this week"), "{out}");
        let reloaded = Config::from_toml(&out, Path::new("config.toml")).expect("reparse");
        assert_eq!(reloaded.active_project.as_deref(), Some("Checkout rebuild"));
        let project = reloaded.project("Checkout rebuild").expect("renamed");
        assert_eq!(project.vocabulary, ["hitbox", "traversal"]);
        assert!(!project.session_dividers, "the rename lost a setting");
    }

    #[test]
    fn renaming_a_project_leaves_a_kept_folder_and_an_unrelated_active_alone() {
        let mut config = Config::default();
        config
            .projects
            .push(Project::new("Ludo", r"C:\shared\notes"));
        config.projects.push(Project::new("Sotone", r"C:\sotone"));
        config.active_project = Some("Sotone".to_owned());

        // `None` is "the folder did not move" — the caller's decision, made
        // before anything was written.
        config
            .rename_project("Ludo", "Ludo 2", None)
            .expect("rename");

        assert_eq!(config.projects[0].name, "Ludo 2");
        assert_eq!(
            config.projects[0].notes_dir,
            PathBuf::from(r"C:\shared\notes")
        );
        assert_eq!(
            config.active_project.as_deref(),
            Some("Sotone"),
            "an unrelated active project was moved"
        );
    }

    #[test]
    fn a_project_rename_never_merges_and_never_invents() {
        let mut config = Config::default();
        config.projects.push(Project::new("Ludo", r"C:\ludo"));
        config.projects.push(Project::new("Sotone", r"C:\sotone"));

        assert_eq!(
            config.rename_project("Ludo", "Sotone", None),
            Err(ProjectRenameError::NameTaken)
        );
        assert_eq!(
            config.rename_project("Ludo", "   ", None),
            Err(ProjectRenameError::Blank)
        );
        assert_eq!(
            config.rename_project("Gone", "Anything", None),
            Err(ProjectRenameError::NoSuchProject)
        );
        // Nothing was written by any of the three.
        assert_eq!(config.projects[0].name, "Ludo");
        assert_eq!(config.projects[1].name, "Sotone");

        // Case is a difference, exactly as it is for `project_create`.
        config.rename_project("Ludo", "ludo", None).expect("rename");
        assert_eq!(config.projects[0].name, "ludo");
    }

    #[test]
    fn removing_a_project_touches_the_config_and_nothing_else() {
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");
        config
            .projects
            .push(Project::new("Spreadsheet", r"D:\sheets"));
        config.active_project = Some("Ludo".to_owned());

        assert!(config.remove_project("Ludo"));
        assert!(!config.remove_project("Ludo"), "removing twice");

        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].name, "Spreadsheet");
        assert_eq!(
            config.active_project, None,
            "the active project outlived the project"
        );

        // The positional `[[projects]]` sync shifts the surviving values up
        // into the first table and drops the last one, so a comment written
        // against a *specific* project table can migrate. Values survive, which
        // is what matters and what this pins — an accepted cost; the file's
        // own prose is untouched.
        let out = config.to_toml_string();
        let reloaded = Config::from_toml(&out, Path::new("config.toml")).expect("reparse");
        assert_eq!(reloaded.projects.len(), 1);
        assert_eq!(reloaded.projects[0].name, "Spreadsheet");
        assert_eq!(reloaded.projects[0].notes_dir, PathBuf::from(r"D:\sheets"));
        assert!(
            reloaded.projects[0].vocabulary.is_empty(),
            "a removed project's values leaked into its successor: {out}"
        );
        assert!(out.contains("# do not lose this comment"), "{out}");
        assert!(out.contains("experimental_thing = 42"), "{out}");
        assert!(!out.contains("active_project"), "{out}");
    }

    #[test]
    fn the_folder_follows_the_name_whenever_that_is_safe() {
        // Forward slashes throughout: this test runs on every CI platform and
        // a backslash is a separator only on Windows. `Path` equality compares
        // components, so the Windows-side join (`/notes\Checkout`) still
        // matches the expected `/notes/Checkout`.
        let own = vec![Project::new("Ludo", "/notes/Ludo")];
        assert_eq!(
            folder_plan(&own, "Ludo", "Checkout"),
            FolderPlan::Rename {
                from: PathBuf::from("/notes/Ludo"),
                to: PathBuf::from("/notes/Checkout"),
            }
        );

        // The one assertion that flipped when the gate was dropped: a folder
        // the user picked, whose name is not the project's, moves too — the
        // file follows the action.
        let picked = vec![Project::new("Ludo", "/Documents/notes")];
        assert_eq!(
            folder_plan(&picked, "Ludo", "Checkout"),
            FolderPlan::Rename {
                from: PathBuf::from("/Documents/notes"),
                to: PathBuf::from("/Documents/Checkout"),
            }
        );

        // Two projects, one folder: renaming it would move the other project's
        // notes as a side effect of renaming this one.
        let shared = vec![
            Project::new("Ludo", "/notes/Ludo"),
            Project::new("Sotone", "/notes/Ludo"),
        ];
        assert_eq!(
            folder_plan(&shared, "Ludo", "Checkout"),
            FolderPlan::Keep(FolderKept::Shared)
        );

        // A new name that a filesystem would not take as written.
        assert_eq!(
            folder_plan(&own, "Ludo", "Checkout: 1/2"),
            FolderPlan::Keep(FolderKept::UnsafeName)
        );

        // A project that is not there at all, and one whose folder is a
        // filesystem root with no sibling to become.
        assert_eq!(
            folder_plan(&own, "Gone", "Checkout"),
            FolderPlan::Keep(FolderKept::NoFolder)
        );
        let rooted = vec![Project::new("Ludo", "/")];
        assert_eq!(
            folder_plan(&rooted, "Ludo", "Checkout"),
            FolderPlan::Keep(FolderKept::NoFolder)
        );

        // Every kept reason says why, in the app's voice.
        for kept in [
            FolderKept::NoFolder,
            FolderKept::Shared,
            FolderKept::Occupied,
            FolderKept::UnsafeName,
        ] {
            assert!(kept.note().starts_with("folder kept — "), "{kept:?}");
        }
    }

    #[test]
    fn the_project_mutations_the_ui_makes_round_trip_a_hand_formatted_file() {
        // The Projects tab is an editor over this file, and every one
        // of its edits goes through here. A lost comment is a destroyed user
        // file in spirit (invariant 4), so the whole create/activate/update
        // sequence has to leave the rest of the document alone.
        let mut config = Config::from_toml(HAND_EDITED, Path::new("config.toml")).expect("parse");

        // Create, and make it active.
        config
            .projects
            .push(Project::new("Spreadsheet", r"D:\notes\sheets"));
        config.active_project = Some("Spreadsheet".to_owned());

        // Update: folder, filename template, header.
        let project = config.project_mut("Spreadsheet").expect("Spreadsheet");
        project.notes_dir = PathBuf::from(r"D:\notes\budget");
        project.filename_template = "{date} budget.md".to_owned();
        project.header_template = Some("# {project} — {datetime}".to_owned());

        let out = config.to_toml_string();

        // Everything the user wrote is still there.
        assert!(out.contains("# do not lose this comment"), "{out}");
        assert!(out.contains("experimental_thing = 42"), "{out}");
        assert!(out.contains("# the game I'm testing this week"), "{out}");
        assert!(
            out.contains(r#"vocabulary = ["hitbox", "traversal"]"#),
            "{out}"
        );
        assert!(out.contains(r#"language   =    "en""#), "{out}");

        let reloaded = Config::from_toml(&out, Path::new("config.toml")).expect("reparse");
        assert_eq!(reloaded.active_project.as_deref(), Some("Spreadsheet"));
        let project = reloaded.project("Spreadsheet").expect("Spreadsheet");
        assert_eq!(project.notes_dir, PathBuf::from(r"D:\notes\budget"));
        assert_eq!(project.filename_template, "{date} budget.md");
        assert_eq!(
            project.header_template.as_deref(),
            Some("# {project} — {datetime}")
        );
        // The untouched project is untouched.
        assert_eq!(reloaded.projects[0].name, "Ludo");
        assert_eq!(reloaded.projects[0].vocabulary, ["hitbox", "traversal"]);

        // Clearing the active project removes the key rather than writing "".
        let mut cleared = reloaded;
        cleared.active_project = None;
        let out = cleared.to_toml_string();
        assert!(!out.contains("active_project"), "{out}");

        // And an emptied header template is the key's absence, not an empty
        // string, so the note simply has no header again.
        let mut headerless = Config::from_toml(&out, Path::new("c.toml")).expect("reparse");
        headerless
            .project_mut("Spreadsheet")
            .expect("Spreadsheet")
            .header_template = None;
        let out = headerless.to_toml_string();
        assert!(!out.contains("header_template"), "{out}");
    }

    #[test]
    fn optional_values_are_removed_when_unset() {
        let mut config =
            Config::from_toml("active_model = \"small.bin\"\n", Path::new("config.toml"))
                .expect("parse");
        assert_eq!(config.active_model.as_deref(), Some("small.bin"));

        config.active_model = None;
        let out = config.to_toml_string();
        assert!(!out.contains("active_model"));
    }

    #[test]
    fn first_run_creates_the_file_with_defaults() {
        let dir = temp_dir();
        let path = dir.path().join("nested").join("config.toml");

        let config = Config::load(&path).expect("first run");
        assert_eq!(config.language, DEFAULT_LANGUAGE);
        assert!(path.exists());

        let text = fs::read_to_string(&path).expect("read back");
        assert!(text.starts_with("# Sotone configuration."));
        assert!(text.contains("hand-edit") || text.contains("preserved"));

        // The written file must reload to the same thing it was created from.
        let reloaded = Config::load(&path).expect("second load");
        assert_eq!(reloaded.language, config.language);
        assert_eq!(reloaded.hotkey, config.hotkey);
        assert_eq!(reloaded.models_dir, config.models_dir);
        assert_eq!(reloaded.audio_cues, config.audio_cues);
        assert_eq!(reloaded.overlay, config.overlay);
        assert_eq!(reloaded.overlay_corner, config.overlay_corner);
        assert_eq!(reloaded.reveal_seconds, config.reveal_seconds);
        assert_eq!(reloaded.close_quits, config.close_quits);
        assert_eq!(reloaded.hide_deleted, config.hide_deleted);
    }

    #[test]
    fn load_then_save_is_stable_on_disk() {
        let dir = temp_dir();
        let path = dir.path().join("config.toml");
        fs::write(&path, HAND_EDITED).expect("seed");

        let mut config = Config::load(&path).expect("load");
        config.save(&path).expect("save");

        assert_eq!(fs::read_to_string(&path).expect("read"), HAND_EDITED);
    }

    #[test]
    fn malformed_file_errors_with_its_path_and_is_left_alone() {
        let dir = temp_dir();
        let path = dir.path().join("config.toml");
        let broken = "language = \n[[projects]\n";
        fs::write(&path, broken).expect("seed");

        let err = Config::load(&path).expect_err("must not parse");
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert_eq!(err.path(), Some(path.as_path()));
        assert!(err.to_string().contains("config.toml"));

        // The user's file is untouched — no silent reset to defaults.
        assert_eq!(fs::read_to_string(&path).expect("read"), broken);
    }

    #[test]
    fn wrong_type_is_an_error_not_a_silent_default() {
        let err = Config::from_toml("language = 7\n", Path::new("config.toml"))
            .expect_err("must be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }));
        assert!(err.to_string().contains("language"));

        let err = Config::from_toml(
            "vocabulary = 1\n[[projects]]\nname = \"a\"\nvocabulary = [1]\n",
            Path::new("config.toml"),
        )
        .expect_err("must be rejected");
        assert!(err.to_string().contains("vocabulary"));
    }

    #[test]
    fn project_without_a_name_is_an_error() {
        let err = Config::from_toml("[[projects]]\nnotes_dir = \"x\"\n", Path::new("c.toml"))
            .expect_err("must be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn duplicate_project_names_are_an_error() {
        let text = "[[projects]]\nname = \"a\"\n\n[[projects]]\nname = \"a\"\n";
        let err = Config::from_toml(text, Path::new("c.toml")).expect_err("must be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }));
    }

    #[test]
    fn recording_modes_default_when_absent() {
        // A file that predates toggle mode still describes a working setup.
        let config = Config::from_toml("hotkey = \"MouseX2\"\n", Path::new("c.toml"))
            .expect("parse a pre-toggle file");
        assert_eq!(config.toggle_hotkey, DEFAULT_TOGGLE_HOTKEY);
        assert!(config.ptt_enabled);
        assert!(config.toggle_enabled);
    }

    #[test]
    fn explicit_recording_modes_round_trip() {
        let text = "hotkey = \"MouseX1\"\ntoggle_hotkey = \"MouseX2\"\n\
                    ptt_enabled = true\ntoggle_enabled = false\n";
        let mut config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert_eq!(config.hotkey, "MouseX1");
        assert_eq!(config.toggle_hotkey, "MouseX2");
        assert!(config.ptt_enabled);
        assert!(!config.toggle_enabled);
        assert_eq!(config.to_toml_string(), text);

        config.toggle_enabled = true;
        let out = config.to_toml_string();
        let reloaded = Config::from_toml(&out, Path::new("c.toml")).expect("reparse");
        assert!(reloaded.toggle_enabled);
        assert_eq!(reloaded.toggle_hotkey, "MouseX2");
    }

    #[test]
    fn disabling_both_recording_modes_is_an_error() {
        let text = "ptt_enabled = false\ntoggle_enabled = false\n";
        let err = Config::from_toml(text, Path::new("c.toml")).expect_err("must be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let message = err.to_string();
        assert!(message.contains("at least one"), "message: {message}");
    }

    #[test]
    fn the_same_binding_on_both_enabled_modes_is_an_error() {
        for toggle in ["F13", "f13", "  F13  "] {
            let text = format!("hotkey = \"F13\"\ntoggle_hotkey = \"{toggle}\"\n");
            let err = Config::from_toml(&text, Path::new("c.toml")).expect_err("must be rejected");
            assert!(matches!(err, ConfigError::Invalid { .. }));
            let message = err.to_string();
            assert!(
                message.contains("F13"),
                "message must name the token: {message}"
            );
        }
    }

    #[test]
    fn the_same_binding_is_fine_when_only_one_mode_is_enabled() {
        // Nothing is ambiguous if only one of the two is watching that key.
        let text = "hotkey = \"F13\"\ntoggle_hotkey = \"F13\"\ntoggle_enabled = false\n";
        let config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert_eq!(config.hotkey, config.toggle_hotkey);
        assert!(!config.toggle_enabled);

        let text = "hotkey = \"F13\"\ntoggle_hotkey = \"F13\"\nptt_enabled = false\n";
        let config = Config::from_toml(text, Path::new("c.toml")).expect("parse");
        assert!(!config.ptt_enabled);
    }

    /// The rule the Settings tab asks *before* it writes, which is
    /// the same rule the loader applies after. If these two ever disagree,
    /// Settings can write a file the next launch refuses to load.
    #[test]
    fn the_mode_rule_answers_the_same_way_the_loader_does() {
        // Refused: nothing left to record with.
        let both_off =
            recording_mode_problem("F13", "F14", false, false).expect("both off must be refused");
        assert!(both_off.contains("at least one"), "{both_off}");

        // Refused: two enabled modes on one physical key, however spelled.
        for toggle in ["F13", "f13", "  F13  "] {
            let clash = recording_mode_problem("F13", toggle, true, true)
                .expect("a shared binding must be refused");
            assert!(clash.contains("F13"), "{clash}");
        }

        // Allowed: distinct bindings, and a shared one where only one mode is on.
        assert_eq!(recording_mode_problem("F13", "F14", true, true), None);
        assert_eq!(recording_mode_problem("F13", "F13", true, false), None);
        assert_eq!(recording_mode_problem("F13", "F13", false, true), None);

        // And the loader is the same answer, reached through the same function.
        let text = "ptt_enabled = false\ntoggle_enabled = false\n";
        let err = Config::from_toml(text, Path::new("c.toml")).expect_err("must be rejected");
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn the_default_file_describes_two_distinct_usable_modes() {
        let mut config = Config::default();
        let text = config.to_toml_string();
        let reloaded = Config::from_toml(&text, Path::new("c.toml")).expect("reparse");
        assert_ne!(reloaded.hotkey, reloaded.toggle_hotkey);
        assert!(reloaded.ptt_enabled && reloaded.toggle_enabled);
    }

    #[test]
    fn overrides_resolve_project_then_global() {
        let text = r#"
language = "en"
active_model = "medium.bin"
active_project = "Game"

[[projects]]
name = "Game"
language = "de"
model = "large.bin"

[[projects]]
name = "Docs"

[[projects]]
name = "Blank"
language = "  "
model = ""
"#;
        let config = Config::from_toml(text, Path::new("c.toml")).expect("parse");

        let game = config.project("Game").expect("Game");
        assert_eq!(config.effective_language(Some(game)), "de");
        assert_eq!(config.effective_model(Some(game)), Some("large.bin"));

        let docs = config.project("Docs").expect("Docs");
        assert_eq!(config.effective_language(Some(docs)), "en");
        assert_eq!(config.effective_model(Some(docs)), Some("medium.bin"));

        // A blank override is a mistake, not a language named "".
        let blank = config.project("Blank").expect("Blank");
        assert_eq!(config.effective_language(Some(blank)), "en");
        assert_eq!(config.effective_model(Some(blank)), Some("medium.bin"));

        // No project at all falls back to global.
        assert_eq!(config.effective_language(None), "en");
        assert_eq!(config.effective_model(None), Some("medium.bin"));

        assert_eq!(
            config.active_project().map(|p| p.name.as_str()),
            Some("Game")
        );
    }

    #[test]
    fn session_dividers_defaults_on_is_never_written_unasked_and_round_trips() {
        // A config-only key: there is no UI for it,
        // so the file is the whole interface and it has to behave like every
        // other key here.
        let minimal = "[[projects]]\nname = \"Ludo\"\n";
        let mut config = Config::from_toml(minimal, Path::new("c.toml")).expect("parse");
        assert!(config.projects[0].session_dividers, "the default is on");
        // A default is not information: it must not appear just because we saved.
        assert_eq!(config.to_toml_string(), minimal);

        // Turned off by hand, it survives a load/save cycle untouched.
        let off = "[[projects]]\nname = \"Ludo\"\nsession_dividers = false  # one long note\n";
        let mut config = Config::from_toml(off, Path::new("c.toml")).expect("parse");
        assert!(!config.projects[0].session_dividers);
        assert_eq!(config.to_toml_string(), off);

        // And turning it back on is a value change, not a rewrite of the file.
        config.projects[0].session_dividers = true;
        let out = config.to_toml_string();
        assert!(out.contains("session_dividers = true"), "{out}");
        assert!(out.contains("# one long note"), "{out}");
    }

    #[test]
    fn a_wrong_typed_session_dividers_is_a_hard_load_error() {
        let err = Config::from_toml(
            "[[projects]]\nname = \"Ludo\"\nsession_dividers = \"yes\"\n",
            Path::new("c.toml"),
        )
        .expect_err("must be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }));
        assert!(err.to_string().contains("session_dividers"));
    }

    #[test]
    fn effective_model_is_none_when_nothing_is_chosen() {
        let config = Config::default();
        assert_eq!(config.effective_model(None), None);
    }

    #[test]
    fn project_mut_edits_in_place() {
        let mut config = Config::default();
        config.projects.push(Project::new("A", "dir"));
        config.project_mut("A").expect("A").session_dividers = false;
        assert!(!config.project("A").expect("A").session_dividers);
        assert!(config.project_mut("missing").is_none());
    }

    #[test]
    fn save_is_atomic_so_an_interrupted_write_keeps_the_old_config() {
        let dir = temp_dir();
        let path = dir.path().join("config.toml");
        fs::write(&path, HAND_EDITED).expect("seed");

        let mut config = Config::load(&path).expect("load");
        config.language = "fr".to_owned();

        // Stage the new bytes without renaming: that is what a crash mid-save
        // looks like from the outside.
        let staged =
            crate::fsutil::stage_temp(&path, config.to_toml_string().as_bytes()).expect("stage");
        assert_eq!(fs::read_to_string(&path).expect("read"), HAND_EDITED);

        // And a completed save swaps in the whole new file.
        config.save(&path).expect("save");
        assert!(fs::read_to_string(&path)
            .expect("read")
            .contains(r#"language   =    "fr""#));

        let _ = fs::remove_file(staged);
    }

    #[test]
    fn default_config_path_is_under_a_sotone_directory() {
        // `dirs` only reads environment conventions; nothing here leaves the
        // machine and nothing is created.
        match default_config_path() {
            Ok(path) => {
                assert!(path.ends_with(Path::new("sotone").join("config.toml")));
                assert!(!path.exists() || path.is_file());
            }
            Err(err) => assert!(matches!(err, ConfigError::NoConfigDir)),
        }
    }

    #[test]
    fn default_models_dir_is_absolute_or_a_sane_fallback() {
        let dir = default_models_dir();
        assert!(dir.ends_with("models"));
    }
}
