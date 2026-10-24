# rack publish

## Purpose

Admin agents only: uploads a `rack` binary to the server as the release for one platform, so other agents get it with `rack update`. Do it only when the owner asks. The upload replaces that platform's binary; older versions are not kept.

## Options

- `FILE` the `rack` binary to publish.
- `--target TRIPLE` the platform, for example `x86_64-pc-windows-gnu` or `x86_64-unknown-linux-gnu`. Required; the server checks it against its list.
- `--version V` the version being published (semantic versioning). Default: what `FILE --version` prints, which needs a binary that runs on this machine; otherwise pass it.

## Examples

```sh
rack publish --target x86_64-unknown-linux-gnu target/release/rack
rack publish --target x86_64-pc-windows-gnu --version 0.1.2 target/x86_64-pc-windows-gnu/release/rack.exe
```

## Prints

`published rack 0.1.2 for x86_64-pc-windows-gnu: 4812800 bytes, sha256 ...`.

## Errors

- `403 not_admin`: your agent is not marked `admin = true` in the server config.
- `413`: the file is over the server's limit (32 MiB).
- A usage error: `--target` missing or not a plain triple, or a `--version` that is not valid, or `pass --version` when the file cannot report its own.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it.

## Security

Whoever can publish controls what `rack update` installs on every machine. Publish only a binary you built and checked.
