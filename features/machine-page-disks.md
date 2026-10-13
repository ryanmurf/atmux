# Machine page: disks

Status: deployed fleet-wide 2026-10-13 (runtime `23e66ae`, Helm rev 44).

## Why

The machine page showed CPU, memory, processes, GPUs, and temperatures, but not disks. A disk
filling up was invisible until something failed to write. On Tron, `/mnt/steam-500` was already
at 90% and the root filesystem at 84%.

## What changed

- **Collection.** Owners report each mounted disk once (`src/metrics/disk.rs`), as an optional
  `disks` list in the machine metrics, so older nodes and coordinators keep working. Each entry
  carries the mount point, filesystem label or volume name, kernel device, drive model, filesystem
  type, size, used and available space, and read and write rates.
- **Linux.** The owner reads `/proc/self/mountinfo` and keeps one mount per filesystem, keyed by
  `major:minor`. A mount of the whole filesystem wins over a bind of a subdirectory, then the
  shortest path. Only block-device sources count, so overlay, tmpfs, NFS, and FUSE mounts drop
  out, along with squashfs and other image filesystems (snaps) and mounts under `/proc`, `/sys`,
  `/dev`, and `/run` (except `/run/media`). That's why Tron's 533 mounts come down to its five real
  filesystems. Space comes from `statvfs`, and the percentage uses `df`'s formula (used over used
  plus available), so blocks ext4 reserves for root don't read as free. `df` rounds up, so the
  page can show one point less. Rates come from `/proc/diskstats`. Drive models
  come from udev, because sysfs cuts SATA models to 16 characters, with sysfs as the fallback; LVM
  and LUKS volumes are followed to their first underlying disk.
- **macOS.** The owner uses `sysinfo`'s visible, local volumes and their Finder capacities.
  Volumes on one APFS container report the container's capacity, so `/` and
  `/System/Volumes/Data` fold into one entry under `/` with its volume name.
- **No stalls.** Collection runs on its own short-lived thread at most every 5 s, and the
  refresh loop only reads the latest result. `statvfs` on a wedged device can block
  indefinitely; if that happens, the disk list stops updating and sessions keep refreshing.
- **The page.** A Disk tile joins the top row for the root filesystem. A full-width Disks panel
  sits above Temperatures with one block per filesystem, root first, then largest. Each block
  shows the mount point, then the label, model, and filesystem, then a meter, used of total, free
  space, and current read and write rates. The block and its meter turn amber at 85% full and red
  at 95%, the same thresholds as the other meters. The tooltip adds the kernel device.

ZFS datasets aren't listed. Each dataset reports pool-wide free space against its own usage, so
they need pool-level handling. None of the four machines uses ZFS today.

## Gate

fmt and zero-warning clippy (pedantic); `cargo +1.88 check --locked`. New tests:

- `metrics::disk` parses real Tron `mountinfo` lines. Bind mounts fold into `/`, and virtual,
  image, network, and runtime mounts drop out. A bind mount stands in when its filesystem isn't
  mounted whole. The tests also cover octal and udev escapes, the udev model record, diskstats
  sector counters, rates from the previous counters (none for a new disk, a counter that went
  backwards, or too short a window), root-first ordering, APFS container folding, and the wire
  sanitizer. A live test checks that Tron's own list has one entry per filesystem.
- A JS test covers the disk rows: `df`'s percentage, the joined names, and malformed volumes.

The full Rust suite passed (978 tests) on a disposable `ATMUX_TMUX_SOCKET_NAME`, and 213 JS tests
passed. A dev `atmux web` on Tron reported all five filesystems with the same sizes as
`df -h`, and headless Chrome rendered the tile and panel at 1440 and 390 px with no horizontal
scroll. The macOS path can't be cross-checked from Linux (SQLite's C build needs Apple's
toolchain), so it was first compiled on Midnight, with no new warnings.

## Deployment (2026-10-13)

| Machine | Artifact SHA-256 prefix | Rollback |
| --- | --- | --- |
| Tron | `9cd25f9eeb0338d9` | `target/release/atmux.rollback-38682a6` |
| Max | `9cd25f9eeb0338d9` (Tron's build) | `target/release/atmux.rollback-38682a6` |
| Midnight | `90d7a2763e606f84` (built on Midnight, 4 jobs) | `target/release/atmux.rollback-38682a6` |
| Clue | `d3dcafd0aaf5758c` (aarch64, built on Clue) | `~/.local/bin/atmux.rollback-38682a6` |
| Coordinator | `localhost:32000/atmux:23e66aeb8b1d@sha256:372f2a02…` | Helm rev 43 (`38682a6`) |

Only `server.image` changed in Helm; the values are in
`/mnt/data/herodevs-agents/atmux-rollout-disks-3Pu46a` on Tron. Tron's web was respawned with
`remain-on-exit` on, so a failed start would have left a dead pane rather than ending the
`atmux-web` session; it came up on the first try and the option was cleared. Midnight restarted
through the LaunchAgent kickstart after 16 ptys were opened and closed.

Session counts held: Tron 15 tmux sessions, Max 6, Midnight 18, Clue none (as before). Through
the coordinator, all four machines are online and report their disks:

- Tron: `/` 84%, `/mnt/data` 2%, `/mnt/files` 55%, `/mnt/steam-500` 90% (amber), `/boot/efi` 1%.
- Max: `/` 46%, `/mnt/data` 64%, `/boot/efi` 1%.
- Midnight: `/` (Macintosh HD, 994.7 GB APFS container) 81%, one entry for the container.
- Clue: `/` 22%, `/boot` 22%, `/boot/efi` 6% on the cloud provider's `BlockVolume`.
