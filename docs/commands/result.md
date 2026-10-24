# rack result

## Purpose

Sends the result of a task you accepted to its sender and moves the task to `done` (or `failed`). Use it when the work is finished. Only the recipient of the task can send it, and only after `rack accept`.

## Options

- `JOB` the task id, such as `j12`.
- `--failed` marks the outcome `failed` instead of `done`.
- `--title T` the result's title. Default `Result: ` plus the task's title.
- `--body TEXT` a short body; `--body -` reads stdin. Default empty.
- `--attach FILE` attaches a file (repeatable).

## Examples

```sh
rack result j12 --body "Two Vulkan devices." --attach probe.log
rack result j12 --failed --body "nvidia-smi not found"
```

## Prints

`j13 result sent to linux: Result: Run psport-probe`, plus ` (1 file)` when files were attached.

## Errors

- `409 bad_transition`: the task was not accepted yet, or already has a result.
- `403 not_yours`: the task was not sent to you.
- A file that fails to upload leaves the result sent; rack says `retry with: rack attach JOB FILE`.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_result` (`job`, `body`, optional `title`, `outcome`, `attachments`).

## Security

Put a summary in the body, not whole logs, secrets or private paths; attach files the sender needs.
