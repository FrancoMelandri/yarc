# M4d — Volumes

Status: **done**, with one verification gap (see "Verification status"). Unit and offline tests pass with no warnings (`yarc-runtime` 75, `yarc-cli` 44 unit + 6 + 5 + 2 integration, `yarc-config` 11, `yarc-oci` 58). The 9 new live volume tests passed against Alpine 3.20.

## What was done

### In the file
`volumes: [ "SOURCE:TARGET[:ro]" ]` (already parsed since M0) now takes effect:
- **Host path** — absolute, relative to the directory of the project file (`./data`, `../shared`), or `~` / `~/x` (home directory). Paths are normalized lexically. A path that does not exist yet is **created as a directory** (like `docker run -v`). A file is mounted as a file.
- **Named volume** — a bare name (`pgdata`) is a directory `<root>/volumes/<project>_<name>`, created on first use and shared by every service of the project that names it. Names may contain letters, digits, `_ . -`.
- `:ro` mounts read-only.
- The target must be absolute, not `/`, and contain no `..`; a bad volume is reported before anything is started.

### In the runtime
- `ContainerSpec.volumes` (`VolumeSpec { source, target, read_only }`; optional in old `config.json`).
- The setup plan adds one recursive `Bind` per volume **last**, so a volume wins over anything generated (e.g. `/etc/hosts`). Mount points are created inside the rootfs through the usual symlink-safe path check.
- **Read-only remounts preserve the source filesystem's locked flags.** Inside a user namespace `nosuid`, `nodev`, `noexec` and the atime options of the source mount cannot be cleared, so remounting read-only without them fails with `EPERM` (e.g. for a source on `/dev/shm`). `locked_flags` reads them with `statvfs` and carries them over. This also hardens the read-only `/sys` bind.
- Files written by the container are owned by you on the host (root inside is you outside).

### In the CLI
- `yarc down -v` / `--volumes` also deletes the project's **named** volumes (their data is lost); host paths are never touched. It needs the project file to know which volumes are the project's (otherwise it refuses). `down` without `-v` keeps all data; `project stop` has no `-v`.
- `project config` lists each service's volumes.
- Changing the volumes of a service recreates its container (the definition hash covers them); the hash also carries a wiring-model marker (now 3), so containers made before volumes existed are recreated once.

## Tests
- **Unit:** source resolution (relative, `..`, `./a/../b`, absolute, `~`, bare `~`, named with `:ro`), rejected volumes (root target, `..`, `~` without HOME, bad volume name — the message names the volume), formatting and the list of named volumes (deduplicated, host paths excluded), lexical path cleaning (found a bug: `/../x` must be `/x`), the setup plan (volumes last; files as files, directories as directories), spec JSON compatibility, CLI parsing of `down -v`.
- **Live (`live_volumes.rs`, 9):**
  - bind mount shared both ways, files owned by you, changes on the host visible live through `exec`;
  - missing host directories created;
  - read-only volume cannot be written to; same for a source on `/dev/shm` (`nosuid,nodev`);
  - single-file mount;
  - named volume survives recreation and `down`, is deleted by `down -v`, and a fresh one starts empty;
  - two services share a named volume (one read-only);
  - changing a volume recreates the service;
  - an invalid volume is reported before anything starts.

## Verification status
- The 9 live volume tests all passed on the first run of this step.
- A *second* full run of all four live suites (`live_volumes`, `live_network`, `live_project`, `live_run`) was then blocked: **Docker Hub returned HTTP 429** (anonymous pull limit) because every live test pulls Alpine into a fresh store and many runs were made today. Nothing failed for another reason.
- Instead, one combined scenario was run by hand against a *copy of the local image store* (no registry access), with the final code: project network + health-gated `needs: healthy` + published port + bind mounts read-write and read-only + a named volume shared between two services + a read-only mount from `/dev/shm` + `down -v`. All behaved as expected.
- **Please re-run the four live suites once the limit has reset** (a few hours). See the follow-up below to stop this from recurring.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_volumes -- --ignored --test-threads=1'
```
(and `live_network`, `live_project`, `live_run` the same way.)

### Try it
Inside WSL:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cd /mnt/c/Projects/MINE/yarc && cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

mkdir -p ~/demo4/site && cd ~/demo4
echo "<h1>from the host</h1>" > site/index.html
cat > yarc.yaml <<'EOF'
services:
  writer:
    image: alpine:3.20
    cmd: [sh, -c, "echo run-$(date +%s) >> /data/runs; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    volumes: ["store:/data", "./site:/site:ro"]
  reader:
    image: alpine:3.20
    cmd: [sh, -c, "trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    volumes: ["store:/data:ro"]
EOF
$yarc project config                                  # shows the volumes of each service
$yarc up -d
$yarc exec demo4-reader-1 cat /data/runs              # written by "writer" through the shared named volume
$yarc exec demo4-writer-1 cat /site/index.html        # the host file
$yarc exec demo4-writer-1 sh -c 'echo x > /site/f'    # Read-only file system
$yarc exec demo4-writer-1 sh -c 'echo hi > /site/../data/hello'; cat ~/.local/share/yarc/volumes/demo4_store/hello
$yarc down                                            # the volume stays...
$yarc up -d && $yarc exec demo4-reader-1 cat /data/runs    # ...so the history is still there (2 lines)
$yarc down -v                                         # ...unless you ask for it: "volume demo4_store removed"
```

## Not done / follow-ups
- **Test infrastructure:** every live test pulls its image from Docker Hub, which now hits the anonymous rate limit. A shared image cache for the live tests (pull once per run, copy the blob store into each test's store) would make them faster and immune to this. Not done yet; proposed as the next small step.
- A named volume is created **empty**; unlike Docker, the image's content at the mount point is not copied into it on first use.
- Volumes cannot be given on `yarc run` / `yarc create` (no `-v` flag); only projects have them.
- A volume target below a symlink inside the image (e.g. `/var/run/x` where `/var/run -> /run`) is refused by the symlink-safe mount-point check; use the real path (`/run/x`).
- `~` is resolved by yarc with `$HOME`; `~user` is not supported.
- No ownership mapping: with the single-uid mapping every file looks owned by root inside, and by you outside. Images that expect a specific non-root owner on the mounted directory cannot be satisfied until multi-uid support exists.
- Volume sizes, drivers and options (compose's long syntax) do not exist.

## Next: M4 is complete — what comes after
M5: the agent (`yarc-agent` + `yarc-proto`) and Windows via WSL2: the host-side CLI imports a WSL distribution and talks to the agent inside it. It needs a planning round first (control protocol, how the host finds the agent, path translation for volumes, forwarding of published ports).
