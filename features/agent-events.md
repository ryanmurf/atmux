# A1: Agent events (hooks, spool, federation, MCP, Redpanda)

Status: implementation and acceptance verification in progress on `feat/agent-events`

Read `features/agent-control-plane.md` first; it defines the event envelope, types, session key,
and rules. This record is your brief. Update it with progress, evidence, and gate checkboxes.

## Goal

Every meaningful agent state change on any machine becomes an `atmux.agent.event/v1` event,
available through MCP and published to Redpanda by the coordinator. The most important event is
`agent.needs_input`: a human or supervisor must learn the moment an agent asks for input, asks
permission, or sits at a startup prompt.

## Scope

1. **Event model** (`src/events.rs`): the envelope type, constructors, a bounded serializer
   (64 KiB), and validation. Event ids are UUIDv7 (reuse `tmux::new_session_key`'s generator or
   factor a shared one). `project.remote` is the credential-free origin URL; strip userinfo and
   tokens. Read the git remote/branch with bounded, timeout-guarded commands and cache per cwd.
2. **Hook ingestion for both CLIs.**
   - Add a hidden `atmux hook <claude|codex> [event]` subcommand. It reads the CLI's hook JSON from
     stdin (bounded), adds `TMUX_PANE` and its parent PID, and delivers it to the owner over a
     per-user Unix socket (Linux `$XDG_RUNTIME_DIR/atmux/hooks.sock`, else `/run/user/<uid>/atmux`;
     macOS `$TMPDIR/atmux/hooks.sock`). The socket and its directory are mode 0700/0600 and owned
     by the user; refuse anything else. The hook must never block or fail the agent: exit 0 within
     ~200 ms even if the owner is down, and never print to stdout in a way the CLI interprets.
   - The owner listens on that socket, validates that the pane exists, and when the platform
     allows, that the peer PID descends from that pane's process (Linux `SO_PEERCRED`; macOS
     `LOCAL_PEERPID` if available, else skip). Map hook events to envelope types:
     - Claude Code: `Notification` (by `notification_type`: `permission_prompt` gives
       `needs_input/permission`, `idle_prompt` gives `needs_input/idle_prompt`, others ignored or
       `question`), `Stop` gives `turn_completed`, `PreCompact` gives `compacted` (or use the
       existing compaction detection), `SessionStart`/`SessionEnd` give `started`/`exited`,
       `UserPromptSubmit` gives `working`.
     - Codex: verify from the installed CLI (`codex --version` is 0.159.x; `codex features list`
       shows `hooks` stable) exactly which hook events and config keys exist, and use them. Also
       evaluate Codex `notify` (runs a program with a JSON argument on turn completion). Do not
       guess field names: find them in the installed package, its JSON schema, or its docs.
   - **Inject hooks at launch without editing user config files.** Claude: pass `--settings` with a
     JSON object containing only the hooks (verify the flag on the installed CLI). Codex: pass `-c`
     overrides. Apply to every atmux launch path: new launch, relaunch/restart, maintenance
     relaunch, resume, and Quick Resume `scoped-exec` (find them in `src/tmux.rs`:
     `build_launch_invocation`, `resume_claude`, `resume_after_cli_update`, `scoped_exec`). Add a
     config switch `[events] inject_hooks = true` (default true once `[events]` exists).
3. **Status-derived events.** For panes without hook delivery (old processes, other harnesses),
   derive `working`, `turn_completed`, and `needs_input/startup_prompt` from the existing status
   classifier (`src/status.rs`) and pane scans. Recognize the Claude development-channels dialog
   and trust dialog as `startup_prompt`. De-duplicate with hook events for the same pane and turn.
4. **Owner spool.** A bounded, durable event log under the atmux state directory: JSONL segments,
   size/age retention from `[events]`, a monotonically increasing `seq` plus a random `epoch`
   (regenerated if the spool is lost) so consumers detect resets. Survives owner restarts.
5. **Owner API.** `GET /api/v1/agent-events?after=<epoch:seq>&wait=<0..=30>&limit=<n>` long-poll,
   behind the same auth as the rest of the API; returns `{epoch, events, next, reset}`.
6. **Coordinator federation.** For each configured remote machine, a consumer task long-polls that
   owner's endpoint (mTLS + token like existing federation in `src/remote.rs`), tolerates offline
   owners and older owners that 404, and appends into a coordinator-wide bounded log keyed by
   `(machine, epoch, seq)`. Local owner events join the same log. Cursor state persists so a
   coordinator restart neither loses nor duplicates published events (at-least-once with event-id
   de-duplication is acceptable).
7. **MCP `agent_events`.** Long-poll with a cursor, filters (`types`, `machine`, `session_key`,
   `reasons`), bounded page size. Document it in the README MCP table.
8. **Redpanda sink (coordinator only).** `[events.redpanda]` with brokers, topic
   (`atmux.agent.events.v1`), and auth fields referenced by env/file (never inline secrets). Key =
   `session_key`, value = envelope JSON. Retry with backoff and a bounded backlog; never block the
   control plane. Prefer a pure-Rust client (for example `rskafka`) so macOS and ARM builds keep
   working; check `Cargo.lock` and justify any new dependency. Inspect the cluster read-only
   (`kubectl get svc -A | grep -iE "redpanda|kafka"`) to learn the in-cluster broker address,
   listener, and auth; do not create topics or change the cluster. The lead will append the
   platform's exact conventions to `features/agent-control-plane.md`; follow them if present.
9. **Dashboard.** Show a "Needs input" badge with the reason on the rail and in the agent header,
   driven by events (fall back to status). Keep changes small and append-only in `web/app.js`.

