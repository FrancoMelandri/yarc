# yarc design

## Goals
- Run OCI images as multi-service projects, like docker compose, **without Docker**.
- Learning-first: own the runtime (namespaces, cgroups, overlayfs), image pull and networking.
- Platforms: Linux (native), Windows (WSL2), macOS (best-effort, Virtualization.framework).
- A simple native `yarc.yaml`, and **a practical subset of Docker Compose files read directly** (see "Compose files" below).
- Language: Rust.

## Non-goals (v1)
- `build:` / Dockerfile / BuildKit.
- Full compose-spec compatibility: unsupported keys (`build`, `networks`, most of `deploy`, ...) are errors, never silently ignored.
- Rootless mode (planned later).
- Swarm/Kubernetes-like orchestration.

## Architecture

```
 Host (Win / macOS / Linux)                 Linux (native, or VM on Win/macOS)
┌────────────────────────┐                 ┌─────────────────────────────────┐
│ yarc CLI               │   vsock / unix  │ yarc-agent                      │
│  - parse yarc.yaml     │ ──────────────► │  - image puller (OCI registry)  │
│  - plan up/down        │                 │  - layer store + rootfs         │
│  - VM manager (W/M)    │                 │  - container runtime            │
└────────────────────────┘                 │  - networking (bridge, DNS)     │
                                           └─────────────────────────────────┘
```

The Linux code is written once. Platform-specific code is only "how do I get a Linux box":
- Linux: run directly.
- Windows: `wsl --import` of a minimal yarc distro.
- macOS: Virtualization.framework VM with kernel + rootfs containing the agent.

### Process model (Podman-like, daemonless)
No central long-lived daemon. Each container is supervised by a small per-container
**shim** process (`yarc-agent shim`, conmon-style) that:
- forks/execs the container init, holds its pty/log pipes, writes logs to disk,
- records the exit code,
- keeps running after the CLI exits.

State lives on disk (`/var/lib/yarc`, or `$XDG_DATA_HOME/yarc`): one directory per
container with `state.json`, logs and a control socket owned by the shim.
CLI commands read state from disk and talk to shims directly. File locks guard
project-level operations.

On Windows/macOS the VM is the only long-lived piece; inside it the same
shim model applies. The host CLI invokes the in-VM agent per command.

## Crates

| crate | role |
|---|---|
| `yarc-cli` | host binary: arg parsing, project planning, VM management |
| `yarc-agent` | Linux-side binary: executes plans, hosts shims |
| `yarc-config` | `yarc.yaml` model + validation |
| `yarc-oci` | registry v2 client, blob store, layer unpack, rootfs |
| `yarc-runtime` | `Runtime` trait + in-house implementation |
| `yarc-proto` | CLI ↔ agent message types |

## Key technical decisions

- **Runtime**: hand-written using `nix` (clone/unshare for pid, mount, uts, ipc, net, user
  namespaces; `pivot_root`; cgroups v2; capability dropping). Behind the `Runtime` trait so
  youki's `libcontainer` can be compared/swapped later.
- **Images**: registry v2 over `reqwest`, token auth (Docker Hub), multi-arch index
  resolution, digest verification, tar+gzip layers with whiteouts, content-addressed store.
- **Filesystem**: overlayfs (layers = lowerdirs, per-container upperdir).
- **Networking** (decided at M4): rootless, so one shared user+network namespace per project
  (like a podman pod): services reach each other on `localhost`, service names resolve to
  `127.0.0.1` through `/etc/hosts`, published ports go through a userspace TCP proxy
  (bound to `127.0.0.1` by default); outbound traffic goes through `pasta` (package `passt`) when installed (P3; IPv4) and DNS is forwarded to the host resolver (P5).
  The original idea (per-project bridge, veth pairs, embedded DNS, NAT) needs real root and moves
  to the rootful agent.
- **Privileges** (decided at M2): rootless first (user namespaces, a single uid mapped to 0);
  rootful support, multi-uid maps and the agent running as root come later.
- **Process model** (M3): no daemon. `yarc run -d` starts one shim (`yarc __shim <id>`, own session)
  per container; the shim supervises it, holds its `flock` lock (the lock, not a pid, defines
  "running") and records `state.json` and the JSON-lines log under `<root>/containers/<id>/`.
  Control is by signals to the container's init plus those files.
