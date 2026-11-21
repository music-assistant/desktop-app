// ABOUTME: Lock-free volume/mute control for SyncedPlayer
// ABOUTME: Uses atomics for zero-latency gain changes from any thread

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

/// Packed atomics for [`GainControl`], shared via a single `Arc`.
struct GainState {
    target_gain_bits: AtomicU32,
    muted: AtomicBool,
    volume_pct: AtomicU8,
}

/// Shared volume/mute control for a `SyncedPlayer` or custom playback pipeline.
///
/// All methods are lock-free and safe to call from any thread. Cloning is cheap
/// (single `Arc` increment, no data copy).
#[derive(Clone)]
pub struct GainControl {
    state: Arc<GainState>,
}

/// Clamp volume to 0-100 and compute perceptual gain via a 1.5-power curve.
fn volume_to_gain(volume: u8) -> (u8, f32) {
    let clamped = volume.min(100);
    let vol = f32::from(clamped) / 100.0;
    (clamped, vol.powf(1.5))
}

impl GainControl {
    /// Create a new `GainControl` at the given volume and mute state.
    pub fn new(volume: u8, muted: bool) -> Self {
        let (clamped, gain) = volume_to_gain(volume);
        Self {
            state: Arc::new(GainState {
                target_gain_bits: AtomicU32::new(gain.to_bits()),
                muted: AtomicBool::new(muted),
                volume_pct: AtomicU8::new(clamped),
            }),
        }
    }

    /// Set playback volume (0-100).
    ///
    /// Uses a 1.5-power perceptual curve: `gain = (volume / 100)^1.5`, so that
    /// 50 feels like "half volume" rather than half amplitude. Values above
    /// 100 are clamped to 100.
    ///
    /// Note: `volume_pct` and `target_gain_bits` are stored as two
    /// separate atomics. A concurrent reader may briefly observe the
    /// new volume with the old gain or vice versa. This is harmless
    /// because the gain ramp smooths any transition.
    pub fn set_volume(&self, volume: u8) {
        let (clamped, gain) = volume_to_gain(volume);
        log::debug!("Volume set: {clamped}% (gain={gain:.3})");
        // Store gain first so a concurrent reader never sees the new volume
        // with a stale gain value (the ramp smooths any brief inconsistency).
        self.state
            .target_gain_bits
            .store(gain.to_bits(), Ordering::Relaxed);
        self.state.volume_pct.store(clamped, Ordering::Relaxed);
    }

    /// Set the mute state. When muted, output gain is 0 regardless of volume.
    pub fn set_mute(&self, muted: bool) {
        log::debug!("Mute set: {muted}");
        self.state.muted.store(muted, Ordering::Relaxed);
    }

    /// Current volume as 0-100.
    pub fn volume(&self) -> u8 {
        self.state.volume_pct.load(Ordering::Relaxed)
    }

    /// Whether playback is currently muted.
    pub fn is_muted(&self) -> bool {
        self.state.muted.load(Ordering::Relaxed)
    }

    /// Read the effective linear gain (0.0-1.0).
    ///
    /// Returns `0.0` when muted. This is useful for custom playback pipelines
    /// that want to reuse sendspin-rs volume/mute state handling while applying
    /// gain themselves.
    pub fn gain(&self) -> f32 {
        if self.state.muted.load(Ordering::Relaxed) {
            return 0.0;
        }
        let gain = f32::from_bits(self.state.target_gain_bits.load(Ordering::Relaxed));
        debug_assert!(gain.is_finite(), "gain bits produced non-finite value");
        // NaN is unordered, so `clamp` propagates it unchanged. Fail safe to
        // silence rather than letting NaN poison the entire gain ramp.
        if !gain.is_finite() {
            return 0.0;
        }
        gain.clamp(0.0, 1.0)
    }
}

impl fmt::Debug for GainControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GainControl")
            .field("volume", &self.volume())
            .field("muted", &self.is_muted())
            .finish()
    }
}

