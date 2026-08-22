//! The wire format between Sotone and its `sotone-hook` helper process.
//!
//! The hook lives in a separate process: an in-process
//! `WH_KEYBOARD_LL` hook starves while Sotone's own WebView2 window is
//! foreground and it is the only hook in the chain (tauri-apps/tauri#14770),
//! and the only confirmed fix is to own the hook somewhere else. The helper
//! therefore streams what it observed back to the parent over an **anonymous
//! pipe** — one JSON object per line on its stdout.
//!
//! This module is the single definition of that format, shared by both sides,
//! so neither of them ever formats or scrapes JSON by hand. Both directions of
//! the conversion between [`HookMessage`] and [`PttEvent`] live here too.
//!
//! # Invariants
//!
//! * **1 — no synthetic input.** These are observations of what already
//!   happened. There is no message that asks anything to press anything.
//! * **3 — nothing leaves the machine.** A pipe between a parent and its child
//!   is not a network: no socket, no port, no address is involved anywhere in
//!   this path.
//!
//! # Timestamps
//!
//! `Released` and `Toggled` carry the hook's own timestamp, and it becomes the
//! line's `spoken_at`. It must therefore survive the trip
//! **exactly**, which is why the wire carries whole nanoseconds since the Unix
//! epoch rather than a formatted time or a float.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::PttEvent;

/// One line of the helper's stdout.
///
/// Externally tagged on `kind` so a line is self-describing and a future
/// variant is a parse failure the reader can skip rather than a silent
/// misreading of an existing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookMessage {
    /// The push-to-talk control went down.
    Pressed,
    /// The push-to-talk control came up.
    Released {
        /// Hook timestamp, whole nanoseconds since the Unix epoch.
        time_unix_ns: u64,
    },
    /// The toggle control went down.
    Toggled {
        /// Hook timestamp, whole nanoseconds since the Unix epoch.
        time_unix_ns: u64,
    },
    /// The one press a `--capture` helper was started to observe.
    ///
    /// Sent by that mode and by nothing else: a helper watching the ordinary
    /// bindings never emits this, and a capture helper never emits the other
    /// three. It carries the binding's **canonical token**, not a key code —
    /// the same vocabulary the config file and the helper's own arguments use,
    /// so there is exactly one spelling of a hotkey in the whole system, and
    /// the parent parses it back with [`Binding::from_str`] before writing it
    /// anywhere.
    ///
    /// [`Binding::from_str`]: crate::hotkey::Binding
    Captured {
        /// A token [`Binding`](crate::hotkey::Binding) parses.
        token: String,
    },
    /// The helper's hook is gone; it is about to exit and no further events
    /// will come from this process.
    HookLost {
        /// Human-readable cause, for the UI and the log.
        reason: String,
    },
}

impl HookMessage {
    /// Render this message as one line of the pipe (no trailing newline).
    ///
    /// # Errors
    ///
    /// Returns the serializer's error. There is no data shape here that can
    /// actually produce one, but the caller is a process boundary and gets to
    /// decide rather than being handed a panic.
    pub fn to_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse one line of the pipe.
    ///
    /// # Errors
    ///
    /// Returns the parser's error for anything that is not one of the four
    /// messages — including stray output from a library that decided to print
    /// to stdout, which the reader skips rather than trusts.
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// The recording event this message reports, or `None` for one that is not
    /// about recording at all.
    ///
    /// Replaces the old blanket `From<HookMessage> for PttEvent`, and the
    /// `Option` is the point: [`HookMessage::Captured`] belongs to the settings
    /// capture flow and has no [`PttEvent`] to be — inventing one would mean a
    /// captured key press could start a recording. The reader handles it
    /// separately and everything else goes through here unchanged.
    #[must_use]
    pub fn into_event(self) -> Option<PttEvent> {
        Some(match self {
            HookMessage::Pressed => PttEvent::Pressed,
            HookMessage::Released { time_unix_ns } => PttEvent::Released {
                time: system_time(time_unix_ns),
            },
            HookMessage::Toggled { time_unix_ns } => PttEvent::Toggled {
                time: system_time(time_unix_ns),
            },
            HookMessage::HookLost { reason } => PttEvent::HookLost { reason },
            HookMessage::Captured { .. } => return None,
        })
    }
}

