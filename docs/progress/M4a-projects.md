# M4a — Projects: `up`, `down`, `project ps|logs|stop|config`

Status: **done**. Workspace tests pass with no warnings (`yarc-cli` 35 unit + 6 + 5 integration, `yarc-runtime` 55, `yarc-config` 11, `yarc-oci` 58). 6 new live project tests (`--ignored`) pass, and the 29 earlier live tests still pass.

M4 plan decisions (defaults): shared per-project network namespace (M4c), bind `127.0.0.1` and TCP only for published ports, foreground `up` + `up -d`, order a → b → c → d.

## What was done

### Project model in the CLI (`yarc-cli/src/project.rs`)
- **Project name:** `-p/--project NAME`, else the name of the directory holding the project file (lowercased, other characters become `-`). An explicit name must be valid (lowercase letters, digits, `_`, `-`, max 40).
- **Container names:** `<project>-<service>-1`; service names are checked up front (letters, digits, `_ . -`) so a bad name fails before anything is created.
- **Labels** on every container (new `labels` map in `ContainerSpec`, optional in old `config.json` files): `yarc.project`, `yarc.service`, `yarc.config-hash`. A project is "the containers with `yarc.project=<name>`".
- **Start order:** dependencies first, alphabetical among the ready ones, so it is deterministic.
- **Definition hash:** sha256 over image, `cmd`, env, ports, volumes and health. `needs` is left out on purpose: it only affects ordering.
- Global options `--file FILE` (default `yarc.yaml`, env `YARC_FILE`) and `-p/--project`. (`-f` stays free for `logs -f`.)

### Config: `cmd`
The service model had no way to set a command, so no useful project could be written (`sleep`, `echo`, `serve`). `cmd: [sh, -c, "..."]` (a list, not a string) now replaces the image's default `CMD`; it is part of the hash.

### Commands
```
yarc up [-d]                         create + start every service, dependencies first
yarc down [-t SECONDS]               stop (dependents first) and remove all containers of the project
yarc project ps                      SERVICE / CONTAINER / STATUS / IMAGE
yarc project logs [-f] [-t] [--tail N] [service...]    lines prefixed with the service, merged by time
yarc project stop [-t SECONDS]       stop but keep the containers
yarc project config                  print the resolved project: name, file, start order, needs
```
- **`up` is idempotent:** a service whose container exists with the same hash is left alone ("up to date"), or just started if it had stopped; one whose definition changed is stopped, removed, recreated ("recreating"). A service that cannot start stops `up` there, so its dependents are never created (exit code is Docker-style, e.g. 127).
- **Foreground `up`** streams all services' output with `service | ` prefixes (stderr lines stay on stderr), and only shows this run's output, not earlier runs'. It returns 0 when every container has exited. **Ctrl-C** stops all services gracefully, dependents first, and exits 130.
- `up -d` returns once everything is started.
- **`down`** also removes containers of services that were dropped from the file (orphans), first; it works even if the file is gone when `-p` is given.
- Unsupported parts of a file are accepted but warned about, so nothing is silently ignored: ports not published, volumes not mounted, health checks not run, `needs: healthy` treated as `started` (M4b–d).
- On non-Linux, `up/down/project stop` say to use WSL; `config`, `ps` and `logs` only read files.

### Docs
`docs/design.md` is up to date with the decisions of M2–M4 (rootless first, shim/lock process model, shared project network, milestone list renumbered: agent + Windows are now M5, macOS M6).

## Tests
- **Unit (`project.rs`, 9):** name sanitizing and validation, name from option or directory, container and service names, start order (diamond-ish graph, deterministic, alphabetical), hash covers image/cmd/env/ports/volumes/health but not `needs`, container sorting with orphans, labels, file loading errors (missing file, no services, unknown dependency, bad service name). CLI parsing of project commands and global options (before and after the subcommand). Config: `cmd` is a list and optional.
- **Integration, offline (5):** `project config` shows the start order and needs, default name from the directory, errors for a bad dependency / bad project name / missing file, `project ps`/`logs` of a project never started, `down` with nothing to remove.
- **Live (6):**
  - `up -d` starts both services (dependency first), logs are prefixed and filterable by service; a second `up -d` touches nothing (same `started_ms`); changing `web` recreates only `web`; `project stop` then `up -d` restarts the same containers; `down` removes dependents first and leaves no containers.
  - `down` removes the container of a service dropped from the file.
  - Foreground `up`: prefixed stdout/stderr, ends when all exit, second run shows only its own output.
  - Ctrl-C during `up` stops both services, exit 130.
  - A service that cannot start: `up -d` exits 127 naming the service, its dependent is not created.
  - Two projects can use the same service names (separate stores) and a different `-p` in one store is a different project.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_project -- --ignored --test-threads=1'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_run -- --ignored --test-threads=1'
```

### Try it
Inside WSL:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cd /mnt/c/Projects/MINE/yarc && cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

mkdir -p ~/demo && cd ~/demo
cat > yarc.yaml <<'EOF'
services:
  db:
    image: alpine:3.20
    cmd: [sh, -c, "echo db ready; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
  web:
    image: alpine:3.20
    env: { GREETING: hello }
    cmd: [sh, -c, "echo $GREETING from web; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    needs: { db: started }
EOF
$yarc project config               # name = demo, db before web
$yarc up -d                        # created and started db, then web
$yarc project ps
$yarc project logs                 # db  | db ready / web | hello from web
$yarc up -d                        # both "up to date"
sed -i 's/hello/howdy/' yarc.yaml
$yarc up -d                        # "web: definition changed, recreating"; db untouched
$yarc project logs web
$yarc down                         # web stopped and removed first, then db
$yarc up                           # foreground: prefixed live output; Ctrl-C stops everything (exit 130)
$yarc down
```

## Not done / follow-ups
- Services are on the **host network** for now (no isolation, no service names, no published ports): M4c.
- No healthchecks, so `needs: healthy` is only `started` (M4b); no volumes (M4d).
- `up` starts services one after the other; independent services are not started in parallel.
- Only one container per service (`-1`); no scaling.
- `up` does not re-pull an image whose tag moved; a changed *tag in the file* recreates the service, a moved tag does not.
- During the graceful stop after Ctrl-C a second Ctrl-C is not handled specially.
- A stopped project can be restarted with `up`; there is no `project start/restart`.
- The hash uses the `Debug` form of the model, so a yarc upgrade that changes it recreates containers once.

## Next: M4b — `exec` and healthchecks
`yarc exec <container> cmd...` (new runtime piece: join the container's namespaces and root), then the supervisor runs `health.cmd` every `interval`, records `starting/healthy/unhealthy` in `state.json`, and `needs: healthy` waits for it. A spike for `setns` into a rootless user namespace comes first.
