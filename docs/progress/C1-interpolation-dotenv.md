# C1 — Interpolation and `.env`

Status: **done**. `yarc-config`: 37 unit tests pass (26 new), no warnings. Nothing is wired into the CLI yet: C1 is two pure modules.

Decisions taken: the plan's "defaults" (read compose directly, error on unsupported keys, implement `restart` in C4, `x-yarc` block for `user`).

## What was done
### `interpolate` (`yarc-config/src/interpolate.rs`)
`interpolate(input, &lookup) -> Result<String, InterpolationError>`; values come from a closure, so the module has no environment access of its own.
- `$VAR`, `${VAR}`; `$$` is a literal `$`.
- `${VAR:-d}` / `${VAR-d}` default; `${VAR:?msg}` / `${VAR?msg}` required; `${VAR:+r}` / `${VAR+r}` replacement. `:` also treats an empty value as unset.
- Operands nest (`${A:-${B:-x}}`) and are only evaluated when used.
- An unset variable with no operator becomes `""`. A lone `$`, `${}`, `${1A}`, an unknown operator or a missing `}` are errors.

### `parse_dotenv` (`yarc-config/src/dotenv.rs`)
`parse_dotenv(input, &outer) -> Result<BTreeMap<String,String>, DotenvError>`
- `KEY=value`, `export KEY=value`, comments, blank lines, BOM, CRLF; a bare `KEY` takes the outer value if set.
- Unquoted values: inline ` #` comments stripped. Single quotes: literal. Double quotes: `\n \r \t \\ \" \$`. Quoted values may span lines.
- Unquoted and double-quoted values are interpolated, seeing earlier entries first, then `outer` (the process environment, at the call site). Later duplicates win.

### Secrets safety
Errors never contain values: `.env` holds real secrets, and a test found that the first draft of `InterpolationError` echoed the whole input. Errors now carry only a line number and a key name.

## Not in C1
Precedence between process env, `.env` and `--env-file`, and applying interpolation to parsed YAML string values: that is C2 (the reader). Both modules are exported from `yarc-config`.

## How to verify
```
wsl -d Ubuntu-24.04 -- bash -lc 'source ~/.cargo/env; cd /mnt/c/Projects/MINE/yarc && CARGO_TARGET_DIR=~/yarc-target cargo test -p yarc-config'
```
