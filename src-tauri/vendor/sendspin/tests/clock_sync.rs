use sendspin::sync::{ClockSync, DefaultClock, SyncQuality};
use std::sync::Arc;

/// Assert that two values are within tolerance (microseconds precision)
fn assert_within(actual: Option<i64>, expected: i64, tolerance: i64) {
    let actual = actual.expect("expected Some value");
    let diff = (actual - expected).abs();
    assert!(
        diff <= tolerance,
        "expected {} ± {}, got {} (diff: {})",
        expected,
        tolerance,
        actual,
        diff
    );
}

#[test]
fn test_fresh_clock_sync_initial_state() {
    let sync = ClockSync::new(Arc::new(DefaultClock::new()));

    assert_eq!(sync.rtt_micros(), None);
    assert!(!sync.is_synchronized());
    assert_eq!(sync.quality(), SyncQuality::Lost);
    assert!(sync.is_stale());
    assert_eq!(sync.server_to_client_micros(1000), None);
    assert_eq!(sync.client_to_server_micros(1000), None);
}

#[test]
fn test_single_update_not_synchronized() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // One sample isn't enough for the filter to converge (needs count >= 2)
    sync.update(1_000_000, 500_000, 500_010, 1_000_040);

    assert_eq!(sync.rtt_micros(), Some(30));
    assert!(
        !sync.is_synchronized(),
        "single update should not synchronize"
    );
    assert_eq!(
        sync.server_to_client_micros(500_000),
        None,
        "should return None when not synchronized"
    );
}

#[test]
fn test_clock_sync_rtt_calculation() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Simulate sync: client sends at 1000µs, server receives at 500µs (server loop time)
    let t1 = 1_000_000; // Client transmitted (Unix µs)
    let t2 = 500_000; // Server received (server loop µs)
    let t3 = 500_010; // Server transmitted (server loop µs)
    let t4 = 1_000_050; // Client received (Unix µs)

    sync.update(t1, t2, t3, t4);

    // RTT = (t4 - t1) - (t3 - t2) = 50 - 10 = 40µs
    assert_eq!(sync.rtt_micros(), Some(40));
}

#[test]
fn test_server_to_client_conversion() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    let t1 = 1_000_000;
    let t2 = 1_005_100;
    let t3 = 1_005_100;
    let t4 = 1_000_200;

    sync.update(t1, t2, t3, t4);
    sync.update(2_000_000, 2_005_100, 2_005_100, 2_000_200);

    let client_micros = sync.server_to_client_micros(2_005_000);
    // Kalman filter may introduce small rounding errors
    assert_within(client_micros, 2_000_000, 10);
}

#[test]
fn test_sync_quality() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Good RTT (30µs)
    sync.update(1_000_000, 500_000, 500_010, 1_000_040);
    assert_eq!(sync.quality(), sendspin::sync::SyncQuality::Good);

    // Degraded RTT (75ms = 75,000µs)
    sync.update(2_000_000, 600_000, 600_010, 2_075_010);
    assert_eq!(sync.quality(), SyncQuality::Degraded);
}

#[test]
fn test_sync_quality_recovery() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Start with degraded RTT (75ms)
    sync.update(1_000_000, 600_000, 600_010, 1_075_010);
    assert_eq!(sync.quality(), SyncQuality::Degraded);

    // Recover with good RTT (20µs)
    sync.update(2_000_000, 700_000, 700_010, 2_000_030);
    assert_eq!(sync.quality(), SyncQuality::Good);
}

#[test]
fn test_sync_quality_unchanged_after_high_rtt() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // First, establish a good RTT (30µs)
    sync.update(1_000_000, 500_000, 500_010, 1_000_040);
    assert_eq!(sync.quality(), SyncQuality::Good);
    assert_eq!(sync.rtt_micros(), Some(30));

    // Now provide a very high RTT (> 100_000µs), which should be discarded
    // RTT = (t4 - t1) - (t3 - t2) = (3_100_020 - 3_000_000) - (700_010 - 700_000)
    //     = 100_020 - 10 = 100_010µs
    sync.update(3_000_000, 700_000, 700_010, 3_100_020);

    // The high RTT should not overwrite the previously stored good RTT,
    // and the quality should remain unchanged.
    assert_eq!(sync.quality(), SyncQuality::Good);
    assert_eq!(sync.rtt_micros(), Some(30));
}

