# Machine page: CPU, memory composition, and processes

Status: deployed fleet-wide 2026-10-06 (runtime `4e9dd72`).

## Why

The machine page showed CPU and memory as two large single-number cards. When Midnight ran short
of memory there was no way to see where it went: 41 GiB of its 64 GiB is wired memory held by the
local Qwen server's Metal allocations, and the page reported only "used".

## What changed

Owners report more, all optional on the wire, so mixed-version nodes keep working:

- CPU model, core and thread counts, mean clock, load averages, and per-core utilization.
- Swap, available memory, and a composition of physical memory. Linux reads `/proc/meminfo`
  (apps, kernel, shared, cache, free). macOS runs `vm_stat` every 5 s and uses Activity
  Monitor's categories (apps, wired, compressed, cache, free). Slices always sum to physical
  memory.
- The top process groups by name: ten by memory plus up to five CPU-heavy groups the memory list
  would hide. Sampling runs every 15 s; it costs about 130 ms for 4,900 processes on Tron.

The page leads with compact stat tiles, then pairs CPU with Memory and Processes with System and
Graphics, and ends with a sensor grid sorted hottest first. Memory slice colors passed the dataviz
palette validator against `--panel`. Shared memory takes the aqua slot because magenta next to
orange failed the normal-vision floor. One tooltip follows a stable key, so the 750 ms live
re-render doesn't drop it, and open GPU details stay open.

## Deployment (2026-10-06)

Gate: fmt and zero-warning clippy; full Rust suite passed (federation needs a disposable
`ATMUX_TMUX_SOCKET_NAME=atmux-test-…`); 203 JS tests passed; browser suites passed serially.
`mobile browser Back stays inside atmux…` fails on some first runs with "Inspected target
navigated or closed", on the unchanged base commit `2eaa31d` too, and passes on rerun.

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Tron | `8ef078872a936028` | `target/release/atmux.rollback-2eaa31d` |
| Max | `8ef078872a936028` (Tron's build) | `target/release/atmux.rollback-2eaa31d` |
| Midnight | `d92197afd8320773` (built on Midnight, 4 jobs) | `target/release/atmux.rollback-2eaa31d` |
| Clue | `84f5c2fe576581ce` (aarch64, built on Clue) | `~/.local/bin/atmux.rollback-2eaa31d` |
| Coordinator | `localhost:32000/atmux:4e9dd7222c64@sha256:a67c0358…` | Helm rev 38; rev 36 is `2eaa31d` |

Helm values changed only `server.image`; saved values live in
`/mnt/data/herodevs-agents/atmux-rollout-d524e8e-UNLWy9` on Tron. All four machines report CPU
detail, five memory slices, and process groups through the coordinator. Midnight kept its 9 tmux
sessions across both `launchctl kickstart` restarts.
