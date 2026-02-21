//! Software volume control via gain processing applied to PCM audio samples.
//!
//! Applies a gain factor to audio buffers in-place before they reach the audio device.
//! Supports smooth ramping between gain levels to avoid audible clicks.

use sendspin::audio::Sample;

/// Convert a 0-100 volume percentage to a gain factor using a perceptual power curve.
///
/// Uses `(volume / 100.0).powi(4)` which provides ~60dB dynamic range.
/// - Volume 0 → gain 0.0 (silence)
/// - Volume 100 → gain 1.0 (unity, no change)
pub fn volume_to_gain(volume: u8) -> f32 {
    let normalized = f32::from(volume.min(100)) / 100.0;
    normalized.powi(4)
}

/// Tracks the current gain state for software volume processing.
///
/// Created once per playback session. Updated when volume/mute commands arrive.
/// Called on every audio buffer to apply gain before enqueue.
pub struct SoftwareGainState {
    /// The gain factor currently being applied
    current_gain: f32,
    /// The gain factor we're ramping toward
    target_gain: f32,
    /// Number of samples remaining in the current ramp (0 = no ramp active)
    ramp_samples_remaining: u32,
    /// Per-sample increment during ramp (can be negative for decreasing gain)
    ramp_step: f32,
    /// Whether muted (gain forced to 0, volume remembered for unmute)
    is_muted: bool,
    /// The volume level (0-100) — remembered separately from mute state
    volume: u8,
    /// Total ramp duration in samples, calculated from sample rate
    ramp_duration_samples: u32,
}

impl SoftwareGainState {
    /// Create a new gain state with the given sample rate.
    /// Starts at volume 100 (unity gain, no modification).
    pub fn new(sample_rate: u32) -> Self {
        // ~20ms ramp duration
        let ramp_duration_samples = (sample_rate as f32 * 0.020) as u32;
        Self {
            current_gain: 1.0,
            target_gain: 1.0,
            ramp_samples_remaining: 0,
            ramp_step: 0.0,
            is_muted: false,
            volume: 100,
            ramp_duration_samples,
        }
    }

    /// Set the volume level (0-100). Starts a ramp to the new gain.
    pub fn set_volume(&mut self, volume: u8) {
        self.volume = volume;
        if !self.is_muted {
            self.set_target_gain(volume_to_gain(volume));
        }
    }

    /// Set mute state. Muting ramps to silence; unmuting ramps back to current volume.
    pub fn set_mute(&mut self, muted: bool) {
        self.is_muted = muted;
        if muted {
            self.set_target_gain(0.0);
        } else {
            self.set_target_gain(volume_to_gain(self.volume));
        }
    }

    /// Returns the current volume level (0-100), independent of mute state.
    #[cfg(test)]
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Apply gain to a buffer of f32 samples in-place.
    /// Handles ramping if a gain transition is in progress.
    #[cfg(test)]
    pub fn apply(&mut self, samples: &mut [f32]) {
        // Fast path: no ramp active and gain is unity — skip processing entirely
        if self.ramp_samples_remaining == 0 && (self.current_gain - 1.0).abs() < f32::EPSILON {
            return;
        }

        // Fast path: no ramp active and gain is zero — zero the buffer
        if self.ramp_samples_remaining == 0 && self.current_gain.abs() < f32::EPSILON {
            for sample in samples.iter_mut() {
                *sample = 0.0;
            }
            return;
        }

        // Fast path: no ramp active — constant gain multiply
        if self.ramp_samples_remaining == 0 {
            for sample in samples.iter_mut() {
                *sample *= self.current_gain;
            }
            return;
        }

        // Ramp path: per-sample gain interpolation
        for sample in samples.iter_mut() {
            *sample *= self.current_gain;

            if self.ramp_samples_remaining > 0 {
                self.ramp_samples_remaining -= 1;
                if self.ramp_samples_remaining == 0 {
                    // Ramp complete — snap to target to avoid floating point drift
                    self.current_gain = self.target_gain;
                } else {
                    self.current_gain += self.ramp_step;
                }
            }
        }
    }

    /// Apply gain to a buffer of 24-bit signed integer samples in-place.
    /// Handles ramping if a gain transition is in progress.
    /// Clamps results to the valid range for 24-bit signed integers (Sample).
    pub fn apply_i24(&mut self, samples: &mut [Sample]) {
        // Fast path: no ramp active and gain is unity — skip processing entirely
        if self.ramp_samples_remaining == 0 && (self.current_gain - 1.0).abs() < f32::EPSILON {
            return;
        }

        // Fast path: no ramp active and gain is zero — zero the buffer
        if self.ramp_samples_remaining == 0 && self.current_gain.abs() < f32::EPSILON {
            for sample in samples.iter_mut() {
                *sample = Sample(0);
            }
            return;
        }

        // Fast path: no ramp active — constant gain multiply with clamping
        if self.ramp_samples_remaining == 0 {
            for sample in samples.iter_mut() {
                let value = sample.0 as f32 * self.current_gain;
                let clamped = clamp_i24(value);
                *sample = Sample(clamped);
            }
            return;
        }

        // Ramp path: per-sample gain interpolation with clamping
        for sample in samples.iter_mut() {
            let value = sample.0 as f32 * self.current_gain;
            let clamped = clamp_i24(value);
            *sample = Sample(clamped);

            if self.ramp_samples_remaining > 0 {
                self.ramp_samples_remaining -= 1;
                if self.ramp_samples_remaining == 0 {
                    // Ramp complete — snap to target to avoid floating point drift
                    self.current_gain = self.target_gain;
                } else {
                    self.current_gain += self.ramp_step;
                }
            }
        }
    }

