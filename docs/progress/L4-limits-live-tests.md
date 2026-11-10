# L4 — limits: live tests (done)

`crates/yarc-cli/tests/live_limits.rs`, run with
`cargo test -p yarc-cli --test live_limits -- --ignored --test-threads=1` (needs Docker Hub for alpine, user namespaces and a systemd user manager; one at a time because the CPU test measures a share of the machine). Each test reads the container's real scope under `/sys/fs/cgroup`, found through the supervisor's pid.

| Test | Proves |
|---|---|
| `a_memory_limit_kills_the_allocator_with_137_and_ps_says_so` | foreground `run --rm -m 64m` exits 137; detached: `Exited (137) OOM killed`, `oom_killed` in the state, no output after the kill; a smaller allocation under 256m is not flagged |
| `the_scope_carries_the_limits_and_swap_is_off_by_default` | `memory.max`, `memory.swap.max` (0 by default; `--memory-swap` is memory plus swap), `cpu.max` (`50000 100000`, `150000 100000`), `pids.max`; a CPU-only limit leaves memory at `max` |
| `a_cpu_quota_throttles_busy_loops_to_about_the_quota` | two busy loops under `--cpus 0.5` use 0.3-0.8 CPU over 3 s; `stats` lists the container |
| `the_pids_limit_stops_a_fork_bomb` | the shell reports `can't fork` under `--pids-limit 20` |
| `no_limits_means_no_scope_and_nothing_to_measure` | the supervisor is not in a `run-*.scope`; `stats` says there is nothing to measure |
| `on_failure_restarts_an_oom_killed_container_into_the_same_scope` | `--restart always` + OOM: restarts accumulate, the supervisor's cgroup is the same, `OOM killed` shows, `rm -f` removes the scope |
| `stopping_a_limited_container_during_its_backoff_returns_at_once` | `stop` during a 5 s backoff takes < 3 s, stays `Exited (1)`, scope gone |
| `stop_and_rm_force_leave_no_scope_behind` | the cgroup directories disappear |
| `a_limit_asked_for_without_a_usable_systemd_fails_with_the_reason` | without `systemd-run` a limited container fails (exit 125, names systemd and `wsl.conf`) while an unlimited one starts; `--cpus 9999` is refused |
| `project_limits_are_enforced_and_down_removes_the_scopes` | `yarc.yaml` limits OOM-kill the hog, set the idle service's cgroup, show in `project config`; changing a limit recreates the service (new scope value); `down` removes the scopes |

## Bug found and fixed
`run --rm` of a container with limits left the container behind: the foreground re-entry into the scope was an `exec`, so the cleanup after it never ran. `restart_in_scope` now runs the scope as a child and returns its exit code (128 + signal when killed), so `--rm` works.

## Also run (no regressions)
`live_restart` 13, `live_run` 30, `live_project` 10, `live_volumes` 9, `live_agent` 2, all green; fmt, clippy `-D warnings` and `cargo test --workspace` clean on Windows and Linux.

## Not covered
The "husk/lock" behaviour from F1 has no limits-specific test: the existing suites cover it and still pass. The pre-existing `live_network` failure was not touched.
