//! Mounted disks for the machine view.
//!
//! Linux reads `/proc/self/mountinfo` and keeps one entry per block-device
//! filesystem, so bind mounts (kubelet volume subpaths, snap host mounts) and
//! overlay, tmpfs, and squashfs mounts never show up as extra disks. macOS uses
//! `sysinfo`'s volume list, which already skips hidden system volumes, and
//! folds volumes that share an APFS container into the one Finder shows.
//!
//! Collection runs on a short-lived thread at most every
//! [`DISK_SAMPLE_INTERVAL`]. The caller is the session refresh loop, and
//! `statvfs` on a wedged device can block indefinitely, so a stuck collection
//! leaves the disk list stale instead of stopping session updates.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::bounded_text;

pub(super) const DISK_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_DISKS: usize = 32;
const MAX_MOUNT_POINT_BYTES: usize = 256;
const MAX_DISK_TEXT_BYTES: usize = 96;
/// Rates need at least this much time between counter readings.
const MIN_RATE_WINDOW_MS: u64 = 500;
#[cfg(any(target_os = "linux", test))]
const SECTOR_BYTES: u64 = 512;
#[cfg(target_os = "linux")]
const MAX_MOUNTINFO_BYTES: usize = 4 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_DISKSTATS_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_LABELS: usize = 128;

/// One mounted filesystem.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DiskVolume {
    /// Where the filesystem is mounted, such as `/` or `/mnt/data`.
    pub mount_point: String,
    /// Filesystem label, or the volume name on macOS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Kernel device name, such as `nvme0n1p2` or `disk3s1s1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Drive model when the operating system reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_system: Option<String>,
    pub total_bytes: u64,
    /// Space in use, as `df` reports it. On Linux, used plus available can
    /// fall short of the total by the blocks reserved for root.
    pub used_bytes: u64,
    /// Space an unprivileged process can still write.
    pub available_bytes: u64,
    /// Device reads averaged over the last sample interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bytes_per_second: Option<u64>,
    /// Device writes averaged over the last sample interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bytes_per_second: Option<u64>,
}

impl DiskVolume {
    fn sanitized(mut self) -> Option<Self> {
        self.mount_point = bounded_text(&self.mount_point, MAX_MOUNT_POINT_BYTES);
        if self.mount_point.is_empty() || self.total_bytes == 0 {
            return None;
        }
        let bounded = |value: Option<String>| {
            value
                .map(|value| bounded_text(&value, MAX_DISK_TEXT_BYTES))
                .filter(|value| !value.is_empty())
        };
        self.label = bounded(self.label);
        self.device = bounded(self.device);
        self.model = bounded(self.model);
        self.file_system = bounded(self.file_system);
        self.used_bytes = self.used_bytes.min(self.total_bytes);
        self.available_bytes = self.available_bytes.min(self.total_bytes);
        Some(self)
    }
}

pub(super) fn sanitize_disks(disks: Vec<DiskVolume>) -> Vec<DiskVolume> {
    disks
        .into_iter()
        .take(MAX_DISKS)
        .filter_map(DiskVolume::sanitized)
        .collect()
}

/// Cumulative bytes a device has read and written since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct IoCounters {
    read: u64,
    written: u64,
}

/// A filesystem as collected, before rates and ordering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RawVolume {
    /// Identifies one filesystem across its mounts and between samples.
    key: String,
    mount_point: String,
    label: Option<String>,
    device: Option<String>,
    model: Option<String>,
    file_system: Option<String>,
    total: u64,
    used: u64,
    available: u64,
    io: Option<IoCounters>,
}

/// Runs disk collection off the caller's thread and keeps the latest result.
#[derive(Debug, Default)]
pub(super) struct DiskSampler {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Debug, Default)]
struct Shared {
    volumes: Vec<DiskVolume>,
    counters: HashMap<String, IoCounters>,
    collected_at: Option<Instant>,
    started_at: Option<Instant>,
    running: bool,
}

