# Claude/Codex transcripts, tools, and compact UI

Status: completed

## Acceptance criteria

- [x] Old or manually launched Claude panes resolve conventional `~/.claude*` profile storage only
  when PID, cwd, start time, session ID, and exactly-one-root checks agree.
- [x] Claude `/clear` and Codex `/new` never retain the prior conversation.
- [x] Chat is rendered as native DOM Markdown; tool calls/results are compact, collapsed,
  expandable, bounded, and text-only.
- [x] Markdown links are safely constrained and fenced code blocks are highlighted and expandable.
- [x] Common secret-bearing strings are redacted, with the documented limitation that arbitrary
  output cannot be guaranteed secret-free.
- [x] Desktop and mobile layouts remain usable and visibly denser than the original agent cards.

## Gates

- [x] Implementation
- [x] Focused unit/browser tests, including adversarial transcript and browser reducer/DOM tests
- [x] Full post-hardening integration suite after aggregate tool-output and derived-ID allocation
  fixes: 152 Rust tests passed on Linux, Midnight, and Max; 45 browser tests passed locally and on
  Midnight (Max does not have Node installed)
- [x] Final Fable/Claude Max review: SAFE, no blockers
- [x] Final independent security review: SAFE, no blockers

Live evidence: Midnight's existing `ibm` pane resolved from `~/.claude-max` and produced 48 message
cards plus 176 collapsed tool rows without restarting tmux.
