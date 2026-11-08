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
- **Networking**: per-project bridge, veth pairs, embedded DNS (service name → IP), port publishing;
  forwarded host-side when running in a VM.
- **Privileges**: rootful first (root in the VM on Win/macOS; sudo on Linux).
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

Principles: short keys, one way to write each thing, `needs` replaces `depends_on` conditions.

## CLI (v1)
`up`, `down`, `ps`, `logs`, `exec`, `pull`, `restart`, `run`.

## Milestones
1. **M0** workspace + `yarc.yaml` parser.
2. **M1** pull and unpack an image (any OS).
3. **M2** run one container on native Linux (`yarc run alpine echo hi`).
4. **M3** multi-service `up/down/ps/logs`: shim, networking, DNS, ports, volumes, health.
5. **M4** Windows via WSL2.
6. **M5** macOS via Virtualization.framework.

## Dev environment
Windows host; M1–M3 are developed and tested inside WSL2 (Ubuntu-24.04).
Rust is not yet installed on this machine (neither Windows nor WSL).

## Open questions
- Control protocol between CLI and agent/shims: JSON over unix socket vs gRPC.
- Volume model: named volumes as plain dirs vs loopback images.
- Log format and rotation.