/// Clears `running` when a collection ends, even if it panicked.
struct Running(Arc<Mutex<Shared>>);

impl Drop for Running {
    fn drop(&mut self) {
        lock(&self.0).running = false;
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DiskSampler {
    /// Returns the latest finished collection and starts the next one when
    /// it is due. The first calls return nothing until a collection lands.
    pub(super) fn sample(&self) -> Vec<DiskVolume> {
        let mut shared = lock(&self.shared);
        let due = !shared.running
            && shared
                .started_at
                .is_none_or(|started_at| started_at.elapsed() >= DISK_SAMPLE_INTERVAL);
        if due {
            shared.running = true;
            shared.started_at = Some(Instant::now());
            let worker = Arc::clone(&self.shared);
            let spawned = thread::Builder::new()
                .name("atmux-disks".to_owned())
                .spawn(move || {
                    let running = Running(worker);
                    let (volumes, read_at) = collect();
                    let mut shared = lock(&running.0);
                    let window = shared
                        .collected_at
                        .map(|collected_at| read_at.saturating_duration_since(collected_at));
                    let (volumes, counters) = finish(volumes, &shared.counters, window);
                    shared.volumes = volumes;
                    shared.counters = counters;
                    shared.collected_at = Some(read_at);
                });
            if spawned.is_err() {
                shared.running = false;
            }
        }
        shared.volumes.clone()
    }
}

/// Adds read and write rates from the previous counters, then orders the
/// list: the root filesystem first, then the largest.
fn finish(
    volumes: Vec<RawVolume>,
    previous: &HashMap<String, IoCounters>,
    window: Option<Duration>,
) -> (Vec<DiskVolume>, HashMap<String, IoCounters>) {
    let window_ms = window
        .and_then(|window| u64::try_from(window.as_millis()).ok())
        .filter(|ms| *ms >= MIN_RATE_WINDOW_MS);
    let mut counters = HashMap::new();
    let mut disks: Vec<DiskVolume> = volumes
        .into_iter()
        .map(|volume| {
            let before = previous.get(&volume.key);
            let rate = |current: u64, before: u64| {
                let delta = current.checked_sub(before)?;
                Some(delta.saturating_mul(1000) / window_ms?)
            };
            let (read, write) = match (volume.io, before) {
                (Some(now), Some(before)) => (
                    rate(now.read, before.read),
                    rate(now.written, before.written),
                ),
                _ => (None, None),
            };
            if let Some(io) = volume.io {
                counters.insert(volume.key, io);
            }
            DiskVolume {
                mount_point: volume.mount_point,
                label: volume.label,
                device: volume.device,
                model: volume.model,
                file_system: volume.file_system,
                total_bytes: volume.total,
                used_bytes: volume.used,
                available_bytes: volume.available,
                read_bytes_per_second: read,
                write_bytes_per_second: write,
            }
        })
        .collect();
    disks.sort_by(|left, right| {
        (right.mount_point == "/")
            .cmp(&(left.mount_point == "/"))
            .then_with(|| right.total_bytes.cmp(&left.total_bytes))
            .then_with(|| left.mount_point.cmp(&right.mount_point))
    });
    (sanitize_disks(disks), counters)
}

/// Keeps one mount per filesystem: one that mounts the whole filesystem
/// rather than a bound subdirectory, then the shortest path.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn distinct<T>(items: Vec<T>, describe: impl Fn(&T) -> (&str, bool, &str)) -> Vec<T> {
    let mut chosen: Vec<T> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for item in items {
        let (key, whole, mount_point) = describe(&item);
        let Some(&at) = index.get(key) else {
            index.insert(key.to_owned(), chosen.len());
            chosen.push(item);
            continue;
        };
        let (_, kept_whole, kept_mount) = describe(&chosen[at]);
        let better = (
            whole,
            std::cmp::Reverse(mount_point.len()),
            std::cmp::Reverse(mount_point),
        ) > (
            kept_whole,
            std::cmp::Reverse(kept_mount.len()),
            std::cmp::Reverse(kept_mount),
        );
        if better {
            chosen[at] = item;
        }
    }
    chosen
}

/// One line of `/proc/self/mountinfo` that names a block-device filesystem.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct MountEntry {
    /// `major:minor`, shared by every mount of one filesystem.
    key: String,
    mount_point: String,
    /// The mount's root is the filesystem's root, not a bound subdirectory.
    whole: bool,
    file_system: String,
    source: String,
}

