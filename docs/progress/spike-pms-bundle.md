# Spike — can yarc run the ssw-pms-fias-inspector bundle?

Status: **spike done (Phase 0), no repo code changed.** Run inside WSL2 Ubuntu-24.04 with a throwaway store in `~/spike/root` (hand-built from `images.tar`, a copy of the blobs plus `refs/*.json`) and a hand-written `~/spike/proj/yarc.yaml`.

## Results
| Question | Result |
|---|---|
| Do the layers (uncompressed tars) unpack? | **Yes.** Both images unpack and start. |
| Inspector (.NET 8080) | **Runs** in a project ("Now listening on http://[::]:8080"). |
| PMS (Java 11, otel agent, env, `/data` volume, secret as ro bind mount) | **Runs**: reads the env, finds `/run/secrets/pms_auth_key`, validates it (rejects a dummy key; the bundle's key file is empty, so the connect phase was not reached). |
| Image `User: pms` | **Blocker**: `rootless containers can only run as root (uid 0) for now`. Worked around by clearing `User` in the image config of the spike store. Needs a `user:` key in `yarc.yaml` or multi-uid support. Files in the image are root-owned, so running as root is fine functionally. |
| Service-name call pms -> `http://ssw-platform-pms-inspect:8080` | Resolved through the shared `/etc/hosts` (not exercised end to end because pms stopped at the key check). |
| Outbound TCP to the PMS IP | **Not implemented in yarc** (project netns has only `lo`). With `pasta` (Ubuntu `passt` package, not installed; extracted from the .deb into `~/spike/passt`) attached to the holder's netns (`pasta --config-net -4 <holder pid>`), the namespace gets eth0 with the host's address and default route, and a TCP connect to `1.1.1.1:443` succeeds. |

## Conclusions
1. The runtime is capable; the images need no changes.
2. Required work: `yarc load`, a `user:` key (or multi-uid), outbound networking via pasta (spawn it for the holder, find the binary, fall back with a clear message), and the config gaps (env interpolation, secrets, restart).
3. Not verified: a real FIAS conversation (needs a real auth key and a PMS or a fake FIAS server) and whether the WSL2 network can reach the hotel LAN the way Docker Desktop does (pasta uses WSL's own routing, so it equals what WSL itself can reach).
4. Pasta notes: it must run as the same user, joins the holder's user+net namespaces; the connected address inside the ns equals the host's, so the ns cannot reach the host's own IP (use `--map-host-loopback` for that).
