# rack join

## Purpose

The joiner's side of a pairing: run on a new machine with the code the inviter's `rack link` printed. It needs no token or config: it is how a machine gets them.

`rack join CODE --server URL` redeems the code, saves the server URL and the new agent's token in `rack.conf` (mode 600, written atomically) in the default config directory, `~/.config/rack` (`%APPDATA%\rack` on Windows), and prints a fingerprint of four words. Read them to the person at the inviter's terminal, or check them against that screen: they must be the same words. The command then waits for the inviter to confirm and prints `linked with NAME`, or `denied`.

Until the inviter confirms, the token does nothing: every request but the status check answers `403 not_confirmed`. If the link is denied, or nobody confirms within 10 minutes, the token is dead and `rack join` removes the `rack.conf` it wrote (restoring the old one with `--force`).

The token is never printed. On success the command ends with the next steps: registering the MCP tools with `claude mcp add` (without the token: `racking-mcp` reads it from `rack.conf`), reading `rack guide`, and for a worker where the `crossSessionInbound` setting goes if jobs should wake a session.

The server URL must start with `http://`. Racking has no TLS, so the code and the token cross the network in clear: a host outside loopback and the private and link-local ranges is accepted with a warning.

## Options

- `CODE` the code from `rack link`; case, dashes and spaces do not matter.
- `--server URL` the Racking server, `http://HOST:PORT`; required.
- `--name N` the agent name to ask for, when the inviter did not set one (`[a-z0-9-]`, up to 32).
- `--config-dir DIR` write `rack.conf` in `DIR` instead of the default directory; other commands then need `RACK_CONFIG=DIR/rack.conf`.
- `--force` replace an existing `rack.conf`, keeping the old one as `rack.conf.bak`. Without it, an existing file is never overwritten and the code is not used.
- `--resume` wait again for the confirmation, with the credentials already saved (after Ctrl-C).

## Examples

```sh
rack join ABCD-EFGH-JKMN-PQRS --server http://192.168.1.20:7878
rack join abcd efgh jkmn pqrs --server http://192.168.1.20:7878 --name mac
rack join ABCD-EFGH-JKMN-PQRS --server http://192.168.1.20:7878 --config-dir ./rack-test
rack join --resume
```

## Prints

- `joined as mac (worker); credentials saved in ...`, then `fingerprint: ...` and that it waits.
- `linked with linux`, then the next steps; or `denied`.

## Errors

- `403 bad_code`: the code is unknown, used, expired or voided (one answer for all four). Ask the inviter for a new one.
- `429`: too many wrong codes from this address; wait `Retry-After` seconds.
- `409 conflict`: `--name` differs from the name the inviter chose, or is taken.
- `403 peer_not_allowed`: this machine's address is outside the server's `allow_from`.
- A usage error: no code, no `--server`, a URL that is not `http://HOST:PORT`, or an existing `rack.conf` without `--force`.

## Exit codes

- 0 linked
- 1 denied, not confirmed in time, a wrong code, or a server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it. Joining needs a human at both terminals.
