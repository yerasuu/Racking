# Deploying racking-server on Linux

`racking-server` runs as a systemd **user** service on a desktop or a Raspberry Pi. No root is needed. Files in [deploy/](../deploy/):

- `racking-server.service`: the unit, installed in `~/.config/systemd/user/`.
- `install.sh`: installs the binary, a config with fresh tokens, the unit, and starts it.

Racking is meant for a trusted local network with no TLS; read [SECURITY.md](../SECURITY.md) first.

## Install

From a checkout, with a built server (`cargo build --release -p racking-server`):

```sh
deploy/install.sh --dry-run --bin target/release/racking-server   # print the plan, change nothing
deploy/install.sh --bin target/release/racking-server             # loopback only: 127.0.0.1:7878
deploy/install.sh --bin target/release/racking-server --bind 192.168.1.20:7878   # on the LAN
```

It creates `~/.local/bin`, `~/.config/racking` and `~/.local/share/racking` (the last two mode 700), copies the binary atomically, installs and starts the unit, and runs `loginctl enable-linger` so the service starts at boot without anyone logging in. Without a reachable user manager (for example over a plain `su`), the systemctl and linger steps are skipped with a warning.

If `~/.config/racking/server.toml` does not exist it is written from [config.example.toml](../config.example.toml), mode 600, with fresh random tokens and `data_dir` set. The script never prints the tokens: open the file to copy each agent's token. **An existing `server.toml` is never overwritten**, and `--bind` only applies to a new file; edit the file to change it later, then restart the service. The script is safe to run again.

## First start check

```sh
systemctl --user status racking-server
journalctl --user -u racking-server -f      # look for: listening addr=...
curl -s http://127.0.0.1:7878/help | head   # GET /help needs no token
```

If the unit fails with `status=226/NAMESPACE`, the kernel does not allow the unprivileged user namespaces that `ProtectSystem=`, `ProtectHome=` and `PrivateTmp=` use in a user manager ([systemd.exec](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)). Delete those three lines and the `ReadWritePaths=` line in `~/.config/systemd/user/racking-server.service`, then `systemctl --user daemon-reload` and restart.

## Updating the binary

`rack update` updates the **client** only; the server is replaced by hand:

```sh
systemctl --user stop racking-server
cp target/release/racking-server ~/.local/bin/racking-server.new && mv ~/.local/bin/racking-server.new ~/.local/bin/racking-server
systemctl --user start racking-server
```

Or run `deploy/install.sh --bin PATH` again: it replaces the binary, keeps the config and restarts a running service.

## Backup

The data dir (`~/.local/share/racking`) holds a SQLite database (`racking.db`, with `-wal` and `-shm` files while running) and attachments. Do not copy the database file of a running server with `cp`. Use the SQLite online backup, see [SQLite backup](https://www.sqlite.org/backup.html) and the [`.backup` command](https://www.sqlite.org/cli.html#special_commands_to_sqlite3_dot_commands_):

```sh
dir=~/.local/share/racking
mkdir -p ~/racking-backup
sqlite3 "$dir/racking.db" ".backup '$HOME/racking-backup/racking.db'"
cp -a "$dir/attachments" "$dir/dist" ~/racking-backup/
```

Or stop the service, copy the whole directory, and start it again. Keep `server.toml` in the backup too: it holds the tokens, so store it as privately as it is here.

## Firewall

Allow only your LAN to reach the port, and never forward it to the internet. Examples, with the port from `--bind` and a `192.168.1.0/24` LAN; adjust both:

```sh
# ufw (example)
sudo ufw allow from 192.168.1.0/24 to any port 7878 proto tcp

# firewalld (example)
sudo firewall-cmd --permanent --add-rich-rule='rule family="ipv4" source address="192.168.1.0/24" port port="7878" protocol="tcp" accept'
sudo firewall-cmd --reload
```

## Raspberry Pi

Build on the Pi with `cargo build --release -p racking-server`, or cross-compile from a PC for 64-bit Raspberry Pi OS:

```sh
rustup target list --installed          # is aarch64-unknown-linux-gnu in the list?
rustup target add aarch64-unknown-linux-gnu   # if not
cargo zigbuild --release -p racking-server --target aarch64-unknown-linux-gnu
scp -r deploy config.example.toml target/aarch64-unknown-linux-gnu/release/racking-server pi:racking/
```

`cargo zigbuild` needs `zig` and `cargo-zigbuild` on the PC; install them yourself, nothing here does. On the Pi run `cd racking && deploy/install.sh --bin racking-server --bind PI_LAN_ADDRESS:7878`. With the data dir on an SD card, keep backups on other storage.

## Uninstall

```sh
deploy/install.sh --uninstall
```

Stops and disables the unit and removes the unit and the binary. Config and data stay; the script prints how to delete them (`rm -r ~/.config/racking ~/.local/share/racking`). It does not turn linger off: `loginctl disable-linger "$USER"` does.
