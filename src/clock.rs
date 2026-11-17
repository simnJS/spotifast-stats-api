//! Time source: the system clock, or a simulated one in tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// "Now", in unix milliseconds (UTC).
///
/// The system clock by default. A simulated clock only moves when told to, and
/// its clones share it, so a test can move the time of a running app.
#[derive(Clone, Debug, Default)]
pub struct Clock {
    simulated: Option<Arc<AtomicI64>>,
}

impl Clock {
    /// The system clock.
    pub fn system() -> Self {
        Self::default()
    }

    /// A clock stopped at `start_ms` until [`Clock::set_ms`] or
    /// [`Clock::advance_ms`] moves it.
    pub fn simulated(start_ms: i64) -> Self {
        Self {
            simulated: Some(Arc::new(AtomicI64::new(start_ms))),
        }
    }

    pub fn now_ms(&self) -> i64 {
        match &self.simulated {
            Some(t) => t.load(Ordering::SeqCst),
            None => chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Set a simulated clock. Panics on the system clock.
    pub fn set_ms(&self, ms: i64) {
        self.simulated
            .as_ref()
            .expect("only a simulated clock can be set")
            .store(ms, Ordering::SeqCst);
    }

    /// Move a simulated clock forward. Panics on the system clock.
    pub fn advance_ms(&self, ms: i64) {
        self.simulated
            .as_ref()
            .expect("only a simulated clock can be moved")
            .fetch_add(ms, Ordering::SeqCst);
    }
}

/// Unix ms as RFC 3339 (UTC, whole seconds), for logs and messages.
pub fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms} ms"),
        |t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_clock_moves_only_when_told_and_is_shared_by_clones() {
        let clock = Clock::simulated(1_000);
        let clone = clock.clone();
        assert_eq!(clock.now_ms(), 1_000);
        clone.advance_ms(500);
        assert_eq!(clock.now_ms(), 1_500);
        clock.set_ms(42);
        assert_eq!(clone.now_ms(), 42);
    }

    #[test]
    fn times_are_formatted_as_rfc3339() {
        assert_eq!(fmt_ms(1_800_000_000_000), "2027-01-15T08:00:00Z");
    }

    #[test]
    fn system_clock_follows_the_real_time() {
        let before = chrono::Utc::now().timestamp_millis();
        let now = Clock::system().now_ms();
        assert!(now >= before && now - before < 60_000);
    }
}
