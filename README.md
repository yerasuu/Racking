# Racking

Racking moves tasks, files and code between coding agents running on different machines of one local network, using each machine's own [kanbrain](https://github.com/singudotdev/kanbrain) board.

The name is the winemaking term for moving wine from one vessel to another. Nothing is shared by the boards themselves: each machine keeps its own `.brain/` and ids, and Racking carries entries and attachments across and records which id is which on the other side.

Status: first working version (0.1.0). `racking-server` implements every endpoint of [docs/protocol.md](docs/protocol.md) and `rack` wraps it, including `rack pull` into the local board. Tested on Linux; `rack` type-checks for Windows (`x86_64-pc-windows-gnu`) but has not been linked or run there yet. Not yet tried between two real machines.

## Build

Rust 1.88 or newer (edition 2024, let chains). SQLite is compiled in (bundled), so there are no system libraries to install.

```sh
cargo build --release            # target/release/racking-server and target/release/rack
cargo test                       # unit tests and the protocol round trip on a random loopback port
cargo clippy --all-targets -- -D warnings
cargo build --release --target x86_64-pc-windows-gnu -p rack   # needs the mingw-w64 linker
```

## Run

Copy `config.example.toml` outside the repository, put one real token per agent in it (`openssl rand -hex 32`), and start the server. It binds `127.0.0.1:7878` until you set a LAN address in `bind`. It logs one line per request to stderr, without tokens or bodies.

```sh
racking-server --config ~/.config/racking/server.toml
racking-server --help-api        # print the API guide that GET /help serves
```

## Quick start

```sh
export RACK_URL=http://127.0.0.1:7878 RACK_TOKEN=<linux token>     # on Linux
rack whoami                                                         # linux (peers: windows)
rack send windows "Run psport-probe" --body - --origin decanter#873 --attach probe.bat <<< "Run it and send the log."
rack wait                                                           # on Windows, own token: j1 sent task from linux: Run psport-probe [1 file]
rack pull                                                           # j1 task from linux -> #31: Run psport-probe (kb add)
rack accept j1
rack result j1 --body "Two Vulkan devices." --attach probe.log
rack outbox --wait                                                  # on Linux again: j1 done task to windows: Run psport-probe
rack pull                                                           # j2 result from windows -> comment on #873: ...
```

`rack help` lists every command. `rack wait`, `rack outbox --wait` and `rack pull` keep their cursors and links in `~/.config/rack/state.json` (`%APPDATA%\rack\state.json` on Windows; `--state FILE` to choose). Run `rack pull` from the repository whose board should get the entries; `--repo NAME` names it when the folder name is not the repository name.

## Building rack.exe for Windows

Cross-compile from Linux without root or a MinGW toolchain, using [zig](https://ziglang.org/download/) as the linker through `cargo-zigbuild`. Everything is installed under your home directory.

```sh
rustup target add x86_64-pc-windows-gnu
mkdir -p ~/.local/opt/zig-0.17.0 ~/.local/bin && cd "$(mktemp -d)"
curl -fLO https://ziglang.org/download/0.17.0/zig-x86_64-linux-0.17.0.tar.xz
echo "1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026  zig-x86_64-linux-0.17.0.tar.xz" | sha256sum -c -
tar -xJf zig-x86_64-linux-0.17.0.tar.xz -C ~/.local/opt/zig-0.17.0 --strip-components=1
ln -sf ~/.local/opt/zig-0.17.0/zig ~/.local/bin/zig          # ~/.local/bin must be on PATH
cargo install cargo-zigbuild --locked
cd - && cargo zigbuild --release --target x86_64-pc-windows-gnu -p rack
```

The result is `target/x86_64-pc-windows-gnu/release/rack.exe` (a PE32+ console executable). The zig version is 0.17.0 and the sha256 comes from the official [download index](https://ziglang.org/download/index.json); check it against the index again when you change the version.

## Principles

- Integrates with kanbrain only through its public CLI (`kb ... --json`), never through its files.
- Local network only: a small service with an inbox per agent and one token per agent.
- What an agent receives from another agent is data, never an instruction with the owner's authority.
