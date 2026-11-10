# C2 — Core compose reader

Status: **done**. Workspace tests pass with no warnings (`yarc-config` 57, `yarc-cli` 89 unit + 7 new compose integration + the earlier ones). Live (container) tests were not run: C2 changes no runtime code, only what `up` passes to it.

## What was done
### `parse_compose` (`yarc-config/src/compose.rs`)
Fills the same `Project` model as `yarc.yaml`, so `up`, `down`, `ps`, hashing and recreate-on-change work unchanged.
- **Supported per service:** `image`, `command` / `entrypoint` (string, split like a shell, or list; `entrypoint: []` clears the image's), `environment` (list or map; `KEY` alone and `KEY:` with no value pass through from the environment, skipped when unset), `ports` (short form), `volumes` (short form, `:ro`, `:rw`), `depends_on` (list, or map with `service_started` / `service_healthy`), `user`, `working_dir`, `healthcheck` (`test` as string, `CMD-SHELL`, `CMD`, `NONE`; `interval`, compound durations such as `1m30s`), `container_name`.
- **Top level:** `volumes` may only declare names (options such as `driver` are an error); a named volume used by a service must be declared. `version` and any `x-...` key are ignored; YAML anchors and `<<` merges work.
- **Everything else is an error** naming the key, e.g. `service 'a': unsupported key 'build'`. That includes `restart`, `secrets`, `profiles`, `stop_grace_period` and top-level `secrets`: they arrive in C3 and C4, so the real PMS bundles do not load yet.
- **Ports:** `[127.0.0.1:]HOST:CONTAINER[/proto]`. Any other bind address, a bare container port (random host port), ranges and the long syntax are errors with an explanation. `yarc` always publishes on 127.0.0.1.
- **Interpolation** runs on the string values of the parsed YAML, never on comments or keys. Errors name the place (`services.app.environment[0]: required variable TOKEN is missing a value: set TOKEN in .env`) and never contain values.
- **Model:** `Service` gained `entrypoint`, `working_dir` and `container_name` (`None` for `yarc.yaml`). The three are part of the definition hash only when set, so existing containers keep their hash. `parse_duration` now accepts compound values. `yarc.yaml` still rejects `:rw`; only the compose reader accepts it.
- **`container_name`** may not collide with another service's name or `container_name` (its own service name is fine, and the common case).

### CLI (`yarc-cli/src/project.rs`)
- **Format by file name:** `yarc.*` is the native format, any other name is read as compose.
- **Discovery:** with no `--file`, if `yarc.yaml` does not exist the first of `yarc.yml`, `compose.yaml`, `compose.yml`, `docker-compose.yaml`, `docker-compose.yml` in the current directory is used. `yarc.yaml` wins when both exist.
- **Variables:** the process environment, then the `.env` next to the file. A broken `.env` is reported with a line number and never its contents.
- **Wiring:** `entrypoint` and `working_dir` reach the container's overrides. `container_name` is an extra host name inside the project network (the container itself is still `<project>-<service>-1`).
- **`project config`** now also prints the alias, entrypoint, working dir, command and the **names** (never values) of the environment variables.

## Limits / notes for later steps
- `-f` as a short for `--file` is not added: `logs -f` already uses `-f`, and a global short would clash. Decide in C5 (e.g. `-f` only before the subcommand, or keep `--file`).
- `--env-file` and `--profile` come in C3/C5. Precedence stays: process environment over `.env`.
- Ports with a protocol and a bind address, volume `:z`/`:Z` flags and `healthcheck` `timeout`/`retries`/`start_period` are errors for now.

## Tests
- **`yarc-config` (20 new in `compose/tests.rs`):** every supported key in each of its forms, every rejection above, shell-style word splitting, interpolation place names, anchors and merges, collisions, and a fixture shaped like the PMS bundles (minus C3/C4 keys).
- **`yarc-cli` integration (7, `tests/project_compose.rs`):** discovery, `.env` and process-environment precedence, a missing required variable, an explicit file with another name, `yarc.yaml` winning and keeping its own `$(...)` shell untouched, unsupported keys, a broken `.env` that does not leak its contents; `project config` never prints a value.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```
`server::tests::create_options_reach_the_command_line` in `yarc-agent` failed once during the first full run and passed in every rerun (3 alone, 2 in full runs); it looks like a timing flake in the fake-yarc test, unrelated to this change. Worth a look if it comes back.
