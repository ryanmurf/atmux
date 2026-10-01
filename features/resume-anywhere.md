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
- Codex's current [folder trust renderer](https://github.com/openai/codex/blob/main/codex-rs/tui/src/onboarding/trust_directory.rs)
  uses **Folder access**, an exact wrapped trust disclosure, **1. Trust and continue**, **2. Quit**,
  and `enter continue · esc quit`. The fixture follows its official 40-column onboarding snapshot;
  recognition rejoins only the complete exact disclosure so narrow worktree dialogs work. Restricted
  folder actions, altered disclosures/choices, and changed confirmation footers are near misses.
- A1 integration: `src/startup_prompts.rs::report` is the single body-free log call site for
  `agent.needs_input/startup_prompt`; replace it with the event emitter after integration.
- A3 was absent in this worktree, so `src/resume_anywhere.rs` defines
  `atmux.native-bundle/v1`, `[registry]` resume configuration, a bounded bundle cache, and
  owner desired-state snapshots. The lead should adapt A3's archive record to `NativeBundle`
  and consolidate the store/config. The manifest carries the original session key, harness,
  profile/mode, native id, source project/cwd, credential-free git remote/branch, relative native
  path, and optional source process binding. Native logs and base64 Claude sibling files are
  transported together; no source command, environment, credentials, or target path is accepted.
- Reads and requests are bounded: 8 MiB maximum serialized bundle (configurable downward),
  256 KiB per native JSONL row, 128 sibling files, 512 desired sessions per owner, 512 KiB desired
  snapshot, and 4,096 listed records. Repository lookup checks at most 4,096 entries, depth four,
  and ten seconds; native sibling traversal has its own count/depth/time bounds. All native/cache
  file access opens each path component with `NOFOLLOW`; files must be regular, singly linked,
  and owned by the current user. Native publication uses exclusive staging plus hard links;
  identical content is a no-op. Preflight checks all files before publication. Cache state uses
  atomic rename, an owner-only root, and an advisory transaction lock.
- Only verified metadata is translated: top-level Claude `cwd`, including subagent JSONL, and
  Codex `session_meta.payload.cwd`. Message/tool bodies and binary siblings retain their content.
  Codex keeps its rollout date and filename. Explicit configured profile stores are supported
  without weakening the conventional-home transcript locator.
- Target repository selection requires both the normalized origin remote and recorded branch.
  Ambiguity and an existing repository on a different branch are refused to preserve existing
  workspaces; the user can configure a matching worktree. Only a newly created clone is checked
  out. Cross-machine projects without a git remote are refused because a folder-name guess
  cannot prove identity; same-owner restoration can reuse the configured local project.
- Imports use the existing resume launcher and native-conversation lease. The stable key is set
  on the placeholder pane before respawn or discovery. Target verification requires the expected
  harness, configured profile, cwd, and native conversation metadata within five seconds. The
  startup handler runs during that verification window. Retry of an already verified target is
  idempotent only when its translated native files match exactly; a divergent running copy is
  refused so a move cannot discard newer source turns. Source selection prefers a running owner
  other than the target. A move rechecks the exported pane, PID, kernel process-start stamp, native id,
  and a SHA-256 digest of the source log and siblings
  under the process lock before recording a close and stopping the source.
  Move kills only the exported pane, preserving neighboring panes in that tmux session.
- Web and TUI explicit closes save tombstones for every affected Claude/Codex pane before
  killing the named tmux session, including panes not yet captured by the periodic observer.
  Final native exports are retained when available. This closes the restart-between-close-and-scan
  gap and gives A3 a common `record_named_close` integration point.
- The coordinator pulls owner snapshots and bundles every 15 seconds over the existing mTLS /
  bearer-token federation client. Snapshot boot ids, tmux-server identity changes, and health
  reconnections substitute for A1's future `node.started` signal. Restore requires both
  `restore_on_start` and `restore_machines`; pending desired entries survive failed requests.
  Missing panes on an unchanged boot/server are treated as intentional closes, a fail-closed
  choice where intent cannot otherwise be proven. Closed/archived tombstones win over stale
  coordinator intent and are rechecked inside the restore launch transaction.
- Restore refreshes the owner's native log before import so complete turns newer than the last
  cached snapshot survive. If the native file is gone, the retained bundle is used; unsafe or
  oversized local files remain errors. Export ignores an incomplete final appended JSONL row
  until that row completes, and a closed-session export refreshes the final complete native log.
- A3's Sessions view was absent, so a working **Saved sessions** picker supplies the durable
  running/closed list alongside agent Actions. A3 can dispatch the document event
  `atmux:session-resume` with its selected durable record to open the same machine picker, then
  replace the fallback list/API with its richer archive/search view. Requests capture the
  durable key when the dialog opens, independent of changing live selection; copy is the default.
- A1/A3 own the final event plumbing: connect startup reporting to A1's emitter, use its
  `node.started` as an additional restore signal, and emit A3's `session.resumed` / archive events
  around these successful operations. A4 deliberately keeps the current log and federation
  fallback runnable before those branches land. Shared Rust/web files contain insertion points
  and small wiring additions rather than reorganizing their existing implementations.
- Copy intentionally allows one stable key to be live on multiple owners. The fallback keeps
  desired state per machine and the last pulled bundle per key; live resume prefers a non-target
  owner. A3 should reconcile its primary-location and per-owner bundle/version model for these
  running copies. Divergent native logs are refused rather than merged, and a source that advances
  after export is left running when a move's content digest no longer matches.
- The Quick Talk browser suite previously required an external running atmux and Chrome. Its
  default invocation now starts a private HTTP fixture and disposable Chrome profile, preserving
  explicit external arguments for existing workflows. This makes the required browser gate
  reproducible without contacting a running agent or service.

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
- [x] Native bundle translation, conflict/size/profile/symlink refusal
- [x] Coordinator API, MCP, and UI resume on another machine
- [x] Durable phone-home selection and restore
- [x] Federation export/import/translated recorder launch
- [ ] Required Rust, JavaScript, and browser gates

Startup evidence: `cargo test --lib startup_` passed 6 tests (including the disposable fake
CLI); `cargo clippy --all-targets --all-features` reported zero warnings. Lead scope update:
Codex option `1. Trust and continue` is recognized under the same root/claim/verification
policy, with its own exact fixture and near-miss tests.

Backend completion evidence: `cargo test --lib startup_` passed 7 tests;
`cargo test --lib resume_anywhere` passed 8 tests; `cargo test --lib federation_exports_imports`
passed the authenticated two-owner fixture (native profile/mode/cwd/key verification, copy,
divergent-target refusal, advanced-source refusal, neighboring-pane preservation, direct and
coordinator restore, and explicit-close exclusion). `cargo test --lib registry_mutations`
passed cross-origin and body-limit checks. `cargo clippy --all-targets --all-features -- -D warnings`
reported zero warnings, and `cargo fmt --check` / `git diff --check` passed.
