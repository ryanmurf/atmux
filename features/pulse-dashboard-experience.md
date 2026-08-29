# Pulse dashboard experience

Status: implementation active

## Request

Usage should open as a useful Pulse-style dashboard. Ryan must not need to know or type an
internal numeric account ID.

## Acceptance

- The authenticated server exposes only the bounded Pulse accounts explicitly configured for
  this atmux runtime, with secret-free display metadata.
- Usage discovers those accounts automatically and selects the sole account without prompting.
- Multiple configured accounts use a labeled account switcher; raw account IDs are secondary
  metadata rather than the interaction model.
- The default dashboard presents usage quotas, Gemini quota, token/cost reporting, context,
  alerts, and subscriptions together in the familiar Pulse overview.
- Empty, disabled, loading, and failed states explain what is missing without asking for an ID.
- Existing account-scoped authorization, pagination, mutation Origin checks, and settings remain
  unchanged.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Integration tested
- [x] Independently reviewed
- [ ] Native/legacy usage data rollout verified

## Live status

The authenticated Tron runtime now serves the sole configured account as `Ryan` and the browser
selects it automatically. The dashboard shell, reports, quotas, context, Gemini, alerts, and
subscriptions are live. Collection, receiver ingest, and legacy import remain disabled, so the live
account is intentionally empty until the separate reviewed data rollout is complete. The private
atmux Pulse database uses a `0700` directory and `0600` file. Two independent read-only reviews
returned SAFE on 2026-08-09.
