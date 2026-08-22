//! jsonl → markdown: the projection a draft is saved as.
//!
//! The draft store is the source of truth (append-only, fsync'd per line); the
//! markdown file is a *rendering* of it, rewritten whole on every save. That is
//! why this module is pure: no clock, no filesystem, no configuration. Given
//! the same lines it produces the same bytes, which is what makes the
//! external-edit hash guard in [`draft`](crate::draft) mean anything — a
//! mismatch can only be someone else's edit, never our own non-determinism.
//!
//! The shape, and it is the only shape:
//!
//! ```markdown
//! <header, verbatim, if given>
//!
//! - 14:32:07 — the menu button does nothing
//! - 14:33:12 — clipping through the fence at checkpoint 3
//!
//! ---
//!
//! - 09:14:55 — resumed the next morning
//! ```
//!
//! The `---` is a *session divider*: the note was dictated in two
//! sittings, and a resumed session shows as a rule. It is opt-out per
//! project, and with it off the bytes are exactly what they were before the
//! feature existed.

use chrono::{DateTime, FixedOffset};

use crate::draft::LineRecord;

/// Between the timestamp and the text. An em dash, not a hyphen: the bullet
/// already starts with `-`, and two hyphens in one line reads as a typo.
const SEPARATOR: &str = " — ";

/// A session divider and the blank lines that make it a thematic break rather
/// than a setext heading underlining the bullet above it.
///
/// Written *before* a bullet, never after one, which is the whole of the
/// "never trailing" rule.
const DIVIDER: &str = "\n---\n\n";

/// Render a draft's lines as the markdown file the user gets.
///
/// One `- HH:MM:SS — text` bullet per non-deleted line, in file order, with the
/// time taken from each line's `spoken_at` in the offset it was recorded in —
/// the timestamp is what the user's clock said when they released the key, and
/// re-rendering it in another zone later would be a lie about their session.
///
/// `header` is emitted verbatim, followed by a blank line. Token expansion
/// (`{project}`, `{date}`, …) happens in the caller; this function
/// deliberately does not know what a token is.
///
/// Deleted lines vanish completely, timestamp included: a soft-deleted line is
/// one the user chose not to hand to their agent, so leaving a gap in the
/// bullets would defeat the point.
///
/// **Unresolved failed lines are excluded the same way**: a line
/// the model refused has no words, and rendering it would put an empty bullet
/// in the note. It is not lost — the record and its wav are on disk, the row is
/// on screen in `--bad` offering Retry, and the moment anything supplies text —
/// a Retry that worked, or the user typing it — the fold clears `failed` and
/// the line renders like any other (invariant 4: nothing a user wrote can be
/// excluded, because writing text is exactly what resolves the state).
///
/// Rendering is total. An empty draft renders to the header alone, or to the
/// empty string when there is no header; whether saving that is sensible is the
/// UI's call, not the renderer's.
#[must_use]
pub fn render_markdown(header: Option<&str>, lines: &[LineRecord]) -> String {
    render_markdown_with(header, lines, None)
}

