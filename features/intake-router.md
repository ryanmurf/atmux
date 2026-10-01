# A5: Intake router (sources, ledger, routing) and shared platform clients

Status: assigned to a Sol 6.1 agent on branch `feat/intake`

Read `features/agent-control-plane.md` (all sections, especially "Phase 2") first. The events,
conversation/summaries, registry, and resume workstreams are merged on this base: use their APIs
(`ControlPlane` launch/send/session search, `EventService` emit and fleet append, the registry,
`agent_summary`, `sessions_find`, `sessions_search`). This record is your brief; keep it updated.

## Goal

Work from every source lands in one ledger (herodevs channel jobs) and is routed to the right
agent: an existing session that already owns that project, or a new session launched in the right
folder on the right machine with a clear kickoff. No single agent holds everything in context: each
item is triaged and routed independently with free local Qwen.

## Scope

1. **Shared platform clients** (commit these first; the supervisor workstream A6 consumes them):
   - `src/herodevs.rs`: a bounded HTTPS client for herodevs GraphQL (`https://hq.herodevs.dev/graphql`)
     and/or hd-mcp (`https://hq.herodevs.dev/hd-mcp/mcp`), whichever fits each call, covering:
     `postJob` (`channelMutations.postJob`, with `clientRequestId` for idempotency and the metadata
     keys `source_url`, `project_item_id`, `repo_remote`, `folder`, `machine`, `session_key`,
     `priority`, `due_date`), `listJobs` with metadata filters, claim/start/complete/fail/escalate,
     channel message send/history, and `search.query`. The API shapes are in
     `/home/ryan/IdeaProjects/herodevs.dev/hd-api-channel-jobs/docs/plans/channel-job-create.md` and
     `/home/ryan/IdeaProjects/herodevs.dev/hd-api-search-atmux/docs/plans/atmux-session-index.md`.
   - **Auth provider**: the recommended mode is a one-time device login by Ryan
     (`atmux herodevs login`, OAuth 2.0 Device Authorization Grant against the `hd-atmux`
     Keycloak client with `offline_access`), storing the refresh token in a 0600 file; refresh on
     demand and verify `sub`, tenant, and audience before use. The exact endpoints and parameters
     are being finalized in the hd-helm PR (#292) and the H4 plan file above; read them, and keep
     the provider pluggable so the documented token-exchange mode can be added later. Never log or
     print tokens.
   - `src/github.rs`: GitHub GraphQL client (token from a file) for ProjectsV2 items and field
     updates, PR status, and issue links.
   - `src/llm.rs`: an OpenAI-compatible chat client shared with the summarizer if practical
     (reuse `src/summarizer.rs`'s client rather than duplicating), with per-endpoint config: Flash
     Next at `http://192.168.0.124:8091/v1` (`qwen3.8-flash-next`) for triage, and the 27B through
     Max's LiteLLM gateway at `http://192.168.0.124:8094/v1` with a key file for routing decisions.
2. **Sources** (each with a durable cursor, bounded batches, and idempotent job creation):
   - GitHub ProjectsV2 boards in org `neverendingsupport`: **#40 NES Factorio** and
     **#51 NES Java Team v2**. Items not Done (configurable status names), optionally filtered to
     assignee `ryanmurf`. One ledger channel per board. Item updates (status, assignee, title)
     update the job's metadata or close it.
   - Gather meeting transcripts: new `FileUpload` Markdown transcripts in herodevs search since the
     cursor (names like `…_Gather_…md`); fetch the text; Flash Next extracts action items for Ryan
     (owner, what, due, repo/project hints) as strict JSON; one job per action item in a
     `meetings` channel, linking the transcript.
   - Slack: messages that existing herodevs Slack triggers forward into a configured intake
     channel; Flash Next classifies actionable requests for Ryan; jobs link the message.
   - atmux itself: sessions that went idle with unfinished work (registry + digest) become
     follow-up jobs when no job references them.
3. **Router** for each new or unassigned job: candidates are live sessions whose project matches
   (`repo_remote`, folder) plus digest search (`sessions_find`, herodevs `search.query` over
   `ATMUX_SESSION`). The 27B decides, with strict JSON output validated against an allowlist, to
   (a) hand the job to an existing session (send a concise kickoff), or (b) launch a new session:
   find the folder by repo remote under the owners' project roots (launch-directory listing; clone
   with the existing clone API if absent), choose machine/profile/mode by policy (default
   Codex `profile-0` + `sol61-xhigh` for implementation, configurable per job type), launch, and send
   the kickoff. The kickoff carries the job id, source link, goal, completion criteria, and the
   completion protocol (`JOB DONE <job_id>: <summary> <PR URL>` or `JOB BLOCKED <job_id>: <why>`).
   Record the assignment in the job metadata and claim the job.
4. **Configuration and safety**: `[intake]` off by default, `dry_run` (decide and log, act on
   nothing), a kill switch, budgets (new sessions per hour/day, per machine; LLM calls per hour),
   and every decision published as an `intake.*` event through the fleet append so the audit
   trail reaches Redpanda and search.
5. **Dashboard**: a compact "Work" view listing ledger jobs by source and state with the assigned
   session (click through to it).

## Acceptance

- Unit tests with fake GitHub, herodevs, and LLM servers: idempotent job creation and updates,
  cursor durability, Gather action-item extraction (including malformed model output), Slack
  classification, routing decisions (existing session, launch, clone), budget and dry-run
  enforcement, token refresh and redaction.
- An end-to-end test against fixtures: a board item becomes a job, routes to a launched disposable
  "agent" in a disposable tmux socket, and receives the kickoff.
- `cargo fmt`, zero-warning clippy, `cargo test --all-features`, node tests, browser suites.
- Do not contact live GitHub, herodevs, or Qwen in tests. Read-only manual checks against the live
  Qwen endpoints are fine and should be recorded here.
