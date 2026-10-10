//! Time for the application core: a monotonic clock for timers and the wall clock for stored
//! timestamps, behind one trait so tests move both by hand.

use std::cell::Cell;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ssw_io::store::UnixSeconds;

/// Monotonic and wall-clock time.
pub trait AppClock: std::fmt::Debug {
    /// Monotonic time since an arbitrary origin; never decreases.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time as Unix seconds (history timestamps).
    fn unix_seconds(&self) -> UnixSeconds;
}

/// The real clocks.
#[derive(Clone, Copy, Debug)]
pub struct SystemAppClock {
    origin: Instant,
}

impl SystemAppClock {
    /// A clock whose monotonic origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemAppClock {
    fn default() -> Self {
        Self::new()
    }
}

impl AppClock for SystemAppClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn unix_seconds(&self) -> UnixSeconds {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
    }
}

/// A clock that only moves when told to: both clocks advance together.
#[derive(Debug)]
pub struct ManualClock {
    monotonic: Cell<Duration>,
    unix: Cell<UnixSeconds>,
}

impl ManualClock {
    /// A clock at monotonic time zero and wall-clock time `unix`.
    #[must_use]
    pub fn new(unix: UnixSeconds) -> Self {
        Self {
            monotonic: Cell::new(Duration::ZERO),
            unix: Cell::new(unix),
        }
    }

    /// Moves both clocks forward by `by` (whole seconds for the wall clock).
    pub fn advance(&self, by: Duration) {
        self.monotonic.set(self.monotonic.get() + by);
        self.unix
            .set(self.unix.get() + i64::try_from(by.as_secs()).unwrap_or(i64::MAX));
    }
}

impl AppClock for ManualClock {
    fn monotonic(&self) -> Duration {
        self.monotonic.get()
    }

    fn unix_seconds(&self) -> UnixSeconds {
        self.unix.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_clock_moves_forward_and_reports_the_current_date() {
        let clock = SystemAppClock::default();
        let first = clock.monotonic();
        std::thread::sleep(Duration::from_millis(5));
        assert!(clock.monotonic() > first);
        // After 2024-01-01.
        assert!(clock.unix_seconds() > 1_704_067_200);
        assert!(SystemAppClock::new().monotonic() < Duration::from_secs(60));
    }

    #[test]
    fn the_manual_clock_advances_both_clocks_together() {
        let clock = ManualClock::new(1_000);
        assert_eq!(clock.monotonic(), Duration::ZERO);
        assert_eq!(clock.unix_seconds(), 1_000);
        clock.advance(Duration::from_millis(2_500));
        assert_eq!(clock.monotonic(), Duration::from_millis(2_500));
        assert_eq!(clock.unix_seconds(), 1_002);
    }
}