#[test]
fn test_timestamp_boundary_zero_values() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // All-zero timestamps: RTT = (0 - 0) - (0 - 0) = 0, which is valid
    sync.update(0, 0, 0, 0);
    assert_eq!(sync.rtt_micros(), Some(0));
}

#[test]
fn test_clock_drift_correction() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Low-RTT samples keep drift covariance low enough for the SNR gate to
    // permit drift correction in both conversion directions.
    sync.update(1_000_000, 1_005_100, 1_005_100, 1_000_000);
    sync.update(2_000_000, 2_005_200, 2_005_200, 2_000_000);

    let server_time = sync.client_to_server_micros(3_000_000);
    // Kalman filter may introduce small rounding errors
    assert_within(server_time, 3_005_300, 10);
}

#[test]
fn test_conversions_use_offset_only_when_drift_snr_is_low() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // First sample: offset = +100µs with tight uncertainty.
    sync.update(1_000, 1_100, 1_100, 1_000);
    // Second sample 200ms later (a legitimate drift baseline): offset =
    // +120µs, but a 1ms RTT gives the drift estimate high covariance. Its
    // SNR is below 2σ, so both conversion directions should ignore drift
    // and use only the current offset.
    sync.update(200_000, 200_620, 200_620, 201_000);

    assert!(sync.is_synchronized());
    assert_eq!(sync.server_to_client_micros(3_120), Some(3_000));
    assert_eq!(sync.client_to_server_micros(3_000), Some(3_120));
}

#[test]
fn test_conversions_apply_drift_when_snr_is_high() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Offset moves +100µs → +4100µs over a 200ms baseline (drift = 0.02),
    // with low-RTT samples. The covariance is small enough for the 2σ SNR
    // gate to pass, so conversions apply the drift estimate.
    sync.update(1_000, 1_100, 1_100, 1_000);
    sync.update(201_000, 205_100, 205_100, 201_000);

    assert!(sync.is_synchronized());
    assert_eq!(sync.server_to_client_micros(3_120), Some(2_980));
    assert_eq!(sync.client_to_server_micros(3_000), Some(3_140));
}

#[test]
fn test_diverged_drift_returns_none() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Two samples 200ms apart (a legitimate drift baseline) where the NTP
    // offset collapses from +100µs to -99_900µs, giving drift = -0.5 with
    // high SNR — far beyond any real hardware clock skew. The filter would
    // apply that drift, so both conversions must refuse and return None.
    sync.update(1_000, 1_100, 1_100, 1_000);
    sync.update(201_000, 101_100, 101_100, 201_000);

    assert!(
        sync.server_to_client_micros(2000).is_none(),
        "server_to_client should return None when drift has diverged"
    );
    assert!(
        sync.client_to_server_micros(2000).is_none(),
        "client_to_server should return None when drift has diverged"
    );
}

#[test]
fn test_negative_rtt_discarded() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // Craft timestamps where t4 < t1 (response "before" request),
    // producing negative RTT. Should be silently discarded.
    sync.update(1000, 500, 500, 900);
    assert_eq!(sync.rtt_micros(), None, "negative RTT should not be stored");
    assert!(
        !sync.is_synchronized(),
        "filter should not advance on invalid RTT"
    );
}

#[test]
fn test_zero_rtt_accepted() {
    let mut sync = ClockSync::new(Arc::new(DefaultClock::new()));

    // RTT = 0 is legitimate on localhost. The filter clamps max_error
    // to 1µs so zero-variance samples don't corrupt covariance.
    sync.update(1000, 1100, 1100, 1000);
    sync.update(2000, 2100, 2100, 2000);
    assert_eq!(sync.rtt_micros(), Some(0));
    assert!(sync.is_synchronized(), "zero RTT should still allow sync");
}
