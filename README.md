# atmux

`atmux` is a tmux control plane for terminal coding agents. It puts every session in one live view, makes waiting agents obvious, and launches Codex or Claude into the right project with a few keystrokes.

```text
 atmux  tmux agent control plane     ● 4 working   ◆ 3 waiting
┌ Sessions (8) ─────────────┐ ┌ chptr-2 ──────────────────────────────┐
│ ◆ chptr-2                 │ │ ◆ waiting  Claude                     │
│ ◆ mercury                 │ │ path  ~/IdeaProjects/chptr-2          │
│ ● release-review          │ └───────────────────────────────────────┘
│ ● atmux                   │ ┌ Live pane ────────────────────────────┐
│ ○ database                │ │ All checks passed.                    │
│                           │ │                                       │
│                           │ │ ❯                                     │
└───────────────────────────┘ └───────────────────────────────────────┘
 e quick edit   enter switch   n new   / filter   x kill   ? help   q quit
```

## What it does

- Shows all tmux sessions in a compact left rail that grows to fit, stops at 25%, and ellipsizes long names.
- Aggregates several machines into one dashboard, grouped by machine with online/offline health.
- Marks detected Codex and Claude agents as `● working` or `◆ waiting`.
- Shows a live preview of the selected session's active pane.
- Opens a selected session for a quick edit in a popup without leaving the control plane.
- Switches the current tmux client instantly, or attaches when run outside tmux.
- Launches an agent through an agent → profile → project → session-name wizard.
- Recursively finds Git projects and folders with agent instruction files beneath configured roots, remembers project-local choices, and discovers installed Codex and Claude profiles.
- Supports custom commands, arguments, environment variables, state markers, and exact tmux status overrides.

`atmux` is intentionally local and direct: it talks to the tmux server you already run and starts the same agent CLIs you use by hand.

## Install

atmux supports Linux and macOS and tests both in CI. You need Rust 1.88+ and tmux.

On macOS, install tmux first if needed:

```bash
brew install tmux
```

Then install atmux on either platform:

```bash
cargo install --git https://github.com/ryanmurf/atmux
atmux doctor
atmux
```

## Web dashboard

Run the browser control plane and MCP endpoint alongside the existing TUI:

```bash
atmux web
```

Open <http://127.0.0.1:7345>. The dashboard can:

- stream agent status and the selected pane;
- send literal multiline messages;
- interrupt or kill a session; and
- launch configured profiles in configured project directories.

The web server polls tmux once regardless of how many interfaces are connected. Browser clients receive change-only
Server-Sent Events: compact overview patches on one stream and line-level patches for only the
selected pane on another. Both streams are driven by a shared revision counter rather than by a
queue, so a client that falls behind skips straight to the newest state instead of replaying every
intermediate one. A hidden browser tab closes both streams, so idle tabs use only an occasional SSE
keepalive.

Each patch names the revision it applies to. A client that receives a patch which does not continue
the revision it holds discards it and reconnects for a fresh snapshot, so a missed update can never
be merged into a half-correct view.

Click a machine header in the session rail to inspect that machine's live CPU, memory, GPU, and temperature readings. GPU data is shown when the host exposes NVIDIA's `nvidia-smi`; unavailable hardware probes stay empty instead of failing the dashboard. The launch dialog has an **Agent** picker (Claude or Codex), a profile picker, and a project field that filters recursively discovered projects as you type. **Browse** safely explores the selected machine's configured project roots; it can navigate to every allowed parent, create a folder, or clone a credential-free HTTPS/SSH repository into the displayed directory. These mutations run only on the selected owning machine and never overwrite an existing target. A chosen folder is remembered for that machine while every launch remains server-validated. When viewing an agent in a browser that supports the Web Speech API, hold **Talk** to dictate and release it to send the recognized text directly to that agent.

### Automatic context compaction

Each atmux node can compact its own inactive Claude and Codex panes. The
defaults send the literal native `/compact` only when a pane has been waiting
for more than 15 minutes and its exactly mapped native session log reports more
than 200,000 current input/context tokens:

```toml
[auto_compact]
enabled = false # set true explicitly on each owner node
inactivity_minutes = 15
input_tokens = 200000
poll_seconds = 30
```

Claude context is its latest assistant `input_tokens` plus cache creation and
cache-read input. Codex context is the latest native
`token_count.info.last_token_usage.input_tokens`; its cumulative usage is not
used and cached input is not counted twice. Terminal text is never interpreted
as token data. Missing/ambiguous session identity, malformed or incomplete
usage, active/working panes, and unsupported harnesses all fail closed.

The scheduler runs only on the pane's owning node. It serializes through the
same per-pane gate as messages, interrupts, and model changes and stores a
claim in tmux before sending `/compact`. That claim survives atmux restarts and
is cleared only after the same native session reports a context value back at
or below the threshold, preventing duplicate compactions across polls and
coordinators.

### Per-agent memory isolation

Linux owner nodes can place every newly launched agent process tree in its own
transient systemd user scope with a cgroup `MemoryMax`:

```toml
[agent_resources]
memory_max_bytes = 34359738368 # 32 GiB; example only, size per host
# Optional: permit New Agent / Duplicate to select a whole-GiB cap no larger
# than this explicit owner ceiling. Absence keeps overrides disabled.
memory_override_max_bytes = 51539607552 # 48 GiB
```

