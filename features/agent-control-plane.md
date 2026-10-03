# Agent control plane: events, conversation, lifecycle, resume anywhere, intake

Status: A1–A6 merged; A1–A4 deployed fleet-wide 2026-10-03 (runtime `acbee1b`); A5/A6 intake and supervisor not yet enabled (need credentials and chart wiring)

## Requests (2026-09-30)

- Track work across tmux sessions, nodes, two GitHub project boards (`neverendingsupport` org:
  **NES Factorio** #40 and **NES Java Team v2** #51), Gather meetings (Whisper transcripts already
  land in herodevs.dev), Slack, and unfinished threads; keep agents moving toward goals.
- Use herodevs.dev channel jobs as the ticket ledger (source of truth). Full autonomy is approved.
- An intake that knows the pool of agents and can find the right folder, rather than one agent
  holding everything in context. Free local Qwen (Flash Next, 27B) for grunt work and monitoring.
- Events whenever an agent asks for input or prompts; simple tools to send a message and get a
  summary; conversation loading with filters (tools, user messages) for both Claude Code and Codex.
- No startup prompts. Go as deep as needed.
- atmux events go to a Redpanda topic, and the herodevs trigger service treats them as a
  first-class trigger type.
- Auto summaries and easy rename in the UI.
- Powerful search: everything indexed into herodevs; agent sessions kept continuously updated for
  search as tool-free, compaction-style digests (Qwen).
- Closing a tmux session archives it. A durable record of every session and its current state so
  resume works on any computer; a node that starts phones home and gets its sessions back.

## Architecture

```
 CLI hooks ──► atmux owner ──(federation pull)──► atmux coordinator (k8s) ──► Redpanda topic
 (Claude, Codex)  │  event spool,                   │  federated event log,      │
 status changes ──┘  session registry,              │  central registry,         ├─► hd-api-trigger
                     archive bundles                │  summarizer (Qwen),        │   (atmux.agent trigger)
                                                    │  MCP tools                 └─► hd-api-search
                                                    ▼                                  (session digests)
                                         intake / supervisor agents (herodevs jobs + atmux MCP)
```

- **Owners never need cluster access.** The coordinator already pulls from owners over mTLS; it
  pulls events and registry snapshots the same way and is the only component that talks to
  Redpanda and herodevs. Owners keep a durable bounded spool so nothing is lost while the
  coordinator is down.
- **The coordinator is "home".** It keeps the central registry and archive bundles on its
  persistent volume, publishes events, runs the summarizer, and serves the MCP tools.

## Shared contracts (all workstreams)

### Session key

Every agent session gets a stable `session_key`: a lowercase UUIDv7 minted by atmux at launch (or
on first discovery of an unmanaged pane) and stored in the tmux pane option `@atmux_session_key`.
This is already implemented in the base commit: `Session::session_key`,
`tmux::new_session_key`, `tmux::valid_session_key`, `tmux::SESSION_KEY_OPTION`, and
`SessionSummary::session_key` in the API. To carry a key to a restored or resumed pane, set the
option on the new pane before the first scan. It survives pane respawns, CLI relaunches, renames, archive, and resume on
another machine. The pane id and the native conversation id are attributes of it, not identity.

### Event envelope `atmux.agent.event/v1`

One JSON object per event, at most 64 KiB serialized:

```json
{
  "schema": "atmux.agent.event/v1",
  "id": "<uuidv7>",
  "time": "<RFC 3339 UTC>",
  "machine": "tron",
  "session_key": "<uuidv7>",
  "pane": "tron~%10",
  "instance_id": "pane-v1-…",
  "session_name": "atmux-fable",
  "harness": "claude | codex | other",
  "profile": "hd",
  "model": "gpt-6.1-sol",
  "cwd": "/home/ryan/IdeaProjects/atmux",
  "project": { "remote": "https://github.com/ryanmurf/atmux", "branch": "main", "root": "/home/ryan/IdeaProjects/atmux" },
  "type": "agent.needs_input",
  "reason": "idle_prompt",
  "summary": "Optional one-line human text",
  "detail": { }
}
```

Event types (`type`), with `reason` where noted:

