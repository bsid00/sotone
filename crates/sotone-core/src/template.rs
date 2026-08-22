//! Token expansion for the two templates a project owns: the note's filename
//! and the header written at the top of it.
//!
//! Seven tokens are defined — `{project} {date} {time} {datetime} {session} {n}
//! {build}`. This module implements the first four. The other three, and anything
//! else in braces, are left **verbatim**: a template is the user's text, and a
//! token this version has never heard of is far better shown as `{n}` than
//! silently deleted. That is also what makes the deferred three additive — the
//! day they mean something, files named with them stop containing them and
//! nothing else changes.
//!
//! # Two modes, and why they are not one function with a flag
//!
//! A filename and a line of markdown obey different rules. `:` is legal in a
//! header and illegal in a Windows filename; a header has no extension and must
//! not grow one; and the *project name* is untrusted input that lands in both —
//! it may contain a slash, a quote, or nothing at all. So
//! [`expand_filename`] sanitizes its whole result and guarantees something
//! writable comes out, while [`expand_text`] hands back exactly what the
//! template said. Two entry points, one scanner, no boolean at the call sites.
//!
//! # Time is a parameter
//!
//! Nothing here reads the clock — the caller passes the moment, the same rule
//! [`savepath`](crate::savepath) and the draft store follow. That is what lets a
//! filename's shape be pinned by a test without a fixed-clock harness.

use chrono::{DateTime, FixedOffset, Local};

/// Characters Windows refuses in a filename. Replaced rather than dropped, so
/// two project names that differ only in punctuation cannot collapse onto one
/// filename.
const FORBIDDEN: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// What a forbidden character becomes.
const REPLACEMENT: char = '-';

/// Used when the expanded, sanitized filename is empty — a template of `{n}`
/// alone with the deferred token stripped by a future version, say, or a
/// project named entirely in slashes. A note has to land *somewhere*.
const EMPTY_FALLBACK: &str = "note";

/// Extension every note gets, whether or not the template said so.
const EXTENSION: &str = ".md";

/// Which set of rules an expansion is playing by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Dots in the time, sanitized result, `.md` guaranteed.
    Filename,
    /// Colons in the time, no sanitization, no extension.
    Text,
}

/// A project's `filename_template`, expanded and made safe to write.
///
/// The result is always a single filename — never a path — and always ends in
/// `.md`.
#[must_use]
pub fn expand_filename(template: &str, project: &str, now: DateTime<FixedOffset>) -> String {
    file_safe(&expand(template, project, now, Mode::Filename))
}

/// A project's `header_template`, expanded for the top of the markdown.
///
/// No sanitization: this is file *content*, and every character the user typed
/// is legal there.
#[must_use]
pub fn expand_text(template: &str, project: &str, now: DateTime<FixedOffset>) -> String {
    expand(template, project, now, Mode::Text)
}

/// [`expand_filename`] for the moment it is called.
///
/// The two `_now` functions are the only clock readers in this module, and they
/// exist so `src-tauri` does not have to carry a `chrono` of its own — the same
/// arrangement [`savepath::resolve_now`](crate::savepath::resolve_now) has. The
/// pure halves above are still where every decision is made.
#[must_use]
pub fn expand_filename_now(template: &str, project: &str) -> String {
    expand_filename(template, project, Local::now().fixed_offset())
}

/// [`expand_text`] for the moment it is called.
#[must_use]
pub fn expand_text_now(template: &str, project: &str) -> String {
    expand_text(template, project, Local::now().fixed_offset())
}

/// Whether `name` already obeys the filename rules, so a **folder** of that
/// name can be created exactly as written.
///
/// Asks [`file_safe`] rather than restating its rules, so there is still only
/// one place that knows what a legal name is: if sanitizing changes anything
/// beyond appending the extension, the folder would not be called what the
/// project is called and the project rename keeps the folder instead. A name
/// that already ends in `.md` answers `false`, which is the conservative
/// direction and the only one that costs nothing.
#[must_use]
pub fn is_file_safe(name: &str) -> bool {
    file_safe(name) == format!("{name}{EXTENSION}")
}

