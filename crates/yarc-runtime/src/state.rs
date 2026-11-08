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
/// `Dead` (running on record, but nobody supervises it any more).
pub fn describe(state: Option<&ContainerState>, supervised: bool) -> String {
    match state {
        None => "Created".into(),
        Some(s) if s.status == Status::Exited => format!("Exited ({})", s.exit_code.unwrap_or(-1)),
        Some(s) if s.is_running(supervised) => match &s.health {
            Some(h) => format!("Running ({})", h.status.as_str()),
            None => "Running".into(),
        },
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
    fn health_is_shown_while_running_and_dropped_on_exit() {
        let health = Health { status: HealthStatus::Healthy, failing_streak: 0, last_exit: Some(0), last_check_ms: Some(5) };
        let run = ContainerState::running(10, Some(11), 0).with_health(Some(health));
        assert_eq!(describe(Some(&run), true), "Running (healthy)");
        assert_eq!(describe(Some(&run), false), "Dead");
        let starting = run.with_health(Some(Health { status: HealthStatus::Starting, failing_streak: 0, last_exit: None, last_check_ms: None }));
        assert_eq!(describe(Some(&starting), true), "Running (starting)");
        let unhealthy = run.with_health(Some(Health { status: HealthStatus::Unhealthy, failing_streak: 3, last_exit: Some(1), last_check_ms: Some(9) }));
        assert_eq!(describe(Some(&unhealthy), true), "Running (unhealthy)");
        let done = unhealthy.exited(0, 20);
        assert_eq!(done.health, None);
        assert_eq!(describe(Some(&done), false), "Exited (0)");
    }

    #[test]
    fn old_state_files_without_health_still_load() {
        let json = r#"{"status":"running","supervisor_pid":1,"pid":2,"started_ms":3,"finished_ms":null,"exit_code":null}"#;
        let s: ContainerState = serde_json::from_str(json).unwrap();
        assert_eq!(s.health, None);
    }

    #[test]
    fn json_shape_is_stable() {
        let s = ContainerState::running(1, None, 5).exited(0, 6);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"status\":\"exited\""), "{json}");
        assert_eq!(serde_json::from_str::<ContainerState>(&json).unwrap(), s);
    }
}
