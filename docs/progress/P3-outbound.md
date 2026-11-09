# P3 — Outbound network with `pasta` (PMS bundle, step 3)

Status: **done.** Workspace tests pass (`yarc-runtime` 50, `yarc-cli` 78 unit + 6 + 5 + 2, `yarc-config` 13, `yarc-oci` 71), no warnings. Verified live with the real PMS inspector image. Nothing committed.

## What was done
- The project holder now gives the shared network namespace an **outbound IPv4 network** using [`pasta`](https://passt.top) (Ubuntu/Debian package `passt`): the namespace gets `eth0` with the host's address and default route, and TCP/UDP connections leave through the host (no root needed).
- How (`yarc-runtime/src/netns.rs`): before it unshares, while it is still single-threaded in the host's namespaces, the holder forks a helper. The helper blocks on a pipe; once the holder has created its user+net namespace and brought `lo` up, the holder releases it and it `exec`s
  `pasta --foreground -4 --config-net -t none -u none -T none -U none <holder pid>`. The holder waits (3 s max) until `/proc/self/net/dev` shows an interface besides `lo`.
- `-t/-u/-T/-U none`: pasta's automatic port forwarding is **off**. Published ports stay the holder's job; with `auto`, everything a service listens on would have been exposed on the host (checked: inspector's 8080 is not reachable from the host).
- Lifecycle: pasta is the holder's child (`PR_SET_PDEATHSIG` SIGTERM), stopped when the holder stops and when it is killed with SIGKILL (checked). `down` leaves no pasta behind.
- Never fatal: no pasta, a broken pasta or one that does not configure a network leaves the project working with loopback only, and `up` prints a warning with the reason. Recorded in `net.json` (`outbound`, `outbound_note`; old files without them still read).
- Finding pasta: `YARC_PASTA=/path` or the first `pasta` on `PATH`; `YARC_PASTA=` (empty) turns outbound off. Hidden `__holder --pasta <path>`.
- A network created before this change keeps running without outbound (and `up` warns); `yarc down` + `yarc up` recreates it.

## Verified
With the real `ssw-platform-pms-inspect` image in a project: `exec` shows `lo` and `eth0`; a TCP connect to `1.1.1.1:443` from inside succeeds; host `127.0.0.1:8080` refused; SIGKILL of the holder removes pasta; `YARC_PASTA=` and `YARC_PASTA=/bin/false` give the warning and a working loopback-only project.

## Not done / known limits
- **DNS:** project containers still get no `/etc/resolv.conf`, so host *names* do not resolve (IPs work, which is what the PMS bundle uses). Copying the host's resolv.conf when outbound is on is the obvious next step.
- A service cannot reach a service listening on the **host** (`--map-host-loopback` not enabled); the namespace has the host's own IP, so connecting to it stays inside the namespace.
- IPv4 only; TCP/UDP outbound only (no inbound except published ports).
- A project with an already running holder is not upgraded automatically.
- Not yet checked: reaching a PMS on the hotel LAN from a Windows-hosted WSL2 (NAT mode) — it behaves like the WSL distro itself.
