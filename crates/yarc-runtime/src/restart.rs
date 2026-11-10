//! Restart policies: what to do when a supervised container exits. Pure logic (no
//! processes, no clocks), so the whole decision table can be unit tested.
//!
//! See `docs/progress/C4-restart-design.md`.

use std::time::Duration;

use yarc_config::RestartPolicy;

/// Delay between restarts: `initial`, doubling for every quick failure in a row, never more
/// than `max`; a run that lasted `reset_after` or longer starts over at `initial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::with_initial(Duration::from_secs(1))
    }
}

impl Backoff {
    /// The standard shape (cap 60x and reset 10x the initial delay), scaled; the defaults are
    /// 1 s / 60 s / 10 s. Tests use a tiny `initial` to make restarts fast.
    pub fn with_initial(initial: Duration) -> Self {
        Backoff { initial, max: initial * 60, reset_after: initial * 10 }
    }

    /// The delay after `streak` quick failures in a row.
    pub fn delay(&self, streak: u32) -> Duration {
        let factor = 1u32.checked_shl(streak.min(31)).unwrap_or(u32::MAX);
        self.initial.saturating_mul(factor).min(self.max)
    }
}

/// How one run of the container ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunEnd {
    /// Exit status (128 + signal when it was killed).
    pub exit_code: i32,
    pub ran_for: Duration,
    /// A person asked for it to stop (`stop`, `down`, `rm -f`, `kill`): see the design note.
    pub stop_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Stop,
    Restart { after: Duration },
}

/// Follows one container's runs and decides, after each, whether to run it again.
#[derive(Debug, Clone)]
pub struct RestartTracker {
    policy: RestartPolicy,
    backoff: Backoff,
    restarts: u32,
    streak: u32,
}

impl RestartTracker {
    pub fn new(policy: RestartPolicy, backoff: Backoff) -> Self {
        RestartTracker { policy, backoff, restarts: 0, streak: 0 }
    }

    /// How many times the container has been restarted so far.
    pub fn restarts(&self) -> u32 {
        self.restarts
    }