This policy is deliberately disabled when the key is absent. When configured,
atmux runs a bounded property preflight and fails the launch before starting an
agent if `/usr/bin/systemd-run`, the systemd user manager, cgroup v2 memory
controller, or `MemoryMax` is unavailable. The supported minimum is systemd
236 (`--collect`); no newer expansion-control option is required. atmux derives
the user bus only from the effective uid, validates the owner-private
`/run/user/<uid>` directory and owner socket, and supplies that same fixed bus
environment to both the preflight and pane command. It does not trust bus
variables inherited by the service or tmux server. Enabling the policy on
macOS or another non-Linux host is also an error.

Zero, systemd's `u64::MAX` infinity sentinel, and a limit at or above the
smaller of host `MemTotal` and any inherited cgroup-v2 `memory.max` are
rejected: a configured value must be a real per-worker cap. `atmux doctor`
performs the same bounded, collected probe and prints the configured byte
limit. Each normal launch, saved-conversation launch, explicit Claude resume,
maintenance relaunch, and checked-in recovery path receives a unique
`atmux-tmux-spawn-*.scope`; the foreground scope runner preserves the pane's
terminal I/O while the whole descendant process tree shares the limit.

`memory_max_bytes` is always the default. `memory_override_max_bytes` is an
explicit opt-in ceiling for the New Agent memory picker; it requires a default
and may not be lower than that default. Overrides are whole GiB, greater than
zero, and are revalidated by the pane's owning node against both the current
configuration and effective host/cgroup ceiling. A federation coordinator only
forwards the requested number and cannot expand the owner's policy. The picker
offers Default, bounded presets, and a bounded custom GiB value. Older clients
omit the request and therefore continue to receive the default; older nodes
omit the capability, so the picker stays owner-managed and a new coordinator
rejects any explicit override before contacting that owner. This prevents an
older serde decoder from silently ignoring the additive request field.
Explicit caps are forwarded only through the versioned memory-launch route;
there is no fallback to the legacy launch endpoint if an owner was downgraded
after advertising support.

`atmux doctor` reports the effective advertised override ceiling, clamped to a
whole-GiB value strictly below the current host/inherited-cgroup ceiling. When
that is lower than the configured policy it labels both values; it never
presents the configured ceiling as currently accepted capacity.

Duplicate and normal saved-conversation launches carry an explicitly selected
cap. An in-place Claude resume or automatic CLI-maintenance relaunch preserves
the exact observed pane cap only while the current owner policy still permits
it. atmux deliberately does not mutate a live worker's cgroup: a changed limit
applies only to a new process generation, avoiding a runtime reduction below
current usage. The session header and API show the cap that is actually stored
on the pane.

The exact scope name and byte limit are retained in tmux pane metadata and
reported in session API summaries. `systemctl --user show <scope> -p
MemoryMax -p MemoryCurrent` can inspect a live worker. Scope arguments are
fixed argv. Every literal `$` in an agent argument is doubled using systemd's
documented portable `$$` escape, so `$FOO`, `${FOO}`, `$$`, spaces, and quotes
reach the agent literally even on releases predating
`--expand-environment=no`. The argv is shell-quoted only for tmux's required
command-string transport.

The hidden `atmux scoped-exec -- <command>...` bridge is reserved for the
owner-validated Quick Resume/boot scripts. It reloads the active configuration,
requires memory isolation to be enabled, preflights once, records scope
metadata on `TMUX_PANE`, and then replaces itself with the exact scope argv.
Tron's live `/home/ryan/resume-tron.sh` must replace its raw `send()` function
with `deploy/systemd/resume-tron-scoped-exec-block.bash` before Quick Resume is
available; atmux rejects the old script shape. Max's checked-in boot recovery
already uses the bridge for every roster entry. There is no unbounded recovery
fallback. `scoped-exec` deliberately preserves opaque launcher argv instead of
guessing what a credential wrapper does internally; the pinned Claude recovery
commands remain responsible for supplying their already-configured permission
policy exactly once.

Boot/Quick Resume roster scripts use the current configured default. They do
not retain a prior per-pane override across a host reboot because tmux metadata
is gone and the pinned recovery formats contain no separate authenticated,
durable per-pane override source. This is an intentional fail-closed fallback:
recovery never trusts browser input, stale metadata, or a rewritten command to
raise the cap. A normal saved-conversation launch can select an override again.

Choose a limit below host capacity but above the largest legitimate native or
GPU build, leaving memory for the OS, tmux, atmux, caches, and other workers.
Roll it out on one Linux owner at a time after verifying its user manager and
cgroup delegation. Keep this setting absent on Midnight/macOS.

### Owner-local CLI maintenance

Each owner can check its native Claude and Codex installations every 30
minutes and safely resume only exact idle conversations after the executable
actually changes:

```toml
[maintenance]
enabled = false # enable explicitly on each owner node
interval_minutes = 30
update_timeout_seconds = 180
relaunch_limit = 4
```

Claude uses its fixed native `claude update` command. Codex downloads the
official standalone installer from `https://chatgpt.com/codex/install.sh` with
a bounded system `curl`, then runs that owner-local installer with a bounded
system shell. atmux records the canonical executable path, reported version,
size, timestamp, and SHA-256 before and after; no pane is relaunched when the
binary digest is unchanged.

Maintenance is embedded in the owner service. A cross-process lock and atomic
state file ensure that a second local service or federation coordinator cannot
run another updater. The first delayed pass stores a launcher baseline without
updating; every later vendor invocation stores its old-pane plan and intent
before it starts. Provider background updates and a crash immediately after an
installer mutation are therefore reconciled against the last durable applied
identity on the next poll.

