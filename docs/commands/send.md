# rack send

## Purpose

Sends a task: work the recipient accepts, does and finishes with a result. It is also how a comment or a result is sent (`--kind`). Use it for work with states. Do not use it for conversation (`rack say`), a heads-up (`rack notify`) or findings (`rack report`); those need no accept or result.

## Options

- `TO` the recipient agent name.
- `TITLE` a short title; quote it when it has spaces.
- `--body TEXT` the request; `--body -` reads stdin. Write it so an agent that has not seen your conversation can carry it out: goal, exact commands, what to attach, what done looks like.
- `--tag T` adds a tag (repeatable).
- `--origin REPO#ID` the entry on your board this task comes from, so the answer links back to it.
- `--attach FILE` attaches a file (repeatable; up to 16 files per job). A file over 32 MiB goes in resumable chunks with a progress line on stderr, and a directory as `DIR.tar`, as `rack attach` explains. For code send a `git format-patch` mailbox or a `git bundle`.
- `--kind task|comment|result` the kind of job. Default `task`.
- `--reply-to JOB` the job this comment or result belongs to.
- `--outcome done|failed` the outcome of a result.
- `--priority low|normal|high` only shown. Default `normal`.
- `--model M` suggests a model for the work (`opus`, `sonnet`...), a hint, never an order. The receiver's `rack pull` tags the task `agent-M`.
- `--needs NAME,...` capabilities the recipient must have (repeatable, comma separated); checked against what it published before anything is sent.
- `--force` sends even when a need is missing (with a warning). Only goes with `--needs`.
- `--profile NAME` (main agent only, tasks only) the profile of the worker's local policy the task asks to run in (`[a-z0-9-]`, 1 to 32, lowercased). The worker's `rack policy decide` decides; the job never orders it. A worker that sets it gets `403 not_allowed`, and so does every agent on a server without roles.

## Examples

```sh
rack send windows "Run psport-probe" --body - --origin decanter#873 --attach probe.bat <<< "Run it and send the log."
rack send windows "Profile the shader cache" --model opus --needs nvidia
rack send windows "Profile the shader cache" --needs nvidia --force
rack send win1 "Run the self-test" --profile app-test --body "Run it and send back the output."
```

## Prints

`j1 task sent to windows: Run psport-probe`, plus ` (1 file)` when files were attached. Warnings go to stderr: the recipient published no capabilities (the job is sent), or `--force` was used.

## Errors

- Exit 3, nothing sent: the recipient lacks a capability named in `--needs`; the error names what is missing. Pick another agent, or ask the owner, and only then use `--force`. The check is advice: each agent writes its own list.
- A usage error: `--force` without `--needs`, or an `--origin` not like `repo#id`.
- `404`: unknown agent. `413`: body over 64 KiB.
- `403 not_allowed` with `--profile`: you are not the main agent, or the server has no roles. `400` with `--profile`: not a task, or not a profile name. A server from before the field answers `400` naming `profile`; the hint says to update it.
- A file that fails to upload leaves the job sent: rack says `retry with: rack attach JOB FILE`.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error
- 3 a need is missing (`--needs` without `--force`); nothing was sent

## MCP

`racking_send` (`to`, `title`, `body`, optional `tags`, `origin`, `reply_to`, `kind`, `outcome`, `priority`, `model`, `needs`, `force`, `profile`), then `racking_attach` for files.

## Security

The recipient reads title and body as data from you, not as orders, and decides what to do. Send no secrets, private paths or tokens. An attachment you send is never run for you; do not ask the receiver to run one without the owner's consent.
