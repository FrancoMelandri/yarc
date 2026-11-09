# Compose support — plan

Status: **planned, nothing implemented. To be started in the next session** (after the user answers the decisions below, or says "defaults").

Goal: `yarc -f docker-compose.yml up` works on a real bundle unchanged (the two bundles under `tmp/ssw-pms-fias-inspector` and `tmp/ssw-pms-fias-aero`), instead of a hand-written `yarc.yaml`.

## What the bundles use that `yarc.yaml` cannot express today
| Compose feature | Where |
|---|---|
| `${VAR}`, `${VAR:-default}`, `${VAR:?message}` plus a `.env` file | everywhere |
| `environment:` as a list (`- K=V`) | every service |
| `secrets:` (file-based, mounted read-only at `/run/secrets/<name>`) | pms, aero |
| `profiles:` (Aspire only with `--profile observability`) | Aspire dashboard |
| `restart: unless-stopped` | every service |
| `stop_grace_period: 20s` | pms |
| `container_name:` (also the DNS name: the dispatcher URI uses it) | every service |
| `depends_on:` as a list | pms |
| `ports:` with a bind address (`127.0.0.1:${PORT}:18888`) | Aspire |
| top-level `volumes:` / `secrets:` declarations | both |

Bundles also contain a `.env` (real values: never print or commit its contents) and an `env.template`.

## Approach
A second reader for compose files that fills the same internal `Project` model (`yarc-config`). `yarc.yaml` stays as it is, **without** `$` interpolation (its `cmd` fields already contain shell such as `$(date +%s)`).

Interpolation runs on string values after the YAML is parsed (as Docker does), so a `$` in a comment cannot fail. Value sources: process environment, then `.env` next to the file, then `--env-file`. `yarc project config` already prints the resolved project and becomes the way to debug interpolation.

## Steps (each: tests, a progress doc, then stop for the user's go)
- **C1 interpolation and `.env`:** pure module, every `${...}` form (`-`, `:-`, `?`, `:?`, `+`, `:+`, `$$`), `.env` parser (quotes, comments, `export`). Heavily unit tested.
- **C2 core compose reader:** `image`, `command`, `entrypoint`, `environment` (list and map), `ports`, `volumes`, `depends_on` (list and map with conditions), `user`, `working_dir`, `healthcheck`, `container_name`. New model fields: `entrypoint`, `working_dir`, `container_name`. File discovery finds `compose.yaml` / `docker-compose.yml`. Variables without a value (`- FOO`) pass through from the environment.
- **C3 secrets, profiles, stop grace:** file secrets become read-only mounts at `/run/secrets/<name>` (clear error if the file is missing); `--profile` and `COMPOSE_PROFILES` (services in an inactive profile are skipped; a `needs` on a skipped service is an error); `stop_grace_period` used by `down`/`stop` when `-t` is not given.
- **C4 `restart` policy:** the shim restarts the process on exit with a backoff, honouring `unless-stopped`, `always`, `on-failure[:N]`; a manual stop or `down` never triggers a restart. The only step that changes runtime behaviour: design it separately before coding.
- **C5 end to end:** forward `--profile` and `--env-file` through the protocol (additive fields, so no version bump), the agent and the Windows client (translate the env-file path); run both real bundles from their `docker-compose.yml` and compare with the hand-written `yarc.yaml`, which then becomes redundant. The compose files contain no secrets, so they can be test fixtures.

## Decisions needed (recommendation first)
1. **Read compose directly, or generate a `yarc.yaml` (`yarc convert`)?** Directly: one file, `.env` changes are picked up on every run. A converter is simpler but loses that.
2. **Unsupported keys** (`build`, `networks`, `deploy`, `cap_add`, ...): error with a clear message (recommended) or warn and ignore. `x-...` extension keys are ignored either way.
3. **`restart`:** implement it in C4 (recommended) or parse it and warn that it is ignored.
4. **Images with `USER pms` / `USER aero`** still need `user: root` until multi-uid support exists, and compose has no place for it:
   - `x-yarc:` block per service, e.g. `x-yarc: { user: root }` (recommended for now);
   - an overlay file `yarc.override.yaml`, leaving the compose file untouched;
   - or wait for multi-uid support and keep the compose file as shipped.

"defaults" = read directly, error on unsupported keys, implement restart, `x-yarc`.

## Known limits that stay
No `build`, `networks`, `extends`/`include`, multiple files; the Aspire dashboard image is not in the bundles' `images.tar`, so its profile stays unused until the image is available.
