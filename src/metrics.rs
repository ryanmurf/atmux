//! Bounded, cross-platform machine telemetry for the dashboard.
//!
//! CPU, memory, and general temperatures come from `sysinfo`. GPU telemetry is
//! best effort and never requires elevated privileges: Linux uses NVIDIA's
//! documented CSV query plus bounded DRM/sysfs reads for AMD and Intel, while
//! macOS uses bounded `system_profiler`, `ioreg`, `pmset`, and
//! `memory_pressure` output. Missing counters stay `None` and are named in the
//! device's `unavailable` list instead of being reported as zero.

use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(any(target_os = "linux", test))]
use std::{collections::BTreeMap, fs, path::Path};

use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
#[cfg(any(target_os = "macos", test))]
use serde_json::Value;
use sysinfo::{Components, System};

const GPU_SAMPLE_INTERVAL: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const COMMAND_STDOUT_LIMIT: usize = 256 * 1024;
const COMMAND_STDERR_LIMIT: usize = 16 * 1024;
const MAX_GPU_DEVICES: usize = 16;
const MAX_GPU_DIAGNOSTICS: usize = 24;
const MAX_GPU_UNAVAILABLE: usize = 20;
const MAX_TEMPERATURES: usize = 64;
const MAX_TEXT_BYTES: usize = 160;
#[cfg(any(target_os = "linux", test))]
const MAX_SYSFS_BYTES: usize = 16 * 1024;
#[cfg(any(target_os = "linux", test))]
const MAX_DRM_ENTRIES: usize = 64;
#[cfg(any(target_os = "linux", test))]
const MAX_HWMON_ENTRIES: usize = 16;
const MIB: u64 = 1024 * 1024;

/// One temperature sensor reported by the operating system.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TemperatureReading {
    pub label: String,
    pub celsius: f32,
}

/// One bounded collector-level diagnostic. These messages explain why a GPU
/// family could not be inspected without turning absence into a fake reading.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GpuDiagnostic {
    pub source: String,
    pub message: String,
}

/// Telemetry and stable identity for one GPU.
///
/// The original five fields remain unchanged. New fields are optional/defaulted
/// so older nodes and browser clients continue to interoperate during rollout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, JsonSchema)]
pub struct GpuMetrics {
    /// Stable vendor UUID, PCI address, or macOS registry-derived identifier.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_bus_id: Option<String>,
    pub utilization_percent: Option<u8>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
    pub temperature_celsius: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_shared: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_pressure_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_draw_watts: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_limit_watts: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphics_clock_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_clock_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_clock_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fan_speed_rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermal_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performance_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_count: Option<u32>,
    /// Names of counters this device cannot safely expose on this host.
    #[serde(default)]
    pub unavailable: Vec<String>,
}

#[derive(Deserialize)]
#[serde(default)]
struct GpuMetricsWire {
    id: String,
    name: String,
    vendor: Option<String>,
    pci_bus_id: Option<String>,
    utilization_percent: Option<u8>,
    memory_used_bytes: Option<u64>,
    memory_total_bytes: Option<u64>,
    temperature_celsius: Option<f32>,
    memory_shared: Option<bool>,
    memory_pressure_percent: Option<u8>,
    power_draw_watts: Option<f32>,
    power_limit_watts: Option<f32>,
    graphics_clock_mhz: Option<u32>,
    memory_clock_mhz: Option<u32>,
    video_clock_mhz: Option<u32>,
    fan_percent: Option<u8>,
    fan_speed_rpm: Option<u32>,
    thermal_state: Option<String>,
    performance_state: Option<String>,
    driver_version: Option<String>,
    runtime_version: Option<String>,
    compute_capability: Option<String>,
    core_count: Option<u32>,
    unavailable: Vec<String>,
}

impl Default for GpuMetricsWire {
    fn default() -> Self {
        Self::from(GpuMetrics::default())
    }
}

impl From<GpuMetrics> for GpuMetricsWire {
    fn from(value: GpuMetrics) -> Self {
        Self {
            id: value.id,
            name: value.name,
            vendor: value.vendor,
            pci_bus_id: value.pci_bus_id,
            utilization_percent: value.utilization_percent,
            memory_used_bytes: value.memory_used_bytes,
            memory_total_bytes: value.memory_total_bytes,
            temperature_celsius: value.temperature_celsius,
            memory_shared: value.memory_shared,
            memory_pressure_percent: value.memory_pressure_percent,
            power_draw_watts: value.power_draw_watts,
            power_limit_watts: value.power_limit_watts,
            graphics_clock_mhz: value.graphics_clock_mhz,
            memory_clock_mhz: value.memory_clock_mhz,
            video_clock_mhz: value.video_clock_mhz,
            fan_percent: value.fan_percent,
            fan_speed_rpm: value.fan_speed_rpm,
            thermal_state: value.thermal_state,
            performance_state: value.performance_state,
            driver_version: value.driver_version,
            runtime_version: value.runtime_version,
            compute_capability: value.compute_capability,
            core_count: value.core_count,
            unavailable: value.unavailable,
        }
    }
}

impl<'de> Deserialize<'de> for GpuMetrics {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GpuMetricsWire::deserialize(deserializer)?;
        let name = bounded_text(&wire.name, MAX_TEXT_BYTES);
        let id = if wire.id.trim().is_empty() {
            name.clone()
        } else {
            bounded_text(&wire.id, MAX_TEXT_BYTES)
        };
        Ok(Self {
            id,
            name,
            vendor: bounded_optional(wire.vendor),
            pci_bus_id: bounded_optional(wire.pci_bus_id),
            utilization_percent: valid_percent(wire.utilization_percent),
            memory_used_bytes: wire.memory_used_bytes,
            memory_total_bytes: wire.memory_total_bytes,
            temperature_celsius: finite_temperature(wire.temperature_celsius),
            memory_shared: wire.memory_shared,
            memory_pressure_percent: valid_percent(wire.memory_pressure_percent),
            power_draw_watts: finite_nonnegative(wire.power_draw_watts),
            power_limit_watts: finite_nonnegative(wire.power_limit_watts),
            graphics_clock_mhz: wire.graphics_clock_mhz,
            memory_clock_mhz: wire.memory_clock_mhz,
            video_clock_mhz: wire.video_clock_mhz,
            fan_percent: valid_percent(wire.fan_percent),
            fan_speed_rpm: wire.fan_speed_rpm,
            thermal_state: bounded_optional(wire.thermal_state),
            performance_state: bounded_optional(wire.performance_state),
            driver_version: bounded_optional(wire.driver_version),
            runtime_version: bounded_optional(wire.runtime_version),
            compute_capability: bounded_optional(wire.compute_capability),
            core_count: wire.core_count,
            unavailable: wire
                .unavailable
                .into_iter()
                .take(MAX_GPU_UNAVAILABLE)
                .map(|value| bounded_text(&value, MAX_TEXT_BYTES))
                .filter(|value| !value.is_empty())
                .collect(),
        })
    }
}

/// Live resource snapshot for one atmux machine.
#[derive(Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct MachineMetrics {
    pub cpu_percent: Option<u8>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub gpus: Vec<GpuMetrics>,
    pub temperatures: Vec<TemperatureReading>,
    pub gpu_diagnostics: Vec<GpuDiagnostic>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct MachineMetricsWire {
    cpu_percent: Option<u8>,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    gpus: Vec<GpuMetrics>,
    temperatures: Vec<TemperatureReading>,
    gpu_diagnostics: Vec<GpuDiagnostic>,
}

#[derive(Serialize)]
struct MachineMetricsOutput {
    cpu_percent: Option<u8>,
    memory_used_bytes: u64,
    memory_total_bytes: u64,
    gpus: Vec<GpuMetrics>,
    temperatures: Vec<TemperatureReading>,
    gpu_diagnostics: Vec<GpuDiagnostic>,
}