/// [`render_markdown`], optionally with session dividers.
///
/// `sessions` is the per-line ordinal from
/// [`Draft::read_transcript`](crate::draft::Draft::read_transcript), parallel to
/// `lines`. `None` means the project has dividers off, and then this function is
/// byte-for-byte [`render_markdown`] — which is what makes turning the feature
/// on and off a safe thing to do to a file that is already bound to a draft.
///
/// A `---` goes between two consecutive **rendered** lines whose ordinals
/// differ. Everything about that sentence is load-bearing:
///
/// * *rendered*, so a soft-deleted line cannot cause a divider, and a sitting
///   whose every line was deleted contributes none at all;
/// * *between*, so there is never a leading or trailing one;
/// * *consecutive*, so two dividers can never end up adjacent.
///
/// After a drag the ordinals need not ascend, and interleaved sittings then
/// produce several dividers. That is the accepted cost: the
/// alternative is a renderer that second-guesses where the user put their lines.
///
/// A `sessions` slice shorter than `lines` simply stops producing dividers past
/// its end rather than panicking — the two always come from the same read, so
/// this is defence, not a supported mode.
#[must_use]
pub fn render_markdown_with(
    header: Option<&str>,
    lines: &[LineRecord],
    sessions: Option<&[usize]>,
) -> String {
    // One allocation for the common case: bullets run about 60 bytes.
    let mut out = String::with_capacity(header.map_or(0, str::len) + lines.len() * 64);

    if let Some(header) = header {
        out.push_str(header);
        out.push('\n');
    }

    // The ordinal of the last line actually written, which is the only one a
    // divider is ever measured against.
    let mut previous: Option<usize> = None;
    let mut first = true;

    for (at, line) in lines.iter().enumerate() {
        if line.deleted || line.failed {
            continue;
        }
        let session = sessions.and_then(|s| s.get(at)).copied();

        if first && header.is_some() {
            // The blank line only exists to separate a header from bullets, so
            // it is written when the first bullet arrives, not when the header
            // is. A header with nothing under it gets no trailing blank line.
            out.push('\n');
        } else if let (Some(before), Some(now)) = (previous, session) {
            if before != now {
                out.push_str(DIVIDER);
            }
        }
        first = false;
        previous = session.or(previous);

        out.push_str("- ");
        out.push_str(&format_time(line.spoken_at));
        out.push_str(SEPARATOR);
        push_flattened(&mut out, &line.text);
        out.push('\n');
    }

    out
}

/// `HH:MM:SS` in the line's own offset.
fn format_time(at: DateTime<FixedOffset>) -> String {
    at.format("%H:%M:%S").to_string()
}

