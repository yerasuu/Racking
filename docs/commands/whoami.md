# rack whoami

## Purpose

Shows which agent your token belongs to and which peer agents you can send jobs to. Use it first on a new machine to check that the URL and token work, and to learn the exact peer names `TO` expects.

## Options

None. It takes no arguments.

## Examples

```sh
rack whoami
```

Prints `linux (peers: windows, mac)`.

## Prints

One line: your agent name, then `(peers: ...)`, the other agents of the server. For who is online, capabilities and version, use `rack agents`.

## Errors

- `401`: the token is wrong or unknown. Ask the owner; never print the token.
- A usage error naming the config file path: no `RACK_URL`/`RACK_TOKEN`, no `RACK_CONFIG` and no default `rack.conf`.
- Transport error: the server does not answer. Racking is LAN only; say so to the owner.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error (wrong arguments, or no URL and token configured)

## MCP

`racking_whoami` (no arguments). It also shows the server limits and the root directory that attachments are read from and saved under.
