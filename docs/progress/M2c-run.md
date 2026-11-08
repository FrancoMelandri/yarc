# M2c — `yarc run`

Status: **done**. Workspace tests pass with no warnings (`yarc-cli` 15 unit + 6 integration, `yarc-runtime` 33, `yarc-oci` 58, `yarc-config` 9). 13 live tests (`--ignored`, network + user namespaces) pass against Alpine 3.20.

**M2 is complete:** `yarc run --rm alpine:3.20 echo hi` pulls (if needed), creates, runs the container isolated and rootless, passes its exit status on, and cleans up.

## What was done

### `yarc run`
```
yarc run [--rm] [--debug] [--name N] [-e K=V]... [--entrypoint X] [-w DIR] [-u USER] [--network host|none] <image> [cmd...]
```
- It is `create` + `container start` in one command; all `create` options apply. As in Docker, options go before the image and everything after the image is the container's command.
- `--rm` deletes the container afterwards **whether it ran or failed to start**. A failed cleanup only prints a warning and never hides the container's own status.
- Without `--rm` the container stays (visible in `yarc container ls`, restartable with `yarc container start`).
- `create` and `run` share `create_container`; `create` still just prints the id.

### Docker-style exit statuses
A container that cannot be started now makes yarc exit with:
- **127** command not found (searched in the container's `PATH`, or a path that does not exist),
- **126** found but not executable (`EACCES`/`ENOEXEC`),
- **125** any other setup failure.

Otherwise yarc exits with the container's own status (`128+n` if it died from signal `n`). A start failure is `RuntimeError::Start { msg, code }`; the child reports it over the handshake pipe as `error:<code>:<message>` and `main` maps it to the exit code. Errors outside of starting (bad arguments, pull failures) still exit 1.

## Tests
- **Unit:** `run` argument parsing (create options + `--rm`/`--debug`, flags after the image belong to the command, defaults keep the container, image is required).
- **Live (13 in total, 3 new/changed):**
  - missing executable → 127, non-executable file → 126,
  - `run --rm` returns the container's status (3), streams its output and leaves nothing in `container ls`,
  - `run` without `--rm` keeps the container; `run --rm` of a command that cannot start still removes it and exits 127.

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

$yarc run --rm alpine:3.20 echo hi
$yarc run --rm alpine:3.20 sh -c 'echo "hello from $(. /etc/os-release; echo $PRETTY_NAME), pid=$$ uid=$(id -u)"'
$yarc run --rm -e NAME=yarc -w /tmp alpine:3.20 sh -c 'echo "$NAME in $(pwd)"'
$yarc run --rm alpine:3.20 sh -c 'exit 7'; echo "status=$?"          # 7
$yarc run --rm alpine:3.20 nope;            echo "status=$?"          # 127
$yarc run --rm alpine:3.20 /etc/passwd;     echo "status=$?"          # 126
$yarc run --rm --network none alpine:3.20 cat /proc/net/dev           # only lo
$yarc run --rm alpine:3.20 sleep 100                                  # Ctrl-C twice stops it (137)
$yarc container ls                                                    # nothing left behind
```
Not supported yet: `-it` (an interactive shell needs a pty, future work) and `ping` (needs `net.ipv4.ping_group_range` to include your gid; I did not test it).

## Not done / follow-ups (carried from M2b)
- No capability dropping/seccomp/masked paths, no cgroups, single uid only, no pty, no init reaper.
- `--rm` leaks the container directory if yarc itself is killed with SIGKILL.
- Interactive use (`-it`) needs a pty: planned with the shim in M3.

## Next: M3 — agent, shim and detached containers
Per-container shim that outlives the CLI, state on disk (running/exited, pid, exit code, log files), `yarc run -d`, `ps`, `logs`, `stop`, `kill`, `start`, `rm` against the shim; `yarc-agent` and `yarc-proto` start to matter (the Windows host will talk to the agent in WSL). Before planning it we should decide: logs format and location, how the CLI finds a shim (pid file + unix socket per container), and whether the agent already ships in M3 or only the shim.