- **Platform**: arm64 images on Apple Silicon; virtiofs for bind mounts on macOS.

## `yarc.yaml` sketch

```yaml
services:
  db:
    image: postgres:16
    env: { POSTGRES_PASSWORD: secret }
    volumes: [ "pgdata:/var/lib/postgresql/data" ]
    health: { cmd: "pg_isready", interval: 5s }
  api:
    image: myorg/api:1.2
    ports: [ "8080:80" ]
    needs: { db: healthy }
```

`user` (added for the PMS bundle) replaces the image's `USER`; only `root` works until multi-uid support exists.

Principles: short keys, one way to write each thing, `needs` replaces `depends_on` conditions.
`cmd` (a list of arguments, added in M4a) replaces the image's default command.
`resources: { memory: 512m, memory_swap: 1g, cpus: 0.5, pids: 200 }` sets the container's limits (see "CPU and memory limits").

## CLI (v1)
Images: `pull`, `load`, `image ls|rm|unpack`.
Containers: `run`, `create` (with `--memory`, `--memory-swap`, `--cpus`, `--pids-limit`), `start`, `stop`, `kill`, `wait`, `ps`, `stats`, `logs`, `rm`, `pull`.
Projects: `up`, `down`, `project ps|logs|stop|config`; global `--file` / `--project`.
Also `doctor`. Still to come: `restart`. Hidden: `agent --stdio` (serves the `yarc-proto` protocol).

## Milestones
1. **M0** workspace + `yarc.yaml` parser. ✔
2. **M1** pull and unpack an image (any OS). ✔
3. **M2** run one container on native Linux, rootless (`yarc run alpine echo hi`). ✔
4. **M3** detached containers: shim, state, logs, `ps/logs/stop/kill/wait/rm`. ✔
5. **M4** projects: `up/down/project ps|logs|stop|config` (a ✔), exec + healthchecks (b ✔),
   project network, names and published ports (c ✔), volumes (d ✔).
6. **M5** the agent and Windows via WSL2 (see `docs/progress/M5-plan.md`): shared image cache for live tests (d ✔),
   `yarc-proto` (a1 ✔), stdio agent running the CLI as child processes (a3 ✔; a2, an output sink, was dropped), Windows client (a4 ✔, see M5a4 for what is still unverified on real Windows), ports and signals (b ✔, Ctrl-C and ports checked on Windows by the user), setup (c, postponed: see `docs/progress/M5c-setup-plan.md`).
7. **M6** macOS via Virtualization.framework.

## Dev environment
Windows host; everything up to M4 is developed and tested inside WSL2 (Ubuntu-24.04), with
`CARGO_TARGET_DIR=~/yarc-target`. Progress notes per step are in `docs/progress/`.

## Open questions
- Control protocol between CLI and agent/shims: JSON over unix socket vs gRPC.
- Volume model: named volumes as plain dirs vs loopback images.
- Log format and rotation.

## Compose files
`yarc up` reads a Docker Compose file as is (`compose.yaml`, `docker-compose.yml`, ... are found in the current directory when there is no `yarc.yaml`; `-f FILE` / `--file FILE` names one; any file not called `yarc.*` is read as compose). Details and decisions: `docs/progress/C-compose-plan.md`.

- **Supported:** `image`, `command`, `entrypoint`, `environment`, `ports` (published on 127.0.0.1), `volumes` (short form), top-level `volumes`, `depends_on` (list or with `service_started` / `service_healthy`), `user`, `working_dir`, `healthcheck`, `container_name` (an extra name on the project network), file `secrets` (read-only at `/run/secrets/<name>`), `profiles`, `stop_grace_period`, `restart`, and the limits `mem_limit`, `memswap_limit`, `cpus`, `pids_limit`, `deploy.resources.limits` (see "CPU and memory limits").
- **Variables:** `${VAR}`, `${VAR:-d}`, `${VAR:?msg}` and the other forms; values from the process environment, then the `.env` next to the file, or the `--env-file`s instead of it (repeatable, later wins).
- **Profiles:** `--profile NAME` (repeatable) or `COMPOSE_PROFILES`.
- **`x-yarc:`** per service holds yarc-only settings; today `user: root`, needed for images that declare a non-root `USER` because the rootless runtime only maps uid 0. Docker ignores the block.
- **Restart policies** (`no`, `always`, `unless-stopped`, `on-failure[:N]`) are carried out by the container's shim; `stop`, `down`, `rm -f` and `kill` are never undone. `always` and `unless-stopped` behave the same because there is no boot-time supervisor.

