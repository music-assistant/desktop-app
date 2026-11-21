// ABOUTME: Clock synchronization implementation
// ABOUTME: Drift-aware time sync with RTT estimation and server/client time conversion

use super::raw_clock::{Clock, DefaultClock};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADAPTIVE_FORGETTING_CUTOFF: f64 = 0.75;

fn sync_timing_log_sample(count: u64) -> bool {
    count != 0 && (count <= 10 || count.is_multiple_of(30))
}

fn rejected_sync_warning(streak: u64) -> bool {
    streak >= 10 && (streak - 10).is_multiple_of(30)
}

fn log_sync_timing_sample(sample: u64, t1: i64, t2: i64, t3: i64, t4: i64, rtt: i64) {
    if sync_timing_log_sample(sample) {
        log::info!(
            "Clock sync receive sample {}: t1={}µs, t2={}µs, t3={}µs, t4={}µs, rtt={}µs (t4 captured before JSON parsing)",
            sample, t1, t2, t3, t4, rtt
        );
    }
}

#[derive(Debug, Clone, Copy)]
struct TimeElement {
    last_update: i64,
    offset: f64,
    drift: f64,
    use_drift: bool,
}

#[derive(Debug)]
struct TimeFilter {
    last_update: i64,
    count: u32,
    offset: f64,
    drift: f64,
    offset_covariance: f64,
    offset_drift_covariance: f64,
    drift_covariance: f64,
    process_variance: f64,
    forget_variance_factor: f64,
    current: TimeElement,
}

impl TimeFilter {
    fn new(process_std_dev: f64, forget_factor: f64) -> Self {
        let process_variance = process_std_dev * process_std_dev;
        let forget_variance_factor = forget_factor * forget_factor;
        Self {
            last_update: 0,
            count: 0,
            offset: 0.0,
            drift: 0.0,
            offset_covariance: f64::INFINITY,
            offset_drift_covariance: 0.0,
            drift_covariance: 0.0,
            process_variance,
            forget_variance_factor,
            current: TimeElement {
                last_update: 0,
                offset: 0.0,
                drift: 0.0,
                use_drift: false,
            },
        }
    }

