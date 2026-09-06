# Profile launch reliability under service environments

Status: completed

## Acceptance criteria

- Configured default Claude and Codex profiles resolve to the discovered absolute executable while
  preserving their arguments and environment.
- Discovery deterministically prefers executable wrappers, then shell aliases, then sorted Codex
  config files, and finally generic defaults for same-named profiles.
- A configured profile opts into discovered command details only with `inherit_discovered = true`;
  its configured environment, modes, and explicit Claude relaunch policy remain authoritative.
- Launching does not depend on the reduced `PATH` inherited by a launchd/tmux web service.
- Max can launch its reported default Codex profile from the web API.
- The existing Claude profile/config-directory behavior remains unchanged on Midnight.

## Gates

- [x] Implementation
- [x] Focused unit test
- [x] Full local/native regression suites
- [x] Max live launch test
- [x] Fable/Claude Max review
- [x] Independent security review

## Verification evidence

- All 165 local Rust tests, strict Clippy, formatting, Rust 1.88 locked compatibility, and the
  native-focused Mac test passes are green.
- Max launched its reported default Codex profile through the web API using the discovered absolute
  `/home/ryan/.local/bin/codex` path under the service's reduced `PATH`.
- Midnight launched the Max Claude profile with both `/Users/ryan/.local/bin/claude` and
  `CLAUDE_CONFIG_DIR=/Users/ryan/.claude-max`; its protected Aqua tmux server was not rebuilt.
- Fable/Claude Max and the independent security reviewer both returned `SAFE` on the final frozen
  snapshot, including the canonical absolute executable resolution and launch-label redaction.
- Full-pipeline discovery regressions cover executable-versus-alias precedence, aliases without a
  wrapper, named Codex config fallbacks, generic defaults, and deterministic case collisions.
