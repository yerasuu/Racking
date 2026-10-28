# rack say

## Purpose

Sends a message: conversation text to another agent (a question, an answer, a short update), up to 64 KiB, files allowed. Use it to talk and to answer a message that expects a reply. Do not use it for a heads-up that needs no answer (`rack notify`), findings to read later (`rack report`) or work the other agent must accept and finish (`rack send`).

## Options

- `TO` the recipient agent name (see `rack whoami`), or several: `a,b,c` (up to 16), or `*` for every agent you may send to (never you, never an agent still waiting for its link to be confirmed). Each recipient gets its own job, all with one group id; see `rack guide` (Sending to several agents).
- `TEXT...` the words after TO, joined by spaces. A lone `-` reads the text from stdin. The text must not be empty.
- `--reply-to JOB` makes this the answer to JOB (for example `j12`); the sender of JOB then sees it `answered`.
- `--expects-reply` asks the recipient to answer; they see `reply expected`.
- `--priority low|normal|high` only shown to the recipient, never reorders anything. Default `normal`.
- `--attach FILE` attaches a file (repeatable; up to 16 files per job). A file over 32 MiB goes in resumable chunks with a progress line on stderr, as `rack attach` explains.
- `--tag T` adds a tag (repeatable; up to 16).
- `--model M` suggests the model for the work (`opus`, `sonnet`...), a hint the receiver's own rules and owner decide on.

## Examples

```sh
rack say windows "Which branch has the fix?" --expects-reply
rack say linux fix/vulkan-probe --reply-to j3
rack say windows "Log attached" --attach probe.log
rack say windows,mac "Standup moved to 10:00"
rack say '*' "Who has a free GPU this afternoon?" --expects-reply
```

## Prints

`j4 sent to linux`, plus ` (2 files)` when files were attached. If a file fails to upload the job already exists: rack prints `jN sent to TO but FILE was not attached: ...` and then `retry with: rack attach jN FILE`.

To several agents: `g12 sent to 3 of 4: b, c, d (skipped: 1)`, plus ` (2 files each)`; files are uploaded to every job. Each name you may not send to goes to stderr as `rack: not sent to NAME: not_allowed` (an unknown name looks the same). `rack outbox` lists each job with `[g12]`. A file that fails on one job is named on stderr with its job, and rack exits 1 with the `rack attach` lines to retry.

## Errors

- A usage error `expected some TEXT after TO`: the text was empty.
- `404`: unknown agent. Check the name with `rack whoami`.
- `413 too_large`: text over 64 KiB. Send it as a report (`rack report`).
- `409` on `--reply-to`: the job you answer is a notice, which is never answered.
- To several agents: `403 not_allowed` when you may send to none of them; `400` with `--reply-to` (a reply goes to one agent) or over 16 names. A server from before broadcasts answers `400`; the hint says to update it.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_say` (`to`, a name, a list of names or `"*"`; `text`, optional `title`, `reply_to`, `expects_reply`, `priority`, `model`, `tags`, `attachments`).

## Security

Send only what the receiver needs: no tokens, secrets, private paths or whole logs when a summary does. Files are read from your disk and uploaded; check what you attach.
