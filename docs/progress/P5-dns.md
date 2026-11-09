# P5 — DNS in project networks (AERO bundle)

Status: **done.** Workspace tests pass (`yarc-runtime` 53, `yarc-cli` 81 unit + integration, `yarc-config` 13, `yarc-oci` 71), no warnings. Verified in the real AERO bundle. Nothing committed.

## Why
`ssw-platform-pms-aero` calls `AERO_CLOUD_URI=https://dev-keyctrl.allegion.com`; project containers had no `/etc/resolv.conf`, so no host name resolved (P3 limit).

## What was done
- `pasta` is started with `--dns-forward 169.254.0.53`: inside the project namespace that link-local address is mapped to the host's first resolver (UDP/53). This is needed because the host's own nameserver (on WSL: `10.255.255.254`) lives on the host's loopback and cannot be reached from the namespace.
- `up` writes `<net dir>/resolv.conf` (`nameserver 169.254.0.53` plus the host's `search` domains, max 6) when the network has outbound access, or an **empty** file when it has not (lookups then fail at once instead of hanging). Every project container bind-mounts it over `/etc/resolv.conf` (optional mount: a network from before this change has no file and is skipped).
- Wiring marker 3 -> 4: existing project containers are recreated once to get the mount.
- Tests: search-domain parsing, resolv.conf content with/without outbound, rewrite, the mount plan, pasta arguments.

## Verified (AERO bundle, WSL2)
`/etc/resolv.conf` = `nameserver 169.254.0.53` + `search ALLEGION.COM`; `dev-keyctrl.allegion.com` resolves (CloudFront IPs), `example.com` resolves, service names still resolve through `/etc/hosts`; TCP 443 connects; `openssl s_client` verifies the server certificate (issuer Amazon RSA 2048 M04) with the container's CA bundle. The corporate CA from `certs/` is installed by the image's entrypoint (`entrypoint: trusting extra CA ...`).

## Not done / limits
- DNS over TCP (answers larger than a UDP packet, DNSSEC-heavy zones) is not forwarded by pasta; IPv6-only resolvers are not used (IPv4 only).
- `options`/`nameserver` entries other than the first host resolver are ignored.
- **Not yet run:** a real request from the PMS through pms -> aero -> cloud (it would send the bearer token to the dev cloud), and pms connecting to the PMS at 192.168.1.14 (not reachable from the test machine within 35 s).