    fn update(&mut self, measurement: i64, max_error: i64, time_added: i64) {
        if time_added <= self.last_update {
            log::warn!(
                "TimeFilter: non-monotonic sample rejected (t4={time_added} <= last_update={})",
                self.last_update
            );
            return;
        }

        let dt = (time_added - self.last_update) as f64;
        self.last_update = time_added;

        let update_std_dev = max_error as f64;
        let measurement_variance = update_std_dev * update_std_dev;

        if self.count == 0 {
            self.count = 1;
            self.offset = measurement as f64;
            self.offset_covariance = measurement_variance;
            self.drift = 0.0;
            self.current = TimeElement {
                last_update: self.last_update,
                offset: self.offset,
                drift: self.drift,
                use_drift: Self::drift_has_sufficient_snr(self.drift, self.drift_covariance),
            };
            return;
        }

        if self.count == 1 {
            self.count = 2;
            if dt < Self::MIN_DRIFT_BASELINE_MICROS {
                // Δoffset/Δt over a tiny Δt is measurement noise amplified
                // without bound. Seed drift at zero with uncertainty from the
                // floored baseline; later Kalman updates refine it.
                self.drift = 0.0;
                self.drift_covariance = (self.offset_covariance + measurement_variance)
                    / (Self::MIN_DRIFT_BASELINE_MICROS * Self::MIN_DRIFT_BASELINE_MICROS);
            } else {
                self.drift = (measurement as f64 - self.offset) / dt;
                // Divide by dt^2, not dt: variance of (x/c) is var(x)/c^2.
                self.drift_covariance = (self.offset_covariance + measurement_variance) / (dt * dt);
            }
            self.offset = measurement as f64;
            self.offset_covariance = measurement_variance;
            self.current = TimeElement {
                last_update: self.last_update,
                offset: self.offset,
                drift: self.drift,
                use_drift: Self::drift_has_sufficient_snr(self.drift, self.drift_covariance),
            };
            return;
        }

        let predicted_offset = self.offset + self.drift * dt;
        let dt_squared = dt * dt;

        let drift_process_variance = 0.0;
        let mut new_drift_covariance = self.drift_covariance + drift_process_variance;

        let offset_drift_process_variance = 0.0;
        let mut new_offset_drift_covariance = self.offset_drift_covariance
            + self.drift_covariance * dt
            + offset_drift_process_variance;

        let offset_process_variance = dt * self.process_variance;
        let mut new_offset_covariance = self.offset_covariance
            + 2.0 * self.offset_drift_covariance * dt
            + self.drift_covariance * dt_squared
            + offset_process_variance;

        let residual = measurement as f64 - predicted_offset;
        let max_residual_cutoff = max_error as f64 * ADAPTIVE_FORGETTING_CUTOFF;

        if self.count < 100 {
            self.count += 1;
        } else if residual.abs() > max_residual_cutoff {
            log::debug!(
                "TimeFilter: outlier residual={:.1}µs exceeds cutoff={:.1}µs, inflating covariance",
                residual,
                max_residual_cutoff
            );
            new_drift_covariance *= self.forget_variance_factor;
            new_offset_drift_covariance *= self.forget_variance_factor;
            new_offset_covariance *= self.forget_variance_factor;
        }

        let uncertainty = 1.0 / (new_offset_covariance + measurement_variance);
        let offset_gain = new_offset_covariance * uncertainty;
        let drift_gain = new_offset_drift_covariance * uncertainty;

        self.offset = predicted_offset + offset_gain * residual;
        self.drift += drift_gain * residual;

        self.drift_covariance = new_drift_covariance - drift_gain * new_offset_drift_covariance;
        self.offset_drift_covariance =
            new_offset_drift_covariance - drift_gain * new_offset_covariance;
        self.offset_covariance = new_offset_covariance - offset_gain * new_offset_covariance;

        self.current = TimeElement {
            last_update: self.last_update,
            offset: self.offset,
            drift: self.drift,
            use_drift: Self::drift_has_sufficient_snr(self.drift, self.drift_covariance),
        };
    }

    /// Minimum baseline for estimating drift from the first two samples.
    /// The fast-start pair can arrive bunched to near-zero spacing, so it
    /// never qualifies; drift converges through later Kalman updates.
    const MIN_DRIFT_BASELINE_MICROS: f64 = 100_000.0;

    /// Maximum plausible drift magnitude. Real clock drift between
    /// hardware oscillators is measured in ppm; 20% is far beyond
    /// physical reality. This is a last-resort safety net for a
    /// diverged filter, not a quality gate.
    const MAX_DRIFT: f64 = 0.2;

    /// Minimum drift signal-to-noise ratio before applying drift to
    /// conversions. `drift_covariance` is variance, so compare squared
    /// drift against k² × covariance to avoid taking a square root.
    const DRIFT_SNR_SIGMA: f64 = 2.0;

    fn drift_is_plausible(&self) -> bool {
        // Accept only drift <= threshold; reject NaN/incomparable values.
        matches!(
            self.current.drift.abs().partial_cmp(&Self::MAX_DRIFT),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        )
    }

    fn drift_has_sufficient_snr(drift: f64, drift_covariance: f64) -> bool {
        if !matches!(
            drift_covariance.partial_cmp(&0.0),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ) {
            return false;
        }

        let threshold = Self::DRIFT_SNR_SIGMA * Self::DRIFT_SNR_SIGMA * drift_covariance;
        drift * drift >= threshold
    }

    /// True when the filter would apply its drift estimate but it is beyond
    /// physical plausibility. An implausible estimate the SNR gate already
    /// rejects blocks nothing: conversions then run with zero drift.
    fn conversions_blocked(&self) -> bool {
        self.current.use_drift && !self.drift_is_plausible()
    }

