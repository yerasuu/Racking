# rack policy

## Purpose

A worker's local policy (decisions #44 and #48, task #49): what this machine's agent may run on its own for a task that carries the owner's delegated authority, and nothing else. A worker never runs anything because a message says so; it works inside a **profile** of this file only after `rack policy decide` allows the task.

The policy is one TOML file per machine, written and edited only by that machine's owner: `~/.config/rack/policy.toml` (`%APPDATA%\rack\policy.toml` on Windows), mode 600, or the file the owner names in `RACK_POLICY`. `rack` never reads a policy from a work directory, a job or a message, and refuses one that lies inside the workdir. `docs/policy-example.toml` is the commented template `init` writes: `[worker]` limits and four starter profiles, `workdir` (enabled: read, write and list inside the workdir, no network), `app-test`, `compat-launch` (an app under Wine or Proton, to be tuned with real runs) and `diagnostics` (logs and fixed metric commands), the last three disabled.

A task asks for a profile with the **first line of its body**: `Racking-Profile: NAME` (`[a-z0-9-]`, 1 to 32). The server has no field for it yet (a server-side field is a later change), so the line is read only when the job's `from_role`, set by the server, is `main`; from a worker or a server without roles it is data and never starts a profile.

Patterns in the file are anchored wildcards: `*` any run of characters, `?` one character, everything else literal, matched against the whole text (no regex, no prefix match). `args = ["--ok", "--n=*"]` allows exactly two arguments, the first `--ok`, the second starting with `--n=`; a wildcard never matches a `..` component the pattern does not spell out. In `read` and `write` paths a wildcard stays inside one directory level (`/var/log/*.log`), and a plain path covers everything under it. `{workdir}` and the keys of a profile's `vars` are replaced first. Paths are resolved before every check: absolute only, no `..`, symlinks followed (a link inside the workdir that points outside it is outside), compared without case on Windows.

Subcommands:

- `init` writes the template with its comments, the workdir filled in, mode 600, and creates the workdir. It never overwrites an existing policy.
- `check` validates the file: TOML and schema (`version = 1`, no unknown key), patterns and placeholders, absolute paths, an existing workdir, mode 600 on Unix, no command a profile may also write, no write path that reaches the policy or the config directory, no `env_allow` entry that changes which code runs (`LD_*`, `PATH`, `RACK_*`...), and `accept_from` holding only `main`. Every problem is listed.
- `show` prints the effective policy: limits, each profile with its commands, paths, network, variables, timeout and returns, and the active profile.
- `decide JOB` reads the job with the normal API (`GET /whoami`, `GET /jobs/JOB`, the accepted tasks of the inbox) and prints `allow profile=NAME reason=...` or `deny reason=...`, running nothing. It denies when the policy cannot be read or does not validate (fail closed), the server is paused, the job was not sent to this agent, its `from_role` is not in `accept_from`, it is not a `task`, it is closed, its `hops` are above `max_hops`, it names no profile or an unknown or disabled one, another task's profile is still active, or `max_open_jobs` tasks are already accepted.
- `decide JOB --accept` also accepts the task and records the profile as active in `~/.config/rack/policy-active.json` (mode 600) for `max_runtime_secs`. Only this command writes that file.
- `release` ends the active profile when the task is finished (send the result first).
- `hook` is a Claude Code `PreToolUse` command hook for the worker's session: it reads the hook's JSON on stdin and prints a decision (`permissionDecision` `allow`, `deny` or `ask`) as the [hooks reference](https://code.claude.com/docs/en/hooks.md#pretooluse-decision-control) defines it, exit 0. Bash commands are read as a plain argv (any `;`, `|`, `&`, redirection, `$`, backquote, glob or `~` outside single quotes is denied, since commands never go through a shell) and must match an `allow_run` rule of the active profile exactly, from a working directory inside the workdir; leading `NAME=value` words only for names in `env_allow`; the profile's `timeout_secs` caps the call's `timeout` (`updatedInput`), and background runs are denied. `Read`, `Glob` and `Grep` must stay in `read` or `write` paths, `Edit`, `Write` and `NotebookEdit` in `write` paths, `WebFetch` on a `network` host. `rack` itself (by name or this binary's path) may run the worker's protocol (`whoami`, `inbox`, `wait`, `show`, `accept`, `reject`, `result`, `attach`, `get-attachment`, `say`, `notify`, `report`, `links`, `guide`, and `policy decide`, `show`, `check`, `release`) even with no profile, but files it would send or save must be inside the active profile, and `--state` is refused. `AskUserQuestion`, `ExitPlanMode` and `TodoWrite` are left to Claude Code. Anything else is denied; with no active profile, `[worker] no_profile = "ask"` asks the owner instead. Unreadable input, an unusable policy or a damaged active file deny.
- `settings` prints a `--settings` JSON for the worker's Claude Code session: the hook on every tool (`"matcher": "*"`, run as this binary with `args` `["policy", "hook"]`, no shell), `permissions.allow` for the exact commands of enabled profiles, `permissions.deny` for `sudo`, `WebSearch` and, when no enabled profile has a network host, `WebFetch`, and `"crossSessionInbound": "accept"`. Keys from the [settings reference](https://code.claude.com/docs/en/settings-reference.md) (`hooks`, `permissions`, `crossSessionInbound`) and [permissions](https://code.claude.com/docs/en/permissions.md). Use it only in the worker's own `claude --settings FILE`, never in user settings.

Every decision of `decide`, `release` and `hook` is appended to `~/.config/rack/policy-audit.log`: one JSON line with time, source, decision, job, profile, sender, `from_role`, tool and reason, never a body, command line or token; mode 600, rotated at 1 MiB keeping three old files. An allow that cannot be logged becomes a deny.

On Windows the template's commands and log paths are Unix paths: replace them with absolute Windows paths (`C:\...`) before `check` passes, write such paths in single quotes in a Bash command (a backslash outside quotes is shell syntax and denied), and protect the policy file with its ACL, which `check` does not inspect.

What the hook cannot see: what an allowed program does once it runs (its own file and network access), output sizes (`max_output_bytes` is the most a result should carry back), and anything outside Claude Code. Keep profiles minimal; see SECURITY.md.

## Options

- `--workdir DIR` (`init` only) the workdir to write into the template, an absolute path. Default `~/rack-work` (`%USERPROFILE%\rack-work`).
- `--accept` (`decide` only) when allowed, accept the task and activate its profile.

Environment: `RACK_POLICY=FILE` reads another policy file; set it only as the owner, never from a job. The active profile and audit log stay in the config directory.

## Examples

```sh
rack policy init --workdir /home/me/rack-work
rack policy check
rack policy show
rack policy decide j42                 # allow profile=app-test reason=... | deny reason=...
rack policy decide j42 --accept        # then work inside the profile
rack result j42 --body "tests pass" --attach /home/me/rack-work/logs/run.log
rack policy release
rack policy settings > ~/.config/rack/worker-settings.json
claude --settings ~/.config/rack/worker-settings.json
```

A task from main that asks for a profile:

```sh
rack send win1 "Run the self-test" --body "Racking-Profile: app-test
Run the self-test and send back the output."
```

## Exit codes

- 0 success; `decide`: allowed; `hook`: always, with its decision on stdout
- 1 `decide`: denied (or the server could not be read); `check`, `show`, `settings`: the policy has problems; `init`: the file exists; local error
- 2 usage error

## MCP

None: there is no MCP tool for it. The policy is the owner's, and the hook runs inside Claude Code; an agent on MCP still runs `rack policy decide` through Bash.
