# Feature request workflow

This directory is the durable record of Ryan's atmux requests. Active work stays directly under
`features/`. A record may move to `features/completed/` only after all four gates are checked:

1. implementation is present in the shared worktree;
2. focused unit/browser tests pass;
3. an integration or live runtime test passes on every affected platform;
4. Fable/Claude Max and an independent security reviewer approve the frozen snapshot.

Each record names its evidence. A feature that is implemented but missing any gate remains active.
Public ingress is a separate authorization gate and must remain absent until Ryan explicitly asks
to enable it after the authentication matrix succeeds.

The request index is in [requests.md](requests.md).
