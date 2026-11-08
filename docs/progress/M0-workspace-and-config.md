# M0 — Workspace and `yarc.yaml` parser

Status: **done** (9/9 tests passing in WSL Ubuntu-24.04).

## What was done

### Workspace skeleton
Cargo workspace (`Cargo.toml`, edition 2021, resolver 2) with six crates under `crates/`:

| crate | state |
|---|---|
| `yarc-cli` | binary, prints version (stub) |
| `yarc-agent` | binary, prints version (stub) |
| `yarc-config` | **implemented** (see below) |
| `yarc-oci` | empty placeholder |
| `yarc-proto` | empty placeholder |
| `yarc-runtime` | `Runtime` trait + `ContainerSpec` / `State` types, no implementation |

Also added: `README.md`, `.gitignore`, `docs/design.md`.

### `yarc-config`
Files: `src/lib.rs` (public API + `ConfigError`), `src/model.rs` (typed model + serde raw layer + field parsers), `src/validate.rs` (cross-service validation), `src/tests.rs`.

Public API: `yarc_config::parse(&str) -> Result<Project, ConfigError>`.

Supported keys per service: `image`, `env` (map), `ports`, `volumes`, `health` (`cmd`, `interval`), `needs` (map of service → `started` | `healthy`). Unknown keys are rejected (`deny_unknown_fields`).

Parsing rules:
- **ports**: `HOST:CONTAINER[/tcp|/udp]`, each 1–65535, default tcp.
- **volumes**: `SOURCE:TARGET[:ro]`; target must be absolute. Source starting with `/`, `.` or `~` is a host path, anything else is a named volume (implicit, managed by yarc).
- **health.interval**: `ms`, `s`, `m`, `h`; defaults to 30s.

Validation rules: empty image, no services, `needs` on unknown service, self-need, `healthy` on a service without `health`, dependency cycles (Kahn's algorithm).

Dependencies: `serde`, `serde_yaml_ng` 0.10, `thiserror` 2.

## Decisions
- Format is YAML; compose-spec compatibility is a non-goal.
- `needs` replaces `depends_on` (with condition `started` / `healthy`).
- `serde_yaml` is deprecated, so `serde_yaml_ng` is used.
- Two-layer parsing (raw serde structs → validated model) so the public model contains parsed types (ports, durations), not strings.

## How to verify

```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```

`CARGO_TARGET_DIR` keeps build output on WSL's filesystem (the `/mnt/c` mount is slow).

## Environment notes
- Rust 1.99.1 installed in WSL via rustup. Not installed on the Windows host.
- `Cargo.lock` is generated at the repo root.

## Not done / follow-ups
- No `yarc` CLI behaviour yet (no argument parsing, no `yarc.yaml` loading from disk).
- Top-level `volumes:` / `networks:` sections are not supported (named volumes are implicit).
- `env` values must be strings in YAML (e.g. quote numbers); no `.env` file support yet.
