# M4b — `yarc exec` and health checks

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 66, `yarc-cli` 36 unit + 6 + 5 integration, `yarc-config` 11, `yarc-oci` 58). Live tests (`--ignored`): 10 project tests (4 new) and 30 container tests (1 new) pass.

## The spike (done first, as planned)
Question: can a process of the same user join the namespaces of a running *rootless* container? **Yes, on this WSL kernel.** `yarc exec` into a running container showed the container's own PID namespace (it sees PID 1, its own pid is small), uid 0, the container's hostname, environment and filesystem; stdin, exit codes and `-e/-w` work. No design change was needed.

## What was done

### `yarc exec [-e K=V] [-w DIR] <container> <command...>` (`yarc-runtime/src/exec.rs`)
```
caller (yarc / shim)
 └─ helper        setns(user, mnt, uts, ipc, [net,] pid)
     └─ process   fchdir + chroot into the container's root, chdir, execve
```
- The user namespace is joined **first**: being its owner (same uid as the container's creator) is what grants the capabilities for the other `setns` calls, and then the container's uid mapping applies (root inside, like `init`).
- Everything under `/proc/<init pid>` (the namespace files and the root directory) is opened *before* joining, because afterwards `/proc` is not ours any more. `setns(mnt)` does not change the root directory, hence `fchdir` + `chroot(".")` into the container's root.
- Joining the PID namespace only affects children, hence the second fork. The helper is a fresh fork of a single-threaded process, which `setns(CLONE_NEWUSER)` requires.
- Environment: the container's, plus `-e` (which wins); working directory: `-w` or the container's; executable lookup uses the container's `PATH`; exit statuses 127/126/125 as for `run`. `PR_SET_PDEATHSIG`: if the caller dies, so does the command.
- The recorded init pid is only trusted while the supervisor lock is held (shared helper `running_pid`), so exec into a stopped container says "not running" and can never hit a recycled pid.
- Also available as `yarc container exec`.
- Shared with the container's init: `exec_program` (lookup + `execve` + status codes) was factored out of `linux.rs`.

### Health checks
- **Config → container:** `health: { cmd, interval }` of a service becomes a `HealthSpec` in the container's `config.json` (`sh -c <cmd>`, Docker's defaults for the rest: 30 s timeout, 3 retries). `cmd` is a shell command line, so the image needs a `sh`.
- **State machine (`health.rs`, pure):**
  - the first check runs 1 s after the container starts, later ones `interval` after the previous one *finished* (never overlapping);
  - success → `healthy` (streak reset); a failure (non-zero exit, could not run, timeout) extends the streak and after `retries` failures in a row the container is `unhealthy` until a check passes; before the first success it is `starting`.
- **Supervisor (`lifecycle.rs`):** the checks run from the same `poll` loop that pumps the output, via a new periodic tick (200 ms): no extra thread, so the supervisor stays single-threaded. A check is *started* on one tick and *polled* on later ones, so a slow check never stops the output from being drained. Checks time out (killed) and are killed when the container exits. Their output is discarded.
- **State:** `state.json` gets an optional `health` (`status`, `failing_streak`, `last_exit`, `last_check_ms`), written only when the status or streak changes; dropped when the container exits. Old state/config files without the new fields still load.
- **Display:** `Running (starting|healthy|unhealthy)` in `ps`, `ls` and `project ps`; `ps` (running only) lists them all.
- **`up` with `needs: healthy`:** after starting each dependency that is required healthy, `up` waits until it reports `healthy`, with a progress line ("waiting for db to become healthy..."). It fails with a clear message if the dependency becomes `unhealthy`, exits first ("exited (status N) before it became healthy"), or dies; the dependents are not created. The wait times out after 120 s plus `interval × retries`. Ctrl-C during the wait ends the foreground `up` as usual.
- The "health checks do not run yet" warnings are gone.

## Tests
- **Unit:** health state machine (initial delay, success, interval, no overlap, failure streak and recovery, failure after healthy needs all retries, nothing to persist when unchanged, timeouts and unrunnable checks, retries = 1); the pump's tick runs while the container is silent; spec/state JSON round trips and compatibility with files written before this step; status text with health; CLI parsing of `exec`.
- **Live (exec):** inside the container: small pid, uid 0, hostname, env, working directory, only the container's processes visible; `-e`/`-w`; exit status 3; command not found → 127; bad `-e`; what `exec` writes is what the container sees; exec into a stopped or unknown container is refused with a clear message.
- **Live (health):**
  - `needs: healthy` waits until the check passes (web starts ≥ 3 s after db), `Running (healthy)`, and the check keeps running periodically inside the container;
  - a container is `Running (starting)` until its first check passes, then `healthy`, and is listed by `ps`;
  - an unhealthy dependency makes `up` fail naming both services and `web` is never created;
  - a dependency that exits before becoming healthy is reported with its status.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_project -- --ignored --test-threads=1'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_run -- --ignored --test-threads=1'
```

### Try it
Inside WSL:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cd /mnt/c/Projects/MINE/yarc && cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

# exec
$yarc run -d --name box -e GREETING=hi alpine:3.20 sh -c "trap 'exit 0' TERM; while true; do sleep 0.2; done"
$yarc exec box sh -c 'echo "$GREETING from pid $$ as uid $(id -u) on $(hostname)"; ps'
$yarc exec -e GREETING=yo -w /tmp box sh -c 'echo "$GREETING in $(pwd)"'
echo piped | $yarc exec box cat
$yarc exec box sh -c 'exit 3'; echo "status=$?"
$yarc stop box && $yarc rm box

# health + needs: healthy
mkdir -p ~/demo2 && cd ~/demo2
cat > yarc.yaml <<'EOF'
services:
  db:
    image: alpine:3.20
    cmd: [sh, -c, "sleep 4; touch /tmp/ready; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    health: { cmd: "test -f /tmp/ready", interval: 1s }
  web:
    image: alpine:3.20
    cmd: [sh, -c, "echo web is up; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    needs: { db: healthy }
EOF
$yarc up -d                        # "waiting for db to become healthy..." ~4 s, then web starts
$yarc project ps                   # db: Running (healthy)
$yarc down
sed -i 's/test -f \/tmp\/ready/false/' yarc.yaml
$yarc up -d; echo "status=$?"      # "db is unhealthy" after ~3 s, web is never created
$yarc project ps                   # db: Running (unhealthy)
$yarc down
```

## Not done / follow-ups
- `yarc exec` has no terminal (`-it`): stdin/stdout are inherited as pipes or the terminal as is, but no pty is allocated.
- Health checks run `sh -c`; there is no exec-form (`cmd: [...]`) yet, and `timeout`, `retries`, `start period` are fixed (30 s, 3, none) because the file format has no keys for them.
- The check's output is not kept (Docker keeps the last few lines, shown in `inspect`).
- Unhealthy containers are not restarted or killed; `unhealthy` is only reported and used by `up`.
- `yarc exec` into a container started *before* this step works, but its health state is absent (no health spec in its config).
- A supervisor started by an older yarc keeps running without health checks until the container is restarted.

## Next: M4c — project network, DNS and published ports
The big one: all services of a project in one shared user + network namespace (a holder process owned by `up`), service names resolved through `/etc/hosts`, `ports:` published by a userspace TCP proxy on `127.0.0.1`. It starts with a spike on joining a shared user namespace from several containers (the part of the M2b runtime that has to change).
