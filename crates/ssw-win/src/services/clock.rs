//! A monotonic clock on `QueryPerformanceCounter`.

use std::time::Duration;

use ssw_ui::services::Clock;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// The performance counter, measuring from its creation.
#[derive(Debug, Clone, Copy)]
pub struct QpcClock {
    frequency: i64,
    origin: i64,
}

fn counter() -> i64 {
    let mut value = 0i64;
    // SAFETY: valid out pointer; the call cannot fail on Windows XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut value);
    }
    value
}

fn frequency() -> i64 {
    let mut value = 0i64;
    // SAFETY: valid out pointer.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut value);
    }
    value.max(1)
}

/// `ticks` at `frequency` Hz as a [`Duration`] (no overflow for any `i64` tick count).
#[must_use]
pub fn ticks_to_duration(ticks: i64, frequency: i64) -> Duration {
    let ticks = u128::try_from(ticks.max(0)).unwrap_or(0);
    let nanos = ticks * 1_000_000_000 / u128::try_from(frequency.max(1)).unwrap_or(1);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

impl QpcClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            frequency: frequency(),
            origin: counter(),
        }
    }
}

impl Default for QpcClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for QpcClock {
    fn now(&self) -> Duration {
        ticks_to_duration(counter() - self.origin, self.frequency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn tick_conversion_is_exact_and_saturating() {
        assert_eq!(
            ticks_to_duration(10_000_000, 10_000_000),
            Duration::from_secs(1)
        );
        assert_eq!(
            ticks_to_duration(5_000_000, 10_000_000),
            Duration::from_millis(500)
        );
        assert_eq!(ticks_to_duration(-5, 10), Duration::ZERO);
        assert_eq!(ticks_to_duration(1, 0), Duration::from_nanos(1_000_000_000));
        assert_eq!(
            ticks_to_duration(i64::MAX, 1),
            Duration::from_nanos(u64::MAX)
        );
    }

    #[test]
    fn the_clock_is_monotonic_and_tracks_wall_time() {
        let clock = QpcClock::default();
        let wall = Instant::now();
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(30));
        let second = clock.now();
        assert!(second > first);
        assert!(second - first >= Duration::from_millis(25));
        assert!(second - first < wall.elapsed() + Duration::from_millis(50));
        assert!(QpcClock::new().now() < Duration::from_secs(1));
    }
}