impl Serialize for MachineMetrics {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let gpus = self
            .gpus
            .iter()
            .take(MAX_GPU_DEVICES)
            .enumerate()
            .map(|(index, gpu)| {
                let mut gpu = gpu.clone();
                gpu.unavailable = gpu
                    .unavailable
                    .into_iter()
                    .take(MAX_GPU_UNAVAILABLE)
                    .map(|value| bounded_text(&value, MAX_TEXT_BYTES))
                    .filter(|value| !value.is_empty())
                    .collect();
                finalize_gpu(gpu, &format!("gpu-{index}"))
            })
            .collect();
        let temperatures = self
            .temperatures
            .iter()
            .take(MAX_TEMPERATURES)
            .filter(|reading| reading.celsius.is_finite())
            .map(|reading| TemperatureReading {
                label: bounded_text(&reading.label, MAX_TEXT_BYTES),
                celsius: reading.celsius,
            })
            .collect();
        let gpu_diagnostics = self
            .gpu_diagnostics
            .iter()
            .take(MAX_GPU_DIAGNOSTICS)
            .map(|diagnostic| GpuDiagnostic {
                source: bounded_text(&diagnostic.source, MAX_TEXT_BYTES),
                message: bounded_text(&diagnostic.message, MAX_TEXT_BYTES),
            })
            .filter(|diagnostic| !diagnostic.source.is_empty() && !diagnostic.message.is_empty())
            .collect();
        MachineMetricsOutput {
            cpu_percent: valid_percent(self.cpu_percent),
            memory_used_bytes: self.memory_used_bytes,
            memory_total_bytes: self.memory_total_bytes,
            gpus,
            temperatures,
            gpu_diagnostics,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MachineMetrics {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = MachineMetricsWire::deserialize(deserializer)?;
        Ok(Self {
            cpu_percent: valid_percent(wire.cpu_percent),
            memory_used_bytes: wire.memory_used_bytes,
            memory_total_bytes: wire.memory_total_bytes,
            gpus: wire.gpus.into_iter().take(MAX_GPU_DEVICES).collect(),
            temperatures: wire
                .temperatures
                .into_iter()
                .take(MAX_TEMPERATURES)
                .filter_map(|reading| {
                    reading.celsius.is_finite().then(|| TemperatureReading {
                        label: bounded_text(&reading.label, MAX_TEXT_BYTES),
                        celsius: reading.celsius,
                    })
                })
                .collect(),
            gpu_diagnostics: wire
                .gpu_diagnostics
                .into_iter()
                .take(MAX_GPU_DIAGNOSTICS)
                .map(|diagnostic| GpuDiagnostic {
                    source: bounded_text(&diagnostic.source, MAX_TEXT_BYTES),
                    message: bounded_text(&diagnostic.message, MAX_TEXT_BYTES),
                })
                .filter(|diagnostic| {
                    !diagnostic.source.is_empty() && !diagnostic.message.is_empty()
                })
                .collect(),
        })
    }
}

/// Retains the prior CPU sample and throttles optional hardware utilities.
#[derive(Debug)]
pub struct HardwareSampler {
    system: System,
    components: Components,
    gpu_sample: GpuSample,
    gpu_sampled_at: Option<Instant>,
}

impl Default for HardwareSampler {
    fn default() -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        system.refresh_memory();
        Self {
            system,
            components: Components::new_with_refreshed_list(),
            gpu_sample: GpuSample::default(),
            gpu_sampled_at: None,
        }
    }
}

impl HardwareSampler {
    /// Samples CPU, memory, temperatures, and safely available GPU telemetry.
    #[must_use]
    pub fn sample(&mut self) -> MachineMetrics {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.components.refresh(false);
        if self
            .gpu_sampled_at
            .is_none_or(|sampled_at| sampled_at.elapsed() >= GPU_SAMPLE_INTERVAL)
        {
            self.gpu_sample = gpu_metrics();
            self.gpu_sampled_at = Some(Instant::now());
        }
        let temperatures = self
            .components
            .list()
            .iter()
            .take(MAX_TEMPERATURES)
            .filter_map(|component| {
                let celsius = component.temperature()?;
                celsius.is_finite().then(|| TemperatureReading {
                    label: bounded_text(component.label(), MAX_TEXT_BYTES),
                    celsius: round_one_decimal(celsius),
                })
            })
            .collect();
        MachineMetrics {
            cpu_percent: finite_percent(self.system.global_cpu_usage()),
            memory_used_bytes: self.system.used_memory(),
            memory_total_bytes: self.system.total_memory(),
            gpus: self.gpu_sample.gpus.clone(),
            temperatures,
            gpu_diagnostics: self.gpu_sample.diagnostics.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct GpuSample {
    gpus: Vec<GpuMetrics>,
    diagnostics: Vec<GpuDiagnostic>,
}

impl GpuSample {
    #[cfg(any(
        target_os = "linux",
        test,
        not(any(target_os = "linux", target_os = "macos"))
    ))]
    fn diagnostic(&mut self, source: &str, message: &str) {
        if self.diagnostics.len() < MAX_GPU_DIAGNOSTICS {
            self.diagnostics.push(GpuDiagnostic {
                source: bounded_text(source, MAX_TEXT_BYTES),
                message: bounded_text(message, MAX_TEXT_BYTES),
            });
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn extend(&mut self, mut other: Self) {
        for gpu in other.gpus.drain(..) {
            if let Some(existing) = self
                .gpus
                .iter_mut()
                .find(|existing| same_gpu(existing, &gpu))
            {
                if gpu_field_score(&gpu) > gpu_field_score(existing) {
                    *existing = gpu;
                }
            } else if self.gpus.len() < MAX_GPU_DEVICES {
                self.gpus.push(gpu);
            } else {
                self.diagnostic("gpu", "device list truncated at the safety limit");
                break;
            }
        }
        let room = MAX_GPU_DIAGNOSTICS.saturating_sub(self.diagnostics.len());
        self.diagnostics
            .extend(other.diagnostics.drain(..).take(room));
    }
}

#[cfg(any(target_os = "linux", test))]
fn same_gpu(left: &GpuMetrics, right: &GpuMetrics) -> bool {
    (!left.id.is_empty() && left.id == right.id)
        || matches!(
            (&left.pci_bus_id, &right.pci_bus_id),
            (Some(left), Some(right)) if normalize_pci_bus_id(left) == normalize_pci_bus_id(right)
        )
}

#[cfg(any(target_os = "linux", test))]
fn gpu_field_score(gpu: &GpuMetrics) -> usize {
    [
        gpu.vendor.is_some(),
        gpu.pci_bus_id.is_some(),
        gpu.utilization_percent.is_some(),
        gpu.memory_used_bytes.is_some(),
        gpu.memory_total_bytes.is_some(),
        gpu.temperature_celsius.is_some(),
        gpu.power_draw_watts.is_some(),
        gpu.power_limit_watts.is_some(),
        gpu.graphics_clock_mhz.is_some(),
        gpu.memory_clock_mhz.is_some(),
        gpu.video_clock_mhz.is_some(),
        gpu.fan_percent.is_some(),
        gpu.fan_speed_rpm.is_some(),
        gpu.thermal_state.is_some(),
        gpu.performance_state.is_some(),
        gpu.driver_version.is_some(),
        gpu.runtime_version.is_some(),
        gpu.compute_capability.is_some(),
        gpu.core_count.is_some(),
    ]
    .into_iter()
    .filter(|present| *present)
    .count()
}

#[cfg(any(target_os = "linux", test))]
fn normalize_pci_bus_id(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    let Some((domain, address)) = value.split_once(':') else {
        return value;
    };
    let domain = domain.trim_start_matches('0');
    let domain = if domain.is_empty() { "0" } else { domain };
    let domain = if domain.len() > 4 {
        &domain[domain.len() - 4..]
    } else {
        domain
    };
    format!("{domain:0>4}:{address}")
}

fn finite_percent(value: f32) -> Option<u8> {
    value.is_finite().then(|| {
        format!("{:.0}", value.round().clamp(0.0, 100.0))
            .parse()
            .expect("a clamped whole percentage always fits in u8")
    })
}

fn valid_percent(value: Option<u8>) -> Option<u8> {
    value.filter(|value| *value <= 100)
}

fn finite_temperature(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite() && (-100.0..=250.0).contains(value))
}

fn finite_nonnegative(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite() && *value >= 0.0)
}

fn round_one_decimal(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

fn bounded_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| bounded_text(&value, MAX_TEXT_BYTES))
        .filter(|value| !value.is_empty())
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_bytes));
    for character in value.trim().chars() {
        if output.len() + character.len_utf8() > max_bytes {
            break;
        }
        if !character.is_control() || character == ' ' {
            output.push(character);
        }
    }
    output
}

fn unavailable(gpu: &mut GpuMetrics, field: &'static str) {
    if gpu.unavailable.len() < MAX_GPU_UNAVAILABLE
        && !gpu.unavailable.iter().any(|existing| existing == field)
    {
        gpu.unavailable.push(field.to_owned());
    }
}

