//! Native interleaved audio → 16 kHz mono, whisper's only input shape.
//!
//! Nothing here touches a device, so all of it is testable on any machine.
//!
//! We deliberately open the stream at the device's native rate and resample
//! here rather than asking WASAPI for 16 kHz: whether the OS honours that
//! request at all depends on the driver's APO, and when it does the quality is
//! whatever the vendor felt like shipping. Speech recognition is sensitive to
//! it, so we own the conversion.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use super::{AudioError, TARGET_SAMPLE_RATE};

/// Frames per resampler chunk. Whole utterances are resampled at once on the
/// worker thread, so this only trades memory against FFT efficiency — it is not
/// a latency knob.
const RESAMPLER_CHUNK_FRAMES: usize = 1024;

/// How far apart the loudest and quietest channel's RMS may be before we stop
/// averaging them. 10 is 20 dB: well beyond any honest stereo mic placement, and
/// the signature of a device whose second channel is dead, unplugged, or carrying
/// only the noise floor. Averaging such a pair halves the level for nothing.
const CHANNEL_IMBALANCE_RATIO: f32 = 10.0;

/// Peak the normalizer aims for. Not 1.0: whisper's front end is happier with a
/// little headroom, and a clip that just touches full scale often means the
/// device was already clipping.
const NORMALIZE_TARGET_PEAK: f32 = 0.9;

/// Below this peak an utterance is treated as silence and left alone. Scaling a
/// silent room up to 0.9 produces amplified hiss, which is precisely the input
/// whisper hallucinates sentences from.
const SILENCE_PEAK: f32 = 1e-4;

/// Ceiling on normalization gain, ~30 dB. Same reasoning as [`SILENCE_PEAK`] at
/// the other end: a very quiet but not silent recording gets help, not a full
/// 60 dB lift that drags the noise floor up with it.
const MAX_NORMALIZE_GAIN: f32 = 31.6;

/// Widest delay searched when correlating two channels, in samples. At 48 kHz,
/// ±8 samples is ±167 µs — the range in which a duplicated-and-delayed channel
/// produces comb filtering inside the speech band.
const CORRELATION_MAX_LAG: i32 = 8;

/// How one channel lines up against another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelCorrelation {
    /// The offset, in samples, applied to the second channel to get the
    /// strongest match: `r = corr(ch0[i], ch1[i + lag])`. A nonzero lag with a
    /// high `coefficient` means one channel is a delayed copy of the other,
    /// which is what "underwater" comb filtering sounds like.
    pub lag: i32,
    /// Pearson coefficient at [`ChannelCorrelation::lag`]. Near +1 is a
    /// duplicate, near −1 is a phase-inverted duplicate (averaging those two
    /// cancels the signal to near-nothing), near 0 is genuinely independent.
    pub coefficient: f32,
}

/// Per-channel levels for one interleaved buffer, for diagnostics only.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStats {
    /// Whole frames the buffer contained.
    pub frames: usize,
    /// Largest absolute sample, per channel.
    pub peaks: Vec<f32>,
    /// Root-mean-square level, per channel.
    pub rms: Vec<f32>,
    /// Only computed for exactly two channels; `None` otherwise.
    pub correlation: Option<ChannelCorrelation>,
}

/// Measure an interleaved buffer channel by channel.
///
/// Pure arithmetic over a slice — no device, no allocation beyond the returned
/// vectors, and never called from a callback.
#[must_use]
pub fn channel_stats(interleaved: &[f32], channels: u16) -> ChannelStats {
    let channels = usize::from(channels);
    if channels == 0 {
        return ChannelStats {
            frames: 0,
            peaks: Vec::new(),
            rms: Vec::new(),
            correlation: None,
        };
    }

    let frames = interleaved.len() / channels;
    let mut peaks = vec![0.0f32; channels];
    let mut squares = vec![0.0f64; channels];
    for frame in interleaved.chunks_exact(channels) {
        for ((peak, square), sample) in peaks.iter_mut().zip(squares.iter_mut()).zip(frame) {
            *peak = peak.max(sample.abs());
            *square += f64::from(*sample) * f64::from(*sample);
        }
    }

    let rms = squares
        .iter()
        .map(|sum| {
            if frames == 0 {
                0.0
            } else {
                (sum / frames as f64).sqrt() as f32
            }
        })
        .collect();

    let correlation = (channels == 2).then(|| {
        let left = extract_channel(interleaved, 2, 0);
        let right = extract_channel(interleaved, 2, 1);
        best_correlation(&left, &right)
    });

    ChannelStats {
        frames,
        peaks,
        rms,
        correlation,
    }
}

