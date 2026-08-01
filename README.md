# atmux

`atmux` is a tmux control plane for terminal coding agents. It puts every session in one live view, makes waiting agents obvious, and launches Codex or Claude into the right project with a few keystrokes.

```text
 atmux  tmux agent control plane     ● 4 working   ◆ 3 waiting
┌ Sessions (8) ─────────────┐ ┌ chptr-2 ──────────────────────────────┐
│ ◆ chptr-2                 │ │ ◆ waiting  Claude                     │
│ ◆ mercury                 │ │ path  ~/IdeaProjects/chptr-2          │
│ ● release-review          │ └───────────────────────────────────────┘
│ ● atmux                   │ ┌ Live pane ────────────────────────────┐
│ ○ database                │ │ All checks passed.                    │
│                           │ │                                       │
│                           │ │ ❯                                     │
└───────────────────────────┘ └───────────────────────────────────────┘
 e quick edit   enter switch   n new   / filter   x kill   ? help   q quit
```

## What it does

- Shows all tmux sessions in a compact left rail that grows to fit, stops at 25%, and ellipsizes long names.
- Marks detected Codex and Claude agents as `● working` or `◆ waiting`.
- Shows a live preview of the selected session's active pane.
- Opens a selected session for a quick edit in a popup without leaving the control plane.
- Switches the current tmux client instantly, or attaches when run outside tmux.
- Launches an agent through a folder → harness → profile → session-name wizard.
- Finds projects beneath configured roots, Codex layered profiles, and local `claude-*` wrapper profiles.
- Supports custom commands, arguments, environment variables, state markers, and exact tmux status overrides.

`atmux` is intentionally local and direct: it talks to the tmux server you already run and starts the same agent CLIs you use by hand.

## Install

atmux supports Linux and macOS and tests both in CI. You need Rust 1.88+ and tmux.

On macOS, install tmux first if needed:

```bash
brew install tmux
```

Then install atmux on either platform:

```bash
cargo install --git https://github.com/ryanmurf/atmux
atmux doctor
atmux
```

For a fast tmux popup, add this to `~/.tmux.conf`:

```tmux
bind-key A display-popup -E -w 92% -h 90% "atmux"
```

Then reload tmux and press your prefix followed by `A`.

## Keyboard

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Select a session |
| `e` | Quick edit in a popup; press your tmux prefix then `d` to return |
| `Enter` / `s` | Switch or attach; press your tmux prefix then `L` to return |
| `n` | Launch a new agent |
| `/` | Filter sessions |
| `Page Up` / `Page Down` | Scroll pane preview |
| `r` | Refresh now |
| `x` | Kill a session, with confirmation |
| `?` | Help |
| `q` | Quit |

The launch wizard accepts text to filter project folders. `Enter` advances and `Esc` goes back.

Quick edit creates a temporary nested tmux client inside the popup. Detaching closes only that
client and reveals atmux again; it does not detach your outer terminal. A full switch uses tmux's
standard last-session shortcut to return.

## Configuration

The first run creates `~/.config/atmux/config.toml`. Print the exact path with:

```bash
atmux config-path
```

The default configuration is small:

```toml
[general]
project_roots = ["~/IdeaProjects", "~/work"]
favorite_dirs = ["~/Documents/notes"]
refresh_ms = 750
preview_lines = 160
switch_on_launch = true

[[profiles]]
name = "Default"
harness = "codex"
command = "codex"
args = []

[[profiles]]
name = "Review"
harness = "codex"
command = "codex"
args = ["--profile", "review"]

[[profiles]]
name = "Work account"
harness = "claude"
command = "claude-hd"
args = []

[status]
working_markers = []
waiting_markers = []
```

Each project root and its immediate child directories appear in the folder picker. Favorites are placed in the same searchable list. Profiles sharing a `harness` value are grouped together in the wizard.

On startup, atmux also discovers:

- `~/.codex/<name>.config.toml` as Codex profile `<name>`.
- Executable `claude-*` wrappers in `~/.local/bin` and `~/bin` as Claude profiles.

## Agent-state detection

State detection combines the active pane's process tree, terminal title, recent output, and output changes. It recognizes the normal Codex and Claude busy indicators, prompts, and approval questions without requiring hooks.

For exact integration from a hook or wrapper, set a pane-scoped tmux option:

```bash
tmux set-option -pt "$TMUX_PANE" @atmux_status working
tmux set-option -pt "$TMUX_PANE" @atmux_status waiting
tmux set-option -pt "$TMUX_PANE" -u @atmux_status
```

Accepted values are `working`, `waiting`, `idle`, `ready`, `other`, and `off`. An explicit override wins over automatic detection.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Contributions are welcome. Please keep tmux mutations explicit and preserve the zero-hook default experience.

## License

MIT
