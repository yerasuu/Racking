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

Agents are the `[agents.*]` sections of the config: give each machine its URL and token (see the README). Prefer pairing over copying tokens: see [Linking a new machine](#linking-a-new-machine). The first agent on a Docker server: `docker compose exec racking racking-server invite --config /config/server.toml --name pc1 --role worker` (not tested with Docker here).

### Upgrade

```sh
git pull && docker compose build && docker compose up -d
```

The volume and the config are kept. A stop sends SIGTERM and the server shuts down gracefully ([Stopping](#stopping)): it stops accepting, lets requests in flight finish and checkpoints SQLite. The compose file also runs it with `init: true` ([compose init](https://docs.docker.com/reference/compose-file/services/#init)), so the signal reaches the process even as PID 1, and `stop_grace_period` is 15 s, longer than the server's own 10 s grace. After a kill the SQLite docs say the next connection to open the database runs recovery ([SQLite WAL](https://www.sqlite.org/wal.html)), though they do not state it for a killed process specifically.

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

For Internet exposure, the only supported route is [Traefik](https://doc.traefik.io/traefik/) v3 (pinned to `v3.6.25` in `compose.yaml`) terminating TLS with Let's Encrypt. It is off by default (`profiles: ["traefik"]`). Traefik gets its routes from a file ([file provider](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/providers/others/file/)), not from the Docker socket, so it has no access to the Docker API.

1. DNS: an A/AAAA record for the domain pointing at the host, and the host's port 443 reachable from the Internet (the TLS-ALPN challenge needs only 443; port 80 only redirects to HTTPS).
2. Create the dynamic file and put your domain in it. The file provider does not expand environment or compose variables, so the domain is written in the file:

```sh
cp config/traefik-dynamic.yml.example config/traefik-dynamic.yml   # ignored by version control
# edit the Host(`racking.example.org`) rule, or generate it:
RACKING_DOMAIN=racking.example.org
sed "s/racking\.example\.org/$RACKING_DOMAIN/" config/traefik-dynamic.yml.example > config/traefik-dynamic.yml
```

3. Set `ACME_EMAIL=you@example.org` (shell or `.env`, Let's Encrypt expiry notices) and start with the override file, which removes the host port of `racking`, so only Traefik is reachable and Racking is on the compose network alone:

```sh
docker compose -f compose.yaml -f compose.traefik.yaml --profile traefik up -d
docker compose logs -f traefik
```

What the configuration sets, from the Traefik docs ([entrypoints](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/entrypoints/), [ACME](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/tls/certificate-resolvers/acme/), [RateLimit](https://doc.traefik.io/traefik/v3.6/reference/routing-configuration/http/middlewares/ratelimit/), [Buffering](https://doc.traefik.io/traefik/v3.6/reference/routing-configuration/http/middlewares/buffering/)):

- Entrypoint `web` on 80 redirects to `websecure` on 443, which has read 60 s, write 90 s and idle 120 s timeouts against slow connections. The write timeout stays above the 55 s maximum of `GET /inbox/wait`.
- In `config/traefik-dynamic.yml`: a rate limit of 50 requests/s (burst 100) per source address, a request body limit of 64 MiB (`maxRequestBodyBytes`, the server's largest attachment) through the Buffering middleware, and the service `http://racking:7878`.
- The certificate is stored in the `traefik-acme` volume as `acme.json`; Traefik creates it with mode 600.
- The router matches ``Host(`your domain`)`` only; other hosts get 404.
- Traefik watches the dynamic file (`providers.file.watch=true`); edits apply without a restart.

Known limitations:

- Until `trusted_proxies` is set (below), the server sees Traefik's address as the peer and ignores `X-Forwarded-For`, so per-address limits (the `/join` lockout) count every client as one.
- Traefik's timeouts are the only bound on slow connections: the server has no per-connection read timeout.
- ACME issuance could not be tested here (no public domain). Traefik starting with these options, routing `Host` to Racking over its self-signed default certificate, `/whoami` answering 401 and another host 404 was tested with Podman, with no socket mounted.

Alternative, label discovery. Traefik's own docs say that [unrestricted access to the Docker API is a security concern](https://doc.traefik.io/traefik/v3.6/reference/install-configuration/providers/docker/) (if Traefik is attacked, the attacker might get access to the host) and name the Tecnativa Docker Socket Proxy as a mitigation. It is not enabled here; a sketch, untested:

```yaml
# Traefik talks to a proxy that allows only the read verbs it needs, on an
# internal network with no route out; only the proxy mounts the socket.
#   socket-proxy:
#     image: tecnativa/docker-socket-proxy
#     environment: { CONTAINERS: 1, POST: 0 }     # everything else defaults to 0
#     volumes: ["/var/run/docker.sock:/var/run/docker.sock:ro"]
#     networks: [socket]
#   traefik:
#     command:
#       - --providers.docker.endpoint=tcp://socket-proxy:2375
#       - --providers.docker.exposedByDefault=false
#     networks: [socket, default]
#   networks:
#     socket: { internal: true }
```

You would then put `traefik.*` labels on the `racking` service instead of using the dynamic file.

#### Real client addresses behind Traefik

Two settings with similar names live on two different sides:

- **Traefik's `forwardedHeaders.trustedIPs`** (on the entrypoint) is about Traefik trusting *its own* upstream: "the IPs or CIDR from where Traefik trusts the forwarded headers information (`X-Forwarded-*`)", default none ([Traefik: entrypoints](https://doc.traefik.io/traefik/reference/install-configuration/entrypoints/)). It matters only when something else (a CDN, another proxy) sits in front of Traefik; leave it unset here. Traefik appends the address of whoever connected to it to `X-Forwarded-For` unless `notAppendXForwardedFor` is set (same page), so the rightmost entry Racking receives is the address Traefik saw.
- **Racking's `trusted_proxies`** (in `server.toml`) is about Racking trusting Traefik: only then does it read `X-Forwarded-For`, from the right, and take the first entry that is not a trusted proxy as the client. Anything the client wrote into the header itself sits further left and is never used.

To turn it on, with the Traefik route only (`compose.traefik.yaml` removes the host port, so Traefik is the only peer Racking can see):

1. Find the subnet of the compose network: `docker network inspect racking_default --format '{{(index .IPAM.Config 0).Subnet}}'` (the network is `<project>_default`).
2. Put it in `config/server.toml`: `trusted_proxies = ["172.18.0.0/16"]` (your subnet), and `docker compose restart racking`. The log line of each request then shows `peer=` (Traefik) and `client=` (the real address), and the `/join` lockout counts each client on its own.

Never set `trusted_proxies` while the port is also published on the host (the default `compose.yaml`): Docker NATs those connections, which can then arrive from the network's gateway, inside that subnet, and every LAN client would be trusted to name its own address. The subnet changes if the network is recreated; check it after `docker compose down`. Not tested with Docker here (no Docker on the test machine); the server side is covered by the integration tests on loopback.

## Stopping

On SIGTERM (`docker compose stop`, `systemctl --user stop`) or SIGINT (Ctrl-C), the server:

1. stops taking requests and closes its port; long polls (`/inbox/wait`, `/outbox/wait` and the pairing waits) answer at once with what they have, so clients call again after the restart;
2. lets the requests already running finish for up to `shutdown_grace_secs` (config, default 10, at most 60);
3. moves SQLite's write-ahead log into the database and empties it (`PRAGMA wal_checkpoint(TRUNCATE)`, [SQLite: wal_checkpoint](https://www.sqlite.org/pragma.html#pragma_wal_checkpoint)), and exits with status 0. Each step is logged.

A request still running when the grace period ends, such as a slow upload, is cut off: nothing of it is kept, and the client uploads again. A second SIGTERM or SIGINT exits at once with status 143 or 130; SQLite's WAL then recovers at the next start, as after a crash.

The supervisor must wait longer than the grace period, or it kills the server before the checkpoint. Docker waits 10 s by default, the same as the default grace period: with the default, set `stop_grace_period: 15s` on the `racking` service in `compose.yaml` (or lower `shutdown_grace_secs`), and keep it above `shutdown_grace_secs` plus a few seconds if you raise that ([Compose: stop_grace_period](https://docs.docker.com/reference/compose-file/services/#stop_grace_period)). systemd waits `TimeoutStopSec=`, which defaults to `DefaultTimeoutStopSec=`, 90 s in the user manager too, and is enough ([systemd.service](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html), [systemd-system.conf](https://www.freedesktop.org/software/systemd/man/latest/systemd-system.conf.html)).

## Linking a new machine

A new agent joins with a one-time code and a fingerprint both people compare; nobody copies a token by hand. Read [Linking in the protocol](protocol.md#linking) for the details and [SECURITY.md](../SECURITY.md) for why the check is mandatory. The names below are examples.

1. On a machine with an admin agent (`admin = true` in `server.toml`), run `rack link pc1 --role worker`. It prints a code like `ABCD-EFGH-JKMN-PQRS` and waits.
2. Give the code to whoever sits at the new machine, by a channel you trust (read it aloud, or type it from your screen).
3. On the new machine, with `rack` installed, run `rack join ABCD-EFGH-JKMN-PQRS --server http://192.168.1.20:7878`. It saves `rack.conf` (mode 600) and shows four words.
4. Back at the first machine, `rack link` shows the joiner's name, OS, `rack` version, address and four words. If they are the same words as on the new machine, type `y`; otherwise press Enter to deny, and the new agent and its token are deleted.
5. `rack join` prints `linked with ...` and the next steps (registering the MCP tools without the token, `rack guide`).

**The first agent, without an admin agent at hand.** On the server machine itself, as the user that runs the service:

```sh
racking-server invite --config ~/.config/racking/server.toml --name pc1 --role worker
```

It needs no token: it opens the database in `data_dir` directly, which only the owner of that directory can, and works while the service runs. It prints the code and the `rack join` line, waits, and asks the same question on its terminal; `--no-wait` prints the code and stops, and `--resume ID` waits again. An agent made this way is linked with no other API agent; it reaches the agents of `server.toml`, which are linked with everyone.

`rack link --list` shows open invites, `rack link --cancel ID` voids one, `rack links` lists an agent's links and `rack unlink NAME` removes one.

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
