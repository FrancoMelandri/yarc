# L — CPU and memory limits: plan (for review, nothing implemented)

Status: **proposal, waiting for review.** Decisions that need your answer are at the end, each with a recommendation; "defaults" accepts them all.

## What I measured first (WSL Ubuntu-24.04, kernel 6.18, cgroup v2, systemd, uid 1000)

Rootless limits are a cgroup question, so I tested what an unprivileged process can really do here, before designing anything:

| Experiment | Result |
|---|---|
| Write to the cgroup the `wsl.exe` session lives in (`/non-systemd`) | **denied**. A process started from Windows cannot create children there, nor be moved into the delegated tree (`cgroup.procs` write fails) |
| `mkdir` + write `memory.max` under the user's delegated `app.slice` | works, but `cpu` is **not** enabled there (only `memory pids`), so `cpu.max` is denied |
| `systemd-run --user --scope -p MemoryMax=64M -p CPUQuota=50% …` | **both limits applied** (`memory.max=67108864`, `cpu.max=50000 100000`) |
| Same scope, two busy loops for 3 s | 1.55 s of CPU used, 31 throttle events: the quota is enforced |
| Same scope, allocate 200 MB | killed, **exit 137**, `memory.events: oom_kill 1`: the limit is enforced |
| Same scope with `-p Delegate=yes`, create a child cgroup inside | works |
| `sudo` | needs a password: not an option |

Conclusion: **we cannot write cgroup files directly from where yarc runs, but we can ask the user's systemd to create a scope with the limits.** Podman's rootless `--cgroup-manager=systemd` does the same.

## Mechanism

A container **with** limits has its shim started inside a transient systemd scope; a container without limits is launched exactly as today (no wrapper, no behaviour change, no new risk for existing setups).

```
yarc up -d  ->  systemd-run --user --scope --collect --quiet \
                   -p MemoryMax=512M -p MemorySwapMax=0 -p CPUQuota=50% -p Delegate=yes \
                   yarc --root R __shim <id>
```

