# M5d — Shared image cache for the live tests

Status: done. Nothing committed.

## Why
Every live test used a fresh store, so each one pulled `alpine:3.20` from Docker Hub (~70 pulls per full run) and hit 429s. That blocked re-running the four live suites left over from M4d.

## What
- `crates/yarc-cli/tests/common/mod.rs` (new): `seeded_root()` returns a new temp store pre-filled from a cache.
  - Cache: `$YARC_TEST_CACHE`, else `~/.cache/yarc-test-images`, else the temp dir. Content-addressed blobs and refs only, safe to keep or delete.
  - Filled on first use (once per process, behind a mutex) with `yarc pull` for each image in `IMAGES` (just `alpine:3.20`).
  - Blobs/refs are hard-linked into the test store (copied if the filesystems differ); they are never modified in place.
- `live_run`, `live_network`, `live_project`, `live_volumes` use `common::seeded_root()` for their store; project directories stay plain temp dirs.
- Adding an image to a live test means adding it to `IMAGES`.

## Found on the way
`live_network` still asserted the project namespace has only `lo`; since P3 (pasta) it also has one outbound interface. The assertion now accepts `lo` plus at most one more.

## Results
All four live suites pass with `--ignored --test-threads=1`: live_run 30, live_network 8, live_project 10, live_volumes 9. A warm run prints no "pulling" lines. Workspace tests pass with no warnings. This closes the "re-run the live suites" item from M4d.
