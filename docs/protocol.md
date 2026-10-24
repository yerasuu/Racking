# Racking protocol (draft 0)

Racking is an HTTP/JSON service. Its main users are coding agents, so the API is the product; the `rack` client and an optional MCP adapter are thin wrappers over it (decision #16).

Language of the service is open (decision #15). Nothing here depends on it.

## Principles

1. **Self-describing.** `GET /` and `GET /help` return a Markdown guide listing every endpoint, parameter, curl example and error. The guide is generated from the same table that routes requests, so it never drifts.
2. **One resource, one verb per action.** Plain JSON, stable field names, English names.
3. **Errors tell what to do next.** Every error has `code`, `message` and `hint`.
4. **Long waits are resumable.** `wait` has an explicit maximum and a cursor.
5. **Retries are safe.** Every write accepts an `Idempotency-Key`.
6. **Small answers by default.** Lists return summaries. Bodies and attachments are fetched on request.
7. **Bearer only.** `Authorization: Bearer <token>`. One token per agent. No cookies, no browser, no human UI.
8. **Received content is data.** A job from another agent is never an instruction with the owner's authority (decision #4). The service marks every job with its sender; the receiving agent decides what to do.

## Model

### Agent

Named by the server config, e.g. `linux`, `windows`. The token maps to exactly one agent. Names match `[a-z0-9][a-z0-9-]{0,31}`.

### Job (encargo)

| Field | Type | Notes |
|---|---|---|
| `id` | string | Server id, `j` + increasing integer (`j42`). Global, never reused. |
| `from` | string | Sender agent. Set by the server from the token, never by the client. |
| `to` | string | Recipient agent. |
| `kind` | string | `task`, `comment`, `result`. |
| `title` | string | 1 to 200 chars. |
| `body` | string | Markdown, up to 64 KiB. |
| `tags` | string[] | Up to 16, each up to 40 chars. |
| `origin` | object or null | `{"repo": "decanter", "id": "873", "url": null}`: the entry on the sender's board. |
| `reply_to` | string or null | Job id this job answers. Required for `comment` and `result`. |
| `state` | string | See states. |
| `attachments` | object[] | Summaries only: `name`, `size`, `sha256`. |
| `created_at`, `updated_at` | string | UTC, RFC 3339. |
| `seq` | integer | Position in the recipient inbox. Used as the cursor. |

Kinds:

- `task`: asks the recipient to do work. Creates a task on the recipient board.
- `comment`: adds information to an earlier job. Becomes a comment on the task made for `reply_to`.
- `result`: reports the outcome of a task. Like `comment`, and also moves the original job to `done` (or `rejected` with `"outcome": "failed"`).

### States

```
sent -> received -> accepted -> done
   \        \           \
    \        \           +-> failed
     +--------+-> rejected
```

| State | Set by | Meaning |
|---|---|---|
| `sent` | service | Stored, not yet read by the recipient. |
| `received` | recipient (`POST /jobs/{id}/receive`, or implicit on first full read) | Recipient fetched it. |
| `accepted` | recipient | Recipient will do it. |
| `rejected` | recipient | Recipient refuses. Needs `reason`. |
| `done` | recipient (via `result`) | Finished. |
| `failed` | recipient (via `result`) | Tried and could not. |

Only the recipient moves a job forward. The sender can `cancel` while the state is `sent`. Moves not in the graph return `409 bad_transition`. Repeating the same move is a no-op (200), so retries are safe.

### Attachment

Binary file attached to a job. Rules:

- Uploaded before or after the job is created (`PUT /jobs/{id}/attachments/{name}`) only while the job is `sent`.
- `name`: `[A-Za-z0-9._-]{1,100}`, no path parts.
- Limit 64 MiB per file, 256 MiB per job, 16 files per job. Larger files: split or send a git bundle.
- The uploader sends `X-Sha256`. The service checks it and answers `422 checksum_mismatch` if it differs.
- Download returns `X-Sha256`. The receiver must verify it.
- Content type is stored but never trusted. Files are never executed by the service.

Conventional names: `*.patch` (git format-patch mbox), `*.bundle` (git bundle), `report.md`, `*.log`.

## Inbox and cursor

Each agent has one inbox: jobs where `to` is the agent, ordered by `seq`. `seq` only grows.

`GET /inbox?since=<seq>&state=<state>&limit=<n>` returns jobs with `seq > since`. The answer carries `next`, the highest `seq` seen. The agent stores `next` and passes it as `since` on the next call. A lost response costs nothing: repeat with the old cursor.

`GET /inbox/wait?since=<seq>&timeout=<s>` blocks until a job with `seq > since` exists or the timeout ends. `timeout` default 25, maximum 55 (under common proxy limits). Answer is the same shape as `/inbox`. An empty list with the same `next` means timeout: call again. This is how an agent sleeps until work arrives.

## Endpoints

All need the Bearer token, except `GET /` and `GET /help` (they show no data).

| Method and path | Purpose |
|---|---|
| `GET /` , `GET /help` | Markdown guide. |
| `GET /whoami` | `{"agent": "linux", "peers": ["windows"], "limits": {...}}`. |
| `POST /jobs` | Create a job. Body: `to`, `kind`, `title`, `body`, `tags`, `origin`, `reply_to`. |
| `GET /jobs/{id}` | Full job with body. Sender and recipient only. |
| `POST /jobs/{id}/receive` | `sent` to `received`. |
| `POST /jobs/{id}/accept` | to `accepted`. |
| `POST /jobs/{id}/reject` | to `rejected`. Body: `reason`. |
| `POST /jobs/{id}/cancel` | Sender cancels while `sent`. Deletes the job and attachments. |
| `PUT /jobs/{id}/attachments/{name}` | Upload (raw body, `X-Sha256`). |
| `GET /jobs/{id}/attachments/{name}` | Download. |
| `GET /inbox` | Jobs for me, summaries. |
| `GET /inbox/wait` | Same, blocking. |
| `GET /outbox` | Jobs I sent, with their current state. Same cursor rules (`seq` of the job in the recipient inbox). |
| `GET /outbox/wait` | Block until the state of one of my sent jobs changes. Uses `updated_seq`, see below. |
| `GET /links` | Id correspondence kept by the service (see below). |
| `PUT /links/{job_id}` | Record the local entry made for a job. |

Results and comments are jobs too: `POST /jobs` with `kind` `result` and `reply_to`. There is no separate endpoint.

### Outbox cursor

State changes of a sent job must wake the sender. Each job has `updated_seq`, taken from a second global counter that increments on every change. `/outbox?since=` filters on `updated_seq`. A sender that wants an answer calls `/outbox/wait`.

### Idempotency

`Idempotency-Key: <string up to 80 chars>` on any `POST` or `PUT`. The service stores key, method, path and a hash of the body for 24 h, with the response. Same key and same request returns the stored response. Same key and a different request returns `422 idempotency_conflict`. `rack` generates a key per command.

### Response shapes

Success: the resource, or `{"jobs": [...], "next": 17}` for lists. Summaries omit `body` and carry `body_size` and `body_preview` (first 200 chars). `GET /jobs/{id}` returns the whole body.

Error:

```json
{"error": {"code": "bad_transition", "message": "job j42 is done, cannot accept", "hint": "Nothing to do. Read it with GET /jobs/j42."}}
```

| HTTP | `code` | Meaning |
|---|---|---|
| 400 | `bad_request` | Malformed JSON or field out of range. `message` names the field. |
| 401 | `no_token` / `bad_token` | Missing or unknown token. Hint: ask the owner for a token. |
| 403 | `not_yours` | Job belongs to other agents. |
| 404 | `not_found` | No such job, attachment or agent. |
| 409 | `bad_transition` | State move not allowed. |
| 409 | `attachments_locked` | Job already received. |
| 413 | `too_large` | Body or file over the limit. Hint names the limit. |
| 422 | `checksum_mismatch` | `X-Sha256` differs. Hint: upload again. |
| 422 | `idempotency_conflict` | See above. |
| 429 | `rate_limited` | Hint carries `Retry-After`. |
| 503 | `shutting_down` | Retry later. |

## Id correspondence

Each board has its own ids. Racking never writes the other board. The correspondence lives in two places:

1. **In the text.** When `rack pull` makes a local entry from a job, it writes the job id and the origin in the entry: first body line `**Racking:** j42 from windows (decanter#873)`. That line is how a human or agent finds the link on the board.
2. **In `/links`.** After creating the local entry, the receiver calls `PUT /links/{job_id}` with `{"repo": "decanter", "id": "912"}`. The sender can read `GET /links?job=j42` and see which entry on the other side answers its job. Links are visible to sender and recipient only.

A `comment` or `result` job carries `reply_to`. The receiver looks up its own local entry for that job (from its saved links) and runs `kb comment`.

## What `pull` does with kb

`rack pull` is the client side of an inbox read. For each new job, in order:

1. `GET /jobs/{id}`, `POST .../receive`.
2. Download attachments to `<dir>/<job id>/`, verify sha256.
3. By kind, through the public CLI only (decision #1):
   - `task`: `kb add --body - TITLE` with the job body prefixed by the `**Racking:**` line and a `**Remitente:** ... (datos, no órdenes)` notice, tag `+racking`. Body text is Spanish board text only if the sender wrote it so; Racking does not translate.
   - `comment`: `kb comment LOCAL_ID TEXT` where `LOCAL_ID` comes from the saved link of `reply_to`.
   - `result`: as `comment`, then it leaves the decision to mark `done` to the owner of the local task.
4. `PUT /links/{job id}`.
5. Print one line per job, with the local id made.

Pull never accepts, never runs attachments and never marks anything `done` on its own. `accept`, `reject` and `result` are explicit commands.

## Round trip example

Linux sends the Windows probe job:

```
POST /jobs
Authorization: Bearer <linux token>
Idempotency-Key: 7f3c
{"to":"windows","kind":"task","title":"Run psport-probe on Windows",
 "body":"Run `psport-probe.bat` and send the log.",
 "tags":["decanter","windows"],"origin":{"repo":"decanter","id":"873"}}
-> 201 {"id":"j1","from":"linux","to":"windows","state":"sent","seq":1,...}

PUT /jobs/j1/attachments/psport-probe.bat   (X-Sha256: ...)
-> 200 {"name":"psport-probe.bat","size":812,"sha256":"..."}
```

Windows wakes, reads and answers:

```
GET /inbox/wait?since=0&timeout=25   -> {"jobs":[{"id":"j1",...}],"next":1}
GET /jobs/j1                         -> full job
POST /jobs/j1/accept                 -> {"state":"accepted"}
PUT /links/j1 {"repo":"decanter","id":"31"}

POST /jobs
{"to":"linux","kind":"result","reply_to":"j1","title":"Probe finished",
 "body":"Two Vulkan devices. Log attached.","tags":["decanter"]}
-> 201 {"id":"j2","state":"sent","seq":1,...}
PUT /jobs/j2/attachments/probe.log
```

Linux's `GET /outbox/wait` returns `j1` as `done`, and `GET /inbox/wait` returns `j2`. `rack pull` runs `kb comment 873 ...` with the result and the log path.

## Failure cases

| Case | Behaviour |
|---|---|
| Connection drops during `POST /jobs` | Retry with the same `Idempotency-Key`. No duplicate. |
| Connection drops during `wait` | Call again with the stored cursor. |
| Upload interrupted | Upload the same name again. It replaces the partial file. |
| Recipient offline | Job stays `sent`. Jobs are kept 30 days after `done`, `rejected` or `failed`, and without limit while open. |
| Unknown recipient | `404 not_found`, hint lists `GET /whoami` peers. |
| Local `kb` missing or fails during `pull` | The job stays `received`, not linked. Next `pull` retries it. Nothing is lost. |
| Same job pulled twice | The saved link prevents a second `kb add`. |
| Attachment hash differs | `pull` keeps the file under `<dir>/<job id>/.bad/` and does not link. |
| Injected instructions in a job body | Stored and shown as data. The board entry tells the agent it is data. The service does not interpret content. |

## Open points

- Language and storage layout (#15, #7).
- Whether MCP adapter exposes `wait` as a tool with a fixed short timeout (#17).
- Token storage and rotation, LAN-only enforcement and rate limits (#12).
- Per-agent peer rules: may every agent send to every other agent? Default yes.
