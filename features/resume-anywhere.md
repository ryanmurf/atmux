# A4: Resume anywhere, phone-home restore, no startup prompts

Status: implementation in progress on branch `feat/resume-anywhere`

Read `features/agent-control-plane.md` first. This record is your brief; keep it updated with
progress, evidence, and gate checkboxes.

## Goal

Any session, running or archived, can be resumed on any machine with its conversation intact. A
node that boots (or whose atmux restarts) phones home and gets back the sessions it was running.
Agents never sit at a startup prompt.

## Scope

1. **Startup prompt auto-answer (do this first; it is independent).** After any atmux launch,
   relaunch, resume, restore, or Quick Resume, and for any pane classified at a known startup
   prompt, answer only these exact, recognized dialogs:
   - Claude Code development-channels confirmation ("--dangerously-load-development-channels is
     for local channel development only", option "1. I am using this for local development"):
     select option 1 only when the pane's agent process argv actually contains that flag.
   - Claude Code workspace trust dialog: accept only when the pane cwd is under a configured
     project root.
   - Leave anything else alone and report it as `agent.needs_input/startup_prompt` (A1 defines
     the event; until it lands, log through one call site).
   Match on bounded, exact pane text plus the process check; send the minimal keys; verify the
   dialog disappeared; never answer twice. Config: `[startup_prompts] auto_answer = true`.
   Also write a short note in this record on removing the dialog at its source: Claude Code says to
   use `--channels` with an approved channel; record what that would require for the herodevs
   channel without changing anyone's wrappers.
2. **Bundle import with path translation.** Given an archive bundle (A3's format; coordinate via
   the shared types once A3's branch exists, or define the manifest here and let the lead
   reconcile), restore the native log on a target owner:
   - Find the project on the target by git remote (search configured project roots, bounded); if
     absent, clone it into a project root using the existing credential-free clone path
     (`/api/v1/launch-directories/clone`) and check out the recorded branch; never overwrite.
   - Map the config store by profile name (for example `hd` to that machine's `CLAUDE_CONFIG_DIR`
     for profile `hd`; Codex `CODEX_HOME`). Refuse if the target has no matching profile.
   - Claude: write `projects/<encoded new cwd>/<id>.jsonl` (+ sibling directory), rewriting only
     `cwd` fields (and equivalent path fields you verify) from the old project root to the new
     one, line by line, bounded. Codex: place the rollout under
     `sessions/YYYY/MM/DD/` with `session_meta.cwd` rewritten. Never overwrite an existing native
     log with different content; same content is a no-op.
   - Launch through the existing resume path (`launch_resumed` / `resume_claude` in
     `src/tmux.rs`) with the recorded profile and mode, and set `@atmux_session_key` to the
     original key on the new pane before its first scan.
   - Linux and macOS homes differ (`/home/ryan` versus `/Users/ryan`): test that translation.
3. **Resume on any machine.** Coordinator API and MCP `session_resume {session_key, machine}`:
   fetch the bundle (central store, or pull from the source owner if the session is still
   running: then copy its current log and leave the source running unless `move: true`, which
   stops the source after the target is verified), push to the target owner
   (`POST /api/v1/registry/import`, bounded body, mTLS + token), and launch. UI: a "Resume on…"
   action with a machine picker in A3's Sessions view and in the agent actions menu.
4. **Phone home.** When the coordinator sees an owner (re)connect with a new boot id or after an
   atmux restart (A1's `node.started`, or federation health transitions until it lands), it
   compares that machine's last known running sessions with what is running now and asks the
   owner to restore the missing ones (`POST /api/v1/registry/restore {session_keys}`), which
   relaunch with resume locally. Config: `[registry] restore_on_start = true` on the coordinator
   and an allowlist of machines. Never restore a session the user closed or archived on purpose;
   restore only sessions that vanished because the node or tmux server went away.

## Acceptance

- Unit tests: dialog recognition (exact fixtures for both dialogs plus near-misses that must not
  match), process-flag check, idempotence; path translation for Claude and Codex logs, including
  Linux-to-macOS; refusal cases (missing profile, conflicting existing log, oversized bundle,
  symlinks); restore selection logic (closed-on-purpose sessions excluded).
- Disposable-tmux integration test: a fake CLI that prints the dev-channels dialog and waits for
  "1" plus Enter is answered automatically exactly once.
- Federation test: export from a fixture owner, import into another fixture owner with a
  different home, and launch a recorder "claude" that receives `--resume <id>` in the translated
  cwd.
- `cargo fmt`, zero-warning clippy, `cargo test --all-features`, `node --check web/app.js`,
  `node --test web/*.test.mjs tests/navigation.test.mjs`, and the browser suites if the UI
  changes.

## Engineering decisions and merge contract

- Startup handling runs in both owner scan paths (web/MCP and TUI), covering launches,
  relaunches, Quick Resume, and newly discovered panes. It is off by default. A per-dialog
  tmux option claims the exact native PID/start-time generation before sending any keys,
  under the existing owner process lock. This trades a missed answer on a crash for never
  answering twice. Failed verification leaves the claim intact and reports input needed.
- Recognition uses bounded native rows and a terminal confirmation footer. The development
  flag must be a complete active argv token read from the live process; launch-command labels
  are insufficient. Workspace trust uses canonical configured project roots. The scanner's
  cwd can briefly lag a just-started CLI, so trust checks use freshly queried pane metadata.
- A1 integration: `src/startup_prompts.rs::report` is the single body-free log call site for
  `agent.needs_input/startup_prompt`; replace it with the event emitter after integration.

### Removing the development-channel prompt at its source

The [official Claude channels docs](https://code.claude.com/docs/en/channels#research-preview)
and [reference](https://code.claude.com/docs/en/channels-reference) describe approved plugin
channels and organization-managed `allowedChannelPlugins` (with `channelsEnabled = true`).
For herodevs, package the MCP channel as a plugin in its marketplace, have the organization
admin approve that exact plugin/marketplace pair (or obtain Anthropic approval), and launch
with `--channels plugin:<name>@<marketplace>` instead of the development flag. A bare
`server:herodevs` development channel needs that plugin packaging first. No wrappers or
managed settings are changed by A4.

## Gates and evidence

- [x] Startup dialog fixtures, near misses, exact argv check, and disposable-tmux idempotence
- [ ] Native bundle translation, conflict/size/profile/symlink refusal
- [ ] Coordinator API, MCP, and UI resume on another machine
- [ ] Durable phone-home selection and restore
- [ ] Federation export/import/translated recorder launch
- [ ] Required Rust, JavaScript, and browser gates

Startup evidence: `cargo test --lib startup_` passed 6 tests (including the disposable fake
CLI); `cargo clippy --all-targets --all-features` reported zero warnings. Lead scope update:
Codex option `1. Trust and continue` is recognized under the same root/claim/verification
policy, with its own exact fixture and near-miss tests.
