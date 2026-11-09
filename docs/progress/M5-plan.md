# M5 — Plan: the agent and Windows via WSL2

Status: **decisions taken** (transport: on-demand agent over stdio; existing distro first; image cache first). M5d, M5a1 and M5a3 done; next is M5a4.

## Starting point
The `yarc` binary does everything on Linux: the CLI calls `yarc-runtime` directly and detached containers are held by shims. `yarc-agent` and `yarc-proto` are empty crates. On Windows only `pull`, `load`, `image` and `project config` work.

Goal: `yarc.exe` on Windows drives a Linux `yarc` inside WSL2 as if it were local.

## Decisions
1. **Transport: on-demand agent over stdio.** `yarc.exe` starts `wsl.exe -d <distro> -- yarc agent --stdio`; the agent serves requests on that pipe and exits when it closes. No daemon, no socket. A `--socket` mode can come later (macOS, warm sessions). Native Linux keeps calling `yarc-runtime` directly; the agent is the remote door to the same functions.
2. **Protocol:** newline-delimited JSON frames: version handshake, request id, typed requests, and a stream of events (`Stdout`, `Stderr`, `Progress`, `Result`, `Exit`), plus `Cancel` for Ctrl-C. `yarc-proto` has no runtime dependencies. Typed requests for every command from the start; output begins as text stream frames and `ps`, `project ps`, `image ls`, `config` become typed results first.
3. **Distro:** the user's existing one (`--distro` / `YARC_WSL_DISTRO`, else the default); `yarc setup` with a dedicated distro comes last (M5c).
4. **Linux binary:** `YARC_WSL_BIN`, else `yarc` on the distro's PATH; later `setup` installs a musl build.
5. **Store:** inside the distro (`~/.local/share/yarc`), never on `/mnt/c`.
6. **Windows paths:** typed path fields translated by the agent side/client to `/mnt/c/...`; warn once about slow 9p bind mounts and recommend named volumes.
7. **Ports and Ctrl-C:** verify WSL2 localhost forwarding and cancel handling (M5b).

## Steps (each ends with a progress doc, then stop)
- **M5d** shared image cache for the live tests. ✔
- **M5a1** ✔ `yarc-proto`: frames and messages, handshake, round-trip and golden wire-format tests.
- **M5a2** dropped: an output sink cannot capture what the runtime itself prints, so the agent runs each request as a child `yarc` process instead (see M5a3).
- **M5a3** ✔ `yarc-agent` + hidden `yarc agent --stdio`: typed request -> argv, child process per request, pipes relayed as events, `cancel` = SIGINT; tested with a fake yarc, the real binary and live containers.
- **M5a4** Windows client: spawn the agent through `wsl.exe`, translate paths, relay events and exit code, `yarc doctor` (WSL, distro, binary, pasta, user namespaces); live test `yarc.exe up -d / ps / down`.
- **M5b** ports and signals: localhost forwarding, Ctrl-C as `Cancel`, `logs -f`; `exec` stays without a pty.
- **M5c** `yarc setup`: dedicated distro and musl binary (confirm with the user first).

## Risks
`wsl.exe` argument quoting and UTF-16 output of `wsl -l`; slow `/mnt/c` mounts; a distro without `passt` (outbound degrades with a warning, as now); the output-sink refactor (M5a2) touches most commands.

## Open
Forwarding automatic on Windows by default (assumed) or behind a flag until trusted.