Relaunches are sequential and require a fresh exact
top-level empty composer, native saved-session mapping, profile/config store,
model, effort, and fast-tier mode. Every local message, model change, interrupt,
explicit resume, auto-compact, and maintenance relaunch uses the same
owner-local per-pane OS lock plus a durable tmux mutation sequence, so briefly
overlapping old/new web processes cannot race. Working, approval, unknown,
wrapper, Grok, and unmapped panes fail closed.

Every native Claude command that atmux reconstructs for a saved-conversation
launch, explicit in-place resume, or CLI-maintenance relaunch includes Claude's
`--dangerously-skip-permissions` global option. For the default
`atmux_injects` profile policy, atmux keeps one configured active copy or
inserts one immediately before the native resume selector; a same-looking value
after `--` remains literal data. Discovery does not inspect opaque executable
wrappers, so they retain the safe `atmux_injects` default. A manually configured
wrapper that provides the option and owns any `--` forwarding boundary must set
`claude_relaunch_permissions = "launcher_provides"`; a forwarding wrapper that
wants atmux to provide it may explicitly select `"atmux_injects"`. Fresh
Claude launches and every non-Claude harness keep their existing argv.

A persisted pending plan repairs partial marker writes after a crash. A Ready
marker remains deferred through transient working, approval, and unrecognized
states. Immediately before destructive respawn it becomes Claimed; a Claimed
operation is never retried after any ambiguous result, preferring a missed
relaunch over a duplicate. The plan survives atmux restart and a newer atmux
user mutation invalidates it via the durable sequence. The old raw tmux start
command is never replayed.

### Native Pulse management (implementation active)

When Pulse serving is configured, the dashboard exposes one explicit account at a time. Its
management surface shows secret-free collector health (dead, null, authentication-failed, stale,
unchanged, or healthy), bounded profile poll intervals and monthly budgets, account-wide or
per-profile collection, alerts/replies, delivery subscriptions, pricing, and receiver-token
administration.

Profile collection always runs through the existing embedded single-flight scheduler. Repeated
requests for the same account/profile coalesce, and an unknown, cross-account, reported, or remote
profile is rejected before work is queued. Profile settings never accept credential values or local
paths; leaving a budget out preserves it, sending JSON `null` clears it, and sending a number replaces
it. Alert delivery can be pull-only, a currently controllable agent pane, or a negotiated channel.
Authentication-failure alerts are never eligible for pane delivery.

Account pricing overrides have an explicit **Revert** action in REST, MCP, and the web UI. Revert
removes only that account's override, so a seeded authoritative default becomes effective again;
another account's same-named override is indistinguishable from a missing rule.

Pulse REST routes remain behind atmux's existing authentication, host, body-size, and mutation-Origin
policies. An authenticated, account-scoped, latest-only SSE stream invalidates the safe-DOM UI after
committed collector, mutation, ingest, federation, and retention changes. Reconnects always receive
an initial revision, hidden tabs close the stream, and burst events are debounced without adding a
collector or polling loop. The frozen Fable/independent-security review is still pending, so the
overall Claude Pulse merge is not yet marked complete.

The standalone legacy-import command has no simultaneous web runtime to notify. Its committed rows
are therefore picked up by the mandatory initial account refresh when the web runtime next starts;
atmux does not add a second process, IPC channel, or filesystem watcher for that case.

The default bind is loopback-only. This interface has the same practical authority as your shell.
A non-loopback bind is rejected unless it is explicitly acknowledged and configured with a
private atmux TLS CA, node certificate, and node credential:

```bash
atmux web --bind 100.64.0.10:7345 --allow-remote
```

Use the private atmux CA for federation and an authenticated reverse proxy for browser access.
atmux does not provide public-internet authentication by itself.

When the browser reaches atmux through a hostname or reverse proxy, explicitly allow that HTTP
authority and browser origin. Host validation protects pane output from DNS-rebinding attacks:

```bash
atmux web --bind 0.0.0.0:7345 --allow-remote \
  --allowed-host tron.example.ts.net:7345 \
  --allowed-origin https://tron.example.ts.net:7345
```

## LAN discovery

atmux can automatically find other atmux web nodes on the same LAN with DNS-SD/mDNS. Discovery is opt-in and authenticated: every participating node needs a unique id, the same token file, and a certificate signed by the same private atmux CA. The advertisement contains only an id, label, and private-network address; a discovered address must pass TLS validation before atmux sends its token.

Create the same `~/.config/atmux/lan.token` value on each participating machine, mode `0600`, then configure a unique node identity on each:

```toml
# ~/.config/atmux/config.toml on tron
[node]
id = "tron"
label = "Tron"
token_file = "~/.config/atmux/lan.token"

[node.tls]
cert_file = "~/.config/atmux/tls/tron.crt"
key_file = "~/.config/atmux/tls/tron.key"
ca_file = "~/.config/atmux/tls/ca.crt"

[discovery]
enabled = true
token_file = "~/.config/atmux/lan.token"

[node.tls]
cert_file = "~/.config/atmux/tls/max.crt"
key_file = "~/.config/atmux/tls/max.key"
ca_file = "~/.config/atmux/tls/ca.crt"
```

```toml
# ~/.config/atmux/config.toml on max
[node]
id = "max"
label = "Max"
token_file = "~/.config/atmux/lan.token"

[discovery]
enabled = true
token_file = "~/.config/atmux/lan.token"
```

The leaf certificate must include the host's advertised LAN IP address in its
subject alternative names and be usable for both server and client TLS. Start
the web node on each host with an all-interface IPv4 bind:

