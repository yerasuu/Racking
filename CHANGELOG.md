# Changelog

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
