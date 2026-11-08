# M1a — Image references, registry auth, manifest resolution

Status: **done**. 26 unit tests pass; the live Docker Hub test (`alpine:3.20`) passes. No build warnings.

## What was done

All in `crates/yarc-oci`. Public entry point:

```rust
let client = RegistryClient::new(UreqTransport::new());
let image  = client.resolve(&"postgres:16".parse::<Reference>()?, &Platform::host())?;
// image.manifest_digest, image.manifest.layers, image.config (Entrypoint/Cmd/Env/…)
```

| module | responsibility | I/O? |
|---|---|---|
| `reference.rs` | Parse `alpine`, `postgres:16`, `ghcr.io/o/a:1`, `localhost:5000/x`, `name@sha256:…`. Docker Hub defaults (`registry-1.docker.io`, `library/` prefix, `latest`). Digest wins over tag. Builds manifest/blob URLs; `http` for localhost, `https` otherwise. | no |
| `digest.rs` | `sha256:<hex>` type; compute and verify. | no |
| `auth.rs` | Parse `WWW-Authenticate: Bearer …` (quote-aware), build token URL (percent-encoded), parse token response (`token` / `access_token`). Basic auth is rejected for now. | no |
| `manifest.rs` | OCI + Docker v2 manifest and index types, shape-based detection, platform selection (`unknown/unknown` attestation entries never match unless asked), image config (`Entrypoint`, `Cmd`, `Env`, `WorkingDir`, `User`, `ExposedPorts`, `diff_ids`). Schema v1 rejected. | no |
| `transport.rs` | `Transport` trait + `Response` (with size-limited body read) + `UreqTransport` (blocking, rustls, follows redirects). | yes (only here) |
| `client.rs` | `RegistryClient`: anonymous bearer-token flow with per-repository token cache, manifest → index → platform manifest → config. Verifies digests of the platform manifest, the config, and a top-level manifest fetched by digest. 4 MiB cap on JSON bodies. | via `Transport` |

### Async-readiness (requested)
HTTP is only reachable through the `Transport` trait; everything else is pure and synchronous. To add async later: implement an async transport and an async mirror of `RegistryClient::get/resolve` (or wrap the sync client in `spawn_blocking`). The pure modules are reused unchanged. The trait also made it possible to test the full auth flow with a scripted fake transport, without a network.

### Tests
- Reference parsing table (Hub defaults, custom registries, digests, invalid input, URLs).
- Digest known vector, malformed digests, mismatch detection.
- Challenge parsing (Docker Hub header, quoted commas, unquoted values, Basic/no realm rejected), token URL, token response variants.
- Platform selection, Docker vs OCI manifests, schema v1, config with `null` fields.
- Client: full 401 → token → index → child manifest → config flow (asserts the first request is anonymous and later ones carry `Bearer tok123`), `Accept` header lists all four media types, tampered platform manifest rejected, missing platform, plain (non-index) Docker manifest, 404 surfaces status, a second 401 after a token fails instead of looping.
- `tests/live.rs` (`#[ignore]`, run with `-- --ignored`): resolves `alpine:3.20` on Docker Hub for the host platform.

## Decisions
- `ureq` 3.x for HTTP (blocking), threads later for parallel layer download; async possible via the trait seam.
- Manifest vs index is decided by shape (`manifests` / `layers` keys), not by `Content-Type`, which registries set inconsistently.
- A manifest fetched by tag has its digest computed locally; there is no tag to verify against, so we trust the registry for the tag → content mapping.

## How to verify

```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-oci'
# live, needs network:
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-oci --test live -- --ignored --nocapture'
```

## Not done / follow-ups
- No credentials for private registries (Basic auth to the token endpoint, `~/.docker/config.json`).
- No blob download yet (M1b). Note for M1b: layer blobs on Docker Hub redirect to a CDN; `ureq` does not forward `Authorization` across hosts by default, which is what we want, but it needs checking against the live registry.
- No retry/backoff on transient errors, no request timeouts configured.
- `variant` is ignored in platform selection (first `linux/arm64` wins).

## Next: M1b — blob store and download
Content-addressed store at `<root>/blobs/sha256/<hex>`, streaming download with on-the-fly hashing, atomic rename, skip blobs already present, parallel layer downloads.
