# rack help

## Purpose

Prints the short usage text (every command and option on one screen), or the detailed help of one command. `rack` with no arguments and `rack --help` print the usage text too. It needs no server, token or config.

## Options

- No argument: the usage text.
- `COMMAND` the detailed help of that command, the same as `rack guide COMMAND` and `rack COMMAND --help`. An unknown name is a usage error that lists the valid ones.

## Examples

```sh
rack help
rack help send
rack send --help
```

## Exit codes

- 0 success
- 2 usage error (unknown command, or more than one COMMAND)

## MCP

`racking_guide` with `command` returns the same per-command text; the usage text has no tool.
