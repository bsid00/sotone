//! Where a draft's markdown goes: the *first* time it is saved, and the name a
//! drop into another project's folder has to fall back to.
//!
//! [`resolve`] is the first-save half. A draft binds to its `saved_path` and
//! re-saving rewrites that same file (Notepad semantics, enforced in
//! [`draft`](crate::draft)), so nothing here decides where an existing note
//! *lives* — a note's file moves when the user moves it, and that move is
//! [`Draft::move_to_project`](crate::draft::Draft::move_to_project)'s
//! job: files move when the user moves them.
//!
//! [`first_free`] is the second half, and it exists because of one word in that
//! section: a name clash on a drop **asks**, and "keep both" needs a name to
//! keep the second one under. It numbers, it never sanitizes —
//! [`template::file_safe`] is the only name cleaner
//! in this codebase and a second one would be a second answer to "what will
//! this file be called".
//!
//! # There is no projectless save
//!
//! [`resolve`] takes a `&Project`, not an `Option<&Project>`. Notes exist only
//! within projects, by design, so the `Documents/sotone` fallback an earlier
//! version shipped is gone along with the stand-in
//! filename it named files with. A caller with no project has nothing to resolve
//! and must say so — the shell answers that case with the pick-or-create popup,
//! not with a guessed folder. A project whose `notes_dir` is blank is refused by
//! the same caller for the same reason: saving into the process's current
//! directory would scatter notes wherever Sotone happened to be launched from.
//!
//! # Time is a parameter
//!
//! [`resolve`] never reads the clock. The caller passes the moment, so the shape
//! of a filename can be pinned by a test without a fixed-clock harness — the
//! same rule the draft store follows for `spoken_at`. [`resolve_now`] is the one
//! function here that looks at a clock, and it exists so `src-tauri` does not
//! have to carry a `chrono` of its own.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, Local};

use crate::config::Project;
use crate::template;

/// The full path a first save writes to: the project's notes folder, plus its
/// `filename_template` expanded for `now`.
///
/// The filename is sanitized by [`template::expand_filename`], so a project name
/// full of slashes still produces one writable file rather than a path.
#[must_use]
pub fn resolve(project: &Project, now: DateTime<FixedOffset>) -> PathBuf {
    project.notes_dir.join(template::expand_filename(
        &project.filename_template,
        &project.name,
        now,
    ))
}

/// [`resolve`] for the moment it is called.
#[must_use]
pub fn resolve_now(project: &Project) -> PathBuf {
    resolve(project, Local::now().fixed_offset())
}

/// How many numbered variants of a name are tried before giving up. A folder
/// holding a thousand notes of one name is not a case worth inventing a
/// thousand-and-first for; it is a case worth refusing out loud.
const MOST_COPIES: u32 = 999;

/// `note.md` numbered: `note (2).md`, `note (3).md`, …
///
/// The number goes before the extension, where a file manager puts it, because
/// this name is read by a human in a folder listing. Pure, so the shape can be
/// pinned without a temp directory.
#[must_use]
pub fn numbered(file_name: &str, n: u32) -> String {
    let path = Path::new(file_name);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) => {
            format!("{} ({n}).{}", stem.to_string_lossy(), ext.to_string_lossy())
        }
        // No extension, or a name that is nothing but one (".gitignore"): the
        // whole thing is the stem.
        _ => format!("{file_name} ({n})"),
    }
}