## Out of scope

Conversation/summaries (A2), registry/archive (A3), resume and startup-prompt auto-answer (A4).
A4 will consume your `needs_input/startup_prompt` events; keep that type stable.

## Acceptance

- Unit tests for envelope validation/bounds, hook-to-envelope mapping for both CLIs (fixtures
  from real hook payload shapes), spool rotation/retention/epoch reset, cursor semantics, and
  remote URL credential stripping.
- A disposable-tmux integration test: a fake "agent" script in a pane calls `atmux hook claude`
  with a Notification payload and the owner endpoint returns the `needs_input` event with that
  pane's `session_key`.
- Coordinator federation test with a fixture remote (see `tests/federation.rs`).
- MCP tool test. Sink unit test against a fake producer; no live broker required in tests.
- `cargo fmt`, zero-warning `cargo clippy --all-targets --all-features`, `cargo test
  --all-features`, `node --check web/app.js`, `node --test web/*.test.mjs tests/navigation.test.mjs`.

## Engineering decisions and evidence (2026-09-30)

- Telemetry is `Option<EventsConfig>`: absence opens no spool/socket, injects no
  hooks and returns 404 on event routes. Presence enables telemetry; hook
  injection defaults true. Private files/directories and a single-writer lock
  reject unsafe ownership, permissions and competing owners.
- Owner and fleet logs are separate on coordinators. Federation reads only
  owner events, preventing loops. Record fsync precedes atomic manifest and
  checkpoint persistence. Interrupted batches may replay; retained event ids
  are de-duplicated. Retention bounds disk and memory; cursor reset exposes
  gaps. Partial final JSONL lines are truncated after a crash, complete corrupt
  records fail closed. A lost manifest also changes epoch.
- Hook stdin/transport have a 160 ms deadline and silent success. The client
  waits briefly for a validation acknowledgement so SO_PEERCRED/LOCAL_PEERPID
  ancestry can be checked before it exits, without waiting on git or fsync.
  Eight server handlers and 64 KiB reads bound concurrent ingestion. Only
  event names, fixed reasons and safe model/compaction metadata are extracted;
  no native message, transcript, command, token or environment is logged.
- Codex was verified locally as 0.159.3 (`hooks stable true`); Claude is
  2.1.286 and its help confirms inline `--settings`. Native hook shapes come
  from the official [Codex hook reference](https://learn.chatgpt.com/docs/hooks)
  and [Claude hook reference](https://code.claude.com/docs/en/hooks), represented
  in `tests/fixtures/agent-events`. No real agents were launched to collect
  fixtures. Codex has PermissionRequest rather than Claude Notification.
- Codex inline `-c hooks.*` overrides require trust. Injection uses the native
  automation flag `--dangerously-bypass-hook-trust` to avoid startup prompts.
  It applies to all loaded hooks and CLI overrides replace the corresponding
  event arrays; this native limitation is documented. `inject_hooks=false`
  opts out. Existing `notify` callbacks are preserved; Stop supplies turn
  completion and avoids replacing the user's notification command.
- Native hook signals take precedence for their process generation; explicit
  visible dialogs supplement incomplete CLI coverage. Stop also emits
  needs_input/idle_prompt so Codex idle attention does not depend on a
  nonexistent Notification hook. A4's startup_prompt reason stays stable.
- Project metadata uses three bounded, 80 ms timeout-guarded git reads and a
  256-cwd/60-second cache. URL parsing strips userinfo, all queries/fragments
  and SCP usernames. New direct dependencies: `rskafka 0.6` (pure Rust,
  compression disabled, optional Rustls transport enabled), `chrono` (already
  transitive, shared with Kafka timestamps), `url` (credential-safe parsing).
  No librdkafka, OpenSSL or native Kafka build dependency is introduced.
- Follow the verified herodevs contract in the lead's
  `/home/ryan/IdeaProjects/atmux/features/agent-control-plane.md`: PLAINTEXT
  `redpanda.herodevs.svc.cluster.local:9092`, HdEventEnvelope v2, tenantId in
  securityContext and payload, configurable HQ tenant_id. Read-only kubectl
  service/deployment inspection confirms advertised `redpanda:9092` and
  ClusterIP `10.152.183.23`. No broker connection, topic creation, deployment,
  service restart, push or running-session action has been performed.
- Shared APIs for the other streams: `AgentEvent::from_session`,
  `ControlPlane::emit_agent_event`, `ControlPlane::agent_events(query, owner)`,
  and `events::sink::{KafkaProducer, Producer, PublishFuture}` with
  `publish(topic, key, value)` and `platform_envelope`.

## Gates

- [x] Envelope, URL sanitization and documented native-hook fixture tests.
- [x] Spool restart, rotation, retention, partial-tail recovery, epoch/cursor,
  filtered-page advancement and long-poll wakeup tests.
- [x] Disposable-tmux hook delivery with stable session_key; spoof rejection;
  silent successful helper when owner is down.
- [x] HTTP fixture federation, token forwarding, 404 compatibility, durable
  restart/replay de-duplication; no fleet events re-exported as owner events.
- [x] Fake-producer retry/checkpoint and exact platform wrapper tests.
- [ ] MCP enabled/disabled/filter tool tests.
- [ ] Launch/resume/maintenance/Quick Resume hook propagation coverage.
- [ ] Dashboard reason badges and browser suites.
- [ ] Helm hostAliases rendering/default and documented broker resolution.
- [ ] Full format, zero-warning clippy, Rust and JavaScript acceptance commands.
