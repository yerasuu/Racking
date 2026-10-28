# rack unlink

## Purpose

Admin agents only: removes a stored link. `rack unlink NAME` removes the link between your agent and `NAME`; `rack unlink A B` the link between two other agents. Use it only when the owner asks; it goes to the audit log.

An agent that joined through an invite of the other agent exists for that link, so it loses its token too; its jobs are kept. A link made with `rack link --pair` only stops the two workers from reaching each other. Agents of the config file are never removed, and their implicit links with every agent cannot be removed.

## Options

- `NAME` the agent to unlink from yours.
- `A B` two agents to unlink from each other.

## Examples

```sh
rack unlink mac
rack unlink mac pc
```

## Prints

`unlinked linux and mac`, then one line per agent whose token was revoked.

## Errors

- `403 not_admin`: your agent is not an admin agent.
- `404`: no stored link between the two (an agent of the config file has none to remove).

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it.
