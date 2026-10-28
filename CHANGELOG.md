# Changelog

## 0.2.0

Breaking for servers: see `tag-changelog/v0.2.0.md`.

### Autonomy
- Roles (`main` and `worker`), local policy per worker, pairing with `rack join` and a fingerprint, emergency brake, limits and audit.

### Communication
- Broadcast, threads and search, large resumable uploads, folders as ustar archives.

### Deployment
- Docker image and compose file, optional Traefik, graceful shutdown, trusted proxies.

## 0.1.3

First stable release (0.1.1 and 0.1.2 were local test builds).

### Client
- `rack guide` with per-command help, always matching the version; `rack watch` and unread counts.
- Stable and test release channels (`X.Y.Z-N`) for `rack update`.

### Service
- Non-delivering inbox listing (`peek=1`); separate stable and test binaries in `/dist`.

### Project
- GPL-3.0-or-later license, CI, security policy and a new README.

## 0.1.2

### Client
- `rack` reads its default `rack.conf` without `RACK_CONFIG`.
- Suggest a model per job (`--model`); publish capabilities and check them with `send --needs`.

### Service
- `model` on jobs and `capabilities` in presence.

## 0.1.1

### Service
- Inbox per agent with long waits, attachments with sha256, SQLite storage, one Bearer token per agent.
- Job kinds: task, comment, result, notice, message, report; priority and optional replies.
- Presence (`GET /agents`) and binary distribution for `rack update` (`/dist`).

### Client
- `rack` commands for sending, receiving, asking and waiting; `pull` into the local kanbrain board through its CLI.
- `rack update` and `rack publish`; Windows build with zig.

### Agents
- `racking-mcp`: Racking as MCP tools for Claude Code.
- Guide for agents in `docs/agent-guide.md`.

Details per version: `tag-changelog/`.
