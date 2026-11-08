# M2a — Container spec, on-disk containers, `yarc create`

Status: **done**. 19 `yarc-runtime` tests, 12 `yarc-cli` unit + 6 integration tests added or updated; whole workspace passes (58 `yarc-oci`, 9 `yarc-config`), no warnings.

M2a decisions (from the M2 plan): rootless first, flat per-container rootfs (overlayfs later), `--network host` by default, `nix` crate for syscalls (starts in M2b).

## What was done

### `yarc-runtime` (portable, no syscalls yet)
- `spec.rs`
  - `resolve_process(ImageDefaults, Overrides) -> Process` is pure, with Docker semantics:
    - CLI args replace the image's `Cmd` but keep its `Entrypoint`.
    - `--entrypoint` discards the image's `Cmd`; `--entrypoint ""` clears the entrypoint.
    - Env: image `KEY=VALUE` entries, then user `-e` entries override by key; `PATH` defaults to the Docker default if nobody sets it.
    - Working dir defaults to `/` and must be absolute; empty strings in image configs mean "unset".
    - Nothing to run is an error (`NoCommand`).
  - `ContainerSpec` (id, image, rootfs, args, env, working_dir, user, hostname, network), serializable. Hostname is the first 12 characters of the id.
  - `Network::{Host, None}`.
- `container.rs`: `ContainerStore` with layout `<root>/containers/<id>/{config.json, rootfs/}`.
  - `allocate(name?)` creates the directory (name or random 12-hex id). The rootfs directory is deliberately not created, so the unpacker can create it.
  - `save` (write-then-rename), `load`, `list` (sorted; ignores half-created containers), `remove`.
  - Ids double as directory names, so they are validated (`[A-Za-z0-9][A-Za-z0-9_.-]{0,63}`); that also blocks `../` traversal in `load`/`remove`.
  - Random ids use std's randomly seeded `RandomState`, so no extra crate.
- `error.rs`: `RuntimeError` (NoCommand, InvalidEnv, RelativeWorkDir, InvalidName, AlreadyExists, NotFound, Io, Json).
- The `Runtime` trait stays as it was (implementation comes in M2b).

### `yarc-cli`
```
yarc create [--name N] [-e K=V]... [--entrypoint CMD] [-w DIR] [-u USER] [--network host|none] <image> [cmd...]
yarc container ls
yarc container rm <id>...
```
- `create` pulls the image first if it is not in the local store, validates the command line before touching the disk, unpacks the flat rootfs, writes `config.json`, and prints the id. If anything fails after the id was reserved, the half-built container is removed.
- Everything after `<image>` belongs to the container's command (`trailing_var_arg`), so `yarc create alpine sh -c 'echo $A'` works with hyphenated arguments.
- Pull logic is now shared (`fetch`) between `pull` and `create`.
- `create` and `container rm` need Linux file semantics; on Windows they say to use WSL. `container ls` works anywhere.
- `-u/--user` is stored but not applied until M2b.

## Tests
- **spec (12):** entrypoint/cmd matrix, env merging and precedence, default PATH, malformed env (rejected from the user, skipped from the image), working dir/user precedence, relative working dir, network parsing, hostname.
- **container store (7):** roundtrip, random ids differ, name validation and uniqueness, hostile ids, sorted list that skips unfinished containers, removal with rootfs, corrupt config.
- **cli:** argument parsing for `create` (defaults, options before the image, command after it) and `container`; integration tests for `ls` on an empty root and `rm` of an unknown container.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```

### Try it
Inside WSL, from the repo root:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

$yarc create alpine:3.20 echo hi            # pulls if needed, prints an id
$yarc create --name web -e A=1 --entrypoint /bin/sh -w /tmp --network none alpine:3.20 -c 'echo $A'
$yarc container ls
cat ~/.local/share/yarc/containers/web/config.json
ls  ~/.local/share/yarc/containers/web/rootfs
$yarc container rm web
```
`create` fails with a clear error for a duplicate name, an invalid name, or no command (`--entrypoint ""` with no args).

I ran this against Docker Hub with Alpine 3.20: auto-pull, named container with overrides, duplicate name, no command (nothing left on disk), `rm`, `rm` of an unknown id.

## Not done / follow-ups
- Nothing is started yet; that is M2b.
- A container directory is removed with `remove_dir_all`. Once the container runs in a user namespace and writes files, files may be owned by sub-uids; revisit with the multi-uid mapping.
- The flat rootfs is a full copy per container (slow and large for big images); overlayfs is planned later.
- `--user` is not resolved to a uid/gid yet; with a single-uid rootless mapping only root will work.
- `ls` columns are not width-aware for long names.

## Next: M2b — the runtime
Namespaces (user, mount, pid, uts, ipc, optionally net) with `nix`, uid/gid map, mounts (`/proc`, `/dev`, `/sys`, tmpfs), `pivot_root`, `execve`, exit code and signal forwarding, with the mount plan built as testable data. `yarc run` follows in M2c.
