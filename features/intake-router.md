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

## Implementation record: shared clients (2026-09-30)

- Shared bounded, non-redirecting hyper/rustls transport; HTTPS required unless a private
  HTTP host is explicitly allowlisted (all resolved addresses checked). Responses <=2 MiB,
  prompts <=96 KiB, LLM output <=64 KiB, operation timeouts <=300 seconds. Upstream error
  bodies are never propagated, so rejected operations cannot reflect credentials into logs.
- `HerodevsClient` exposes GraphQL jobs, metadata filters, fenced transitions, messages/history,
  and MCP search. Claim/transition use message `id`, not `jobId`, as H4 requires. `AuthProvider`
  is pluggable for A6/fakes/future exchange. No implicit exchange or service-account fallback.
- `DeviceAuth` implements pinned discovery/origin, RS256 JWKS verification, expected subject,
  issuer/time, azp, tenant, USER identity, all audiences, initial ID-token nonce/offline scope,
  tenant_slug on each grant, serialized refresh and atomic 0600 token rotation. A durable
  LOGIN_REQUIRED marker precedes refresh to fail closed after ambiguous consumption or crash.
  `atmux herodevs login` displays only verification URI/user code. Client secret and refresh
  token stay in separate files. Refresh locks also exclude multiple coordinator processes.
- `GithubClient` provides ProjectsV2 pagination, single-select status updates, PR CI/merge/review
  state and issue/PR links; its token is read from a file. `LlmClient` handles per-endpoint
  model/key/timeout settings and strict JSON. The summarizer now shares that transport/client
  while retaining its own prompt, output sanitation and scheduling.
- Platform shapes were read from the H4/H2 plans and local GraphQL/resolver sources. No live
  platform calls, login, Keycloak/cluster changes, deployment, push or session mutation.
- H4 does not expose job metadata updates. Intake will use idempotent channel assignment/update
  messages plus durable local overlays; the lead should add a metadata-update API if the row
  itself must change. Retrying postJob deliberately returns the original payload.
- Gates: client fake-server integration tests 4/4; clippy all targets/features with -D warnings
  passed. Initial full suite: 733 passed, 12 failed, 6 ignored: ten recovery ancestry failures
  (group-writable checkout and /tmp), one self-update ETXTBSY race, one summarizer literal-IP
  validation regression subsequently fixed. Full gates will be repeated in a private fixture
  checkout with a private TMPDIR. Test-only RSA fixture is generated locally, never a credential.

## Intake implementation decisions and integration contract (2026-10-01)

- Sources are bounded per poll. Boards #40/#51 are explicit configurable entries; status/title/
  assignee changes produce idempotent source-update messages and Done/unassigned sources close
  their job through a fenced completion. Finished board scans restart so older item updates are
  eventually observed. Slack keeps its append-only Relay cursor. Gather keeps a Relay scan cursor
  and a per-upload completion marker, confirms FileUpload search indexing, and fetches bounded
  Base64 Markdown through GraphQL. The H2 search API is relevance-ranked, not an exhaustive
  change feed, so FileUpload enumeration is necessary to avoid silently missing transcripts.
- Strict Flash JSON is validated in full before posting anything. Only configured owner names
  are admitted; due dates must be RFC3339; model repository hints must match configured policies.
  Extractions are persisted before posting so partial retries retain identical item identities.
  Idle sessions with unfinished digest work produce follow-ups unless already referenced by a job.
- Router choices are an enum of offered idle session UUIDs, offered policy ids, or escalation.
  Existing sessions must match repository/folder or a source's exact session key and cannot be
  assigned another active job. Policies bind machine/root/profile/mode; the model cannot invent
  commands or paths. Registry/digest search and herodevs ATMUX_SESSION search provide evidence.
  Repository lookup verifies actual git remotes in a bounded owner-local scan; cloning uses the
  existing owner clone API, only from configured prefixes and explicitly clone-enabled policies.
- Ledger claims precede clone/launch; renewClaim validates the current fence before side effects.
  Reservations, budgets, assignments and delivery markers are fsynced atomically. Stable launch
  names let retries rediscover a created session. Interrupted kickoff delivery escalates for
  inspection instead of replaying a possibly delivered prompt. Assignment messages include job
  message id, job id, fence token, lease expiry and metadata; A6 can consume these channel records,
  `intake.assigned` events, or the local `/api/v1/work` mirror. Kickoff includes source, goal,
  criteria and JOB DONE/JOB BLOCKED protocol. Owners enforce the pane instance on send.
- Every decision uses the fleet append; explicitly enumerated `intake.*` event types are accepted
  by the existing event envelope. Events contain bounded metadata, not tokens or source bodies.
  Dry-run records decisions/audits/local budget accounting but performs no ledger mutations,
  cursor advancement, clone, launch or kickoff. Live mode ignores persisted dry-run mock jobs.
  A filesystem kill switch is checked before each external mutation. One coordinator worker owns
  the durable store lock; hourly/daily budgets survive restart and count failed reservations
  conservatively. New sessions default to Codex profile-0/sol61-xhigh through an explicit policy.
- Work is a compact accessible dialog, showing the local ledger mirror by source/state with
  assigned-session links. It refreshes only when visible; hostile text is rendered as text.
- Shared-client follow-up corrects the gateway's zero-argument `tenant` query (verified against
  hd-api-tenant source) and uses deterministic UUIDv8 clientRequestId values, because H4's ID
  input is backed by Java UUID. `herodevs::client_request_id` is shared with A6. Job type is now
  exposed for routing policy. Device polling honors pending/slow_down and the device lifetime;
  refresh reads the same checked file descriptor. Initial-login fixtures cover all identity pins.

### Deviations and practical limits

