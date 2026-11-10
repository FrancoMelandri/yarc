# C4 — Restart policies: design (for review, nothing implemented)

Status: **proposal, waiting for review.** No code yet. Decisions that need your answer are in "Decisions" at the end, each with a recommendation; "defaults" accepts them all.

## Why this is the risky step
Everything before C4 only read files. This changes the **shim**, the one process that supervises a container, holds its lock and records its state. Two facts about the current code drive the design:

1. **The lock is held for exactly one run.** `run_recorded_with` takes the container's `flock` on entry and drops it on return. "Supervised" means "someone holds that lock". If the shim simply called it in a loop, the lock would be free between runs, `up` would see a stopped container and start a second shim. The lock must be held across restarts.
2. **A manual stop is invisible to the shim.** `stop_container` sends SIGTERM straight to the container's *init* (pid from `state.json`), not to the shim. The shim just sees an exit code. Without a signal from `stop`, it cannot tell `yarc stop` from a crash, and would restart what you just stopped.

## Behaviour

### Policies
`restart: no` (default) · `always` · `unless-stopped` · `on-failure` · `on-failure:N`.

| exit | `no` | `on-failure[:N]` | `always` / `unless-stopped` |
|---|---|---|---|
| 0 | stop | stop | restart |
| non-zero (including killed by a signal, e.g. 137) | stop | restart (up to N times) | restart |
| manual stop (see below) | stop | stop | stop |

A policy only applies once the container has started successfully. If the **first** start fails (no such program, refused), `up` / `run -d` still fails immediately with the real message and exit code, exactly as today; no restart loop on a typo. A start failure *after* a successful first run counts as a failed run and is retried with backoff.

Health checks do not restart anything (`unhealthy` is reported only), same as Docker.

### What counts as a manual stop
A **stop request** is recorded by the command that causes it, before the signal is sent:
- `yarc stop`, `yarc project stop`, `yarc down`, Ctrl-C of a foreground `up` (they all go through `stop_container`);
- `yarc rm -f`;
- `yarc kill` with `KILL` or `TERM` (the default is `KILL`). Other signals (`USR1`, `HUP`, ...) do not count: they are how you poke a process, and a crash after one must still restart.
- `up` recreating a service whose definition changed (it stops it first).

It is a file `containers/<id>/stop-requested`, created by the command only if the container is currently supervised (so a no-op `stop` of a stopped container leaves nothing behind). The supervisor deletes any stale one at its start, and consumes it when it acts on it.

### Backoff
Restarts are delayed so a crash loop cannot spin: **1 s, then ×2 each time, capped at 60 s**; the delay resets to 1 s once a run lasted **10 s or more**. The wait is interruptible: the supervisor looks at the stop request every 100 ms, so `down` during a backoff returns at once instead of waiting 60 s.

`on-failure:N` counts restarts since the container was last started by a person (`up`, `start`, `run`); the counter is **not** reset by the 10 s rule (only the delay is). N restarts are allowed, so the container runs at most N + 1 times.

### `always` and `unless-stopped` are identical
They differ only after a daemon/host restart, and yarc has neither: the shim dies with its WSL session or reboot, and nothing revives it. After a reboot the containers are simply `Dead`/stopped until the next `yarc up`, which starts them all anyway. Both names are accepted (compose files use them) and behave the same; this is documented, not hidden. A boot-time supervisor (a systemd user unit that runs `yarc up`) is a separate, later feature.

### Where it shows
- `yarc ps` / `project ps` status: `Running (restarted 3 times)` and, during a backoff, `Restarting (exit 1, in 4s)`. `Exited (n)` once the policy gives up or a stop request ends it.
- The container log is continuous across restarts (same file, appended). The supervisor writes one line per restart to `shim.log` (`restart 3 after exit 1, waited 4s`), not into the container log.
- `yarc project config` / `project ps` show the policy.

## Implementation plan

### State and spec (small, additive)
- `ContainerSpec.restart: RestartPolicy`, serialized only when not `no` (old `config.json` files load unchanged). It is part of the service definition hash **only when set**, so existing containers are not recreated.
- `ContainerState` gains `restarts: u32` (default 0) and `next_restart_ms: Option<u64>` (set only during a backoff). No new `Status`: a restarting container is `Running` + supervised + no init pid, so `up`'s "up to date" and `needs: started` logic stay correct. `describe()` renders the two new texts.

