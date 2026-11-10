# C4b — Restart supervisor, stop requests, `--restart`

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 104, `yarc-cli` 94 unit, `yarc-agent` 24 and the rest). **10 new live tests** (`live_restart`) pass, and so do the existing live suites `live_run` (30), `live_project` (10), `live_volumes` (9), `live_agent` (2) and `live_remote` (2). Nothing is committed. `restart:` in `yarc.yaml`/compose is C4c; `up` still creates every service with policy `no`.

## What was done
### Runtime (`yarc-runtime`)
- **`supervise`** (`lifecycle.rs`): takes the container's lock **once** and holds it across runs, so the container never looks stopped between runs and `up` cannot start a second supervisor. It loops `run once -> RestartTracker -> wait`. With policy `no` it is exactly one run. `run_recorded` (foreground) is unchanged in behaviour; the "exited" state is now written by its caller, which is what lets the supervisor write "waiting to restart" instead with no flash of `Exited`.
- **First start failure** is returned as an error (reported to the CLI as before, no loop); a start failure on a later run counts as a failed run.
- **Stop requests:** `ContainerStore::{request_stop, stop_requested, clear_stop_request}` (`containers/<id>/stop-requested`). `signal_container` records one for `SIGTERM` and `SIGKILL` **before** signalling, so `stop`, `down`, `project stop`, `rm -f`, Ctrl-C of `up` and `yarc kill` (default and TERM) are all covered in one place. It is only written while the container is supervised; every supervisor clears a stale one at start and consumes it when it acts.
- **Waiting to restart** checks for a stop request every 100 ms (and for the container having been removed), so `stop`/`down` during a 60 s backoff return at once. `stop_container` no longer treats "no process to signal" as "already stopped": it waits for the supervisor to leave.
- **The race** from the design: a stop that lands after the last check but before the next spawn is passed on by TERM to the new init right after it starts.
- **Signals to the shim itself** (`kill -TERM <shim>`, a service manager) count as a stop too, also while it waits to restart. This was not in the design note; without it a SIGTERM during the wait would have killed the supervisor with the state half written. The termination handlers now stay installed for the whole supervision.
- The state records the number of restarts actually performed; during the wait it is `Running` + no pid + `next_restart_ms` (shown as `Restarting (exit 1, in 4s)`).

### CLI / protocol
- `yarc run|create --restart POLICY` (default `no`). `run --restart` without `-d`, and `-d --rm --restart`, are errors before anything is created. A shim also refuses `--rm` for a container that has a policy. A foreground `yarc container start` of such a container runs it once, ignoring the policy.
- The shim calls `supervise`. `YARC_RESTART_BACKOFF_MS` (hidden, for tests) replaces the initial 1 s delay; the 60x cap and 10x reset scale with it.
- `CreateSpec.restart` in the protocol (additive, omitted when unset) and `--restart=` in the agent's command line, placed before the image.
- `ps` lists `Restarting` containers as running; `project ps` has a wider status column.

## Tests
- **Unit:** stop-request recording (only while supervised, hostile ids), only TERM/KILL record one, the wait ends at once on a request or a removal, `stop` of a container between two runs waits for the supervisor, `stop` of an unsupervised one leaves nothing behind, `--restart` parsing and forwarding.
- **Live (`live_restart`):**
  - `always` restarts a crashing container while it shows as running at every sample, a second `start -d` is refused, `stop` keeps it down and consumes the request;
  - `on-failure:2` runs exactly 3 times; `on-failure` leaves `exit 0` alone; `always` restarts a clean exit; the default is `no`;
  - `stop` during a 5 s wait returns in under 3 s and stays down;
  - `kill` and `rm -f` stay down (the removed container does not come back);
  - `kill -s USR1` followed by an exit, and a `kill -9` from outside, both restart;
  - `kill -TERM <shim>` is a stop;
  - a first start failure under `always` reports 127 and never loops;
  - `--restart` needs `-d`, excludes `--rm`, and rejects bad policies without creating anything;
  - the status shows `Running (restarted 1 time)`.
- **The tests can fail:** with the stop request deliberately disabled, exactly the three stay-down tests failed (`always_restarts...`, `kill_and_rm_force...`, `stopping_during_the_wait...`); restored afterwards.

## Known issues / notes
- **`live_network::down_stops_the_holder_closes_the_ports_and_removes_the_network_state` fails, and failed before this work:** it fails identically on the commit I started from (`91aea8d`, checked in a clean worktree). After `down` the host port still accepts connections. Probably WSL2's localhost relay, not yarc; the other 7 tests of that suite pass. Not investigated further.
- A start failure on a *later* run shows `Exited (n)` for a moment before `Restarting`, because the failed start records itself first (the CLI relies on that for the first start).
- A shim killed with SIGKILL still takes its container with it and does not restart anything, as before.

## Next: C4c
`restart:` as a key of `yarc.yaml` and compose (validated, in the definition hash, shown by `project config`, passed by `up`). After it the real PMS/AERO bundles should load for the first time.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_restart -- --ignored --test-threads=1'
```
