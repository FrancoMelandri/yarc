//! Enforcing CPU and memory limits.
//!
//! A rootless process cannot write cgroup files from where yarc runs (the `wsl.exe` session's
//! cgroup is not delegated), but it can ask the user's systemd for a transient scope with the
//! limits and run inside it. A container **with** limits therefore has its supervisor (the
//! shim, or the foreground `container start`) started as `systemd-run --user --scope … yarc …`;
//! everything the supervisor forks stays in that scope. A container without limits is started
//! exactly as before.
//!
//! See `docs/progress/L-limits-plan.md`.

use std::path::Path;
use std::process::Command;

use yarc_config::Resources;

use crate::error::{Result, RuntimeError};

/// Set in the environment of a process that is already inside its scope, so it is never
/// wrapped twice.
pub const SCOPE_MARKER: &str = "YARC_LIMITS_SCOPE";

/// What the container's init gets added to its `oom_score_adj`: when a limit is hit the kernel
/// kills the container's processes rather than the supervisor that records what happened.
pub const CONTAINER_OOM_SCORE_ADJ: &str = "500";

/// True in a process that was started inside its limits scope.
pub fn in_scope() -> bool {
    std::env::var_os(SCOPE_MARKER).is_some()
}

/// `500` thousandths of a CPU -> `50%`; systemd takes a percentage of one CPU.
fn cpu_quota(millis: u32) -> String {
    match (millis / 10, millis % 10) {
        (whole, 0) => format!("{whole}%"),
        (whole, tenth) => format!("{whole}.{tenth}%"),
    }
}

/// The `systemd-run` arguments (before the command) that create a scope with `res`.
pub fn scope_args(res: &Resources) -> Vec<String> {
    let mut args: Vec<String> = ["--user", "--scope", "--collect", "--quiet"].into_iter().map(String::from).collect();
    let mut property = |p: String| {
        args.push("-p".into());
        args.push(p);
    };
    if let Some(memory) = res.memory {
        property(format!("MemoryMax={memory}"));
        // Without this the kernel would page the container out instead of killing it.
        property(format!("MemorySwapMax={}", res.swap_bytes().unwrap_or(0)));
    }
    if let Some(millis) = res.cpus_millis {
        property(format!("CPUQuota={}", cpu_quota(millis)));
    }
    if let Some(pids) = res.pids {
        property(format!("TasksMax={pids}"));
    }
    // The container's own processes may create cgroups below the scope.
    property("Delegate=yes".into());
    args.push("--".into());
    args
}

/// `systemd-run … -- program`, ready for the program's own arguments. The child is marked
/// as being in its scope.
pub fn scope_command(res: &Resources, program: &Path) -> Command {
    let mut cmd = Command::new("systemd-run");
    cmd.args(scope_args(res)).arg(program).env(SCOPE_MARKER, "1");
    cmd
}

/// Rejects limits this machine cannot honour. `host_cpus` is the number of CPUs the machine
/// (the WSL VM) has.
pub fn check_host(res: &Resources, host_cpus: usize) -> Result<()> {
    if let Some(millis) = res.cpus_millis {
        if u64::from(millis) > host_cpus as u64 * 1000 {
            return Err(RuntimeError::start(format!(
                "cpus {} is more than the {host_cpus} CPU(s) of this machine",
                yarc_config::format_cpus(millis)
            )));
        }
    }
    Ok(())
}

/// The CPUs of this machine.
pub fn host_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Checks that systemd can create a scope for this user, with an actionable message when it
/// cannot. A limit that is silently not enforced would be worse than no limit.
pub fn require_scopes() -> Result<()> {
    let hint = "limits need systemd for the user: enable it in /etc/wsl.conf ([boot] systemd=true), \
                run `wsl --shutdown` from Windows and try again";
    let out = Command::new("systemd-run")
        .args(["--user", "--scope", "--collect", "--quiet", "--", "true"])
        .env_remove(SCOPE_MARKER)
        .output()
        .map_err(|e| RuntimeError::start(format!("cannot run systemd-run ({e}); {hint}")))?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr);
    let why = why.lines().next().unwrap_or("no reason given").trim();
    Err(RuntimeError::start(format!("systemd cannot create a scope ({why}); {hint}")))
}

/// `0::/user.slice/.../run-r1.scope` -> `/user.slice/.../run-r1.scope`.
fn own_cgroup(proc_cgroup: &str) -> Option<&str> {
    proc_cgroup.lines().find_map(|l| l.strip_prefix("0::")).map(str::trim)
}

/// The `oom_kill` counter of a `memory.events` file.
fn oom_kill_count(memory_events: &str) -> Option<u64> {
    memory_events.lines().find_map(|l| l.strip_prefix("oom_kill ")).and_then(|n| n.trim().parse().ok())
}

/// How many processes the kernel's OOM killer has killed so far in this process's cgroup
/// (the limits scope); `None` outside a scope or when the counter cannot be read. The counter
/// only grows, so a run is compared against its value before it started.
pub fn oom_kills() -> Option<u64> {
    if !in_scope() {
        return None;
    }
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let dir = Path::new("/sys/fs/cgroup").join(own_cgroup(&cgroup)?.trim_start_matches('/'));
    oom_kill_count(&std::fs::read_to_string(dir.join("memory.events")).ok()?)
}

