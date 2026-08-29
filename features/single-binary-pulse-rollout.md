# Single-binary Pulse rollout

Status: implementation active

## Request

Every box must collect and report provider limits, usage, token/cost, context, and related Pulse
data from its existing `atmux` binary. Ryan must not run or manage a separate Claude Pulse server or
agent process.

## Acceptance

- Tron, Midnight, and Max each run one `atmux web` process that owns local Pulse collection,
  persistence, retention, alerts, and authenticated Pulse APIs.
- Each host uses explicit account `4` (`ryanmurf@gmail.com`) and explicit secret-free local profile
  configuration; credentials remain external references or native CLI stores.
- Tron's authenticated Usage dashboard discovers the account automatically and shows local plus
  federated machine data without a numeric account prompt.
- Pull federation uses the existing authenticated machine connections; Pulse does not add or modify
  public Ingress routes.
- Legacy Node `claude-pulse` processes are retired only after Rust collection and federation are
  verified, with rollback instructions retained.
- No second Pulse scheduler, server, receiver, or sidecar is introduced.

## Completion gate

- [x] Implemented on all three hosts
- [x] Unit tested
- [x] Integration tested
- [x] Native Linux and macOS acceptance tested
- [ ] Two independent security reviews complete
- [ ] Legacy Pulse processes retired after parity verification

## Current status

The Rust binary embeds the scheduler, native collectors, store, reporting API, alerts, retention,
SSE, and authenticated pull federation. Tron, Max, and Midnight now run `collect = true`,
`serve = true`, and `receive = false` for explicit account 4 profiles. Tron's authenticated API
discovers the account without a numeric prompt and currently returns seven profiles, five quota
windows, five pace rows, nine context rows, four Gemini rows, three machines, and non-empty priced
token reports. Max and Midnight expose the same account and federated three-machine view from their
single native `atmux web` processes. The Linux and native arm64 artifacts have completed live
acceptance, including the Aqua-only Midnight restart without replacing its tmux server. Public
Ingress was not modified and still terminates at the existing authenticated proxy.

Legacy Node Pulse remains running only as a rollback/history source during the soak; it is not in
the Rust collection, reporting, API, or federation path. Historical import/backfill and final
legacy retirement remain deferred until the known v7 restart/outbox contract receives its v8
hardening and parity review.
