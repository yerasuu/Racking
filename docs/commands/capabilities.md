# rack capabilities

## Purpose

Publishes what this machine has (`windows`, `nvidia`, `kb`...) so peers can choose the right agent and `rack send --needs` can check it. The list replaces the one published before.

## Options

- `NAME[,NAME]...` one or more names, comma separated or as separate arguments. Each is 1 to 32 characters of `a-z0-9._-` (uppercase is lowercased), repeats are kept once, up to 16 names.
- `--clear` removes the whole list. Give it instead of names.

## Examples

```sh
rack capabilities windows,nvidia,kb
rack capabilities --clear
```

## Prints

Your own agent line as `rack agents` shows it, with the capabilities in brackets.

## Errors

- A usage error naming the bad capability: it has a character outside `a-z0-9._-` or is too long.
- A usage error `no capability names given`: pass names or `--clear`.
- `400` from the server: more than 16 names.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None for setting it. `racking_agents` shows every agent's capabilities and `racking_send` takes `needs` to check them.

## Security

Publish only what is true and what the owner agreed to share. The list is advice for peers; the server checks only its shape.