fn finalize_gpu(mut gpu: GpuMetrics, fallback_id: &str) -> GpuMetrics {
    gpu.name = bounded_text(&gpu.name, MAX_TEXT_BYTES);
    gpu.id = bounded_text(
        if gpu.id.trim().is_empty() {
            fallback_id
        } else {
            &gpu.id
        },
        MAX_TEXT_BYTES,
    );
    gpu.vendor = bounded_optional(gpu.vendor);
    gpu.pci_bus_id = bounded_optional(gpu.pci_bus_id);
    gpu.utilization_percent = valid_percent(gpu.utilization_percent);
    gpu.memory_pressure_percent = valid_percent(gpu.memory_pressure_percent);
    gpu.temperature_celsius = finite_temperature(gpu.temperature_celsius);
    gpu.power_draw_watts = finite_nonnegative(gpu.power_draw_watts);
    gpu.power_limit_watts = finite_nonnegative(gpu.power_limit_watts);
    gpu.fan_percent = valid_percent(gpu.fan_percent);
    gpu.thermal_state = bounded_optional(gpu.thermal_state);
    gpu.performance_state = bounded_optional(gpu.performance_state);
    gpu.driver_version = bounded_optional(gpu.driver_version);
    gpu.runtime_version = bounded_optional(gpu.runtime_version);
    gpu.compute_capability = bounded_optional(gpu.compute_capability);
    if gpu.utilization_percent.is_none() {
        unavailable(&mut gpu, "utilization");
    }
    if gpu.memory_used_bytes.is_none() {
        unavailable(&mut gpu, "memory used");
    }
    if gpu.memory_total_bytes.is_none() {
        unavailable(&mut gpu, "memory total");
    }
    if gpu.temperature_celsius.is_none() {
        unavailable(&mut gpu, "temperature");
    }
    if gpu.power_draw_watts.is_none() {
        unavailable(&mut gpu, "power draw");
    }
    if gpu.graphics_clock_mhz.is_none() {
        unavailable(&mut gpu, "graphics clock");
    }
    if gpu.memory_clock_mhz.is_none() {
        unavailable(&mut gpu, "memory clock");
    }
    if gpu.fan_percent.is_none() && gpu.fan_speed_rpm.is_none() {
        unavailable(&mut gpu, "fan");
    }
    if gpu.thermal_state.is_none() {
        unavailable(&mut gpu, "thermal state");
    }
    if gpu.driver_version.is_none() {
        unavailable(&mut gpu, "driver version");
    }
    if gpu.runtime_version.is_none() {
        unavailable(&mut gpu, "runtime version");
    }
    gpu
}

#[derive(Debug, PartialEq, Eq)]
enum CommandFailure {
    NotFound,
    TimedOut,
    OutputTooLarge,
    Failed,
    Io,
}

fn run_command_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<String, CommandFailure> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CommandFailure::NotFound
            } else {
                CommandFailure::Io
            }
        })?;
    let stdout = child.stdout.take().ok_or(CommandFailure::Io)?;
    let stderr = child.stderr.take().ok_or(CommandFailure::Io)?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout, COMMAND_STDOUT_LIMIT));
    let stderr_reader = thread::spawn(move || read_bounded(stderr, COMMAND_STDERR_LIMIT));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|_| CommandFailure::Io)? {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(CommandFailure::TimedOut);
            }
        }
    };
    let (stdout, stdout_truncated) = stdout_reader.join().map_err(|_| CommandFailure::Io)??;
    let (_, stderr_truncated) = stderr_reader.join().map_err(|_| CommandFailure::Io)??;
    if stdout_truncated || stderr_truncated {
        return Err(CommandFailure::OutputTooLarge);
    }
    if !status.success() {
        return Err(CommandFailure::Failed);
    }
    String::from_utf8(stdout).map_err(|_| CommandFailure::Io)
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<(Vec<u8>, bool), CommandFailure> {
    let take_limit = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(limit.min(16 * 1024));
    reader
        .take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| CommandFailure::Io)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, truncated))
}

fn command_diagnostic(source: &str, failure: &CommandFailure) -> GpuDiagnostic {
    let message = match failure {
        CommandFailure::NotFound => "collector command is not installed",
        CommandFailure::TimedOut => "collector command exceeded its time limit",
        CommandFailure::OutputTooLarge => "collector command exceeded its output limit",
        CommandFailure::Failed => "collector command returned an error",
        CommandFailure::Io => "collector command output could not be read",
    };
    GpuDiagnostic {
        source: source.to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn gpu_metrics() -> GpuSample {
    let mut sample = linux_drm_gpus();
    sample.extend(nvidia_gpus());
    sample
}

#[cfg(any(target_os = "linux", test))]
const NVIDIA_QUERY_FIELDS: usize = 16;

#[cfg(any(target_os = "linux", test))]
fn nvidia_gpus() -> GpuSample {
    let query = "--query-gpu=uuid,pci.bus_id,name,driver_version,pstate,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit,clocks.gr,clocks.mem,clocks.video,fan.speed,compute_cap";
    let program = if Path::new("/usr/bin/nvidia-smi").is_file() {
        "/usr/bin/nvidia-smi"
    } else {
        "nvidia-smi"
    };
    let output = run_command_bounded(
        program,
        &[query, "--format=csv,noheader,nounits"],
        COMMAND_TIMEOUT,
    );
    let has_nvidia = has_linux_pci_vendor("0x10de");
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let mut sample = GpuSample::default();
            if has_nvidia {
                sample
                    .diagnostics
                    .push(command_diagnostic("nvidia-smi", &error));
            }
            return sample;
        }
    };
    let runtime = run_command_bounded(program, &[], COMMAND_TIMEOUT)
        .ok()
        .and_then(|output| parse_nvidia_cuda_version(&output));
    let mut sample = GpuSample::default();
    for (index, line) in output.lines().take(MAX_GPU_DEVICES).enumerate() {
        if let Some(mut gpu) = parse_nvidia_line(line) {
            if let Some(runtime) = &runtime {
                gpu.runtime_version = Some(runtime.clone());
            }
            sample
                .gpus
                .push(finalize_gpu(gpu, &format!("nvidia-{index}")));
        } else if !line.trim().is_empty() {
            sample.diagnostic("nvidia-smi", "ignored a malformed GPU row");
        }
    }
    if output.lines().count() > MAX_GPU_DEVICES {
        sample.diagnostic("nvidia-smi", "device list truncated at the safety limit");
    }
    if sample.gpus.is_empty() && has_nvidia {
        sample.diagnostic("nvidia-smi", "returned no parseable GPU devices");
    }
    sample
}

#[cfg(any(target_os = "linux", test))]
fn parse_nvidia_line(line: &str) -> Option<GpuMetrics> {
    let fields = split_csv_line(line, NVIDIA_QUERY_FIELDS)?;
    if fields.len() != NVIDIA_QUERY_FIELDS || fields[2].trim().is_empty() {
        return None;
    }
    let uuid = parse_optional_text(&fields[0]);
    let pci_bus_id = parse_optional_text(&fields[1]).map(|value| normalize_pci_bus_id(&value));
    let id = uuid
        .clone()
        .or_else(|| pci_bus_id.clone())
        .unwrap_or_else(|| fields[2].clone());
    Some(GpuMetrics {
        id,
        name: fields[2].clone(),
        vendor: Some("NVIDIA".to_owned()),
        pci_bus_id,
        driver_version: parse_optional_text(&fields[3]),
        performance_state: parse_optional_text(&fields[4]),
        utilization_percent: parse_optional_u8(&fields[5]).filter(|value| *value <= 100),
        memory_used_bytes: parse_optional_u64(&fields[6]).and_then(|mib| mib.checked_mul(MIB)),
        memory_total_bytes: parse_optional_u64(&fields[7]).and_then(|mib| mib.checked_mul(MIB)),
        temperature_celsius: parse_optional_f32(&fields[8]),
        power_draw_watts: parse_optional_f32(&fields[9]),
        power_limit_watts: parse_optional_f32(&fields[10]),
        graphics_clock_mhz: parse_optional_u32(&fields[11]),
        memory_clock_mhz: parse_optional_u32(&fields[12]),
        video_clock_mhz: parse_optional_u32(&fields[13]),
        fan_percent: parse_optional_u8(&fields[14]).filter(|value| *value <= 100),
        compute_capability: parse_optional_text(&fields[15]),
        ..GpuMetrics::default()
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_nvidia_cuda_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (_, suffix) = line.split_once("CUDA Version:")?;
        let version = suffix
            .split(|character: char| character == '|' || character.is_whitespace())
            .find(|part| !part.is_empty())?;
        parse_optional_text(version).map(|version| format!("CUDA {version}"))
    })
}

