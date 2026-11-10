# C5 — Compose end to end: real bundles, `--env-file`, `-f`

Status: **done. Compose support is complete.** Workspace tests pass with no warnings; live suites `live_project` (10), `live_restart` (13) and `live_run` (30) pass after the last change. Nothing is committed.

## What was done
- **`--env-file FILE`** (global, repeatable). Like Docker Compose, giving one **replaces** the `.env` next to the project file (the plan had said "layered on top"; replacing is what compose does, so the same command line means the same thing in both tools). Later files win over earlier ones, the process environment wins over all files, a missing file is an error, and errors name the file and line but never a value. Forwarded to the agent (`Globals.env_files`, additive) with Windows paths translated.
- **`-f FILE`** names the project file, before the subcommand only (`yarc -f docker-compose.yml up`). It is a separate argument from the global `--file`, which is why `logs -f` (follow) keeps working after the subcommand; `up -f x` is an error, as in Docker. One file only (repeating `-f` is an error). The Windows client resolves and forwards it.
- **`project ps`** columns now fit their content (long service names no longer break the table).
- README and `docs/design.md` no longer say compose is a non-goal; a "Compose files" section lists what is supported.

## Verified on the real bundles (Linux/WSL, a throwaway store, nothing of yours touched)
- **Inspector bundle**, from its shipped `docker-compose.yml`: both services start; the PMS prints that it is connecting to the FIAS server from `.env`; the secret is mounted read-only (a write fails with "Read-only file system"); `ssw-platform-pms-inspect` and the `container_name` aliases resolve inside the project network; interpolated values arrive (`PMS_FIAS_SERVER_IP_PORT=5009` default, dispatcher URI); a `kill -9` of the PMS from outside restarts it (`Running (restarted 1 time)`); `down` takes 0.5 s.
- **AERO bundle** with `--profile observability`, all three services: the Aspire dashboard answers on `127.0.0.1:18888` (HTTP 302) and the port closes after `down`; AERO's Javalin server starts, the CA certificate and the bearer-token file are mounted, the PMS resolves `ssw-platform-pms-aero`; `down` takes 0.9 s. (No request was sent to the AERO cloud.)
- **Not verified:** a Windows run (`yarc.exe` -> WSL) of these bundles, and the PMS actually talking to a real PMS (10.140.10.177 is not reachable from here).

## Findings that need your files
- **Every service whose image declares a non-root `USER` needs `x-yarc: { user: root }`.** Seen on the real images: `ssw-platform-pms`, `ssw-platform-pms-aero`, `ssw-platform-pms-inspect` (as you found) **and** `dotnet-aspire-dashboard` (user 1654). Your aero compose file has none yet; the inspector one has it on the PMS. I tested the aero bundle on a temporary copy with the block added to all three services (the copy, which held your `.env` and secrets, was deleted).
- Without it `up` stops at the first such service with `cannot start container: user '...': rootless containers can only run as root`. Services after it are not created (the dependency order is alphabetical among independent ones, so a failing dashboard blocks the PMS).

## Housekeeping
- The mutation run in C4b left six shims in `always` crash loops in `/tmp` test stores (failed live tests do not clean up). Stopped and removed; no shims of any kind are left.
- Hand-written `yarc.yaml` of the bundles: already renamed `_yarc.yaml` by you; `up` no longer needs it.

## Tests added
`--env-file` (replaces `.env`, later wins, process env wins, missing file, broken file without leaking contents), `-f` before the subcommand and `logs -f` after it, CLI parsing of both, forwarding with Windows path translation, agent command line.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```

## Follow-ups found by running the inspector bundle for real
- **Skipped services are now announced.** A service in an inactive profile used to be dropped silently; `up` prints `note: dotnet-aspire-dashboard: not started, it belongs to profile observability (use --profile observability to include it)` and `project config` lists it as `not included:`.
- **`COMPOSE_PROFILES` is read from the env files too**, as Docker Compose does. The bundles' `Install-App.ps1` turns observability on by writing `COMPOSE_PROFILES=observability`, `PMS_OTEL_SDK_DISABLED=false` and `PMS_OTEL_ENDPOINT=http://dotnet-aspire-dashboard:18890` into `.env`, and yarc previously ignored the first of them. Precedence: `--profile` flags, then `COMPOSE_PROFILES` in the process environment (set but empty means none), then the env files. Tested through the real binary.
- **"Telemetry is not configured" in the Aspire dashboard** was a `.env` without those three lines: with the compose defaults the services export nothing. Not a yarc fault.
- **Not verified:** that telemetry actually arrives in the dashboard. On a throwaway copy with the three lines the dashboard started with no flag and the services received `OTEL_SDK_DISABLED=false` and the endpoint, the Java agent loaded without errors, but no connection to port 18890 was seen in the first ~60 s (the JVM metrics are exported every 60 s, and the PMS had no traffic to trace). Check in the browser instead of waiting on the script.