/// Interleaved channels → mono, averaging unless one channel dominates.
///
/// The unconditional average is only correct when every channel carries
/// comparable signal. A mic whose second channel is silent halves the level for
/// free; one whose second channel is a delayed or inverted copy comb-filters the
/// result. So the per-channel RMS decides: past [`CHANNEL_IMBALANCE_RATIO`] we
/// take the loudest channel alone.
///
/// A trailing partial frame (which cannot happen with an intact stream, but
/// could if a buffer were ever truncated) is dropped rather than smeared across
/// channels.
#[must_use]
pub fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let channels = usize::from(channels);
    // `chunks_exact(0)` panics, and a 1-channel stream is already mono.
    if channels <= 1 {
        return interleaved.to_vec();
    }

    let rms = per_channel_rms(interleaved, channels);
    let loudest = rms
        .iter()
        .copied()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.total_cmp(b));
    let quietest = rms.iter().copied().fold(f32::INFINITY, f32::min);

    if let Some((index, level)) = loudest {
        // A level of zero everywhere is silence, and there is no "loudest"
        // channel to prefer — fall through to the average rather than picking
        // channel 0 arbitrarily.
        let imbalanced =
            level > 0.0 && (quietest <= 0.0 || level / quietest > CHANNEL_IMBALANCE_RATIO);
        if imbalanced {
            tracing::info!(
                channel = index,
                rms = ?rms,
                "downmix: one channel dominates; using it alone"
            );
            return extract_channel(interleaved, channels, index);
        }
    }

    tracing::info!(rms = ?rms, "downmix: averaging channels");
    let scale = 1.0 / channels as f32;
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() * scale)
        .collect()
}

/// Scale an utterance so its loudest sample sits at [`NORMALIZE_TARGET_PEAK`].
///
/// Returns the gain applied; `1.0` means the utterance was left alone. Runs
/// *after* the downmix, because normalizing first would change which channel the
/// downmix considers loudest.
///
/// This is a single per-utterance scalar, deliberately: no compression, no AGC,
/// nothing that changes the shape of the waveform whisper sees.
pub fn normalize_peak(samples: &mut [f32]) -> f32 {
    let peak = samples
        .iter()
        .fold(0.0f32, |acc, sample| acc.max(sample.abs()));

    // Written as `!(peak > SILENCE_PEAK)` so a NaN peak also takes the
    // leave-it-alone branch instead of producing a NaN gain.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(peak > SILENCE_PEAK) {
        tracing::info!(
            peak,
            "normalize: utterance is effectively silent; left alone"
        );
        return 1.0;
    }

    let gain = (NORMALIZE_TARGET_PEAK / peak).min(MAX_NORMALIZE_GAIN);
    for sample in samples.iter_mut() {
        *sample *= gain;
    }
    tracing::info!(peak, gain, "normalize: scaled utterance");
    gain
}

/// Copy one channel out of an interleaved buffer.
fn extract_channel(interleaved: &[f32], channels: usize, index: usize) -> Vec<f32> {
    interleaved
        .chunks_exact(channels.max(1))
        .filter_map(|frame| frame.get(index).copied())
        .collect()
}

/// RMS per channel over whole frames.
fn per_channel_rms(interleaved: &[f32], channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = interleaved.len() / channels;
    if frames == 0 {
        return vec![0.0; channels];
    }
    let mut squares = vec![0.0f64; channels];
    for frame in interleaved.chunks_exact(channels) {
        for (square, sample) in squares.iter_mut().zip(frame) {
            *square += f64::from(*sample) * f64::from(*sample);
        }
    }
    squares
        .iter()
        .map(|sum| (sum / frames as f64).sqrt() as f32)
        .collect()
}

