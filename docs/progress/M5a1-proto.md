# M5a1 — `yarc-proto`: the CLI <-> agent protocol

Status: done. Nothing committed. Not used by anything yet (the agent is M5a3, the client M5a4).

## What
`crates/yarc-proto` (deps: serde, serde_json) defines newline-delimited JSON frames.

- **Client -> agent (`ClientFrame`):** `hello {version, client}`, `request {id, globals, command}`, `cancel {id}`.
- **Agent -> client (`AgentFrame`):** `hello {version, agent}`, `event {id, event}`, `fatal {message}`.
- **`Event`:** `stdout {text}`, `stderr {text}`, then exactly one of `exit {code}` / `failed {message, code}`.
- **`Globals`:** `root`, `file`, `project` (the global CLI options).
- **`Command`:** one typed variant per CLI command: pull, load, image ls/rm/unpack, create, run, ps, container ls, start, exec, stop, kill, wait, logs, rm, up, down, project ps/logs/stop/config. `CreateSpec` is shared by `create` and `run` (flattened in `run`).
- **Paths** in requests (`file`, `load.file`, `image_unpack.dir`, `globals.root/file`) are paths as the agent sees them; a client on another OS translates those fields before sending (M5a4).
- **Codec:** `write_frame` / `read_frame` (one line per frame, newlines inside text are escaped, blank lines skipped, 1 MiB limit on both sides), `check_version` (versions must be equal; no negotiation yet).
- **Compatibility rule:** unknown fields are ignored and optional ones default, so either side can grow within a version; changing any golden string in the tests means a new `PROTOCOL_VERSION`.

## Not here yet
Typed results (`Event::Result`) and `Progress`: added when `ps`, `project ps`, `image ls`, `config` become typed (after M5a3). Output travels as text events until then.

## Tests (10)
Round trip of every command; golden wire format; `run` flattening; defaults and unknown fields; unknown types rejected; framing and ordering; blank lines and missing final newline; malformed/invalid UTF-8; oversize both ways (nothing partial written); version check. Workspace builds without warnings.
