# L3 — limits: the user surface (done)

## What exists now
- `run` / `create`: `-m, --memory SIZE`, `--memory-swap SIZE` (memory plus swap), `--cpus N`, `--pids-limit N`. Values are validated by the parser, so a bad one fails before anything happens (`invalid value '1m' for '--memory <SIZE>': '1m' is too small, the minimum is 6m`); swap without memory, or below it, is rejected too. `--pids-limit -1` is refused ("omit the flag for no limit").
- Windows client: the limits are validated on Windows, forwarded to the agent as text (`CreateSpec.memory`, `memory_swap`, `cpus`, `pids_limit`, absent when unset) and turned back into flags by `argv.rs`. A bad combination is reported before the agent is called.
- `create_new` / `NewContainer.resources` -> `ContainerSpec`; `project up` passes each service's limits (compose `mem_limit`/`cpus`/`deploy…`, `yarc.yaml` `resources`).
- `project config` prints `limits memory 64m, cpus 0.25` per service that has any.
- `yarc stats [ID…]` (Linux; forwarded from Windows as `Command::Stats`): one-shot table `CONTAINER ID  CPU %  MEM USAGE / LIMIT  MEM %  PIDS` for running containers with limits. CPU is the share of one CPU over a 1 s sample. Without ids it lists every running container that has limits; with ids it says on stderr why one is skipped (no limits / not running).

## Checked live (WSL, and the Windows client against the WSL agent)
- `run -d -m 64m --cpus 0.5 --pids-limit 100` with two busy loops: `stats` shows 50.0% CPU, 1.9 MiB / 64.0 MiB, 4 pids.
- `run --rm -m 64m` with a 200 MB allocation: exit 137.
- Compose with `mem_limit: 64m`, `cpus: 0.25` and `deploy.resources.limits {memory: 32m, pids: 50}`: `up -d` starts both, `stats` shows the 64 MiB and 32 MiB limits.
- From `yarc.exe`: `run -d -m 64m --cpus 1`, `stats`, and the two error cases above behave the same.
- A container without limits is untouched; `stats` names it as having nothing to measure.

## Notes
- `deploy.resources.reservations` is still accepted and ignored without a note (see L1).
- A container without limits cannot be measured: its supervisor shares a cgroup with unrelated processes.
- Next: L4 (live tests under `-- --ignored`), then L5 (real bundles, the JVM question from L2, `design.md`, README).
