# M4c — Project network: shared namespace, service names, published ports

Status: **done**. Workspace tests pass with no warnings (`yarc-runtime` 73, `yarc-cli` 40 unit + 6 + 5 + 2 integration, `yarc-config` 11, `yarc-oci` 58). Live tests (`--ignored`): 8 new network tests, plus the 10 project and 30 container tests, all pass.

M4 decisions (defaults) implemented: one shared user + network namespace per project, ports bound to `127.0.0.1`, TCP only.

## What was done

### The model
```
host network namespace                         project network namespace (owned by the holder)
 127.0.0.1:8080 ← listener (bound first)         lo ← db :9000, web, ...   (all services share it)
        └── accept ─► holder thread ─ connect ─►  127.0.0.1:9000
```
- **Holder (`yarc __holder <project> --publish H:C...`, `netns.rs`):** one small long-lived process per project, started detached by `up` (own session, like a shim). It
  1. takes the project's `flock` (`net.lock`) — liveness is the lock, never a pid;
  2. **binds the published ports first**, in the host's network namespace (so a port in use is reported by `up` as `cannot publish 127.0.0.1:N: ...`);
  3. `unshare(user + net)`, maps the invoking user to root in its new user namespace, brings `lo` up;
  4. records its pid and the published ports in `net.json`, reports `ready` to `up`;
  5. serves: every accepted connection is relayed to `127.0.0.1:<container port>` *inside* the project network (threads created after the `unshare` live in it), copying both ways with half-close.
- **Containers join it** instead of creating their own user/net namespaces: the monitor opens `/proc/<holder>/ns/{user,net}` and `setns`es the user namespace first (we own it: same uid), then the network namespace, and only unshares mount/pid/uts/ipc, which are therefore owned by the project's user namespace (so `/proc` and `sysfs` can be mounted). The holder already mapped the ids, so the supervisor skips writing the maps. The holder is resolved **when the container starts**, so a restarted network works; with no live holder the start fails with "the project's network is not running; start the project with `yarc up`".
- **Service names:** `up` writes one hosts file (`projects/<name>/hosts`: `127.0.0.1 localhost db demo-db-1 web demo-web-1 ...`) that is **bind-mounted over `/etc/hosts`** of every service, so adding or removing a service updates all containers without recreating them. Services reach each other on `db:9000`, `localhost:9000` or by container name.
- **`exec` and health checks** join the container's own network namespace (the project's), so checks and `yarc exec` see what the service sees.

### `up` / `down`
- `up` makes sure the network exists with exactly the published ports of the file. If they changed, or the holder is gone, the containers still running in the old network are stopped (they cannot be moved), the network is recreated, and the services are started again in it (recreated if their definition changed). It prints `recreating the project network (...)`.
- `project stop` keeps the network; the next `up` resumes in it. `down` removes the containers, then stops the holder and deletes the network state, so the published ports close.
- Validation: a host port can be published only once per project (`host port 8080 is published by both a and b`); UDP mappings are accepted but ignored with a warning.
- `project ps` has a PORTS column (`127.0.0.1:8080->80/tcp`); `project config` shows the published ports (UDP marked `(not published)`).
- The container definition hash now includes a network-model marker, so containers made by M4a (host network) are recreated once.