### Runtime (`yarc-runtime`)
- **`restart.rs`, pure:** `RestartPolicy` (parse/format `always`, `on-failure:3`, ...), and one function `decide(policy, exit_code, restarts_so_far, ran_for, stop_requested) -> Stop | RestartAfter(Duration)`, plus the backoff arithmetic. This is where nearly all the logic lives and where the bulk of the unit tests go.
- **Lock held across runs:** split `run_recorded_with` into "take lock" and "run once with a lock I already hold". `supervise(store, spec, launch, on_started)` takes the lock once, loops `run once -> decide -> wait`, and releases it only when it finally stops. `run_recorded` (foreground) keeps working unchanged: policy `no` is a loop of one.
- **Stop requests:** `request_stop(store, id)` writes the marker if supervised; `stop_container`, `remove_container(force)` and `signal_container` (for `TERM`/`KILL`, from `yarc kill`) call it first.
- **Closing the race:** if a stop request lands after the supervisor's last check but before it spawns the next run, the supervisor re-checks right after the spawn and, if set, sends TERM to the new init itself (the stop command's own SIGKILL escalation still applies).
- **`stop` during a backoff:** there is no init pid then. `stop_container` writes the request and waits for the lock to be released, which the supervisor does within 100 ms; `signal_container` reports "restarting" rather than "not running".

### CLI / shim (`yarc-cli`)
- `detached::shim` calls `supervise` instead of `run_recorded_with`. `--rm` and a restart policy are mutually exclusive (error), as in Docker. A foreground `yarc run --restart` is an error ("needs -d"): only the shim supervises across restarts.
- `yarc run|create --restart POLICY` (and `--restart` in the protocol's `CreateSpec`, an additive field). This is not strictly needed for compose, but it is how the live tests can exercise restarts without a project file, and costs one flag.
- `restart:` in the model (`Service.restart`), read from **both** `yarc.yaml` and compose; `up` passes it into the container spec.

### Test hook
A hidden env var, `YARC_RESTART_BACKOFF_MS`, overrides the 1 s initial delay (cap and reset rule scale with it) so live tests take milliseconds. Undocumented for users.

## Steps (each ends with tests, a progress doc, then stop for your go)
- **C4a runtime core:** `RestartPolicy`, `decide`, backoff, state fields, `describe`. Pure, fully unit tested. No behaviour change yet.
- **C4b supervise + stop requests + shim + `--restart` flag:** lock held across runs, markers, `ps` text. Live tests (below).
- **C4c config:** `restart:` in `yarc.yaml` and compose, hash, `project config`, `up`. After this the real bundles load for the first time (everything else is already supported).

## Tests
- **Unit (C4a):** policy parsing and display (`on-failure:0`, `:-1`, `unless-stopped`, junk are errors/edge cases); the whole `decide` table above; backoff 1,2,4,…,60,60 and its reset; `on-failure:N` allows exactly N restarts; stop request beats every policy; state serialization of old files without the new fields.
- **Live (C4b), with a 20 ms backoff:**
  - `always` + `sh -c 'exit 1'`: `restarts` climbs, log shows each run, one shim throughout (same lock, `ps` never shows it stopped).
  - `on-failure:2` + `exit 1`: runs 3 times then `Exited (1)`; `on-failure` + `exit 0`: runs once.
  - `always` + `exit 0`: restarts; `no`: does not.
  - `yarc stop` of a running `always` container stays stopped; `stop` **during a backoff** returns promptly and stays stopped; `kill` (KILL) stays stopped; `kill -s USR1`-then-crash still restarts.
  - `kill -9` of the container's init from outside (a real crash) restarts it.
  - `up -d` twice does not create a second supervisor; changing the definition recreates cleanly; `down` and Ctrl-C of a foreground `up` leave nothing running and nothing restarting.
  - A first-start failure under `always` still reports the real error and does not loop.
  - `--rm` + `--restart` is rejected.
- **Offline (C4c):** compose and `yarc.yaml` parse the keys, reject bad values (`restart: sometimes`, `on-failure:x`), and the hash changes when the policy does.

## Risks
- **Orphaned restart loops.** If a stop request is lost (a bug in the marker path), a container could not be stopped. Mitigation: `stop` still escalates to SIGKILL of the init, and `rm -f` and `down` remove the container files, which ends the supervisor's next loop (it re-reads state each round and exits if the container is gone). Covered by a live test.
- **Time and clocks.** Backoff uses a monotonic clock; `next_restart_ms` is only for display.
- **Windows/WSL.** Nothing changes in the agent: restarts happen entirely inside the WSL shim. A WSL shutdown still kills every shim, as today.

## Decisions (recommendation first)
1. **`always` = `unless-stopped`** in practice, because yarc has no boot-time supervisor (recommended: accept and document), or implement a persistent "was stopped" flag now even though nothing can act on it.
2. **Backoff:** 1 s ×2 up to 60 s, reset after a 10 s run (recommended). Docker starts at 100 ms; that spams a crash loop.
3. **`on-failure:N`:** counts all restarts since a person last started it (recommended, Docker-like), or counts only consecutive quick failures.
4. **`kill`:** `KILL` and `TERM` count as manual stops, other signals do not (recommended), or no kill ever counts, so `kill` can be used to test crash recovery.
5. **Extra surface:** add `--restart` to `run`/`create` and `restart:` to `yarc.yaml` as well as compose (recommended), or compose only.
6. **Split into C4a/b/c** as above (recommended), or one step.

"defaults" = all the recommendations.