```bash
atmux web --bind 0.0.0.0:7345 --allow-remote
```

Within a few seconds, each dashboard combines all discovered sessions under machine headers and its launcher offers the discovered machine names. A departed service is removed automatically. mDNS is link-local, so it does not cross routed subnets or the public internet; use explicit configuration below for Tailscale, WireGuard, or other non-LAN links. Explicit `[[machines]]` entries take precedence over a discovered node with the same id.

## Public web gateway

`deploy/helm/atmux-web` exposes a host-run atmux instance at
`https://atmux.murphytek.com` through Keycloak's existing Google-login realm.
It is deliberately a three-hop path: Ingress → OAuth2 Proxy → a loopback-only
gateway sidecar that presents a private CA client certificate and injects a
distinct bearer credential over HTTPS. Co-locating both proxies prevents node
traffic from reaching the privileged gateway around OAuth. The host continues
to require its LAN federation token; do not reuse it for the web gateway.

Configure the additional host credential before installing the chart:

```toml
[web]
# Keep the secure default; the gateway authenticates with this credential.
allow_unauthenticated_loopback = false
proxy_token_file = "~/.config/atmux/web-proxy.token"

[node.tls]
cert_file = "~/.config/atmux/tls/tron.crt"
key_file = "~/.config/atmux/tls/tron.key"
ca_file = "~/.config/atmux/tls/ca.crt"
```

Start the host node with the public host and origin explicitly allowlisted:

```bash
atmux web --bind 0.0.0.0:7345 --allow-remote \
  --allowed-host atmux.murphytek.com \
  --allowed-origin https://atmux.murphytek.com
```

Provision the separate `atmux-web` Keycloak client in the `usage` realm before
installing, then create its three dedicated Kubernetes secrets in `murphytek`.
Keep direct-access grants disabled. Bind the client to a dedicated browser
flow with only a required Google Identity Provider Redirector—no Cookie or
Forms execution—so stripping or changing `kc_idp_hint` fails closed instead of
falling back to local realm credentials. Map Keycloak's `identity_provider`
user-session note into the ID token (and access token) for this client; OAuth2
Proxy independently requires that signed claim to equal `google`. The chart
accepts only explicitly reviewed addresses through OAuth2 Proxy’s file-based
email allowlist.
The Helm release never receives Keycloak administrator credentials. Review and
dry-run the chart before installing; none of those credentials belong in Git.

## Multiple machines

One `atmux web` process can act as a **coordinator** that aggregates the live state of other
machines running `atmux web` as **nodes**. Nothing is copied or synchronized: the coordinator
subscribes to each node's existing change-only event stream and forwards commands back to the
machine that owns the session. tmux processes never leave the machine they run on.

For a coordinator that must never act as an owner machine (for example, the
Kubernetes deployment), enable the explicit fail-closed mode:

```toml
profiles = []

[general]
project_roots = []
favorite_dirs = []
switch_on_launch = false

[node]
id = "home"
label = "Home"
coordinator_only = true

[discovery]
enabled = false

[auto_compact]
enabled = false

[maintenance]
enabled = false

[pulse]
collect = false
serve = true
receive = false
```

In this mode the node id still identifies Pulse and the federation client, but
the coordinator does not open tmux or sample local hardware. Its machine,
sessions, metrics, launch inputs, and local mutation targets are absent from
REST, SSE, and MCP. Startup rejects local profiles, project/favorite roots,
discovery, maintenance, auto-compaction, Pulse collection/receive/push, or
owner-local agent resource limits or Pulse credential references. Omitting
`coordinator_only` preserves the existing default (`false`) and all host
behavior.

```text
       browser / MCP client
                │            (only the coordinator is ever contacted)
        ┌───────┴────────┐
        │  coordinator   │  local tmux + one watcher per node
        └───┬────────┬───┘
      https │        │ https
      ┌─────┴──┐  ┌──┴─────┐
      │ node A │  │ node B │   each runs its own tmux + atmux web
      └────────┘  └────────┘
```

Add trusted machines to the coordinator's configuration. Leaving both `[[machines]]` and
`[discovery]` out keeps atmux exactly as it was: one machine, one tmux server, no outbound
connections.

```toml
[node]
# This machine's federated identity. The default "local" keeps existing ids stable.
id = "hub"
label = "Workstation"

[node.tls]
cert_file = "~/.config/atmux/tls/hub.crt"
key_file = "~/.config/atmux/tls/hub.key"
ca_file = "~/.config/atmux/tls/ca.crt"

[[machines]]
id = "gpu-box"
label = "GPU box"
url = "https://gpu-box.tail1234.ts.net:7345"
token_env = "ATMUX_GPU_BOX_TOKEN"    # or token_file = "~/.config/atmux/gpu-box.token"
```

On each node, require a token for non-loopback callers and allow the hostname the coordinator
dials:

```bash
export ATMUX_NODE_TOKEN=$(openssl rand -hex 32)   # same value the coordinator reads
atmux web --bind 100.64.0.9:7345 --allow-remote \
  --allowed-host gpu-box.tail1234.ts.net:7345
```

```toml
# node's own ~/.config/atmux/config.toml
[node]
id = "gpu-box"
token_env = "ATMUX_NODE_TOKEN"
```

The dashboard groups sessions under a header per machine showing its label, online state, agent
count, and — when a machine is unreachable — the reason and how long ago it was last seen. Output,
send, interrupt, launch, and stop all route to the owning machine. The launcher gains a machine
picker whose projects and profiles come from that machine.

