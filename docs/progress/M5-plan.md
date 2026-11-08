# M5 — Plan: the agent and Windows via WSL2

Status: **planning only, nothing implemented. Waiting for the user's decisions** (to be given on the next session).

## Starting point
The `yarc` binary already does everything on Linux: the CLI calls `yarc-runtime` directly and detached containers are held by shims. `yarc-agent` and `yarc-proto` are empty crates. On Windows only `pull`, `image` and `project config` work; everything else says "run yarc inside WSL".

Goal of M5: `yarc.exe` on Windows drives a Linux `yarc` inside WSL2 as if it were local.

## Decision 1 — transport between the Windows CLI and Linux
- **A. Thin forwarding (recommended).** `yarc.exe up` runs `wsl -d <distro> -- yarc up …`, streams stdio and the exit code, and translates Windows paths (`--file`, `--root`, volume sources) to `/mnt/c/...`. No protocol, no daemon: the Linux `yarc` is the agent. Cost: one WSL process spawn per command (~100–300 ms); text pipe, not typed messages.
- **B. Typed protocol over a socket.** `yarc-proto` JSON requests/responses, `yarc-agent` as a Linux daemon or socket-activated process; reached via `wsl.exe` stdio or localhost TCP. The "real" agent design, also suits macOS. Much more work and a long-lived piece we avoided so far.
- **C. Hybrid.** Build A now; introduce `yarc-proto` at M6 when macOS forces it, keeping the Linux command surface clean so the swap is local.

## Other decisions
2. **WSL distro:** (a) use the user's existing distro (`--distro` / `YARC_WSL_DISTRO`) first, then (b) `yarc setup` imports a dedicated minimal distro (e.g. an Alpine rootfs pulled with our own puller). Recommended: (a) first, (b) as M5c.
3. **Linux binary inside WSL:** dev = configurable path to the binary built in WSL (`~/yarc-target/...`); later `setup` installs a `x86_64-unknown-linux-musl` build. Recommended as stated.
4. **Store location:** inside the distro's ext4 (`~/.local/share/yarc`), never on `/mnt/c`.
5. **Volumes from Windows paths:** `./data` translated to `/mnt/c/...` (works, 9p is slow, ownership odd). Translate and warn once; document Linux paths / named volumes as the fast route.
6. **Published ports:** WSL2 localhost forwarding should already expose `127.0.0.1` ports bound in the distro to Windows. Verify first; add a Windows-side TCP relay only if needed.
7. **Interrupts:** Ctrl-C on `yarc.exe up` must reach the Linux `yarc` (stop project, exit 130). Verify `wsl.exe` forwards it; cover with a test.

## Proposed steps (each ends with a progress doc, then stop)
- **M5a — host-side forwarding.** Windows/macOS host path: detect WSL, pick the distro, translate paths and rewrite args, forward stdio and exit codes, `yarc doctor` for WSL state. Unit tests for path translation / arg rewriting; live test `yarc.exe up/ps/down` against WSL.
- **M5b — ports and signals.** Check localhost forwarding, Ctrl-C, `logs -f`, `exec` (no pty exists yet).
- **M5c — `yarc setup`.** Import a dedicated distro and install the Linux binary.
- **Optional M5d — shared image cache for live tests** (pull once per run, copy the blob store into each test store); avoids Docker Hub 429s. Suggested to do it first.

## Questions to answer next session
1. Transport: A, B or C? (recommended: A or C)
2. Distro: existing first or dedicated first? (recommended: existing first)
3. Image-cache step: first or last? (recommended: first)
"defaults" is an acceptable answer.

## Also pending from M4d
Re-run the four live suites (`live_volumes`, `live_network`, `live_project`, `live_run`) once the Docker Hub rate limit has reset.