#[cfg(any(target_os = "linux", test))]
fn split_csv_line(line: &str, max_fields: usize) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '"' if quoted && characters.peek() == Some(&'"') => {
                field.push('"');
                characters.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if fields.len() >= max_fields {
                    return None;
                }
                fields.push(bounded_text(field.trim(), MAX_TEXT_BYTES));
                field.clear();
            }
            _ if field.len() + character.len_utf8() <= MAX_TEXT_BYTES => field.push(character),
            _ => {}
        }
    }
    if quoted || fields.len() >= max_fields {
        return None;
    }
    fields.push(bounded_text(field.trim(), MAX_TEXT_BYTES));
    Some(fields)
}

fn parse_optional_text(value: &str) -> Option<String> {
    let value = bounded_text(value, MAX_TEXT_BYTES);
    (!value.is_empty() && !is_unavailable_value(&value)).then_some(value)
}

fn is_unavailable_value(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("n/a")
        || value.eq_ignore_ascii_case("not available")
        || value.eq_ignore_ascii_case("unsupported")
        || value == "-"
}

fn parse_optional_u8(value: &str) -> Option<u8> {
    parse_optional_u64(value).and_then(|value| u8::try_from(value).ok())
}

fn parse_optional_u32(value: &str) -> Option<u32> {
    numeric_prefix(value).and_then(|value| value.parse().ok())
}

fn parse_optional_u64(value: &str) -> Option<u64> {
    numeric_prefix(value).and_then(|value| value.parse().ok())
}

fn parse_optional_f32(value: &str) -> Option<f32> {
    numeric_prefix(value)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
}

fn numeric_prefix(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() || is_unavailable_value(value) {
        return None;
    }
    let end = value
        .char_indices()
        .take_while(|(_, character)| {
            character.is_ascii_digit() || matches!(character, '.' | '-' | '+')
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()?;
    Some(&value[..end])
}

#[cfg(any(target_os = "linux", test))]
fn has_linux_pci_vendor(expected: &str) -> bool {
    drm_card_devices()
        .iter()
        .any(|device| read_bounded_file(&device.join("vendor")).as_deref() == Some(expected))
}

#[cfg(any(target_os = "linux", test))]
fn linux_drm_gpus() -> GpuSample {
    let devices = drm_card_devices();
    let mut sample = GpuSample::default();
    if devices.len() >= MAX_DRM_ENTRIES {
        sample.diagnostic("linux-drm", "DRM device scan reached the safety limit");
    }
    for device in devices.into_iter().take(MAX_GPU_DEVICES) {
        match linux_drm_gpu(&device) {
            Ok(Some(gpu)) => sample.gpus.push(gpu),
            Ok(None) => {}
            Err(message) => sample.diagnostic("linux-sysfs", &message),
        }
    }
    sample
}

#[cfg(any(target_os = "linux", test))]
fn drm_card_devices() -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut devices = entries
        .take(MAX_DRM_ENTRIES)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name.starts_with("card")
                && name["card".len()..]
                    .chars()
                    .all(|character| character.is_ascii_digit()))
            .then(|| entry.path().join("device"))
        })
        .collect::<Vec<_>>();
    devices.sort();
    devices
}

#[cfg(any(target_os = "linux", test))]
fn linux_drm_gpu(device: &Path) -> Result<Option<GpuMetrics>, String> {
    let uevent_text = read_bounded_file(&device.join("uevent"))
        .ok_or_else(|| "a DRM device had no readable bounded uevent".to_owned())?;
    let uevent = parse_uevent(&uevent_text);
    let driver = uevent.get("DRIVER").map_or("", String::as_str);
    let vendor_id = read_bounded_file(&device.join("vendor"));
    let Some(vendor) = linux_gpu_vendor(driver, vendor_id.as_deref()) else {
        return Ok(None);
    };
    let slot = uevent.get("PCI_SLOT_NAME").cloned();
    let pci_id = uevent
        .get("PCI_ID")
        .cloned()
        .or_else(|| pci_id_from_sysfs(device));
    let product_name = read_bounded_file(&device.join("product_name"));
    let name = product_name
        .filter(|name| !name.is_empty())
        .or_else(|| pci_id.as_ref().map(|id| format!("{vendor} GPU ({id})")))
        .unwrap_or_else(|| format!("{vendor} GPU"));
    let stable = slot
        .clone()
        .or_else(|| pci_id.clone())
        .unwrap_or_else(|| name.clone());
    let mut gpu = GpuMetrics {
        id: format!("{}:{stable}", vendor.to_ascii_lowercase()),
        name,
        vendor: Some(vendor.to_owned()),
        pci_bus_id: slot,
        utilization_percent: read_first_number(
            device,
            &["gpu_busy_percent", "gt_busy_percent", "busy_percent"],
        )
        .and_then(|value| u8::try_from(value).ok())
        .filter(|value| *value <= 100),
        memory_used_bytes: read_first_number(
            device,
            &["mem_info_vram_used", "mem_info_local_memory_used"],
        ),
        memory_total_bytes: read_first_number(
            device,
            &["mem_info_vram_total", "mem_info_local_memory_total"],
        ),
        driver_version: (!driver.is_empty()).then(|| {
            read_bounded_file(&Path::new("/sys/module").join(driver).join("version"))
                .unwrap_or_else(|| driver.to_owned())
        }),
        runtime_version: read_first_text(device, &["vbios_version", "firmware_version"]),
        graphics_clock_mhz: read_first_number(
            device,
            &[
                "gt_cur_freq_mhz",
                "tile0/gt0/freq0/cur_freq",
                "tile0/gt0/freq0/act_freq",
            ],
        )
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| {
            read_bounded_file(&device.join("pp_dpm_sclk"))
                .and_then(|value| parse_active_dpm_clock(&value))
        }),
        memory_clock_mhz: read_bounded_file(&device.join("pp_dpm_mclk"))
            .and_then(|value| parse_active_dpm_clock(&value)),
        performance_state: read_first_text(
            device,
            &["power_dpm_force_performance_level", "power_dpm_state"],
        ),
        ..GpuMetrics::default()
    };
    apply_linux_hwmon(device, &mut gpu);
    Ok(Some(finalize_gpu(
        gpu,
        &format!("{}:{stable}", vendor.to_ascii_lowercase()),
    )))
}

