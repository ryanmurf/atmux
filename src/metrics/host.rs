//! Host CPU, memory composition, and process telemetry for the machine view.
//!
//! Everything here is optional on the wire: older owners omit it and older
//! coordinators ignore it. Linux reads `/proc/meminfo`; macOS runs `vm_stat`
//! no more often than [`DETAIL_SAMPLE_INTERVAL`]. Process groups come from
//! `sysinfo` no more often than [`PROCESS_SAMPLE_INTERVAL`], because walking
//! every process costs far more than the CPU and memory counters.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use super::{bounded_text, finite_percent};

pub(super) const DETAIL_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const PROCESS_SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
const MAX_CPU_CORES: usize = 512;
const MAX_MEMORY_SEGMENTS: usize = 8;
const MAX_PROCESS_GROUPS: usize = 15;
const TOP_PROCESSES_BY_MEMORY: usize = 10;
const TOP_PROCESSES_BY_CPU: usize = 5;
const MAX_PROCESS_NAME_BYTES: usize = 64;
const MAX_SEGMENT_KIND_BYTES: usize = 24;
const MAX_BRAND_BYTES: usize = 96;
#[cfg(any(target_os = "linux", test))]
const KIB: u64 = 1024;

/// System load averages over one, five, and fifteen minutes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct LoadAverage {
    pub one: f32,
    pub five: f32,
    pub fifteen: f32,
}

/// Processor identity, load, and per-core utilization.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CpuDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_cores: Option<u32>,
    /// Mean current clock across cores that report one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_mhz: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_average: Option<LoadAverage>,
    /// Utilization of each logical core, in the operating system's order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub core_percent: Vec<u8>,
}

/// One slice of physical memory. Known kinds are `apps`, `wired`, `kernel`,
/// `compressed`, `shared`, `cache`, and `free`; clients show others as-is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemorySegment {
    pub kind: String,
    pub bytes: u64,
}

/// Physical memory composition plus swap.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemoryDetail {
    /// Memory the OS can hand to new work without swapping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_used_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_total_bytes: Option<u64>,
    /// Ordered composition that sums to physical memory when present.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<MemorySegment>,
}

/// Processes sharing one executable name, summed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ProcessGroup {
    pub name: String,
    pub count: u32,
    /// Summed resident memory. Shared pages count once per process.
    pub memory_bytes: u64,
    /// Share of the whole machine's CPU capacity, 0-100.
    pub cpu_percent: f32,
}

impl CpuDetail {
    pub(super) fn sanitized(mut self) -> Option<Self> {
        self.brand = self
            .brand
            .map(|brand| bounded_text(&brand, MAX_BRAND_BYTES))
            .filter(|brand| !brand.is_empty());
        self.logical_cores = self.logical_cores.filter(|count| *count > 0);
        self.physical_cores = self.physical_cores.filter(|count| *count > 0);
        self.frequency_mhz = self.frequency_mhz.filter(|mhz| *mhz > 0);
        self.load_average = self.load_average.filter(|load| {
            [load.one, load.five, load.fifteen]
                .iter()
                .all(|value| value.is_finite() && *value >= 0.0)
        });
        self.core_percent.truncate(MAX_CPU_CORES);
        for value in &mut self.core_percent {
            *value = (*value).min(100);
        }
        let empty = self.brand.is_none()
            && self.logical_cores.is_none()
            && self.physical_cores.is_none()
            && self.frequency_mhz.is_none()
            && self.load_average.is_none()
            && self.core_percent.is_empty();
        (!empty).then_some(self)
    }
}

impl MemoryDetail {
    pub(super) fn sanitized(mut self) -> Option<Self> {
        self.segments = self
            .segments
            .into_iter()
            .filter_map(|segment| {
                let kind = segment_kind(&segment.kind)?;
                Some(MemorySegment {
                    kind,
                    bytes: segment.bytes,
                })
            })
            .take(MAX_MEMORY_SEGMENTS)
            .collect();
        if self.swap_total_bytes == Some(0) {
            self.swap_used_bytes = None;
            self.swap_total_bytes = None;
        }
        let empty = self.available_bytes.is_none()
            && self.swap_used_bytes.is_none()
            && self.swap_total_bytes.is_none()
            && self.segments.is_empty();
        (!empty).then_some(self)
    }
}

