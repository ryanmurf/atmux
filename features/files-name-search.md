# Files: search by file name

Status: deployed fleet-wide 2026-10-10 (runtime `334e6d5`).

## Why

The Files tab only browsed one folder at a time, so opening a file meant knowing where it lived
and clicking down to it. Agents print paths such as `src/web.rs:1174` constantly, and there was
no way to jump to one.

## What changed

A search box sits above the file list. Typing searches the whole project through the owning
machine and lists matches best first, with the typed letters marked. Arrow keys and Enter pick a
match, Escape returns to the folder, and Ctrl+P or ⌘+P focuses the box while Files is open. A
pasted `path:line` opens the file at that line. The open file isn't redrawn while you type.

The owner answers `GET /api/v1/panes/{id}/code/files?query=…` with the existing code-navigation
walker, so coordinators forward it like the other code routes. Ranking: exact name, same stem,
name prefix, name substring, folder, then letters in order; every word must match and a word with
`/` matches the path. File search lists dotfiles such as `.github/workflows/ci.yml`. It still
skips sensitive names, generated folders, tool caches, and any Git checkout inside a hidden
folder. The checkout rule exists because agent worktrees under `.claude/worktrees` otherwise
repeated every file. Results stop at 50; the walk keeps the existing 20,000-file and 2 s budgets.
On Tron a search of this repo takes about 2 ms end to end, and the whole `IdeaProjects` tree
about 80 ms.

## Gate

fmt and zero-warning clippy; `cargo +1.88 check --locked`; the full Rust suite passed (federation
needs a disposable `ATMUX_TMUX_SOCKET_NAME=atmux-test-…`); 205 JS tests passed. The browser
suites passed, with a new phone walk-through of search, `path:line`, Back, and Escape. `mobile
browser Back stays inside atmux…` still fails on some first runs with "Inspected target navigated
or closed", as it does on the base commit, and passes on rerun. A preview against this repo's
real file list was checked at 1440 px and 390 px.

## Deployment (2026-10-10)

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Tron | `e437fb84e3f2981b` | `target/release/atmux.rollback-4e9dd72` |
| Max | `e437fb84e3f2981b` (Tron's build) | `target/release/atmux.rollback-4e9dd72` |
| Midnight | `872ad02e87944c34` (built on Midnight) | `target/release/atmux.rollback-4e9dd72` |
| Clue | `afae4ab53c64ba1b` (aarch64, built on Clue) | `~/.local/bin/atmux.rollback-4e9dd72` |
| Coordinator | `localhost:32000/atmux:334e6d598322@sha256:5f672567…` | Helm rev 39; rev 38 is `4e9dd72` |

Helm values changed only `server.image`; saved values live in
`/mnt/data/herodevs-agents/atmux-rollout-334e6d5-uh4EW7` on Tron. A file search through the
coordinator returned results from an agent on each of the four machines. Midnight restarted only
through `launchctl kickstart`, and its 14 tmux sessions kept their creation times.