/// Append `text` with every line break turned into a single space.
///
/// One utterance is one line — that is the format guarantee the whole file
/// rests on, and a stray newline in `text` would silently turn one bullet into
/// a bullet plus a paragraph. Whisper does not emit them today, but the schema
/// allows them and the editor lets the user type whatever they like into `text`.
/// `\r\n` collapses to one space rather than two so a line pasted from a
/// Windows editor does not gain a double gap.
fn push_flattened(out: &mut String, text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' => out.push(' '),
            other => out.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(at: &str, text: &str) -> LineRecord {
        LineRecord {
            id: "01JLINE".to_owned(),
            spoken_at: DateTime::parse_from_rfc3339(at).expect("test timestamp"),
            text: text.to_owned(),
            original: None,
            deleted: false,
            audio: "audio/01JLINE.wav".to_owned(),
            transcribe_ms: None,
            failed: false,
        }
    }

    #[test]
    fn bullets_are_time_em_dash_text_in_file_order() {
        let lines = vec![
            line("2026-08-04T14:32:07+02:00", "the menu button does nothing"),
            line(
                "2026-08-04T14:33:12+02:00",
                "clipping through the fence at checkpoint 3",
            ),
        ];

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:07 — the menu button does nothing\n\
             - 14:33:12 — clipping through the fence at checkpoint 3\n"
        );
    }

    #[test]
    fn a_header_is_verbatim_and_followed_by_one_blank_line() {
        let lines = vec![line("2026-08-04T14:32:07+02:00", "first")];

        assert_eq!(
            render_markdown(Some("# Playtest — 2026-08-04"), &lines),
            "# Playtest — 2026-08-04\n\n- 14:32:07 — first\n"
        );
    }

    #[test]
    fn a_multi_line_header_is_not_reflowed() {
        // Verbatim means verbatim: the template decides what a header says, and a
        // renderer that "tidied" it would fight the template.
        let lines = vec![line("2026-08-04T09:00:00+02:00", "x")];

        assert_eq!(
            render_markdown(Some("# Title\n\nSome preamble."), &lines),
            "# Title\n\nSome preamble.\n\n- 09:00:00 — x\n"
        );
    }

    #[test]
    fn an_empty_draft_renders_to_the_header_alone_or_to_nothing() {
        assert_eq!(render_markdown(None, &[]), "");
        assert_eq!(render_markdown(Some("# Empty"), &[]), "# Empty\n");
    }

    #[test]
    fn deleted_lines_leave_no_trace() {
        let mut lines = vec![
            line("2026-08-04T14:32:07+02:00", "kept"),
            line("2026-08-04T14:32:40+02:00", "removed"),
            line("2026-08-04T14:33:12+02:00", "also kept"),
        ];
        lines[1].deleted = true;

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:07 — kept\n- 14:33:12 — also kept\n"
        );
    }

    #[test]
    fn a_draft_of_only_deleted_lines_renders_like_an_empty_one() {
        let mut lines = vec![line("2026-08-04T14:32:07+02:00", "gone")];
        lines[0].deleted = true;

        assert_eq!(render_markdown(None, &lines), "");
        assert_eq!(render_markdown(Some("# Head"), &lines), "# Head\n");
    }

    #[test]
    fn an_unresolved_failed_line_is_left_out_rather_than_rendered_empty() {
        // The record and its wav are on disk; what the note must not
        // grow is a bullet with a timestamp and no finding after it.
        let mut lines = vec![
            line("2026-08-04T14:32:07+02:00", "kept"),
            line("2026-08-04T14:32:40+02:00", ""),
            line("2026-08-04T14:33:12+02:00", "also kept"),
        ];
        lines[1].failed = true;

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:07 — kept\n- 14:33:12 — also kept\n"
        );
    }

    #[test]
    fn a_failed_line_that_has_been_given_words_renders_like_any_other() {
        // The fold clears `failed` the moment an edit supplies text, so what
        // reaches the renderer is an ordinary line. Pinned here because the
        // exclusion above is the one place a *user-written* line could be lost
        // if the two rules ever drifted (invariant 4).
        let mut lines = vec![line("2026-08-04T14:32:40+02:00", "typed it myself")];
        lines[0].failed = false;

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:40 — typed it myself\n"
        );
    }

    #[test]
    fn a_failed_line_contributes_no_divider_either() {
        // Same rule deleted lines get: a sitting whose only line failed does
        // not put a rule in the note.
        let mut lines = three();
        lines[1].failed = true;
        lines[1].text = String::new();

        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1, 2])),
            "- 09:00:00 — one\n\n---\n\n- 11:00:00 — three\n"
        );
    }

    #[test]
    fn embedded_newlines_are_flattened_to_spaces() {
        let lines = vec![
            line("2026-08-04T14:32:07+02:00", "one\ntwo"),
            line("2026-08-04T14:32:08+02:00", "crlf\r\nstyle"),
            line("2026-08-04T14:32:09+02:00", "lone\rcarriage"),
        ];

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:07 — one two\n\
             - 14:32:08 — crlf style\n\
             - 14:32:09 — lone carriage\n"
        );
    }

    #[test]
    fn the_timestamp_is_read_in_the_offset_it_was_spoken_in() {
        // 14:32 in Berlin is not 14:32 in UTC, and the note says what the clock
        // on the wall said.
        let lines = vec![
            line("2026-08-04T14:32:07+02:00", "berlin"),
            line("2026-08-04T14:32:07Z", "utc"),
        ];

        assert_eq!(
            render_markdown(None, &lines),
            "- 14:32:07 — berlin\n- 14:32:07 — utc\n"
        );
    }

    // -----------------------------------------------------------------------
    // Session dividers.
    // -----------------------------------------------------------------------

    /// Three lines an hour apart, so a divider between any pair is legible.
    fn three() -> Vec<LineRecord> {
        vec![
            line("2026-08-04T09:00:00+02:00", "one"),
            line("2026-08-04T10:00:00+02:00", "two"),
            line("2026-08-04T11:00:00+02:00", "three"),
        ]
    }

    #[test]
    fn dividers_off_is_byte_for_byte_what_it_always_was() {
        // The safety property: turning the feature off (or having no ordinals
        // at all) cannot change one byte of an existing note.
        let lines = three();
        let plain = render_markdown(Some("# H"), &lines);
        assert_eq!(render_markdown_with(Some("# H"), &lines, None), plain);
        // Even when the lines really do come from three different sittings.
        assert_eq!(
            render_markdown_with(Some("# H"), &lines, None),
            "# H\n\n- 09:00:00 — one\n- 10:00:00 — two\n- 11:00:00 — three\n"
        );
    }

    #[test]
    fn a_divider_goes_between_two_sittings_and_nowhere_else() {
        let lines = three();
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1, 1])),
            "- 09:00:00 — one\n\n---\n\n- 10:00:00 — two\n- 11:00:00 — three\n"
        );
        // Never leading and never trailing: the same lines all in one sitting,
        // and each in its own, bracket the cases.
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[2, 2, 2])),
            "- 09:00:00 — one\n- 10:00:00 — two\n- 11:00:00 — three\n"
        );
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1, 2])),
            "- 09:00:00 — one\n\n---\n\n- 10:00:00 — two\n\n---\n\n- 11:00:00 — three\n"
        );
    }

    #[test]
    fn a_divider_never_lands_between_the_header_and_the_first_bullet() {
        // The first rendered line has nothing before it, whatever its ordinal.
        assert_eq!(
            render_markdown_with(Some("# H"), &three(), Some(&[3, 3, 3])),
            "# H\n\n- 09:00:00 — one\n- 10:00:00 — two\n- 11:00:00 — three\n"
        );
    }

    #[test]
    fn a_sitting_that_contributed_no_live_lines_produces_no_divider() {
        // The whole middle session was deleted, so the note reads as the two
        // sittings that are actually in it — not three, and never two rules in
        // a row.
        let mut lines = vec![
            line("2026-08-04T09:00:00+02:00", "first sitting"),
            line("2026-08-04T10:00:00+02:00", "deleted"),
            line("2026-08-04T11:00:00+02:00", "third sitting"),
        ];
        lines[1].deleted = true;

        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1, 2])),
            "- 09:00:00 — first sitting\n\n---\n\n- 11:00:00 — third sitting\n"
        );

        // And a whole sitting deleted from the *end* leaves no trailing rule.
        let mut tail = three();
        tail[2].deleted = true;
        assert_eq!(
            render_markdown_with(None, &tail, Some(&[0, 0, 1])),
            "- 09:00:00 — one\n- 10:00:00 — two\n"
        );
    }

    #[test]
    fn reordering_two_sittings_into_each_other_produces_a_rule_per_crossing() {
        // The accepted cost, pinned so nobody "fixes" it by deduplicating: the
        // ordinals say where each line came from, and a drag can interleave
        // them. Second-guessing the user's chosen order is the worse answer.
        let lines = three();
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1, 0])),
            "- 09:00:00 — one\n\n---\n\n- 10:00:00 — two\n\n---\n\n- 11:00:00 — three\n"
        );
    }

    #[test]
    fn a_short_or_empty_ordinal_slice_simply_stops_producing_dividers() {
        // Defence, not a supported mode: the two always come from one read.
        let lines = three();
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[0, 1])),
            "- 09:00:00 — one\n\n---\n\n- 10:00:00 — two\n- 11:00:00 — three\n"
        );
        assert_eq!(
            render_markdown_with(None, &lines, Some(&[])),
            render_markdown(None, &lines)
        );
    }

    #[test]
    fn rendering_with_dividers_is_deterministic_too() {
        // The hash guard covers this render as well: a save that re-rendered
        // differently would report every later save as a conflict.
        let lines = three();
        let sessions = [0, 1, 1];
        assert_eq!(
            render_markdown_with(Some("# H"), &lines, Some(&sessions)),
            render_markdown_with(Some("# H"), &lines, Some(&sessions))
        );
    }

    #[test]
    fn rendering_is_deterministic_for_the_same_input() {
        // The hash guard in `draft` depends on this: two renders of unchanged
        // lines must be byte-identical or every save would look like a
        // conflict.
        let lines = vec![line("2026-08-04T14:32:07+02:00", "stable")];
        assert_eq!(
            render_markdown(Some("# H"), &lines),
            render_markdown(Some("# H"), &lines)
        );
    }
}
