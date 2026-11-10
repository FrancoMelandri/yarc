# L1 — limits: model and parsing (done)

Decisions taken: the plan's defaults (systemd scope, fail without systemd, swap 0, a minimal `stats`, memory/swap/cpus/pids only).

## What exists now
- `yarc-config/src/resources.rs`: `Resources { memory, memory_swap, cpus_millis, pids }` (all optional, serialized only when set), the parsers `parse_memory`, `parse_memory_swap`, `parse_cpus`, `parse_pids`, `format_size`, and `ResourceError { field, reason }` so each reader can name the key the way the user wrote it.
  - Sizes: `512m`, `1g`, `1.5g`, `256mb`, `64k`, plain bytes; case-insensitive, powers of 1024; at least 6m; overflow is an error.
  - CPUs: decimal, kept in thousandths, at least 0.001.
  - Pids: positive; `-1` means unlimited (compose spelling).
  - `Resources::check()`: `memory_swap` is memory **plus** swap (Docker semantics), needs a memory limit and cannot be below it. `swap_bytes()` is 0 when a memory limit has no swap setting.
- `Service.resources` filled by:
  - `yarc.yaml`: `resources: {memory, memory_swap, cpus, pids}` (numbers or text).
  - compose: `mem_limit`, `memswap_limit`, `cpus`, `pids_limit`, and `deploy.resources.limits` (`memory`, `cpus`, `pids`). The same limit in both places must agree. `deploy.resources.reservations` (`memory`, `cpus`) is accepted and **ignored silently**; every other `deploy` key, `cpu_shares`, `cpuset`, `mem_reservation`, `ulimits` still fails as an unsupported key. `${VAR:-x}` interpolation works.
  - Errors name the service, the key and the offending token only.
- `ContainerSpec.resources` (+ `with_resources`), absent from `config.json` when unset, so older containers load unchanged.
- `service_hash` covers the limits only when set: existing containers keep their hash, a changed limit recreates the service; `mem_limit: 512m` and `deploy…memory: 512m` hash the same.

## Not in L1 (by design)
- The "cpus <= host CPUs" check needs the host: it belongs to L2/L3, before anything is created.
- `project config` output, flags, protocol: L3.

## Checks
fmt clean, clippy `-D warnings` clean, tests green on Windows and Linux (`cargo test --workspace`).
