# rack publish

## Purpose

Admin agents only: uploads a `rack` binary to the server for one platform, so other agents get it with `rack update`. Do it only when the owner asks. The version picks the slot: a stable `X.Y.Z` replaces that platform's stable release, a test build `X.Y.Z-N` replaces its test build (installed only by `rack update --pre`); the other slot is left as it is, and older versions are not kept.

## Options

- `FILE` the `rack` binary to publish.
- `--target TRIPLE` the platform, for example `x86_64-pc-windows-gnu` or `x86_64-unknown-linux-gnu`. Required; the server checks it against its list.
- `--version V` the version being published: `X.Y.Z` (stable) or `X.Y.Z-N` (test build), [semantic versioning](https://semver.org/spec/v2.0.0.html) without build metadata (`+...`). Default: what `FILE --version` prints, which needs a binary that runs on this machine; otherwise pass it.

## Examples

```sh
rack publish --target x86_64-unknown-linux-gnu target/release/rack
rack publish --target x86_64-pc-windows-gnu --version 0.1.2 target/x86_64-pc-windows-gnu/release/rack.exe
rack publish --target x86_64-pc-windows-gnu --version 0.1.3-1 target/x86_64-pc-windows-gnu/release/rack.exe
```

## Prints

`published rack 0.1.2 for x86_64-pc-windows-gnu in the stable slot: 4812800 bytes, sha256 ...`; a test build says `in the pre slot`.

## Errors

- `403 not_admin`: your agent is not marked `admin = true` in the server config.
- `400 bad_request`: the version has build metadata (`+...`); publish `X.Y.Z` or `X.Y.Z-N`.
- `413`: the file is over the server's limit (32 MiB).
- A test build to a server from before test builds: refused before the upload, since that server would make it the stable release. Update `racking-server` first.
- A usage error: `--target` missing or not a plain triple, or a `--version` that is not valid, or `pass --version` when the file cannot report its own.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it.

## Security

Whoever can publish controls what `rack update` installs on every machine. Publish only a binary you built and checked.
