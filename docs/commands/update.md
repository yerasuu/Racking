# rack update

## Purpose

Installs the newest `rack` the Racking server publishes for this machine's platform. Do it only when the owner agrees: it replaces a program on their machine. With `--check` it only tells whether a newer one exists.

Versions are `X.Y.Z` for stable releases and `X.Y.Z-N` for test builds of the coming `X.Y.Z`, ordered by [Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html) precedence: `0.1.3 < 0.1.4-1 < 0.1.4-2 < 0.1.4`. The server keeps the latest of each kind. By default only stable releases count; a machine running a test build `X.Y.Z-N` moves on to the stable `X.Y.Z` as soon as it is published, since it is newer.

## Options

- `--check` report whether an update is available and change nothing.
- `--pre` let test builds count too: of the stable release and the test build, the newer one is installed if it is newer than this `rack`. A test build older than the stable release is ignored. Use it only when the owner asked to try a test build.

## Examples

```sh
rack update --check
rack update
rack update --check --pre
rack update --pre
```

## Prints

`--check`: `rack 0.1.2 (x86_64-unknown-linux-gnu), available 0.2.0: update available, run ...`, or `up to date`, or `none published, no update`. With `--pre`, one line per channel: `rack 0.1.3 (...), stable 0.1.3: up to date` and `rack 0.1.3 (...), pre 0.1.4-2: update available, run ...` (or `not newer than stable ..., ignored`, or `none published`). Update: the line "rack updated 0.1.2 -> 0.2.0", or `rack 0.1.2 is up to date (available 0.1.2)`. Only a newer version is installed.

## Errors

- No `rack` published for this platform: an admin agent publishes one with `rack publish`.
- A failed update (size or sha256 differs from the manifest, the new binary does not run or reports another version) keeps the current binary and says why and what to do.
- On Windows the previous binary stays as `rack.old` next to `rack.exe` until the next update.

## Exit codes

- 0 success, including already up to date
- 1 the update failed, or a server, network or local error
- 2 usage error

## MCP

None: there is no MCP tool for it. `racking-mcp` is updated by replacing its binary.

## Security

The binary comes from the Racking server on the LAN and is checked only by its sha256 in the server's manifest; it is not signed. Treat an update you did not expect as suspicious and ask the owner. After an update read `rack guide` again.
