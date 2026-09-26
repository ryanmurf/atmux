# Rename and describe sessions from the left rail

Status: implemented and live-tested on Linux; macOS runtime, reviews, and deployment pending

## Request

Rename a tmux session from the left navigation, and give it a short description.

## Acceptance criteria

- [x] Each controllable rail row has a ✎ action beside pin and delete that opens an edit dialog
  prefilled with the session's current name and description.
- [x] Saving renames the real tmux session (`rename-session`) on the owning machine, local or
  federated, and the rail, agent header, and search reflect it on the next refresh.
- [x] New names follow the launch rules (1–100 letters, numbers, `-`, `_`; `atmux-web` reserved;
  names beginning with `-` are passed after `--`). Duplicates are refused with a clear message.
  A session created outside atmux with an unusual name can still get a description without
  being renamed.
- [x] A description is one trimmed line of at most 120 characters; an empty one clears it. It
  is stored base64url-encoded in the session's `@atmux_description` tmux option, so it lives and
  dies with the session and can never inject a separator into the pane record.
- [x] The description appears under the name in the rail and under the title in the desktop
  agent header, is included in rail search, and is part of each row's accessible label.
- [x] Edits are bound to the pane generation (`instance_id`) seen when the dialog opened; a
  replaced pane is refused before tmux or federation is reached.
- [x] A rename and description change are one tmux command list, so a refused rename cannot
  half-apply the description change sent with it.
- [x] Killing a session now targets the pane's own session rather than a possibly stale cached
  name, so a rename followed quickly by a kill cannot hit a different session.
- [x] An older owner without the route answers 405, reported as "update atmux on <machine> before
  renaming its sessions". Older coordinators ignore the additive `description` field.
- [x] Pins, drafts, and agent links are keyed by pane identity, so they survive a rename.

## Gates

- [x] Implementation
- [x] Focused unit/source-contract tests (Rust codec/parser, route validation and generation
  binding, federation forwarding and legacy-owner failure; web request builder and wiring)
- [x] Linux integration: isolated-server tmux test (rename, dash-leading name, refused duplicate
  leaves the description, clear, invalid stored value, kill by pane); full serialized
  all-features suite; seven-test Chromium browser suite with a 44 px ✎ touch target
- [x] Linux live runtime: `atmux web` on a disposable tmux socket and loopback port; rename,
  describe, duplicate, invalid, clear, and cross-origin requests over HTTP; the headless-Chrome
  dialog flow including search by description
- [ ] macOS runtime test on Midnight (tmux option and `rename-session --` behavior)
- [ ] Fable/Claude Max review
- [ ] Independent security review
- [ ] Deploy coordinator and owners; verify federated rename end to end

## Boundaries

Midnight's tmux server and socket stay protected; its web service restarts only through the Aqua
LaunchAgent. Tron's Quick Resume script recreates sessions under their original names, so a
rename does not survive a Tron host restart.
