# M3a — Output capture, JSON-lines logs and run state

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 51, `yarc-cli` 16 unit + 6 integration, `yarc-oci` 58, `yarc-config` 9). 16 live tests (`--ignored`, network + user namespaces) pass against Alpine 3.20, 3 of them new.

M3 plan decisions (defaults): logs as JSON lines, control by signals + state files (no socket yet), shim only (agent comes with compose in M4). M3a is the groundwork: **a running container's output and state are now recorded**, whether it runs in the foreground (today) or under a shim (M3b).

## What was done

### Runtime: `start` + `Running` instead of a blocking `run`
- `start(spec, StartOptions) -> Running` returns as soon as the container's command has been exec'd; `Running::wait()` blocks until it exits (status as before, `128+n` for signals); `Running::pid()` is the host pid of the container's init.
- `StartOptions { debug, stdin, stdout, stderr }` takes optional descriptors that become the container's stdio (`None` = inherit). `start` closes yarc's copies, so a reader sees EOF exactly when the container's side closes. In the container's init they are `dup2`ed onto 0/1/2 before anything else.
- Signal forwarding stays active between `start` and `wait`, so Ctrl-C works while output is being pumped.
- `process_alive(pid)` (`kill(pid, 0)`; EPERM counts as alive).

### Output pump (`pump.rs`)
One `poll` loop over the stdout and stderr pipes, no threads (the process stays single-threaded and safe to fork from). Raw chunks are forwarded to the terminal immediately, so prompts without a newline appear; the log is line-framed. A closed terminal stops the forwarding silently; a failing log (disk full) is reported at the end, *after* draining, so the container never blocks on a full pipe.

### Logs (`logs.rs`, portable)
- One JSON line per output line: `{"ts":1700000000123,"stream":"stdout","line":"hello"}` in `containers/<id>/container.log`, opened in append mode: restarting a container keeps its history.
- Per-stream partial-line buffers (stdout and stderr chunks never mix), one `write` per batch of complete lines, unterminated output flushed at EOF, lines over 64 KiB split, invalid UTF-8 replaced, a torn last line skipped by the reader.
- `format_timestamp` gives RFC 3339 UTC without a date/time crate.

### State (`state.rs`, portable)
`containers/<id>/state.json`, written atomically: `status` (`running`/`exited`), `supervisor_pid`, `pid`, `started_ms`, `finished_ms`, `exit_code`. A container without a state file is `Created`.
- `Exited (n)` for finished containers (including ones that failed to start: 125/126/127).
- **`Dead`**: recorded as running but its supervisor process no longer exists (yarc killed with SIGKILL, machine rebooted). Its exit code is unknown; starting it again is allowed.
- Starting a container whose supervisor is alive fails with "container X is already running".

### `run_recorded` (`lifecycle.rs`)
The single code path that runs a container and records it: state → `running`, start, state with pid, pump output to the log (and to the terminal unless detached), wait, state → `exited`. The foreground CLI uses it now; the shim in M3b will use the same function with `detached: true` (stdin is `/dev/null`, no terminal output).

### CLI
- `yarc container start/run` record state and logs as above.
- `yarc container logs [--tail N] [-t] <id>`: stdout lines to stdout, stderr lines to stderr, like `docker logs` (follow mode `-f` comes in M3c).
- `yarc container ls` now has a STATUS column (`Created`, `Running`, `Exited (7)`, `Dead`).

## Tests
- **Unit:** log framing (partial lines, per-stream, finish, empty lines, CR, long lines, exact-limit line, invalid UTF-8, JSON escaping, torn last line, missing file, timestamps incl. a leap day), state transitions/descriptions/JSON, store state roundtrip and status text, pump with real pipes (both streams until EOF, output over time with one stream closing first, broken terminal, failing log), CLI parsing of `logs`.
- **Live (3 new):** output per stream with a partial last line shown live and recorded, `--tail`, `-t`, `Exited (4)` listed, a second start appends to the log, `state.json` contents; a failed start is recorded as `Exited (127)`; a hand-written state with a live supervisor gives "already running", with a missing one gives `Dead` and can be started again.

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

$yarc run --name demo alpine:3.20 sh -c 'echo out; echo err >&2; sleep 1; echo done; exit 3'
echo "status=$?"                                   # 3
$yarc container ls                                  # STATUS: Exited (3)
$yarc container logs demo                           # out, done (err goes to stderr)
$yarc container logs -t --tail 2 demo               # with timestamps
cat ~/.local/share/yarc/containers/demo/container.log   # the raw JSON lines
cat ~/.local/share/yarc/containers/demo/state.json
$yarc container start demo >/dev/null; $yarc container logs demo   # history is kept, now twice
$yarc container rm demo
```

## Not done / follow-ups
- Foreground output now goes through pipes, so the container sees pipes (not a terminal) on stdout/stderr; programs that check `isatty` behave as in `docker run` without `-t`. A pty arrives with interactive support.
- Log files grow without limit (no rotation).
- `Dead` containers lose their exit code; there is no recovery.
- pid reuse could make a dead supervisor look alive (the check is `kill(pid, 0)`); a start-time check can be added.
- Two simultaneous `start`s of the same container race on the check (the state is written right after it); the shim in M3b will need a lock file.

## Next: M3b — the shim
Hidden `yarc __shim <id>`, `run -d` / `container start -d` (the CLI spawns the shim detached and returns once the container is running or failed), `container ps [-a]`, and stale-state handling; the shim calls `run_recorded` with `detached: true`.