| Type | Meaning |
| --- | --- |
| `agent.started` | A CLI process started in a pane (reason: `launch`, `relaunch`, `resume`, `restore`, `discovered`) |
| `agent.turn_completed` | The agent finished a turn and is idle |
| `agent.needs_input` | The agent is waiting on a human (reason: `idle_prompt`, `question`, `permission`, `startup_prompt`, `plan_approval`) |
| `agent.startup_prompt_answered` | A known startup dialog was answered once (detail: dialog, verified, agent_pid, process_start; no dialog text or argv) |
| `agent.working` | The agent resumed work after input |
| `agent.compacted` | A native compaction happened (detail: trigger, pre/post tokens) |
| `agent.summary_updated` | The rolling digest or title changed (detail: title, description, digest, digest_version) |
| `agent.renamed` | Session name or description changed (detail: old/new, by: user or auto) |
| `agent.exited` | The CLI process exited (detail: exit status if known) |
| `session.closed` | The tmux session disappeared |
| `session.archived` | The registry archived the session (detail: archive bundle id) |
| `session.resumed` | An archived or remote session was resumed (detail: from machine, to machine) |
| `node.started` | An owner started or reconnected (detail: boot id, version) |

Never include message bodies beyond the bounded `summary`/digest, secrets, tokens, or raw
environment. Paths are allowed (they are already visible to anyone with shell-equivalent access).

### Redpanda

The coordinator publishes every event to topic `atmux.agent.events.v1`, keyed by `session_key`,
value = the envelope above. Broker, auth, and topic provisioning follow the platform's existing
conventions (see the herodevs research notes appended below).

### Configuration sections (new)

- `[events]` — spool size/retention on owners; on the coordinator, the Redpanda sink.
- `[summaries]` — OpenAI-compatible endpoint (`http://192.168.0.124:8091/v1`, model
  `qwen3.8-flash-next`), timeouts, concurrency, minimum interval per session. Runs on the
  coordinator, reading conversations through the existing federated transcript path.
- `[registry]` — central store location on the coordinator; archive bundle size limits.

Every section is optional and off by default; absence changes nothing.

## Workstreams and ownership

Each workstream is one Sol 6.1 agent in its own git worktree and branch from the commit that
added this record. Owned
modules are new files wherever possible; shared files (`src/control.rs`, `src/web.rs`,
`src/mcp.rs`, `src/main.rs`, `web/app.js`, `README.md`, `features/requests.md`) get append-only
additions so merges stay mechanical.

| Id | Branch | Owns |
| --- | --- | --- |
| A1 | `feat/agent-events` | `src/events.rs`, `atmux hook` subcommand, hook injection at launch for both CLIs, status-derived events, owner spool, owner `GET /api/v1/agent-events` long-poll, coordinator federation of events, MCP `agent_events`, Redpanda sink |
| A2 | `feat/agent-conversation` | MCP `agent_conversation` (filters + cursor) and `agent_summary`, `src/summarizer.rs` (Qwen), rolling tool-free digests and auto titles/descriptions, `agent.summary_updated`, UI auto summary display and easy inline rename |
| A3 | `feat/session-registry` | `src/registry.rs`: session keys, owner registry, coordinator central registry, archive on close (bundle: record + native log + git state), archived sessions in UI and MCP (`sessions_search`, `session_get`), `session.*` events |
| A4 | `feat/resume-anywhere` | native log bundle export/import with path translation (Linux ↔ macOS homes, repo lookup by remote), resume on any machine (UI + MCP `session_resume`), node phone-home restore of its last desired sessions, startup prompt auto-answer for known dialogs |
| H1 | herodevs `hd-api-trigger` | first-class `atmux.agent` trigger type consuming `atmux.agent.events.v1` |
| H2 | herodevs `hd-api-search` (+ `hd-mcp`) | index session digests and archived sessions from the topic; search filters for source `atmux` |
| H3 | herodevs intake | intake/router + supervisor: GitHub boards #40/#51, Gather transcripts, Slack into channel jobs; route to the agent pool or launch in the right folder via atmux MCP; Qwen monitoring |

## Rules for every agent

- Do not deploy, restart services, push, or touch running tmux sessions or agents. The lead
  integrates, reviews, and deploys.
