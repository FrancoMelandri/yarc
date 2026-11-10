# C4c — `restart:` in the project files (and `x-yarc`)

Status: **done**. Workspace tests pass with no warnings (`yarc-config` 69, `yarc-runtime` 101, `yarc-cli` 11 compose integration and the rest). 12 live restart tests pass (10 from C4b + 2 new project ones). Nothing is committed. **Both real bundles now load unchanged** from their `docker-compose.yml`.

## What was done
- **`restart:`** is a key of both `yarc.yaml` and compose: `no`, `always`, `unless-stopped`, `on-failure`, `on-failure:N`. Interpolation applies (`restart: ${POLICY:-always}`); a bad value is an error naming the service (`service 'a': restart: invalid restart policy 'often'`). In compose an unquoted `no` and `false` both mean `no`.
- `up` passes the policy to the container, so the shim supervises it. It is part of the **definition hash only when set**: changing it recreates the service (tested live), writing `restart: "no"` out loud changes nothing. `project config` prints it.
- **`RestartPolicy` moved to `yarc-config`** (the model needs the type; `yarc-runtime` now depends on `yarc-config`, the smaller crate, and re-exports it, so nothing else changed). Its parse/print/serde tests moved with it.
- **`x-yarc:` per service (compose), decision 4 of the plan.** Today only `user`: `x-yarc: { user: root }`. It exists because both bundle images declare a non-root `USER` and rootless yarc can only run as root (P2); compose has nowhere to say so. It wins over the generic `user:`; any other key inside `x-yarc` is an error, so a typo is not silently ignored. The bundles' compose files were not edited.

## Verified on the real bundles
`yarc project config` in `tmp/ssw-pms-fias-aero/docker-bundle` and `tmp/ssw-pms-fias-inspector/docker-bundle` loads the shipped `docker-compose.yml` as is: services in dependency order, `restart unless-stopped`, `stop grace period 20s`, secrets, the `container_name` aliases, volumes, and variable **names** only. `--profile observability` adds the Aspire dashboard to the aero bundle (the inspector file currently has no dashboard service).

## Tests
- **Config:** every policy spelling in both formats, bad values, `false`/unquoted `no`, interpolated policy, `x-yarc` (user, precedence, unknown key, empty user, wrong shape).
- **Offline CLI:** `project config` shows policy and `x-yarc` user for compose and `yarc.yaml`, rejects an interpolated bad policy.
- **Live (project):** a crashing `restart: always` service keeps restarting while a service without a policy runs once; a second `up -d` finds it up to date (no second supervisor); `project stop` is final; `up` revives it; `down` in the middle of a crash loop removes it and it does not come back; changing the policy recreates the service and the new container has no policy.

## What is left for C5
- **Running a real bundle.** `up` on the bundles needs `x-yarc: { user: root }` added to each service of the compose file (or an overlay; decision 4). Then: `yarc load images.tar` into a throwaway `--root`, `up -d`, check the services start, compare with the old hand-written file (now `_yarc.yaml`).
- `--env-file` (and the Windows client forwarding it with a translated path); `COMPOSE_PROFILES`/`--profile` already forward.
- The `-f` short option for `--file` (it clashes with `logs -f`).
- Then the hand-written `yarc.yaml` of the bundles is redundant.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test --workspace'
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-cli --test live_restart -- --ignored --test-threads=1'
```