### Identity

A coordinator with at least one `[[machines]]` entry emits a composite id, `machine~pane`, such as
`gpu-box~%7`. `~` is an unreserved URL character, so composite ids survive `encodeURIComponent` and
reverse proxies that normalize `%2F`. Two machines may run identically named sessions and identical
tmux pane ids without colliding.

With no `[[machines]]` configured there is nothing to disambiguate, so atmux emits the bare tmux
pane id (`%7`) exactly as it did before federation existed. Saved dashboard URLs and MCP clients that
stored a bare id keep working, and adding the first machine is the point at which emitted ids become
composite. **Both forms are always accepted on input**, on every API route and every MCP tool, so a
stored `%7` still resolves after you federate. A bare pane id or session name resolves the same way
it always did: a match on the coordinator's own tmux server wins, and an ambiguous bare id across
several remote machines is rejected with the composite id to use instead.

One edge case remains: a tmux session whose *name* happens to be shaped like `machine~pane`, where
`machine` is a configured machine id, is read as a composite reference. Avoid `~` in session names on
a federated coordinator; the launcher already rejects it.

### Bandwidth

Federation is deliberately sparse:

- one shared watcher connection per machine, no matter how many browsers or MCP clients connect;
- overview payloads carry status and a content hash, never pane output;
- pane output is fetched on demand only when the owning machine advertises a new hash, and the
  coordinator caches that fetch. Simultaneous readers of the same pane are collapsed by a per-pane
  lock, so eight browsers opening one pane at once cost one request, not eight;
- reconnect uses bounded backoff from 0.5s to a 30s ceiling, so an offline node costs at most two
  connection attempts per minute. A node that delivers a valid snapshot is healthy again, and the
  next failure starts over at 0.5s rather than at whatever ceiling an earlier outage reached.

Commands and one-off reads open a fresh TCP connection each; there is no connection pool. With one
watcher per machine and hash-gated, coalesced pane reads, the steady state is roughly one connection
per machine, so pooling has not been worth its complexity.

One offline machine never affects the local machine or any other machine. Its group is shown
offline with the failure reason, its commands fail immediately instead of hanging, and everything
else keeps streaming. A pane whose machine goes offline reports the outage once on its own stream —
not as local tmux health — and delivers a fresh snapshot when the machine answers again.

Mirrors are strict about ordering: a node's patch is applied only when it continues the exact
revision the coordinator mirrored. A patch that arrives before a snapshot, or one built on a
revision the coordinator never saw, is discarded and the watcher reconnects for a fresh snapshot
rather than merging into a gap.

### Security

- Only the coordinator is reachable from a browser. Browsers never connect to a node and never
  receive a node URL credential.
- Machines are an explicit allowlist. No request body, query string, or MCP argument can introduce
  a URL or an unconfigured machine, so the coordinator cannot be used as an open proxy.
- Node URLs are validated at startup: network federation requires `https://`; plaintext `http://`
  is limited to loopback development. URLs never accept embedded credentials, queries, fragments,
  or unsafe path prefixes.
- Credentials are referenced by `token_env` or `token_file`, never inlined in configuration. They
  are redacted from every `Debug` rendering and never appear in an API response or log line.
- Conversation views show bounded agent messages and collapsed tool calls/results. The compact
  **Show** control can independently hide Human messages or Internal tool/status activity; Agent
  prose always remains enabled, and the preference follows the browser across panes and reloads.
  Tool fields are
  rendered as text and atmux redacts common secret-bearing JSON keys, headers, assignments, bearer
  values, and private-key blocks. That redaction is defense in depth, not a guarantee: arbitrary
  command output can encode a secret in an unrecognizable form. Anyone allowed to view a session
  already has shell-equivalent access to its unredacted raw pane and agent log.
- Host and Origin protections are unchanged. A non-loopback listener requires both TLS and a
  configured node or reverse-proxy credential; every API and MCP caller must present a configured
  bearer token. Unauthenticated loopback is disabled by default because local processes are not an
  authentication boundary. A single-user development machine may explicitly opt in with
  `[web].allow_unauthenticated_loopback = true`.
- A node's event stream is deliberately not rate limited. A node is an explicitly configured,
  already-trusted peer with shell-equivalent authority over the sessions it exposes; a rate limit
  would not reduce that authority, and it would break a legitimately busy node.
- `MachineSummary.address` exposes a configured node's credential-free `host:port` so an operator can
  tell which endpoint is failing. It carries no credential and no path prefix. Anyone who can read it
  already has authenticated access to the coordinator.

### Limitations

- **Private CA required.** Federation uses `https://` with a private atmux CA and a client
  certificate. A discovered address that cannot prove a CA-signed server certificate receives no
  bearer credential. A node behind an HTTPS reverse proxy cannot be federated directly; expose it
  on the private network instead.
- **Reverse proxies.** A proxy connecting over loopback still presents its dedicated proxy bearer;
  loopback is not implicitly trusted in the default configuration.
- **One level of federation.** A coordinator federates only the sessions a node runs itself. A node
  that is also a coordinator does not re-export its own remotes.
- **Trusted configuration.** A configured machine has shell-equivalent authority over the sessions
  it exposes. Only federate machines you already trust.
- **Clock display.** "last seen" is rendered from the coordinator's clock; a badly skewed clock
  affects only that label.
- **No connection pool.** Every forwarded command and every uncached pane read opens its own TCP
  connection. Streaming is unaffected: the watcher holds one long-lived connection per machine.