#[cfg(any(target_os = "linux", test))]
fn linux_gpu_vendor(driver: &str, vendor_id: Option<&str>) -> Option<&'static str> {
    match driver {
        "amdgpu" | "radeon" => Some("AMD"),
        "i915" | "xe" => Some("Intel"),
        "nvidia" | "nouveau" => Some("NVIDIA"),
        _ => match vendor_id {
            Some("0x1002") => Some("AMD"),
            Some("0x8086") => Some("Intel"),
            Some("0x10de") => Some("NVIDIA"),
            _ => None,
        },
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_uevent(input: &str) -> BTreeMap<String, String> {
    input
        .lines()
        .take(64)
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (!key.is_empty() && key.len() <= 64)
                .then(|| (bounded_text(key, 64), bounded_text(value, MAX_TEXT_BYTES)))
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn pci_id_from_sysfs(device: &Path) -> Option<String> {
    let vendor = read_bounded_file(&device.join("vendor"))?;
    let model = read_bounded_file(&device.join("device"))?;
    Some(format!(
        "{}:{}",
        vendor.trim_start_matches("0x"),
        model.trim_start_matches("0x")
    ))
}

#[cfg(any(target_os = "linux", test))]
fn read_first_number(root: &Path, candidates: &[&str]) -> Option<u64> {
    candidates
        .iter()
        .find_map(|candidate| read_number(&root.join(candidate)))
}

#[cfg(any(target_os = "linux", test))]
fn read_first_text(root: &Path, candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find_map(|candidate| read_bounded_file(&root.join(candidate)))
        .filter(|value| !value.is_empty())
}

#[cfg(any(target_os = "linux", test))]
fn apply_linux_hwmon(device: &Path, gpu: &mut GpuMetrics) {
    let Ok(entries) = fs::read_dir(device.join("hwmon")) else {
        return;
    };
    let mut entries = entries
        .take(MAX_HWMON_ENTRIES)
        .flatten()
        .collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let root = entry.path();
        if gpu.temperature_celsius.is_none() {
            gpu.temperature_celsius = read_number(&root.join("temp1_input"))
                .and_then(|value| scaled_decimal(value, 1_000))
                .and_then(|value| finite_temperature(Some(value)))
                .map(round_one_decimal);
        }
        if gpu.power_draw_watts.is_none() {
            gpu.power_draw_watts = read_first_number(&root, &["power1_average", "power1_input"])
                .and_then(|value| scaled_decimal(value, 1_000_000));
        }
        if gpu.power_limit_watts.is_none() {
            gpu.power_limit_watts =
                read_first_number(&root, &["power1_cap", "power1_cap_default", "power1_crit"])
                    .and_then(|value| scaled_decimal(value, 1_000_000));
        }
        if gpu.fan_speed_rpm.is_none() {
            gpu.fan_speed_rpm =
                read_number(&root.join("fan1_input")).and_then(|value| u32::try_from(value).ok());
        }
        if gpu.fan_percent.is_none() {
            gpu.fan_percent = match (
                read_number(&root.join("pwm1")),
                read_number(&root.join("pwm1_max")),
            ) {
                (Some(value), Some(maximum)) if maximum > 0 => value
                    .saturating_mul(100)
                    .checked_div(maximum)
                    .and_then(|value| u8::try_from(value.min(100)).ok()),
                _ => None,
            };
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_active_dpm_clock(input: &str) -> Option<u32> {
    input.lines().take(64).find_map(|line| {
        let active = line.contains('*');
        let (_, frequency) = line.split_once(':')?;
        active.then(|| parse_optional_u32(frequency)).flatten()
    })
}

#[cfg(any(target_os = "linux", test))]
fn read_number(path: &Path) -> Option<u64> {
    read_bounded_file(path)?.trim().parse().ok()
}

#[cfg(any(target_os = "linux", test))]
fn read_bounded_file(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let take_limit = u64::try_from(MAX_SYSFS_BYTES.saturating_add(1)).ok()?;
    let mut bytes = Vec::with_capacity(256);
    file.take(take_limit).read_to_end(&mut bytes).ok()?;
    decode_bounded_file(bytes)
}

#[cfg(any(target_os = "linux", test))]
fn decode_bounded_file(bytes: Vec<u8>) -> Option<String> {
    if bytes.len() > MAX_SYSFS_BYTES {
        return None;
    }
    let value = String::from_utf8(bytes).ok()?;
    Some(value.trim().to_owned())
}

#[cfg(any(target_os = "linux", test))]
fn scaled_decimal(value: u64, scale: u64) -> Option<f32> {
    if scale == 0 {
        return None;
    }
    let whole = value / scale;
    let remainder = value % scale;
    let width = scale.checked_ilog10()?;
    format!(
        "{whole}.{remainder:0width$}",
        width = usize::try_from(width).ok()?
    )
    .parse::<f32>()
    .ok()
    .filter(|value| value.is_finite())
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, Default, PartialEq)]
struct MacIoGpu {
    id: Option<String>,
    name: Option<String>,
    utilization_percent: Option<u8>,
    temperature_celsius: Option<f32>,
    power_draw_watts: Option<f32>,
    graphics_clock_mhz: Option<u32>,
    fan_percent: Option<u8>,
    fan_speed_rpm: Option<u32>,
}

#[cfg(target_os = "macos")]
fn macos_gpus() -> GpuSample {
    let profiler = run_command_bounded(
        "/usr/sbin/system_profiler",
        &[
            "SPDisplaysDataType",
            "SPHardwareDataType",
            "-json",
            "-detailLevel",
            "mini",
        ],
        COMMAND_TIMEOUT,
    );
    let mut sample = match profiler {
        Ok(output) => GpuSample {
            gpus: parse_system_profiler_json(&output),
            diagnostics: Vec::new(),
        },
        Err(error) => GpuSample {
            gpus: Vec::new(),
            diagnostics: vec![command_diagnostic("system_profiler", &error)],
        },
    };

    let mut io_devices = Vec::new();
    for class in ["AGXAccelerator", "IOAccelerator"] {
        match run_command_bounded(
            "/usr/sbin/ioreg",
            &["-l", "-w", "0", "-r", "-c", class],
            COMMAND_TIMEOUT,
        ) {
            Ok(output) => merge_mac_io_devices(&mut io_devices, parse_ioreg_gpus(&output)),
            Err(CommandFailure::Failed) => {}
            Err(error) => {
                if sample.gpus.is_empty() || class == "AGXAccelerator" {
                    sample
                        .diagnostics
                        .push(command_diagnostic(&format!("ioreg:{class}"), &error));
                }
            }
        }
    }
    overlay_mac_io(&mut sample.gpus, &io_devices);

    let memory_pressure =
        match run_command_bounded("/usr/bin/memory_pressure", &["-Q"], COMMAND_TIMEOUT) {
            Ok(output) => parse_memory_pressure(&output),
            Err(error) => {
                sample
                    .diagnostics
                    .push(command_diagnostic("memory_pressure", &error));
                None
            }
        };
    let thermal_state =
        match run_command_bounded("/usr/bin/pmset", &["-g", "therm"], COMMAND_TIMEOUT) {
            Ok(output) => parse_pmset_thermal(&output),
            Err(error) => {
                sample.diagnostics.push(command_diagnostic("pmset", &error));
                None
            }
        };
    for (index, gpu) in sample.gpus.iter_mut().enumerate() {
        if gpu.memory_shared == Some(true) {
            gpu.memory_pressure_percent = memory_pressure;
        }
        if gpu.thermal_state.is_none() {
            gpu.thermal_state.clone_from(&thermal_state);
        }
        let fallback = format!("mac-gpu-{index}");
        *gpu = finalize_gpu(std::mem::take(gpu), &fallback);
    }
    sample.gpus.truncate(MAX_GPU_DEVICES);
    sample.diagnostics.truncate(MAX_GPU_DIAGNOSTICS);
    sample
}

#[cfg(any(target_os = "macos", test))]
fn parse_system_profiler_json(output: &str) -> Vec<GpuMetrics> {
    let Ok(document) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let shared_total = document
        .get("SPHardwareDataType")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| {
            value_text(
                item,
                &["physical_memory", "SPHardwareDataType_physical_memory"],
            )
        })
        .and_then(|value| parse_memory_size(&value));
    let Some(displays) = document.get("SPDisplaysDataType").and_then(Value::as_array) else {
        return Vec::new();
    };
    displays
        .iter()
        .take(MAX_GPU_DEVICES)
        .enumerate()
        .filter_map(|(index, item)| parse_system_profiler_gpu(item, index, shared_total))
        .collect()
}

#[cfg(any(target_os = "macos", test))]
fn parse_system_profiler_gpu(
    item: &Value,
    index: usize,
    shared_total: Option<u64>,
) -> Option<GpuMetrics> {
    let name = value_text(item, &["sppci_model", "_name", "spdisplays_chipset-model"])?;
    let vendor_text = value_text(
        item,
        &["spdisplays_vendor", "spdisplays_vendor-id", "sppci_vendor"],
    );
    let vendor = vendor_text.as_deref().map(mac_vendor).or_else(|| {
        let lowered = name.to_ascii_lowercase();
        if lowered.contains("apple") {
            Some("Apple".to_owned())
        } else if lowered.contains("intel") {
            Some("Intel".to_owned())
        } else if lowered.contains("amd") || lowered.contains("radeon") {
            Some("AMD".to_owned())
        } else {
            None
        }
    });
    let device_id = value_text(item, &["spdisplays_device-id", "sppci_device"]);
    let vendor_id = vendor_text
        .as_deref()
        .and_then(parenthesized_hex)
        .or_else(|| value_text(item, &["spdisplays_vendor-id"]));
    let registry_id = value_text(item, &["_spdisplays_display-registryid"]);
    let id = registry_id.unwrap_or_else(|| {
        format!(
            "mac:{}:{}:{}",
            vendor_id.as_deref().unwrap_or("vendor"),
            device_id.as_deref().unwrap_or("device"),
            index
        )
    });
    // Current Apple Silicon `system_profiler -detailLevel mini` output omits
    // `spdisplays_vram_shared`. An Apple GPU on the built-in bus still uses
    // unified system memory, so derive that stable hardware fact rather than
    // reporting its memory as dedicated or unavailable.
    let shared = value_text(item, &["spdisplays_vram_shared"])
        .is_some_and(|value| !value.eq_ignore_ascii_case("no"))
        || (vendor.as_deref() == Some("Apple")
            && value_text(item, &["sppci_bus"])
                .is_some_and(|value| value.eq_ignore_ascii_case("spdisplays_builtin")));
    let dedicated_memory = value_text(
        item,
        &[
            "spdisplays_vram",
            "spdisplays_vram_dynamic",
            "spdisplays_vram_total",
        ],
    )
    .and_then(|value| parse_memory_size(&value));
    Some(GpuMetrics {
        id,
        name,
        vendor,
        memory_total_bytes: if shared {
            shared_total
        } else {
            dedicated_memory
        },
        memory_shared: Some(shared),
        runtime_version: value_text(
            item,
            &[
                "spdisplays_metal",
                "spdisplays_mtlgpufamilysupport",
                "spdisplays_metalfeatureset",
            ],
        ),
        driver_version: value_text(item, &["spdisplays_kext_info", "spdisplays_driver-version"]),
        core_count: value_text(item, &["sppci_cores", "spdisplays_gpu-cores"])
            .and_then(|value| parse_optional_u32(&value)),
        ..GpuMetrics::default()
    })
}

#[cfg(any(target_os = "macos", test))]
fn value_text(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        match value {
            Value::String(value) => parse_optional_text(value),
            Value::Number(value) => parse_optional_text(&value.to_string()),
            _ => None,
        }
    })
}

