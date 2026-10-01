# Agent control plane: events, conversation, lifecycle, resume anywhere, intake

Status: design accepted 2026-09-30; implementation in progress by Sol 6.1 agents

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