- Never kill or rebuild a tmux server; tests use disposable sockets (`DisposableTmux` pattern).
- Keep every new capability fail-closed and off unless configured; bound every read and payload.
- Tests: `cargo fmt`, `cargo clippy --all-targets --all-features` with zero warnings, `cargo test
  --all-features`, `node --check web/app.js`, `node --test web/*.test.mjs tests/navigation.test.mjs`,
  and the browser suites when the UI changes.
- Commit on your branch with clear messages. Keep a feature record under `features/` with
  acceptance criteria, gates, and evidence.

## herodevs platform contract (verified 2026-09-30 against origin/main and the live cluster)

**Redpanda.** In-cluster broker `redpanda.herodevs.svc.cluster.local:9092`, PLAINTEXT, no SASL/TLS,
single node (`--mode=dev-container`), `auto_create_topics_enabled=true` (1 partition, RF 1). The
broker advertises `redpanda:9092`, so a client outside namespace `herodevs` must resolve
`redpanda` (the atmux coordinator Deployment needs `hostAliases: [{ip: <redpanda ClusterIP>,
hostnames: [redpanda]}]`; ClusterIP today is `10.152.183.23`). There is no HTTP proxy and no LAN
path; only the coordinator publishes. Anything in the cluster can publish any tenant's events: that
is the platform's existing trust model.

**Envelope.** Platform listeners unwrap a message only when it has both `envelopeVersion` and
`eventPayload`. atmux publishes to `atmux.agent.events.v1`, key `session_key`, value:

```json
{"envelopeVersion": 2, "eventType": "atmux.agent.event.v1", "publishedAt": "<RFC 3339>",
 "securityContext": {"tenantId": "<tenant>", "token": null, "platform": "SYSTEM", "userId": null, "sessionId": null},
 "mdc": {}, "payloadRef": null, "payloadSummary": null,
 "eventPayload": { "...the atmux.agent.event/v1 object...": "...", "tenantId": "<tenant>" }}
```

`tenantId` must be inside `eventPayload` as well: without a JWT the platform cannot resolve a
tenant otherwise and drops the event. HQ tenant: `95efe33d-fa71-53ce-8e0a-3fe45ac0e58a`
(configurable, `[events.redpanda] tenant_id`). `platform` must be one of
`ANDROID|API|IOS|SYSTEM|UNKNOWN|WEB|WORKFLOW`. Keep values well under 800 KB.

**Search.** hd-api-search has no ingestion API; documents enter only through `entity-change`
events with an index definition for the entity type. atmux session digests are indexed as entity
type `ATMUX_SESSION`, `entityId` = `session_key` (a UUID), by the coordinator publishing an
`HdEntityChangeEvent` envelope to topic `entity-change` (exact shape in the H2 record). Embeddings
are OpenAI `text-embedding-3-small`; keep each digest document under 8 KB for one clean vector.
Search is exposed as MCP `search.query` with `entityTypes: ["ATMUX_SESSION"]` (source `atmux`).
Gather transcripts are already indexed as `FileUpload` Markdown.

**Triggers.** The generic `event` trigger has no generic topic consumer and never evaluates its
filter, so `atmux.agent` is a dedicated first-class type (H1) with its own listener and selector.

**Jobs.** hd-api-channel can claim/complete jobs but cannot create them, and its lease sweeper is
off in production. H4 adds job creation and enables the sweeper.

**Shipping herodevs changes.** Production runs the `hd-api` monolith built from every module's
`main`; a merged module ships only after hd-api's Publish Image workflow runs, and new
`trigger.*` RLS-ignored tables must be mirrored in `hd-api/src/main/resources/service.yml` and
`hd-helm` `charts/local/values.yaml`.

## Phase 2: intake router and supervisor (A5, A6), after A1–A4 and H4 merge

Runs inside the atmux coordinator (Kubernetes, federates every owner), enabled by `[intake]` with a
`dry_run` switch and a kill switch. Free local Qwen does the per-item work so no single agent holds
everything in context: Flash Next (`192.168.0.124:8091`, 262K context) for triage and digests, the
27B (`192.168.0.124:8096`, router on Max) for routing and supervision decisions. Hard or ambiguous
decisions escalate to Ryan.

