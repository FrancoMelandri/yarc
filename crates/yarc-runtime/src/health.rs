//! The health state machine, free of processes and clocks: the supervisor tells
//! it what time it is and how checks ended.
//!
//! - the first check runs shortly after the container starts, later ones every
//!   `interval` after the previous one *finished* (checks never overlap);
//! - a success makes the container `healthy` (and resets the failure streak);
//! - a failure (non-zero exit, could not run, or timed out) extends the streak,
//!   and `retries` failures in a row make it `unhealthy` until a check succeeds.

use crate::spec::HealthSpec;
use crate::state::{Health, HealthStatus};

/// Delay before the first check, so `needs: healthy` does not wait a whole
/// interval for it.
pub const FIRST_CHECK_DELAY_MS: u64 = 1_000;

#[derive(Debug)]
pub struct HealthTracker {
    spec: HealthSpec,
    health: Health,
    next_due_ms: u64,
    in_flight_since: Option<u64>,
}

impl HealthTracker {
    pub fn new(spec: HealthSpec, now_ms: u64) -> Self {
        HealthTracker {
            spec,
            health: Health { status: HealthStatus::Starting, failing_streak: 0, last_exit: None, last_check_ms: None },
            next_due_ms: now_ms + FIRST_CHECK_DELAY_MS,
            in_flight_since: None,
        }
    }

    pub fn spec(&self) -> &HealthSpec {
        &self.spec
    }

    pub fn health(&self) -> &Health {
        &self.health
    }

    /// Time to start a check?
    pub fn due(&self, now_ms: u64) -> bool {
        self.in_flight_since.is_none() && now_ms >= self.next_due_ms
    }

    pub fn started_check(&mut self, now_ms: u64) {
        self.in_flight_since = Some(now_ms);
    }

    /// Has the running check exceeded its timeout?
    pub fn timed_out(&self, now_ms: u64) -> bool {
        self.in_flight_since.is_some_and(|since| now_ms.saturating_sub(since) >= self.spec.timeout_ms)
    }

    /// Records the end of a check: `Some(code)` is its exit status, `None` means
    /// it timed out or could not be run. Returns whether the status or the
    /// failure streak changed (i.e. whether it is worth persisting).
    pub fn finished(&mut self, now_ms: u64, exit: Option<i32>) -> bool {
        self.in_flight_since = None;
        self.next_due_ms = now_ms + self.spec.interval_ms;
        let before = (self.health.status, self.health.failing_streak);
        self.health.last_exit = exit;
        self.health.last_check_ms = Some(now_ms);
        match exit {
            Some(0) => {
                self.health.failing_streak = 0;
                self.health.status = HealthStatus::Healthy;
            }
            _ => {
                self.health.failing_streak += 1;
                if self.health.failing_streak >= self.spec.retries {
                    self.health.status = HealthStatus::Unhealthy;
                }
            }
        }
        before != (self.health.status, self.health.failing_streak)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(interval_ms: u64, timeout_ms: u64, retries: u32) -> HealthSpec {
        HealthSpec { cmd: vec!["true".into()], interval_ms, timeout_ms, retries }
    }

    fn tracker() -> HealthTracker {
        HealthTracker::new(spec(5_000, 2_000, 3), 10_000)
    }

    #[test]
    fn starts_in_starting_with_the_first_check_after_a_short_delay() {
        let t = tracker();
        assert_eq!(t.health().status, HealthStatus::Starting);
        assert!(!t.due(10_000));
        assert!(!t.due(10_000 + FIRST_CHECK_DELAY_MS - 1));
        assert!(t.due(10_000 + FIRST_CHECK_DELAY_MS));
    }

    #[test]
    fn a_success_makes_it_healthy_and_the_next_check_follows_the_interval() {
        let mut t = tracker();
        t.started_check(11_000);
        assert!(!t.due(99_999), "no overlapping checks");
        assert!(t.finished(11_300, Some(0)));
        let h = t.health();
        assert_eq!((h.status, h.failing_streak, h.last_exit, h.last_check_ms), (HealthStatus::Healthy, 0, Some(0), Some(11_300)));
        assert!(!t.due(16_299));
        assert!(t.due(16_300));
    }

    #[test]
    fn failures_accumulate_until_unhealthy_and_a_success_recovers() {
        let mut t = tracker();
        assert!(t.finished(1, Some(1)), "streak 1");
        assert_eq!(t.health().status, HealthStatus::Starting, "still starting after one failure");
        assert!(t.finished(2, Some(1)), "streak 2");
        assert_eq!(t.health().status, HealthStatus::Starting);
        assert!(t.finished(3, Some(2)), "streak 3 → unhealthy");
        assert_eq!((t.health().status, t.health().failing_streak, t.health().last_exit), (HealthStatus::Unhealthy, 3, Some(2)));
        assert!(t.finished(4, Some(1)), "the streak keeps growing");
        assert_eq!(t.health().status, HealthStatus::Unhealthy);
        assert!(t.finished(5, Some(0)));
        assert_eq!((t.health().status, t.health().failing_streak), (HealthStatus::Healthy, 0));
    }

    #[test]
    fn a_failure_after_healthy_needs_all_retries_to_flip() {
        let mut t = tracker();
        t.finished(1, Some(0));
        t.finished(2, Some(1));
        t.finished(3, Some(1));
        assert_eq!(t.health().status, HealthStatus::Healthy);
        t.finished(4, Some(1));
        assert_eq!(t.health().status, HealthStatus::Unhealthy);
    }

    #[test]
    fn nothing_to_persist_when_nothing_changed() {
        let mut t = tracker();
        t.finished(1, Some(0));
        assert!(!t.finished(2, Some(0)), "healthy stays healthy");
        assert_eq!(t.health().last_check_ms, Some(2), "but the details are still updated");
    }

    #[test]
    fn timeouts_and_unrunnable_checks_count_as_failures() {
        let mut t = HealthTracker::new(spec(1_000, 2_000, 2), 0);
        t.started_check(5_000);
        assert!(!t.timed_out(6_999));
        assert!(t.timed_out(7_000));
        assert!(t.finished(7_000, None));
        assert_eq!((t.health().failing_streak, t.health().last_exit), (1, None));
        assert!(!t.timed_out(99_999), "nothing in flight");
        assert!(t.finished(8_000, None));
        assert_eq!(t.health().status, HealthStatus::Unhealthy);
    }

    #[test]
    fn retries_of_one_flips_on_the_first_failure() {
        let mut t = HealthTracker::new(spec(1_000, 1_000, 1), 0);
        t.finished(1, Some(1));
        assert_eq!(t.health().status, HealthStatus::Unhealthy);
    }
}