/// The scanner. Walks the template once, substituting the tokens it knows and
/// copying everything else — including unknown tokens, braces and all.
fn expand(template: &str, project: &str, now: DateTime<FixedOffset>, mode: Mode) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // An unclosed brace is not a token, it is text. Copy the remainder
            // and stop looking.
            out.push_str(&rest[open..]);
            return out;
        };
        let name = &after[..close];
        match token(name, project, now, mode) {
            Some(value) => out.push_str(&value),
            // Verbatim, braces included: `{n}` has to survive to be implemented.
            None => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
        }
        rest = &after[close + 1..];
    }

    out.push_str(rest);
    out
}

/// What one token expands to, or `None` for "leave it alone".
fn token(name: &str, project: &str, now: DateTime<FixedOffset>, mode: Mode) -> Option<String> {
    let date = || now.format("%Y-%m-%d").to_string();
    let time = || match mode {
        // Dots, because `:` is not a legal character in a Windows filename and a
        // template that produces an unwritable path is a bug the user finds at
        // the worst possible moment.
        Mode::Filename => now.format("%H.%M.%S").to_string(),
        Mode::Text => now.format("%H:%M:%S").to_string(),
    };

    match name {
        "project" => Some(project.to_owned()),
        "date" => Some(date()),
        "time" => Some(time()),
        "datetime" => Some(format!("{} {}", date(), time())),
        // `session`, `n`, `build` and anything else: deferred or unknown, and
        // both mean the same thing here.
        _ => None,
    }
}

