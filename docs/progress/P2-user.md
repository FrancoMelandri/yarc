# P2 — `user:` in `yarc.yaml` (PMS bundle, step 2)

Status: **done.** `yarc-config` 13, `yarc-oci` 71, `yarc-cli` tests pass, no warnings. Nothing committed.

## What was done
- `user:` per service replaces the image's `USER` (like `docker run -u` / compose `user:`). Accepts `root`, `"1000:1000"` or a bare number (`user: 0`); an empty value is an error. Omitted keeps the image's user.
- Passed to the runtime as the `user` override of `create_new`; `project config` prints it.
- The definition hash includes it **only when set**, so existing containers are not recreated by this change; adding, changing or removing `user:` recreates the service.
- Tests: config (names, ids, numbers, absent, empty), hash changes with `user`.

## Important limit (unchanged)
The rootless runtime still maps a single uid to 0, so the only accepted values are `root`/`0`. Any other user (including the image's `pms`) fails with `rootless containers can only run as root (uid 0) for now`. `user: root` is the way to run images that declare a non-root `USER`; the files inside such images are root-owned, so this works for the PMS bundle. Real multi-uid support is a separate, larger step.

## Verified on the real bundle
Store loaded with `yarc load images.tar` (unmodified images). With `user: root` the pms container starts and the Java app runs ("Start Application - CISA AERO PMS"); removing the line makes `up` fail with the message above and recreates the service when it is put back.

## Notes
- `yarc load` counts blobs in the archive that no image references (26 here, tiny) as "added"; `image rm` never deletes them because it only removes an image's own blobs. Harmless; a store-wide `gc` would clean them.