**A5 intake (sources, ledger, router).**
- Sources: GitHub ProjectsV2 boards `neverendingsupport` #40 NES Factorio and #51 NES Java Team v2
  (GraphQL polling, token from a mounted secret); Gather meeting transcripts already indexed in
  herodevs search (new `FileUpload` Markdown since a cursor, Qwen extracts Ryan's action items);
  Slack via existing herodevs Slack triggers posting to an intake channel; atmux sessions that went
  idle with unfinished work.
- Ledger: herodevs channel jobs (H4), one channel per board or source, job metadata carrying the
  source URL, project item id, repo remote, folder, machine, and assigned `session_key`.
- Router: for each new job, candidates are live sessions whose project matches (registry) plus
  digest search (`sessions_find`, herodevs `search.query` over `ATMUX_SESSION`); Qwen chooses to
  hand the job to an existing session or launch a new one in the right folder (found by repo remote
  under the configured project roots, cloning if absent) with the right profile and mode, then
  sends a kickoff prompt carrying the job context and completion criteria.

**A6 supervisor.**
- On `agent.needs_input`: read `agent_summary` and the last turn; answer routine prompts (continue,
  permission within the job's scope, questions answerable from the job) or escalate to Ryan via
  Slack and the `ryan-tron` channel, with a link to the pane.
- On `agent.turn_completed`: check the job's completion criteria (PR opened or merged, CI green,
  tests reported), complete or fail the job, update the GitHub project item status, and close the
  tmux session (which archives it through A3) once the job is done and the session is idle.
- Nudge stalled sessions; enforce budgets (sessions per machine, Qwen calls per hour); publish
  every decision as an event so the audit trail lives in Redpanda and search.

## Deployment — 2026-10-03

Runtime commit `acbee1b`: the A1–A6 integration (`354dfce`), plus `888d705`, which keeps an owner
running when a profile's session store can't be resolved, plus the Conversation Copy button.
Gate: fmt and clippy clean; 939 Rust tests passed. Run them with `ATMUX_REQUIRE_TMUX=1` and a
disposable `ATMUX_TMUX_SOCKET_NAME=atmux-ci-…`, or the federation suite fails. 200 JS tests
passed. `disposable_owner_archives_external_and_dashboard_kills` once hit its 20 s native-log
mapping deadline under `--test-threads=4` and passes on its own.

Owners got `[events] inject_hooks = true`, `[registry] enabled = true` and
`[startup_prompts] auto_answer = true`. Each node keeps its previous config as
`config.toml.pre-control-plane` and its previous binary as `atmux.rollback-pre-*`.

| Machine | Artifact SHA-256 prefix | Notes |
| --- | --- | --- |
| Tron | `50a55e23ae250f05` | `atmux-web:0.0` respawned with its original `scoped-exec` command; 24 tmux sessions kept |
| Max | `50a55e23ae250f05` (Tron's build) | the first enable on `354dfce` crash-looped (fixed by `888d705`); profile `max` stays unbound (no `CLAUDE_CONFIG_DIR`) |
| Midnight | `e3aa2db26394d2eb` (built on Midnight) | loopback is plain HTTP; mutual TLS only on LAN addresses |
| Clue | `639c42d745d82863` (aarch64, built on Clue) | |
| Coordinator | `localhost:32000/atmux:acbee1b5f142@sha256:69925122…` | Helm rev 34: events + Redpanda sink, registry, summaries (Qwen Flash Next), `hostAliases` redpanda |

Coordinator notes: don't use `helm upgrade --reuse-values` when the chart adds new keys. It drops
the new defaults, which rendered `brokers = ""` and an empty egress CIDR (rev 32 crash-looped and
was rolled back to rev 31). Instead, pass the release's saved values plus the overlay with `-f`.
Redpanda topic `atmux.agent.events.v1` (1 partition, RF 1) was created by hand: the sink requires
a provisioned topic. Events are verified on the topic with envelope v2 and the HQ tenant.
`restore_on_start` stays off until the registry has run for a while.

Still open: shared herodevs device login, intake/supervisor channel ids, GitHub token and the
chart's intake/supervisor values; `[summaries] search_tenant_id` once H2 indexing is live.
