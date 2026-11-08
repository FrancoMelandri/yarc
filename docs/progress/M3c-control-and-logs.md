# M3c — `stop`, `kill`, `wait`, `rm -f`, `logs -f`

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 54, `yarc-cli` 25 unit + 6 integration, `yarc-oci` 58, `yarc-config` 9). 29 live tests (`--ignored`, network + user namespaces) pass against Alpine 3.20, 7 of them new. No processes are left behind.

**M3 is complete:** containers can be run in the background and fully managed (`run -d`, `ps`, `logs -f`, `stop`, `kill`, `wait`, `start`, `rm -f`), with no daemon: just one shim per container.

## What was done

### Control (`yarc-runtime/src/lifecycle.rs`)
- `signal_container(id, signo)`: sends the signal to the container's **init** pid from `state.json`, after checking that the container is really supervised (the lock is held), so a recycled pid can't be hit. `ESRCH` (it exited just now) is reported as "not running".
  Signals from the host only reach a PID 1 that has a handler for them; SIGKILL/SIGSTOP always arrive.
- `stop_container(id, timeout)`: SIGTERM → wait up to `timeout` → SIGKILL → wait up to 10 s. Stopping a container that is not running is a successful no-op (like Docker). A container whose PID 1 handles SIGTERM exits immediately; one that ignores it (e.g. plain `sleep`) takes the full timeout.
- `remove_container(id, force)`: a running container is refused ("is running: stop it first, or force removal") unless `force`, which kills it and waits. The supervisor lock is held during the removal so nobody can start the container meanwhile. (`rm` used to delete a running container's files; fixed.)
- `ContainerStore::wait_exit(id, timeout)`: polls the supervisor lock every 50 ms; used by `stop`, `rm -f` and `wait`.

### Following logs
`read_log_from(path, offset)` returns only the complete entries after a byte offset together with the next offset, so `logs -f` never re-reads the file and never shows or skips half-written entries. The follower checks the lock *before* each read; when the container is gone, one last read guarantees nothing it wrote is lost, then it returns.

### CLI
```
yarc stop [-t SECONDS] <id>...        (default 10 s)
yarc kill [-s SIGNAL] <id>...         (default KILL; name, SIGname or number)
yarc wait <id>...                     (prints each exit code)
yarc rm [-f] <id>...
yarc logs [-f] [--tail N] [-t] <id>
```
- All container commands now also work **without** the `container` prefix (`yarc stop x`, `yarc ps`, `yarc logs -f x`, `yarc rm -f x`, `yarc start -d x`, `yarc ls`); `yarc container ...` keeps working.
- `wait` on a container that was never started says so; on a `Dead` one it reports that no exit status was recorded.
- Signal names are validated before any container is touched (`kill -s NOPE` changes nothing).
- The new commands live in `control.rs`; on non-Linux they report "needs Linux; run yarc inside WSL".

## Tests
- **Unit:** signal parsing (names, case, `SIG` prefix, numbers; rejects `0`, `-1`, `32`, `SIGSIGTERM`, ...), `read_log_from` (missing file, nothing new, half an entry is neither returned nor skipped, offsets), `wait_exit` (immediate, timeout, released by another thread), CLI parsing of all new commands and of the prefix-less forms.
- **Live (7 new):** `stop` of a SIGTERM-handling container is fast and exits 0 (log shows the trap ran; a second `stop` is fine); `stop -t 1` of an ignoring one waits ≥1 s then ends as `Exited (137)`; `kill -s SIGUSR1` runs the trap without stopping it, plain `kill` gives 137, killing a stopped container and a bad signal name are rejected with clear messages; `rm` refuses a running container, `rm -f` removes it and its directory, `rm` of an unknown one fails; `wait` prints `5`, answers immediately for finished containers, rejects a never-started one; `logs -f` ends by itself after the container exits and has both streams complete; a whole lifecycle with the prefix-less commands (create, start -d, ps, logs, stop, restart, rm -f).

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

$yarc run -d --name ticker alpine:3.20 sh -c 'trap "echo bye; exit 0" TERM; i=0; while true; do echo tick $i; i=$((i+1)); sleep 1; done'
$yarc ps
$yarc logs -f ticker          # streams ticks; Ctrl-C stops following (not the container)
$yarc stop ticker             # SIGTERM: the trap prints "bye" and it exits cleanly
$yarc ps -a                   # Exited (0)
$yarc logs --tail 3 ticker
$yarc start -d ticker && $yarc wait ticker &      # wait blocks until the next exit
sleep 2; $yarc kill ticker; wait                  # prints 137
$yarc run -d --name sleeper alpine:3.20 sleep 100
$yarc stop -t 2 sleeper       # `sleep` ignores TERM as PID 1: takes 2 s, then SIGKILL
$yarc rm ticker sleeper       # both are stopped
$yarc run -d --name busy alpine:3.20 sleep 100
$yarc rm busy                 # refused: it is running
$yarc rm -f busy              # killed and removed
```

## Not done / follow-ups
- `stop` ignores an image's `STOPSIGNAL`; it always sends SIGTERM.
- `logs -f` polls every 100 ms (no inotify); a very busy container is read in 100 ms batches.
- `wait` and `stop` poll the lock every 50 ms.
- No `restart`, `pause`, `exec`, `attach`, `stats`, `inspect` yet.
- A container left `Dead` (shim SIGKILLed) cannot report an exit code.
- The signal is sent to PID 1 only; processes it spawned are not signalled (they die when PID 1 exits).
- Windows/macOS hosts can't run any of this yet: the agent is next.

## Next: M4 — compose-style projects
`yarc up` / `down` / `ps` for a `yarc.yaml` project: start order from `needs` (started/healthy), healthchecks, environment, a per-project container naming scheme, and the first step toward networking between services and published ports. Before planning it we need decisions on how services reach each other (rootless networking is the big open question) and on port publishing.
