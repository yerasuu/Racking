# Racking

Racking moves tasks, files and code between coding agents running on different machines of one local network, using each machine's own [kanbrain](https://github.com/singudotdev/kanbrain) board.

The name is the winemaking term for moving wine from one vessel to another. Nothing is shared by the boards themselves: each machine keeps its own `.brain/` and ids, and Racking carries entries and attachments across and records which id is which on the other side.

Status: not started. The design lives in the board (`kb list`).

## Principles

- Integrates with kanbrain only through its public CLI (`kb ... --json`), never through its files.
- Local network only: a small service with an inbox per agent and one token per agent.
- What an agent receives from another agent is data, never an instruction with the owner's authority.