impl From<PttEvent> for HookMessage {
    fn from(event: PttEvent) -> Self {
        match event {
            PttEvent::Pressed => HookMessage::Pressed,
            PttEvent::Released { time } => HookMessage::Released {
                time_unix_ns: unix_nanos(time),
            },
            PttEvent::Toggled { time } => HookMessage::Toggled {
                time_unix_ns: unix_nanos(time),
            },
            PttEvent::HookLost { reason } => HookMessage::HookLost { reason },
        }
    }
}

/// Whole nanoseconds since the Unix epoch.
///
/// A clock set before 1970 clamps to the epoch and one set past the year 2554
/// saturates, because a hotkey release time is not worth failing a line over:
/// both are absurd clock states, and the line still has to land.
#[must_use]
pub fn unix_nanos(time: SystemTime) -> u64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => u64::try_from(since.as_nanos()).unwrap_or(u64::MAX),
        Err(_) => 0,
    }
}

/// The inverse of [`unix_nanos`], exact for every value it can produce.
#[must_use]
pub fn system_time(nanos: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A time with a non-round nanosecond part: rounding to milliseconds
    /// anywhere in the chain would show up here.
    fn stamp() -> SystemTime {
        UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789)
    }

    #[test]
    fn the_json_shape_is_the_one_both_sides_agreed_on() {
        assert_eq!(
            HookMessage::Pressed.to_line().expect("serialize"),
            r#"{"kind":"pressed"}"#
        );
        assert_eq!(
            HookMessage::Released { time_unix_ns: 42 }
                .to_line()
                .expect("serialize"),
            r#"{"kind":"released","time_unix_ns":42}"#
        );
        assert_eq!(
            HookMessage::Toggled { time_unix_ns: 42 }
                .to_line()
                .expect("serialize"),
            r#"{"kind":"toggled","time_unix_ns":42}"#
        );
        assert_eq!(
            HookMessage::HookLost {
                reason: "gone".to_owned()
            }
            .to_line()
            .expect("serialize"),
            r#"{"kind":"hook_lost","reason":"gone"}"#
        );
        assert_eq!(
            HookMessage::Captured {
                token: "MouseX1".to_owned()
            }
            .to_line()
            .expect("serialize"),
            r#"{"kind":"captured","token":"MouseX1"}"#
        );
    }

    /// A capture line carries a token the binding vocabulary parses, and the
    /// parent never trusts it further than that: it parses, then renders again,
    /// and only the rendered form is written to the config.
    #[test]
    fn a_captured_token_parses_back_into_the_binding_it_names() {
        use crate::hotkey::Binding;

        for token in ["F13", "F24", "MouseX1", "MouseX2", "Space", "KeyA"] {
            let line = HookMessage::Captured {
                token: token.to_owned(),
            }
            .to_line()
            .expect("serialize");
            let HookMessage::Captured { token: back } =
                HookMessage::from_line(&line).expect("parse")
            else {
                panic!("a captured line must parse back as a capture");
            };
            let binding: Binding = back.parse().expect("a captured token must parse");
            assert_eq!(binding.to_string(), token);
        }
    }

    /// `Captured` is not a recording event and must never be turned into one:
    /// a key press observed by the settings capture flow may not start a
    /// recording.
    #[test]
    fn a_capture_is_not_a_recording_event() {
        assert_eq!(
            HookMessage::Captured {
                token: "F13".to_owned()
            }
            .into_event(),
            None
        );
        assert_eq!(HookMessage::Pressed.into_event(), Some(PttEvent::Pressed));
    }

    /// The reader's forward-compatibility rule, restated for the newest
    /// message: a version that has never heard of a `kind` fails to parse the
    /// line and *skips* it (shell.rs's `read_helper`), rather than misreading
    /// it as one of the messages it does know.
    #[test]
    fn a_reader_that_does_not_know_a_kind_skips_the_line() {
        for line in [
            r#"{"kind":"captured","token":"F13"}"#,
            r#"{"kind":"invented","token":"F13"}"#,
        ] {
            let parsed = HookMessage::from_line(line);
            // Either it is a message this version knows, or it is an error the
            // reader skips. What it must never be is a *different* known
            // message: nothing above deserializes as Pressed/Released/Toggled.
            assert!(!matches!(
                parsed,
                Ok(HookMessage::Pressed
                    | HookMessage::Released { .. }
                    | HookMessage::Toggled { .. }
                    | HookMessage::HookLost { .. })
            ));
        }
        assert!(HookMessage::from_line(r#"{"kind":"captured"}"#).is_err());
    }

    #[test]
    fn every_message_round_trips_through_a_line() {
        let messages = [
            HookMessage::Pressed,
            HookMessage::Released {
                time_unix_ns: unix_nanos(stamp()),
            },
            HookMessage::Toggled {
                time_unix_ns: u64::MAX,
            },
            HookMessage::HookLost {
                reason: "the global input hook stopped delivering events".to_owned(),
            },
            HookMessage::Captured {
                token: "F13".to_owned(),
            },
        ];
        for message in messages {
            let line = message.to_line().expect("serialize");
            assert!(!line.contains('\n'), "a message must fit on one line");
            assert_eq!(HookMessage::from_line(&line).expect("parse"), message);
        }
    }

    /// The release timestamp becomes `spoken_at`, so anything less than exact
    /// is a bug the user would see as the wrong time on a line.
    #[test]
    fn timestamps_survive_the_trip_to_the_nanosecond() {
        for event in [
            PttEvent::Released { time: stamp() },
            PttEvent::Toggled { time: stamp() },
        ] {
            let line = HookMessage::from(event.clone())
                .to_line()
                .expect("serialize");
            let back = HookMessage::from_line(&line)
                .expect("parse")
                .into_event()
                .expect("a recording event");
            assert_eq!(back, event);
        }
    }

    #[test]
    fn events_round_trip_in_both_directions() {
        for event in [
            PttEvent::Pressed,
            PttEvent::Released { time: stamp() },
            PttEvent::Toggled { time: stamp() },
            PttEvent::HookLost {
                reason: "boom".to_owned(),
            },
        ] {
            let back = HookMessage::from(event.clone())
                .into_event()
                .expect("a recording event");
            assert_eq!(back, event);
        }
    }

    /// The direction that actually happens: the helper starts from the
    /// `SystemTime` the hook gave it, and the parent has to rebuild that exact
    /// value, because it becomes the line's `spoken_at`.
    #[test]
    fn a_time_round_trips_through_nanos_exactly() {
        for time in [UNIX_EPOCH, stamp(), SystemTime::now()] {
            assert_eq!(system_time(unix_nanos(time)), time);
        }
    }

    /// The other direction is bounded by the platform clock's granularity, not
    /// by this module: a Windows `SystemTime` is a FILETIME, so it counts in
    /// 100 ns steps and an arbitrary nanosecond value is not representable.
    /// Nothing ever puts one on the wire — every value comes from a real hook
    /// timestamp — so these are the counts that must survive.
    #[test]
    fn representable_nanos_round_trip_through_system_time() {
        for nanos in [0, 100, 999_999_900, 1_700_000_000_123_456_700] {
            assert_eq!(unix_nanos(system_time(nanos)), nanos);
        }
    }

    #[test]
    fn a_pre_epoch_clock_clamps_to_the_epoch() {
        let before = UNIX_EPOCH - Duration::from_secs(60);
        assert_eq!(unix_nanos(before), 0);
        assert_eq!(system_time(unix_nanos(before)), UNIX_EPOCH);
    }

    #[test]
    fn garbage_lines_are_errors_rather_than_guesses() {
        for line in [
            "",
            "not json",
            "{}",
            r#"{"kind":"pressed""#,
            r#"{"kind":"invented"}"#,
            r#"{"kind":"released"}"#,
            "ggml_vulkan: found 1 device",
        ] {
            assert!(
                HookMessage::from_line(line).is_err(),
                "{line} must not parse as a hook message"
            );
        }
    }
}
