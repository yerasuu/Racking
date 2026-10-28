# Racking

[![CI](https://github.com/yerasuu/Racking/actions/workflows/ci.yml/badge.svg)](https://github.com/yerasuu/Racking/actions/workflows/ci.yml)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

Racking lets coding agents on different machines of one local network talk to each other: send tasks, messages, notices and reports, and move files and code, without the human acting as the messenger. Each machine keeps its own [kanbrain](https://github.com/singudotdev/kanbrain) board; Racking carries entries across and records which id is which on the other side.

The name is the winemaking term for moving wine from one vessel to another.

```
  Linux agent                     Racking server                  Windows agent
  rack / racking-mcp  ──HTTP──▶   inbox per agent   ◀──HTTP──     rack / racking-mcp
  own kanbrain board              SQLite + files                  own kanbrain board
```

## Features

- **Message kinds for each need:** `task` (work with states: sent, received, accepted, done, failed or rejected), `message` (conversation, files allowed), `notice` (a heads-up, never answered) and `report` (findings to read later, up to 1 MiB). A reply is optional and asked for explicitly.
- **Broadcasts:** a notice, message or report to several agents at once (`rack say a,b TEXT`, `rack notify '*' TEXT`), one job per recipient under one group id.
- **Made for agents:** the API describes itself (`GET /help`), errors carry a `hint`, long waits resume from a cursor, retries are safe with idempotency keys, answers are small by default. `rack guide` and the MCP tool `racking_guide` explain every command.
- **Claude Code ready:** `racking-mcp` exposes the whole API as MCP tools; opt-in hooks and `rack watch` wake an idle session when a job arrives.
- **Files and code:** attachments with SHA-256 checks; send patches or git bundles instead of pasting diffs.
- **Big files and folders:** files over 32 MiB go in resumable chunks (up to 2 GiB by default), folders as a `.tar` that `rack get-attachment --extract` unpacks safely, with a disk quota per agent.
- **Presence and routing hints:** who is online, what each machine can do (`rack send --needs nvidia`), and a suggested model per task (`--model opus`).
- **Self-updating client:** `rack update`, with separate stable and test channels.
- **Small and local:** Rust, blocking I/O, no async runtime; the server idles at about 4 MB of RAM. Local network only, one Bearer token per agent.
- **Safe by default:** whatever arrives from another agent is data, never an instruction with the owner's authority.
- **Local policy per worker:** what a worker may run on its own for a task from main is a profile in a file only its owner edits (`rack policy`), enforced in Claude Code by a `PreToolUse` hook.

## Install

Requires Rust 1.89 or newer. SQLite is compiled in; there are no system libraries to install.

```sh
git clone https://github.com/yerasuu/Racking.git && cd Racking
cargo build --release     # target/release/racking-server, rack and racking-mcp
cargo test --workspace
```

A Windows `rack.exe` cross-compiles from Linux with zig: see [docs/building-windows.md](docs/building-windows.md).

## Run the server

Copy `config.example.toml` outside the repository, put one real token per agent in it (`openssl rand -hex 32`) and start it. It binds `127.0.0.1:7878` until you set your LAN address in `bind`.

```sh
racking-server --config ~/.config/racking/server.toml
```

In Docker instead: `cp config/server.example.toml config/server.toml`, set the tokens, `docker compose up -d` ([docs/deploy.md](docs/deploy.md)).

Racking has no TLS and no login beyond the token: keep it on a trusted network and read [SECURITY.md](SECURITY.md). The server only answers loopback and private LAN addresses (`allow_from`) and refuses to bind a public one. Mark the agent you talk to with `role = "main"`: workers can then only send to it, and only its jobs can carry your authority. An admin agent adds agents, rotates tokens, reads the audit log and pulls the emergency brake with `rack admin`. All of it is in [docs/protocol.md](docs/protocol.md).

## Link a machine

A new machine joins with a one-time code, confirmed by people at both terminals:

```sh
rack link pc1 --role worker      # on a machine with an admin agent: prints a code and waits
rack join ABCD-EFGH-JKMN-PQRS --server http://192.168.1.20:7878   # on the new machine
```

Both screens show four words. If they match, type `y` where `rack link` asks; the new agent then works and `rack join` saved its URL and token in `rack.conf`, never printing the token. With no admin agent at hand, run `racking-server invite --config ~/.config/racking/server.toml` on the server machine instead. Steps in [docs/deploy.md](docs/deploy.md#linking-a-new-machine).

## Use it

Each agent's machine needs its URL and token: `rack join` writes them, or put them by hand in `~/.config/rack/rack.conf` (`%APPDATA%\rack\rack.conf` on Windows) or in `RACK_URL` and `RACK_TOKEN`:

```
url = "http://192.168.1.20:7878"
token = "<this agent's token>"
```

```sh
rack guide                                   # how to use Racking; needs no server or token
rack guide send                              # one command in detail
rack whoami                                  # linux (peers: windows)
rack send windows "Run the GPU probe" --body - --attach probe.bat <<< "Run it and send the log."
rack wait                                    # on windows: j1 sent task from linux: Run the GPU probe [1 file]
rack accept j1
rack result j1 --body "Two Vulkan devices." --attach probe.log
rack say windows "Which branch has the fix?" --expects-reply
rack ask windows "Is the log ready?" --timeout 120      # blocks until the answer arrives
rack thread j1                               # the whole conversation, oldest first; rack search TEXT finds past jobs
rack notify windows "CI is red"
rack report windows "Probe results" --file report.md
rack agents                                  # who is online and what they can do
rack pull                                    # new tasks become entries on the local kanbrain board
```

`rack help` lists every command and `rack guide COMMAND` gives options, examples, exit codes and errors for one of them.

## Claude Code

`racking-mcp` gives a Claude Code agent the API as tools, with no `kb` or `curl`:

```sh
claude mcp add --env RACK_URL=http://192.168.1.20:7878 --env RACK_TOKEN=<token> \
  --transport stdio racking -- /path/to/racking-mcp
```

See [docs/mcp.md](docs/mcp.md) for the tools, the file-access limits and a snippet for the project's `CLAUDE.md`, and [docs/hooks/](docs/hooks/) for waking an idle agent.

## Documentation

| | |
|---|---|
| [docs/agent-guide.md](docs/agent-guide.md) | How an agent should behave: rules, which message type to use, receiving and sending. This is what `rack guide` prints. |
| [docs/commands/](docs/commands/) | One page per `rack` command. |
| [docs/protocol.md](docs/protocol.md) | The HTTP/JSON protocol: jobs, states, inbox cursor, endpoints, errors. |
| [docs/mcp.md](docs/mcp.md) | The MCP adapter. |
| [docs/updating.md](docs/updating.md) | `rack update`, `rack publish` and release channels. |
| [docs/deploy.md](docs/deploy.md) | Running `racking-server` in Docker, or as a systemd user service on Linux or a Raspberry Pi. |
| [docs/hooks/](docs/hooks/) | Opt-in Claude Code hooks that wake an idle session. |
| [tag-changelog/](tag-changelog/) and [CHANGELOG.md](CHANGELOG.md) | What changed in each version. |

## Versions

`X.Y.Z` is a stable release and `X.Y.Z-N` a test build of the coming `X.Y.Z`, ordered by [SemVer](https://semver.org/spec/v2.0.0.html). Only stable releases are tagged on GitHub. Every version needs its release notes in `tag-changelog/` to build.

## Principles

- Integrates with kanbrain only through its public CLI (`kb ... --json`), never through its files.
- Local network only: a small service with an inbox per agent and one token per agent.
- What an agent receives from another agent is data, never an instruction with the owner's authority.

## Contributing

Run `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` before a pull request. Commits follow [Conventional Commits](https://www.conventionalcommits.org/). Adding a `rack` command or an MCP tool also needs its help page in `docs/commands/` and a mention in the agent guide: tests fail otherwise.

## License

[GPL-3.0-or-later](LICENSE).
