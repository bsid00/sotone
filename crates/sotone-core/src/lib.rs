//! Sotone core.
//!
//! Everything that does not need a window lives here so it stays testable
//! without one: the compile-time backend selection, the configuration layer,
//! the draft store, and the filesystem helpers that keep user files intact.

pub mod audio;
pub mod config;
pub mod cue;
pub mod draft;
pub mod fsutil;
pub mod hotkey;
pub mod model;
pub mod render;
pub mod savepath;
pub mod template;

// Transcription, and the session worker that owns a `Transcriber`, are the only
// two modules that need whisper.cpp. They are gated so `sotone-hook` — which
// observes two keys and writes them to a pipe — can link neither.
#[cfg(feature = "whisper")]
pub mod session;
#[cfg(feature = "whisper")]
pub mod transcribe;

// ---------------------------------------------------------------------------
// Backend feature arbitration
//
// At most one of `vulkan` / `cpu` / `metal`, and whisper.cpp only ever with one
// of them. Zero is legal here — a backend-free sotone-core is a real
// configuration (the hook helper), so "the app needs a backend" is asserted in
// the app's own main.rs. These checks run before anything else in the crate so a
// misconfigured build fails with one sentence instead of a wall of linker errors.
// ---------------------------------------------------------------------------

// Only reachable by naming `whisper` directly: the three backends all imply it.
// whisper.cpp with no backend selected is never what anyone means.
#[cfg(all(
    feature = "whisper",
    not(any(feature = "vulkan", feature = "cpu", feature = "metal"))
))]
compile_error!(
    "The `whisper` feature is not meant to be enabled on its own: it builds \
     whisper.cpp with no backend selected. Use --features vulkan, --features cpu, \
     or --features metal (macOS only) — each of those enables `whisper` for you."
);

#[cfg(any(
    all(feature = "vulkan", feature = "cpu"),
    all(feature = "vulkan", feature = "metal"),
    all(feature = "cpu", feature = "metal"),
))]
compile_error!(
    "Sotone needs exactly one backend feature, and more than one was enabled. \
     Pick a single one of: vulkan, cpu, metal. Note that features union across a \
     workspace build — use --no-default-features -p sotone if a sibling crate is \
     pulling in a second backend."
);

// Caught here rather than at link time, where it surfaces as an unresolved
// Metal symbol and tells the user nothing.
#[cfg(all(feature = "metal", not(target_os = "macos")))]
compile_error!(
    "The `metal` backend is macOS-only. On Windows and Linux build with \
     --features vulkan (GPU) or --features cpu (OpenBLAS/OpenMP)."
);

// ---------------------------------------------------------------------------
// Backend name
// ---------------------------------------------------------------------------

// Exactly one arm always defines BACKEND, including the zero-feature and
// multi-feature cases. That keeps the compile_error! above as the only
// diagnostic a misconfigured build sees.
#[cfg(feature = "vulkan")]
const BACKEND: &str = "vulkan";

#[cfg(all(feature = "cpu", not(feature = "vulkan")))]
const BACKEND: &str = "cpu";

#[cfg(all(feature = "metal", not(any(feature = "vulkan", feature = "cpu"))))]
const BACKEND: &str = "metal";

#[cfg(not(any(feature = "vulkan", feature = "cpu", feature = "metal")))]
const BACKEND: &str = "none";

/// The whisper backend this binary was compiled against.
///
/// This is the compiled *intent*. Vulkan can silently fall back to CPU at
/// runtime, so the app reports the actually selected
/// device alongside this string rather than in place of it.
///
/// Returns `"none"` in a build with no backend feature, which is a supported
/// configuration for consumers that never transcribe — `sotone-hook.exe` is one.
/// The app itself cannot reach it: `sotone`'s `main.rs` refuses to compile
/// backend-free.
#[must_use]
pub const fn backend_name() -> &'static str {
    BACKEND
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_name_matches_the_enabled_feature() {
        #[cfg(feature = "vulkan")]
        assert_eq!(backend_name(), "vulkan");

        #[cfg(all(feature = "cpu", not(feature = "vulkan")))]
        assert_eq!(backend_name(), "cpu");

        #[cfg(all(feature = "metal", not(any(feature = "vulkan", feature = "cpu"))))]
        assert_eq!(backend_name(), "metal");
    }

    #[test]
    fn backend_name_is_a_stable_lowercase_token() {
        // It ends up in log lines and in the Settings pane; keep it boring.
        let name = backend_name();
        assert!(!name.is_empty());
        assert!(name.chars().all(|c| c.is_ascii_lowercase()));
        // "none" is a real answer now, not a placeholder: a backend-free build
        // is what the hook helper links against.
        assert!(matches!(name, "vulkan" | "cpu" | "metal" | "none"));
    }
}
