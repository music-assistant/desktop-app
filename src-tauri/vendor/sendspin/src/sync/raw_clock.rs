// ABOUTME: Clock trait and raw monotonic clock implementation
// ABOUTME: Provides NTP-slew-immune time source for accurate clock synchronization

use std::time::{Duration, Instant};

/// A monotonic time source for clock synchronization.
///
/// Implementations must return microseconds from a stable, monotonic source
/// that is **not** conditioned by NTP rate adjustments (slewing). On Linux,
/// this means `CLOCK_MONOTONIC_RAW` rather than `CLOCK_MONOTONIC`.
///
/// # Why not `std::time::Instant`?
///
/// Rust's `Instant` uses `CLOCK_MONOTONIC` on Linux, which is subject to NTP
/// frequency discipline. When NTP adjusts the tick rate to synchronize with
/// upstream sources, `CLOCK_MONOTONIC` speeds up or slows down slightly. At
/// audio-sync precision (~1ms), even small slew corrections (a few ms over
/// minutes) can destabilize a Kalman filter that's tracking clock offset and
/// drift between client and server.
///
/// On macOS (`mach_absolute_time`) and Windows (`QueryPerformanceCounter`),
/// `Instant` is already raw/unconditioned, so this distinction only matters
/// on Linux.
pub trait Clock: Send + Sync + 'static {
    /// Monotonic microseconds from an arbitrary epoch.
    ///
    /// The epoch is implementation-defined and need not relate to Unix time.
    /// The only requirement is that successive calls return non-decreasing
    /// values on a single thread, and the timebase is not NTP-conditioned.
    fn now_micros(&self) -> i64;

    /// Convert this clock's microseconds to a [`std::time::Instant`].
    ///
    /// This bridges between the raw monotonic timebase and Rust's `Instant`,
    /// which is needed for interop with `tokio`, `cpal`, and the audio
    /// scheduler. The default implementation samples both clocks and computes
    /// the delta — the bridge error is typically under 10µs, bounded by
    /// the time between the two clock reads (which can grow under scheduler
    /// pressure, but is sub-microsecond in the common case).
    ///
    /// Returns `None` if the requested time is so far in the past that it
    /// precedes `Instant`'s internal epoch (before process start).
    fn micros_to_instant(&self, micros: i64) -> Option<Instant> {
        let now_micros = self.now_micros();
        let now_instant = Instant::now();
        let delta = micros - now_micros;
        if delta >= 0 {
            Some(now_instant + Duration::from_micros(delta as u64))
        } else {
            now_instant.checked_sub(Duration::from_micros((-delta) as u64))
        }
    }

    /// Convert a [`std::time::Instant`] to this clock's microseconds.
    ///
    /// Inverse of [`micros_to_instant`](Clock::micros_to_instant). Used during
    /// re-anchoring to map the audio callback's playback instant back to the
    /// clock domain for server-time conversion.
    ///
    /// The bridge error is typically under 10µs (see
    /// [`micros_to_instant`](Clock::micros_to_instant) for details).
    /// Sub-microsecond truncation from `Duration::as_micros()` introduces
    /// a consistent -0 to -1µs bias, which the Kalman filter absorbs trivially.
    fn instant_to_micros(&self, instant: Instant) -> i64 {
        let now_micros = self.now_micros();
        let now_instant = Instant::now();
        let delta = if instant >= now_instant {
            instant.duration_since(now_instant).as_micros() as i64
        } else {
            -(now_instant.duration_since(instant).as_micros() as i64)
        };
        now_micros + delta
    }
}

/// Default clock implementation using the best available raw monotonic source.
///
/// | Platform | Source | NTP-immune? |
/// |----------|--------|-------------|
/// | Linux | `CLOCK_MONOTONIC_RAW` | Yes |
/// | macOS | `mach_absolute_time` (via `Instant`) | Yes |
/// | Windows | `QueryPerformanceCounter` (via `Instant`) | Yes |
pub struct DefaultClock {
    /// `Instant` captured at construction, used as the epoch for non-Linux
    /// platforms where we derive microseconds from `Instant` arithmetic.
    /// On Linux this field is unused (we read `CLOCK_MONOTONIC_RAW` directly),
    /// but the cost of a single `Instant` isn't worth a `cfg` attr.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    epoch: Instant,
}

impl DefaultClock {
    /// Create a new default clock.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for DefaultClock {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DefaultClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultClock").finish()
    }
}

impl Clock for DefaultClock {
    #[cfg(target_os = "linux")]
    fn now_micros(&self) -> i64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes to the provided pointer and
        // `CLOCK_MONOTONIC_RAW` is always available on Linux ≥ 2.6.28.
        // We pass a valid, aligned, stack-allocated `timespec`.
        let ret = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        if ret != 0 {
            // CLOCK_MONOTONIC_RAW can fail in heavily restricted containers
            // (e.g. seccomp filters) or on very old kernels. Fall back to
            // CLOCK_MONOTONIC which is NTP-conditioned but still monotonic.
            log::error!(
                "CLOCK_MONOTONIC_RAW unavailable (errno {}), falling back to CLOCK_MONOTONIC",
                std::io::Error::last_os_error()
            );
            // SAFETY: same pointer validity as above; CLOCK_MONOTONIC is
            // universally available on all Linux kernels.
            let ret2 = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            if ret2 != 0 {
                // Both clocks failed — this is essentially impossible on any
                // real Linux system, but if it happens, ts is still zeroed from
                // initialization. Log and accept 0 as the degenerate case.
                log::error!(
                    "CLOCK_MONOTONIC also failed (errno {}); returning epoch (0)",
                    std::io::Error::last_os_error()
                );
            }
        }
        // This cast is necessary to make builds on musl targets work
        #[allow(clippy::unnecessary_cast)]
        let time = (ts.tv_sec as i64) * 1_000_000 + (ts.tv_nsec as i64) / 1_000;
        time
    }

    #[cfg(not(target_os = "linux"))]
    fn now_micros(&self) -> i64 {
        // On macOS and Windows, Instant is already a raw/unconditioned source.
        // The cast from u128 to i64 is safe: i64::MAX microseconds is ~292,277
        // years of uptime. Use try_from to be explicit rather than silently
        // truncating.
        i64::try_from(Instant::now().duration_since(self.epoch).as_micros()).unwrap_or(i64::MAX)
    }
}
