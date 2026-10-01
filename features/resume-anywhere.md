# A4: Resume anywhere, phone-home restore, no startup prompts

Status: A4 MERGED; all acceptance gates passed on `feat/resume-anywhere` (2026-10-01)

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
   - Lead update: Codex folder trust, option `1. Trust and continue`, under the same canonical
     project-root, exact-dialog, once-per-process, and verify-cleared policy (including worktrees).
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

## Engineering decisions and integration contract

- A3's `src/registry.rs` is the only record, store, configuration and archive owner. A4's
  parallel `ResumeStore`, desired snapshots, `RegistryResumeConfig`, JSON/base64 manifest and
  Saved sessions view are removed. Resume derives translation inputs from A3's private
  `StoredRecord` and `atmux.session.archive/v1` manifest. The public SessionRecord remains
  unchanged and does not expose native ids/config roots. Temporary imports live in the registry's
  existing staging area; transfer, decompression and native translation stream through bounded
  descriptors instead of whole-conversation JSON buffers.
- A3 private records add owner boot/server identity, desired-running intent, close reason,
  process-start stamp and resume generation. These distinguish node loss from intentional close
  without a second store. A monotonic resume generation prevents a still-running source copy's
  later heartbeat from stealing the primary Sessions location back from the target. Each owner
  retains its own source record and archive; divergent conversations are refused, never merged.
- Peer import/export/restore/stop use A3's node-token guard, reject browser Origin/Sec-Fetch,
  and retain existing mTLS transport. The browser/MCP submits only stable key, configured machine
  and optional move. Archive entry types, relative paths, compressed/decompressed caps and
  per-file checksums are verified before native publication. Native paths are opened component
  by component with NOFOLLOW, same-user regular single-link files, exclusive staging and
  no-overwrite hard links. Identical translated content is a no-op.
- Limits follow A3's configured bundle maximum/quota (default 256 MiB/4 GiB), with 256 KiB
  JSONL rows, 4 MiB archive manifests, 4,096 entries and 512 restore keys. Repository search
  remains bounded to 4,096 entries/depth four/ten seconds. Only verified Claude top-level cwd
  (including subagent JSONL) and Codex session_meta.payload.cwd change; message/tool bodies,
  binary siblings and Codex rollout date/filename remain intact. Empty Claude sibling directories
  are preserved. An incomplete final live JSONL row is deferred until it completes.
- Target profile must match name and harness, using its configured native store. Repository
  identity requires origin and recorded branch. Ambiguous repositories or an existing checkout
  on a different branch are refused to preserve workspaces; only new clones are checked out.
  The existing credential-free git URL policy is reused. Projects without a remote can be
  restored on their owner (A3 records without a git root use the configured cwd as the local
  project anchor), but cannot be guessed on another machine. New clones use the existing
  launch_directory::clone_repository implementation and its ten-minute clone deadline.
- Import uses the existing native resume lease and launcher, setting the original session key
  before discovery. Launch verification checks harness/profile/cwd/native identity within five
  seconds. The transaction lock lives inside the blocking worker so request cancellation cannot
  release it during launch. Copy is the default. Move rechecks pane identity, PID/kernel start
  stamp, native id and log/sibling content digest under the process lock, and kills only the
  exported pane after target verification. Advanced sources stay running.
- Explicit web closes write authoritative user tombstones before tmux mutation. TUI closes
  atomically queue generation-bound intents in the same registry's private close-intents mailbox;
  the single registry writer consumes these before observing panes, including on restart. This
  avoids opening a second writer and closes the restart-before-scan gap. Restore commit
  rechecks desired intent atomically; an exact-generation TUI intent also wins before commit.
  Unmatched intents are retained briefly while a new launch is awaiting its first registry scan.
  No mailbox contains conversation content. Native files are refreshed before restore to retain
  post-archive turns.
- A1 EventService records unknown startup dialogs as agent.needs_input/startup_prompt and
  auto-answers as agent.startup_prompt_answered with dialog/verification/process-generation
  metadata only. The TUI's existing per-process tmux claim and bounded outcome marker let the
  owner emit a TUI answer as well.
  session.resumed goes through ControlPlane::emit_agent_event. A1 node.started carries A3's
  owner boot id; tmux server changes emit another node.started. The existing durable event
  checkpoint retains latest imported node boots. An allowlisted coordinator retries missing
  desired node-loss entries every 15 seconds using that signal and a filtered A3 owner feed.
  Reading authoritative owner records also recovers non-primary source copies without creating
  a per-owner manifest/store on the coordinator. Each poll requests at most 32 pages/512 keys.
  Live checkpoints use A3 archives (one eligible checkpoint per scan, 30-second interval per key)
  so its existing streaming pull retains running conversation snapshots centrally.
- Resume on… is integrated into A3's Sessions rows for both live and archived sessions, and
  remains in agent Actions. The picker captures the stable key independently of live selection.
  A lightweight history query detects registry availability; no fallback listing/API remains.
