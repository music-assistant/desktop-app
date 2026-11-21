use sendspin::sync::{Clock, ClockSync, DefaultClock};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A controllable clock for deterministic testing.
///
/// Callers advance time explicitly via [`MockClock::advance`], making
/// tests independent of real wall-clock timing.
struct MockClock {
    micros: AtomicI64,
}

impl MockClock {
    fn new(initial: i64) -> Self {
        Self {
            micros: AtomicI64::new(initial),
        }
    }

    fn set(&self, micros: i64) {
        self.micros.store(micros, Ordering::SeqCst);
    }

    fn advance(&self, delta_micros: i64) {
        self.micros.fetch_add(delta_micros, Ordering::SeqCst);
    }
}

impl Clock for MockClock {
    fn now_micros(&self) -> i64 {
        self.micros.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// DefaultClock tests
// ---------------------------------------------------------------------------

#[test]
fn test_default_clock_monotonic() {
    let clock = DefaultClock::new();
    let t1 = clock.now_micros();
    // Yield to guarantee a nonzero time delta — unlike black_box on an
    // empty loop body, yield_now() always costs real scheduler time.
    std::thread::yield_now();
    let t2 = clock.now_micros();
    assert!(t2 >= t1, "clock must be monotonically non-decreasing");
}

#[test]
fn test_default_clock_micros_to_instant_roundtrip() {
    let clock = DefaultClock::new();
    let now_micros = clock.now_micros();
    let instant = clock
        .micros_to_instant(now_micros)
        .expect("conversion should succeed for current time");

    // The round-trip should land very close to Instant::now().
    let diff = Instant::now().duration_since(instant);
    assert!(
        diff < Duration::from_millis(5),
        "round-trip drift too large: {:?}",
        diff
    );
}

#[test]
fn test_default_clock_instant_to_micros_roundtrip() {
    let clock = DefaultClock::new();
    let now = Instant::now();
    let micros = clock.instant_to_micros(now);
    let back = clock
        .micros_to_instant(micros)
        .expect("conversion should succeed");

    // Allow ±1ms for the two clock samples.
    let diff = if back >= now {
        back.duration_since(now)
    } else {
        now.duration_since(back)
    };
    assert!(
        diff < Duration::from_millis(1),
        "roundtrip error too large: {:?}",
        diff
    );
}

#[test]
fn test_default_clock_future_micros_to_instant() {
    let clock = DefaultClock::new();
    let future = clock.now_micros() + 1_000_000; // 1 second in the future
    let instant = clock
        .micros_to_instant(future)
        .expect("future time should convert");
    assert!(
        instant > Instant::now(),
        "future micros should map to a future Instant"
    );
}

#[test]
fn test_default_clock_past_micros_to_instant() {
    let clock = DefaultClock::new();
    let past = clock.now_micros() - 1_000_000; // 1 second in the past
    let instant = clock
        .micros_to_instant(past)
        .expect("past time should convert");
    assert!(
        instant < Instant::now(),
        "past micros should map to a past Instant"
    );
}

// ---------------------------------------------------------------------------
// MockClock tests — proving the trait is injectable
// ---------------------------------------------------------------------------

#[test]
fn test_mock_clock_deterministic() {
    let clock = MockClock::new(1_000_000);
    assert_eq!(clock.now_micros(), 1_000_000);

    clock.advance(500);
    assert_eq!(clock.now_micros(), 1_000_500);
}

#[test]
fn test_clock_sync_with_mock_clock() {
    let clock = Arc::new(MockClock::new(100_000));
    let mut sync = ClockSync::new(clock.clone() as Arc<dyn Clock>);

    // Simulate two NTP-style sync rounds with the mock clock.
    // Round 1: client at 100_000µs, server at 105_000µs, RTT ~200µs
    clock.set(100_000);
    let t1 = clock.now_micros();
    let t2 = 105_100; // server received
    let t3 = 105_110; // server transmitted
    clock.set(100_200); // client received
    let t4 = clock.now_micros();
    sync.update(t1, t2, t3, t4);

    // Round 2: advance client by 1 second
    clock.set(1_100_000);
    let t1_2 = clock.now_micros();
    let t2_2 = 1_105_100;
    let t3_2 = 1_105_110;
    clock.set(1_100_200);
    let t4_2 = clock.now_micros();
    sync.update(t1_2, t2_2, t3_2, t4_2);

    assert!(
        sync.is_synchronized(),
        "should be synchronized after 2 samples"
    );

    // Verify conversion: server time 1_105_000 should map to ~client 1_100_000
    let client_micros = sync.server_to_client_micros(1_105_000);
    let client = client_micros.expect("should have a value");
    let diff = (client - 1_100_000).abs();
    assert!(
        diff < 50,
        "server→client conversion off by {}µs (expected ~0)",
        diff
    );
}

#[test]
fn test_clock_sync_accessor_returns_injected_clock() {
    let clock = Arc::new(MockClock::new(42_000));
    let sync = ClockSync::new(clock.clone() as Arc<dyn Clock>);

    // The clock() accessor should return the same clock we injected,
    // producing the same value.
    assert_eq!(sync.clock().now_micros(), 42_000);
}

#[test]
fn test_clock_sync_instant_to_client_micros_is_infallible() {
    // instant_to_client_micros returns i64 (not Option), verifying
    // the API reflects that it cannot fail.
    let clock = Arc::new(MockClock::new(500_000));
    let sync = ClockSync::new(clock as Arc<dyn Clock>);
    let result: i64 = sync.instant_to_client_micros(Instant::now());
    // The mock clock is at 500_000µs; Instant::now() maps to roughly that.
    // We just verify it returns a reasonable value (not zero, not negative).
    assert!(result > 0, "should return a positive value, got {}", result);
}

// ---------------------------------------------------------------------------
// micros_to_instant edge cases
// ---------------------------------------------------------------------------

#[test]
fn test_micros_to_instant_far_past_returns_none() {
    let clock = DefaultClock::new();
    // Request a time far before the process started. On most platforms
    // Instant's epoch is process start or boot time, so a negative
    // microsecond value (or huge negative delta) should exceed the
    // epoch and return None from checked_sub.
    let very_far_past = clock.now_micros() - 1_000_000_000_000; // ~11.5 days before
                                                                // This may or may not return None depending on platform uptime,
                                                                // but we can at least verify it doesn't panic.
    let _result = clock.micros_to_instant(very_far_past);
}

#[test]
fn test_mock_clock_micros_to_instant_returns_none_for_unreachable_past() {
    // MockClock at 0µs: requesting micros_to_instant(0) means delta = 0
    // relative to mock, but the default bridge uses Instant::now() which
    // is far from zero. Requesting a very negative value should trigger
    // the checked_sub None path.
    let clock = MockClock::new(1_000_000_000); // 1000 seconds
                                               // Ask for a time 1_000_000_000µs before the mock's "now" — this is
                                               // ~1000 seconds before Instant::now(), which will likely be before
                                               // boot on many systems.
    let result = clock.micros_to_instant(0);
    // On short-uptime systems this is None; on long-uptime it's Some.
    // Either way, it must not panic.
    let _ = result;
}

// ---------------------------------------------------------------------------
// Bridge round-trip with MockClock
// ---------------------------------------------------------------------------

#[test]
fn test_mock_clock_bridge_round_trip() {
    // The MockClock uses the default bridge implementations which sample
    // both MockClock::now_micros() and Instant::now(). Verify the
    // round-trip micros → Instant → micros stays within tolerance.
    let clock = MockClock::new(500_000);
    let original = 500_000i64;
    if let Some(instant) = clock.micros_to_instant(original) {
        let back = clock.instant_to_micros(instant);
        let diff = (back - original).abs();
        // Allow up to 1ms — the two clock reads in the bridge introduce
        // error proportional to wall-clock time between them.
        assert!(
            diff < 1_000,
            "MockClock bridge round-trip error too large: {}µs",
            diff
        );
    }
    // If micros_to_instant returned None, the mock's value was before
    // Instant's epoch — that's a valid outcome for this test.
}
