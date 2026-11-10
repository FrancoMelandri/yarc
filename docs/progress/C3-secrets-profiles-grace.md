# C3 — Secrets, profiles, stop grace period

Status: **done**. Workspace tests pass with no warnings (`yarc-config` 62, `yarc-cli` 93 unit + 9 compose integration and the other crates). Live (container) tests were not run; the bundle check below stops at `restart`, which is C4.

## What was done
### Secrets
- Top level `secrets:` accepts `file: PATH` only (`external`, `environment`, ... are errors). A service lists names (short form; the long form with `source`/`target`/`mode` is an error).
- Each secret is bind-mounted **read-only at `/run/secrets/<name>`**. The path is relative to the compose file (`~/` and absolute paths work).
- `up` checks every secret file **before creating anything**: a missing file, or a directory, fails with `service 'pms': secret 'pms_auth_key': file ... does not exist`.
- The secret names and paths are part of the definition hash; the file's contents are not (the mount is live, so editing a secret needs no recreate).
- `project config` lists secret **names**, never paths' contents.

### Profiles
- `profiles: [name]` per service. Services with a profile only run when one is active. `--profile NAME` (repeatable, comma separated, global option) or `COMPOSE_PROFILES`; `*` activates all.
- A service that `needs` one in an inactive profile is an error that names the profile and says to use `--profile` (never a silently missing dependency).
- Services of an inactive profile that are still running count as orphans, so `down` without the profile removes them.
- Through WSL: the new `Globals.profiles` field in the protocol (additive, skipped when empty, so the golden wire format is unchanged) and `--profile=` in the agent's command line; `COMPOSE_PROFILES` set in Windows is forwarded as profiles.

### `stop_grace_period`
- `down` and `project stop` now wait: `-t` if given, else the service's `stop_grace_period`, else 10 s (also for orphans). `-t` no longer has a default of 10 in the CLI (`Option`), and the protocol already carried `Option`.
- The same value is used when `up` recreates a changed service and when Ctrl-C stops a foreground `up`.

## Notes
- Mixing `--profile` before and after the subcommand keeps only the later one (clap's rule for global options, same as `--file`). Use one position.
- Not covered by an automated test: a real container reading `/run/secrets/...` (needs a live run; the PMS image runs as a non-root user, see the plan's decision 4) and the Windows forwarding of `--profile` end to end (the argument building is unit tested).

## Real bundles
`yarc project config` in `tmp/ssw-pms-fias-aero/docker-bundle` now stops at `service 'dotnet-aspire-dashboard': unsupported key 'restart'`: the last blocker, C4.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```
