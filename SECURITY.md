# Security

Racking is meant for a trusted local network: one service, one Bearer token per agent, no TLS. Do not expose it to the internet. The server runs on Unix (Linux); the `rack` client and `racking-mcp` also run on Windows.

## What it protects

- **Who can connect.** Before reading a token, the server checks the TCP peer address against `allow_from` (loopback and the private and link-local ranges by default) and refuses any other with 403. `X-Forwarded-For` is ignored; behind a reverse proxy the peer is the proxy, so filter there too. The server refuses to bind a public or wildcard address (`0.0.0.0`) unless the config says `allow_public_bind = true`, and then warns at every start.
- **Who is who.** Every route except `GET /` and `GET /help` needs a token. File tokens live in the server config (keep it private); tokens made with the admin API are 32 bytes from `/dev/urandom`, shown once and stored only as their SHA-256. All are compared by digest in constant time. Rotating or removing an API agent takes effect on the next request, without a restart. Answers that carry a token are never kept for idempotent replays.
- **Who may tell whom what.** The server gives each agent a role from its config: one `main` (the agent the owner talks to) and `worker`s. A job's `from_role` comes from the server, never from the request, so no message can claim the owner's authority. With roles set, a worker may start jobs only for the main agent unless `[permissions]` says otherwise. Only a job with `from_role: main` may carry delegated authority, and what a worker does with it is its local policy's call (a later part of #43); everything else is data.
- **Runaway agents.** Chains of tasks answered with tasks stop at 3 hops, open tasks per sender and recipient and jobs per sender per minute are capped, and the owner's emergency brake (`POST /admin/pause`) stops workers from sending while still delivering, so every agent learns it should stop.
- **Accountability.** Admin actions and refused admin calls go to an audit table (who, what, target, result), never bodies or tokens.
- `rack update` checks a sha256 only. A compromised service or admin token can serve a bad binary (see the decision on update distribution in the project board notes).

## What it does not protect

- **No TLS.** On the LAN, tokens and job contents travel in clear; anyone who can sniff the network can read them and replay a token. Use a network you trust, or a tunnel (WireGuard, SSH) you set up yourself.
- **A stolen token is that agent** until it is rotated or removed from the config. A stolen admin token can create agents, read the audit log, pause the server and publish `rack` binaries. A stolen main token carries the owner's authority as far as the workers' local policies allow.
- **`allow_from` trusts the network.** Any host inside the allowed ranges can try tokens; a spoofed source address on the LAN is not detected.
- **Content is never checked.** Bodies, titles, attachments, `about` lines and capabilities are whatever the sender wrote. The roles decide who sent a job, not whether what it says is true or safe; a main agent can still be fooled by data it read and pass it on.
- **`parent` is voluntary.** A sender that leaves it out starts a new chain, so the depth limit stops honest loops, not a hostile agent; the per-sender limits and the brake are the backstop.
- **The server host.** Whoever controls the machine, its config file or its database controls Racking. The database holds token hashes, jobs and attachments in clear.

Report a vulnerability privately through GitHub: [Security advisories](https://github.com/yerasuu/Racking/security/advisories/new). Please do not open a public issue for it.
