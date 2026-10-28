# rack link

## Purpose

Admin agents only (`admin = true` in the server config): the inviter's side of a pairing. The first connection between two agents is made from the terminals of both: `rack link` here, `rack join` on the other machine. Use it only when the owner asks.

`rack link [NAME] --role worker` asks the server for an invite and prints a one-time code (`XXXX-XXXX-XXXX-XXXX`), then waits, showing what happens. Give the code to the owner of the other machine by a channel you trust (said aloud, typed from this screen). When they run `rack join CODE --server URL`, this command prints the joiner's name, OS, `rack` version, its address as the server sees it, and a fingerprint of four words. The other screen shows four words too. The command then asks on this terminal: `Does the other screen show the same words? [y/N]`.

- `y`: the link is confirmed. The new agent starts working and is linked with you.
- Anything else, or no answer in 10 minutes: the joiner is denied and its agent and token are deleted at once.

The question is read from the terminal (`/dev/tty`, or the console on Windows), never from stdin, so no script or agent can answer it in the owner's place. Without a terminal the joiner is denied. Never confirm when the words differ: someone else may have used the code.

The code works once, for 10 minutes by default (`--ttl`, at most an hour), and only its salted hash is stored. Five wrong codes sent to the server void every open invite. Ctrl-C stops waiting without harm: the invite stays, and `rack link --resume ID` waits again, until it expires or, once redeemed, until the server denies the joiner 10 minutes after it joined.

Without `NAME` the server proposes `agent-` and 4 random characters; the joiner may then pick its own name with `rack join --name`. With `NAME`, the joiner gets that name.

## Options

- `NAME` the new agent's name (`[a-z0-9-]`, up to 32), not taken by an agent or another open invite.
- `--role worker|main` the new agent's role; default `worker`. `main` only when no agent is main and the server config sets roles.
- `--ttl SECS` how long the code lives, 1 to 3600; default 600.
- `--no-wait` print the code and stop; wait later with `--resume ID`.
- `--resume ID` wait again for invite `ID` and ask about it.
- `--yes-if-fingerprint WORDS` for scripted tests only: confirm when the fingerprint equals `WORDS` (spaces or dashes), deny otherwise, without asking. Never use it to link a real machine: it skips the human check this command exists for.
- `--list` the invites still open or waiting for a confirmation, never their codes.
- `--cancel ID` void an open invite; its code stops working at once.
- `--pair A B` link two existing worker agents, so they may send to each other (the role rules and `[permissions]` still apply). It is an explicit act of the owner and goes to the audit log. The main agent is linked only through invites.

## Examples

```sh
rack link mac --role worker
rack link --role worker --ttl 300 --no-wait
rack link --resume 7
rack link --list
rack link --cancel 7
rack link --pair mac pc
```

## Prints

- The invite: `invite 7 for mac (worker), expires ...`, the code, and the `rack join` line to run on the other machine.
- Once redeemed: `redeemed by mac (worker): os linux, rack 0.1.3, from 192.168.1.30`, then `fingerprint: ...` and the question.
- `confirmed: linked with mac`, or `denied: mac and its token are deleted`.
- `--list`: one line per invite: id, name, role, state, and when it expires or until when it can be confirmed.

## Errors

- `403 not_admin`: your agent is not an admin agent.
- `409 conflict`: the name is taken by an agent or an open invite, a second main agent was asked for, the invite is not in the state needed, or it was confirmed too late.
- `404`: no invite with that id.
- A usage error: options of a new invite mixed with `--list`, `--cancel`, `--resume` or `--pair`, or a bad name or role.

## Exit codes

- 0 the link is confirmed (or, with `--no-wait`, `--list`, `--cancel`, `--pair`, the call worked)
- 1 denied, expired, voided, or a server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it. Linking needs a human at both terminals.
