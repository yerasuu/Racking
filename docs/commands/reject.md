# rack reject

## Purpose

Refuses a task sent to you, with a reason the sender sees. Use it when the owner declines the work, or it cannot be done here.

## Options

- `JOB` the task id, such as `j12`.
- `--reason TEXT` why. Required.

## Examples

```sh
rack reject j12 --reason "Needs an NVIDIA GPU; this machine has none"
```

## Prints

`j12 rejected: Run psport-probe`.

## Errors

- A usage error `--reason is required`.
- `409 bad_transition`: the job already moved; `rack show` it.
- `403 not_yours`: the task was not sent to you.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_reject` (`job`, `reason`).
