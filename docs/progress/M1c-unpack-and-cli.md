# M1c — Layer unpacking and the `yarc` CLI

Status: **done**. 58 `yarc-oci` + 9 `yarc-cli` unit tests, 4 CLI integration tests, 9 `yarc-config` tests pass; 3 live tests (`--ignored`) from M1a/M1b still pass. No build warnings.

**M1 is complete:** `yarc pull alpine` followed by `yarc image unpack alpine ./rootfs` produces a rootfs that really runs (see "End-to-end" below).

## What was done

### `yarc-oci/src/unpack.rs` (Unix only)
`unpack_layer(reader, dest)` applies one layer; `unpack_image(store, reference, dest)` applies all layers of a pulled image in order. `dest` must not exist or be empty. Returns an `UnpackReport` (files, dirs, symlinks, hardlinks, whiteouts, skipped special files).

**Safety (layers are untrusted):**
- No entry is extracted with a plain `join`. Every existing parent component is resolved chroot-style: absolute symlink targets restart at the rootfs, `..` is clamped at the rootfs, at most 40 symlinks are followed (`TooManySymlinks`). A symlink from one layer therefore cannot redirect a later write, whiteout or hardlink source outside the rootfs.
- `..` in an entry path is rejected (`UnsafePath`); leading `/` and `./` are tolerated and stay inside the root.
- Replacing entries never follows the existing final component (`symlink_metadata`), so a symlink can be replaced by a directory and vice versa.

**OCI semantics:**
- Whiteouts `.wh.<name>` delete the file or whole directory from lower layers; the markers are never extracted.
- Opaque marker `.wh..wh..opq` removes everything lower layers put in that directory but keeps what the same layer wrote, in either order.
- Type changes between layers work (file ↔ dir ↔ symlink).
- Hardlinks (resolved inside the rootfs, across layers), symlinks (target text kept verbatim), modes.
- Directory permissions and ownership are applied after each layer, deepest first, so read-only directories don't block their own contents.
- Ownership (`lchown`) is applied only when running as root. When not root, directories keep owner rwx so later layers can still write.

**Compression:** detected from magic bytes, not media types. Plain tar and gzip work; zstd gives a clear `Unsupported` error.

**Skipped on purpose:** device nodes and fifos (counted in the report; M2 mounts `/dev`), xattrs, mtimes, sparse files (error).

### `yarc-oci` other changes
- `BlobStore::load_image(reference) -> StoredImage` (manifest + config, e.g. for `Entrypoint`/`Cmd` in M2).
- New errors: `NotFound`, `UnsafePath`, `TooManySymlinks`, `BadLayer`, `Unsupported`, `DestNotEmpty`.
- The module is `cfg(unix)`, so `yarc-oci` (and `pull`) is still intended to build on Windows. Not verified: there is no Rust toolchain on the Windows host yet.

### `yarc-cli`
The binary is now named `yarc` (`[[bin]]`). Built with `clap` (derive).

```
yarc [--root DIR] pull <image> [--jobs N] [--platform OS/ARCH]
yarc [--root DIR] image unpack <image> <dir>
```

- `--root` / `YARC_ROOT` selects the store. Default: `$XDG_DATA_HOME/yarc` or `~/.local/share/yarc` (Linux), `~/Library/Application Support/yarc` (macOS), `%LOCALAPPDATA%\yarc` (Windows).
- `pull` prints progress to stderr and a summary (digest, layers downloaded / cached, size) to stdout.
- On non-Unix, `image unpack` reports that it needs Linux (run it inside WSL).
- Files: `cli.rs` (definitions), `commands.rs`, `root.rs`, `main.rs`.

