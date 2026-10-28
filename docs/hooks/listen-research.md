# Waking a session through its inbox socket (`rack listen`, experimental)

Research for decision #33, option A (task #35, part of #43): can a long-lived process on the machine wake an idle Claude Code session when a Racking job arrives, without anyone typing? Checked on 2026-10-28 with Claude Code 2.1.266 on Linux.

## What the official docs say

From [Cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging.md):

- Requires Claude Code 2.1.224 or later on macOS, Linux and WSL 2; 2.1.234 or later on native Windows. Same-machine messaging on Bedrock, Vertex, Foundry or with flag fetching off needs 2.1.248 ([Availability](https://code.claude.com/docs/en/cross-session-messaging.md#availability)).
- Each session binds an inbox: a Unix domain socket on macOS and Linux (WSL 2 included), a named pipe on native Windows. `claude -p` sessions bind one too; `--bare` sessions do not ([Non-interactive sessions](https://code.claude.com/docs/en/cross-session-messaging.md#non-interactive-sessions)).
- Where the path is: `/status` (`Peer address` row, prefixed `uds:`) and the variable `CLAUDE_CODE_MESSAGING_SOCKET`, exported to hooks and Bash commands, before any hook runs, `SessionStart` included. A per-session token is exported as `CLAUDE_CODE_MESSAGING_TOKEN` (2.1.228 or later) ([The session's inbox socket](https://code.claude.com/docs/en/cross-session-messaging.md#the-sessions-inbox-socket), [Environment variables](https://code.claude.com/docs/en/env-vars.md)). No registry file or directory is documented: a process outside the session learns the path only from the session (a hook, a Bash command, or `/status`). The socket is restricted to the operating-system user; when the directory is not acceptable it falls back to `/tmp/cc-socks-<uid>`.
- Authentication: a first line `{"type":"auth","token":"<token>"}` is optional on macOS and Linux and required on native Windows, where a connection without a valid one is closed and nothing is delivered. A connection that sends no complete line within 30 s is closed.
- Inbound controls: every message on the socket goes through `crossSessionInbound` ([settings reference](https://code.claude.com/docs/en/settings-reference.md#crosssessioninbound)): `accept` delivers, `hold` shows a notice without delivering (released when an `accept` later applies), `refuse` drops. Precedence: managed settings, then `--settings`, then user settings; a project or local value applies only when stricter. Unset: a session that prompts for permissions delivers, unless the sender says it bypasses; a session that bypasses permissions holds, unless the sender also bypasses. A message the session verifies as coming from its own child process is delivered when no value applies; on Linux by process evidence, elsewhere by the token.
- Held messages: an interactive session asks with a dialog; a `-p` session keeps them until `dialogExpiry` (default 5 minutes, [settings reference](https://code.claude.com/docs/en/settings-reference.md#dialogexpiry)), then drops them. At most 100 held messages.
- Delivery: an idle session starts a new turn with the message; a busy one reads it between tool calls. The receiving Claude is told it came from another session: it cannot approve a prompt or change configuration, and commands in it do not run ([How a session treats an incoming message](https://code.claude.com/docs/en/cross-session-messaging.md#how-a-session-treats-an-incoming-message)). Each delivered message costs like a typed prompt.
- Limits ([Limitations](https://code.claude.com/docs/en/cross-session-messaging.md#limitations)): plain text only; about 1 MiB per message; the receiver rate-limits each sender, drops identical repeats within a short window, and queues at most 50 messages; the sending `SendMessage` refuses bursts past 30 recent messages ([errors](https://code.claude.com/docs/en/errors.md#too-many-messages-to-this-session-just-now)). Not across a container boundary, nor between WSL 2 and native Windows.
- A closed session: the docs do not say. Observed: the socket file is removed when the session exits.

The docs do not give the format of the line that carries the message.

## What was tested (Linux, 2.1.266)

A throwaway `claude -p --input-format stream-json --model haiku --tools ""` session in a temp directory, with `--setting-sources project` and a `--settings` file whose `SessionStart` hook records the socket path; a throwaway `racking-server` on 127.0.0.1 with a temp data dir.

- A plain-text line is rejected: the session's `--debug` log says `Failed to parse JSON line`. The same log prints the format to use (`[uds-messaging] Inject messages`): `{"type":"user","message":{"role":"user","content":"hello"}}`, after the optional auth line. This comes from the tool's own log, not from the docs, so it may change without notice.
- With that line, a session that had never had a turn started one and answered (`Routed user message to queue`). Asked where the text came from, it answered "another session", not the user.
- `crossSessionInbound: "refuse"` dropped it (`refused inbound peer message`); with no value and the default permission mode it was delivered.
- `rack listen` started in the background by the `SessionStart` hook kept running after the hook ended, saw a job sent by another agent, and woke the idle session with one message. When the session ended, the listener exited at its next round (within one 55 s wait) without moving the cursor.
- With `--permission-mode bypassPermissions` and no `crossSessionInbound`, the message was held (`cause=no-mode-asserted`, `from=unknown`), even with the token on the auth line: the backgrounded listener is not recognised as the session's own child. Such a session needs `"crossSessionInbound": "accept"`.

Not tested: native Windows (named pipe, required auth line), macOS, an interactive terminal session, a busy session.

## Setup (if adopted)

The owner adds to the session's settings (prefer `--settings` for the agent's session over user settings):

```json
{
  "crossSessionInbound": "accept",
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command",
        "command": "rack listen </dev/null >/dev/null 2>>\"$HOME/.config/rack/listen.log\" &" } ] }
    ]
  }
}
```

`accept` is needed only when the session bypasses permissions. One listener runs per state file, so with several sessions the first one started gets the jobs until it ends.
