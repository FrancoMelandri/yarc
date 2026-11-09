# M5a3 — `yarc-agent` and `yarc agent --stdio`

Status: done. Nothing committed. M5a2 (output sink) was dropped in favour of this design, agreed with the user.

## Why no output sink
`yarc-runtime` writes to the real stdout/stderr itself (foreground container output is teed to them, warnings, `exec` inherits the descriptors), so an in-process sink in `yarc-cli` would not capture everything. Instead the agent runs each request as a **child process of the same `yarc` binary** and relays its pipes.

## What
- `crates/yarc-agent` is now a library (deps: `yarc-proto`, `libc`).
  - `argv.rs`: `to_args(globals, command)` builds the command line of the equivalent CLI call. Options are `--name=value`, positionals follow `--`, so no client-sent value can become an option (an image called `--root=/etc` stays an image name). Empty required lists (`ids`, `images`, `cmd`) are rejected before anything runs.
  - `server.rs`: `serve(input, output, exe)`:
    - handshake (first frame must be `hello`, versions must match, else a `fatal` frame and an error);
    - per request: spawn `exe` (own process group, no stdin, piped stdout/stderr) in the reader thread, so a `cancel` right after finds it; a thread relays stdout and stderr as `Stdout`/`Stderr` events (UTF-8 characters split across reads are rejoined: `utf8.rs`) and ends with `Exit {code}` (`128+n` if killed by signal n) or `Failed` (code 125: bad arguments, cannot spawn, request id already running);
    - `cancel {id}` is SIGINT to that child; unknown ids are ignored; requests run concurrently;
    - the child's pid is removed from the table *before* it is reaped (`waitid(WNOWAIT)`), so a late cancel can never hit a reused pid;
    - a grandchild that keeps the pipes open after the command exited (daemons from `up -d`) cannot delay `Exit`: once the child is gone a quiet pipe ends the relay;
    - when the input ends, children get SIGINT, then SIGKILL after 5 s.
- `yarc agent --stdio` (hidden) in `yarc-cli` serves on stdin/stdout, running `current_exe` for each request. Linux only.

## Tests
- `yarc-agent` 23 unit tests, with a fake `yarc` shell script: argv mapping for every command, UTF-8 chunking, output/exit codes, command line from globals, cancel only hits its request, closing the stream interrupts, concurrency, duplicate ids, 125 failures, signal exit codes, grandchild pipes, split characters, handshake and garbage.
- `tests/agent.rs` (2): the real binary through the agent (`image ls`, error text and status, option-looking values, `project config` with the requested file and name).
- `tests/live_agent.rs` (2, `--ignored`): `run` through the agent streams output and exit status 4; `cancel` of a foreground `up` ends it with 130 and "Gracefully stopping", then `down`.
- Workspace builds without warnings; all other suites unchanged.

## Limits
No stdin for requests (no interactive `exec`). Output is text events; typed results come later through a hidden JSON format on a few commands. A stream with no ending newline is relayed when it arrives, not line-buffered. Windows paths are translated by the client (M5a4), not the agent.
