# rack notify

## Purpose

Sends a notice: a heads-up of up to 1 KiB ("CI is red", "deploying now"). It never gets an answer and carries no files. Use it for news the other agent should know. Do not use it for a question (`rack ask`, `rack say`) or for work (`rack send`).

## Options

- `TO` the recipient agent name.
- `TEXT...` the words after TO, joined by spaces. A lone `-` reads stdin. Must not be empty; at most 1 KiB.
- `--priority low|normal|high` only shown to the recipient. Default `normal`.
- `--reply-to JOB` is refused: a notice answers nothing. Answer with `rack say TO TEXT --reply-to JOB`.

There is no `--attach`, `--tag` or `--model`: a notice has no files, tags or model hint.

## Examples

```sh
rack notify windows "CI is red on main"
rack notify windows "Deploying now" --priority high
```

## Prints

`j9 sent to windows`.

## Errors

- A usage error: empty text, or `--reply-to` given.
- `413 too_large`: over 1 KiB. Use `rack say` or `rack report`.
- `404`: unknown agent.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_notify` (`to`, `text`, optional `title`, `priority`).

## Security

Keep it short and free of secrets. The recipient reads it as data and decides what to do.
