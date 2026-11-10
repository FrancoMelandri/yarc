//! Run state of a container, persisted as `state.json` next to its config.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// No check has succeeded yet.
    Starting,
    Healthy,
    Unhealthy,
}

impl HealthStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HealthStatus::Starting => "starting",
            HealthStatus::Healthy => "healthy",
            HealthStatus::Unhealthy => "unhealthy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: HealthStatus,
    /// Consecutive failed checks.
    pub failing_streak: u32,
    pub last_exit: Option<i32>,
    pub last_check_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerState {
    pub status: Status,
    /// The yarc (or, later, shim) process supervising the container. If this
    /// is gone while the status is still `running`, the container is `dead`.
    pub supervisor_pid: Option<u32>,
    /// Host pid of the container's init.
    pub pid: Option<u32>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i32>,
    /// Only for containers with a health check, and only while they run.
    #[serde(default)]
    pub health: Option<Health>,
    /// How many times the supervisor has restarted the container since a person started it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub restarts: u32,
    /// Set while the supervisor waits to restart the container (status `running`, no init
    /// pid; `exit_code` / `finished_ms` describe the run that just ended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_ms: Option<u64>,
    /// The run that ended was killed by the kernel for exceeding its memory limit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub oom_killed: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl ContainerState {
    pub fn running(supervisor_pid: u32, pid: Option<u32>, started_ms: u64) -> Self {
        ContainerState {
            status: Status::Running,
            supervisor_pid: Some(supervisor_pid),
            pid,
            started_ms,
            finished_ms: None,
            exit_code: None,
            health: None,
            restarts: 0,
            next_restart_ms: None,
            oom_killed: false,
        }
    }

    /// The run ended because the kernel killed it for exceeding its memory limit.
    pub fn with_oom_killed(&self, oom_killed: bool) -> Self {
        ContainerState { oom_killed, ..self.clone() }
    }

    /// The same state with the restart count carried over from the earlier runs.
    pub fn with_restarts(&self, restarts: u32) -> Self {
        ContainerState { restarts, ..self.clone() }
    }

    /// The run ended with `exit_code` and the supervisor will start it again at
    /// `next_restart_ms`: still supervised, but no process is running.
    pub fn restarting(&self, exit_code: i32, finished_ms: u64, next_restart_ms: u64) -> Self {
        ContainerState {
            status: Status::Running,
            pid: None,
            exit_code: Some(exit_code),
            finished_ms: Some(finished_ms),
            health: None,
            next_restart_ms: Some(next_restart_ms),
            ..self.clone()
        }
    }

    pub fn with_health(&self, health: Option<Health>) -> Self {
        ContainerState { health, ..self.clone() }
    }

    pub fn exited(&self, exit_code: i32, finished_ms: u64) -> Self {
        ContainerState {
            status: Status::Exited,
            exit_code: Some(exit_code),
            finished_ms: Some(finished_ms),
            health: None,
            next_restart_ms: None,
            ..self.clone()
        }
    }

    /// True when the state says running and a supervisor still holds the
    /// container's lock (see `ContainerStore::is_supervised`).
    pub fn is_running(&self, supervised: bool) -> bool {
        self.status == Status::Running && supervised
    }
}

/// One-word-ish status for listings: `Created`, `Running`, `Exited (7)`,
/// `Dead` (running on record, but nobody supervises it any more); with details when
/// there are any: `Running (healthy, restarted 2 times)`, `Restarting (exit 1, in 4s)`.
pub fn describe(state: Option<&ContainerState>, supervised: bool) -> String {
    describe_at(state, supervised, now_ms())
}

