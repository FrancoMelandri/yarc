# yarc design

## Goals
- Run OCI images as multi-service projects, like docker compose, **without Docker**.
- Learning-first: own the runtime (namespaces, cgroups, overlayfs), image pull and networking.
- Platforms: Linux (native), Windows (WSL2), macOS (best-effort, Virtualization.framework).
- Simple, opinionated `yarc.yaml` (not compose-spec compatible).
- Language: Rust.

## Non-goals (v1)
- `build:` / Dockerfile / BuildKit.
- Compose-spec compatibility.
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

## CLI (v1)
Images: `pull`, `load`, `image ls|rm|unpack`.
Containers: `run`, `create`, `start`, `stop`, `kill`, `wait`, `ps`, `logs`, `rm`, `pull`.
Projects: `up`, `down`, `project ps|logs|stop|config`; global `--file` / `--project`.
Still to come: `restart`. Hidden: `agent --stdio` (serves the `yarc-proto` protocol).

## Milestones
1. **M0** workspace + `yarc.yaml` parser. ✔
2. **M1** pull and unpack an image (any OS). ✔
3. **M2** run one container on native Linux, rootless (`yarc run alpine echo hi`). ✔
4. **M3** detached containers: shim, state, logs, `ps/logs/stop/kill/wait/rm`. ✔
5. **M4** projects: `up/down/project ps|logs|stop|config` (a ✔), exec + healthchecks (b ✔),
   project network, names and published ports (c ✔), volumes (d ✔).
6. **M5** the agent and Windows via WSL2 (see `docs/progress/M5-plan.md`): shared image cache for live tests (d ✔),
   `yarc-proto` (a1 ✔), stdio agent running the CLI as child processes (a3 ✔; a2, an output sink, was dropped), Windows client (a4), ports and signals (b), setup (c).
7. **M6** macOS via Virtualization.framework.

## Dev environment
Windows host; everything up to M4 is developed and tested inside WSL2 (Ubuntu-24.04), with
`CARGO_TARGET_DIR=~/yarc-target`. Progress notes per step are in `docs/progress/`.

## Open questions
- Control protocol between CLI and agent/shims: JSON over unix socket vs gRPC.
- Volume model: named volumes as plain dirs vs loopback images.
- Log format and rotation.
