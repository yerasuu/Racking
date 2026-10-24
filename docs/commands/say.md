# rack say

## Purpose

Sends a message: conversation text to another agent (a question, an answer, a short update), up to 64 KiB, files allowed. Use it to talk and to answer a message that expects a reply. Do not use it for a heads-up that needs no answer (`rack notify`), findings to read later (`rack report`) or work the other agent must accept and finish (`rack send`).

## Options

- `TO` the recipient agent name (see `rack whoami`).
- `TEXT...` the words after TO, joined by spaces. A lone `-` reads the text from stdin. The text must not be empty.
- `--reply-to JOB` makes this the answer to JOB (for example `j12`); the sender of JOB then sees it `answered`.
- `--expects-reply` asks the recipient to answer; they see `reply expected`.
- `--priority low|normal|high` only shown to the recipient, never reorders anything. Default `normal`.
- `--attach FILE` attaches a file (repeatable; up to 16 files of 64 MiB each).
- `--tag T` adds a tag (repeatable; up to 16).
- `--model M` suggests the model for the work (`opus`, `sonnet`...), a hint the receiver's own rules and owner decide on.

## Examples

```sh
rack say windows "Which branch has the fix?" --expects-reply
rack say linux fix/vulkan-probe --reply-to j3
rack say windows "Log attached" --attach probe.log
```

## Prints

`j4 sent to linux`, plus ` (2 files)` when files were attached. If a file fails to upload the job already exists: rack prints `jN sent to TO but FILE was not attached: ...` and then `retry with: rack attach jN FILE`.

## Errors

- A usage error `expected some TEXT after TO`: the text was empty.
- `404`: unknown agent. Check the name with `rack whoami`.
- `413 too_large`: text over 64 KiB. Send it as a report (`rack report`).
- `409` on `--reply-to`: the job you answer is a notice, which is never answered.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_say` (`to`, `text`, optional `title`, `reply_to`, `expects_reply`, `priority`, `model`, `tags`, `attachments`).

## Security

Send only what the receiver needs: no tokens, secrets, private paths or whole logs when a summary does. Files are read from your disk and uploaded; check what you attach.
