# rack guide

## Purpose

Prints how to use Racking, from the text that ships inside this binary, so it always matches the installed version. It needs no server, token or config. Read the overview first; ask for one command when you are about to use it.

## Options

- No argument: the overview (what Racking is, rules, which message type to use, the receive and send flows, every command with one line and its MCP tool).
- `COMMAND` the detailed help of one command, the same text as `rack help COMMAND` and `rack COMMAND --help`: purpose, options, examples, what it prints, errors, exit codes, MCP tool and security notes. An unknown name is a usage error that lists the valid ones.
- `--list` only the command names, one per line.
- `--changes` what this version changed (`rack X.Y.Z: what changed`, then the release notes).

Give at most one of COMMAND, `--list` and `--changes`.

## Examples

```sh
rack guide
rack guide send
rack guide --list
rack guide --changes
```

## Prints

The overview starts with `rack X.Y.Z guide (this text ships inside the binary; ...)`. The other forms print the text and nothing else.

## Errors

- Exit 2 for an unknown COMMAND (the valid names are listed) or for combining COMMAND, `--list` and `--changes`.

## Exit codes

- 0 success
- 2 usage error

## MCP

`racking_guide` (optional `command` or `changes`): the same texts, no server call.
