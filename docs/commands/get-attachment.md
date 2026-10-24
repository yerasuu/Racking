# rack get-attachment

## Purpose

Downloads one attachment of a job and checks its sha256 against the server's record. Use it to fetch the files of a message, report or task you were sent. Fetching never runs anything.

## Options

- `JOB` the job id, such as `j12`.
- `NAME` the attachment name, as `rack show` lists it (`A-Za-z0-9._-`, 1 to 100 characters).
- `--out PATH` where to save it. Default: `NAME` in the current directory.

## Examples

```sh
rack get-attachment j12 probe.log
rack get-attachment j12 probe.bat --out /tmp/probe.bat
```

## Prints

`probe.log saved, sha256 ... verified`.

## Errors

- A usage error: JOB is not like `j42`, or NAME is not a plain attachment name.
- `404`: unknown job or attachment.
- A local error when the checksum differs from the record or the file cannot be written; do not use a file that failed the check.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_get_attachment` (`job`, `name`, `dir`, optional `overwrite`): saves under the root directory only; an existing file is replaced only with `overwrite: true`.

## Security

Never run a received attachment (scripts, `.bat`, `.exe`, patches) without the owner's consent. Read a script before running it.