### Runtime pieces
`ProjectNet` (`network.rs`: directory layout, lock, info, hosts file), `run_holder` / `stop_holder` (`netns.rs`), `Network::Project`, `ContainerSpec.project_net` + `hosts_file`, `ContainerSpec::in_project_network`. The setup plan treats `Project` like `None` for `/sys` (mounts `sysfs`, which the project's user namespace may do) and binds the shared hosts file instead of generating `/etc/hosts`.

## Tests
- **Unit:** `ProjectNet` (layout and name check, info round trip, lock-based liveness guarding the pid, hosts file content, removal), published-port validation and formatting, the setup plan for project containers (shared hosts file, no resolv.conf, `sysfs`), spec JSON compatibility, `--publish` parsing, CLI parsing of the hidden `__holder`.
- **Offline integration:** duplicate host ports rejected; `config` lists ports and marks UDP.
- **Live (`live_network.rs`, 8):**
  - web resolves `db` by name over the shared loopback and gets the answer; names in `/etc/hosts`; the host reads the same answer through the published port; `lo` is the only interface;
  - isolation: inside the network a loopback listener is reachable, a listener on the host's loopback is not;
  - two projects can use the same inner port at once, and taking one down leaves the other's port working;
  - changing the published ports recreates the network, closes the old port, opens the new one, restarts web into it;
  - a taken host port is reported as `cannot publish 127.0.0.1:N`, nothing is created, and a later `up` works;
  - `down` closes the ports and removes the network state; `project stop` keeps it;
  - a project container cannot be started by hand once the holder is gone (clear message), and `up` fixes that.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_network -- --ignored --test-threads=1'
```
(the `live_project` and `live_run` suites still pass too.)

### Try it
Inside WSL:
```bash
source ~/.cargo/env
export CARGO_TARGET_DIR=~/yarc-target
cd /mnt/c/Projects/MINE/yarc && cargo build -p yarc-cli
yarc=$CARGO_TARGET_DIR/debug/yarc

mkdir -p ~/demo3 && cd ~/demo3
cat > yarc.yaml <<'EOF'
services:
  db:
    image: alpine:3.20
    ports: ["38080:9000"]
    cmd: [sh, -c, "nc -ll -p 9000 -e /bin/echo pong"]
  web:
    image: alpine:3.20
    cmd: [sh, -c, "sleep 1; nc db 9000 > /tmp/answer; trap 'exit 0' TERM; while true; do sleep 0.2; done"]
    needs: { db: started }
EOF
$yarc up -d
$yarc project ps                              # PORTS: 127.0.0.1:38080->9000/tcp
$yarc exec demo3-web-1 cat /tmp/answer        # pong  (web asked "db" by name)
$yarc exec demo3-web-1 cat /etc/hosts         # every service name → 127.0.0.1
curl -s --http0.9 127.0.0.1:38080             # pong, from the host, through the proxy
$yarc exec demo3-web-1 sh -c 'cat /proc/net/dev | tail -n +3 | cut -d: -f1'   # only lo
ps -ef | grep -E '__holder|__shim' | grep -v grep   # one holder + one shim per service
sed -i 's/38080/38081/' yarc.yaml
$yarc up -d                                   # "recreating the project network (published ports changed)"
$yarc down                                    # containers, then "network removed"; the port closes
```

## Not done / follow-ups
- **No outbound network** from project containers: the shared namespace has only loopback. A userspace network stack would be needed (`pasta` or `slirp4netns`; neither is installed in this WSL, so it is not integrated). Containers that need to download things at runtime will fail; images already contain what they run.
- Two services of one project **cannot listen on the same port** (they share the loopback); different projects can.
- Published ports bind `127.0.0.1` only (no `0.0.0.0` or other address) and are TCP only; ports below 1024 cannot be bound rootless (`ip_unprivileged_port_start` is 1024).
- The proxy connects per connection and copies with threads (two per connection); fine for development traffic, not for heavy load. The client address seen by the service is always `127.0.0.1`.
- Changing the published ports restarts *all* running services of the project (they must join the new namespace).
- A service that listens only on its own container name or a non-loopback address inside the namespace is not reachable by the proxy (it connects to `127.0.0.1`).
- `/etc/hosts` is writable from inside; a container modifying it changes it for the whole project.
- If a holder is killed with SIGKILL its listeners die with it; `up` notices ("its holder is gone") and rebuilds.

## Next: M4d — volumes
Bind mounts (`./data:/var/lib/x[:ro]`, resolved relative to the project file) and named volumes (`<root>/volumes/<project>_<name>`), as extra `Bind` operations in the setup plan. With the single-uid mapping, host files owned by you appear as root inside.
