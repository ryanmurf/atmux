# Midnight Operations

The default tmux server on Midnight is the protected Aqua/Keychain-capable
server. Preserve it and its existing socket at all times: never kill or rebuild
that server, and never create a replacement tmux socket.

- Never launch `atmux web` directly from an SSH shell.
- Builds may run on Midnight from `/Users/ryan/IdeaProjects/atmux` when needed.
- Restart the web service only through the Aqua login session:
  `launchctl kickstart -k gui/$(id -u)/dev.herodevs.atmux-web`
- Keep `dev.herodevs.atmux-web` configured to start in
  `/Users/ryan/IdeaProjects/atmux`.
- For Claude launches, use profile `max` or the credential-bound `Default`
  profile. Both select `CLAUDE_CONFIG_DIR=/Users/ryan/.claude-max`; do not use
  the unauthenticated bare `~/.claude` configuration.
