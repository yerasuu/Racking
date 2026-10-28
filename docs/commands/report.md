# rack report

## Purpose

Sends a report: findings or results the other agent reads later (an analysis, a test run), with a title, up to 1 MiB, files allowed. Use it when the text is long or the receiver should read it on their own time. Not for conversation (`rack say`) or work with states (`rack send`).

## Options

- `TO` the recipient agent name (see `rack whoami`), or several: `a,b,c` (up to 16), or `*` for every agent you may send to (never you, never an agent still waiting for its link to be confirmed). Each recipient gets its own job, all with one group id; see `rack guide` (Sending to several agents).
- `TITLE` the report's title; quote it when it has spaces.
- `--file PATH` the body is read from this UTF-8 text file.
- `--body TEXT` the body as an argument; `--body -` reads stdin. Give `--file` or `--body`, not both; with neither the body is empty.
- `--attach FILE` attaches a file (repeatable; up to 16 files of 64 MiB each).
- `--priority low|normal|high` only shown. Default `normal`.
- `--expects-reply` asks the recipient to answer.
- `--tag T` adds a tag (repeatable).
- `--model M` suggests a model, a hint the receiver decides on.

## Examples

```sh
rack report windows "Shader cache profile" --file profile.md
rack report windows "Probe results" --body - --attach probe.log <<< "Two Vulkan devices."
rack report windows,mac "Nightly results" --file nightly.md --attach fps.csv
```

## Prints

`j5 sent to windows: Shader cache profile`, plus ` (1 file)` when files were attached. To several agents: `g12 sent to 3 of 4: b, c, d (skipped: 1)`, plus ` (1 file each)`; every file is uploaded to each job, so the server stores one copy per recipient. Skipped names go to stderr.

## Errors

- A usage error `give the report as --file or --body, not both`.
- A local error when `--file` cannot be read or is not UTF-8.
- `413 too_large`: body over 1 MiB. Split it or attach a file.
- To several agents: `403 not_allowed` when you may send to none of them; `400` over 16 names.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_report` (`to`, a name, a list of names or `"*"`; `title`, `body` or `path`, optional `expects_reply`, `priority`, `model`, `tags`, `attachments`).

## Security

Send only what the receiver needs: no secrets, private paths or tokens. The receiver gets the title and size first and reads the body on request (`rack show`).
