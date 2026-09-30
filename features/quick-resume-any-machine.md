# Quick Resume on any machine

Status: deployed fleet-wide 2026-09-30 UTC (with `38bc395`); rosters exist only on Tron

## Request

"We have a quick resume and I want that to work on any computer." Quick Resume was wired to Tron
only: the machine id, the script path (`/home/ryan/resume-tron.sh`), the home directory, the
sanitized `PATH`, the preflighted launcher list, the `/run/user/<uid>` lock directory, the
byte-exact `scoped-exec` bridge, and the dashboard's `machines/tron/quick-resume` calls were all
constants.

## Acceptance criteria

- Every owner node offers Quick Resume from its own configured roster script (`[recovery].script`,
  default `quick-resume.sh` beside `config.toml`) with no machine id special-cased anywhere.
- The script safety model is unchanged: owner-only, non-writable ancestry, no symlink, size cap,
  idempotency marker, byte-exact transactional helper block, no browser-supplied path or command,
  no output exposure, single flight, three-minute timeout, process-group cleanup.
- The launch bridge follows the node's memory policy: a memory-isolated node accepts only the
  `scoped-exec` bridge pointing at a trusted atmux executable and the daemon's own configuration;
  any other node (macOS included) accepts only the new direct bridge. Neither is accepted on the
  wrong kind of node.
- The runtime environment, `PATH`, and lock directory derive from the daemon's identity and
  platform instead of Tron's paths.
- The dashboard reads every machine through one coordinator endpoint and lists each answering
  owner with its own message, enabling the action only where the owner reports it available.
- Tron's live roster keeps validating without edits once its config names the script.

## Gates

- [x] Implementation (`src/recovery.rs`, `src/config.rs`, `src/control.rs`, `src/web.rs`,
  `web/`, `deploy/quick-resume/`)
- [x] Focused Rust and dashboard tests
- [x] Live runtime test on Tron and Max after rollout (Clue and Midnight pending)
- [ ] Fable/Claude Max review
- [ ] Independent security review

## Verification evidence

- Rust unit tests cover: the direct and scoped bridges validating only on their own kind of
  node; the scoped bridge rejecting a foreign executable, a foreign configuration path, extra
  arguments, a commented or conditional block, a raw `exec $2` launch, and a duplicated block,
  while accepting an absent or re-valued `atmux-web` service cap; the shipped example roster
  validating as-is; the fixed identity environment (`HOME`, `USER`, `LOGNAME`, `PATH`) reaching
  the script and nothing else; pathless unavailability messages; the production runner following
  `[recovery]`, the memory policy, and `required_commands`.
- Route tests cover `/api/v1/fleet/quick-resume` (one entry per machine, no paths in the body)
  and the existing owner/origin protections on the per-machine start route.
- Dashboard tests cover the per-machine dialog model and assert that no machine id is
  special-cased and nothing about a script crosses the wire.
- The Tron shell tests (`tests/resume_tron_scoped_exec.sh`, `tests/resume_tron_transactional.sh`)
  still pass against the unchanged canonical fixture.

## Deployment — 2026-09-29

Runtime commit `3e6a7a1`, release build `33e4b692…` (x86_64, LTO), embedding the committed
dashboard assets.

| Machine | Artifact | Restart | Result |
| --- | --- | --- | --- |
| Tron | local release build | `atmux-web:0.0` respawned in its original `scoped-exec` 56 GiB scope | reports its real `/home/ryan/resume-tron.sh` available under the scoped bridge with no script edit |
| Max | Tron's binary, checksum-verified | user `atmux-web.service` (`KillMode=process`) | reports "roster script is not installed" |

- Only web services restarted. Tron's tmux server kept pid 11243 and Max's kept pid 25728; every
  pre-existing agent pane id, pid, and command is unchanged on both.
- `/api/v1/fleet/quick-resume` on Tron lists all three online owners with their own documents;
  Midnight still runs the previous binary and answers with its old Tron-only message until it is
  redeployed.
- Rollback copies: Tron `target/release/atmux.rollback-8d5cd01` (the running pre-rollout binary,
  `37a04f7a…`), Max `target/release/atmux.rollback-8d5cd01-pre-3e6a7a1`.
- Not deployed: Clue (needs its native ARM64 build) and Midnight, which had rebooted at 18:41 MDT
  and was being brought back up by hand during this work; its web service was restarted from the
  LaunchAgent at 19:27 on the previous binary.
- The full parallel `cargo test --lib` run on Tron intermittently fails one self-update test with
  `Text file busy`; the same flake reproduces at the previous commit and the tests pass in
  isolation, so it is unrelated to this change.

## Rollout notes

- Tron: `[recovery] script = "/home/ryan/resume-tron.sh"` plus its launcher list in
  `required_commands`; the live script needs no edit.
- Max, Clue, Midnight: write a roster from `deploy/quick-resume/quick-resume.example.sh`. Max's
  boot roster (`deploy/systemd/resume-max-at-boot`) is a different, boot-only format and is not
  reused. Midnight and Clue take the direct bridge; Max takes the scoped bridge.

## Fleet completion — 2026-09-30 (UTC)

Midnight, Clue, and the coordinator received this change as part of the `38bc395` rollout (see
`conversation-mapping-compaction-raw-fit.md`). The public dashboard now reads
`/api/v1/fleet/quick-resume`. Max, Midnight, and Clue still have no roster script, so they report
"roster script is not installed" until one is written from `deploy/quick-resume/`.