/// Per-frame gain ramp to avoid clicks on volume changes.
///
/// Operates per-frame (not per-sample) so ramp duration is independent
/// of channel count. All samples within a frame get the same gain value.
pub(crate) struct GainRamp {
    /// Number of frames over which to ramp (20ms worth at the configured sample rate).
    /// Zero for very low sample rates — in that case, gain changes snap instantly.
    ramp_duration_frames: u32,
    /// The gain value currently being applied to output frames.
    current_gain: f32,
    /// How many frames remain in the current ramp (0 = not ramping).
    ramp_frames_remaining: u32,
    /// Per-frame gain increment during a ramp.
    ramp_step: f32,
    /// The most recent target gain, used to detect changes.
    last_target: f32,
}

impl GainRamp {
    /// Create a new ramp with a 20ms transition at the given sample rate.
    ///
    /// `initial_gain` sets both `current_gain` and `last_target` so that
    /// the first callback applies the correct gain without ramping.
    pub(crate) fn new(sample_rate: u32, initial_gain: f32) -> Self {
        let gain = initial_gain.clamp(0.0, 1.0);
        Self {
            ramp_duration_frames: sample_rate / 50, // 20ms = 1/50th of a second
            current_gain: gain,
            ramp_frames_remaining: 0,
            ramp_step: 0.0,
            last_target: gain,
        }
    }

    /// Update ramp state for `frames` frames without touching any audio buffer.
    ///
    /// Use this to keep the ramp synchronized during silent periods (e.g.
    /// pre-start silence) without paying for per-sample multiplies on zeros.
    ///
    /// Note: when advancing multiple frames in a single call, the result may
    /// differ from calling `apply()` frame-by-frame by a tiny amount due to
    /// floating-point non-associativity (`step * N` vs N individual adds).
    /// The snap-to-target at ramp completion and the `clamp` bound the error.
    pub(crate) fn advance(&mut self, frames: usize, target: f32) {
        if frames == 0 {
            return;
        }

        self.update_target(target);

        let advance = u32::try_from(frames)
            .unwrap_or(u32::MAX)
            .min(self.ramp_frames_remaining);
        if advance > 0 {
            self.current_gain += self.ramp_step * advance as f32;
            self.ramp_frames_remaining -= advance;
            if self.ramp_frames_remaining == 0 {
                self.current_gain = target;
            } else {
                self.current_gain = self.current_gain.clamp(0.0, 1.0);
            }
        }
    }

    /// Apply gain to an interleaved f32 buffer.
    ///
    /// `channels` is the number of samples per frame (must be > 0).
    /// `target` is the desired final gain (0.0-1.0).
    ///
    /// Note: the ramp step is applied *before* the frame's samples, so the
    /// first frame of a new ramp uses `current_gain + step` rather than the
    /// pre-ramp `current_gain`. The final frame snaps to `target` exactly.
    /// This one-frame offset is inaudible at typical sample rates (960 frames
    /// at 48 kHz) and is consistent with [`Self::advance()`].
    pub(crate) fn apply(&mut self, data: &mut [f32], channels: usize, target: f32) {
        // Guard degenerate inputs. channels == 0 is a programming error but
        // must not panic on the audio thread; debug_assert catches it during
        // development.
        if data.is_empty() || channels == 0 {
            // N.B. Early return *before* updating last_target is intentional:
            // an empty buffer must not commit a target change, otherwise the
            // ramp step would be computed from a stale current_gain on the
            // next real call.
            return;
        }
        debug_assert!(
            data.len().is_multiple_of(channels),
            "buffer length must be a multiple of channels"
        );

        self.update_target(target);

        // Fast path: skip per-sample multiply when gain is unity and stable.
        // `== 1.0` is exact because `current_gain` only reaches 1.0 via the
        // snap-to-target assignment (`self.current_gain = target`) when the
        // ramp completes, so there is no accumulated floating-point error.
        if self.ramp_frames_remaining == 0 && self.current_gain == 1.0 {
            return;
        }

        let frames = data.len() / channels;
        let ramp_frames = (self.ramp_frames_remaining as usize).min(frames);

        // Ramp region: per-frame gain stepping (not vectorizable).
        let (ramp_data, steady_data) = data.split_at_mut(ramp_frames * channels);
        for frame in ramp_data.chunks_mut(channels) {
            self.current_gain += self.ramp_step;
            self.ramp_frames_remaining -= 1;
            if self.ramp_frames_remaining == 0 {
                self.current_gain = target;
            }
            for sample in frame.iter_mut() {
                *sample *= self.current_gain;
            }
        }
        // Clamp once after the ramp region to bound any FP accumulation error.
        if ramp_frames > 0 && self.ramp_frames_remaining > 0 {
            self.current_gain = self.current_gain.clamp(0.0, 1.0);
        }

        // Steady-state region: constant gain, SIMD-friendly.
        let gain = self.current_gain;
        if gain == 0.0 {
            // memset is faster than N fmuls when muted.
            steady_data.fill(0.0);
        } else {
            for sample in steady_data.iter_mut() {
                *sample *= gain;
            }
        }
    }