    /// Start a ramp from current gain to the given target.
    fn set_target_gain(&mut self, target: f32) {
        self.target_gain = target;

        if self.ramp_duration_samples == 0 {
            // No ramping (e.g., sample rate not yet known)
            self.current_gain = target;
            self.ramp_samples_remaining = 0;
            return;
        }

        let diff = target - self.current_gain;
        if diff.abs() < f32::EPSILON {
            // Already at target
            self.ramp_samples_remaining = 0;
            return;
        }

        self.ramp_samples_remaining = self.ramp_duration_samples;
        self.ramp_step = diff / self.ramp_duration_samples as f32;
    }
}

/// Clamp a floating-point value to the valid range for 24-bit signed integers.
/// 24-bit signed: -8388608 to 8388607
fn clamp_i24(value: f32) -> i32 {
    const MIN_I24: i32 = -8388608;
    const MAX_I24: i32 = 8388607;
    if value.is_nan() {
        0
    } else if value <= MIN_I24 as f32 {
        MIN_I24
    } else if value >= MAX_I24 as f32 {
        MAX_I24
    } else {
        value.round() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_to_gain_boundaries() {
        // AC3.1: Volume 100 → unity gain
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(volume_to_gain(100), 1.0);
            // AC3.2: Volume 0 → silence
            assert_eq!(volume_to_gain(0), 0.0);
        }
    }

    #[test]
    fn volume_to_gain_intermediate() {
        // AC3.3: Intermediate values follow power curve
        let gain_50 = volume_to_gain(50);
        assert!(
            (gain_50 - 0.0625).abs() < 0.001,
            "50% should be ~0.0625, got {}",
            gain_50
        );

        // Monotonically increasing
        let mut prev = 0.0f32;
        for v in 1..=100 {
            let g = volume_to_gain(v);
            assert!(
                g > prev,
                "gain should increase: vol={}, gain={}, prev={}",
                v,
                g,
                prev
            );
            prev = g;
        }
    }

    #[test]
    fn volume_to_gain_clamps_above_100() {
        // Values above 100 should clamp to 100
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(volume_to_gain(255), 1.0);
        }
    }

    #[test]
    fn apply_unity_gain_is_noop() {
        // AC3.1: Volume 100 does not modify samples
        let mut state = SoftwareGainState::new(48000);
        let original = vec![0.5, -0.3, 1.0, -1.0, 0.0];
        let mut samples = original.clone();
        state.apply(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn apply_zero_volume_produces_silence() {
        // AC3.2: Volume 0 produces all-zero samples
        let mut state = SoftwareGainState::new(48000);
        state.set_volume(0);
        // Exhaust the ramp first
        let mut ramp_buf = vec![0.0f32; 48000];
        state.apply(&mut ramp_buf);
        // Now apply to actual samples
        let mut samples = vec![0.5, -0.3, 1.0, -1.0, 0.123];
        state.apply(&mut samples);
        #[allow(clippy::float_cmp)]
        {
            for s in &samples {
                assert_eq!(*s, 0.0);
            }
        }
    }

    #[test]
    fn apply_intermediate_volume() {
        // AC3.3: Intermediate volume scales by gain factor
        let mut state = SoftwareGainState::new(48000);
        state.set_volume(50);
        // Exhaust ramp
        let mut ramp_buf = vec![0.0f32; 48000];
        state.apply(&mut ramp_buf);
        // Apply to samples
        let gain = volume_to_gain(50);
        let mut samples = vec![1.0, -1.0, 0.5];
        state.apply(&mut samples);
        assert!((samples[0] - gain).abs() < 1e-6);
        assert!((samples[1] - (-gain)).abs() < 1e-6);
        assert!((samples[2] - 0.5 * gain).abs() < 1e-6);
    }

    #[test]
    fn ramp_produces_smooth_transition() {
        // AC3.4: Gain changes ramp over ~20ms
        let sample_rate = 48000u32;
        let mut state = SoftwareGainState::new(sample_rate);
        state.set_volume(0); // Ramp from 1.0 to 0.0

        let ramp_samples = (sample_rate as f32 * 0.020) as usize;
        let mut samples = vec![1.0f32; ramp_samples + 100];
        state.apply(&mut samples);

        // First sample should still be close to 1.0 (start of ramp)
        assert!(
            samples[0] > 0.9,
            "first sample should be near unity: {}",
            samples[0]
        );

        // Mid-ramp sample should be between 0 and 1
        let mid = ramp_samples / 2;
        assert!(
            samples[mid] > 0.0 && samples[mid] < 1.0,
            "mid-ramp should be intermediate: {}",
            samples[mid]
        );

        // After ramp completes, samples should be 0
        assert!(
            (samples[ramp_samples + 50]).abs() < 1e-6,
            "post-ramp should be zero: {}",
            samples[ramp_samples + 50]
        );

        // Check monotonically decreasing during ramp
        for i in 1..ramp_samples {
            assert!(
                samples[i] <= samples[i - 1] + 1e-6,
                "ramp should be monotonically decreasing at sample {}",
                i
            );
        }
    }

    #[test]
    fn mute_and_unmute() {
        // AC3.5: Mute sets gain to zero; unmute restores previous volume
        let mut state = SoftwareGainState::new(48000);
        state.set_volume(50);
        // Exhaust ramp
        let mut buf = vec![0.0f32; 48000];
        state.apply(&mut buf);

        // Mute
        state.set_mute(true);
        let mut buf = vec![0.0f32; 48000];
        state.apply(&mut buf);
        let mut samples = vec![1.0; 10];
        state.apply(&mut samples);
        #[allow(clippy::float_cmp)]
        {
            for s in &samples {
                assert_eq!(*s, 0.0, "muted samples should be zero");
            }
        }

        // Unmute — should restore volume 50
        state.set_mute(false);
        let mut buf = vec![0.0f32; 48000];
        state.apply(&mut buf);
        let gain = volume_to_gain(50);
        let mut samples = vec![1.0; 10];
        state.apply(&mut samples);
        for s in &samples {
            assert!(
                (*s - gain).abs() < 1e-6,
                "unmuted should restore gain: {}",
                s
            );
        }
        assert_eq!(state.volume(), 50);
    }

    #[test]
    fn unity_gain_does_not_modify_samples() {
        // Verifies that apply() at unity gain (volume 100) is a true no-op.
        // This supports AC3.1 (volume 100 = unchanged samples) and is also the
        // foundation for AC3.6: in Hardware mode, the playback thread never creates
        // a SoftwareGainState or calls apply() — that conditional logic is in Task 2.
        let mut state = SoftwareGainState::new(48000);
        let original = vec![0.5, -0.3, 1.0, -1.0, 0.0, 0.999, -0.999];
        let mut samples = original.clone();
        state.apply(&mut samples);
        assert_eq!(
            samples, original,
            "unity gain state must not modify samples"
        );
    }

    #[test]
    fn apply_i24_unity_gain_is_noop() {
        // Verifies that apply_i24() at unity gain (volume 100) leaves samples unchanged
        let mut state = SoftwareGainState::new(48000);
        let original = vec![
            Sample(1000),
            Sample(-1000),
            Sample(8388607),
            Sample(-8388608),
            Sample(0),
        ];
        let mut samples = original.clone();
        state.apply_i24(&mut samples);
        assert_eq!(
            samples, original,
            "unity gain should not modify i24 samples"
        );
    }

    #[test]
    fn apply_i24_zero_volume_produces_silence() {
        // Verifies that apply_i24() at volume 0 produces all-zero samples
        let mut state = SoftwareGainState::new(48000);
        state.set_volume(0);
        // Exhaust the ramp first
        let mut ramp_buf = vec![Sample(0); 48000];
        state.apply_i24(&mut ramp_buf);
        // Now apply to actual samples
        let mut samples = vec![
            Sample(1000),
            Sample(-1000),
            Sample(5000),
            Sample(-5000),
            Sample(123),
        ];
        state.apply_i24(&mut samples);
        for s in &samples {
            assert_eq!(s.0, 0, "zero volume should produce silence");
        }
    }

    #[test]
    fn apply_i24_clamps_overflow() {
        const MAX_I24: i32 = 8388607;
        const MIN_I24: i32 = -8388608;

        // Verifies that apply_i24() clamps results to 24-bit range
        let mut state = SoftwareGainState::new(48000);
        state.set_volume(100); // unity gain first
        let mut ramp_buf = vec![Sample(0); 48000];
        state.apply_i24(&mut ramp_buf);

        // Now test with intermediate gain (50% volume)
        state.set_volume(50);
        let mut ramp_buf = vec![Sample(0); 48000];
        state.apply_i24(&mut ramp_buf);

        // Test with a sample that would overflow if not clamped
        let mut samples = vec![Sample(i32::MAX)];
        state.apply_i24(&mut samples);

        // The result should be clamped to MAX_I24
        assert!(
            samples[0].0 <= MAX_I24,
            "apply_i24 should clamp to MAX_I24, got {}",
            samples[0].0
        );

        // Similarly, test negative overflow
        let mut samples = vec![Sample(i32::MIN)];
        state.apply_i24(&mut samples);
        assert!(
            samples[0].0 >= MIN_I24,
            "apply_i24 should clamp to MIN_I24, got {}",
            samples[0].0
        );
    }
}