/// The lag in ±[`CORRELATION_MAX_LAG`] with the strongest correlation by
/// magnitude, so an inverted copy scores as highly as an identical one.
fn best_correlation(left: &[f32], right: &[f32]) -> ChannelCorrelation {
    let mut best = ChannelCorrelation {
        lag: 0,
        coefficient: 0.0,
    };
    for lag in -CORRELATION_MAX_LAG..=CORRELATION_MAX_LAG {
        let (a, b) = match lag.cmp(&0) {
            std::cmp::Ordering::Less => {
                let shift = lag.unsigned_abs() as usize;
                (
                    left.get(shift..),
                    right.get(..right.len().saturating_sub(shift)),
                )
            }
            std::cmp::Ordering::Equal => (Some(left), Some(right)),
            std::cmp::Ordering::Greater => {
                let shift = lag as usize;
                (
                    left.get(..left.len().saturating_sub(shift)),
                    right.get(shift..),
                )
            }
        };
        let (Some(a), Some(b)) = (a, b) else { continue };
        let coefficient = pearson(a, b);
        if coefficient.abs() > best.coefficient.abs() {
            best = ChannelCorrelation { lag, coefficient };
        }
    }
    best
}

/// Pearson correlation over the overlapping prefix of two slices.
///
/// Zero for a constant (zero-variance) input rather than a division by zero: a
/// silent or DC channel correlates with nothing meaningfully.
fn pearson(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let count = n as f64;
    let mean_a = a[..n].iter().map(|v| f64::from(*v)).sum::<f64>() / count;
    let mean_b = b[..n].iter().map(|v| f64::from(*v)).sum::<f64>() / count;

    let (mut covariance, mut var_a, mut var_b) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a[..n].iter().zip(&b[..n]) {
        let dx = f64::from(*x) - mean_a;
        let dy = f64::from(*y) - mean_b;
        covariance += dx * dy;
        var_a += dx * dx;
        var_b += dy * dy;
    }

    let denominator = (var_a * var_b).sqrt();
    if denominator <= 0.0 {
        return 0.0;
    }
    (covariance / denominator).clamp(-1.0, 1.0) as f32
}

