# Updating rack

The server doubles as the place `rack` updates itself from. One agent, marked `admin = true` in the server config, publishes a release build per platform:

```sh
rack publish --target x86_64-unknown-linux-gnu target/release/rack        # version read from `rack --version`
rack publish --target x86_64-pc-windows-gnu --version 0.2.0 \
  target/x86_64-pc-windows-gnu/release/rack.exe                           # not runnable here: say the version
```

Every machine then runs, with its own token:

```sh
rack update --check     # rack 0.1.0 (x86_64-pc-windows-gnu), available 0.2.0: update available, run `rack update`
rack update             # rack updated 0.1.0 -> 0.2.0
rack --version
```

Versions are `X.Y.Z` for stable releases and `X.Y.Z-N` for test builds of the coming `X.Y.Z`, ordered by [SemVer](https://semver.org/spec/v2.0.0.html) precedence (`0.1.4-1 < 0.1.4-2 < 0.1.4`). The server keeps one of each per platform and `rack publish` puts a binary in the slot its version names, so publishing a test build never replaces the stable release. `rack update` installs stable releases only; `rack update --pre` takes the newer of the two (`rack update --check --pre` prints one line per channel). A machine on `0.1.4-1` moves to `0.1.4` with a plain `rack update` once it is published.

`rack update` installs only a newer version, checks the size and SHA-256 from the server's manifest, and runs the new binary with `--version` before it replaces itself; if anything fails it keeps the current binary and says why. On Windows the previous binary is left as `rack.old` next to `rack.exe` and removed by the next update. Binaries are not signed: the SHA-256 guards against a broken transfer, not against a compromised server or admin token. See [docs/protocol.md](protocol.md#distribution-of-rack).