impl ProcessGroup {
    fn sanitized(mut self) -> Option<Self> {
        self.name = bounded_text(&self.name, MAX_PROCESS_NAME_BYTES);
        if self.name.is_empty() || self.count == 0 {
            return None;
        }
        self.cpu_percent = if self.cpu_percent.is_finite() {
            round_tenth(self.cpu_percent.clamp(0.0, 100.0))
        } else {
            0.0
        };
        Some(self)
    }
}

pub(super) fn sanitize_processes(groups: Vec<ProcessGroup>) -> Vec<ProcessGroup> {
    groups
        .into_iter()
        .take(MAX_PROCESS_GROUPS)
        .filter_map(ProcessGroup::sanitized)
        .collect()
}

fn segment_kind(kind: &str) -> Option<String> {
    let kind = kind.trim();
    (!kind.is_empty()
        && kind.len() <= MAX_SEGMENT_KIND_BYTES
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_'))
    .then(|| kind.to_owned())
}

fn round_tenth(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

/// The memory, process, and CPU detail that `HardwareSampler` adds to every
/// snapshot, with the slow collectors throttled.
#[derive(Debug, Default)]
pub(super) struct HostSampler {
    #[cfg(target_os = "macos")]
    vm_stat: Option<VmStat>,
    detail_sampled_at: Option<Instant>,
    frequency_mhz: Option<u32>,
    processes: Vec<ProcessGroup>,
    process_count: Option<u32>,
    processes_sampled_at: Option<Instant>,
}

pub(super) struct HostSample {
    pub cpu: Option<CpuDetail>,
    pub memory: Option<MemoryDetail>,
    pub processes: Vec<ProcessGroup>,
    pub process_count: Option<u32>,
}

impl HostSampler {
    /// Expects `system` to have fresh CPU usage and memory counters.
    pub(super) fn sample(&mut self, system: &mut System) -> HostSample {
        let detail_due = self
            .detail_sampled_at
            .is_none_or(|sampled_at| sampled_at.elapsed() >= DETAIL_SAMPLE_INTERVAL);
        if detail_due {
            system.refresh_cpu_frequency();
            self.frequency_mhz = mean_frequency(system);
            #[cfg(target_os = "macos")]
            {
                self.vm_stat = super::run_command_bounded("vm_stat", &[], super::COMMAND_TIMEOUT)
                    .ok()
                    .and_then(|output| parse_vm_stat(&output));
            }
            self.detail_sampled_at = Some(Instant::now());
        }
        if self
            .processes_sampled_at
            .is_none_or(|sampled_at| sampled_at.elapsed() >= PROCESS_SAMPLE_INTERVAL)
        {
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_memory()
                    .with_cpu()
                    .without_tasks(),
            );
            let logical = system.cpus().len().max(1);
            let processes: Vec<_> = system
                .processes()
                .values()
                .filter(|process| process.thread_kind().is_none())
                .map(|process| {
                    (
                        process.name().to_string_lossy().into_owned(),
                        process.memory(),
                        process.cpu_usage(),
                    )
                })
                .collect();
            self.process_count = u32::try_from(processes.len()).ok();
            self.processes = group_processes(processes, logical);
            self.processes_sampled_at = Some(Instant::now());
        }
        HostSample {
            cpu: self.cpu_detail(system),
            memory: self.memory_detail(system),
            processes: self.processes.clone(),
            process_count: self.process_count,
        }
    }

    fn cpu_detail(&self, system: &System) -> Option<CpuDetail> {
        let cpus = system.cpus();
        CpuDetail {
            brand: cpus.first().map(|cpu| cpu.brand().to_owned()),
            logical_cores: u32::try_from(cpus.len()).ok(),
            physical_cores: System::physical_core_count()
                .and_then(|count| u32::try_from(count).ok()),
            frequency_mhz: self.frequency_mhz,
            load_average: load_average(),
            core_percent: cpus
                .iter()
                .take(MAX_CPU_CORES)
                .map(|cpu| finite_percent(cpu.cpu_usage()).unwrap_or(0))
                .collect(),
        }
        .sanitized()
    }

    fn memory_detail(&self, system: &System) -> Option<MemoryDetail> {
        let total = system.total_memory();
        MemoryDetail {
            available_bytes: (total > 0).then(|| system.available_memory().min(total)),
            swap_used_bytes: Some(system.used_swap()),
            swap_total_bytes: Some(system.total_swap()),
            segments: self.memory_segments(total),
        }
        .sanitized()
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::unused_self)]
    fn memory_segments(&self, _total: u64) -> Vec<MemorySegment> {
        super::read_bounded_file(std::path::Path::new("/proc/meminfo"))
            .map(|input| meminfo_segments(&input))
            .unwrap_or_default()
    }

    #[cfg(target_os = "macos")]
    fn memory_segments(&self, total: u64) -> Vec<MemorySegment> {
        self.vm_stat
            .as_ref()
            .map(|stat| vm_stat_segments(stat, total))
            .unwrap_or_default()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[allow(clippy::unused_self)]
    fn memory_segments(&self, _total: u64) -> Vec<MemorySegment> {
        Vec::new()
    }
}

fn mean_frequency(system: &System) -> Option<u32> {
    let reported: Vec<u64> = system
        .cpus()
        .iter()
        .map(sysinfo::Cpu::frequency)
        .filter(|mhz| *mhz > 0)
        .collect();
    let count = u64::try_from(reported.len())
        .ok()
        .filter(|count| *count > 0)?;
    u32::try_from(reported.iter().sum::<u64>() / count).ok()
}

// Option keeps one signature with the non-Unix variant below.
#[cfg(unix)]
#[allow(clippy::cast_possible_truncation, clippy::unnecessary_wraps)]
fn load_average() -> Option<LoadAverage> {
    let load = System::load_average();
    Some(LoadAverage {
        one: round_hundredth(load.one as f32),
        five: round_hundredth(load.five as f32),
        fifteen: round_hundredth(load.fifteen as f32),
    })
}

#[cfg(not(unix))]
fn load_average() -> Option<LoadAverage> {
    None
}

#[cfg(unix)]
fn round_hundredth(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

/// Sums processes by name, then keeps the heaviest by memory plus the busiest
/// by CPU, ordered by memory. `cpu_percent` arrives in sysinfo's per-core
/// units and leaves as a share of the whole machine.
pub(super) fn group_processes(
    processes: impl IntoIterator<Item = (String, u64, f32)>,
    logical_cores: usize,
) -> Vec<ProcessGroup> {
    let mut groups: HashMap<String, ProcessGroup> = HashMap::new();
    for (name, memory, cpu) in processes {
        let name = bounded_text(&name, MAX_PROCESS_NAME_BYTES);
        if name.is_empty() {
            continue;
        }
        let group = groups.entry(name.clone()).or_insert_with(|| ProcessGroup {
            name,
            ..ProcessGroup::default()
        });
        group.count = group.count.saturating_add(1);
        group.memory_bytes = group.memory_bytes.saturating_add(memory);
        if cpu.is_finite() && cpu > 0.0 {
            group.cpu_percent += cpu;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let capacity = logical_cores.max(1) as f32;
    let mut groups: Vec<ProcessGroup> = groups
        .into_values()
        .map(|mut group| {
            group.cpu_percent /= capacity;
            group
        })
        .collect();
    groups.sort_by(|left, right| {
        right
            .memory_bytes
            .cmp(&left.memory_bytes)
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut selected: Vec<ProcessGroup> = groups
        .iter()
        .take(TOP_PROCESSES_BY_MEMORY)
        .cloned()
        .collect();
    let mut by_cpu: Vec<&ProcessGroup> = groups.iter().skip(TOP_PROCESSES_BY_MEMORY).collect();
    by_cpu.sort_by(|left, right| {
        right
            .cpu_percent
            .total_cmp(&left.cpu_percent)
            .then_with(|| left.name.cmp(&right.name))
    });
    let busiest_selected = selected
        .iter()
        .map(|group| group.cpu_percent)
        .fold(0.0_f32, f32::max);
    let mut by_cpu_added = 0;
    for group in by_cpu {
        if by_cpu_added == TOP_PROCESSES_BY_CPU || group.cpu_percent < 0.05 {
            break;
        }
        // Only add CPU-heavy groups the memory list would otherwise hide.
        if group.cpu_percent >= 0.5 || group.cpu_percent >= busiest_selected {
            selected.push(group.clone());
            by_cpu_added += 1;
        }
    }
    sanitize_processes(selected)
}

/// Linux composition that sums exactly to `MemTotal`:
/// apps = everything not free, cache, shared, or kernel bookkeeping.
#[cfg(any(target_os = "linux", test))]
pub(super) fn meminfo_segments(input: &str) -> Vec<MemorySegment> {
    let mut fields: HashMap<&str, u64> = HashMap::new();
    for line in input.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = value.split_whitespace();
        let Some(number) = parts.next().and_then(|number| number.parse::<u64>().ok()) else {
            continue;
        };
        let bytes = match parts.next() {
            Some("kB") => number.saturating_mul(KIB),
            None => number,
            Some(_) => continue,
        };
        fields.insert(key.trim(), bytes);
    }
    let field = |key: &str| fields.get(key).copied().unwrap_or(0);
    let (Some(total), Some(free), Some(cached)) = (
        fields.get("MemTotal").copied(),
        fields.get("MemFree").copied(),
        fields.get("Cached").copied(),
    ) else {
        return Vec::new();
    };
    let buffers = field("Buffers");
    let reclaimable = field("SReclaimable");
    let shared = field("Shmem").min(cached);
    let kernel = field("SUnreclaim")
        .saturating_add(field("KernelStack"))
        .saturating_add(field("PageTables"));
    let cache = buffers
        .saturating_add(cached - shared)
        .saturating_add(reclaimable);
    fit_segments(
        total,
        &[("kernel", kernel), ("shared", shared), ("cache", cache)],
        free,
    )
}

/// Builds `apps, <ordered>, free` summing to `total`. Apps is the remainder;
/// when the counters overshoot `total`, later slices are trimmed instead.
#[cfg(any(target_os = "linux", test))]
fn fit_segments(total: u64, ordered: &[(&str, u64)], free: u64) -> Vec<MemorySegment> {
    if total == 0 {
        return Vec::new();
    }
    let mut remaining = total;
    let free = free.min(remaining);
    remaining -= free;
    let mut middle = Vec::with_capacity(ordered.len());
    for (kind, bytes) in ordered {
        let bytes = (*bytes).min(remaining);
        remaining -= bytes;
        middle.push(((*kind).to_owned(), bytes));
    }
    let mut segments = Vec::with_capacity(ordered.len() + 2);
    segments.push(MemorySegment {
        kind: "apps".to_owned(),
        bytes: remaining,
    });
    segments.extend(
        middle
            .into_iter()
            .map(|(kind, bytes)| MemorySegment { kind, bytes }),
    );
    segments.push(MemorySegment {
        kind: "free".to_owned(),
        bytes: free,
    });
    segments
}

/// Page counts from `vm_stat`, already multiplied by the page size.
#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct VmStat {
    free: u64,
    speculative: u64,
    wired: u64,
    purgeable: u64,
    file_backed: u64,
    anonymous: u64,
    compressor: u64,
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn parse_vm_stat(output: &str) -> Option<VmStat> {
    let mut lines = output.lines();
    let header = lines.next()?;
    let page_size: u64 = header
        .split("page size of ")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let mut pages: HashMap<String, u64> = HashMap::new();
    for line in lines {
        let Some((key, value)) = line.rsplit_once(':') else {
            continue;
        };
        if let Ok(count) = value.trim().trim_end_matches('.').parse::<u64>() {
            pages.insert(key.trim().trim_matches('"').to_owned(), count);
        }
    }
    let bytes = |key: &str| pages.get(key).map(|count| count.saturating_mul(page_size));
    Some(VmStat {
        free: bytes("Pages free")?,
        speculative: bytes("Pages speculative").unwrap_or(0),
        wired: bytes("Pages wired down")?,
        purgeable: bytes("Pages purgeable").unwrap_or(0),
        file_backed: bytes("File-backed pages")?,
        anonymous: bytes("Anonymous pages")?,
        compressor: bytes("Pages occupied by compressor").unwrap_or(0),
    })
}

/// Activity Monitor's categories: app memory (anonymous minus purgeable),
/// wired, compressed, cached files (file-backed plus purgeable), and free.
#[cfg(any(target_os = "macos", test))]
pub(super) fn vm_stat_segments(stat: &VmStat, total: u64) -> Vec<MemorySegment> {
    if total == 0 {
        return Vec::new();
    }
    let apps = stat.anonymous.saturating_sub(stat.purgeable);
    let cache = stat.file_backed.saturating_add(stat.purgeable);
    let accounted = apps
        .saturating_add(stat.wired)
        .saturating_add(stat.compressor)
        .saturating_add(cache);
    // Free is what the categories leave over, so the bar always sums to total;
    // vm_stat's own free and speculative counts bound it from below.
    let free = total
        .saturating_sub(accounted)
        .max(stat.free.saturating_add(stat.speculative).min(total));
    let mut segments = Vec::with_capacity(5);
    let mut remaining = total - free;
    for (kind, bytes) in [
        ("apps", apps),
        ("wired", stat.wired),
        ("compressed", stat.compressor),
        ("cache", cache),
    ] {
        let bytes = bytes.min(remaining);
        remaining -= bytes;
        segments.push(MemorySegment {
            kind: kind.to_owned(),
            bytes,
        });
    }
    segments.push(MemorySegment {
        kind: "free".to_owned(),
        bytes: free + remaining,
    });
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       65536000 kB
MemFree:         4096000 kB
MemAvailable:   40960000 kB
Buffers:          512000 kB
Cached:         30720000 kB
SwapCached:            0 kB
Shmem:           2048000 kB
SReclaimable:    1024000 kB
SUnreclaim:       512000 kB
KernelStack:       32000 kB
PageTables:       128000 kB
HugePages_Total:       0
";

    const VM_STAT: &str = r#"Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                    56327.
Pages active:                                 591935.
Pages inactive:                               587651.
Pages speculative:                              3122.
Pages throttled:                                   0.
Pages wired down:                            2682840.
Pages purgeable:                                9015.
"Translation faults":                    18485140885.
File-backed pages:                            414866.
Anonymous pages:                              767842.
Pages stored in compressor:                   416410.
Pages occupied by compressor:                 214105.
"#;

    fn sum(segments: &[MemorySegment]) -> u64 {
        segments.iter().map(|segment| segment.bytes).sum()
    }

    fn kinds(segments: &[MemorySegment]) -> Vec<&str> {
        segments
            .iter()
            .map(|segment| segment.kind.as_str())
            .collect()
    }

    #[test]
    fn meminfo_composition_sums_to_total_and_separates_cache() {
        let segments = meminfo_segments(MEMINFO);
        assert_eq!(
            kinds(&segments),
            ["apps", "kernel", "shared", "cache", "free"]
        );
        assert_eq!(sum(&segments), 65_536_000 * KIB);
        let bytes = |kind: &str| {
            segments
                .iter()
                .find(|segment| segment.kind == kind)
                .unwrap()
                .bytes
        };
        assert_eq!(bytes("free"), 4_096_000 * KIB);
        assert_eq!(bytes("shared"), 2_048_000 * KIB);
        assert_eq!(bytes("kernel"), (512_000 + 32_000 + 128_000) * KIB);
        assert_eq!(
            bytes("cache"),
            (512_000 + 30_720_000 - 2_048_000 + 1_024_000) * KIB
        );
        assert_eq!(
            bytes("apps"),
            (65_536_000 - 4_096_000 - 512_000 - 30_720_000 - 1_024_000 - 672_000) * KIB
        );
    }

    #[test]
    fn meminfo_without_required_fields_or_with_overshoot_stays_bounded() {
        assert!(meminfo_segments("MemTotal: 10 kB\n").is_empty());
        assert!(meminfo_segments("garbage").is_empty());
        let overshoot = meminfo_segments(
            "MemTotal: 100 kB\nMemFree: 80 kB\nCached: 90 kB\nSReclaimable: 50 kB\n",
        );
        assert_eq!(sum(&overshoot), 100 * KIB);
        assert_eq!(
            overshoot[0].bytes, 0,
            "apps clamps to zero, never underflows"
        );
    }

    #[test]
    fn vm_stat_matches_activity_monitor_categories() {
        let stat = parse_vm_stat(VM_STAT).unwrap();
        let page = 16_384;
        assert_eq!(stat.wired, 2_682_840 * page);
        assert_eq!(stat.compressor, 214_105 * page);
        let total = 64 * 1024 * 1024 * 1024;
        let segments = vm_stat_segments(&stat, total);
        assert_eq!(
            kinds(&segments),
            ["apps", "wired", "compressed", "cache", "free"]
        );
        assert_eq!(sum(&segments), total);
        assert_eq!(segments[0].bytes, (767_842 - 9_015) * page);
        assert_eq!(segments[1].bytes, 2_682_840 * page);
        assert_eq!(segments[3].bytes, (414_866 + 9_015) * page);
        assert!(segments[4].bytes >= (56_327 + 3_122) * page);
    }

    #[test]
    fn vm_stat_rejects_missing_page_size_and_never_exceeds_total() {
        assert!(parse_vm_stat("Pages free: 1.\n").is_none());
        let stat = parse_vm_stat(VM_STAT).unwrap();
        let small = vm_stat_segments(&stat, 1024);
        assert_eq!(sum(&small), 1024);
        assert!(vm_stat_segments(&stat, 0).is_empty());
    }

    #[test]
    fn process_groups_sum_by_name_and_report_machine_share() {
        let mut processes = vec![
            ("claude".to_owned(), 400, 50.0),
            ("claude".to_owned(), 600, 30.0),
            ("llama-server".to_owned(), 7_000, 780.0),
            (String::new(), 9_999, 1.0),
        ];
        for index in 0..20 {
            processes.push((format!("idle-{index:02}"), 10, 0.0));
        }
        processes.push(("busy-small".to_owned(), 1, 160.0));
        let groups = group_processes(processes, 16);
        assert_eq!(groups[0].name, "llama-server");
        assert!((groups[0].cpu_percent - 48.8).abs() < 0.01);
        assert_eq!(groups[1].name, "claude");
        assert_eq!(groups[1].count, 2);
        assert_eq!(groups[1].memory_bytes, 1_000);
        assert!((groups[1].cpu_percent - 5.0).abs() < 0.01);
        assert!(
            groups.iter().any(|group| group.name == "busy-small"),
            "a CPU-heavy group outside the memory top list is still shown"
        );
        assert!(groups.len() <= MAX_PROCESS_GROUPS);
        assert!(groups.iter().all(|group| !group.name.is_empty()));
    }

    #[test]
    fn sanitizers_drop_empty_and_out_of_range_values() {
        assert!(CpuDetail::default().sanitized().is_none());
        let cpu = CpuDetail {
            brand: Some("  ".to_owned()),
            frequency_mhz: Some(0),
            load_average: Some(LoadAverage {
                one: f32::NAN,
                five: 1.0,
                fifteen: 1.0,
            }),
            core_percent: vec![120, 40],
            ..CpuDetail::default()
        }
        .sanitized()
        .unwrap();
        assert_eq!(cpu.brand, None);
        assert_eq!(cpu.frequency_mhz, None);
        assert_eq!(cpu.load_average, None);
        assert_eq!(cpu.core_percent, [100, 40]);
        let memory = MemoryDetail {
            swap_used_bytes: Some(0),
            swap_total_bytes: Some(0),
            segments: vec![
                MemorySegment {
                    kind: "apps".to_owned(),
                    bytes: 1,
                },
                MemorySegment {
                    kind: "<script>".to_owned(),
                    bytes: 1,
                },
            ],
            ..MemoryDetail::default()
        }
        .sanitized()
        .unwrap();
        assert_eq!(memory.swap_total_bytes, None);
        assert_eq!(kinds(&memory.segments), ["apps"]);
        assert!(MemoryDetail::default().sanitized().is_none());
    }
}
