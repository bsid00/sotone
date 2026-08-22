//! Input device enumeration and selection.
//!
//! The microphone is pinned by *name substring*, never by index: indices shift
//! whenever a headset is plugged in, and silently recording from the wrong
//! device is the worst possible failure for this app.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Host};

use super::AudioError;

/// One input device, as the settings UI will want to show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceInfo {
    /// Human-readable name, the same string the `mic_substring` setting matches
    /// against.
    pub name: String,
    /// Whether this is the system's current default input device.
    pub is_default: bool,
}

/// The device's human-readable name.
///
/// cpal 0.18 replaced `Device::name()` with a structured `description()`; the
/// `Display` impl is the documented fallback when the backend cannot produce a
/// description (a device that vanished mid-enumeration, typically).
pub(crate) fn device_name(device: &Device) -> String {
    match device.description() {
        Ok(description) => description.name().to_owned(),
        Err(_) => device.to_string(),
    }
}

/// Every input device on the default host, flagged with which one is default.
///
/// Ordering follows the host's own enumeration order. It is presentation data
/// only — nothing selects a device by position in this list.
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, AudioError> {
    list_input_devices_on(&cpal::default_host())
}

fn list_input_devices_on(host: &Host) -> Result<Vec<InputDeviceInfo>, AudioError> {
    let default = host.default_input_device().map(|d| device_name(&d));
    let devices = host.input_devices().map_err(AudioError::Enumerate)?;

    Ok(devices
        .map(|device| {
            let name = device_name(&device);
            InputDeviceInfo {
                is_default: default.as_deref() == Some(name.as_str()),
                name,
            }
        })
        .collect())
}

/// Choose the input device to record from.
///
/// A `substring` that matches nothing is *not* an error: the user may have
/// unplugged the pinned headset and still expects the app to start. It falls
/// back to the system default and warns, so the mismatch is visible in the log
/// rather than discovered as thirty minutes of silence.
pub(crate) fn pick_input_device(
    host: &Host,
    substring: Option<&str>,
) -> Result<Device, AudioError> {
    if let Some(wanted) = substring.map(str::trim).filter(|s| !s.is_empty()) {
        let devices: Vec<Device> = host
            .input_devices()
            .map_err(AudioError::Enumerate)?
            .collect();
        let names: Vec<String> = devices.iter().map(device_name).collect();

        match match_by_substring(&names, wanted) {
            Some(index) => {
                // `nth` rather than indexing: the index came from the same
                // vector, but there is no reason to write a panicking path.
                if let Some(device) = devices.into_iter().nth(index) {
                    tracing::info!(
                        substring = wanted,
                        device = %device_name(&device),
                        "using pinned microphone"
                    );
                    return Ok(device);
                }
            }
            None => {
                tracing::warn!(
                    substring = wanted,
                    "pinned microphone name matched no input device; \
                     falling back to the system default"
                );
            }
        }
    }

    let device = host
        .default_input_device()
        .ok_or(AudioError::NoInputDevice)?;
    tracing::info!(device = %device_name(&device), "using default input device");
    Ok(device)
}

/// Case-insensitive substring match, first hit wins.
///
/// Split out from device handling so it can be tested without a sound card.
pub(crate) fn match_by_substring(names: &[String], substring: &str) -> Option<usize> {
    let needle = substring.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    names
        .iter()
        .position(|name| name.to_lowercase().contains(&needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["Realtek HD Audio", "Yeti Stereo Microphone", "Webcam C920"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(match_by_substring(&names(), "yeti"), Some(1));
        assert_eq!(match_by_substring(&names(), "YETI"), Some(1));
        assert_eq!(match_by_substring(&names(), "Stereo Micro"), Some(1));
    }

    #[test]
    fn first_match_wins() {
        // "o" appears in all three; selection must be deterministic.
        assert_eq!(match_by_substring(&names(), "o"), Some(0));
    }

    #[test]
    fn no_match_is_none_so_the_caller_can_fall_back() {
        assert_eq!(match_by_substring(&names(), "shure"), None);
    }

    #[test]
    fn blank_substring_never_matches() {
        // An empty `mic_substring` in the config means "no preference", not
        // "match the first device".
        assert_eq!(match_by_substring(&names(), ""), None);
        assert_eq!(match_by_substring(&names(), "   "), None);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(match_by_substring(&names(), "  yeti  "), Some(1));
    }
}