- Startup recognition is bounded and exact; the live process owns the pane generation and cwd.
  Development-channel acceptance requires the exact active argv flag. Claude/Codex trust requires
  canonical configured project roots, including new worktrees. Claim before minimal keys; never
  answer the same dialog twice per process, and verify it cleared. Try-locking skips busy panes
  until a later scan to avoid lock inversion. Codex's exact Folder access disclosure, option 1,
  Quit option and confirmation footer are covered by fixtures and near misses.

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

Initial A4 implementation commits: `b5ee2ba`, `5c27e31`, `88140ba`, `66c5d0c`.
Those commits passed 823 Rust tests, 186 JS tests and 11 browser tests before integration.
The original worktree is mode 775, so the full Rust suite uses an identical private source
snapshot and shared Cargo target directory; existing recovery security fixtures require this.
The user's untracked .atmux.toml and original permissions are preserved. Self-update fixtures
use four test threads to avoid the previously observed ETXTBSY flake.

Integration commits:

- `c5dfabd`: merge A1/A2/A3 at `13fe19a`; consolidate native resume onto A3's registry,
  archive transport, configuration and Sessions view.
- `a46bb70`: merge lifecycle/search/fleet wiring at `a585502`; complete A1 event-driven
  recovery, atomic close-intent handling and non-primary source-copy restore.
- `a54725d`: merge the latest lead head, `a4d071d`, including A5 intake; all final gates
  below cover this combined tree.

Acceptance fixtures cover exact Claude development/workspace-trust and Codex folder-trust
dialogs, near misses, live flag/canonical-root checks and once-per-process verification. A
disposable fake CLI verifies the development dialog is answered exactly once; event fixtures
verify body-free startup outcomes and durable process-generation replay de-duplication.
Claude and Codex native translation covers Linux/macOS homes, sibling files/empty directories,
unchanged bodies, no-op identical imports, damaged/oversized archives and profile/branch/symlink
refusals. Authenticated disposable two-owner federation verifies archive export/import, recorder
launch arguments/cwd/stable identity, default copy, changed-source/divergent-target move refusal,
neighboring-pane preservation, native-tail restore, non-primary source-copy recovery,
event-triggered coordinator restore and explicit-close exclusion (including an intent arriving
at resume commit). Browser fixtures exercise both agent Actions and the A3 Sessions picker.

- [x] Consolidate onto A3 registry/config/archive/peer API/Sessions view
- [x] A1 startup input/auto-answer/resumed/node-started wiring
- [x] Merge lead's latest lifecycle/search/fleet wiring (`a585502`)
- [x] Merge subsequent latest lead integration (`a4d071d`, including A5 intake)
- [x] Final cargo fmt, strict clippy, full all-features Rust suite
- [x] Final JS and full browser suites

Final commands (all invoked through `rtk`):

| Command | Result |
| --- | --- |
| `cargo fmt --check` | Passed |
| `cargo clippy --all-targets --all-features -- -D warnings` | Passed, zero warnings |
| `cargo test --all-features -- --test-threads=4` | 913 passed, zero failed; 7 pre-existing declared ignored |
| `node --check web/app.js` | Passed |
| `node --test web/*.test.mjs tests/navigation.test.mjs` | 196 passed, zero failed/skipped |
| `node --test tests/mobile_viewport_browser.mjs tests/navigation_browser.mjs tests/quick_talk_browser.mjs tests/web_mobile_pulse_browser.mjs` | 13 passed, zero failed/skipped |
| `git diff --check` | Passed |

The Rust gate ran from identical tracked sources in private snapshot
`/home/ryan/.cache/atmux-a4-merged-gates-0_g8vopu` (mode 0700), with
`CARGO_TARGET_DIR=/home/ryan/IdeaProjects/atmux-resume/target`, `ATMUX_REQUIRE_TMUX=1` and
`ATMUX_TMUX_SOCKET_NAME=atmux-ci-a4-intake-final-0g8vopu`. The snapshot avoids changing the
original worktree's mode 0775 and satisfies existing secure-executable ancestry tests.

Validation deviations: the existing Pulse post-flock replacement fixture's final reacquire now
allows 200 ms for descriptors briefly inherited by concurrent fork/exec fixtures; the competing
owner and unsafe-replacement assertions still use immediate refusal, and a leaked lock still
fails the bounded deadline. Production Pulse code is unchanged. One earlier Chromium navigation
run hit `Inspected target navigated or closed`; its focused retry and subsequent complete browser
runs passed. Four Rust threads and the private snapshot are validation accommodations, not
product behavior changes.

Lead merge notes: no functional work remains open. This branch contains lead head `a4d071d`;
merge it as a whole to retain the A3/A4 reconciliation. Use the single `[registry].directory`
configuration and A3 archive schema; there is no separate A4 store/manifest/Saved sessions view.
Automatic restore additionally requires `[events]` on coordinator and owners, coordinator
`restore_on_start = true` and explicit `restore_machines`, matching target profile names/native
stores, configured project roots and existing federation credentials. Session lifecycle/search
callbacks remain in `registry_integration.rs`; A4 uses the shared event service/fleet append and
does not introduce another publisher. Defaults remain off. No manual conflict reconciliation is
pending with A1/A2/A3/A5.

No deployment, push, service restart, cluster change or mutation of existing tmux/agents.
All tmux mutation fixtures use explicitly disposable sockets.