/// The first name in `dir` that nothing is sitting on: `file_name` itself, then
/// [`numbered`] from 2 up.
///
/// The "keep both" half of the clash question. It probes the
/// filesystem — the one function in this module that does — because "free" is a
/// fact about disk and nothing else can answer it.
///
/// `None` when every candidate up to [`MOST_COPIES`] is taken, which the caller
/// reports rather than papering over: inventing a name outside the sequence
/// would put the note somewhere the user cannot predict.
///
/// The check-then-write race (a file appearing between this answer and the move)
/// is documented and accepted here exactly as it is for
/// [`Draft::rename_note`](crate::draft::Draft::rename_note) — one user, one app
/// instance — and the move's own guard is what makes the window small rather
/// than zero.
#[must_use]
pub fn first_free(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let plain = dir.join(file_name);
    if !plain.exists() {
        return Some(plain);
    }
    (2..=MOST_COPIES)
        .map(|n| dir.join(numbered(file_name, n)))
        .find(|candidate| !candidate.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::DEFAULT_FILENAME_TEMPLATE;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).expect("test timestamp")
    }

    #[test]
    fn a_first_save_lands_in_the_projects_folder_under_its_template() {
        let project = Project::new("Ludo", r"C:\notes\ludo");
        assert_eq!(project.filename_template, DEFAULT_FILENAME_TEMPLATE);
        assert_eq!(
            resolve(&project, at("2026-08-07T14:32:07+02:00")),
            Path::new(r"C:\notes\ludo").join("Ludo 2026-08-07 14.32.07.md")
        );
    }

    #[test]
    fn a_custom_template_decides_the_whole_filename() {
        let mut project = Project::new("Ludo", "/notes");
        project.filename_template = "{date} findings".to_owned();
        assert_eq!(
            resolve(&project, at("2026-08-07T14:32:07Z")),
            Path::new("/notes").join("2026-08-07 findings.md")
        );
    }

    #[test]
    fn a_project_name_that_is_not_filename_safe_still_produces_one_file() {
        // The name is untrusted input. If this ever produced a separator the
        // "filename" would be a path, and the save would land somewhere nobody
        // chose.
        let project = Project::new(r"Ludo/2 : beta", "/notes");
        let path = resolve(&project, at("2026-08-07T14:32:07Z"));
        assert_eq!(path.parent(), Some(Path::new("/notes")));
        let name = path
            .file_name()
            .expect("a filename")
            .to_string_lossy()
            .into_owned();
        assert_eq!(name, "Ludo-2 - beta 2026-08-07 14.32.07.md");
    }

    #[test]
    fn a_numbered_name_keeps_its_extension_last() {
        // Where a file manager puts the number, because a folder listing is
        // where this name is read.
        assert_eq!(numbered("session 1.md", 2), "session 1 (2).md");
        assert_eq!(numbered("session 1.md", 10), "session 1 (10).md");
        // Dots inside the name are not the extension.
        assert_eq!(numbered("v1.2 notes.md", 3), "v1.2 notes (3).md");
        // No extension at all, and a name that is nothing but one.
        assert_eq!(numbered("notes", 2), "notes (2)");
        assert_eq!(numbered(".gitignore", 2), ".gitignore (2)");
    }

    #[test]
    fn keep_both_takes_the_first_free_number() {
        let dir = tempfile::tempdir().expect("temp dir");
        let at = dir.path();

        // Nothing there: the plain name is free and nothing is numbered.
        assert_eq!(
            first_free(at, "session 1.md"),
            Some(at.join("session 1.md"))
        );

        std::fs::write(at.join("session 1.md"), b"theirs").expect("seed");
        assert_eq!(
            first_free(at, "session 1.md"),
            Some(at.join("session 1 (2).md"))
        );

        // Gaps are filled rather than skipped past: the first free number wins.
        std::fs::write(at.join("session 1 (2).md"), b"theirs too").expect("seed");
        std::fs::write(at.join("session 1 (4).md"), b"and theirs").expect("seed");
        assert_eq!(
            first_free(at, "session 1.md"),
            Some(at.join("session 1 (3).md"))
        );

        // And nothing that was there was touched by asking.
        assert_eq!(
            std::fs::read(at.join("session 1.md")).expect("read"),
            b"theirs"
        );
    }

    #[test]
    fn resolve_now_agrees_with_resolve_about_everything_but_the_clock() {
        // The one function here that reads a clock, and all it may add is the
        // moment: same folder, same extension, same shape.
        let project = Project::new("Ludo", "/notes");
        let path = resolve_now(&project);
        assert_eq!(path.parent(), Some(Path::new("/notes")));
        let name = path.file_name().expect("a filename").to_string_lossy();
        assert!(name.starts_with("Ludo "), "{name}");
        assert!(name.ends_with(".md"), "{name}");
    }
}
