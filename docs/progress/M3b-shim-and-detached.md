# M3b — The shim, detached containers and `ps`

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 52, `yarc-cli` 21 unit + 6 integration, `yarc-oci` 58, `yarc-config` 9). 22 live tests (`--ignored`, network + user namespaces) pass against Alpine 3.20, 7 of them new or rewritten. No processes are left behind after the live tests.

`yarc run -d alpine sleep 100` now returns immediately; the container keeps running after the command that started it has exited.

## What was done

### The shim (`yarc-cli/src/detached.rs`)
- `run -d` and `container start -d` spawn **`yarc __shim <id>`** (a hidden subcommand of the same binary; its stderr goes to `containers/<id>/shim.log`) in a **new session** (`setsid`): closing the terminal or pressing Ctrl-C does not reach it.
- The CLI blocks only until the shim reports **one line on its stdout**: `started <pid>` or `error <code> <message>`. So `run -d` fails like `run` does: `no-such-program` → exit 127 with the real message, a refused start → 125.
- The shim *is* the supervisor: it calls `run_recorded` (M3a) with `detached: true`, so state and logs are recorded exactly as in the foreground. Its signal forwarding is the same too: `kill -TERM <shim>` is forwarded to the container, a second one escalates to SIGKILL. (A proper `stop` is M3c.)
- `--rm` is handed to the shim: it removes the container when it exits, or when it fails to start (but never when the start was refused because the container is already running).

### Liveness by lock instead of pid (`ContainerStore::try_acquire` / `is_supervised`)
M3a recorded the supervisor pid and checked it with `kill(pid, 0)`; that can be fooled by pid reuse and left "start twice" racy. Now the supervisor holds an `flock` on `containers/<id>/lock`:
- the kernel releases it when the holder dies, so it cannot go stale;
- `Running` means "state says running **and** the lock is held"; otherwise it is `Dead`;
- starting a container whose lock is held fails with "already running", race-free (checked by the CLI up front and again by the shim).
- The monitor process inherits the lock too, which is fine: it dies with the supervisor (`PR_SET_PDEATHSIG`) and takes the container with it.

### `yarc ps [-a]`
Running containers only; `-a` lists all. `yarc container ls` still lists everything.

### Bug fixed on the way
Rust starts programs with `SIGPIPE` ignored and the container's command inherited that: `yes | head -n1` printed "Broken pipe" errors instead of dying quietly. The container's init now restores the default (test: `a_container_does_not_inherit_ignored_sigpipe`).

### `run_recorded_with`
Adds an `on_started` callback (called with the init pid or the start error) to `run_recorded`; that is how the shim learns "it's up" without waiting for the container to exit.

## Tests
- **Unit:** report parsing (`started`, `error <code> <msg>`, missing/garbled → 125), CLI parsing of `-d`, `ps`, the hidden `__shim` (not shown in `--help`), `--` before a dash-leading command; lock exclusivity and release, `Running`/`Dead` derived from the lock, updated state descriptions.
- **Live (new/rewritten):** a detached container outlives the command, shows in `ps`, its output is recorded, and two `kill -TERM`s on the shim end it as `Exited (137)`; a detached container that finishes can be started again (logs accumulate); a detached start failure is reported to the caller (127, message, recorded as `Exited (127)`); `-d --rm` removes the container on exit and after a failed start; starting a running detached container again is refused; the lock-based `Running` / `Dead` / "already running" detection (a test process plays the supervisor by holding the lock); SIGPIPE.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_run -- --ignored --test-threads=1'
```

### Try it
Inside WSL, from the repo root:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

$yarc run -d --name ticker alpine:3.20 sh -c 'i=0; while true; do echo tick $i; i=$((i+1)); sleep 1; done'
$yarc ps                                  # Running
sleep 3; $yarc container logs ticker      # tick 0, tick 1, ...
ps -ef | grep -E '__shim|sleep'           # the shim (own session) and the container's processes
pid=$(grep supervisor_pid ~/.local/share/yarc/containers/ticker/state.json | tr -dc 0-9)
kill -TERM $pid; sleep 0.3; kill -TERM $pid     # sh as PID 1 ignores the first TERM; the second one kills it
$yarc ps -a                               # Exited (137)
$yarc container start -d ticker           # start it again; logs keep accumulating
$yarc run -d --rm alpine:3.20 sh -c 'sleep 2; echo bye'; $yarc ps; sleep 3; $yarc ps -a   # gone after it exits
$yarc run -d --rm alpine:3.20 nope; echo "status=$?"       # 127
pid=$(grep supervisor_pid ~/.local/share/yarc/containers/ticker/state.json | tr -dc 0-9)
kill -TERM $pid; sleep 0.3; kill -TERM $pid; sleep 1       # stop it again before removing it
$yarc container rm ticker
```
`stop` and `rm -f` for running containers come in M3c; until then stop a detached container with the two `kill -TERM` steps above, and don't `rm` a running one.

## Not done / follow-ups
- No `stop`/`kill`/`rm -f` yet, and no `logs -f` (M3c); `container rm` on a running container is not guarded yet.
- A shim that is SIGKILLed leaves the container `Dead` (its container is killed along with it via pdeathsig, and the exit code is lost).
- Output of a *detached* container is only in the log (no attach).
- The shim is not restarted after a reboot (no restart policies).
- Spawning uses the current executable path; if the binary is replaced while shims run, they keep running the old one.
- Flags directly after the image are parsed by yarc (`yarc run alpine -d` is `-d` for yarc); use `--` as in Docker.

## Next: M3c — control and logs
`logs -f`, `stop [-t secs]` (SIGTERM to the shim, SIGKILL after the timeout), `kill [-s SIG]`, `rm [-f]` for running containers, `wait`, and a full-lifecycle live test.
