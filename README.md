# yarc — Yet Another Runtime Container

Run OCI/Docker images from a simple `yarc.yaml` or a Docker Compose file, with no
Docker installed. Written in Rust. Targets Linux (native), Windows (WSL2) and macOS
(Virtualization.framework, best-effort).

Status: early design. See [docs/design.md](docs/design.md).

## CPU and memory limits

```
yarc run -d -m 512m --cpus 0.5 --pids-limit 200 alpine sleep 300
yarc stats
```

Compose files take `mem_limit`, `memswap_limit`, `cpus`, `pids_limit` and `deploy.resources.limits`; `yarc.yaml` takes `resources: {memory, memory_swap, cpus, pids}`. A container over its memory limit is killed (exit 137, `ps` says `OOM killed`); with a memory limit there is no swap unless `--memory-swap` allows it. Limits need systemd for the user inside WSL (`[boot] systemd=true` in `/etc/wsl.conf`); without it a container with limits does not start. Details: [docs/design.md](docs/design.md#cpu-and-memory-limits).