#[cfg(any(target_os = "macos", test))]
fn mac_vendor(value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    if lowered.contains("apple") || lowered.contains("106b") {
        "Apple".to_owned()
    } else if lowered.contains("intel") || lowered.contains("8086") {
        "Intel".to_owned()
    } else if lowered.contains("amd") || lowered.contains("ati") || lowered.contains("1002") {
        "AMD".to_owned()
    } else if lowered.contains("nvidia") || lowered.contains("10de") {
        "NVIDIA".to_owned()
    } else {
        bounded_text(value, MAX_TEXT_BYTES)
    }
}

#[cfg(any(target_os = "macos", test))]
fn parenthesized_hex(value: &str) -> Option<String> {
    let (_, suffix) = value.split_once("(0x")?;
    let (hex, _) = suffix.split_once(')')?;
    (!hex.is_empty() && hex.chars().all(|character| character.is_ascii_hexdigit()))
        .then(|| format!("0x{hex}"))
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let number = parts.next()?.replace(',', "");
    let unit = parts.next()?.to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "kb" | "kib" => 1024_u64,
        "mb" | "mib" => MIB,
        "gb" | "gib" => MIB.checked_mul(1024)?,
        "tb" | "tib" => MIB.checked_mul(1024)?.checked_mul(1024)?,
        _ => return None,
    };
    parse_decimal_scaled(&number, multiplier)
}

#[cfg(any(target_os = "macos", test))]
fn parse_decimal_scaled(value: &str, multiplier: u64) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = whole.parse::<u64>().ok()?.checked_mul(multiplier)?;
    if fraction.is_empty() {
        return Some(whole);
    }
    if !fraction.chars().all(|character| character.is_ascii_digit()) || fraction.len() > 6 {
        return None;
    }
    let denominator = 10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let fractional = fraction
        .parse::<u64>()
        .ok()?
        .checked_mul(multiplier)?
        .checked_div(denominator)?;
    whole.checked_add(fractional)
}

#[cfg(any(target_os = "macos", test))]
fn parse_ioreg_gpus(output: &str) -> Vec<MacIoGpu> {
    let mut devices = Vec::new();
    let mut current: Option<MacIoGpu> = None;
    for line in output.lines().take(8_192) {
        // `ioreg -r -c` prints a matching accelerator root followed by its
        // entire child tree. User-client children also have class/id markers,
        // but they are not GPUs and must not consume the bounded device list.
        if line.starts_with("+-o ") && line.contains("<class ") && line.contains(" id 0x") {
            if let Some(device) = current.take()
                && mac_io_has_data(&device)
                && devices.len() < MAX_GPU_DEVICES
            {
                devices.push(device);
            }
            let name = line
                .split("+-o ")
                .nth(1)
                .and_then(|value| value.split("  <class").next())
                .and_then(parse_optional_text);
            let id = line
                .split(" id ")
                .nth(1)
                .and_then(|value| value.split(',').next())
                .and_then(parse_optional_text);
            current = Some(MacIoGpu {
                id,
                name,
                ..MacIoGpu::default()
            });
        }
        let Some(device) = current.as_mut() else {
            continue;
        };
        device.utilization_percent = device.utilization_percent.or_else(|| {
            find_key_number(
                line,
                &[
                    "Device Utilization %",
                    "GPU Core Utilization",
                    "GPU Activity(%)",
                ],
            )
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= 100)
        });
        device.temperature_celsius = device.temperature_celsius.or_else(|| {
            find_key_float(line, &["GPU Temperature(C)", "Temperature(C)"])
                .and_then(|value| finite_temperature(Some(value)))
        });
        device.power_draw_watts = device.power_draw_watts.or_else(|| {
            find_key_float(line, &["GPU Power(W)", "Power(W)"])
                .and_then(|value| finite_nonnegative(Some(value)))
        });
        device.graphics_clock_mhz = device.graphics_clock_mhz.or_else(|| {
            find_key_number(line, &["GPU Clock(MHz)", "Core Clock(MHz)"])
                .and_then(|value| u32::try_from(value).ok())
        });
        device.fan_percent = device.fan_percent.or_else(|| {
            find_key_number(line, &["Fan Speed(%)"])
                .and_then(|value| u8::try_from(value).ok())
                .filter(|value| *value <= 100)
        });
        device.fan_speed_rpm = device.fan_speed_rpm.or_else(|| {
            find_key_number(line, &["Fan Speed(RPM)"]).and_then(|value| u32::try_from(value).ok())
        });
    }
    if let Some(device) = current
        && mac_io_has_data(&device)
        && devices.len() < MAX_GPU_DEVICES
    {
        devices.push(device);
    }
    devices
}

#[cfg(any(target_os = "macos", test))]
fn mac_io_has_data(device: &MacIoGpu) -> bool {
    device.id.is_some()
        || device.utilization_percent.is_some()
        || device.temperature_celsius.is_some()
        || device.power_draw_watts.is_some()
        || device.graphics_clock_mhz.is_some()
}

#[cfg(any(target_os = "macos", test))]
fn find_key_number(line: &str, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        let offset = line.find(key)? + key.len();
        numeric_prefix(line[offset..].trim_start_matches(['"', '=', ' ', ':']))?
            .parse()
            .ok()
    })
}

#[cfg(any(target_os = "macos", test))]
fn find_key_float(line: &str, keys: &[&str]) -> Option<f32> {
    keys.iter().find_map(|key| {
        let offset = line.find(key)? + key.len();
        parse_optional_f32(line[offset..].trim_start_matches(['"', '=', ' ', ':']))
    })
}

#[cfg(target_os = "macos")]
fn merge_mac_io_devices(target: &mut Vec<MacIoGpu>, incoming: Vec<MacIoGpu>) {
    for device in incoming {
        if target
            .iter()
            .any(|existing| existing.id.is_some() && existing.id == device.id)
        {
            continue;
        }
        if target.len() < MAX_GPU_DEVICES {
            target.push(device);
        }
    }
}

