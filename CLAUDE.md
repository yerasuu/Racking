## Build and test

Cargo workspace: `crates/racking-server` (lib + binary), `crates/rack` (HTTP client lib + `rack`
binary) and `crates/racking-mcp` (MCP stdio server on top of `rack::client`).
`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt`
(`max_width = 120`) before each commit. Tests run the server in-process on `127.0.0.1:0`; `rack`
tests use a fake `kb` on PATH, never a real board; `racking-mcp` tests spawn the real binary over
pipes. In `racking-mcp`, stdout is the protocol channel: log with `tracing` (stderr), never print. Never commit a real config: only `config.example.toml` with placeholders.

<!-- kanbrain:start -->
## Project memory: kanbrain

This project's tasks, to-dos, decisions and notes live in `.brain/`, managed only with `kb`, kanbrain's CLI.
The user follows them live on a kanban board: keep it showing what you're doing as you go, not at the end.

- Start of session: `kb context`, unless its output is already above (a session hook adds it).
- Work that changes the repository, or that the user should see, gets a task; questions and lookups don't. Find it
  (`search WORDS`) or create it (`add TITLE`), then `start ID` (`add --start TITLE` does both). "Todos", a plan or
  what's pending, in any language, are tasks here, one `add` each; your built-in to-do list isn't on the board, so
  keep it for steps within the task you've started. A task with real work gets a body, `add --body - TITLE`:
  **Goal** (what and why, a line or two) and **Done when** (what the user will check).
- While working: `comment ID TEXT` when a step is done, when you find a cause, and before you stop (one or two
  lines: what changed, what's next); `done ID` as soon as one is finished. Stopping before it is: `comment`, then
  `park ID`. Doing isn't limited for you: several of your tasks can run at once. Never park, finish or move a task
  you didn't start (the user's, or another session's). A pull request, issue, CI run, commit or page named in a
  comment or body goes with its markdown link, `[PR #39](URL)`, not just `PR #39` or the bare URL: the user opens it
  from the board. An entry is named plainly, `#12`, or `REPO#12` in another repository of the project, several apart
  (`#1, #2 and web#3`), never joined by `/`: the board makes each a link.
- Before ending a turn in which you changed the repository: that change is on a task you started (create it now if
  you skipped it), the turn's work is commented there, and a finished task is `done`. Work that goes through a pull
  request is finished once every check has passed and it's merged; until then its task stays in Doing. Nothing is
  `done` without its evidence commented first: the tests or checks run and their result, or what shows it works (a
  CI run's link, the output, a screenshot). What you couldn't check isn't done: `ask ID TEXT` the user to check it,
  saying what's left and how.
- Splitting work between subagents: give each part that changes the repository its own task, and run the board
  yourself: `add --start` it before launching the subagent, then `comment` its result and `done` it. Subagents
  only do the work; lookups and research get no task. `kb` commands can run in parallel.
- Blocked on the user (a requirement, access, a choice that's theirs): `ask ID TEXT` parks the task with the
  question; don't guess. A choice you shouldn't make alone and weren't asked to (architecture, a dependency,
  storage, a public interface): `propose ID --body - TITLE`, Context / Options / Recommendation on stdin; it parks
  the task until the user accepts or rejects it. Then, unless the user said they're away, if your tool can run a
  command in the background and wake you when it exits, run `kb wait ID` (the task) that way and end your turn: it
  prints the answer, or that there's none yet (then stop, don't wait again). Otherwise go on with other work or
  stop; answers come first in `context`, under Answered: `show ID`, then pick the task up. If the user answers in
  the chat instead, `start` the task (that ends the wait). `accept`/`reject` a decision only when the user tells
  you to.
- Choosing between approaches within your task: `search` for earlier decisions (only accepted ones are in force),
  then `add --decision --link ID --body - TITLE` with Context / Decision / Consequences on stdin. Non-obvious
  findings: `add --note --link ID --body - TITLE`; a problem someone has to fix is a task instead, in the
  repository that has to change.
- Always base what you do and write on official documentation: that of the language, library, tool or API, at the
  version this repository pins (its manifest or lockfile), never on memory or posts elsewhere. Whatever you write
  that rests on it (a description, decision, note or comment) links the page it rests on with a markdown link,
  `[serde: field attributes](URL)`, not a bare URL, so the user opens it from the board and checks where it comes
  from. With no official documentation for it, say so.
- `## Project` in `context` is the application's shared context. If it isn't described, or you're asked to set up
  or load the brain, describe it first, from the README and manifests: `project describe -` (what the app is, its
  services and how they connect, conventions they share: the big picture, in at most five lines, without tables,
  columns, function signatures, files or dependencies, which the code already says). Rewrite it when the big
  picture changes (a service added, another storage), not after every change. With several repositories, this
  one's role too: `project notes -`. How to build, test and run this repository, and its code style, go in this
  file, outside this section: add them as you learn them.
- Also: `show ID` (one entry in full), `describe ID TEXT`, `rename ID TITLE`, `tag ID +TAG -TAG` (add or remove
  tags), `link ID OTHER…` (an id here, or `REPO#ID` in another repository of the project); `-` reads text from
  stdin. Never edit the files in `.brain/` by hand: `kb` keeps them consistent.
<!-- kanbrain:end -->