/// [`describe`] with the current time given, for tests.
pub fn describe_at(state: Option<&ContainerState>, supervised: bool, now_ms: u64) -> String {
    match state {
        None => "Created".into(),
        Some(s) if s.status == Status::Exited => {
            format!("Exited ({}){}", s.exit_code.unwrap_or(-1), if s.oom_killed { " OOM killed" } else { "" })
        }
        Some(s) if s.is_running(supervised) => {
            if let Some(next) = s.next_restart_ms {
                let mut exit = s.exit_code.map_or(String::new(), |c| format!("exit {c}"));
                if s.oom_killed {
                    exit.push_str(", OOM killed");
                }
                return match next.saturating_sub(now_ms) {
                    0 => format!("Restarting ({exit})"),
                    ms => format!("Restarting ({exit}, in {}s)", ms.div_ceil(1000)),
                };
            }
            let mut details = Vec::new();
            if let Some(h) = &s.health {
                details.push(h.status.as_str().to_string());
            }
            match s.restarts {
                0 => {}
                1 => details.push("restarted 1 time".into()),
                n => details.push(format!("restarted {n} times")),
            }
            if details.is_empty() {
                "Running".into()
            } else {
                format!("Running ({})", details.join(", "))
            }
        }
        Some(_) => "Dead".into(),
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_then_exited() {
        let r = ContainerState::running(10, Some(11), 1000);
        assert_eq!((r.status, r.exit_code, r.finished_ms), (Status::Running, None, None));
        let x = r.exited(7, 2000);
        assert_eq!((x.status, x.exit_code, x.finished_ms), (Status::Exited, Some(7), Some(2000)));
        assert_eq!((x.supervisor_pid, x.pid, x.started_ms), (Some(10), Some(11), 1000));
    }

    #[test]
    fn descriptions() {
        let run = ContainerState::running(10, Some(11), 0);
        assert_eq!(describe(None, true), "Created");
        assert_eq!(describe(Some(&run), true), "Running");
        assert_eq!(describe(Some(&run), false), "Dead");
        assert_eq!(describe(Some(&run.exited(3, 1)), false), "Exited (3)");
        assert_eq!(describe(Some(&run.exited(3, 1)), true), "Exited (3)");
    }

    #[test]
    fn an_oom_kill_is_shown_and_only_recorded_when_it_happened() {
        let run = ContainerState::running(10, Some(11), 0);
        assert!(!serde_json::to_string(&run.exited(1, 1)).unwrap().contains("oom"));
        let killed = run.with_oom_killed(true).exited(137, 1);
        assert_eq!(describe(Some(&killed), false), "Exited (137) OOM killed");
        assert!(serde_json::to_string(&killed).unwrap().contains("\"oom_killed\":true"));
        let waiting = run.with_oom_killed(true).restarting(137, 1_000, 5_000);
        assert_eq!(describe_at(Some(&waiting), true, 1_000), "Restarting (exit 137, OOM killed, in 4s)");
        // a state written before limits existed still loads
        let old = r#"{"status":"exited","supervisor_pid":1,"pid":2,"started_ms":0,"finished_ms":1,"exit_code":3}"#;
        assert!(!serde_json::from_str::<ContainerState>(old).unwrap().oom_killed);
    }

    #[test]
    fn health_is_shown_while_running_and_dropped_on_exit() {
        let health =
            Health { status: HealthStatus::Healthy, failing_streak: 0, last_exit: Some(0), last_check_ms: Some(5) };
        let run = ContainerState::running(10, Some(11), 0).with_health(Some(health));
        assert_eq!(describe(Some(&run), true), "Running (healthy)");
        assert_eq!(describe(Some(&run), false), "Dead");
        let starting = run.with_health(Some(Health {
            status: HealthStatus::Starting,
            failing_streak: 0,
            last_exit: None,
            last_check_ms: None,
        }));
        assert_eq!(describe(Some(&starting), true), "Running (starting)");
        let unhealthy = run.with_health(Some(Health {
            status: HealthStatus::Unhealthy,
            failing_streak: 3,
            last_exit: Some(1),
            last_check_ms: Some(9),
        }));
        assert_eq!(describe(Some(&unhealthy), true), "Running (unhealthy)");
        let done = unhealthy.exited(0, 20);
        assert_eq!(done.health, None);
        assert_eq!(describe(Some(&done), false), "Exited (0)");
    }

    #[test]
    fn old_state_files_without_health_still_load() {
        let json =
            r#"{"status":"running","supervisor_pid":1,"pid":2,"started_ms":3,"finished_ms":null,"exit_code":null}"#;
        let s: ContainerState = serde_json::from_str(json).unwrap();
        assert_eq!(s.health, None);
    }

    #[test]
    fn restart_details_in_descriptions() {
        let run = ContainerState::running(10, Some(11), 0);
        assert_eq!(describe_at(Some(&run.with_restarts(1)), true, 0), "Running (restarted 1 time)");
        assert_eq!(describe_at(Some(&run.with_restarts(3)), true, 0), "Running (restarted 3 times)");
        let health =
            Health { status: HealthStatus::Healthy, failing_streak: 0, last_exit: Some(0), last_check_ms: Some(5) };
        let both = run.with_restarts(2).with_health(Some(health));
        assert_eq!(describe_at(Some(&both), true, 0), "Running (healthy, restarted 2 times)");
        assert_eq!(describe_at(Some(&both), false, 0), "Dead");

        let waiting = run.with_restarts(2).restarting(1, 1_000, 5_500);
        assert_eq!(waiting.status, Status::Running);
        assert_eq!((waiting.pid, waiting.exit_code, waiting.restarts), (None, Some(1), 2));
        assert_eq!(describe_at(Some(&waiting), true, 1_000), "Restarting (exit 1, in 5s)"); // 4.5 s rounds up
        assert_eq!(describe_at(Some(&waiting), true, 4_500), "Restarting (exit 1, in 1s)");
        assert_eq!(describe_at(Some(&waiting), true, 5_500), "Restarting (exit 1)");
        assert_eq!(describe_at(Some(&waiting), true, 9_000), "Restarting (exit 1)");
        assert_eq!(describe_at(Some(&waiting), false, 1_000), "Dead");

        // a stop request ends the wait: the state becomes a plain exit, still counting restarts
        let done = waiting.exited(1, 2_000);
        assert_eq!((done.next_restart_ms, done.restarts), (None, 2));
        assert_eq!(describe_at(Some(&done), false, 2_000), "Exited (1)");
    }

    #[test]
    fn restart_fields_are_optional_in_old_files_and_absent_when_unused() {
        let json =
            r#"{"status":"running","supervisor_pid":1,"pid":2,"started_ms":3,"finished_ms":null,"exit_code":null}"#;
        let s: ContainerState = serde_json::from_str(json).unwrap();
        assert_eq!((s.restarts, s.next_restart_ms), (0, None));
        // files of containers that never restarted keep the old shape
        let out = serde_json::to_string(&ContainerState::running(1, None, 5)).unwrap();
        assert!(!out.contains("restarts") && !out.contains("next_restart_ms"), "{out}");
        let out =
            serde_json::to_string(&ContainerState::running(1, None, 5).with_restarts(2).restarting(1, 6, 9)).unwrap();
        assert!(out.contains("\"restarts\":2") && out.contains("\"next_restart_ms\":9"), "{out}");
    }

    #[test]
    fn json_shape_is_stable() {
        let s = ContainerState::running(1, None, 5).exited(0, 6);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"status\":\"exited\""), "{json}");
        assert_eq!(serde_json::from_str::<ContainerState>(&json).unwrap(), s);
    }
}
