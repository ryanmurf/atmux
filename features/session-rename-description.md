# Rename and describe sessions from the left rail

Status: deployed fleet-wide on 2026-09-26 (runtime `8d5cd01`); independent reviews pending

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
- [x] A coordinator refuses a name the owner already reports with "a tmux session named X already
  exists on <machine>" before forwarding; it never relays an owner's own error text, and the owner
  still enforces the rule against live tmux.
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
- [x] macOS runtime test on Midnight (tmux 3.7b option and `rename-session --` behavior)
- [ ] Fable/Claude Max review
- [ ] Independent security review
- [x] Deploy coordinator and owners; verify federated rename end to end

## Boundaries

Midnight's tmux server and socket stay protected; its web service restarts only through the Aqua
LaunchAgent. Tron's Quick Resume script recreates sessions under their original names, so a
rename does not survive a Tron host restart.

## Deployment — 2026-09-26

Ryan asked for all work to be shipped. Runtime commit `8d5cd01` (`1048562` plus the coordinator
duplicate-name precheck) runs on Tron and Max (x86_64 Linux), Clue (ARM64 Linux), Midnight (ARM64
macOS), and the public coordinator (Helm revision 30). Details and rollback are in
[auto-compact-idle-clock.md](auto-compact-idle-clock.md#deployment--2026-09-26).

Live federated check through the public coordinator against a throwaway plain-shell session on
Midnight's existing tmux server (no socket created):
- A stale instance got 409 and an invalid name got 400. Renaming to the dash-leading
  `-atmux-probe-renamed` with a non-ASCII description got 200. tmux showed the new name, the
  decoded description matched, and the coordinator overview showed both.
- Renaming onto an existing Midnight session name got 409 and left the stored description
  unchanged. On `8d5cd01` the message reads "a tmux session named nes-ecosystem-max already
  exists on midnight".
- A description-only edit and an empty-description clear both applied.
- Kill removed only the probe. Midnight's server PID 1061 and every other pane matched the
  post-deploy snapshot.