- The scope contains the shim and everything it forks (container init, exec'd processes, health probes), so the limits cover the whole container and survive restarts: C4's supervisor keeps the same scope across runs, `memory.events` accumulates, and an OOM kill is just a non-zero exit (137) that the restart policy already handles.
- `--collect` removes the scope when the shim exits, so nothing is left behind after `down`/`rm`.
- The shim's own memory (a few MB) counts against the limit. To keep the OOM killer from choosing the shim over the container, the container's init raises its own `oom_score_adj` (an unprivileged process may raise it) before `exec`.
- Foreground `yarc run` with limits re-executes itself under the same `systemd-run` (guarded by an environment marker so it cannot loop); `--pty`-less, same exit code.
- If `systemd-run --user` does not work (no systemd in the WSL distro, no user manager), a container **with** limits fails to start with the reason and the fix (see Decision 2). A container without limits is unaffected.

Not chosen: writing cgroup files ourselves (shown above to be impossible from the `wsl.exe` cgroup, and `cpu` is not delegated by default), and a separate cgroup manager crate (more code for the same effect).

## What users can write

| Where | Keys |
|---|---|
| `yarc run` / `container create` | `--memory`/`-m 512m`, `--memory-swap 1g` (total, like Docker), `--cpus 0.5`, `--pids-limit 200` |
| compose, service level | `mem_limit`, `memswap_limit`, `cpus`, `pids_limit` |
| compose, `deploy.resources.limits` | `memory`, `cpus`, `pids` (`reservations` is parsed and **ignored with a note**: nothing to reserve) |
| `yarc.yaml` (own format) | `resources: {memory, cpus, pids}` |

Sizes: `512m`, `1g`, `1.5g`, `256mb`, plain bytes, case-insensitive; `${MEM:-512m}` interpolation works as everywhere else. Anything else resource-related (`cpu_shares`, `cpuset`, `cpu_period`, `ulimits`, `shm_size`, `mem_reservation`) keeps failing loudly as an unsupported key, same policy as the rest of the reader.

Validation up front, before anything is created: memory ≥ 6 MB (Docker's floor), `cpus` > 0 and ≤ number of host CPUs, `memswap_limit` ≥ `mem_limit`, sizes that overflow are errors.

Visible results:
- `yarc ps`: a container killed by the OOM killer shows **`Exited (137) OOM killed`** (and `Restarting (OOM killed, in 4s)` under a restart policy). Detected by reading `memory.events` (`oom_kill`) of the scope after the run.
- `yarc project config` prints the limits per service.
- `yarc stats [name…]` (one-shot, small): memory used / limit, CPU % over a 1 s sample, pids; from the scope's `memory.current`, `cpu.stat`, `pids.current`. Optional; can be dropped from the first release.

Changing a limit changes the definition hash, so `up` recreates the container (no live update; `yarc update` is out of scope).

## Steps (each ends green: tests, no warnings, a short `docs/progress/L<n>-*.md`; I never commit)

**L1 — Model and parsing (pure, tests run everywhere).**
`ByteSize` and CPU parsing, a `Resources { memory, memory_swap, cpus_millis, pids }` value in `yarc-config` and `ContainerSpec` (serialized only when set), hash coverage, validation, the compose keys above (`deploy` accepted for `resources.limits` only, other `deploy` keys remain errors), `yarc.yaml` `resources`, interpolation. Error messages never echo more than the key and the offending token.

**L2 — Runtime: the scope.**
A `limits` module: build the `systemd-run` command from `Resources` (pure, unit-tested as argv), probe availability once (`systemd-run --user --scope true`), wrap in `detached.rs` (shim) and in foreground `run`, `oom_score_adj` in the init, OOM detection after a run, `ContainerState` gains `oom_killed` so `describe` can say so. Unit tests for the argv, for state rendering and for the "systemd unavailable" error.

**L3 — Surface: CLI, protocol, Windows client.**
Flags on `run`/`create`, `CreateSpec` fields and `argv.rs` so the Windows client forwards them to the WSL agent unchanged, `project config` output, `ps` text, `stats` if kept.

**L4 — Live tests (`-- --ignored`).**
A memory limit kills an allocator with 137 and `ps` says OOM killed; swap off means no paging out; a CPU quota throttles two busy loops to ≈ the quota (generous tolerance, it is a ratio); no limits means no scope at all; `on-failure` restarts an OOM-killed container into the same scope; `down` and `rm -f` leave no scope; stopping during backoff still returns at once; the husk/lock behaviour of F1 is unchanged.

**L5 — End to end and docs.**
Add a `mem_limit`/`cpus` to a copy of each real bundle and run `up`; check the JVM sees the limit (`-XX:+UseContainerSupport` reads `memory.max`, so the PMS heap should size itself from it, a real benefit worth confirming); update `design.md` (cgroups section) and the README; write the "what is not covered" list.

Rough size: L1 and L2 are the substance; L3–L5 are small. L2 is the only risky one (it changes how the shim is launched), which is why it comes second and is tested live before anything else builds on it.

## Decisions

1. **Mechanism.** Recommended: systemd user scope per container *with limits* (above). Alternative: only document how to set `Delegate` and write cgroup files ourselves; I measured that this does not work from a `wsl.exe` session.
2. **No systemd available and a limit was requested.** Recommended: **fail to start** with an actionable message (`systemd --user not available; enable systemd in /etc/wsl.conf …`), because a limit that is silently not enforced is worse than no limit. Alternatives: warn and run unlimited; or fail by default with `YARC_LIMITS=ignore` to downgrade to a warning (useful for running a vendor bundle on a machine without systemd; cheap to add).
3. **Swap.** Recommended: when `mem_limit` is set and no `memswap_limit`, swap is **0**, so exceeding the limit is an OOM kill, predictable and testable. Docker's default allows 2× in total; on WSL the swap file is on by default, so a container would page out instead of failing.
4. **`stats` command.** Recommended: include a minimal one-shot version in L3 (you need *some* way to see the limit working); drop if you want a smaller first release.
5. **Scope of the first release.** Recommended: memory, swap, cpus, pids only. No `cpu_shares`/`cpuset`/`ulimits`/`blkio`; no live update.

## Limits of this design (to state in the docs)
- Page cache and in-container `tmpfs` count against the memory limit (cgroup semantics, same as Docker).
- The limits apply inside the WSL VM; the VM's own size is `.wslconfig`'s business, and a `cpus` above the VM's CPU count is rejected.
- Without a user systemd manager there are no limits at all (Decision 2).
- A machine with cgroup v1 only is unsupported (WSL2's default kernel is v2).
