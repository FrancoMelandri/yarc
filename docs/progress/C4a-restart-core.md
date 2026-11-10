# C4a — Restart policies: runtime core

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 96, up from 81). **No behaviour change**: nothing reads the new types yet; supervising, stop requests and the CLI are C4b, the `restart:` key is C4c. Design and decisions: `C4-restart-design.md` (all defaults accepted).

## What was done (`yarc-runtime`)
- **`restart.rs`** (pure, not Linux-gated, 12 unit tests):
  - `RestartPolicy`: `no` | `always` | `unless-stopped` | `on-failure` | `on-failure:N`; parse/print and (de)serialize as that text. `on-failure:0`, a negative or non-numeric N, and any other spelling are errors naming the policy and the reason.
  - `Backoff`: 1 s, doubling, capped at 60 s, reset by a run of 10 s or more. `Backoff::with_initial(d)` scales all three (cap 60x, reset 10x), which is how the live tests of C4b will make restarts take milliseconds.
  - `RestartTracker::on_exit(RunEnd)` -> `Decision::{Stop, Restart{after}}`: the whole decision table. A stop request beats every policy; `no` never restarts; `always`/`unless-stopped` restart on any exit (a clean exit also backs off, so `exit 0` cannot spin); `on-failure` only after a non-zero exit; `on-failure:N` allows exactly N restarts in total (long runs do not grant more). Counters saturate instead of overflowing.
- **State** (`state.json`): `restarts` and `next_restart_ms`, both optional and omitted when unused, so old files load and containers that never restart keep the old shape. `restarting(exit, finished, next)` is the "waiting to restart" state (status `running`, no init pid); `exited()` clears the wait and keeps the count; `with_restarts(n)` carries the count into the next run.
- **`describe`** (`ps` text): `Running (restarted 3 times)`, `Running (healthy, restarted 2 times)`, `Restarting (exit 1, in 4s)` (rounded up; `Restarting (exit 1)` once due), `Dead` when nobody supervises it. `describe_at` takes the time, for tests.
- **Spec**: `ContainerSpec.restart` (`with_restart`), stored in `config.json` only when set; old files load as `no`.

## Tests
Every row of the design's table, the backoff sequence (1, 2, 4, 8, 16, 32, 60, 60...), the reset at exactly 10 s (and 9.999 s not resetting), `on-failure:N` counts, overflow safety, bad policy texts, JSON round trips, old-file compatibility, and the new `ps` texts.

## Next: C4b
`supervise` (lock held across runs), stop requests (`stop`, `down`, `rm -f`, `kill` KILL/TERM), shim wiring, `--restart` on `run`/`create`, `YARC_RESTART_BACKOFF_MS` test hook, and the live tests.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```
