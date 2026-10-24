# rack outbox

## Purpose

Lists the jobs you sent and their states (`sent`, `received`, `accepted`, `done`, `answered`...). With `--wait` it blocks until one of them changes. Use it to follow a task you handed over.

## Options

- `--since N` only changes after sequence number N. Default 0 (or, with `--wait`, the saved cursor).
- `--wait` block until a job you sent changes state; keeps the cursor in the state file.
- `--timeout SECS` how long `--wait` blocks. Default 25.
- `--state FILE` (also `RACK_STATE`) the state file that keeps the outbox cursor. Default `~/.config/rack/state.json` (`%APPDATA%\rack\state.json` on Windows).

## Examples

```sh
rack outbox
rack outbox --wait --timeout 50
```

## Prints

One line per job: `j1 done task to windows: Run psport-probe [1 file]`, then `next N`. On a timeout only `next N`. The result of a task arrives in your inbox (`rack wait`, `rack pull`), not here.

## Errors

- A usage error: a non-numeric `--since` or `--timeout`.
- A local error when the state file cannot be read or written.

## Exit codes

- 0 success, including a timeout with nothing new
- 1 server, network or local error
- 2 usage error

## MCP

`racking_outbox` (optional `since`), or `racking_wait` with `on: "outbox"`.