/// Resample mono `f32` audio to [`TARGET_SAMPLE_RATE`].
///
/// The resampler is built per call. That costs an FFT plan (sub-millisecond)
/// once per utterance on the worker thread, against whisper's fixed 30-second
/// padding downstream — not a trade worth optimising, and it means no resampler
/// state can leak between utterances.
pub fn resample_to_16k(mono: &[f32], src_rate: u32) -> Result<Vec<f32>, AudioError> {
    if mono.is_empty() {
        return Ok(Vec::new());
    }
    if src_rate == TARGET_SAMPLE_RATE {
        return Ok(mono.to_vec());
    }

    let input = InterleavedSlice::new(mono, 1, mono.len())?;
    let mut resampler = Fft::<f32>::new(
        src_rate as usize,
        TARGET_SAMPLE_RATE as usize,
        RESAMPLER_CHUNK_FRAMES,
        1,
        FixedSync::Input,
    )?;

    // `process_all` handles the whole clip, trims the resampler's own startup
    // delay, and flushes the tail — so the output has neither leading silence
    // nor a clipped final syllable.
    let output = resampler.process_all(&input, mono.len(), None)?;
    Ok(output.take_data())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    #[test]
    fn stereo_downmixes_to_the_channel_average() {
        let interleaved = [1.0, 0.0, 0.5, -0.5, -1.0, 1.0];
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![0.5, 0.0, 0.0]);
    }

    #[test]
    fn mono_passes_through_untouched() {
        let mono = [0.25, -0.5, 1.0];
        assert_eq!(downmix_to_mono(&mono, 1), mono.to_vec());
    }

    #[test]
    fn a_zero_channel_count_does_not_panic() {
        // Defensive: a device claiming zero channels is rejected at stream
        // build time, but this must never be the thing that crashes.
        assert_eq!(downmix_to_mono(&[1.0, 2.0], 0), vec![1.0, 2.0]);
    }

    #[test]
    fn four_channels_average_across_the_frame() {
        let interleaved = [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 2.0, 2.0];
        assert_eq!(downmix_to_mono(&interleaved, 4), vec![1.0, 1.0]);
    }

    #[test]
    fn a_trailing_partial_frame_is_dropped() {
        let interleaved = [1.0, 1.0, 0.5];
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![1.0]);
    }

    /// Interleave two channels of equal length.
    fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
        left.iter().zip(right).flat_map(|(l, r)| [*l, *r]).collect()
    }

    #[test]
    fn a_dead_second_channel_is_dropped_rather_than_averaged_in() {
        // 1.25 against 0.0: the classic broken-stereo-mic case. Averaging would
        // hand whisper half the level for no reason.
        let interleaved = interleave(&[1.25, -1.25, 1.25], &[0.0, 0.0, 0.0]);
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![1.25, -1.25, 1.25]);
    }

    #[test]
    fn a_quiet_but_present_second_channel_is_still_averaged() {
        // RMS ratio is exactly 10 — the boundary is "more than", so this
        // averages. Both levels are exact binary fractions, so the ratio is
        // exactly 10.0 in f32 and the test is not measuring rounding.
        let interleaved = interleave(&[1.25, 1.25], &[0.125, 0.125]);
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![0.6875, 0.6875]);
    }

    #[test]
    fn just_past_the_imbalance_threshold_takes_the_loudest_channel() {
        // RMS ratio 12, i.e. past 10.
        let interleaved = interleave(&[1.5, -1.5], &[0.125, -0.125]);
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![1.5, -1.5]);
    }

    #[test]
    fn the_loudest_channel_is_chosen_by_index_not_assumed_to_be_first() {
        let interleaved = interleave(&[0.125, -0.125], &[1.5, -1.5]);
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![1.5, -1.5]);
    }

    #[test]
    fn silence_on_every_channel_averages_instead_of_picking_one() {
        let interleaved = [0.0f32; 8];
        assert_eq!(downmix_to_mono(&interleaved, 2), vec![0.0; 4]);
    }

    #[test]
    fn channel_stats_report_peak_and_rms_per_channel() {
        let interleaved = interleave(&[1.0, -1.0, 1.0, -1.0], &[0.5, -0.5, 0.5, -0.5]);
        let stats = channel_stats(&interleaved, 2);
        assert_eq!(stats.frames, 4);
        assert_eq!(stats.peaks, vec![1.0, 0.5]);
        assert_eq!(stats.rms, vec![1.0, 0.5]);
    }

    #[test]
    fn channel_stats_on_mono_report_no_correlation() {
        let stats = channel_stats(&[0.5, -0.5, 0.25], 1);
        assert_eq!(stats.frames, 3);
        assert!(stats.correlation.is_none());
    }

    #[test]
    fn a_zero_channel_count_produces_empty_stats() {
        let stats = channel_stats(&[1.0, 2.0], 0);
        assert_eq!(stats.frames, 0);
        assert!(stats.peaks.is_empty());
    }

    #[test]
    fn an_identical_second_channel_correlates_at_lag_zero() {
        let tone = sine(440.0, 48_000, 0.05);
        let stats = channel_stats(&interleave(&tone, &tone), 2);
        let correlation = stats.correlation.expect("stereo input must be correlated");
        assert_eq!(correlation.lag, 0);
        assert!(
            correlation.coefficient > 0.99,
            "expected r near +1, got {}",
            correlation.coefficient
        );
    }

    #[test]
    fn a_phase_inverted_second_channel_correlates_negatively() {
        let tone = sine(440.0, 48_000, 0.05);
        let inverted: Vec<f32> = tone.iter().map(|s| -s).collect();
        let stats = channel_stats(&interleave(&tone, &inverted), 2);
        let correlation = stats.correlation.expect("stereo input must be correlated");
        assert_eq!(correlation.lag, 0);
        assert!(
            correlation.coefficient < -0.99,
            "expected r near -1, got {}",
            correlation.coefficient
        );
    }

    #[test]
    fn a_delayed_second_channel_is_found_at_its_lag() {
        // Right channel is the left delayed by 3 samples: the comb-filter case.
        // Broadband noise, not a tone — a periodic signal correlates equally at
        // several lags and the test would be measuring nothing.
        let left: Vec<f32> = (0..4_000)
            .map(|i| ((i as f32 * 12.9898).sin() * 43_758.547).fract() * 2.0 - 1.0)
            .collect();
        let delay = 3usize;
        let mut right = vec![0.0f32; delay];
        right.extend_from_slice(&left[..left.len() - delay]);

        let stats = channel_stats(&interleave(&left, &right), 2);
        let correlation = stats.correlation.expect("stereo input must be correlated");
        // r = corr(ch0[i], ch1[i + lag]); ch1 lags ch0 by 3, so ch0 matches ch1
        // three samples further along.
        assert_eq!(correlation.lag, delay as i32);
        assert!(
            correlation.coefficient > 0.99,
            "expected r near +1 at lag {delay}, got {}",
            correlation.coefficient
        );
    }

    #[test]
    fn normalization_lifts_a_quiet_utterance_to_the_target_peak() {
        let mut samples = vec![0.1, -0.2, 0.15];
        let gain = normalize_peak(&mut samples);
        assert!((gain - 4.5).abs() < 1e-5, "expected gain 4.5, got {gain}");
        let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!((peak - 0.9).abs() < 1e-5, "expected peak 0.9, got {peak}");
    }

    #[test]
    fn normalization_also_pulls_a_hot_utterance_down() {
        let mut samples = vec![1.8, -0.9];
        let gain = normalize_peak(&mut samples);
        assert!((gain - 0.5).abs() < 1e-6, "expected gain 0.5, got {gain}");
        assert!((samples[0] - 0.9).abs() < 1e-6);
    }

    #[test]
    fn a_silent_utterance_is_left_alone() {
        let original = vec![0.0, 1e-5, -2e-5];
        let mut samples = original.clone();
        assert_eq!(normalize_peak(&mut samples), 1.0);
        assert_eq!(samples, original, "silence must not be amplified");
    }

    #[test]
    fn an_empty_utterance_is_left_alone() {
        let mut samples: Vec<f32> = Vec::new();
        assert_eq!(normalize_peak(&mut samples), 1.0);
    }

    #[test]
    fn normalization_gain_is_clamped_so_a_noise_floor_stays_a_noise_floor() {
        // Peak 0.001 would need 900x to reach 0.9; the clamp allows ~31.6x.
        let mut samples = vec![0.001, -0.0005];
        let gain = normalize_peak(&mut samples);
        assert!(
            (gain - MAX_NORMALIZE_GAIN).abs() < 1e-4,
            "expected the gain clamp, got {gain}"
        );
        let peak = samples.iter().fold(0.0f32, |acc, s| acc.max(s.abs()));
        assert!(peak < 0.9, "clamped gain must fall short of the target");
    }

    #[test]
    fn normalization_does_not_flip_the_waveform() {
        let mut samples = vec![0.2, -0.1, 0.05];
        normalize_peak(&mut samples);
        assert!(samples[0] > 0.0 && samples[1] < 0.0 && samples[2] > 0.0);
    }

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        let len = (rate as f32 * secs) as usize;
        (0..len)
            .map(|i| (TAU * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count()
    }

    #[test]
    fn resampling_48k_to_16k_preserves_length_and_pitch() {
        let input = sine(440.0, 48_000, 1.0);
        let output = resample_to_16k(&input, 48_000).expect("resampling a sine must succeed");

        // One second in, one second out, give or take the resampler's delay.
        let expected = TARGET_SAMPLE_RATE as usize;
        assert!(
            output.len().abs_diff(expected) < 256,
            "expected ~{expected} samples, got {}",
            output.len()
        );

        // 440 Hz for one second is ~440 upward zero crossings. A frequency
        // error (wrong ratio, wrong channel count) shows up here immediately.
        let crossings = zero_crossings(&output);
        assert!(
            crossings.abs_diff(440) <= 3,
            "expected ~440 zero crossings, got {crossings}"
        );
    }

    #[test]
    fn resampling_44100_to_16k_gives_the_expected_length() {
        let input = sine(300.0, 44_100, 0.5);
        let output = resample_to_16k(&input, 44_100).expect("resampling a sine must succeed");
        let expected = TARGET_SAMPLE_RATE as usize / 2;
        assert!(
            output.len().abs_diff(expected) < 256,
            "expected ~{expected} samples, got {}",
            output.len()
        );
    }

    #[test]
    fn audio_already_at_16k_is_returned_verbatim() {
        let input = sine(440.0, TARGET_SAMPLE_RATE, 0.1);
        let output = resample_to_16k(&input, TARGET_SAMPLE_RATE).expect("passthrough must succeed");
        assert_eq!(output, input);
    }

    #[test]
    fn an_empty_utterance_resamples_to_nothing() {
        assert!(resample_to_16k(&[], 48_000)
            .expect("empty input must succeed")
            .is_empty());
    }
}