/// What a limits scope uses right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeUsage {
    pub memory_current: u64,
    /// `None`: no memory limit (`max`).
    pub memory_max: Option<u64>,
    pub pids_current: u64,
    /// Total CPU time used since the scope was created.
    pub cpu_usec: u64,
}

/// `max` -> `None`, a number -> `Some`.
fn parse_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

fn cpu_usage_usec(cpu_stat: &str) -> Option<u64> {
    cpu_stat.lines().find_map(|l| l.strip_prefix("usage_usec ")).and_then(|n| n.trim().parse().ok())
}

/// The usage of the limits scope the process `pid` (a container's supervisor) lives in. Only
/// meaningful for a container started with limits: without them the supervisor shares a cgroup
/// with unrelated processes. `None` when the files cannot be read.
pub fn scope_usage(pid: u32) -> Option<ScopeUsage> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let dir = Path::new("/sys/fs/cgroup").join(own_cgroup(&cgroup)?.trim_start_matches('/'));
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    Some(ScopeUsage {
        memory_current: read("memory.current")?.trim().parse().ok()?,
        memory_max: parse_limit(&read("memory.max")?),
        pids_current: read("pids.current")?.trim().parse().ok()?,
        cpu_usec: cpu_usage_usec(&read("cpu.stat")?)?,
    })
}

/// Makes the container's init (and so everything it starts) the kernel's preferred victim in
/// the scope, ahead of the supervisor. Best effort: raising the score needs no privilege, and
/// without it the limit still holds.
pub fn prefer_as_oom_victim(init_pid: u32) {
    let _ = std::fs::write(format!("/proc/{init_pid}/oom_score_adj"), CONTAINER_OOM_SCORE_ADJ);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(memory: Option<u64>, swap: Option<u64>, cpus: Option<u32>, pids: Option<u64>) -> Resources {
        Resources { memory, memory_swap: swap, cpus_millis: cpus, pids }
    }

    #[test]
    fn the_scope_carries_every_limit_and_no_swap_by_default() {
        let r = res(Some(512 << 20), None, Some(500), Some(200));
        assert_eq!(
            scope_args(&r),
            [
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "-p",
                "MemoryMax=536870912",
                "-p",
                "MemorySwapMax=0",
                "-p",
                "CPUQuota=50%",
                "-p",
                "TasksMax=200",
                "-p",
                "Delegate=yes",
                "--"
            ]
        );
    }

    #[test]
    fn swap_is_the_difference_between_the_two_limits() {
        let args = scope_args(&res(Some(512 << 20), Some(768 << 20), None, None));
        assert!(args.contains(&format!("MemorySwapMax={}", 256 << 20)), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("CPUQuota") || a.starts_with("TasksMax")), "{args:?}");
    }

    #[test]
    fn only_the_limits_that_are_set_become_properties() {
        let args = scope_args(&res(None, None, Some(2000), None));
        assert!(args.contains(&"CPUQuota=200%".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("Memory")), "no memory limit, no swap limit: {args:?}");
    }

    #[test]
    fn cpu_quota_keeps_a_tenth_of_a_percent() {
        assert_eq!(cpu_quota(500), "50%");
        assert_eq!(cpu_quota(1500), "150%");
        assert_eq!(cpu_quota(10), "1%");
        assert_eq!(cpu_quota(15), "1.5%");
    }

    #[test]
    fn the_command_runs_the_program_in_a_marked_scope() {
        let cmd = scope_command(&res(Some(64 << 20), None, None, None), Path::new("/usr/bin/yarc"));
        assert_eq!(cmd.get_program(), "systemd-run");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.last().map(String::as_str), Some("/usr/bin/yarc"));
        assert_eq!(args[args.len() - 2], "--");
        let marker = cmd.get_envs().find(|(k, _)| *k == SCOPE_MARKER).and_then(|(_, v)| v);
        assert_eq!(marker.map(|v| v.to_string_lossy().into_owned()), Some("1".into()));
    }

    #[test]
    fn a_cpu_limit_above_the_machine_is_refused_before_anything_starts() {
        let r = res(None, None, Some(2500), None);
        assert!(check_host(&r, 4).is_ok());
        assert!(check_host(&r, 3).is_ok());
        let e = check_host(&r, 2).unwrap_err().to_string();
        assert!(e.contains("cpus 2.5") && e.contains("2 CPU"), "{e}");
        assert!(check_host(&Resources::default(), 1).is_ok());
    }

    #[test]
    fn usage_files_are_read() {
        assert_eq!(
            cpu_usage_usec(
                "usage_usec 1508000
user_usec 1500000
nr_throttled 31
"
            ),
            Some(1_508_000)
        );
        assert_eq!(
            cpu_usage_usec(
                "user_usec 1
"
            ),
            None
        );
        assert_eq!(
            parse_limit(
                "67108864
"
            ),
            Some(64 << 20)
        );
        assert_eq!(
            parse_limit(
                "max
"
            ),
            None
        );
    }

    #[test]
    fn the_cgroup_and_the_oom_counter_are_read_from_their_files() {
        let proc = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/run-r1.scope\n";
        assert_eq!(own_cgroup(proc), Some("/user.slice/user-1000.slice/user@1000.service/app.slice/run-r1.scope"));
        assert_eq!(own_cgroup("12:memory:/x\n"), None, "cgroup v1 is not supported");
        let events = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(oom_kill_count(events), Some(2));
        assert_eq!(oom_kill_count("low 0\n"), None);
    }
}