- H4 has no metadata-update mutation. The original job row's metadata stays immutable; assignment
  and source updates are durable channel messages plus coordinator overlays. This preserves the
  one-job ledger and its state machine without inventing an API. A6 must read the assignment
  contract above rather than assume listJobs(metadata: {session_key: ...}) finds updated rows.
- H4 listJobs has no pagination and caps newest-first results at 100. The local mirror retains all
  ingested jobs, but importing externally-created jobs is limited to the latest 100 per channel.
  The lead should add ledger pagination for larger externally-fed backlogs; intake will refresh
  known jobs by their idempotency metadata to avoid stale states outside that newest page.
- Repository scans cap at 2,000 entries/depth 4/five seconds; an incomplete scan fails closed.
  A configured policy folder is the deterministic option for larger roots. Store caps are 10,000
  jobs/cursors and 16 MiB; exceeding a cap requires operator retention/cleanup rather than silently
  discarding ledger/cursor history. This first version does not automatically revoke offline
  credentials; Keycloak Account Console can revoke the session.
- Read-only manual probes (2026-10-01): GET 8091/v1/models reports qwen3.8-flash-next, context_length
  262144; GET 8094/v1/models rejects anonymous access with 401. No inference was requested, and no
  live GitHub/herodevs call was made. The exact gateway 27B alias must be configured by the lead.

### Lead configuration (no deployment performed)

1. Integrate H4 and hd-helm PR #292, enable its opt-in confidential hd-atmux device client and mount
   its client secret. Set the pinned public Keycloak issuer and independently obtained Ryan sub;
   retain tenant_slug=hq and both audiences. Ensure Ryan's existing channel memberships/offline
   access. Run `atmux --config <coordinator config> herodevs login` explicitly on the coordinator;
   mount/persist the generated refresh token separately with 0600 permissions. No passwords or
   tokens should be placed in argv, TOML, logs or session archive directories.
2. Mount GitHub and LiteLLM key files; GitHub needs project/read access plus private repository
   visibility, and A6 status updates need project write access. Configure the exact 27B model
   alias from the authenticated gateway. Explicitly allowlist the Qwen LAN host for HTTP.
3. Configure board-channel ids for #40/#51, meetings, Slack trigger inbox and Slack job destination,
   followups, and the Ryan escalation channel (ryan-tron). Existing Slack triggers must forward
   messages into that inbox. All channel ids are UUIDs; no channels are created implicitly.
4. Enable registry and fleet events on the coordinator, provide a private persistent intake store
   and kill-switch path, and configure owner launch policies/roots/profiles/modes. Start with
   dry_run=true. Creating the kill-switch file stops mutations without a restart. A6 must renew
   the initial 3,600-second claim and preserve its fence through completion.

Example additions to the coordinator TOML (replace UUIDs, issuer, alias and paths):

```toml
[herodevs.auth]
issuer = "https://KEYCLOAK_HOST/realms/herodevs"
client_id = "hd-atmux"
client_secret_file = "/run/secrets/hd-atmux-client"
refresh_token_file = "/var/lib/atmux/credentials/herodevs-refresh"
expected_subject = "RYAN_SUB_UUID"
tenant_slug = "hq"
audiences = ["hd-subgraphs", "https://hq.herodevs.dev/hd-mcp"]

[intake]
enabled = true
dry_run = true
store_dir = "/var/lib/atmux/intake"
kill_switch_file = "/var/lib/atmux/intake.STOP"
meetings_channel = "MEETINGS_CHANNEL_UUID"
slack_channel = "SLACK_INBOX_CHANNEL_UUID"
slack_jobs_channel = "SLACK_JOBS_CHANNEL_UUID"
followups_channel = "FOLLOWUPS_CHANNEL_UUID"
escalation_channel = "RYAN_TRON_CHANNEL_UUID"
allowed_repo_prefixes = ["https://github.com/neverendingsupport/"]
new_sessions_per_hour = 4
new_sessions_per_day = 20
sessions_per_machine = 4
llm_calls_per_hour = 120

[intake.github]
token_file = "/run/secrets/github-projects"

[intake.triage]
endpoint = "http://192.168.0.124:8091/v1"
model = "qwen3.8-flash-next"
allow_http_hosts = ["192.168.0.124"]

[intake.routing]
endpoint = "http://192.168.0.124:8094/v1"
model = "REPLACE_WITH_GATEWAY_27B_ALIAS"
api_key_file = "/run/secrets/qwen-router"
allow_http_hosts = ["192.168.0.124"]

[[intake.boards]]
number = 40
channel_id = "FACTORIO_CHANNEL_UUID"
assignee = "ryanmurf"
done_statuses = ["Done"]

[[intake.boards]]
number = 51
channel_id = "JAVA_CHANNEL_UUID"
assignee = "ryanmurf"
done_statuses = ["Done"]

[[intake.policies]]
id = "implementation-tron"
machine = "tron"
project_root = "/home/ryan/IdeaProjects"
profile_id = "profile-0"
mode_id = "sol61-xhigh"
job_types = ["IMPL"]
allow_clone = true
```

### Acceptance evidence

- Eleven intake tests cover idempotent board creation/updates/closure, durable cursors, Gather/
  Slack extraction and malformed JSON, owner filtering, existing/launch/clone decisions, durable
  budgets and budget exhaustion, dry-run/kill-switch enforcement, invalid choice escalation,
  interrupted kickoff recovery, follow-up deduplication and actual repository-remote lookup.
- Board -> fixture channel job -> launched disposable tmux agent -> captured kickoff passes;
  only uniquely named disposable sockets are created/killed. No running tmux session was touched.
- Work unit test and mobile Chromium fixture verify source/state filters, session links, hostile
  text, and narrow-screen layout. All 12 existing browser suites passed before this added check.
- Final full gates and commit ids are recorded below after the private-checkout run completes.
