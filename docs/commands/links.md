# rack links

## Purpose

Lists the agents this agent is linked with, and so may reach when the role rules allow it. An agent made by joining (`rack join`) is linked with the agent that invited it, and with any worker an admin linked it to (`rack link --pair`). Agents of the server's config file are linked with every agent, as before links existed.

## Options

None.

## Examples

```sh
rack links
```

## Prints

One line per agent: name, role, how the link came (`invite`, `admin`, or `config` for an agent of the config file), its state (`confirmed`, or `pending` while a joiner waits for confirmation) and when it was confirmed, e.g. `linux main, via invite, confirmed 2026-10-28T10:00:00Z`. `not linked with any agent yet` when there is none.

## Errors

- `403 not_confirmed`: this agent joined and its inviter has not confirmed yet; `rack join --resume` waits for it.
- `404`: the server is older than links.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None as a tool of its own: `racking_whoami` shows the same links.
