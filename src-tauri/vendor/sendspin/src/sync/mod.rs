// ABOUTME: Clock synchronization for Sendspin protocol
// ABOUTME: NTP-style round-trip time calculation and server timestamp conversion

/// Clock synchronization implementation
pub mod clock;
/// Raw monotonic clock trait and platform implementations
pub mod raw_clock;

pub use clock::{ClockSync, SyncQuality};
pub use raw_clock::{Clock, DefaultClock};
