# rack attach

## Purpose

Adds files to a job you sent, right after sending it. Use it to retry an upload that failed, or to attach more files. Files can only be added while the job is still `sent`: once the recipient received it, attachments are locked.

## Options

- `JOB` the job id, such as `j12`.
- `FILE...` one or more files. Each is named after the file, which must match `A-Za-z0-9._-` (1 to 100 characters). Up to 64 MiB per file and 16 files per job.

There are no `--` options.

## Examples

```sh
rack attach j12 probe.bat
rack attach j12 a.log b.log
```

## Prints

One line per file: `j12 attached probe.bat (812 bytes, sha256 ...)`.

## Errors

- `409 attachments_locked`: the job was already received, or it is a notice (no files).
- `413 too_large`: a file over 64 MiB.
- A local error when a file cannot be read.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_attach` (`job`, `path`): only files under the root directory (`racking_whoami` shows it).

## Security

Check what you attach: the file is uploaded as it is. Never attach secrets.
