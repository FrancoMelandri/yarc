# P1 — `yarc load` (PMS bundle, step 1)

Status: **done.** `yarc-oci` 68 tests, `yarc-cli` unit/integration pass, no warnings. Nothing committed.

## What was done
- `yarc load <file|dir>`: imports an OCI image layout (a tar such as `images.tar`, or the unpacked directory) into the store. Works offline and on any OS (the command is not Linux-only).
- `yarc-oci/src/load.rs` (`load_path`, `load_archive`, `load_dir`, `LoadReport`):
  - single streaming pass over the tar; the archive is never extracted, and only the fixed names `index.json`, `manifest.json` and `blobs/sha256/<64 hex>` are acted on, so entry names cannot choose a destination (`..` names are ignored);
  - each blob is verified against the digest in its name and its tar size (`put_stream`), existing blobs are skipped, so loading twice is a no-op;
  - refs come from the annotation `io.containerd.image.name` (full name, normalised like `pull`: `docker.io/library/x:1` -> `registry-1.docker.io/library/x:1`); `org.opencontainers.image.ref.name` is only used when it is a full name; manifests with no name are reported as warnings;
  - all-or-nothing naming: every image's manifest, config and layers must be present before any ref is set;
  - errors: legacy `docker save` archives (`manifest.json`, no `index.json`) say so; multi-platform indexes are refused with a clear message; truncated or non-tar input is reported.
- Offline `run` / `up` need nothing else: they already skip the pull when the ref exists.

## Verified on the real bundle
`yarc --root R load images.tar`: 3 images, 58 blobs, 738 MiB in about 16 s (debug build, WSL, archive on `/mnt/c`); second load adds 0; `yarc run ssw-platform-pms-inspect:0.0.3` then starts from the store without network.

## Not done
- Docker legacy `docker save` format, multi-platform indexes (choose a platform), progress output for big archives.

## Next (PMS bundle plan)
2. `user:` in `yarc.yaml` (pms image has `USER pms`, rejected by the rootless runtime). 3. Outbound networking with `pasta` in the project holder. 4. Bundle `yarc.yaml` and a run against a fake FIAS listener.

## Addendum — `image ls` / `image rm`
- `yarc image ls` (alias `list`): IMAGE (Docker Hub host and `library/` hidden), IMAGE ID (12 hex of the manifest digest), SIZE (sum of the compressed layers), CREATED (from the image config). Works on any OS.
- `yarc image rm <image>...`: forgets the name and deletes the blobs no other image uses (shared layers stay; an image with two names keeps its blobs until both are gone). Removes what it can, reports unknown images, exits 1 if any failed. Containers already created from the image keep working (rootfs is unpacked); it does not check for them.
- Store: `list_refs`, `image_blobs`, `remove_image` (works out what other images need before deleting, so an unreadable image aborts instead of losing blobs); `ImageConfig.created`.
- Tests: store (sorted listing, shared-layer removal, two names for one image), CLI parsing, name/time formatting. Verified on the real bundle: ls shows the 3 images; removing all freed 738 MiB (store blobs back to 120 KiB).
