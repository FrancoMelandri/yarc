# L5 — limits: end to end and docs (done)

**The limits feature (L1-L5) is complete.** fmt, clippy `-D warnings` and `cargo test --workspace` are clean on Windows and Linux; the live suite `live_limits` (11 tests) and the earlier live suites pass. Nothing is committed.

## The JVM question from L2: answered, no change needed
Inside a container `/sys/fs/cgroup` is the host hierarchy (the container has no cgroup namespace), so reading `/sys/fs/cgroup/memory.max` directly gives `max`. A JVM does not do that: it finds its cgroup through `/proc/self/cgroup` (which names the scope) and reads the limits there. Measured:

| | Temurin 21 JRE | PMS image (`ssw-platform-pms:0.0.4`) |
|---|---|---|
| `-m 256m --cpus 1.5` / `-m 768m --cpus 1` | `Provider: cgroupv2`, `Memory Limit: 256.00M`, `CPU Quota: 150000us`, max heap 126 MiB | `Memory Limit: 768.00M`, `CPU Quota: 100000us`, `Effective CPU Count: 1`, max heap 576 MiB |
| no limits | max heap 3.9 GiB (a quarter of the machine) | |

So `-XX:+UseContainerSupport` works as on Docker. A cgroup namespace per container (what Docker does) was not added: it would also hide the host hierarchy, but it changes the mount setup of every container for no gain the JVM needs. Tools that read `/sys/fs/cgroup/memory.max` directly would still report no limit.

New live test: `a_jvm_sizes_itself_from_the_limits` (Temurin 21 JRE, ~70 MB, pulled once into the test image cache; `common::seeded_root_with`).

## Real bundle (Linux/WSL, a throwaway copy and store, both deleted)
The inspector bundle's `docker-compose.yml` with `mem_limit: 768m, cpus: 1, pids_limit: 500` added to the PMS and `mem_limit: 256m, cpus: 0.5` to the inspector, `images.tar` loaded into a fresh store:
- `project config` lists the limits; `up -d` starts both services (the observability profile stays off).
- `stats` after 10 s: PMS 336.6 MiB / 768.0 MiB (43.8%), 41 pids; inspector 27.8 MiB / 256.0 MiB.
- `down` removes the containers and the network.
The AERO bundle was not repeated: it differs only in services, and the same compose keys apply. Nothing from `.env` or `secrets/` was printed.

## Fixed on the way
`stats` columns now fit long container names (they had overflowed a fixed width).

## Docs
`docs/design.md`: new section "CPU and memory limits" (mechanism, memory/swap/OOM behaviour, no-systemd rule, validation, hash, stats, what containers see, what is not covered); CLI list, yarc.yaml sketch, compose list and non-goals updated. `README.md`: short usage section.

## Not covered / worth knowing
- `deploy.resources.reservations` is accepted and ignored without a note.
- `cpu_shares`, `cpuset`, `ulimits`, `mem_reservation`, block I/O and live updates remain unsupported.
- No Windows-side live run of the bundle with limits (the client forwards and validates the flags; checked with `run -d -m … --cpus …` and `stats` in L3).
- The live suites are not part of CI.
