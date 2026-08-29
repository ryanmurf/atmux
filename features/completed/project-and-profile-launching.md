# Project and profile launching

Status: completed

- [x] One typed project field replaces the separate finder, filters known projects, and accepts
  validated manual absolute paths.
- [x] Git repos, `.atmux.toml`, and Claude/Codex agent-instruction markers define projects; non-repo
  grouping directories are traversed.
- [x] Project selection proposes the session name; `.atmux.toml` remembers session/profile choices
  without following symlinks.
- [x] A top-level agent-family picker exposes Claude and Codex before launch.
- [x] Claude and Codex launch profiles are discovered per machine, profile-specific
  `CLAUDE_CONFIG_DIR` is applied, and the launch command is shown.
- [x] The web UI receives the host's project, instruction-file, agent-family, and profile discovery.
- [x] Unit, tmux integration, Midnight/Max runtime tests, and two security reviews passed.