    fn effective_drift(&self) -> Option<f64> {
        if self.conversions_blocked() {
            return None;
        }

        Some(if self.current.use_drift {
            self.current.drift
        } else {
            0.0
        })
    }

    fn compute_server_time(&self, client_time: i64) -> Option<i64> {
        let effective_drift = self.effective_drift()?;
        let dt = (client_time - self.current.last_update) as f64;
        let offset = self.current.offset + effective_drift * dt;
        Some(client_time + offset.round() as i64)
    }

    fn compute_client_time(&self, server_time: i64) -> Option<i64> {
        let effective_drift = self.effective_drift()?;
        let numerator = server_time as f64 - self.current.offset
            + effective_drift * self.current.last_update as f64;
        Some((numerator / (1.0 + effective_drift)).round() as i64)
    }

    fn is_synchronized(&self) -> bool {
        self.count >= 2 && self.offset_covariance.is_finite()
    }

    /// Samples required before the estimate is trusted for fine-grained
    /// playback corrections. At the sync cadence (two fast-start samples,
    /// then 1Hz — see `ProtocolClient`), ten samples settle ~9s after
    /// connect.
    const SETTLE_MIN_SAMPLES: u32 = 10;

    fn is_settled(&self) -> bool {
        self.count >= Self::SETTLE_MIN_SAMPLES
    }
}

/// Clock synchronization quality
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncQuality {
    /// Good synchronization (RTT < 50ms)
    Good,
    /// Degraded synchronization (RTT 50-100ms)
    Degraded,
    /// Lost synchronization (RTT > 100ms or no sync)
    Lost,
}

/// Clock synchronization state
pub struct ClockSync {
    /// Last known RTT in microseconds
    rtt_micros: Option<i64>,
    /// When we computed this (for staleness detection)
    last_update: Option<Instant>,
    /// Drift-aware time filter
    filter: TimeFilter,
    /// Raw monotonic clock used for timestamps
    clock: Arc<dyn Clock>,
    /// Per-connection diagnostic counters, saturated to remain bounded.
    sync_samples: u64,
    rejected_streak: u64,
}

impl std::fmt::Debug for ClockSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClockSync")
            .field("rtt_micros", &self.rtt_micros)
            .field("last_update", &self.last_update)
            .field("filter", &self.filter)
            .finish()
    }
}

