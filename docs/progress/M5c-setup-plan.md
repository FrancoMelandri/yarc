# M5c — `yarc setup` (future improvement)

Status: **planned, not started.** Postponed by the user: for now the Windows setup is done by hand (build yarc inside the WSL distro, set `YARC_WSL_BIN`). Nothing here is implemented.

## The problem it solves
Three things must exist before `yarc.exe` works, today all by hand:
1. A WSL 2 distro with `passt` installed and unprivileged user namespaces.
2. A Linux `yarc` binary inside it, at a path given by `YARC_WSL_BIN`.
3. Matching versions of `yarc.exe` and that binary.

Someone who only has `yarc.exe` cannot do this. `yarc setup` makes the machine ready with one command.

## What `yarc setup` would do
1. **Check WSL** (`wsl --status`). If WSL is not installed/enabled, stop and tell the user to run `wsl --install` as administrator (and reboot). It does not do that itself.
2. **Create a dedicated distro** named `yarc`, separate from the user's own: a small Alpine rootfs (~3 MB) pulled with yarc's own puller, flattened to a tar, then `wsl --import yarc %LOCALAPPDATA%\yarc\wsl <rootfs.tar> --version 2`.
3. **Provision it:** `apk add passt`; create a normal user `yarc`; write `/etc/wsl.conf` (default user, no Windows PATH appended); add the corporate CA if one is configured.
4. **Install the Linux binary** (a static musl build) to `/usr/local/bin/yarc`.
5. **Remember the result** in a small config file under `%APPDATA%\yarc` (distro, binary path). Precedence afterwards: environment variables (`YARC_WSL_*`), then the config file, then WSL's default distro.
6. **Verify** by running `yarc doctor`.

Related subcommands:
- `yarc setup` is idempotent: a second run only repairs or updates what is missing.
- `yarc setup --upgrade` replaces the Linux binary when it no longer matches `yarc.exe` (the protocol-mismatch error already says "install the same yarc version inside WSL" and would point here).
- `yarc setup --uninstall` runs `wsl --unregister yarc` after a confirmation (it deletes all images and containers).

## Why a dedicated distro
It does not depend on, or risk breaking, the user's own distro; its contents are always the same, so bugs are reproducible; all data lives in one place that can be reset.

## Work breakdown
- **M5c-lite (suggested first):** `yarc setup --distro <existing>`: copy the Linux binary into an existing distro, write the config file, make `Wsl::from_env` read the config (env > config > default distro). Small, removes the manual `YARC_WSL_BIN` step, needs no rootfs work.
- **M5c1** rootfs tar export in `yarc-oci`: flatten an image's layers (applying whiteouts) into one tar stream. Needed because `image unpack` cannot run on Windows (it needs Linux file semantics).
- **M5c2** orchestration: `wsl --import` and the provisioning steps, driven through `wsl -d yarc -u root --exec ...`; the command lines are built by pure functions so they are unit-testable.
- **M5c3** binary delivery and upgrade: version check, replacement of the binary.
- **M5c4** config file, defaults, uninstall, docs.

## Testing
Mostly doable from WSL: `wsl.exe --import` / `--unregister` can be called from inside WSL on a throwaway distro, so the whole flow can run for real. That is still not a clean Windows machine without WSL.

## Risks
- WSL not installed: needs admin rights and a reboot; we can only detect it and explain.
- Corporate TLS inspection (Netskope): `apk` inside the new distro and the Alpine pull from Docker Hub both go through it. Pulling would need yarc's registry client to trust the corporate CA; not yet known whether it does on Windows.
- Policy may block `wsl --import`.
- The distro's virtual disk grows with images.
- Linux binary delivery: a static musl `yarc` needs a CI build step (C dependencies such as `ring`). Ship it next to `yarc.exe` (e.g. `yarc-linux-x86_64`, simplest) or embed it in `yarc.exe`. An arm64 variant would be needed for Windows on ARM.

## Open decisions
1. Lite first, then dedicated distro (suggested), or dedicated distro directly?
2. Rootfs source: Alpine pulled from the registry (depends on the TLS question above), or a tarball supplied by the user?
3. `pasta`: install with `apk` at setup (needs network) or ship the binary?
4. Linux binary: shipped next to `yarc.exe` (suggested) or embedded?
