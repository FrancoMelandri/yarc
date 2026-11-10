# F1 — "a container named ... already exists" after `down`/`rm`

Status: **fixed**. Workspace and live suites pass with no warnings (`yarc-runtime` 105, live: `live_run` 30, `live_project` 10, `live_volumes` 9, `live_agent` 2, `live_remote` 2, `live_restart` 13). Nothing committed.

## The report
`yarc up -d` in `tmp/ssw-pms-fias-inspector/docker-bundle`: the first service was created and started, then
`error: service ssw-platform-pms: a container named 'docker-bundle-ssw-platform-pms-1' already exists`.

## Cause
The store (`~/.local/share/yarc/containers/`) held two directories from the day before (`docker-bundle-ssw-platform-pms-1`, `...-pms-aero-1`) containing **only an empty `lock` file**: no `config.json`, no rootfs. `up` saw no container (no config) and tried to create one, but the name was taken by the husk.

How they appear: `is_supervised` (used by `ps`, `logs -f`, the `up`/`stop`/`wait` loops) was built on `try_acquire`, which **creates** the lock file. A poller running while `down`/`rm` deleted the directory could re-create `lock` between the unlink of the contents and the final `rmdir`: the removal then failed with `Directory not empty` and left the husk. Reproduced exactly that way (4 threads polling while a container is removed: fails on round 0 with the old code, 300 rounds pass now).

## Fix
- `is_supervised` only looks (opens the lock file read-only; a missing file means nobody supervises it) and never creates anything.
- `allocate` reclaims a directory that has no `config.json` when it is certainly dead: nobody holds its lock and nothing in it (or it) was modified in the last 10 s. This also covers a creation killed halfway through unpacking. A finished container, a supervised one, or a directory with recent activity is never touched.
- The existing husks need no manual cleanup: the next `up` removes them.

## Notes
- If you retry within 10 s of killing a `up` that was unpacking an image, the name is still reported as taken; try again a few seconds later.
- The Windows client runs whatever `yarc` the WSL side resolves (`YARC_WSL_BIN`, else `yarc` on the PATH): rebuild/reinstall that Linux binary for the fix to apply.