impl ClockSync {
    /// Create a new clock synchronization instance with the given clock.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            rtt_micros: None,
            last_update: None,
            filter: TimeFilter::new(0.01, 1.001),
            clock,
            sync_samples: 0,
            rejected_streak: 0,
        }
    }

    /// Get a clone of the underlying clock.
    ///
    /// Useful for generating timestamps (t1, t4) in the same timebase
    /// that the filter operates on, without holding the `ClockSync` lock.
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Update clock sync with new measurement.
    ///
    /// - `t1` = client_transmitted (raw monotonic µs from [`Clock::now_micros`])
    /// - `t2` = server_received (server loop µs)
    /// - `t3` = server_transmitted (server loop µs)
    /// - `t4` = client_received (raw monotonic µs from [`Clock::now_micros`])
    pub fn update(&mut self, t1: i64, t2: i64, t3: i64, t4: i64) {
        // RTT = (t4 - t1) - (t3 - t2)
        let rtt = (t4 - t1) - (t3 - t2);
        self.sync_samples = self.sync_samples.saturating_add(1);

        // Discard negative RTT (misordered timestamps) and high RTT
        // (network congestion). Store only valid RTT so that quality()
        // doesn't report Good on a negative value.
        if !(0..=100_000).contains(&rtt) {
            self.rejected_streak = self.rejected_streak.saturating_add(1);
            if rejected_sync_warning(self.rejected_streak) {
                log::warn!(
                    "Clock sync discarded {} consecutive samples (latest RTT={}µs outside 0..=100000µs); samples above 100ms can leave initial clock sync unavailable",
                    self.rejected_streak,
                    rtt
                );
            }
            // Preserve the existing per-discard debug diagnostic.
            log::debug!(
                "Clock sync sample discarded: rtt={}µs outside 0..=100000µs (t1={}, t2={}, t3={}, t4={})",
                rtt,
                t1,
                t2,
                t3,
                t4
            );
            log_sync_timing_sample(self.sync_samples, t1, t2, t3, t4, rtt);
            return;
        }
        self.rejected_streak = 0;
        self.rtt_micros = Some(rtt);

        // NTP offset = ((t2 - t1) + (t3 - t4)) / 2
        // Use f64 division to avoid systematic ±0.5µs bias from integer truncation.
        let measurement = (((t2 - t1) as f64 + (t3 - t4) as f64) / 2.0).round() as i64;
        // Floor max_error at 1µs so the Kalman filter always sees
        // nonzero measurement variance. RTT = 0 is legitimate on
        // localhost; without this floor, repeated zero-variance
        // samples would drive covariance to zero and eventually NaN.
        let max_error = (rtt / 2).max(1);

        let was_synced = self.filter.is_synchronized();
        let was_blocked = self.filter.conversions_blocked();
        self.filter.update(measurement, max_error, t4);
        self.last_update = Some(Instant::now());
        log::trace!(
            "Clock sync sample: offset={:.0}µs, rtt={}µs, drift={:.6}, synchronized={}",
            self.filter.offset,
            rtt,
            self.filter.current.drift,
            self.filter.is_synchronized(),
        );
        if !was_synced && self.filter.is_synchronized() {
            log::info!(
                "Clock sync achieved: offset={:.0}µs, rtt={}µs",
                self.filter.offset,
                rtt
            );
        }
        if !was_blocked && self.filter.conversions_blocked() {
            log::warn!(
                "Clock sync: in-use drift {:.4} exceeded plausibility limit {}; \
                 time conversions will return None until it recovers",
                self.filter.current.drift,
                TimeFilter::MAX_DRIFT
            );
        }
        log_sync_timing_sample(self.sync_samples, t1, t2, t3, t4, rtt);
    }

    /// Get current RTT in microseconds
    pub fn rtt_micros(&self) -> Option<i64> {
        self.rtt_micros
    }

    /// Convert server loop microseconds to client clock microseconds
    pub fn server_to_client_micros(&self, server_micros: i64) -> Option<i64> {
        if !self.filter.is_synchronized() {
            return None;
        }
        self.filter.compute_client_time(server_micros)
    }

    /// Convert client clock microseconds to server loop microseconds
    pub fn client_to_server_micros(&self, client_micros: i64) -> Option<i64> {
        if !self.filter.is_synchronized() {
            return None;
        }
        self.filter.compute_server_time(client_micros)
    }

    /// Convert server loop microseconds to local Instant
    pub fn server_to_local_instant(&self, server_micros: i64) -> Option<Instant> {
        let client_micros = self.server_to_client_micros(server_micros)?;
        self.client_micros_to_instant(client_micros)
    }

    /// Convert server loop microseconds to local Instant, compensating for output latency
    pub fn server_to_local_instant_with_latency(
        &self,
        server_micros: i64,
        output_latency_micros: u64,
    ) -> Option<Instant> {
        let instant = self.server_to_local_instant(server_micros)?;
        instant.checked_sub(Duration::from_micros(output_latency_micros))
    }

    fn client_micros_to_instant(&self, client_micros: i64) -> Option<Instant> {
        self.clock.micros_to_instant(client_micros)
    }

    /// Convert a local Instant to client clock microseconds.
    pub fn instant_to_client_micros(&self, instant: Instant) -> i64 {
        self.clock.instant_to_micros(instant)
    }

    /// Get sync quality based on RTT
    pub fn quality(&self) -> SyncQuality {
        match self.rtt_micros {
            Some(rtt) if rtt < 50_000 => SyncQuality::Good,
            Some(rtt) if rtt < 100_000 => SyncQuality::Degraded,
            _ => SyncQuality::Lost,
        }
    }

    /// Check if sync is stale (>5 seconds since last update).
    ///
    /// Uses `Instant::now()` (not the injected [`Clock`]) because staleness
    /// is a wall-clock concept — we're measuring real elapsed time since the
    /// last successful sync round, regardless of which timebase the filter
    /// operates on. This means `is_stale()` always reflects real time even
    /// when a mock clock is injected for testing.
    pub fn is_stale(&self) -> bool {
        match self.last_update {
            Some(last) => last.elapsed() > Duration::from_secs(5),
            None => true,
        }
    }

    /// Check if clock sync has converged
    pub fn is_synchronized(&self) -> bool {
        self.filter.is_synchronized()
    }

    /// Whether the estimate has settled enough to drive fine-grained
    /// playback corrections. [`is_synchronized`](Self::is_synchronized)
    /// means conversions are available at all: playback starts when
    /// synchronized; the correction planner waits until settled.
    pub fn is_settled(&self) -> bool {
        self.filter.is_settled()
    }
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new(Arc::new(DefaultClock::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_samples_first_ten_then_every_thirty() {
        let sampled: Vec<u64> = (0..=91).filter(|&n| sync_timing_log_sample(n)).collect();
        assert_eq!(sampled, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 30, 60, 90]);
    }

    #[test]
    fn discarded_sample_warning_throttles_and_resets_after_acceptance() {
        assert_eq!(
            (0..=100)
                .filter(|&n| rejected_sync_warning(n))
                .collect::<Vec<_>>(),
            [10, 40, 70, 100]
        );
        let mut sync = ClockSync::default();
        for n in 0..10 {
            sync.update(n * 200_000, 0, 0, n * 200_000 + 150_000);
        }
        assert_eq!(sync.rejected_streak, 10);
        assert_eq!(sync.sync_samples, 10);
        assert_eq!(sync.rtt_micros(), None);
        sync.update(2_000_000, 0, 0, 2_000_020);
        assert_eq!(sync.rejected_streak, 0);
        assert_eq!(sync.rtt_micros(), Some(20));
        sync.update(2_200_000, 0, 0, 2_350_000);
        assert_eq!(sync.rejected_streak, 1);
    }

    /// Feed enough samples to exit the count==0 and count==1 bootstrap paths
    /// and put the filter into the steady-state Kalman branch, where negative
    /// `dt` would actually corrupt the prediction/covariance math.
    fn primed_filter() -> TimeFilter {
        let mut f = TimeFilter::new(0.01, 1.001);
        f.update(100, 10, 1_000);
        f.update(120, 10, 2_000);
        f.update(140, 10, 3_000);
        assert_eq!(f.count, 3);
        assert_eq!(f.last_update, 3_000);
        f
    }

    /// Snapshot every bit of filter state that a monotonicity-guard bypass
    /// could perturb. If the guard works, a rejected sample leaves all of
    /// these untouched.
    fn snapshot(f: &TimeFilter) -> (i64, u32, f64, f64, f64, f64, f64) {
        (
            f.last_update,
            f.count,
            f.offset,
            f.drift,
            f.offset_covariance,
            f.drift_covariance,
            f.offset_drift_covariance,
        )
    }

    #[test]
    fn update_rejects_backwards_time_added() {
        let mut f = primed_filter();
        let before = snapshot(&f);

        // t4 < last_update: pre-fix code would compute negative dt and
        // corrupt the Kalman prediction. Post-fix, the guard rejects it.
        f.update(999, 10, 2_500);

        assert_eq!(snapshot(&f), before, "backwards t4 must not mutate state");
    }

    #[test]
    fn update_rejects_duplicate_time_added() {
        let mut f = primed_filter();
        let before = snapshot(&f);

        // Equal t4: original == guard already handled this; make sure the
        // <= relaxation didn't accidentally change behavior for duplicates.
        f.update(999, 10, 3_000);

        assert_eq!(snapshot(&f), before, "duplicate t4 must not mutate state");
    }

    #[test]
    fn update_accepts_forward_time_added() {
        let mut f = primed_filter();
        let before = snapshot(&f);

        f.update(160, 10, 4_000);

        assert_ne!(snapshot(&f), before, "forward t4 must advance the filter");
        assert_eq!(f.last_update, 4_000);
    }

    #[test]
    fn backwards_sample_does_not_affect_later_predictions() {
        // End-to-end: a rejected backwards sample must leave future
        // conversions identical to a filter that never saw it at all.
        let mut with_bad = TimeFilter::new(0.01, 1.001);
        let mut without_bad = TimeFilter::new(0.01, 1.001);

        for (m, t) in [(100, 1_000), (120, 2_000), (140, 3_000)] {
            with_bad.update(m, 10, t);
            without_bad.update(m, 10, t);
        }

        // Inject a backwards sample into one filter only.
        with_bad.update(9_999, 10, 1_500);

        // Then drive both forward identically.
        for (m, t) in [(160, 4_000), (180, 5_000)] {
            with_bad.update(m, 10, t);
            without_bad.update(m, 10, t);
        }

        assert_eq!(
            with_bad.compute_server_time(10_000),
            without_bad.compute_server_time(10_000),
        );
        assert_eq!(
            with_bad.compute_client_time(10_000),
            without_bad.compute_client_time(10_000),
        );
    }

    #[test]
    fn bunched_first_pair_seeds_zero_drift_and_keeps_conversions() {
        let mut f = TimeFilter::new(0.01, 1.001);
        // Fast-start pair arriving 190µs apart with a large offset delta:
        // pre-fix, this computed drift = -6037/190 ≈ -31.8 and then blocked
        // all conversions on implausibility for a full sample interval.
        f.update(1_000_000, 12_797, 1_000);
        f.update(994_000, 6_813, 1_190);

        assert_eq!(f.drift, 0.0, "tiny-baseline pair must not estimate drift");
        assert!(f.is_synchronized());
        assert!(
            f.compute_client_time(2_000_000).is_some(),
            "conversions must be available right after the fast-start pair"
        );
    }

    #[test]
    fn long_baseline_pair_still_estimates_drift() {
        let mut f = TimeFilter::new(0.01, 1.001);
        f.update(1_000, 10, 1_000);
        // dt = 200ms, at/above the baseline floor: quotient drift applies.
        f.update(1_200, 10, 201_000);
        assert!((f.drift - (200.0 / 200_000.0)).abs() < 1e-12);
    }

    #[test]
    fn unused_implausible_drift_does_not_block_conversions() {
        let mut f = primed_filter();
        // A diverged-looking drift that the SNR gate refuses to use must
        // not veto conversions: effective drift is zero, which is safe.
        f.current.drift = 5.0;
        f.current.use_drift = false;
        assert!(!f.conversions_blocked());
        assert_eq!(f.effective_drift(), Some(0.0));
    }

    #[test]
    fn in_use_implausible_drift_blocks_conversions() {
        let mut f = primed_filter();
        f.current.drift = 5.0;
        f.current.use_drift = true;
        assert!(f.conversions_blocked());
        assert_eq!(f.effective_drift(), None);
    }

    #[test]
    fn in_use_plausible_drift_is_applied() {
        let mut f = primed_filter();
        f.current.drift = 0.0001;
        f.current.use_drift = true;
        assert_eq!(f.effective_drift(), Some(0.0001));
    }

    #[test]
    fn settles_after_minimum_samples() {
        let mut f = TimeFilter::new(0.01, 1.001);
        for i in 0..i64::from(TimeFilter::SETTLE_MIN_SAMPLES) {
            assert!(!f.is_settled(), "must not settle before sample {}", i + 1);
            f.update(1_000 + i, 10, 1_000 + i * 1_000_000);
        }
        assert!(f.is_settled());
    }
}
