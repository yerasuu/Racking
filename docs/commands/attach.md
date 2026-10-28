# rack attach

## Purpose

Adds files to a job you sent, right after sending it. Use it to retry an upload that failed, to attach more files, or to send a file too big for one request. Files can only be added while the job is still `sent`: once the recipient received it, attachments are locked.

A file up to 32 MiB goes in one request. A bigger one goes in resumable chunks (the server's `chunk_size`, 8 MiB by default): rack reads the file once to compute its sha256, then again chunk by chunk, so it never holds the whole file in memory, and the server checks the sha256 of the assembled file at the end. A progress line goes to stderr every few seconds (`disk.img 512/2048 MiB`). A chunk that fails is retried from the offset the server reports; after five failures in a row rack stops and the upload waits on the server for 24 h.

## Options

- `JOB` the job id, such as `j12`.
- `FILE...` one or more files. Each is named after the file, which must match `A-Za-z0-9._-` (1 to 100 characters). Up to the server's `upload_file_bytes` per file (2 GiB by default; `GET /whoami` shows it under `limits`, your quota under `quota`), 16 files and `job_file_bytes` per job, within your quota.
- `--resume` continue an upload that was cut: rack finds the open session of the same name, size and sha256 on the job and sends only what the server is missing. Without an open session it fails instead of starting over. A plain `rack attach JOB FILE` also continues an open session of the same file; `--resume` makes sure nothing restarts by mistake, and sends even a small file in chunks.

## Examples

```sh
rack attach j12 probe.bat
rack attach j12 a.log b.log
rack attach j12 disk.img            # 3 GiB: chunked, with progress on stderr
rack attach --resume j12 disk.img   # after a cut connection
```

## Prints

One line per file: `j12 attached probe.bat (812 bytes, sha256 ...)`.

## Errors

- `409 attachments_locked`: the job was already received, or it is a notice (no files).
- `413 too_large`: a file over the server's limit per file, or the job's files over its limit.
- `413 over_quota`: the files of the jobs you sent would pass your quota; the hint says your use and the limit.
- `507 insufficient_storage`: the server's disk is below its reserve; try later.
- `422 checksum_mismatch`: the file changed while it was sent; the session is gone, attach it again.
- A transport error after five failed chunks names the `rack attach --resume` line to continue.
- A local error when a file cannot be read, or changed size while it was sent.

## Exit codes

- 0 success
- 1 server, network or local error
- 2 usage error

## MCP

`racking_attach` (`job`, `path`): only files under the root directory (`racking_whoami` shows it), in one request, so up to 64 MiB. Bigger files: `rack attach` in a shell.

## Security

Check what you attach: the file is uploaded as it is. Never attach secrets.
