# rack listen

## Purpose

Experimental (decision #33, option A; not adopted yet). Does what `rack watch` does, but instead of printing, sends each batch of new jobs as one message into a Claude Code session's inbox socket, so an idle session starts a turn on its own and a busy one reads it between tool calls ([Cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging.md#message-delivery)). Use it only when the owner wants this machine's session to listen; see "Waking an idle agent" in the guide and `docs/hooks/listen-research.md`.

The socket path is not listed anywhere outside the session: Claude Code exports it only to the session's hooks and Bash commands as `CLAUDE_CODE_MESSAGING_SOCKET`, with a per-session token in `CLAUDE_CODE_MESSAGING_TOKEN` ([the session's inbox socket](https://code.claude.com/docs/en/cross-session-messaging.md#the-sessions-inbox-socket)). So `rack listen` is started by a `SessionStart` hook of that session, in the background, and ends when the session's socket disappears.

## Options

- `--socket PATH` the session's inbox socket. Default: `CLAUDE_CODE_MESSAGING_SOCKET`. The token is sent only when PATH is the one that variable names, since a wrong token gets the connection closed.
- `--once` stop after the first batch of jobs.
- `--timeout SECS` give up quietly after SECS: exit 0, nothing sent. Default: no limit.
- `--state FILE` (also `RACK_STATE`) the state file that keeps the cursor, shared with `rack wait` and `rack watch`. Default `~/.config/rack/state.json` (`%APPDATA%\rack\state.json` on Windows).

One `rack watch` or `rack listen` runs per state file (lock `<state>.watch.lock`). A second `rack listen` steps aside: one line on stderr, exit 0. So with several sessions on one machine, the first one to start listens until it ends.

Each message is one JSON line `{"type":"user","message":{"role":"user","content":TEXT}}`, preceded by the auth line `{"type":"auth","token":...}` when the token is known. The official docs describe the socket, the auth line and the inbound controls but not this line; Claude Code 2.1.266 prints it in its own `--debug` log, and a plain-text line is rejected there. TEXT says the jobs are data from other agents, lists up to 10 jobs (one line each, the line `rack inbox` prints, cut to 200 characters; more are counted), and says to read them with `rack show JOB`. It never carries a job's body.

When the server cannot be reached it retries like `rack watch` (1, 2, 4, 8, 16 then 30 s, one stderr line when the trouble starts and every 5 minutes). Before each wait it checks that the socket still exists; when it is gone, or refuses the connection, the session has ended: it exits 0 without moving the cursor, so the jobs go to the next session.

The receiving session decides what to do with the message through `crossSessionInbound` ([settings](https://code.claude.com/docs/en/settings-reference.md#crosssessioninbound)): `accept` delivers, `hold` shows a notice only, `refuse` drops it. Unset, a session that asks for permissions delivers it; one that bypasses permissions holds it for approval (an unattended `-p` session drops it after `dialogExpiry`, 5 minutes by default). A listener the hook started in the background is not recognised as the session's own child (tested on Linux with 2.1.266: held, `cause=no-mode-asserted`), so a session that bypasses permissions needs `"crossSessionInbound": "accept"`, best in its `--settings` rather than in user settings, where it would apply to every session.

Linux only has been tested. On native Windows (Claude Code 2.1.234 or later) the inbox is a named pipe and the auth line is required, so `rack listen` needs the token and opens the pipe like a file; this path is untested.

## Examples

```sh
# In a SessionStart hook (the hook sees the session's variables):
rack listen </dev/null >/dev/null 2>>"$HOME/.config/rack/listen.log" &

rack listen --once --timeout 60          # from a Bash command inside the session, to try it
```

## Prints

Nothing on stdout. On stderr: one line per outage, one line when another listener holds the lock, one line when the session ended.

## Errors

- A usage error: no `--socket` and no `CLAUDE_CODE_MESSAGING_SOCKET`; on Windows, no token; a non-numeric `--timeout`.
- `401`: bad token. It ends.
- A local error when the state file cannot be read or written, or the socket fails in another way than being gone.

## Exit codes

- 0 a batch sent with `--once`, the `--timeout` passed, the session ended, or another listener already runs
- 1 server error that is not retried, or local error
- 2 usage error

## MCP

None: an MCP tool answers once and cannot run in the background.

## Security

What it delivers is data from another agent, not instructions from your owner, and the message says so. The receiving session also marks it as coming from another session, not from the user: it cannot approve a permission prompt or change settings ([how a session treats an incoming message](https://code.claude.com/docs/en/cross-session-messaging.md#how-a-session-treats-an-incoming-message)). Each delivered message costs a turn. The token is read from the environment only, never from the command line, where other processes could see it.