## CPU and memory limits
Per container: memory, memory plus swap, CPUs and number of processes (`--memory`/`-m`, `--memory-swap`, `--cpus`, `--pids-limit` on `run`/`create`; `mem_limit`, `memswap_limit`, `cpus`, `pids_limit` and `deploy.resources.limits` in compose; `resources: {memory, memory_swap, cpus, pids}` in `yarc.yaml`). Plan, measurements and decisions: `docs/progress/L-limits-plan.md`; steps `L1`-`L5` in `docs/progress/`.

- **Mechanism.** A rootless process cannot write cgroup files from where yarc runs (the `wsl.exe` session's cgroup is not delegated, and `cpu` is not enabled in the user's `app.slice`), but it can ask the user's systemd for a transient scope. A container **with** limits has its shim (or, in the foreground, `container start`) run as `systemd-run --user --scope --collect -p MemoryMax=… -p MemorySwapMax=… -p CPUQuota=…% -p TasksMax=… -p Delegate=yes`; everything it forks, the container included, stays in that scope. A container without limits is started exactly as before. The scope lives as long as the shim, so limits survive restart-policy loops, and `--collect` leaves nothing behind after `stop`, `down` or `rm -f`.
- **Memory.** Exceeding the limit is an OOM kill (exit 137). With a memory limit and no `--memory-swap`, swap is 0, so the outcome is predictable (Docker would allow twice the limit in total; WSL has swap on by default). `--memory-swap` is memory **plus** swap, like Docker. The container's init raises its `oom_score_adj`, so the kernel picks it rather than the shim, which records the outcome: the `oom_kill` counter of the scope is compared before and after each run, and `ps` shows `Exited (137) OOM killed` or `Restarting (exit 137, OOM killed, in 4s)`.
- **No systemd, no limits.** If `systemd-run --user` does not work, a container with limits **fails to start** and says how to enable systemd (`/etc/wsl.conf`, `[boot] systemd=true`, `wsl --shutdown`); a limit that is silently not enforced would be worse. Containers without limits never need it. A `cpus` above the machine's CPU count is refused before anything is created.
- **Validation** is shared by every reader (`yarc-config::resources`): sizes in `b/k/m/g/t` (binary, case-insensitive, fractions allowed), memory at least 6m, `cpus` at least 0.01, a swap limit needs a memory limit and cannot be below it. Errors name the key and the offending token, never more.
- **Definition hash.** Limits take part in it only when set, so existing containers keep their hash and changing a limit makes `up` recreate the service (there is no live update).
- **`yarc stats [ID…]`** shows memory used and limit, CPU % over a 1 s sample and processes for running containers with limits, read from the scope's cgroup files. A container without limits has no cgroup of its own, so there is nothing to measure.
- **Containers see the limits.** `/proc/self/cgroup` inside a container names its scope and `/sys/fs/cgroup` is the host hierarchy, so a JVM (checked with Temurin 21 and with the PMS image) finds `memory.max` and `cpu.max` itself and sizes its heap from them: with `-m 768m --cpus 1` the PMS reports a 768 MiB limit and one CPU. The container does not get a cgroup namespace of its own, so tools that read `/sys/fs/cgroup/memory.max` directly (not through `/proc/self/cgroup`) see the host root and report no limit.

What is not covered: `cpu_shares`, `cpuset`, `cpu_period`, `ulimits`, `shm_size`, `mem_reservation`, block I/O limits and live updates (all still unsupported keys or absent); `deploy.resources.reservations` is accepted and ignored (nothing to reserve), without a note. Page cache and in-container `tmpfs` count against the memory limit, as in Docker. The limits apply inside the WSL VM, whose own size is `.wslconfig`'s business. cgroup v1 only is unsupported (WSL2's kernel is v2).