- **Session names containing `~`.** On a coordinator with configured machines, a session named like
  `machine~pane` is read as a composite reference. The launcher rejects `~` in new names.

## Stateless MCP server

`atmux web` also serves MCP at <http://127.0.0.1:7345/mcp>. It implements the stateless
[MCP 2026-07-28 Streamable HTTP transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http)
using the official Rust SDK. It is intentionally modern-only: clients must use the discovery
lifecycle and send the per-request protocol metadata required by MCP 2026-07-28. There are no
server-side MCP sessions or standalone GET/DELETE streams.

| Tool | Purpose |
| --- | --- |
| `agents_list` | Read compact state for every machine, plus revision and output hashes |
| `machines_list` | Read every federated machine's online state, health, and last contact |
| `agents_observe` | Long-poll a previous revision for up to 30 seconds, for the federation or one machine |
| `agent_output` | Read a bounded tail, omitting content when its supplied hash still matches |
| `agent_send` | Paste and optionally submit a literal message to another agent |
| `agent_interrupt` | Interrupt an agent's current operation |
| `agents_launch_options` | Read allowlisted projects and profiles |
| `agent_launch` | Launch one allowlisted profile on a chosen machine |
| `agent_stop` | Terminate a tmux session |
| `pulse_read` | Read bounded, explicit-account Pulse usage, health, reports, profiles, alerts, limits, machines, and receiver metadata |
| `pulse_mutate` | Change bounded profile settings, queue account/profile collection, manage alerts/subscriptions/pricing, or administer receiver tokens without accepting raw secrets or paths |

An efficient coordinating agent should call `agents_list` once, retain its `revision` and each
`content_hash`, wait with `agents_observe`, and call `agent_output` only for sessions whose hash
changed. This keeps both network traffic and model context small.

Each session `id` is an opaque reference; pass it straight back to any tool rather than parsing it.
Tools also accept an explicit `machine` selector, so `{"id": "review", "machine": "gpu-box"}` and
`{"id": "gpu-box~%7"}` reach the same agent.

`agents_observe` has two modes. Without `machine` it observes the whole federation on the shared
revision, so one call covers every machine. With `machine` it observes only that machine, on that
machine's own revision: another machine changing never returns `changed: true`, and the `revision`
the call returns is the cursor to pass back on the next machine-scoped call. Do not mix the two
cursors. An unknown `machine` is rejected immediately rather than after the long poll. An offline
machine is reported in `machines` and fails only its own calls.

API and MCP failures are classified rather than lumped together: a caller mistake is `400`, `404`, or
`409`, a machine that is offline is `503`, and a machine that rejects a forwarded request — or whose
transport fails — is `502`. The same classification applies to pane reads and to every mutation.

For a fast tmux popup, add this to `~/.tmux.conf`:

```tmux
bind-key A display-popup -E -w 92% -h 90% "atmux"
```

Then reload tmux and press your prefix followed by `A`.

## Keyboard

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Select a session |
| `e` | Quick edit in a popup; press your tmux prefix then `d` to return |
| `Enter` / `s` | Switch or attach; press your tmux prefix then `L` to return |
| `n` | Launch a new agent |
| `/` | Filter sessions |
| `Page Up` / `Page Down` | Scroll pane preview |
| `r` | Refresh now |
| `x` | Kill a session, with confirmation |
| `?` | Help |
| `q` | Quit |

The launch wizard accepts text to filter project folders. `Enter` advances and `Esc` goes back.

Quick edit creates a temporary nested tmux client inside the popup. Detaching closes only that
client and reveals atmux again; it does not detach your outer terminal. A full switch uses tmux's
standard last-session shortcut to return.

## Configuration

The first run creates `~/.config/atmux/config.toml`. Print the exact path with:

```bash
atmux config-path
```

The default configuration is small:

```toml
[general]
project_roots = ["~/IdeaProjects", "~/work"]
favorite_dirs = ["~/Documents/notes"]
refresh_ms = 750
preview_lines = 160
switch_on_launch = true

[[profiles]]
name = "Default"
harness = "codex"
command = "codex"
args = []

[[profiles]]
name = "Review"
harness = "codex"
command = "codex"
args = ["--profile", "review"]

[[profiles]]
name = "Work account"
harness = "claude"
command = "claude-hd"
args = []
# This opaque wrapper already adds Claude's permission flag itself.
claude_relaunch_permissions = "launcher_provides"

[status]
working_markers = []
waiting_markers = []
```

Project roots are searched recursively through non-project grouping folders. A Git worktree (including linked worktrees with a `.git` file) is launchable; traversal stops there. A non-Git directory becomes launchable by adding `.atmux.toml`. Favorites are searched the same way. Profiles are grouped by harness in the wizard, so Claude and Codex are selected before a named profile.

After a successful launch, atmux writes or updates the selected project's `.atmux.toml`. It remembers the session name and selected harness/profile, while preserving other TOML keys:

```toml
session_name = "spring-ws-review"
harness = "claude"
profile = "Default"
```

On startup, atmux also discovers:

- A default Codex or Claude profile when its CLI is installed. On macOS this includes standard Homebrew and Claude desktop-app CLI locations.
- `~/.codex/<name>.config.toml` as Codex profile `<name>`.
- Executable `claude-*` wrappers in `~/.local/bin` and `~/bin` as Claude profiles.

## Agent-state detection

State detection combines the active pane's process tree, terminal title, recent output, and output changes. It recognizes the normal Codex and Claude busy indicators, prompts, and approval questions without requiring hooks.