    /// Detect a target change and (re)start the ramp if needed.
    fn update_target(&mut self, target: f32) {
        debug_assert!(target.is_finite(), "target gain must be finite");
        // NaN/Inf can't reach here through GainControl::gain(), but
        // guard defensively to avoid corrupting ramp state if called directly.
        if !target.is_finite() {
            return;
        }
        if target.to_bits() != self.last_target.to_bits() {
            if self.ramp_duration_frames == 0 {
                self.current_gain = target;
            } else {
                self.ramp_frames_remaining = self.ramp_duration_frames;
                self.ramp_step = (target - self.current_gain) / self.ramp_duration_frames as f32;
            }
            self.last_target = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected gain values are precomputed literals, NOT calls to `volume_to_gain()`.
    // This is intentional: using the function under test as its own oracle is
    // tautological. The literals are independently derived from the 1.5-power
    // perceptual curve: gain = (volume / 100)^1.5.  If the curve changes,
    // these tests break — which is exactly what we want.

    #[test]
    fn test_volume_to_gain_boundaries() {
        let gc = GainControl::new(100, false);

        gc.set_volume(0);
        assert!((gc.gain() - 0.0).abs() < f32::EPSILON);

        gc.set_volume(100);
        assert!((gc.gain() - 1.0).abs() < f32::EPSILON);

        gc.set_volume(50);
        assert!((gc.gain() - 0.353_553).abs() < 1e-3); // (50/100)^1.5
    }

    #[test]
    fn test_volume_store_roundtrip() {
        let gc = GainControl::new(100, false);
        for v in 0..=100u8 {
            gc.set_volume(v);
            assert_eq!(gc.volume(), v, "roundtrip failed for volume {v}");
        }
    }

    #[test]
    fn test_mute_returns_zero_gain() {
        let gc = GainControl::new(100, false);
        gc.set_volume(75);
        gc.set_mute(true);
        assert!((gc.gain() - 0.0).abs() < f32::EPSILON);
        // volume() still reports the stored volume
        assert_eq!(gc.volume(), 75);
    }

    #[test]
    fn test_unmute_restores_previous_gain() {
        let gc = GainControl::new(100, false);
        gc.set_volume(75);
        let expected_gain = gc.gain();

        gc.set_mute(true);
        assert!((gc.gain() - 0.0).abs() < f32::EPSILON);

        gc.set_mute(false);
        assert!(
            (gc.gain() - expected_gain).abs() < f32::EPSILON,
            "unmute should restore gain to {expected_gain}, got {}",
            gc.gain()
        );
    }

    #[test]
    fn test_volume_above_100_clamps() {
        let gc = GainControl::new(100, false);
        gc.set_volume(255);
        assert_eq!(gc.volume(), 100);
        assert!((gc.gain() - 1.0).abs() < f32::EPSILON);

        gc.set_volume(101);
        assert_eq!(gc.volume(), 100);
    }

    #[test]
    fn test_clone_shares_state() {
        let gc = GainControl::new(100, false);
        let gc2 = gc.clone();
        gc.set_volume(42);
        assert_eq!(gc2.volume(), 42);
        gc2.set_mute(true);
        assert!(gc.is_muted());
    }

    // -- GainRamp tests --

    #[test]
    fn test_ramp_duration_is_channel_independent() {
        // At 1000 Hz sample rate, 20ms = 20 frames.
        // Stereo: 40 samples total, but ramp should still take 20 frames.
        let mut ramp = GainRamp::new(1000, 1.0);
        let mut mono = vec![1.0; 20];
        let mut stereo = vec![1.0; 40];

        ramp.apply(&mut mono, 1, 0.0);
        let mono_last = mono[19];

        let mut ramp2 = GainRamp::new(1000, 1.0);
        ramp2.apply(&mut stereo, 2, 0.0);
        // The last frame's left channel (index 38) should match mono's last sample
        let stereo_last = stereo[38];

        assert!(
            (mono_last - stereo_last).abs() < 1e-5,
            "mono={mono_last}, stereo={stereo_last}: ramp duration should not depend on channel count"
        );
    }

    #[test]
    fn test_ramp_reaches_target_exactly() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20 frames for 20ms
                                                 // Buffer with exactly 20 frames of mono
        let mut data = vec![1.0; 20];
        ramp.apply(&mut data, 1, 0.5);

        // After the ramp completes, current_gain should be exactly 0.5
        assert!(
            (ramp.current_gain - 0.5).abs() < f32::EPSILON,
            "current_gain={}, expected 0.5",
            ramp.current_gain
        );
        // Last sample should be 1.0 * 0.5 = 0.5
        assert!(
            (data[19] - 0.5).abs() < f32::EPSILON,
            "last sample={}, expected 0.5",
            data[19]
        );
    }

    #[test]
    fn test_ramp_gain_stays_clamped_across_direction_change() {
        let mut ramp = GainRamp::new(1000, 1.0);
        // Start ramp toward 0.0
        let mut data = vec![1.0; 10];
        ramp.apply(&mut data, 1, 0.0);
        // Midway through ramp, reverse toward 1.0
        let mut data2 = vec![1.0; 30];
        ramp.apply(&mut data2, 1, 1.0);

        // Verify no sample exceeds [0.0, 1.0] (gain applied to 1.0 inputs)
        for (i, &s) in data.iter().chain(data2.iter()).enumerate() {
            assert!((0.0..=1.0).contains(&s), "sample {i} out of range: {s}");
        }
    }

    #[test]
    fn test_ramp_no_change_applies_constant_gain() {
        let mut ramp = GainRamp::new(48000, 1.0);
        // Set gain to 0.5 and complete the ramp
        let mut warmup = vec![1.0; 960]; // 20ms at 48kHz
        ramp.apply(&mut warmup, 1, 0.5);

        // Now apply again with same target — should be constant 0.5
        let mut data = vec![1.0; 100];
        ramp.apply(&mut data, 1, 0.5);
        for (i, &s) in data.iter().enumerate() {
            assert!(
                (s - 0.5).abs() < f32::EPSILON,
                "sample {i}={s}, expected constant 0.5"
            );
        }
    }

    #[test]
    fn test_ramp_empty_buffer_does_not_corrupt_state() {
        let mut ramp = GainRamp::new(1000, 1.0);
        // Set gain to 0.5 and complete the ramp
        let mut warmup = vec![1.0; 20];
        ramp.apply(&mut warmup, 1, 0.5);
        assert!((ramp.current_gain - 0.5).abs() < f32::EPSILON);

        // Apply with empty buffer — state should not change
        ramp.apply(&mut [], 1, 0.0);
        assert!(
            (ramp.current_gain - 0.5).abs() < f32::EPSILON,
            "empty buffer corrupted current_gain: {}",
            ramp.current_gain
        );

        // Next real buffer should ramp from 0.5 to 0.0
        let mut data = vec![1.0; 20];
        ramp.apply(&mut data, 1, 0.0);
        assert!(
            (ramp.current_gain - 0.0).abs() < f32::EPSILON,
            "ramp did not reach target after empty buffer: {}",
            ramp.current_gain
        );
    }

    #[test]
    fn test_ramp_zero_duration_snaps_instantly() {
        // sample_rate < 50 produces ramp_duration_frames = 0
        let mut ramp = GainRamp::new(10, 1.0);
        assert_eq!(ramp.ramp_duration_frames, 0);

        let mut data = vec![1.0; 5];
        ramp.apply(&mut data, 1, 0.25);

        // All samples should be scaled by 0.25 (instant snap, no ramp)
        for (i, &s) in data.iter().enumerate() {
            assert!(
                (s - 0.25).abs() < f32::EPSILON,
                "sample {i}={s}, expected 0.25 (instant snap)"
            );
        }
    }

    #[test]
    fn test_ramp_mid_ramp_reversal_trajectory() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20 frames ramp
                                                 // Start ramping from 1.0 toward 0.0
        let mut data = vec![1.0; 10]; // 10 of 20 frames
        ramp.apply(&mut data, 1, 0.0);

        // current_gain should be approximately 0.5 (halfway through ramp)
        let mid_gain = ramp.current_gain;
        assert!(
            (mid_gain - 0.5).abs() < 0.05,
            "mid-ramp gain={mid_gain}, expected ~0.5"
        );

        // Reverse direction toward 1.0
        let mut data2 = vec![1.0; 20];
        ramp.apply(&mut data2, 1, 1.0);

        // After completing the new ramp, should be exactly 1.0
        assert!(
            (ramp.current_gain - 1.0).abs() < f32::EPSILON,
            "post-reversal gain={}, expected 1.0",
            ramp.current_gain
        );

        // Verify strictly monotonic increase in the reversal buffer
        // (the ramp is active for all 20 frames, so each frame's gain is strictly higher)
        for i in 1..20 {
            assert!(
                data2[i] > data2[i - 1],
                "non-strictly-monotonic at {i}: {} <= {}",
                data2[i],
                data2[i - 1]
            );
        }
    }