/// Turn an expanded filename into one that can actually be created.
///
/// Order matters: replace, then trim, then default, then extend. Trimming
/// before the replacement would leave a trailing `/` as a `-`; extending before
/// the trim would produce `note.md.` on a template ending in a dot.
///
/// Public for the rename path, which renames a note's file to a name the user typed
/// straight into a field. That name needs exactly these rules and **not** the
/// token expansion around them — a note called `{date}` is a note called
/// `{date}`, not today — so the rename calls this rather than
/// [`expand_filename`]. One sanitizer, because two would eventually disagree
/// about what the file on disk is called and the external-edit guard hashes
/// whichever one is right.
#[must_use]
pub fn file_safe(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            // Control characters are illegal in a Windows filename too, and a
            // newline pasted into a project name is exactly the kind of thing
            // that reaches here.
            if FORBIDDEN.contains(&c) || c.is_control() {
                REPLACEMENT
            } else {
                c
            }
        })
        .collect();

    // Windows silently strips trailing dots and spaces from a name at creation
    // time, which would make the file's real name differ from the one the
    // conflict guard remembers. Strip them ourselves so the two agree.
    let trimmed = replaced.trim_start().trim_end_matches(['.', ' ', '\t']);

    let mut out = if trimmed.is_empty() {
        EMPTY_FALLBACK.to_owned()
    } else {
        trimmed.to_owned()
    };

    if !out.to_ascii_lowercase().ends_with(EXTENSION) {
        out.push_str(EXTENSION);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::DEFAULT_FILENAME_TEMPLATE;

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).expect("test timestamp")
    }

    fn moment() -> DateTime<FixedOffset> {
        at("2026-08-07T14:32:07+02:00")
    }

    #[test]
    fn the_default_template_names_the_note_as_designed() {
        assert_eq!(
            expand_filename(DEFAULT_FILENAME_TEMPLATE, "Ludo", moment()),
            "Ludo 2026-08-07 14.32.07.md"
        );
    }

    #[test]
    fn every_implemented_token_expands_in_filename_mode() {
        assert_eq!(expand_filename("{project}", "Ludo", moment()), "Ludo.md");
        assert_eq!(expand_filename("{date}", "Ludo", moment()), "2026-08-07.md");
        assert_eq!(expand_filename("{time}", "Ludo", moment()), "14.32.07.md");
        assert_eq!(
            expand_filename("{datetime}", "Ludo", moment()),
            "2026-08-07 14.32.07.md"
        );
    }

    #[test]
    fn text_mode_uses_colons_and_adds_nothing() {
        assert_eq!(expand_text("{time}", "Ludo", moment()), "14:32:07");
        assert_eq!(
            expand_text("# {project} — {datetime}", "Ludo", moment()),
            "# Ludo — 2026-08-07 14:32:07"
        );
        // No extension, no sanitization: a header is file content.
        assert_eq!(
            expand_text("notes for {project}: session", "A/B", moment()),
            "notes for A/B: session"
        );
    }

    #[test]
    fn a_template_with_no_tokens_is_returned_as_it_was() {
        assert_eq!(
            expand_text("plain header", "Ludo", moment()),
            "plain header"
        );
        assert_eq!(
            expand_filename("findings.md", "Ludo", moment()),
            "findings.md"
        );
        // …except that a filename always ends up with the extension.
        assert_eq!(expand_filename("findings", "Ludo", moment()), "findings.md");
    }

    #[test]
    fn the_deferred_and_unknown_tokens_are_left_verbatim() {
        // The three defined tokens this module does not implement, plus a typo and an
        // unclosed brace. None of them may be silently eaten.
        assert_eq!(
            expand_text(
                "{session} {n} {build} {projekt} {unclosed",
                "Ludo",
                moment()
            ),
            "{session} {n} {build} {projekt} {unclosed"
        );
        assert_eq!(
            expand_filename("{project} {n}", "Ludo", moment()),
            "Ludo {n}.md"
        );
    }

    #[test]
    fn a_project_name_full_of_forbidden_characters_still_names_a_file() {
        // The project name is untrusted input; this is the case that decides
        // whether a save fails at the worst possible moment.
        let name = r#"a<b>c:d"e/f\g|h?i*j"#;
        let out = expand_filename("{project}", name, moment());
        assert_eq!(out, "a-b-c-d-e-f-g-h-i-j.md");
        for bad in FORBIDDEN {
            assert!(!out.contains(bad), "{out} contains {bad}");
        }
    }

    #[test]
    fn a_name_that_sanitizes_to_nothing_falls_back_rather_than_vanishing() {
        assert_eq!(expand_filename("{project}", "", moment()), "note.md");
        assert_eq!(expand_filename("{project}", "///", moment()), "---.md");
        assert_eq!(expand_filename("", "Ludo", moment()), "note.md");
        assert_eq!(expand_filename("   ", "Ludo", moment()), "note.md");
    }

    #[test]
    fn trailing_dots_and_spaces_go_before_the_extension_does() {
        // Windows strips these itself at creation time, which would make the
        // real filename differ from the one the save guard remembers.
        assert_eq!(expand_filename("notes. . .", "Ludo", moment()), "notes.md");
        assert_eq!(expand_filename("notes   ", "Ludo", moment()), "notes.md");
        assert_eq!(expand_filename("  notes", "Ludo", moment()), "notes.md");
    }

    #[test]
    fn an_existing_extension_is_not_doubled_whatever_its_case() {
        assert_eq!(expand_filename("notes.md", "Ludo", moment()), "notes.md");
        assert_eq!(expand_filename("notes.MD", "Ludo", moment()), "notes.MD");
        // A different extension is the user's business; ours is added to it.
        assert_eq!(
            expand_filename("notes.txt", "Ludo", moment()),
            "notes.txt.md"
        );
    }

    #[test]
    fn a_filename_never_contains_a_character_windows_refuses() {
        let out = expand_filename(DEFAULT_FILENAME_TEMPLATE, "Ludo", moment());
        for bad in FORBIDDEN {
            assert!(!out.contains(bad), "{out} contains {bad}");
        }
        // And never a control character, so a pasted newline cannot make one
        // filename look like two.
        let out = expand_filename("{project}", "one\ntwo", moment());
        assert_eq!(out, "one-two.md");
    }

    #[test]
    fn the_moment_is_read_in_the_offset_it_was_given() {
        // The note is named for the clock on the user's wall, exactly as its
        // line timestamps are.
        assert_eq!(
            expand_filename("{datetime}", "p", at("2026-08-05T14:32:07+02:00")),
            "2026-08-05 14.32.07.md"
        );
        assert_eq!(
            expand_filename("{datetime}", "p", at("2026-08-05T12:32:07Z")),
            "2026-08-05 12.32.07.md"
        );
    }

    #[test]
    fn seconds_are_in_the_default_name_so_two_notes_a_minute_apart_cannot_collide() {
        assert_ne!(
            expand_filename(DEFAULT_FILENAME_TEMPLATE, "p", at("2026-08-05T14:32:07Z")),
            expand_filename(DEFAULT_FILENAME_TEMPLATE, "p", at("2026-08-05T14:32:08Z"))
        );
    }

    #[test]
    fn a_token_can_appear_more_than_once_and_next_to_itself() {
        assert_eq!(
            expand_text("{project}{project} {date}{date}", "x", moment()),
            "xx 2026-08-072026-08-07"
        );
    }
}
