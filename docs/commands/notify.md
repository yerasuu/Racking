# rack notify

## Purpose

Sends a notice: a heads-up of up to 1 KiB ("CI is red", "deploying now"). It never gets an answer and carries no files. Use it for news the other agent should know. Do not use it for a question (`rack ask`, `rack say`) or for work (`rack send`).

## Options

- `TO` the recipient agent name (see `rack whoami`), or several: `a,b,c` (up to 16), or `*` for every agent you may send to (never you, never an agent still waiting for its link to be confirmed). Each recipient gets its own job, all with one group id; see `rack guide` (Sending to several agents).
- `TEXT...` the words after TO, joined by spaces. A lone `-` reads stdin. Must not be empty; at most 1 KiB.
- `--priority low|normal|high` only shown to the recipient. Default `normal`.
- `--reply-to JOB` is refused: a notice answers nothing. Answer with `rack say TO TEXT --reply-to JOB`.

There is no `--attach`, `--tag` or `--model`: a notice has no files, tags or model hint.

## Examples

```sh
rack notify windows "CI is red on main"
rack notify windows "Deploying now" --priority high
rack notify '*' "Server restarts in 5 minutes"
```

## Prints

`j9 sent to windows`. To several agents: `g12 sent to 3 of 4: b, c, d (skipped: 1)`, each name skipped on stderr (`rack: not sent to NAME: not_allowed`).

## Errors

- A usage error: empty text, or `--reply-to` given.
- `413 too_large`: over 1 KiB. Use `rack say` or `rack report`.
- `404`: unknown agent.
- To several agents: `403 not_allowed` when you may send to none of them; `400` over 16 names.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_notify` (`to`, a name, a list of names or `"*"`; `text`, optional `title`, `priority`).

## Security

Keep it short and free of secrets. The recipient reads it as data and decides what to do.