    /// Call when a run ends. A `Restart` decision is counted: the caller is expected to wait
    /// `after` and run again (unless a stop is requested meanwhile).
    pub fn on_exit(&mut self, end: RunEnd) -> Decision {
        if end.stop_requested {
            return Decision::Stop;
        }
        let wanted = match self.policy {
            RestartPolicy::No => false,
            RestartPolicy::Always | RestartPolicy::UnlessStopped => true,
            RestartPolicy::OnFailure { max } => end.exit_code != 0 && max.is_none_or(|m| self.restarts < m),
        };
        if !wanted {
            return Decision::Stop;
        }
        // a long run proves the container works: forget the earlier failures
        let streak = if end.ran_for >= self.backoff.reset_after { 0 } else { self.streak };
        let after = self.backoff.delay(streak);
        self.streak = streak.saturating_add(1);
        self.restarts = self.restarts.saturating_add(1);
        Decision::Restart { after }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn end(exit_code: i32, ran_for: Duration) -> RunEnd {
        RunEnd { exit_code, ran_for, stop_requested: false }
    }

    fn tracker(policy: &str) -> RestartTracker {
        RestartTracker::new(policy.parse().unwrap(), Backoff::default())
    }

    #[test]
    fn no_never_restarts() {
        let mut t = tracker("no");
        assert_eq!(t.on_exit(end(1, secs(1))), Decision::Stop);
        assert_eq!(t.on_exit(end(0, secs(1))), Decision::Stop);
        assert_eq!(t.restarts(), 0);
    }

    #[test]
    fn always_and_unless_stopped_restart_on_any_exit() {
        for policy in ["always", "unless-stopped"] {
            let mut t = tracker(policy);
            assert!(matches!(t.on_exit(end(0, secs(1))), Decision::Restart { .. }), "{policy} exit 0");
            assert!(matches!(t.on_exit(end(1, secs(1))), Decision::Restart { .. }), "{policy} exit 1");
            assert!(matches!(t.on_exit(end(137, secs(1))), Decision::Restart { .. }), "{policy} killed");
            assert_eq!(t.restarts(), 3);
        }
    }

    #[test]
    fn on_failure_restarts_only_after_a_non_zero_exit() {
        let mut t = tracker("on-failure");
        assert_eq!(t.on_exit(end(0, secs(1))), Decision::Stop);
        assert_eq!(t.restarts(), 0);
        assert!(matches!(t.on_exit(end(1, secs(1))), Decision::Restart { .. }));
        assert!(matches!(t.on_exit(end(137, secs(1))), Decision::Restart { .. }));
        // a clean exit ends it even after earlier failures
        assert_eq!(t.on_exit(end(0, secs(1))), Decision::Stop);
        assert_eq!(t.restarts(), 2);
    }

    #[test]
    fn on_failure_n_allows_exactly_n_restarts() {
        let mut t = tracker("on-failure:2");
        assert!(matches!(t.on_exit(end(1, secs(1))), Decision::Restart { .. }));
        assert!(matches!(t.on_exit(end(1, secs(1))), Decision::Restart { .. }));
        assert_eq!(t.on_exit(end(1, secs(1))), Decision::Stop); // the third run is the last
        assert_eq!(t.restarts(), 2);
        // long healthy runs do not give extra attempts: the count is total, not consecutive
        let mut t = tracker("on-failure:1");
        assert!(matches!(t.on_exit(end(1, secs(3600))), Decision::Restart { .. }));
        assert_eq!(t.on_exit(end(1, secs(3600))), Decision::Stop);
    }

    #[test]
    fn a_stop_request_beats_every_policy() {
        for policy in ["no", "always", "unless-stopped", "on-failure", "on-failure:5"] {
            let mut t = tracker(policy);
            for code in [0, 1, 137] {
                let stopped = RunEnd { exit_code: code, ran_for: secs(1), stop_requested: true };
                assert_eq!(t.on_exit(stopped), Decision::Stop, "{policy} {code}");
            }
            assert_eq!(t.restarts(), 0, "{policy}");
        }
    }

    #[test]
    fn quick_failures_back_off_1_2_4_up_to_60_seconds() {
        let mut t = tracker("always");
        let mut delays = Vec::new();
        for _ in 0..10 {
            let Decision::Restart { after } = t.on_exit(end(1, Duration::from_millis(50))) else { panic!() };
            delays.push(after.as_secs());
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn a_run_of_ten_seconds_resets_the_delay() {
        let mut t = tracker("always");
        for _ in 0..4 {
            t.on_exit(end(1, secs(1)));
        }
        // the next quick failure would wait 16 s; a 10 s run starts over at 1 s
        assert_eq!(t.on_exit(end(1, secs(10))), Decision::Restart { after: secs(1) });
        assert_eq!(t.on_exit(end(1, secs(1))), Decision::Restart { after: secs(2) });
        // 9.999 s is still "quick"
        let mut t = tracker("always");
        t.on_exit(end(1, secs(1)));
        assert_eq!(t.on_exit(end(1, Duration::from_millis(9999))), Decision::Restart { after: secs(2) });
    }

    #[test]
    fn a_clean_exit_under_always_also_waits() {
        // an `exit 0` loop must not spin either
        let mut t = tracker("always");
        assert_eq!(t.on_exit(end(0, secs(0))), Decision::Restart { after: secs(1) });
        assert_eq!(t.on_exit(end(0, secs(0))), Decision::Restart { after: secs(2) });
    }

    #[test]
    fn backoff_scales_for_tests_and_never_overflows() {
        let b = Backoff::with_initial(Duration::from_millis(20));
        assert_eq!(
            (b.initial, b.max, b.reset_after),
            (Duration::from_millis(20), Duration::from_millis(1200), Duration::from_millis(200))
        );
        assert_eq!(b.delay(0), Duration::from_millis(20));
        assert_eq!(b.delay(3), Duration::from_millis(160));
        assert_eq!(b.delay(u32::MAX), b.max);
        assert_eq!(Backoff::default().delay(100), secs(60));
        let mut t = RestartTracker::new(RestartPolicy::Always, b);
        t.streak = u32::MAX;
        t.restarts = u32::MAX;
        assert_eq!(t.on_exit(end(1, Duration::ZERO)), Decision::Restart { after: b.max });
        assert_eq!((t.streak, t.restarts), (u32::MAX, u32::MAX));
    }
}
