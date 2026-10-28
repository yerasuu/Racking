# rack admin

## Purpose

Admin agents only (`admin = true` in the server config): manage the server's agents and tokens without editing the config file or restarting it, read the audit log, and pull or release the emergency brake. Use it only when the owner asks. Every change is written to the server's audit log.

Subcommands:

- `agents` lists every agent: name, role, `admin`, where it is defined (`file` or `api`), when its token was issued (API agents) and when it was last used. Never a token.
- `add NAME ROLE` creates an agent (`ROLE` is `worker` or `main`) that works at once. Its token is printed once, alone on stdout; the server keeps only its SHA-256.
- `rotate NAME` gives an agent made with `add` a new token, printed once; the old one stops working at once.
- `remove NAME` removes an agent made with `add`: its token stops working at once; its jobs are kept.
- `audit` prints the audit log, newest first: id, time, actor, action, target, result.
- `pause` pulls the emergency brake: workers cannot send, and every agent's inbox shows that the server is paused, so they stop acting on jobs. Jobs are still delivered; the main and admin agents can still send.
- `resume` releases it.

Agents of the config file (the admin agent among them) are changed by editing the file, not here.

## Options

- `--limit N` (`audit` only) entries per page, 1 to 200; default 50.
- `--before ID` (`audit` only) entries older than `ID`: the id `rack admin audit` printed in its `more:` line.

## Examples

```sh
rack admin agents
rack admin add mac worker > mac.token
rack admin rotate mac
rack admin remove mac
rack admin audit --limit 20
rack admin pause
rack admin resume
```

## Prints

- `agents`: one line per agent, e.g. `mac worker (api), token from 2026-10-28T10:00:00Z, last used 2026-10-28T10:05:00Z`.
- `add`, `rotate`: the token alone on stdout; on stderr, that it is shown once. Hand it to the owner of that agent at once, and never put it in a job, board entry, log or commit.
- `audit`: one line per entry, then `more: rack admin audit --before ID`, or `end of the log`.

## Errors

- `403 not_admin`: your agent is not an admin agent.
- `403 not_allowed`: the agent is defined in the config file; the owner edits the file.
- `409 conflict`: the name is taken, or a second main agent was asked for (also on a server whose config sets no roles). If `add` was retried after a lost answer, the agent exists: `rotate` it to get a token.
- `404`: no agent of that name.
- A usage error: an unknown subcommand, a name outside `[a-z0-9-]`, or `--limit`/`--before` outside `audit`.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it. Administration stays with the owner and the admin agent's shell.