For exact integration from a hook or wrapper, set a pane-scoped tmux option:

```bash
tmux set-option -pt "$TMUX_PANE" @atmux_status working
tmux set-option -pt "$TMUX_PANE" @atmux_status waiting
tmux set-option -pt "$TMUX_PANE" -u @atmux_status
```

Accepted values are `working`, `waiting`, `idle`, `ready`, `other`, and `off`. An explicit override wins over automatic detection.

## Native Pulse operations

Pulse is embedded in the existing `atmux web` process; it is not a second daemon. Collection,
serving, and push receiving are independently opt-in and all default to `false`:

```toml
[pulse]
collect = false
serve = false
receive = false
```

With those defaults, Pulse opens no database, starts no collector or receiver, and adds no REST,
MCP, or browser management surface. Enabling `receive` requires `serve`. REST/MCP/SSE continue to
use the web server's existing bearer, Host, body-size, and mutation-Origin boundaries.

### Accounts, profiles, and external secrets

Pulse never derives an account from a forwarded email and never invents a default account. Every
visible account and locally collectable profile must be explicit. This example enables local
Claude and Codex collection plus the authenticated management UI:

```toml
[pulse]
collect = true
serve = true
receive = false
federation_interval_seconds = 300 # accepted range: 30..86400

[pulse.schedule]
usage = 900
context = 120
tokens = 1800
gemini = 1800
retention = 3600
jitter_percent = 10
token_lookback_days = 2

[pulse.credentials]
default_refresh = "in-memory"
anthropic_inference_fallback = false
heal_config_dir = true
# Required only when a local Gemini profile is configured. These are names of
# service environment variables, not OAuth application values.
gemini_oauth_client_id_env = "ATMUX_GEMINI_OAUTH_CLIENT_ID"
gemini_oauth_client_secret_env = "ATMUX_GEMINI_OAUTH_CLIENT_SECRET"

[pulse.retention]
context_days = 1
alert_days = 180
hourly_snapshots_after_days = 7
daily_snapshots_after_days = 90

[[pulse.accounts]]
id = 1
identity = "operator@example.test"
display_name = "Operator"

[[pulse.accounts.profiles]]
name = "claude-max"
vendor = "anthropic-oauth"
config_dir = "/home/ryan/.claude-max"
poll_interval_minutes = 15
refresh = "in-memory"

[[pulse.accounts.profiles]]
name = "codex"
vendor = "openai-codex"
config_dir = "/home/ryan/.codex"
poll_interval_minutes = 15
refresh = "in-memory"
```

The retention job removes context sessions after `context_days`, removes old alert/reply state after
`alert_days`, and down-samples snapshots hourly and then daily at the configured thresholds. Token
report grains are retained; ordinary collection limits its lookback with `token_lookback_days`.

Supported vendor names are `anthropic-oauth`, `openai-codex`, `deepseek-balance`, `xai-grok`,
`gemini`, and `antigravity`. Profiles may use `api_key_env = "VARIABLE_NAME"` or
`api_key_file = "/absolute/private/path"`; reporter and database credentials follow the same
external-reference rule. Never put an API key, receiver token, PostgreSQL URL, or outer node token
directly in `config.toml`. A profile can have only one API-key reference, and relative local paths
are rejected after configuration path expansion.

Gemini collection also requires its OAuth application's client id and client secret. Supply them
to the `atmux web` service as `ATMUX_GEMINI_OAUTH_CLIENT_ID` and
`ATMUX_GEMINI_OAUTH_CLIENT_SECRET` (or choose other environment names in
`[pulse.credentials]`). The two config fields contain only those distinct environment-variable
names. There is no compiled-in fallback, and a missing or malformed value disables that collection
attempt without logging either value.

The UI can change only bounded poll intervals, monthly budgets, visibility, and pricing overrides.
It cannot receive or return credential values or local paths. Removing an account pricing override
reveals the seeded default again; it never deletes that default.

### Storage, migrations, and backup

Omitting `[pulse.database]` uses SQLite below the platform-specific atmux data directory. To select
a location explicitly, use a private absolute path whose parent is not group- or world-writable:

```toml
[pulse.database]
sqlite_path = "/home/ryan/.local/share/atmux/pulse.sqlite3"
```

For PostgreSQL, build with `pulse-postgres` and reference the connection URL through the
environment:

```toml
[pulse.database]
postgres_url_env = "ATMUX_PULSE_POSTGRES_URL"
```

Configure exactly one backend. The PostgreSQL runtime role should own or be able to migrate its
dedicated `atmux_pulse` schema, but must not be `SUPERUSER` and must not have `BYPASSRLS`; account
row-level security is forced and fail-closed when no transaction-local account is set.

Operational opens apply forward-only, transactional migrations and reject a database created by a
newer atmux. Before upgrading, stop every atmux process using that database and take a recoverable
backup. For SQLite, use SQLite's online backup command or copy the main file and any `-wal`/`-shm`
sidecars together while stopped; copying only a live main file is unsafe. For PostgreSQL, take and
verify a `pg_dump`/managed snapshot. Do not edit the migration ledger, run two atmux versions against
one database, or treat downgrade as rollback—restore the backup instead.

### Doctor, one-shot push, backfill, and legacy import

All operational commands require an explicit configured account:

```bash
atmux pulse doctor --account-id 1
atmux pulse push --once --account-id 1
atmux pulse push --once --backfill --account-id 1
```

