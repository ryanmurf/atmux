# Persistent Max web service

Status: implementation active; runtime restored

## Request

Restore atmux on Max and keep it running without exposing an `atmux-web` tmux session in the user
session list.

## Evidence

- [x] Installed the checked-in `deploy/systemd/atmux-web-max.service` as Ryan's user service.
- [x] Enabled and started `atmux-web.service`; systemd reports it active.
- [x] Enabled user lingering so the service survives logout and starts without an interactive login.
- [x] Verified Max's mutual-TLS `/api/v1/health` response is healthy on `192.168.0.124:7345`.
- [x] Verified the only visible tmux session is Ryan's existing `ds-speed` session.
- [x] Replaced Max's stale embedded web bundle with the current reviewed binary after its local UI
  showed zero agents while the API correctly reported one. The service-only restart preserved
  `ds-speed %0 codex`; the refreshed local API and UI bundle now report that one Max agent.
- [x] Set the Max web unit to `KillMode=process`. A default tmux server first created by a web
  launch inherits the web service cgroup; systemd's default `control-group` stop therefore killed
  that otherwise independent server and closed every pane during a later web-only deploy.
  Web restarts now signal only the direct atmux process. The tmux server and agent panes keep their
  independent lifecycle even when their original cgroup ancestry came from atmux.
- [x] Checked in the exact nine-session Max boot roster and its `atmux-max-resume.service` unit.
  Recovery verifies native conversation IDs plus exact session name, cwd, harness, configured
  profile, model, effort, and mode. A same-boot marker is trusted only while that complete roster
  still validates; missing sessions are retried transactionally and a failed attempt rolls back
  only sessions it created. The recovery unit also uses `KillMode=process` because its first tmux
  launch can own the daemonized default server's cgroup ancestry.
- [x] Regression tested full-roster creation, a verified marker no-op, same-boot missing-session
  repair, preflight collision rejection, and partial-failure rollback that preserves a valid
  pre-existing session.
- [x] Aligned boot recovery with the current Claude-Qwen split backend: the 8092 proxy fronts the
  8091 router, with XTX prefill on 8191 and Halo decode on 8192. These readiness-gated services are
  `Wants` plus `After`, not `Requires`, so a backend restart cannot asynchronously cancel recovery;
  bounded pre-launch, post-Qwen, and pre-marker health gates fail closed and trigger transactional
  rollback. The obsolete conflicting monolithic MTP service is not a recovery dependency.
- [x] Made Max's existing single-user browser trust explicit with
  `web.allow_unauthenticated_loopback = true`; LAN access remains protected by mTLS and the node
  bearer (missing bearer returns 401, mTLS plus bearer returns 200).
- [ ] Include the unit and runtime behavior in the final Fable/Claude Max and independent security
  reviews before moving this record to `features/completed/`.

No Midnight process/session and no Kubernetes Ingress was changed.

For a Max web-only deploy, replace the direct binary, run `systemctl --user daemon-reload`, then use
`systemctl --user restart atmux-web.service`. Do not wrap the web daemon in tmux. The checked-in
unit's `KillMode=process` is non-negotiable: removing it makes a web restart capable of killing the
default tmux server and every pane again.
