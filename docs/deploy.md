# Deploying racking-server

Racking is meant for a trusted local network with no TLS; read [SECURITY.md](../SECURITY.md) first. Two routes: a container (recommended, below) or a systemd user service ([without Docker](#without-docker-systemd-user-service)).

## Docker

Files at the repository root: `Dockerfile` (multi-stage: Rust builder, `debian:bookworm-slim` runtime, user 10001), `compose.yaml` and [config/server.example.toml](../config/server.example.toml). The image holds no config, token or data. It is not a static musl build: the musl target is not installed by default and the bundled SQLite would need a musl C toolchain; `bookworm-slim` matches the builder's glibc and the image is about 80 MB.

### Configure

```sh
cp config/server.example.toml config/server.toml
chmod 600 config/server.toml
openssl rand -hex 32     # once per agent; paste each into its `token`, replacing CHANGE-ME
```

`config/server.toml` is git-ignored and mounted read-only at `/config/server.toml`. Keep these settings of the example:

- `bind = "0.0.0.0:7878"`, `allow_public_bind = true` and `allow_from = ["0.0.0.0/0", "::/0"]`: inside the container the server listens on every interface (or the published port cannot reach it) and accepts any peer. This is intended there and only there: Docker NATs published ports, so the server sees the bridge address instead of the client and `allow_from` cannot tell clients apart. Network exposure is decided **only** by the host address `compose.yaml` publishes the port on (`RACKING_BIND_IP`, default `127.0.0.1`; never `0.0.0.0` or a public address). Access control is the tokens, the agent roles and the limits. The server logs a warning about the public bind at start; that is expected.
- `data_dir = "/data"`: the `racking-data` volume.

The server is never opened to the Internet directly. If it must be reachable from the Internet, only through [Traefik with TLS](#behind-traefik-tls).

### Start

```sh
docker compose up -d                                   # this machine only: 127.0.0.1:7878
RACKING_BIND_IP=192.168.1.20 docker compose up -d      # on the LAN (or put it in a .env file)
docker compose logs -f racking                         # look for: listening addr=0.0.0.0:7878
curl -s http://127.0.0.1:7878/help | head              # GET /help needs no token
docker compose ps                                      # health: healthy
```

Use your LAN address in `curl` if you published on it. The healthcheck is a bash `/dev/tcp` request for `GET /help` (the image has bash, not curl). The rootfs is read-only, the process runs as uid 10001 with every capability dropped, and logs rotate at 3 files of 10 MB.

### Peer addresses

With a published port on the default bridge network, the server sees the bridge gateway or the host instead of the real client (tested with rootless Podman: a request from the host's own shell arrived as the host's LAN address, not `127.0.0.1`; Docker was not available to test). Per-peer logs and per-IP limits therefore do not identify clients. If you want real peer addresses on Linux, use host networking: in `compose.yaml` remove `ports:`, uncomment `network_mode: host` and set `bind = "192.168.1.20:7878"` (your LAN address; `allow_public_bind` is then not needed). Tradeoff: no network isolation and no port mapping, so the bind address in the config and the host firewall are the only limits on exposure.

### Agents

Agents are the `[agents.*]` sections of the config: give each machine its URL and token (see the README). `racking-server invite` does not exist in this version; the step `docker compose exec racking racking-server invite --config /config/server.toml` is available after the join feature.

### Upgrade

```sh
git pull && docker compose build && docker compose up -d
```

The volume and the config are kept. The server does not handle SIGTERM, so a stop waits Docker's 10 s grace period and then kills it; SQLite's WAL recovers from that.

### Backup

The `racking-data` volume holds the SQLite database (`racking.db`, with `-wal` and `-shm` while running) and the attachments. Do not `cp` the database of a running server. Either stop it and archive the volume, or use the SQLite online backup ([SQLite backup](https://www.sqlite.org/backup.html)):

```sh
docker compose stop racking
docker run --rm -v racking_racking-data:/data:ro -v "$PWD":/out debian:bookworm-slim \
    tar czf /out/racking-data.tgz -C /data .
docker compose start racking
```

The volume is named `<project>_racking-data` (`docker volume ls`; the project is the directory name, `racking` here). Back up `config/server.toml` too, as privately as it is kept.

### Firewall

Docker publishes ports with its own iptables rules, which bypass `ufw`'s rules. Limit exposure with the published address (`RACKING_BIND_IP`) and, if needed, rules in the `DOCKER-USER` chain ([Docker packet filtering](https://docs.docker.com/engine/network/packet-filtering-firewalls/)). Never forward the port to the internet.

### Raspberry Pi (aarch64)

Build on the Pi with the same commands (slow), or for it from a PC:

```sh
docker buildx build --platform linux/arm64 -t racking-server:local --load .
```

Cross-building needs QEMU emulation on the PC and is slow; `--push` to a registry and pull on the Pi works too. Keep backups off the SD card.

### Behind Traefik (TLS)

For Internet exposure, the only supported route is [Traefik](https://doc.traefik.io/traefik/) v3 (pinned to `v3.6.25` in `compose.yaml`) terminating TLS with Let's Encrypt. It is off by default (`profiles: ["traefik"]`).

1. DNS: an A/AAAA record for the domain pointing at the host, and the host's port 443 reachable from the Internet (the TLS-ALPN challenge needs only 443, not 80).
2. Set the variables (shell or `.env`): `RACKING_DOMAIN=racking.example.org` and `ACME_EMAIL=you@example.org` (Let's Encrypt expiry notices).
3. Start with the override file, which removes the host port of `racking`, so only Traefik is reachable and Racking is on the compose network alone:

```sh
docker compose -f compose.yaml -f compose.traefik.yaml --profile traefik up -d
docker compose logs -f traefik
```

What the configuration sets, from the Traefik docs ([entrypoints](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/entrypoints/), [ACME](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/tls/certificate-resolvers/acme/), [RateLimit](https://doc.traefik.io/traefik/v3.6/reference/routing-configuration/http/middlewares/ratelimit/), [Buffering](https://doc.traefik.io/traefik/v3.6/reference/routing-configuration/http/middlewares/buffering/)):

- Entrypoint `websecure` on 443 with read 60 s, write 90 s and idle 120 s timeouts against slow connections. The write timeout stays above the 55 s maximum of `GET /inbox/wait`.
- A rate limit of 50 requests/s (burst 100) per source address and a request body limit of 64 MiB (`maxRequestBodyBytes`, the server's largest attachment) through the Buffering middleware.
- The certificate is stored in the `traefik-acme` volume as `acme.json`; Traefik creates it with mode 600.
- The router matches ``Host(`$RACKING_DOMAIN`)`` only; other hosts get 404.

Known limitations:

- The server sees Traefik's address as the peer, and `X-Forwarded-For` is ignored. Per-IP limits (for example on `/join`) count the proxy as one client. Trusting `X-Forwarded-For` from a known proxy is a later server change.
- Traefik reads container labels through the Docker socket, which is root-equivalent on the host even when mounted read-only. Accept that, or run Traefik with a static file configuration instead.
- ACME issuance could not be tested here (no public domain). Traefik starting, parsing these options and routing `Host` to Racking over its self-signed default certificate was tested with Podman.

## Without Docker: systemd user service

`racking-server` runs as a systemd **user** service on a desktop or a Raspberry Pi. No root is needed. Files in [deploy/](../deploy/):

- `racking-server.service`: the unit, installed in `~/.config/systemd/user/`.
- `install.sh`: installs the binary, a config with fresh tokens, the unit, and starts it.

### Install

From a checkout, with a built server (`cargo build --release -p racking-server`):

```sh
deploy/install.sh --dry-run --bin target/release/racking-server   # print the plan, change nothing
deploy/install.sh --bin target/release/racking-server             # loopback only: 127.0.0.1:7878
deploy/install.sh --bin target/release/racking-server --bind 192.168.1.20:7878   # on the LAN
```

It creates `~/.local/bin`, `~/.config/racking` and `~/.local/share/racking` (the last two mode 700), copies the binary atomically, installs and starts the unit, and runs `loginctl enable-linger` so the service starts at boot without anyone logging in. Without a reachable user manager (for example over a plain `su`), the systemctl and linger steps are skipped with a warning.

If `~/.config/racking/server.toml` does not exist it is written from [config.example.toml](../config.example.toml), mode 600, with fresh random tokens and `data_dir` set. The script never prints the tokens: open the file to copy each agent's token. **An existing `server.toml` is never overwritten**, and `--bind` only applies to a new file; edit the file to change it later, then restart the service. The script is safe to run again.

### First start check

```sh
systemctl --user status racking-server
journalctl --user -u racking-server -f      # look for: listening addr=...
curl -s http://127.0.0.1:7878/help | head   # GET /help needs no token
```

If the unit fails with `status=226/NAMESPACE`, the kernel does not allow the unprivileged user namespaces that `ProtectSystem=`, `ProtectHome=` and `PrivateTmp=` use in a user manager ([systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)). Delete those three lines and the `ReadWritePaths=` line in `~/.config/systemd/user/racking-server.service`, then `systemctl --user daemon-reload` and restart.

### Updating the binary

`rack update` updates the **client** only; the server is replaced by hand:

```sh
systemctl --user stop racking-server
cp target/release/racking-server ~/.local/bin/racking-server.new && mv ~/.local/bin/racking-server.new ~/.local/bin/racking-server
systemctl --user start racking-server
```

Or run `deploy/install.sh --bin PATH` again: it replaces the binary, keeps the config and restarts a running service.

### Backup

The data dir (`~/.local/share/racking`) holds a SQLite database (`racking.db`, with `-wal` and `-shm` files while running) and attachments. Do not copy the database file of a running server with `cp`. Use the SQLite online backup, see [SQLite backup](https://www.sqlite.org/backup.html) and the [`.backup` command](https://www.sqlite.org/cli.html#special_commands_to_sqlite3_dot_commands_):

```sh
dir=~/.local/share/racking
mkdir -p ~/racking-backup
sqlite3 "$dir/racking.db" ".backup '$HOME/racking-backup/racking.db'"
cp -a "$dir/attachments" "$dir/dist" ~/racking-backup/
```

Or stop the service, copy the whole directory, and start it again. Keep `server.toml` in the backup too: it holds the tokens, so store it as privately as it is here.

### Firewall

Allow only your LAN to reach the port, and never forward it to the internet. Examples, with the port from `--bind` and a `192.168.1.0/24` LAN; adjust both:

```sh
# ufw (example)
sudo ufw allow from 192.168.1.0/24 to any port 7878 proto tcp

# firewalld (example)
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="7878" protocol="tcp" accept'
sudo firewall-cmd --reload
```

### Raspberry Pi

Build on the Pi with `cargo build --release -p racking-server`, or cross-compile from a PC for 64-bit Raspberry Pi OS:

```sh
rustup target list --installed          # is aarch64-unknown-linux-gnu in the list?
rustup target add aarch64-unknown-linux-gnu   # if not
cargo zigbuild --release -p racking-server --target aarch64-unknown-linux-gnu
scp -r deploy config.example.toml target/aarch64-unknown-linux-gnu/release/racking-server pi:racking/
```

`cargo zigbuild` needs `zig` and `cargo-zigbuild` on the PC; install them yourself, nothing here does. On the Pi run `cd racking && deploy/install.sh --bin racking-server --bind PI_LAN_ADDRESS:7878`. With the data dir on an SD card, keep backups on other storage.

### Uninstall

```sh
deploy/install.sh --uninstall
```

Stops and disables the unit and removes the unit and the binary. Config and data stay; the script prints how to delete them (`rm -r ~/.config/racking ~/.local/share/racking`). It does not turn linger off: `loginctl disable-linger "$USER"` does.