## Tests
- **Unpack (18):** modes and implicit parents, absolute entry paths, `..` rejected, read-only dirs, special files, overrides and whiteouts (incl. whole directories and non-existent targets), opaque whiteout in both orders, type changes, hardlinks, symlink text, absolute symlink dirs resolving inside the rootfs, relative and absolute symlink escapes (asserting nothing appears outside the rootfs), whiteout and hardlink through an escaping symlink, symlink loops, gzip detection, zstd and garbage rejected, whole image from a store, unknown image and non-empty destination. Malicious tars are built with raw headers, since `tar::Builder` refuses such paths.
- **CLI:** argument parsing (defaults, options, global `--root`, invalid input), default store location, platform parsing, size formatting; integration tests run the real binary (help, invalid reference, invalid platform, unpacking an unpulled image creates nothing).

## End-to-end (real Docker Hub, in WSL)
- `yarc pull alpine:3.20` (1 layer, 3.5 MiB) and `nginx:alpine-slim` (7 layers, 5.5 MiB).
- Unpacked Alpine: 87 files, 96 dirs, 334 symlinks; `bin/sh -> /bin/busybox` kept absolute. nginx: 472 files, 254 hardlinks, `access.log -> /dev/stdout`.
- Running it: `unshare -Ur chroot <rootfs> /bin/sh -c '…'` printed `hello from Alpine Linux v3.20, uid=0` and listed 304 busybox applets. This is a preview of M2.
- Error paths verified: re-unpack into a non-empty dir, unpack of a never-pulled image, `--platform windows/amd64` (no such image), invalid reference.

## Decisions
- Resolve symlinks ourselves instead of using `tar`'s `unpack_in`, which canonicalizes against the host and therefore rejects legitimate absolute symlinks such as `/lib -> /usr/lib`.
- Unpacking needs no root for correctness testing; ownership fidelity is applied only when root (the agent will run as root in M3).
- Flat unpack for now. M2 needs per-layer directories for overlayfs; that will reuse `unpack_layer` per layer.

## How to verify

```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
```

### Try it: pull, unpack and run the image
Run inside WSL (Ubuntu). From the repo root:

```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

$yarc pull alpine:3.20
$yarc image unpack alpine:3.20 ./rootfs

# Check that the unpacked image works: run a shell inside it, as a fake root
# in an unprivileged user namespace (no sudo needed).
unshare -Ur chroot ./rootfs /bin/sh -c 'echo "hello from $(. /etc/os-release; echo $PRETTY_NAME), uid=$(id -u)"'
```

Expected output of the last command:

```
hello from Alpine Linux v3.20, uid=0
```

How it works:
- `unshare -Ur` creates a new user namespace and maps your user to uid 0 inside it, so `chroot` is allowed without real root. The `uid=0` in the output comes from that mapping.
- `chroot ./rootfs` makes the unpacked image the filesystem root, so `/bin/sh` and `/etc/os-release` are the ones from the image, not from Ubuntu.
- This is only a smoke test of the unpacked filesystem. It has no mount, pid or network isolation and no cgroups; those come with the real runtime in M2.

If `unshare` fails with "Operation not permitted", unprivileged user namespaces are disabled on that machine (`sysctl kernel.unprivileged_userns_clone` or AppArmor settings on some distros).

Remove the test directory afterwards with `rm -rf ./rootfs` (it contains files that were created by your user, so no sudo is needed). `./rootfs` also needs to be empty or non-existent before each `unpack`.

## Not done / follow-ups
- Unpacking as root (ownership, setuid) is untested: `sudo` needs a password in this WSL, so those paths (`lchown`, exact directory modes) have no test run yet.
- No limits on total unpacked size or file count (zip-bomb protection).
- Whiteouts for paths a *same-layer* earlier entry created are not special-cased.
- The temp store from the end-to-end run lived in `/tmp` and is gone; nothing to clean up in the repo.
- Windows build of `yarc-cli`/`yarc-oci` is untested.

## Next: M2 — run one container on native Linux
`yarc run alpine echo hi`: per-layer extraction, overlayfs mount, namespaces (pid/mount/uts/ipc/net/user), `pivot_root`, cgroups v2, exec of `Entrypoint`/`Cmd` from the image config. It needs root or user namespaces, so decisions on privileges (rootful agent vs. rootless-with-userns first) come first.
