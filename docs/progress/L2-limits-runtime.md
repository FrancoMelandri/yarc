# L2 — limits: the runtime scope (done)

## What exists now
- `yarc-runtime/src/limits.rs`:
  - `scope_args` / `scope_command`: `systemd-run --user --scope --collect --quiet -p MemoryMax=… -p MemorySwapMax=… -p CPUQuota=…% -p TasksMax=… -p Delegate=yes -- <program>` (pure, unit-tested as argv). Only the limits that are set become properties; with a memory limit, swap is `MemorySwapMax=0` unless `memory_swap` allows some. The child gets `YARC_LIMITS_SCOPE=1` so it is never wrapped twice.
  - `check_host`: refuses `cpus` above the machine's CPU count, before anything starts (`cannot start container: cpus 99 is more than the 20 CPU(s) of this machine`).
  - `require_scopes`: probes `systemd-run --user --scope true`; if it fails, the container does not start and the message says how to enable systemd (`/etc/wsl.conf`, `wsl --shutdown`). Containers without limits never call it.
  - `oom_kills` (the `oom_kill` counter of the scope's `memory.events`) and `prefer_as_oom_victim` (container init's `oom_score_adj` = 500, best effort).
- `detached::start`: a container with limits has its shim started through `scope_command`; one without is started exactly as before.
- Foreground `container start` of a container with limits re-executes itself under the same scope (`restart_in_scope`, same terminal and exit code).
- `run_once`: reads the counter before and after each run; the run is `oom_killed` when it grew. `ContainerState.oom_killed` (only stored when true); `describe` says `Exited (137) OOM killed` and `Restarting (exit 137, OOM killed, in 4s)`.
- `parse_cpus` now requires at least 0.01 CPU (cgroups need 1 ms per 100 ms period).

## Checked live (WSL Ubuntu-24.04, 20 CPUs, limits written into config.json by hand; there are no flags until L3)
| Case | Result |
|---|---|
| 64m limit, process allocating 200 MB, foreground | exit 137, `ps`: `Exited (137) OOM killed`, state recorded by the surviving shim |
| Within the limit | exit 0; the container's cgroup is a `run-r….scope` |
| `cpus` 99 on a 20-CPU machine | exit 125 with the message above, foreground and `-d` |
| `cpus 0.5`, two busy loops, detached | 1508 ms of CPU in 3 s; scope has `cpu.max 50000 100000` |
| No limits | cgroup is `/non-systemd` as before: no scope |
| 64m + `--restart on-failure:2` | restarted twice, each run flagged OOM killed, final `Exited (137) OOM killed`, scope gone afterwards |
| Container init | `oom_score_adj` is 500 |

## For later steps
- L5: inside the container `/sys/fs/cgroup` shows the host's cgroup files, not the scope's (`memory.max` reads `max`). A JVM reading `memory.max` will not see the limit; check whether the sandbox needs its own cgroup mount or namespace.
- Nothing yet reads the limits from the CLI, the protocol or compose into a container: that is L3 (`NewContainer`/`CreateSpec`/`project up`).