#[cfg(target_os = "macos")]
fn overlay_mac_io(gpus: &mut Vec<GpuMetrics>, io_devices: &[MacIoGpu]) {
    if gpus.is_empty() {
        for (index, io) in io_devices.iter().take(MAX_GPU_DEVICES).enumerate() {
            gpus.push(GpuMetrics {
                id: io
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("mac-ioreg-{index}")),
                name: io.name.clone().unwrap_or_else(|| "macOS GPU".to_owned()),
                utilization_percent: io.utilization_percent,
                temperature_celsius: io.temperature_celsius,
                power_draw_watts: io.power_draw_watts,
                graphics_clock_mhz: io.graphics_clock_mhz,
                fan_percent: io.fan_percent,
                fan_speed_rpm: io.fan_speed_rpm,
                ..GpuMetrics::default()
            });
        }
        return;
    }
    if io_devices.len() == gpus.len() || io_devices.len() == 1 {
        for (gpu, io) in gpus.iter_mut().zip(io_devices) {
            if let Some(id) = &io.id {
                gpu.id.clone_from(id);
            }
            gpu.utilization_percent = gpu.utilization_percent.or(io.utilization_percent);
            gpu.temperature_celsius = gpu.temperature_celsius.or(io.temperature_celsius);
            gpu.power_draw_watts = gpu.power_draw_watts.or(io.power_draw_watts);
            gpu.graphics_clock_mhz = gpu.graphics_clock_mhz.or(io.graphics_clock_mhz);
            gpu.fan_percent = gpu.fan_percent.or(io.fan_percent);
            gpu.fan_speed_rpm = gpu.fan_speed_rpm.or(io.fan_speed_rpm);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn parse_memory_pressure(output: &str) -> Option<u8> {
    output.lines().find_map(|line| {
        let (_, value) = line.split_once("System-wide memory free percentage:")?;
        let free = parse_optional_u8(value.trim().trim_end_matches('%'))?;
        (free <= 100).then_some(100 - free)
    })
}

#[cfg(any(target_os = "macos", test))]
fn parse_pmset_thermal(output: &str) -> Option<String> {
    if output
        .to_ascii_lowercase()
        .contains("no thermal warning level has been recorded")
    {
        return Some("normal; no thermal warning recorded".to_owned());
    }
    let mut limits = Vec::new();
    for line in output.lines().take(64) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if matches!(
            key,
            "Thermal_Level" | "CPU_Speed_Limit" | "GPU_Speed_Limit" | "Scheduler_Limit"
        ) && limits.len() < 4
        {
            limits.push(format!(
                "{}={}",
                bounded_text(key, 32),
                bounded_text(value, 16)
            ));
        }
    }
    (!limits.is_empty()).then(|| limits.join(", "))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const NVIDIA_FIXTURE: &str = "GPU-1234, 00000000:01:00.0, NVIDIA RTX 5090, 590.12, P2, 41, 12000, 32768, 66, 312.4, 575.0, 2415, 14001, 2100, 47, 12.0";
    const APPLE_PROFILER_FIXTURE: &str = r#"{
      "SPHardwareDataType": [{"physical_memory": "24 GB"}],
      "SPDisplaysDataType": [{
        "_name": "Apple M4 Pro",
        "sppci_model": "Apple M4 Pro",
        "spdisplays_vendor": "Apple (0x106b)",
        "spdisplays_device-id": "0x0001",
        "spdisplays_vram_shared": "spdisplays_shared",
        "spdisplays_metal": "Metal 4",
        "spdisplays_gpu-cores": "20"
      }]
    }"#;
    const INTEL_PROFILER_FIXTURE: &str = r#"{
      "SPDisplaysDataType": [{
        "_name": "Intel UHD Graphics 630",
        "spdisplays_vendor": "Intel (0x8086)",
        "spdisplays_device-id": "0x3e9b",
        "spdisplays_vram": "1536 MB",
        "spdisplays_metal": "Supported, feature set macOS GPUFamily2 v1"
      }]
    }"#;
    const MIDNIGHT_PROFILER_FIXTURE: &str = r#"{
      "SPHardwareDataType": [{"physical_memory": "64 GB"}],
      "SPDisplaysDataType": [{
        "_name": "Apple M4 Max",
        "spdisplays_vendor": "sppci_vendor_Apple",
        "sppci_bus": "spdisplays_builtin",
        "sppci_cores": "40",
        "spdisplays_mtlgpufamilysupport": "spdisplays_metal4"
      }]
    }"#;
    const IOREG_FIXTURE: &str = r#"+-o AGXAccelerator  <class AGXAccelerator, id 0x1000006fb, registered, matched, active, busy 0 (1 ms), retain 8>
    {
      "PerformanceStatistics" = {"Device Utilization %"=37,"GPU Clock(MHz)"=1296,"GPU Power(W)"=12.5,"GPU Temperature(C)"=54.25,"Fan Speed(RPM)"=1840}
      +-o AGXDeviceUserClient  <class AGXDeviceUserClient, id 0x100001148, !registered, !matched, active, busy 0, retain 5>
    }"#;

    #[test]
    fn parses_complete_nvidia_csv_without_units() {
        let gpu = parse_nvidia_line(NVIDIA_FIXTURE).unwrap();
        assert_eq!(gpu.id, "GPU-1234");
        assert_eq!(gpu.pci_bus_id.as_deref(), Some("0000:01:00.0"));
        assert_eq!(gpu.name, "NVIDIA RTX 5090");
        assert_eq!(gpu.utilization_percent, Some(41));
        assert_eq!(gpu.memory_used_bytes, Some(12_000 * MIB));
        assert_eq!(gpu.memory_total_bytes, Some(32_768 * MIB));
        assert_eq!(gpu.temperature_celsius, Some(66.0));
        assert_eq!(gpu.power_draw_watts, Some(312.4));
        assert_eq!(gpu.power_limit_watts, Some(575.0));
        assert_eq!(gpu.graphics_clock_mhz, Some(2_415));
        assert_eq!(gpu.memory_clock_mhz, Some(14_001));
        assert_eq!(gpu.video_clock_mhz, Some(2_100));
        assert_eq!(gpu.fan_percent, Some(47));
        assert_eq!(gpu.driver_version.as_deref(), Some("590.12"));
        assert_eq!(gpu.compute_capability.as_deref(), Some("12.0"));
    }

    #[test]
    fn nvidia_csv_handles_quotes_and_unavailable_fields() {
        let line = "GPU-1, 0000:01:00.0, \"NVIDIA, Test GPU\", 1.2, P0, N/A, N/A, 1024, N/A, N/A, N/A, N/A, N/A, N/A, N/A, N/A";
        let gpu = parse_nvidia_line(line).unwrap();
        assert_eq!(gpu.name, "NVIDIA, Test GPU");
        assert_eq!(gpu.utilization_percent, None);
        assert_eq!(gpu.memory_total_bytes, Some(1024 * MIB));
        assert_eq!(gpu.power_draw_watts, None);
        assert!(parse_nvidia_line("one,two").is_none());
        assert!(parse_nvidia_line("\"unterminated").is_none());
    }

    #[test]
    fn parses_nvidia_runtime_banner() {
        let output = "| NVIDIA-SMI 590.12 Driver Version: 590.12 CUDA Version: 13.1 |\n";
        assert_eq!(
            parse_nvidia_cuda_version(output).as_deref(),
            Some("CUDA 13.1")
        );
    }

    #[test]
    fn parses_linux_uevent_and_active_clocks() {
        let bounded = decode_bounded_file(
            b"DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:73BF\nPCI_SLOT_NAME=0000:03:00.0\n"
                .to_vec(),
        )
        .unwrap();
        assert_eq!(bounded.lines().count(), 4);
        let uevent = parse_uevent(&bounded);
        assert_eq!(uevent.get("DRIVER").map(String::as_str), Some("amdgpu"));
        assert_eq!(
            uevent.get("PCI_SLOT_NAME").map(String::as_str),
            Some("0000:03:00.0")
        );
        assert_eq!(
            parse_active_dpm_clock("0: 500Mhz\n1: 2100Mhz *\n"),
            Some(2_100)
        );
        assert_eq!(scaled_decimal(54_250, 1_000), Some(54.25));
        assert_eq!(scaled_decimal(187_500_000, 1_000_000), Some(187.5));
        assert_eq!(linux_gpu_vendor("nouveau", Some("0x10de")), Some("NVIDIA"));
        assert_eq!(linux_gpu_vendor("", Some("0x8086")), Some("Intel"));
        assert_eq!(linux_gpu_vendor("virtio_gpu", Some("0x1af4")), None);
        assert_eq!(normalize_pci_bus_id("00000000:03:00.0"), "0000:03:00.0");
    }

    #[test]
    fn richer_duplicate_gpu_replaces_sysfs_fallback() {
        let mut sample = GpuSample {
            gpus: vec![GpuMetrics {
                id: "nvidia:0000:01:00.0".to_owned(),
                name: "NVIDIA GPU (10de:2684)".to_owned(),
                vendor: Some("NVIDIA".to_owned()),
                pci_bus_id: Some("0000:01:00.0".to_owned()),
                driver_version: Some("nouveau".to_owned()),
                ..GpuMetrics::default()
            }],
            diagnostics: Vec::new(),
        };
        sample.extend(GpuSample {
            gpus: vec![parse_nvidia_line(NVIDIA_FIXTURE).unwrap()],
            diagnostics: Vec::new(),
        });
        assert_eq!(sample.gpus.len(), 1);
        assert_eq!(sample.gpus[0].id, "GPU-1234");
        assert_eq!(sample.gpus[0].utilization_percent, Some(41));
    }

    #[test]
    fn parses_apple_silicon_system_profiler_fixture() {
        let gpus = parse_system_profiler_json(APPLE_PROFILER_FIXTURE);
        assert_eq!(gpus.len(), 1);
        let gpu = &gpus[0];
        assert_eq!(gpu.name, "Apple M4 Pro");
        assert_eq!(gpu.vendor.as_deref(), Some("Apple"));
        assert_eq!(gpu.memory_shared, Some(true));
        assert_eq!(gpu.memory_total_bytes, Some(24 * 1024 * MIB));
        assert_eq!(gpu.runtime_version.as_deref(), Some("Metal 4"));
        assert_eq!(gpu.core_count, Some(20));
    }

    #[test]
    fn infers_unified_memory_from_current_apple_silicon_mini_output() {
        let gpus = parse_system_profiler_json(MIDNIGHT_PROFILER_FIXTURE);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].memory_shared, Some(true));
        assert_eq!(gpus[0].memory_total_bytes, Some(64 * 1024 * MIB));
        assert_eq!(gpus[0].core_count, Some(40));
        assert_eq!(
            gpus[0].runtime_version.as_deref(),
            Some("spdisplays_metal4")
        );
    }

    #[test]
    fn parses_intel_mac_system_profiler_fixture() {
        let gpus = parse_system_profiler_json(INTEL_PROFILER_FIXTURE);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].vendor.as_deref(), Some("Intel"));
        assert_eq!(gpus[0].memory_shared, Some(false));
        assert_eq!(gpus[0].memory_total_bytes, Some(1_536 * MIB));
    }

    #[test]
    fn parses_ioreg_utilization_power_temperature_clock_and_fan() {
        let devices = parse_ioreg_gpus(IOREG_FIXTURE);
        assert_eq!(devices.len(), 1);
        let gpu = &devices[0];
        assert_eq!(gpu.id.as_deref(), Some("0x1000006fb"));
        assert_eq!(gpu.utilization_percent, Some(37));
        assert_eq!(gpu.temperature_celsius, Some(54.25));
        assert_eq!(gpu.power_draw_watts, Some(12.5));
        assert_eq!(gpu.graphics_clock_mhz, Some(1_296));
        assert_eq!(gpu.fan_speed_rpm, Some(1_840));
        assert_eq!(devices.len(), 1, "user-client children are not GPU devices");
    }

    #[test]
    fn parses_mac_memory_pressure_and_thermal_state() {
        assert_eq!(
            parse_memory_pressure("System-wide memory free percentage: 64%\n"),
            Some(36)
        );
        assert_eq!(
            parse_pmset_thermal("Note: No thermal warning level has been recorded\n").as_deref(),
            Some("normal; no thermal warning recorded")
        );
        assert_eq!(
            parse_pmset_thermal(
                "CPU_Speed_Limit = 80\nGPU_Speed_Limit = 70\nScheduler_Limit = 90\n"
            )
            .as_deref(),
            Some("CPU_Speed_Limit=80, GPU_Speed_Limit=70, Scheduler_Limit=90")
        );
    }

    #[test]
    fn finalizer_names_unavailable_counters_instead_of_zeroing() {
        let gpu = finalize_gpu(
            GpuMetrics {
                name: "Intel GPU".to_owned(),
                vendor: Some("Intel".to_owned()),
                driver_version: Some("i915".to_owned()),
                ..GpuMetrics::default()
            },
            "intel:0000:00:02.0",
        );
        assert_eq!(gpu.utilization_percent, None);
        assert!(gpu.unavailable.iter().any(|field| field == "utilization"));
        assert!(gpu.unavailable.iter().any(|field| field == "temperature"));
        assert!(
            !gpu.unavailable
                .iter()
                .any(|field| field == "driver version")
        );
    }

    #[test]
    fn old_machine_metrics_payload_remains_compatible() {
        let json = r#"{
          "cpu_percent": 12,
          "memory_used_bytes": 100,
          "memory_total_bytes": 200,
          "gpus": [{
            "name": "legacy GPU",
            "utilization_percent": 50,
            "memory_used_bytes": 10,
            "memory_total_bytes": 20,
            "temperature_celsius": 60.0
          }],
          "temperatures": []
        }"#;
        let metrics: MachineMetrics = serde_json::from_str(json).unwrap();
        assert_eq!(metrics.gpus[0].name, "legacy GPU");
        assert_eq!(metrics.gpus[0].id, "legacy GPU");
        assert!(metrics.gpu_diagnostics.is_empty());
    }

    #[test]
    fn remote_machine_metrics_are_capped_and_sanitized() {
        let gpus = (0..(MAX_GPU_DEVICES + 5))
            .map(|index| {
                serde_json::json!({
                    "id": format!("gpu-{index}"),
                    "name": "x".repeat(MAX_TEXT_BYTES + 50),
                    "utilization_percent": 200,
                    "memory_used_bytes": null,
                    "memory_total_bytes": null,
                    "temperature_celsius": 500.0,
                    "unavailable": (0..(MAX_GPU_UNAVAILABLE + 5)).map(|n| format!("field-{n}")).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let metrics: MachineMetrics = serde_json::from_value(serde_json::json!({
            "gpus": gpus,
            "gpu_diagnostics": (0..(MAX_GPU_DIAGNOSTICS + 5)).map(|n| serde_json::json!({"source":"test", "message":format!("message-{n}")})).collect::<Vec<_>>()
        }))
        .unwrap();
        assert_eq!(metrics.gpus.len(), MAX_GPU_DEVICES);
        assert_eq!(metrics.gpu_diagnostics.len(), MAX_GPU_DIAGNOSTICS);
        assert!(metrics.gpus[0].name.len() <= MAX_TEXT_BYTES);
        assert_eq!(metrics.gpus[0].utilization_percent, None);
        assert_eq!(metrics.gpus[0].temperature_celsius, None);
        assert_eq!(metrics.gpus[0].unavailable.len(), MAX_GPU_UNAVAILABLE);

        let serialized = serde_json::to_value(MachineMetrics {
            cpu_percent: Some(200),
            gpus: (0..(MAX_GPU_DEVICES + 5))
                .map(|index| GpuMetrics {
                    id: format!("gpu-{index}"),
                    name: "x".repeat(MAX_TEXT_BYTES + 50),
                    utilization_percent: Some(200),
                    temperature_celsius: Some(500.0),
                    unavailable: (0..(MAX_GPU_UNAVAILABLE + 5))
                        .map(|n| format!("field-{n}"))
                        .collect(),
                    ..GpuMetrics::default()
                })
                .collect(),
            ..MachineMetrics::default()
        })
        .unwrap();
        assert_eq!(serialized["cpu_percent"], serde_json::Value::Null);
        assert_eq!(
            serialized["gpus"].as_array().unwrap().len(),
            MAX_GPU_DEVICES
        );
        assert_eq!(
            serialized["gpus"][0]["utilization_percent"],
            serde_json::Value::Null
        );
        assert_eq!(
            serialized["gpus"][0]["temperature_celsius"],
            serde_json::Value::Null
        );
        assert_eq!(
            serialized["gpus"][0]["unavailable"]
                .as_array()
                .unwrap()
                .len(),
            MAX_GPU_UNAVAILABLE
        );
    }

    #[test]
    fn bounded_reader_reports_oversized_output() {
        let (bytes, truncated) = read_bounded(Cursor::new(vec![b'x'; 9]), 8).unwrap();
        assert_eq!(bytes.len(), 8);
        assert!(truncated);
    }
}

#[cfg(target_os = "macos")]
fn gpu_metrics() -> GpuSample {
    macos_gpus()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn gpu_metrics() -> GpuSample {
    let mut sample = GpuSample::default();
    sample.diagnostic(
        "gpu",
        "GPU telemetry is unavailable on this operating system",
    );
    sample
}
