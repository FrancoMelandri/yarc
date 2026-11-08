# M1b — Blob store and parallel layer download

Status: **done**. 40 unit tests pass; 3 live tests against Docker Hub pass (`alpine:3.20`, `nginx:alpine-slim` with 7 layers). No build warnings.

## What was done

All in `crates/yarc-oci`.

### `store.rs` — `BlobStore`
Content-addressed store on disk:

```
<root>/blobs/sha256/<hex>   verified blobs (manifests, configs, layers)
<root>/tmp/                 in-flight downloads
<root>/refs/<hex>.json      image reference -> manifest digest
```

- `put_stream(digest, reader, expected_size)`: streams into `tmp/` while hashing with SHA-256, then checks size and digest and renames atomically into `blobs/`. Anything that fails verification is deleted; `blobs/` never contains partial or corrupt data.
- The reader is capped at `expected_size + 1` bytes, so a registry that never stops sending cannot fill the disk (tested with an endless reader).
- Already-present blobs are skipped without reading the stream. Concurrent writers of the same blob are safe (unique temp names; if the rename loses a race and the blob exists, it counts as success).
- `put_bytes`, `read_blob`, `open_blob`, `contains`, `blob_path`.
- `set_ref` / `get_ref`: record which manifest digest a reference (e.g. `alpine:3.20`) resolves to. Written via temp file + rename. Needed by M1c to find an image by name.

### `pull.rs` — `pull(client, store, image, jobs) -> PullReport`
1. Stores the config and platform manifest (needs the raw bytes, which `ResolvedImage` now keeps).
2. De-duplicates layers by digest, then downloads them with up to `jobs` worker threads (`std::thread::scope`).
3. Layers already in the store are skipped; the first failure stops the other workers.
4. Only when everything succeeded is the reference recorded, so a recorded ref always points at a complete image.

`PullReport { downloaded, skipped, bytes_downloaded }`.

### `client.rs` changes
- `ResolvedImage` gained `manifest_bytes` and `config_bytes`.
- New `RegistryClient::fetch_blob(reference, digest) -> Response` for streaming (shares the same auth/token logic).

### Test infrastructure
`testutil.rs` (test-only): the scripted `Fake` transport moved out of the client tests so the pull tests share it, with `replace` and `count_calls` helpers.

## Tests
- Store: round trip across multiple buffers, idempotent second put, digest mismatch, short and oversized streams (size mismatch), endless stream cut off, 8 concurrent writers of one blob, ref round trip and overwrite. Each failure case also asserts `tmp/` is empty.
- Pull (fake registry, real client): everything lands in the store and the ref is set, second pull makes no layer requests, duplicate layers fetched once, 12 layers with 3 workers, corrupt layer rejected and ref not recorded, wrong layer size rejected, HTTP 500 on a layer fails the pull.
- Live (`cargo test -p yarc-oci --test live -- --ignored`): single-layer and 7-layer pulls, then a second pull that downloads nothing.

## Verified in the live run
The concern noted in M1a (layer blobs redirect to a CDN, and the `Authorization` header must not follow across hosts) works with `ureq`'s defaults: all layers downloaded and verified.

## Decisions
- Blobs are verified before they become visible, never after. Digest and size come from the manifest, which was itself verified.
- A recorded ref means "complete image". An interrupted pull leaves harmless extra blobs but no ref.
- Threads, not async, for parallel downloads (see the `Transport` seam described in M1a).

## How to verify

```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-oci'
# live, needs network:
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-oci --test live -- --ignored --nocapture'
```

## Not done / follow-ups
- No download progress reporting (a callback could be added to `pull` for the CLI).
- No resume of partially downloaded blobs; no retry/backoff or timeouts.
- No garbage collection of unreferenced blobs, and `tmp/` leftovers from a crashed process are not cleaned up.
- Store file permissions are the process defaults.
- Layer media types are not inspected; M1c decides what it can unpack (gzip / uncompressed, zstd maybe).

## Next: M1c — unpack and CLI
Apply layers in order into a rootfs directory with whiteout handling and path-traversal/symlink-escape protection, add `clap`, and expose `yarc pull <image>` and `yarc image unpack <image> <dir>`.