/// Block-device filesystems, one entry per filesystem.
#[cfg(any(target_os = "linux", test))]
fn mount_entries(mountinfo: &str) -> Vec<MountEntry> {
    let mut entries = Vec::new();
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        let (Some(_), Some(_), Some(key), Some(root), Some(mount_point)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            continue;
        };
        // Mount options and optional fields run until the "-" separator.
        let mut rest = fields.skip_while(|field| *field != "-").skip(1);
        let (Some(file_system), Some(source)) = (rest.next(), rest.next()) else {
            continue;
        };
        let mount_point = decode_mount_field(mount_point);
        let source = decode_mount_field(source);
        if !is_disk_mount(file_system, &source, &mount_point) {
            continue;
        }
        entries.push(MountEntry {
            key: key.to_owned(),
            mount_point,
            whole: root == "/",
            file_system: file_system.to_owned(),
            source,
        });
    }
    distinct(entries, |entry| {
        (entry.key.as_str(), entry.whole, entry.mount_point.as_str())
    })
}

/// Real disks only: the source is a block device, the filesystem isn't a
/// read-only image (snaps, ISOs), and the mount isn't kernel or runtime state.
#[cfg(any(target_os = "linux", test))]
fn is_disk_mount(file_system: &str, source: &str, mount_point: &str) -> bool {
    const IMAGE_FILE_SYSTEMS: [&str; 5] = ["squashfs", "erofs", "iso9660", "udf", "cramfs"];
    const SYSTEM_PATHS: [&str; 4] = ["/proc", "/sys", "/dev", "/run"];
    let under = |root: &str| {
        mount_point == root
            || mount_point
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let system = SYSTEM_PATHS.iter().any(|root| under(root)) && !under("/run/media");
    source.starts_with("/dev/") && !IMAGE_FILE_SYSTEMS.contains(&file_system) && !system
}

/// Undoes the kernel's octal escapes (`\040` for a space) in a mount field.
#[cfg(any(target_os = "linux", test))]
fn decode_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escape = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        if let Some(digits) = escape {
            let value = digits
                .iter()
                .fold(0_u32, |value, digit| value * 8 + u32::from(digit - b'0'));
            if let Ok(byte) = u8::try_from(value) {
                decoded.push(byte);
                at += 4;
                continue;
            }
        }
        decoded.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Undoes udev's `\x20`-style escapes in a `/dev/disk/by-label` name.
#[cfg(any(target_os = "linux", test))]
fn decode_label(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' && bytes.get(at + 1) == Some(&b'x') {
            let byte = bytes
                .get(at + 2..at + 4)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = byte {
                decoded.push(byte);
                at += 4;
                continue;
            }
        }
        decoded.push(bytes[at]);
        at += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// `ID_MODEL_ENC` from a udev database record, with its escapes undone.
#[cfg(any(target_os = "linux", test))]
fn udev_model(record: &str) -> Option<String> {
    record
        .lines()
        .find_map(|line| line.strip_prefix("E:ID_MODEL_ENC="))
        .map(|model| decode_label(model).trim().to_owned())
        .filter(|model| !model.is_empty())
}

/// Cumulative counters per device from `/proc/diskstats`.
#[cfg(any(target_os = "linux", test))]
fn diskstats(input: &str) -> HashMap<String, IoCounters> {
    input
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let sectors = |at: usize| fields.get(at)?.parse::<u64>().ok();
            Some((
                (*fields.get(2)?).to_owned(),
                IoCounters {
                    read: sectors(5)?.saturating_mul(SECTOR_BYTES),
                    written: sectors(9)?.saturating_mul(SECTOR_BYTES),
                },
            ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn collect() -> (Vec<RawVolume>, Instant) {
    use std::path::{Path, PathBuf};

    let read = |path: &str, limit: usize| {
        let file = std::fs::File::open(path).ok()?;
        let (mut bytes, truncated) = super::read_bounded(file, limit).ok()?;
        if truncated {
            // Keep whole lines; the mounts past the limit are dropped.
            let end = bytes.iter().rposition(|byte| *byte == b'\n').unwrap_or(0);
            bytes.truncate(end);
        }
        String::from_utf8(bytes).ok()
    };
    let mounts = read("/proc/self/mountinfo", MAX_MOUNTINFO_BYTES)
        .map(|input| mount_entries(&input))
        .unwrap_or_default();
    let labels: HashMap<PathBuf, String> = std::fs::read_dir("/dev/disk/by-label")
        .into_iter()
        .flatten()
        .take(MAX_LABELS)
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let label = decode_label(&entry.file_name().to_string_lossy());
            Some((std::fs::canonicalize(entry.path()).ok()?, label))
        })
        .collect();
    let counters = read("/proc/diskstats", MAX_DISKSTATS_BYTES)
        .map(|input| diskstats(&input))
        .unwrap_or_default();
    let read_at = Instant::now();
    let volumes = mounts
        .into_iter()
        .filter_map(|mount| {
            let stat = rustix::fs::statvfs(mount.mount_point.as_str()).ok()?;
            let total = stat.f_blocks.saturating_mul(stat.f_frsize);
            if total == 0 {
                return None;
            }
            let free = stat.f_bfree.saturating_mul(stat.f_frsize);
            let available = stat.f_bavail.saturating_mul(stat.f_frsize);
            let source = std::fs::canonicalize(&mount.source).ok();
            let block = std::fs::canonicalize(Path::new("/sys/dev/block").join(&mount.key)).ok();
            let device = block
                .as_deref()
                .or(source.as_deref())
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned());
            Some(RawVolume {
                io: device.as_ref().and_then(|name| counters.get(name)).copied(),
                label: source.as_ref().and_then(|path| labels.get(path)).cloned(),
                model: block.as_deref().and_then(|block| drive_model(block, 0)),
                device,
                key: mount.key,
                mount_point: mount.mount_point,
                file_system: Some(mount.file_system),
                total,
                used: total.saturating_sub(free),
                available,
            })
        })
        .collect();
    (volumes, read_at)
}

/// The model of the drive under a sysfs block device. A partition's model
/// lives on its parent disk; device-mapper volumes (LVM, LUKS) are followed
/// through their first underlying device. udev's copy comes first because
/// sysfs cuts SATA models to 16 characters ("Samsung SSD 850").
#[cfg(target_os = "linux")]
fn drive_model(block: &std::path::Path, depth: usize) -> Option<String> {
    let disk = if block.join("partition").exists() {
        block.parent()?
    } else {
        block
    };
    let udev = super::read_bounded_file(&disk.join("dev")).and_then(|dev| {
        super::read_bounded_file(&std::path::Path::new("/run/udev/data").join(format!("b{dev}")))
    });
    let model = udev
        .as_deref()
        .and_then(udev_model)
        .or_else(|| super::read_bounded_file(&disk.join("device/model")))
        .map(|model| bounded_text(&model, MAX_DISK_TEXT_BYTES))
        .filter(|model| !model.is_empty());
    if model.is_some() || depth >= 2 {
        return model;
    }
    let mut slaves: Vec<_> = std::fs::read_dir(block.join("slaves"))
        .ok()?
        .take(8)
        .filter_map(|entry| std::fs::canonicalize(entry.ok()?.path()).ok())
        .collect();
    slaves.sort();
    drive_model(slaves.first()?, depth + 1)
}

#[cfg(target_os = "macos")]
fn collect() -> (Vec<RawVolume>, Instant) {
    use sysinfo::{DiskRefreshKind, Disks};

    let disks = Disks::new_with_refreshed_list_specifics(
        DiskRefreshKind::nothing().with_storage().with_io_usage(),
    );
    let read_at = Instant::now();
    let volumes = disks
        .list()
        .iter()
        .filter_map(|disk| {
            let total = disk.total_space();
            if total == 0 {
                return None;
            }
            let available = disk.available_space().min(total);
            let mount_point = disk.mount_point().to_string_lossy().into_owned();
            let device = rustix::fs::statfs(disk.mount_point())
                .ok()
                .and_then(|stat| mount_source(&stat.f_mntfromname));
            // Volumes in one APFS container report the container's capacity.
            let container = device.as_deref().map_or_else(
                || mount_point.clone(),
                |device| whole_disk(device).to_owned(),
            );
            let usage = disk.usage();
            Some(RawVolume {
                key: format!("{container}:{total}"),
                label: Some(disk.name().to_string_lossy().into_owned()),
                device,
                model: None,
                file_system: Some(disk.file_system().to_string_lossy().into_owned()),
                total,
                used: total - available,
                available,
                io: (usage.total_read_bytes > 0 || usage.total_written_bytes > 0).then_some(
                    IoCounters {
                        read: usage.total_read_bytes,
                        written: usage.total_written_bytes,
                    },
                ),
                mount_point,
            })
        })
        .collect();
    let volumes = distinct(volumes, |volume| {
        (volume.key.as_str(), true, volume.mount_point.as_str())
    });
    (volumes, read_at)
}

/// `/dev/disk3s1s1` from `statfs`, without the `/dev/` prefix.
#[cfg(target_os = "macos")]
#[allow(clippy::cast_sign_loss)]
fn mount_source(raw: &[std::ffi::c_char]) -> Option<String> {
    let bytes: Vec<u8> = raw
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    String::from_utf8(bytes)
        .ok()?
        .strip_prefix("/dev/")
        .map(str::to_owned)
}

/// `disk3` for `disk3s1s1`: the whole disk, or APFS container, a volume is on.
#[cfg(any(target_os = "macos", test))]
fn whole_disk(device: &str) -> &str {
    let digits = device.strip_prefix("disk").map_or(0, |rest| {
        rest.bytes().take_while(u8::is_ascii_digit).count()
    });
    if digits == 0 {
        device
    } else {
        &device[.."disk".len() + digits]
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn collect() -> (Vec<RawVolume>, Instant) {
    (Vec::new(), Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines from Tron: the root filesystem and two bind mounts of it, other
    /// real disks, an escaped mount point, and the virtual mounts to skip.
    const MOUNTINFO: &str = "\
459 33 259:4 /usr/share/hunspell /var/snap/firefox/common/host-hunspell ro,noexec,noatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
33 2 259:4 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro,stripe=32
56 33 259:3 / /boot/efi rw,relatime shared:126 - vfat /dev/nvme0n1p1 rw,fmask=0077
249 33 259:8 / /mnt/files rw,nosuid,nodev,relatime shared:213 - ext4 /dev/nvme1n1 rw,stripe=32
274 33 8:17 / /mnt/steam\\040500 rw,nosuid,nodev,relatime shared:238 - ext4 /dev/sdb1 rw
9907 33 259:4 /var/snap/microk8s/common/x/config.toml /var/snap/microk8s/common/var/lib/kubelet/pods/0/volume-subpaths/server-config/atmux/0 rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
32 33 0:28 / /run rw,nosuid,nodev,noexec,relatime shared:5 - tmpfs tmpfs rw,size=6362304k
88 33 7:9 / /snap/core18/2999 ro,nodev,relatime shared:40 - squashfs /dev/loop9 ro
91 33 0:62 / /var/lib/docker/overlay2/abc/merged rw,relatime - overlay overlay rw,lowerdir=/a
92 33 259:9 / /run/media/ryan/usb rw,relatime shared:300 - exfat /dev/sdc1 rw
93 33 259:10 / /run/user/1000/doc rw - ext4 /dev/sdd1 rw
94 33 0:50 / /mnt/share rw - nfs4 server:/export rw
garbage line
";

    fn mounts(entries: &[MountEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| entry.mount_point.as_str())
            .collect()
    }

    #[test]
    fn mountinfo_keeps_one_mount_per_block_filesystem() {
        let entries = mount_entries(MOUNTINFO);
        assert_eq!(
            mounts(&entries),
            [
                "/",
                "/boot/efi",
                "/mnt/files",
                "/mnt/steam 500",
                "/run/media/ryan/usb"
            ],
            "bind mounts fold into the root mount; virtual, image, network, and runtime mounts are skipped"
        );
        assert_eq!(entries[0].key, "259:4");
        assert!(entries[0].whole);
        assert_eq!(entries[0].source, "/dev/nvme0n1p2");
        assert_eq!(entries[3].file_system, "ext4");
    }

    #[test]
    fn a_bind_mount_stands_in_when_the_whole_filesystem_is_not_mounted() {
        let entries = mount_entries(
            "1 0 8:1 /srv/a /data/a rw - ext4 /dev/sda1 rw\n2 0 8:1 /b /b rw - ext4 /dev/sda1 rw\n",
        );
        assert_eq!(mounts(&entries), ["/b"], "the shorter mount point wins");
    }

    #[test]
    fn mount_fields_and_labels_decode_escapes() {
        assert_eq!(decode_mount_field("/mnt/a\\040b\\011c"), "/mnt/a b\tc");
        assert_eq!(
            decode_mount_field("/mnt/back\\134slash"),
            "/mnt/back\\slash"
        );
        assert_eq!(decode_mount_field("/odd\\09"), "/odd\\09");
        assert_eq!(decode_mount_field("/end\\04"), "/end\\04");
        assert_eq!(decode_label("Steam\\x20500"), "Steam 500");
        assert_eq!(decode_label("bad\\xZZ"), "bad\\xZZ");
        assert_eq!(
            udev_model(
                "S:disk/by-id/ata-x\nE:ID_MODEL=Samsung_SSD_850_EVO_500GB\nE:ID_MODEL_ENC=Samsung\\x20SSD\\x20850\\x20EVO\\x20500GB\\x20\\x20\\x20\n"
            )
            .as_deref(),
            Some("Samsung SSD 850 EVO 500GB")
        );
        assert_eq!(udev_model("E:ID_MODEL=Samsung SSD 980 1TB\n"), None);
    }

    #[test]
    fn diskstats_reads_sector_counters_in_bytes() {
        let stats = diskstats(
            " 259       4 nvme0n1p2 571069 99189 38910302 165547 9824180 1076154 462374760 4139911 0\n   8 17 sdb1 1 2\n",
        );
        assert_eq!(
            stats["nvme0n1p2"],
            IoCounters {
                read: 38_910_302 * 512,
                written: 462_374_760 * 512,
            }
        );
        assert!(!stats.contains_key("sdb1"), "short lines are skipped");
    }

    fn raw(key: &str, mount_point: &str, total: u64, io: Option<(u64, u64)>) -> RawVolume {
        RawVolume {
            key: key.to_owned(),
            mount_point: mount_point.to_owned(),
            total,
            used: total / 2,
            available: total / 2,
            io: io.map(|(read, written)| IoCounters { read, written }),
            ..RawVolume::default()
        }
    }

    #[test]
    fn rates_come_from_the_previous_counters_and_root_sorts_first() {
        let previous = HashMap::from([
            (
                "259:4".to_owned(),
                IoCounters {
                    read: 1_000,
                    written: 9_000,
                },
            ),
            (
                "8:17".to_owned(),
                IoCounters {
                    read: 500,
                    written: 0,
                },
            ),
        ]);
        let (disks, counters) = finish(
            vec![
                raw("8:17", "/mnt/steam", 500, Some((100, 0))),
                raw("259:7", "/mnt/data", 4_000, Some((7, 7))),
                raw("259:4", "/", 2_000, Some((11_000, 9_000))),
                raw("259:3", "/boot/efi", 1, None),
            ],
            &previous,
            Some(Duration::from_secs(5)),
        );
        let order: Vec<_> = disks.iter().map(|disk| disk.mount_point.as_str()).collect();
        assert_eq!(order, ["/", "/mnt/data", "/mnt/steam", "/boot/efi"]);
        assert_eq!(disks[0].read_bytes_per_second, Some(2_000));
        assert_eq!(disks[0].write_bytes_per_second, Some(0));
        assert_eq!(
            disks[1].read_bytes_per_second, None,
            "a new disk has no rate until its second sample"
        );
        assert_eq!(
            disks[2].read_bytes_per_second, None,
            "a counter that went backwards gives no rate"
        );
        assert_eq!(disks[3].read_bytes_per_second, None);
        assert_eq!(counters.len(), 3);

        let (disks, _) = finish(
            vec![raw("259:4", "/", 2_000, Some((11_000, 9_000)))],
            &previous,
            Some(Duration::from_millis(100)),
        );
        assert_eq!(
            disks[0].read_bytes_per_second, None,
            "too short a window gives no rate"
        );
    }

    #[test]
    fn apfs_volumes_in_one_container_fold_to_the_shortest_mount() {
        assert_eq!(whole_disk("disk3s1s1"), "disk3");
        assert_eq!(whole_disk("disk12s5"), "disk12");
        assert_eq!(whole_disk("disk"), "disk");
        assert_eq!(whole_disk("nvme0n1"), "nvme0n1");
        let volumes = distinct(
            vec![
                raw("disk3:994", "/System/Volumes/Data", 994, None),
                raw("disk3:994", "/", 994, None),
                raw("disk5:64", "/Volumes/USB", 64, None),
            ],
            |volume| (volume.key.as_str(), true, volume.mount_point.as_str()),
        );
        let order: Vec<_> = volumes
            .iter()
            .map(|volume| volume.mount_point.as_str())
            .collect();
        assert_eq!(order, ["/", "/Volumes/USB"]);
    }

    #[test]
    fn sanitizer_bounds_text_and_drops_empty_volumes() {
        let disks = sanitize_disks(vec![
            DiskVolume {
                mount_point: "/".to_owned(),
                label: Some("  ".to_owned()),
                model: Some("x".repeat(MAX_DISK_TEXT_BYTES + 10)),
                total_bytes: 10,
                used_bytes: 20,
                available_bytes: 30,
                ..DiskVolume::default()
            },
            DiskVolume {
                mount_point: "/zero".to_owned(),
                ..DiskVolume::default()
            },
            DiskVolume {
                mount_point: " ".to_owned(),
                total_bytes: 1,
                ..DiskVolume::default()
            },
        ]);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].label, None);
        assert_eq!(disks[0].model.as_ref().unwrap().len(), MAX_DISK_TEXT_BYTES);
        assert_eq!(disks[0].used_bytes, 10);
        assert_eq!(disks[0].available_bytes, 10);
        let many = vec![
            DiskVolume {
                mount_point: "/a".to_owned(),
                total_bytes: 1,
                ..DiskVolume::default()
            };
            MAX_DISKS + 5
        ];
        assert_eq!(sanitize_disks(many).len(), MAX_DISKS);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_collection_lists_the_root_filesystem() {
        let (volumes, _) = collect();
        if let Some(root) = volumes.iter().find(|volume| volume.mount_point == "/") {
            assert!(root.total > 0);
            assert!(root.used + root.available <= root.total);
        }
        let mut keys: Vec<_> = volumes.iter().map(|volume| volume.key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), volumes.len(), "one entry per filesystem");
    }
}