    #[test]
    fn test_stereo_channels_get_same_gain_per_frame() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20 frames ramp
                                                 // Stereo buffer: L=1.0, R=0.5 for each frame
        let mut data = Vec::with_capacity(40);
        for _ in 0..20 {
            data.push(1.0); // L
            data.push(0.5); // R
        }
        ramp.apply(&mut data, 2, 0.0);

        // For each frame, L/input_L should equal R/input_R (same gain factor)
        for frame in 0..20 {
            let l = data[frame * 2];
            let r = data[frame * 2 + 1];
            // gain = l / 1.0 = l, gain = r / 0.5 = r * 2
            let gain_from_l = l;
            let gain_from_r = r * 2.0;
            assert!(
                (gain_from_l - gain_from_r).abs() < 1e-6,
                "frame {frame}: L gain={gain_from_l}, R gain={gain_from_r} — channels got different gain"
            );
        }
    }

    #[test]
    fn test_volume_change_while_muted() {
        let gc = GainControl::new(100, false);
        gc.set_volume(75);
        gc.set_mute(true);
        assert!((gc.gain()).abs() < f32::EPSILON);

        // Change volume while muted
        gc.set_volume(25);
        // Still muted — gain should be 0
        assert!((gc.gain()).abs() < f32::EPSILON);

        // Unmute — gain should reflect volume=25, not volume=75
        gc.set_mute(false);
        assert!(
            (gc.gain() - 0.125).abs() < 1e-3, // (25/100)^1.5
            "after unmute, gain should match volume=25 (0.125), got {}",
            gc.gain()
        );
    }

    #[test]
    fn test_advance_tracks_ramp_state_without_buffer() {
        // advance() should produce the same ramp state as apply()
        let mut ramp_apply = GainRamp::new(1000, 1.0); // 20 frames
        let mut ramp_advance = GainRamp::new(1000, 1.0);

        // apply() on a non-zero buffer (output values are irrelevant;
        // we only compare internal ramp state)
        let mut buf = vec![1.0; 20];
        ramp_apply.apply(&mut buf, 1, 0.0);

        // advance() with the same frame count
        ramp_advance.advance(20, 0.0);

        assert!(
            (ramp_apply.current_gain - ramp_advance.current_gain).abs() < f32::EPSILON,
            "apply={}, advance={}: ramp state diverged",
            ramp_apply.current_gain,
            ramp_advance.current_gain
        );
        assert_eq!(
            ramp_apply.ramp_frames_remaining,
            ramp_advance.ramp_frames_remaining
        );
    }

    #[test]
    fn test_advance_zero_is_noop() {
        let mut ramp = GainRamp::new(1000, 1.0);
        let gain_before = ramp.current_gain;
        let remaining_before = ramp.ramp_frames_remaining;
        let last_target_before = ramp.last_target;

        ramp.advance(0, 0.5);

        assert_eq!(ramp.current_gain, gain_before);
        assert_eq!(ramp.ramp_frames_remaining, remaining_before);
        // last_target should also be unchanged (advance(0) must not commit the target)
        assert_eq!(ramp.last_target, last_target_before);
    }

    #[test]
    fn test_advance_chunked_matches_single_call() {
        // Advancing in chunks should produce the same state as one big advance.
        let mut ramp_single = GainRamp::new(1000, 1.0); // 20 frames ramp
        let mut ramp_chunked = GainRamp::new(1000, 1.0);

        // Single advance of 20 frames
        ramp_single.advance(20, 0.0);

        // Chunked: 5 + 7 + 8 = 20 frames
        ramp_chunked.advance(5, 0.0);
        ramp_chunked.advance(7, 0.0);
        ramp_chunked.advance(8, 0.0);

        assert!(
            (ramp_single.current_gain - ramp_chunked.current_gain).abs() < 1e-5,
            "single={}, chunked={}: advance in chunks diverged",
            ramp_single.current_gain,
            ramp_chunked.current_gain
        );
        assert_eq!(
            ramp_single.ramp_frames_remaining,
            ramp_chunked.ramp_frames_remaining
        );
    }

    #[test]
    fn test_apply_channels_zero_returns_without_modifying() {
        let mut ramp = GainRamp::new(1000, 1.0);
        let mut data = [1.0, 2.0, 3.0];
        ramp.apply(&mut data, 0, 0.5);
        // Buffer should be untouched
        assert_eq!(data, [1.0, 2.0, 3.0]);
        // Ramp state should be unchanged (no target committed)
        assert!((ramp.current_gain - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_advance_beyond_ramp_duration() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20-frame ramp
                                                 // Advance 50 frames — ramp is only 20
        ramp.advance(50, 0.0);
        assert_eq!(ramp.ramp_frames_remaining, 0);
        assert!(
            (ramp.current_gain - 0.0).abs() < f32::EPSILON,
            "gain should snap to target when advance exceeds ramp: {}",
            ramp.current_gain
        );
    }

    #[test]
    fn test_perceptual_curve_monotonicity() {
        let gc = GainControl::new(100, false);
        let mut prev_gain = -1.0_f32;
        for v in 0..=100u8 {
            gc.set_volume(v);
            let gain = gc.gain();
            assert!(
                gain > prev_gain || (v == 0 && gain == 0.0),
                "non-monotonic at volume {v}: gain {gain} <= prev {prev_gain}"
            );
            prev_gain = gain;
        }
    }

    #[test]
    fn test_target_gain_clamps_out_of_range_bits() {
        let gc = GainControl::new(100, false);
        // Directly store an out-of-range gain value via the atomic
        gc.state
            .target_gain_bits
            .store(1.5_f32.to_bits(), Ordering::Relaxed);
        assert!(
            (gc.gain() - 1.0).abs() < f32::EPSILON,
            "out-of-range gain should be clamped to 1.0, got {}",
            gc.gain()
        );

        gc.state
            .target_gain_bits
            .store((-0.5_f32).to_bits(), Ordering::Relaxed);
        assert!(
            (gc.gain() - 0.0).abs() < f32::EPSILON,
            "negative gain should be clamped to 0.0, got {}",
            gc.gain()
        );
    }

    /// Verifies that non-finite gain bits fail safe to silence in release mode.
    /// (In debug mode, the `debug_assert!(gain.is_finite())` catches these.)
    #[test]
    #[cfg(not(debug_assertions))]
    fn test_target_gain_clamps_nan_and_inf() {
        let gc = GainControl::new(100, false);

        // NaN → silence
        gc.state
            .target_gain_bits
            .store(f32::NAN.to_bits(), Ordering::Relaxed);
        assert_eq!(gc.gain(), 0.0, "NaN gain should fail safe to 0.0 (silence)");

        // +Inf → silence
        gc.state
            .target_gain_bits
            .store(f32::INFINITY.to_bits(), Ordering::Relaxed);
        assert_eq!(
            gc.gain(),
            0.0,
            "+Inf gain should fail safe to 0.0 (silence)"
        );

        // -Inf → silence
        gc.state
            .target_gain_bits
            .store(f32::NEG_INFINITY.to_bits(), Ordering::Relaxed);
        assert_eq!(
            gc.gain(),
            0.0,
            "-Inf gain should fail safe to 0.0 (silence)"
        );
    }

    #[test]
    fn test_unity_gain_fast_path_leaves_buffer_unchanged() {
        let mut ramp = GainRamp::new(48000, 1.0);
        // Default state: gain = 1.0, no ramp active
        let original = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut data = original;
        ramp.apply(&mut data, 2, 1.0);
        // Buffer should be bit-for-bit identical (fast path returns early)
        assert_eq!(
            data, original,
            "unity gain fast path should not touch the buffer"
        );
    }

    #[test]
    fn test_ramp_and_steady_state_in_same_buffer() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20-frame ramp
                                                 // 40-frame mono buffer: first 20 frames ramp, last 20 are steady-state
        let mut data = vec![1.0; 40];
        ramp.apply(&mut data, 1, 0.5);

        assert_eq!(ramp.ramp_frames_remaining, 0);
        assert!((ramp.current_gain - 0.5).abs() < f32::EPSILON);

        // Ramp region (frames 0-19): should be monotonically decreasing
        for i in 1..20 {
            assert!(
                data[i] < data[i - 1],
                "ramp region not decreasing at frame {i}: {} >= {}",
                data[i],
                data[i - 1]
            );
        }

        // Steady region (frames 20-39): should all be exactly 0.5
        for (i, &s) in data[20..40].iter().enumerate() {
            assert!(
                (s - 0.5).abs() < f32::EPSILON,
                "steady region frame {}={s}, expected 0.5",
                i + 20
            );
        }
    }

    #[test]
    fn test_partial_advance_matches_apply() {
        // Advance 10 of 20 frames should produce the same ramp state as apply on 10 frames.
        let mut ramp_apply = GainRamp::new(1000, 1.0); // 20-frame ramp
        let mut ramp_advance = GainRamp::new(1000, 1.0);

        let mut buf = vec![1.0; 10];
        ramp_apply.apply(&mut buf, 1, 0.0);

        ramp_advance.advance(10, 0.0);

        assert!(
            (ramp_apply.current_gain - ramp_advance.current_gain).abs() < 1e-5,
            "partial apply={}, advance={}: diverged at 10/20 frames",
            ramp_apply.current_gain,
            ramp_advance.current_gain
        );
        assert_eq!(
            ramp_apply.ramp_frames_remaining, ramp_advance.ramp_frames_remaining,
            "remaining frames should match after partial advance"
        );
    }

    #[test]
    fn test_muted_steady_state_fills_zeros() {
        let mut ramp = GainRamp::new(1000, 1.0); // 20-frame ramp
                                                 // Buffer larger than ramp: 40 frames mono, target 0.0 (muted)
        let mut data = vec![1.0; 40];
        ramp.apply(&mut data, 1, 0.0);

        // Steady-state tail (frames 20-39) should be exactly 0.0 via fill(0.0)
        for (i, &s) in data[20..40].iter().enumerate() {
            assert_eq!(
                s,
                0.0,
                "muted steady-state frame {} should be exactly 0.0, got {s}",
                i + 20
            );
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "buffer length must be a multiple of channels")]
    fn test_apply_panics_on_non_multiple_of_channels() {
        let mut ramp = GainRamp::new(1000, 1.0);
        // 5 samples with 2 channels — not a multiple
        let mut data = vec![1.0; 5];
        ramp.apply(&mut data, 2, 0.5);
    }

    // --- Tests for non-default initial values ---

    #[test]
    fn test_gain_control_initial_volume() {
        let gc = GainControl::new(50, false);
        assert_eq!(gc.volume(), 50);
        assert!((gc.gain() - 0.353_553).abs() < 1e-3); // (50/100)^1.5
    }

    #[test]
    fn test_gain_control_initial_muted() {
        let gc = GainControl::new(75, true);
        assert_eq!(gc.volume(), 75);
        assert!(gc.is_muted());
        assert_eq!(gc.gain(), 0.0);

        // Unmuting restores the gain for volume 75
        gc.set_mute(false);
        assert!((gc.gain() - 0.649_519).abs() < 1e-3); // (75/100)^1.5
    }

    #[test]
    fn test_gain_control_initial_volume_clamps_above_100() {
        let gc = GainControl::new(200, false);
        assert_eq!(gc.volume(), 100);
        assert!((gc.gain() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_gain_control_initial_zero_volume() {
        let gc = GainControl::new(0, false);
        assert_eq!(gc.volume(), 0);
        assert_eq!(gc.gain(), 0.0);
    }

    #[test]
    fn test_gain_ramp_initial_gain_no_ramp() {
        let mut ramp = GainRamp::new(1000, 0.5);
        // Apply with target == initial: should be constant, no ramp
        let mut data = vec![1.0; 20];
        ramp.apply(&mut data, 1, 0.5);
        for (i, &s) in data.iter().enumerate() {
            assert!(
                (s - 0.5).abs() < f32::EPSILON,
                "frame {i}: expected 0.5, got {s}"
            );
        }
    }

    #[test]
    fn test_gain_ramp_initial_zero_ramps_up() {
        let mut ramp = GainRamp::new(1000, 0.0); // 20-frame ramp
        let mut data = vec![1.0; 20];
        ramp.apply(&mut data, 1, 1.0);

        // First sample should be near zero (starting from 0.0)
        assert!(
            data[0] < 0.1,
            "first sample should be near 0, got {}",
            data[0]
        );
        // Last sample should reach target
        assert!(
            (data[19] - 1.0).abs() < f32::EPSILON,
            "last sample should be 1.0, got {}",
            data[19]
        );
    }

    #[test]
    fn test_is_muted_reports_false_when_unmuted() {
        // `is_muted()` must reflect both states. The other tests all exercise
        // the `true` branch (asserting the *setter* took effect), so this one
        // pins the `false` branch and the `true → false` transition.
        let gc = GainControl::new(100, false);
        assert!(!gc.is_muted());
        gc.set_mute(true);
        gc.set_mute(false);
        assert!(
            !gc.is_muted(),
            "set_mute(false) should clear the muted flag"
        );
    }

    #[test]
    fn test_debug_includes_volume_and_mute_fields() {
        // The custom `fmt::Debug` impl must expose volume and mute state so
        // debug prints are actually useful. Pin the contract rather than
        // relying on the impl staying in sync with its fields.
        let gc = GainControl::new(42, true);
        let s = format!("{:?}", gc);
        assert!(s.contains("42"), "Debug output missing volume: {s}");
        assert!(s.contains("true"), "Debug output missing mute state: {s}");
    }

    #[test]
    fn test_apply_stereo_buffer_shorter_than_ramp() {
        // Pins the `data.len() / channels` frame calculation: for a stereo
        // buffer with fewer frames than `ramp_frames_remaining`, the split
        // point must be `frames * channels`, not `data.len() * channels`
        // (which would overrun the buffer) or any other arithmetic variant.
        let mut ramp = GainRamp::new(1000, 1.0); // 20-frame ramp
        let mut data = vec![1.0; 6]; // 3 stereo frames, well under the 20-frame ramp
        ramp.apply(&mut data, 2, 0.0);

        // All samples should have been touched by the ramp (gain < 1.0 as it
        // decays toward 0.0). Paired left/right in each frame share a gain.
        for i in 0..3 {
            assert!(
                data[i * 2] < 1.0 && data[i * 2] >= 0.0,
                "frame {i} L out of expected range: {}",
                data[i * 2]
            );
            assert!(
                (data[i * 2] - data[i * 2 + 1]).abs() < 1e-6,
                "frame {i} L/R differ: {} vs {}",
                data[i * 2],
                data[i * 2 + 1]
            );
        }

        // Exactly 3 frames of ramp should have been consumed.
        assert_eq!(
            ramp.ramp_frames_remaining, 17,
            "3 frames consumed from a 20-frame ramp should leave 17 remaining"
        );
    }
}
