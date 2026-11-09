# M5a4 — Windows client: `yarc.exe` drives the agent in WSL

Status: implemented and tested from Linux with a stand-in for `wsl.exe`. **Not yet run on a real Windows host** (no Rust toolchain there; see "To try it on Windows"). Nothing committed.

## How it works
On Windows (or on Linux with `YARC_REMOTE=1`, which is how it is tested) every command except the ones below is not run locally: `yarc.exe` starts `wsl.exe [-d DISTRO] --exec YARC agent --stdio`, sends the request, relays stdout/stderr and returns the command's exit status. `--exec` runs the program directly: no shell, no quoting.

Handled on this machine: `project config` (only reads the project file), `doctor`, and the internal commands. Everything else, including `pull`, `load` and `image ...`, goes to WSL so that images land in the store the containers use.

Code, all in `crates/yarc-cli/src/remote/` (compiled on every OS so it can be tested on Linux):
- `winpath.rs`: `C:\a\b` -> `/mnt/c/a/b`, `\\?\C:\...`, `\\wsl$\<distro>\...` and `\\wsl.localhost\...` -> the Linux path (an error if it names another distro), Linux paths unchanged, `.`/`..` resolved without leaving the drive; relative paths are made absolute first (`std::path::absolute`); network shares, `C:x` and the like are clear errors.
- `forward.rs`: the parsed command line -> a protocol request. Translated: `--root`, `--file` (only for `up`, `down`, `project ...`; the default `yarc.yaml` is resolved against the current directory), `load`'s file, `image unpack`'s directory. Volume sources in the yaml are resolved by the Linux side relative to the translated file.
- `session.rs`: handshake, send, relay events. A protocol-version mismatch, a refusing/missing agent or a stream that ends early are explained and point to `yarc doctor`; a closed terminal does not lose the exit status.
- `wsl.rs`: the launcher, configured by `YARC_WSL_DISTRO` (default: WSL's default distro), `YARC_WSL_BIN` (the Linux `yarc`, default `yarc` on the distro's PATH; use an absolute path for a dev build, e.g. `/home/me/yarc-target/debug/yarc`; `~` is not expanded) and `YARC_WSL_EXE` (launcher, default `wsl.exe`).
- `doctor.rs` and `yarc doctor`: WSL distro reachable, `yarc` found inside it (warns if its version differs), unprivileged user namespaces (required), `pasta` (warning only). On Linux without `YARC_REMOTE` the same checks run locally. Exit status 1 if something required fails.
- `yarc agent` clears `YARC_REMOTE` for itself and the commands it runs. Found by a test run that forked processes recursively (the agent's own children forwarded again); all of them were killed, and the clearing is now in place.

## Tests
- Unit (in the binary): path translation, request building for every command (with the container prefix and without), session relay (exit codes, failures, other ids, version mismatch, fatal, truncated stream, closed terminal), launcher settings, doctor outcomes. Binary unit tests: 81.
- `tests/remote.rs` (5): the whole path with a fake `wsl.exe`: output and status, the store the agent was given, the relative project file resolved against the current directory, `project config` without any launcher, unusable launcher / missing binary messages, `doctor`.
- `tests/live_remote.rs` (1, `--ignored`): `up -d`, `project ps`, `down` with a real container through the forwarding path.
- Full workspace green without warnings; `yarc-proto`/`yarc-agent` also `cargo check` for `x86_64-pc-windows-gnu`. `yarc-cli` could not be cross-checked for Windows here (a C dependency of the registry client needs a mingw compiler).

## To try it on Windows (not done yet)
1. Install Rust on Windows (MSVC toolchain) and `cargo build -p yarc-cli`; the binary is `yarc.exe`.
2. Have a yarc build inside the distro and point `YARC_WSL_BIN` at its absolute path; `YARC_WSL_DISTRO` if it is not the default.
3. `yarc.exe doctor`, then `yarc.exe up -d` with a `yarc.yaml`, `ps`, `down`.

Things only a real run can confirm: how `wsl.exe --exec` finds `YARC_WSL_BIN`, console encoding of the output, and Ctrl-C (see below).

## Known limits / next
- Ctrl-C on `yarc.exe up`: for now Windows kills `yarc.exe` and `wsl.exe`, the agent sees its input close and interrupts what it runs, so `up` still stops the project. A proper `Cancel` frame on Ctrl-C is M5b.
- No stdin for forwarded commands (no interactive `exec`).
- Output is text; typed results later.
- The forwarding is automatic on Windows; there is no switch to run locally.

## Found on the first real Windows run
- **Staircase output.** While `wsl.exe` runs it leaves the shared console in a mode where a bare `\n` does not return to column 0, so every line we relayed started where the previous one ended. Fix: `remote/console.rs` writes `\r\n` for `\n` when (and only when) the stream is a console on Windows; redirected output stays LF. A lone `\r` (progress lines) is kept and an existing CR LF is not doubled, even across chunks. Unit tested on Linux; needs a re-check on Windows.
- `yarc-runtime` did not compile on Windows (`std::os::fd` / `StartOptions` not gated on Linux): fixed.
- `YARC_WSL_BIN` must be set to an absolute Linux path for a dev build; `yarc doctor` says so.

## Ctrl-C (done early from M5b, after "project logs -f cannot be stopped on Windows")
Cause (to be confirmed on Windows): `wsl.exe` shared our console and changed its input mode, so Ctrl-C was no longer delivered to `yarc.exe` as a signal (the same sharing caused the staircase output).
- `wsl.exe` now starts with `CREATE_NO_WINDOW`: its own hidden console. Its stderr is therefore piped and relayed (`relay_stderr`); its own messages are UTF-16LE on a pipe, decoded by `wsl_text`.
- `yarc.exe` installs a Ctrl-C/termination handler (`ctrlc` crate, `termination` feature: also closing the terminal) that sends `Cancel`; the agent turns it into SIGINT for the command, as at a Linux terminal, so `logs -f` ends with 130 and `up` stops the project. A **third** Ctrl-C exits `yarc.exe` immediately (130) in case the agent is stuck. `session::cancel/close` share the agent's input behind a mutex (`Shared<W>`).
- Tests: session `cancel`/`close`, `wsl_text`, and a live test (`live_remote`) that follows a log through the forwarding path, sends SIGINT to the client and expects exit 130. 307 tests green, no warnings on Linux.
- Still to confirm on real Windows: that Ctrl-C reaches the handler now, and that `wsl.exe` works in a hidden console with all three streams piped.
