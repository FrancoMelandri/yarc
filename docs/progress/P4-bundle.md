# P4 — Running the PMS bundle with yarc

Status: **runs up to the PMS authorization key.** The bundle's `secrets/pms_auth_key.txt` is empty, so the pms service cannot be taken past key validation here; the FIAS conversation itself (connect to a PMS, send commands to the inspector) is **not verified**. Nothing committed.

## What exists
`tmp/ssw-pms-fias-inspector/docker-bundle/yarc.yaml` — the compose file translated by hand (values inline, aspire dashboard / restart / stop grace left out; comments in the file say so).

```
wsl -d Ubuntu-24.04
cd /mnt/c/.../docker-bundle          # or copy the folder into WSL (faster volumes)
yarc load images.tar                 # once
echo -n '<PMS authorization key>' > secrets/pms_auth_key.txt
# edit PMS_FIAS_SERVER_IP_ADDRESS in yarc.yaml
yarc up -d ; yarc project logs ; yarc down
```

## Verified (real images, unmodified `images.tar`)
- `yarc load` -> `up -d`: inspector runs; pms starts as root, loads the otel agent, reads its env and finds the secret file; with the empty key it stops with `Invalid PMS Authorization Key in PMS_AUTH_KEY_FILE` (exit 255), as the real app would on Docker.
- Inside the project: the service name `ssw-platform-pms-inspect` resolves and port 8080 accepts a connection; outbound TCP (1.1.1.1:443) works through pasta.

## Not verified / to do with a real key
- pms connecting to a PMS (`Connected to PMS`) and commands appearing in the inspector log.
- Reaching the PMS network from Windows-hosted WSL2.
- Gaps vs compose, none blocking: no `${VAR}`/`.env` interpolation, no `secrets:` section (bind mount instead), no profiles (dashboard), no restart policy / `stop_grace_period`, `container_name` is `<project>-<service>-1`, DNS names do not resolve (IPs only), named volume `pms-data` lives under the yarc root, the image's non-root user is replaced by root.