`doctor` is read-only: it checks the current schema/integrity, persisted profile configuration,
credential references, collector preflight, gauge health, and reporter secret availability. It does
not migrate or heal the database. `push --once` runs one bounded native collection/report pass and
does not start another scheduler. `--backfill` scans bounded full token history instead of the
ordinary recent window; cancellation, truncation, or an incomplete report is a failing exit and the
command must be rerun.

Legacy Claude Pulse SQLite import is source-read-only, account-scoped, provenance-tracked, and
reconciled before success. Dry-run first, supply a source account when the legacy file contains
more than one, and externalize every inline legacy credential:

```bash
atmux pulse import /absolute/path/to/legacy.sqlite \
  --account-id 1 \
  --source-account-id 7 \
  --fallback-machine midnight \
  --credential-env claude-max=CLAUDE_MAX_API_KEY \
  --dry-run

# Repeat without --dry-run only after reviewing the JSON reconciliation plan.
```

Use `--credential-file PROFILE=/absolute/private/file` instead of `--credential-env` when needed.
Import never copies plaintext secrets or ingest-token hashes by default and refuses a non-exact
reconciliation. The import process has no live web runtime to signal; the next web startup and SSE
initial event perform the authoritative account refresh.

### Receiver token administration and push reporting

On a receiving node, set `serve = true` and `receive = true`, start the already-authenticated web
server, then open **Usage → Settings → Receiver tokens**. Create a token for one named remote
machine and copy it immediately—the plaintext is shown once and only its hash is stored. Revocation
is account-scoped and takes effect for subsequent pushes.

On the reporting machine, enable collection and reference that issued ingest token. A non-loopback
receiver additionally requires a distinct outer node/proxy credential and HTTPS:

```toml
[pulse]
collect = true
serve = false
receive = false
report_to = "https://receiver.private.example/api/v1/pulse/ingest"
report_token_file = "/run/secrets/atmux-pulse-ingest"
report_node_token_env = "ATMUX_RECEIVER_NODE_TOKEN"
```

The ingest token fixes both account and machine; payload fields cannot widen either scope. The
receiver authenticates and rate-limits before decoding a bounded request, commits a chunk
transactionally/idempotently, and emits a UI invalidation only after the commit. Plain HTTP is
accepted only on loopback. Do not reuse the ingest token as the outer node token.

### Pulse federation and live invalidation

**Usage** discovers the bounded accounts explicitly listed in `[pulse.accounts]`; the browser never
asks an operator to type or remember a database account ID. A single configured account opens
immediately. Multiple accounts appear as a display-name/identity switcher. The default Dashboard
keeps the familiar Pulse overview together—quota windows, Gemini, token/cost reports, context,
alerts, and subscriptions—while Settings retains the bounded administrative controls. If Pulse is
disabled or no account is configured, the page says so instead of guessing an identity.

When Pulse has explicit accounts and authenticated `[[machines]]` entries, the same `atmux web`
process periodically pulls each remote's local Pulse rows. It does not invent accounts or peers,
pull the local node, or accept a remote without its configured credential. No `report_to` is needed
for pull federation.

Federation uses bounded, versioned keyset cursors rather than offsets into an in-memory history.
Each durable page/outbox transition is account/peer scoped and restart-safe. Imported rows are
marked reported, stripped of local paths and credential references, and never re-exported; one
offline peer does not stop the other configured peers.

The browser subscribes to `/api/v1/pulse/accounts/{account}/events` through the same authentication
and Host policy. The stream sends a secret-free initial revision, latest-only invalidations, and
keepalives. The client performs a debounced full account refresh after a new/gapped revision, closes
the stream on account switch or while hidden, and resumes with bounded reconnect. It never launches
a collector or a second scheduler.

### Deliberate security differences from legacy Pulse

- There is no standalone Pulse HTTP daemon, pidfile, copied HTML server, or implicit account.
- All capabilities are disabled by default; forced collection uses the single embedded scheduler
  and account/profile single-flight bounds.
- REST/MCP never accept raw credentials or arbitrary config paths, and browser content is built with
  safe DOM APIs rather than `innerHTML`.
- Receiver tokens are one-time, hashed, machine/account scoped, and distinct from outer web/node
  authentication. Non-loopback push requires HTTPS.
- Cross-account object misses use the same not-found envelope. Alert acknowledgement is explicit-ID
  only, and authentication-failure alerts are never delivered into agent panes.
- Legacy import is read-only at the source and requires explicit external secret mappings.

On Midnight, Pulse collectors that need Claude OAuth must remain inside the existing Aqua
LaunchAgent security session. Never create or rebuild Midnight's tmux server over SSH, and never
kill its sessions to restart `atmux-web`; use only the established LaunchAgent kickstart procedure.

Pulse work does not create, disable, re-enable, or otherwise modify the existing authenticated
Kubernetes Ingress for `atmux.murphytek.com`. Any future Ingress change requires Ryan's separate,
explicit authorization; Pulse deployment verification must leave its current state unchanged and
must not add a new route or an authentication bypass.
The final Pulse merge status and outstanding native/review gates are tracked in
[features/claude-pulse-rust-merge.md](features/claude-pulse-rust-merge.md).

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo +1.88 check --locked
node --check web/app.js
node --test web/app.test.mjs
```

Contributions are welcome. Please keep tmux mutations explicit and preserve the zero-hook default experience.

## Source provenance

The native Pulse usage, quota, pricing, and reporting behavior is derived from the Claude Pulse
project (Apache-2.0, copyright Ryan Murphy) and reimplemented in Rust for atmux. Atmux does not embed
or require the Claude Pulse TypeScript service at runtime.

## License

MIT
