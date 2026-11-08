# M2b — The rootless runtime: `yarc container start`

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 33, `yarc-cli` 13 unit + 6 integration, `yarc-oci` 58, `yarc-config` 9). 10 new live tests (`--ignored`, need network and user namespaces) pass against Alpine 3.20.

A created container (M2a) can now actually run, isolated, in the foreground, without root and without Docker.

## What was done

### Process model (`yarc-runtime/src/linux.rs`, Linux only, `nix 0.29`)
```
yarc (supervisor)      writes uid/gid maps, forwards signals, returns the exit code
 └─ monitor            unshare(USER|NS|PID|UTS|IPC [|NET]), then forks; relays init's exit status
     └─ init (PID 1)   mounts, pivot_root, execve() of the container command
```
- Why a monitor: `unshare(CLONE_NEWPID)` only affects *children*; the first child afterwards is PID 1. The supervisor can't `wait` on a PID 1 it didn't fork, so the monitor relays the status.
- Handshake over a close-on-exec pipe with a tiny line protocol (`unshared`, `pid:<n>`, `error:<msg>`). EOF means "the command was exec'd", so setup errors from either child come back as a normal `Err` with the real reason (e.g. `executable file not found in $PATH: x`).
- A second pipe lets the supervisor tell the monitor "go" once the id maps are written, or abort.
- **User namespace, single mapping:** the invoking uid/gid maps to 0 inside (`setgroups deny`, `uid_map`, `gid_map`). So the container sees uid 0, the host sees you. Works in WSL without sudo.
- **Init steps:** make mounts private (`MS_REC|MS_PRIVATE`), bind-mount the rootfs onto itself, execute the setup plan, `sethostname`, loopback up (for `--network none`), `chdir` + `pivot_root(".", ".")` + detach the old root, `chdir` to the working dir (created if missing, like Docker), `execve`.
- **Executable lookup uses the container's `PATH`**, not the host's (`execvp` would use the host's). A name containing `/` is used as is.
- **Signals:** Ctrl-C reaches the whole process group; the monitor ignores terminal signals, and the supervisor forwards SIGINT/SIGTERM/SIGHUP/SIGQUIT to PID 1. PID 1 of a namespace ignores signals it has no handler for, so a **second** signal escalates to SIGKILL (exit status 137). Handlers are restored when the run ends.
- `PR_SET_PDEATHSIG(SIGKILL)` in the monitor and init: if yarc dies, the container dies too, nothing is orphaned.
- Exit status: the command's own, or `128 + n` if killed by signal `n`; `yarc` passes it on as its own exit code.

### The setup plan (`plan.rs`, portable and unit-tested)
`mount_plan(spec) -> Vec<SetupOp>` is plain data (`Fs`, `Bind`, `Symlink`, `WriteFile`, `CopyHost`), built without privileges and printable with `--debug`:
- `/proc` (nosuid, nodev, noexec); `/dev` as tmpfs with the host's `null zero full random urandom tty` bind-mounted in (rootless can't `mknod`); `/dev/pts` (devpts, newinstance); `/dev/shm`; symlinks `ptmx`, `fd`, `stdin`, `stdout`, `stderr`.
- `/sys`: read-only recursive bind of the host's (host network), or a read-only `sysfs` mount (`--network none`, where our namespace owns the netns). Optional: if it fails it is skipped.
- Generated `/etc/hostname` and `/etc/hosts`; with host networking the host's `/etc/resolv.conf` is copied in (so DNS works).
- Each step is marked required or optional; optional failures are only reported with `--debug`.
- `check_user`: only root (`0`, `root`, `0:0`, …) is accepted for now; anything else is a clear error instead of a confusing failure.

### Rootfs path safety (`paths.rs`)
The rootfs is untrusted. Mount points and generated files are created *before* `pivot_root`, so an image with `/proc -> /etc` (or `/dev/pts -> /host`) could redirect mounts/writes to the host. `safe_join` refuses `..` and any symlink on the path; `safe_join_file` allows a symlink only as the final component, which is then replaced (`/etc/resolv.conf -> /run/...` is common) and never written through.

### CLI
```
yarc container start [--debug] <id>
```
Runs in the foreground with stdin/stdout/stderr inherited; yarc exits with the container's status. `main` now passes exit codes through. Non-Linux hosts get "running containers needs Linux; run yarc inside WSL".

## Tests
- **Unit (33 in `yarc-runtime`, 3 new in `yarc-cli`):** plan order and contents, device binds, `/sys` per network mode, resolv.conf only for host network, hostname files, required/optional steps, user check, `safe_join` (plain, `..`, symlinks at any position, final symlink for files), executable lookup, CLI parsing.
- **Live (10, `crates/yarc-cli/tests/live_run.rs`)**: output streaming, exit status passthrough (7), PID 1 / uid 0 / hostname = short id / Alpine filesystem, env and working dir (incl. created dir), PID namespace (no host processes visible), device nodes, `--network none` shows only `lo`, missing executable, non-root user refused, double Ctrl-C → 137.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
# containers for real (network + user namespaces):
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_run -- --ignored --test-threads=1'
```

### Try it
Inside WSL, from the repo root:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

id=$($yarc create alpine:3.20 sh -c 'echo "pid=$$ uid=$(id -u) host=$(hostname)"; ls /proc | grep -c "^[0-9]"; ps')
$yarc container start $id
# pid=1 uid=0 host=<12 hex digits>, a handful of processes, and `ps` listing only the container's own

id=$($yarc create alpine:3.20 sleep 100); $yarc container start $id    # Ctrl-C twice stops it (exit 137)
id=$($yarc create alpine:3.20 sh -c 'exit 7'); $yarc container start $id; echo $?   # 7
$yarc container start --debug $id          # prints the setup plan first
$yarc container rm $($yarc container ls | tail -n +2 | cut -d' ' -f1)   # clean up
```

## Findings worth remembering
- `kill -9 $$` inside the container does nothing: a namespace's PID 1 can't be killed by signals sent from inside it. Signals from the host (our supervisor) do work.
- WSL's `/etc/resolv.conf` is generated by WSL; copying it in gives working DNS for host-networked containers.

## Not done / follow-ups
- **Capabilities and hardening:** the process has all capabilities *inside* its user namespace. No capability dropping, `no_new_privs`, seccomp or masked `/proc` paths (`/proc/kcore`, …) yet.
- **Single uid only:** images that switch to another user won't run until multi-uid maps (`newuidmap`, `/etc/subuid`) are added; ownership of files from the image is the invoking user.
- No cgroups (limits, accounting): rootless needs systemd delegation; planned after `run`.
- No pty/TTY allocation: stdio is inherited as is (`sh` works when yarc itself is on a terminal; a dedicated pty comes with the shim in M3).
- No `init` reaper: the command is PID 1 (Docker's default); zombies are not reaped unless the command does it.
- A setup failure always exits yarc with 1 (no 126/127 distinction yet).
- Not tested as real root, on macOS, or on kernels/distros with user namespaces disabled (the error message mentions that).
- The live tests pull from Docker Hub each time (per-test temp store).

## Next: M2c — `yarc run`
`yarc run [--rm] [--name N] [-e K=V] [--entrypoint X] [-w DIR] [--network host|none] <image> [cmd...]` = create + start in one command, `--rm` cleanup on exit (also on failure), better error exit codes; cgroups v2 limits only if time allows or deferred to M3.
