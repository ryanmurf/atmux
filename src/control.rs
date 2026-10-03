use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::RandomState},
    fmt::{self, Write as _},
    fs::{self, File},
    hash::{BuildHasher, DefaultHasher, Hash, Hasher},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use fs2::FileExt;
use rmcp::schemars::JsonSchema;
use rustix::{
    fs::{Mode, OFlags},
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Notify, watch},
    task::AbortHandle,
};

use crate::{
    attachment::{self, DeliveryErrorKind, ImageMessageRequest},
    auto_compact::{self, Decision as AutoCompactDecision},
    auto_update::{self, Harness as UpdateHarness, PendingMarker},
    code_nav::{CodeNavRequest, CodeNavResponse},
    config::{AgentProfile, Config, ProfileMode},
    launch_directory,
    machine::{
        MachineKind, MachineSummary, composite_id, now_ms, split_composite, validate_machine_id,
    },
    metrics::{HardwareSampler, MachineMetrics},
    old_sessions::{self, DiscoveryLimits, ResumeCandidate},
    project::{self, ProjectPreferences},
    recovery::{RecoveryRunner, RecoveryStartError, RecoveryStatus},
    remote::{self, RemoteMachine, encode_segment},
    self_update::{self, SelfUpdater, UpdateStatus},
    status::{AgentKind, AgentStatus},
    summary, systemd_scope,
    tmux::{
        DescriptionUpdate, PaneSpecialKey, RESERVED_SERVICE_SESSION, Session, Tmux,
        UnsupportedModelControl, fast_toggle_verified, known_efforts, known_models,
        valid_pane_identity,
    },
    transcript::Transcript,
    workspace::{FileWriteRequest, FilesResponse, GitResponse, WorkspaceErrorKind},
};

/// How a control-plane failure should be reported to a caller.
///
/// Classifying at the point of failure keeps HTTP and MCP surfaces from having
/// to guess from message text whose fault a failure was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The caller asked for something malformed, unknown, or ambiguous.
    BadRequest,
    /// The referenced session or pane does not exist.
    NotFound,
    /// The request conflicts with state that already exists.
    Conflict,
    /// The owning machine is configured but currently unreachable.
    Offline,
    /// The owning machine rejected the request, or its transport failed.
    Upstream,
    /// This coordinator itself failed.
    Internal,
}

/// A classified control-plane failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlError {
    kind: ErrorKind,
    message: String,
}

impl ControlError {
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ControlError {}

/// Bounds for fitting a detached pane's window to a browser viewport.
pub const MIN_FIT_COLS: u16 = 40;
pub const MAX_FIT_COLS: u16 = 400;
pub const MIN_FIT_ROWS: u16 = 10;
pub const MAX_FIT_ROWS: u16 = 200;

/// A browser's raw-view size in terminal cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaneSizeRequest {
    pub cols: u16,
    pub rows: u16,
}

/// What the owner did: `resized`, `unchanged`, `attached` (a terminal owns
/// the size) or `split` (the window holds more than one pane).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSizeResponse {
    pub fit: String,
}

/// How long one node has to answer a fleet update read before its entry
/// reports a timeout instead of holding up every other machine.
const FLEET_UPDATE_TIMEOUT: Duration = Duration::from_secs(5);
/// The owner-scoped read every node exposes for its own executable.
const UPDATE_STATUS_PATH: &str = "/api/v1/update";

/// One machine's self-update state as seen by a coordinator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FleetUpdate {
    pub id: String,
    pub label: String,
    pub online: bool,
    /// Absent when the machine could not be read; `error` says why.
    pub update: Option<UpdateStatus>,
    pub error: Option<String>,
}

/// One machine's Quick Resume state as seen by a coordinator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FleetRecovery {
    pub id: String,
    pub label: String,
    pub online: bool,
    /// Absent when the machine could not be read; `error` says why.
    pub recovery: Option<RecoveryStatus>,
    pub error: Option<String>,
}

/// Classifies any control-plane failure.
///
/// Unclassified failures are treated as this coordinator's own fault, which is
/// the only safe default: a caller must never be told its request was invalid
/// because of a local bug.
#[must_use]
pub fn error_kind(error: &anyhow::Error) -> ErrorKind {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ControlError>())
        .map_or(ErrorKind::Internal, ControlError::kind)
}

fn bad_request(message: impl Into<String>) -> anyhow::Error {
    ControlError::new(ErrorKind::BadRequest, message).into()
}

fn not_found(message: impl Into<String>) -> anyhow::Error {
    ControlError::new(ErrorKind::NotFound, message).into()
}

fn conflict(message: impl Into<String>) -> anyhow::Error {
    ControlError::new(ErrorKind::Conflict, message).into()
}

fn offline(message: impl Into<String>) -> anyhow::Error {
    ControlError::new(ErrorKind::Offline, message).into()
}

/// Marks a failure as belonging to the owning machine rather than the caller.
///
/// The remote error is flattened into the message so no detail is lost.
fn upstream(error: &anyhow::Error) -> anyhow::Error {
    ControlError::new(ErrorKind::Upstream, format!("{error:#}")).into()
}

/// Preserves safe owner-side mutation classifications without reflecting the
/// owner's response body through a coordinator.
fn remote_mutation_error(error: &anyhow::Error) -> anyhow::Error {
    match remote::rejected_status(error) {
        Some(400) => bad_request("owning machine rejected the mutation as invalid"),
        Some(404) => not_found("owning machine no longer has the requested resource"),
        Some(409) => conflict("owning machine reports that the target changed"),
        Some(status) => ControlError::new(
            ErrorKind::Upstream,
            format!("owning machine rejected the mutation with HTTP {status}"),
        )
        .into(),
        _ => upstream(error),
    }
}

/// Marks a failure as this coordinator's own.
fn internal(error: &anyhow::Error) -> anyhow::Error {
    ControlError::new(ErrorKind::Internal, format!("{error:#}")).into()
}

fn workspace_error(error: &crate::workspace::WorkspaceError) -> anyhow::Error {
    match error.kind() {
        WorkspaceErrorKind::Invalid => bad_request(error.to_string()),
        WorkspaceErrorKind::NotFound => not_found(error.to_string()),
        WorkspaceErrorKind::Conflict => conflict(error.to_string()),
        WorkspaceErrorKind::Internal => {
            ControlError::new(ErrorKind::Internal, error.to_string()).into()
        }
    }
}

fn launch_directory_error(error: &launch_directory::ActionError) -> anyhow::Error {
    match error.kind() {
        launch_directory::ErrorKind::Invalid => bad_request(error.to_string()),
        launch_directory::ErrorKind::Conflict => conflict(error.to_string()),
        launch_directory::ErrorKind::Internal => {
            ControlError::new(ErrorKind::Internal, error.to_string()).into()
        }
    }
}

/// A duplicate's summarized context waits for the new CLI's empty composer.
/// The window covers a slow first start; beyond it the owner still has a
/// running agent and can paste the handover themselves.
const HANDOVER_DELIVERY_TIMEOUT: Duration = Duration::from_secs(180);
const HANDOVER_DELIVERY_POLL: Duration = Duration::from_secs(2);
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_BROWSE_PATH_BYTES: usize = 4_096;
const MAX_BROWSE_SCAN_ENTRIES: usize = 4_096;
const MAX_BROWSE_DIRECTORIES: usize = 512;
/// Remote pane fetches always request the full bounded window so one fetch can
/// serve every browser and MCP caller until the pane's hash changes again.
const REMOTE_FETCH_LINES: usize = 2_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    /// Stable composite identity: `machine~pane`.
    pub id: String,
    /// Opaque owner-issued pane generation used to scope browser-local state.
    ///
    /// Unlike a tmux pane id, this changes when a deleted pane id is reused.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance_id: String,
    /// Stable agent session identity (`UUIDv7`) that survives respawns,
    /// renames, archive and resume on another machine. Absent from older
    /// owners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Machine that owns this session.
    #[serde(default)]
    pub machine: String,
    pub name: String,
    /// Optional short note set from the dashboard; absent from older owners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_source: Option<String>,
    pub pane_id: String,
    pub status: String,
    pub agent: String,
    /// Safe Claude/Codex profile label reported by the owning machine.
    #[serde(default)]
    pub profile: String,
    pub attached: bool,
    pub activity: u64,
    pub path: String,
    pub title: String,
    pub command: String,
    /// Safe, abbreviated tmux start command for the session detail header.
    #[serde(default)]
    pub launch_command: String,
    /// Owner-reported transient systemd scope for this process generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd_scope: Option<String>,
    /// Owner-reported scope `MemoryMax` in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    pub windows: u32,
    pub window_index: u32,
    pub pane_index: u32,
    pub content_hash: String,
}

impl SessionSummary {
    fn from_local(machine: &str, id: String, session: &Session) -> Self {
        Self {
            id,
            instance_id: session.pane_identity.clone(),
            session_key: session.session_key.clone(),
            machine: machine.to_owned(),
            name: session.name.clone(),
            description: session.description.clone(),
            description_source: session.description_source.clone(),
            pane_id: session.pane_id.clone(),
            status: session.status.label().to_owned(),
            agent: session.agent.to_string().to_lowercase(),
            profile: session.profile.clone(),
            attached: session.attached,
            activity: session.activity,
            path: session.path.to_string_lossy().into_owned(),
            title: session.title.clone(),
            command: session.command.clone(),
            launch_command: session.launch_command.clone(),
            systemd_scope: session.systemd_scope.clone(),
            memory_max_bytes: session.memory_max_bytes,
            windows: session.windows,
            window_index: session.window_index,
            pane_index: session.pane_index,
            content_hash: format!("{:016x}", observable_content_hash(&session.content)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Overview {
    /// Observable revision. A whole-federation overview carries the shared
    /// revision; a machine-scoped overview carries that machine's own cursor,
    /// which advances only when that machine changes.
    pub revision: u64,
    pub sessions: Vec<SessionSummary>,
    /// Health of this coordinator's own tmux server.
    pub health: Option<String>,
    /// Every federated machine, including this one.
    #[serde(default)]
    pub machines: Vec<MachineSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct PaneOutput {
    pub revision: u64,
    pub pane_id: String,
    pub session: String,
    pub content_hash: String,
    pub content: Option<String>,
    pub changed: bool,
}

/// One owner-reported model choice for a running agent pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PaneModelOption {
    pub id: String,
    pub label: String,
    /// False for configured launch-only model ids the running TUI cannot select.
    pub switchable: bool,
}

/// Model capabilities observed and validated by the pane's owning machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PaneModels {
    pub pane_id: String,
    pub harness: String,
    pub current: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// The configured profile-mode id matching the current pane, when atmux
    /// can prove one. The display model remains in `current` for clarity.
    #[serde(default)]
    pub current_mode: Option<String>,
    /// Whether the harness's session-scoped fast mode is on, when observable.
    #[serde(default)]
    pub fast: Option<bool>,
    pub version: Option<String>,
    pub models: Vec<PaneModelOption>,
    /// The distinct models this profile allows, selectable on their own.
    #[serde(default)]
    pub model_options: Vec<PaneModelOption>,
    /// The distinct reasoning levels this profile allows, selectable on their
    /// own. Empty when the installed harness has no verified effort control.
    #[serde(default)]
    pub effort_options: Vec<PaneModelOption>,
    /// Whether this pane's harness version exposes a fast-mode toggle atmux can
    /// drive without touching the model or reasoning level.
    #[serde(default)]
    pub fast_supported: bool,
    pub note: Option<String>,
    /// Whether the owning node can safely restart this exact Claude or Codex
    /// pane with its current launcher and native saved conversation. The
    /// configuration root and provider session id remain server-side.
    #[serde(default)]
    pub resume_available: bool,
    /// Human-safe explanation when an in-place agent restart is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_note: Option<String>,
    /// Owner-issued binding for the exact process shown by this snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_token: Option<String>,
}

#[derive(Clone, Debug)]
struct RestartBinding {
    instance_id: String,
    restart_token: String,
}

/// Data-only model switch request. Every id must match an owner-reported,
/// switchable choice and is never interpreted as a command.
///
/// `mode_id` applies one configured profile mode as a unit and stays the
/// compatible shape older clients send. The remaining fields apply the model,
/// the reasoning level, and fast mode independently, leaving the controls the
/// request omits exactly as the running harness has them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ModelSwitchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
}

/// A dashboard edit of one session's tmux name and/or short description.
/// Omitted fields keep their current value; an empty description clears it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Pane generation the caller saw. A pane that has since been replaced is
    /// refused instead of renaming whatever now holds its tmux id.
    pub instance_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutomaticDescriptionRequest {
    pub instance_id: String,
    pub session_key: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct OverviewPatch {
    pub(crate) base_revision: u64,
    pub(crate) revision: u64,
    pub(crate) upsert: Vec<SessionSummary>,
    pub(crate) remove: Vec<String>,
    pub(crate) health: Option<String>,
    #[serde(default)]
    pub(crate) machines: Vec<MachineSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PanePatch {
    pub(crate) base_revision: u64,
    pub(crate) revision: u64,
    pub(crate) pane_id: String,
    pub(crate) content_hash: String,
    pub(crate) start_line: usize,
    pub(crate) delete_lines: usize,
    pub(crate) lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileOption {
    pub id: String,
    pub name: String,
    pub harness: String,
    #[serde(default)]
    pub modes: Vec<ProfileModeOption>,
}

/// A safe, explicit launch choice exposed by one configured profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProfileModeOption {
    pub id: String,
    pub label: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
}

/// Owner-advertised, server-enforced memory choices for one machine.
///
/// The browser treats this as display data only. Every requested byte value is
/// resolved again by the owning node against its current configuration and
/// host/cgroup ceiling immediately before launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentMemoryLaunchOptions {
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_max_bytes: Option<u64>,
    #[serde(default)]
    pub presets_bytes: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Launch inputs accepted by one machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MachineLaunchOptions {
    pub id: String,
    pub label: String,
    pub online: bool,
    pub directories: Vec<String>,
    pub profiles: Vec<ProfileOption>,
    /// Saved project-local defaults keyed by their absolute launch directory.
    #[serde(default)]
    pub project_preferences: BTreeMap<String, ProjectPreferences>,
    /// Per-launch cgroup limit capability reported by this exact owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<AgentMemoryLaunchOptions>,
    /// Why a machine currently offers no launch inputs.
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LaunchOptions {
    /// This machine's directories. Retained so single-machine clients are unaffected.
    pub directories: Vec<String>,
    /// This machine's profiles. Retained so single-machine clients are unaffected.
    pub profiles: Vec<ProfileOption>,
    /// Saved project-local defaults keyed by their absolute launch directory.
    #[serde(default)]
    pub project_preferences: BTreeMap<String, ProjectPreferences>,
    /// This machine's memory capability. Retained beside `machines` for old
    /// single-owner clients and omitted by older nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<AgentMemoryLaunchOptions>,
    /// Per-machine launch inputs, this machine first.
    #[serde(default)]
    pub machines: Vec<MachineLaunchOptions>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LaunchRequest {
    pub name: String,
    pub directory: String,
    pub profile_id: String,
    /// Profile-scoped model/effort/tier selection. Profiles with exactly one
    /// configured mode select it automatically for compatibility with older
    /// clients; profiles with multiple modes require this value.
    #[serde(default)]
    pub mode_id: Option<String>,
    /// Machine to launch on. Defaults to this coordinator's own tmux server.
    #[serde(default)]
    pub machine: Option<String>,
    /// Opaque owner-issued handle for one native saved conversation. The
    /// provider session id and configuration path never cross the API.
    #[serde(default)]
    pub resume_session_id: Option<String>,
    /// Duplicate-with-summary: the pane whose conversation `profile_id`
    /// summarizes headlessly before this session launches. Present means
    /// "resume from summary"; the summary is delivered as the new agent's
    /// first turn. The pane always belongs to the launching machine.
    #[serde(default)]
    pub summarize_pane_id: Option<String>,
    /// Optional exact byte cap. Absence selects the owner's configured
    /// default; presence is never trusted without owner-side revalidation.
    #[serde(default)]
    pub memory_max_bytes: Option<u64>,
}

/// One saved native conversation safe to display in Quick Launch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumableLaunchSession {
    pub id: String,
    pub harness: String,
    pub updated_ms: u64,
    pub preview: String,
}

/// Bounded saved conversations for one exact machine/profile/project tuple.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResumableLaunchSessions {
    pub machine: String,
    pub directory: String,
    pub profile_id: String,
    pub sessions: Vec<ResumableLaunchSession>,
    pub truncated: bool,
}

/// One directory offered by the bounded launch browser.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BrowseDirectory {
    pub path: String,
    pub name: String,
}

/// One machine's safe, immediate-child directory listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LaunchDirectoryListing {
    pub machine: String,
    pub current: Option<String>,
    pub parent: Option<String>,
    pub directories: Vec<BrowseDirectory>,
    pub truncated: bool,
}

/// Creates one child folder in the currently displayed launch directory.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateLaunchDirectoryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub directory: String,
    pub name: String,
}

/// Clones one repository into a new child of the displayed launch directory.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloneLaunchRepositoryRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub directory: String,
    pub repository: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

/// A successful folder mutation and the refreshed parent listing.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LaunchDirectoryActionResult {
    pub directory: BrowseDirectory,
    pub listing: LaunchDirectoryListing,
}

#[derive(Clone, Debug, Default)]
struct RemoteState {
    sessions: Vec<SessionSummary>,
    online: bool,
    health: Option<String>,
    last_seen_ms: Option<u64>,
    launch: Option<LaunchOptions>,
    launch_note: Option<String>,
    metrics: MachineMetrics,
}

#[derive(Debug)]
struct State {
    revision: u64,
    sessions: Vec<Session>,
    health: Option<String>,
    metrics: MachineMetrics,
    remotes: BTreeMap<String, RemoteState>,
    /// Per-machine observable cursor. A machine's entry only advances when that
    /// machine itself changes, so a machine-scoped observer is not woken by
    /// unrelated machines. Values come from the shared revision counter, so
    /// they stay unique and monotonic across the whole coordinator.
    machine_revisions: BTreeMap<String, u64>,
}

impl State {
    /// Publishes one observable change, advancing the shared revision and the
    /// changed machine's own cursor.
    fn bump(&mut self, machine: &str) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.machine_revisions
            .insert(machine.to_owned(), self.revision);
        self.revision
    }
}

#[derive(Clone, Debug)]
struct CachedOutput {
    content_hash: String,
    content: String,
}

/// State held by every in-flight control mutation for one local pane. The
/// generation makes a queued destructive action one-shot: a message, model
/// change, interrupt, or earlier resume advances it while holding this same
/// lock, so an older resume intent cannot act on a newly changed process.
#[derive(Default)]
struct PaneMutationState {
    generation: u64,
}

/// The pane-scoped gate used for every local tmux mutation.  `generation` is
/// mirrored atomically only so a resume request can record its intent before
/// it waits for the blocking mutex; every eligibility check still happens
/// after the mutex is held.
struct PaneMutationGate {
    state: Mutex<PaneMutationState>,
    generation: AtomicU64,
}

impl Default for PaneMutationGate {
    fn default() -> Self {
        Self {
            state: Mutex::new(PaneMutationState::default()),
            generation: AtomicU64::new(0),
        }
    }
}

#[derive(Debug)]
struct ResumeRejected(String);

impl fmt::Display for ResumeRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ResumeRejected {}

/// Where a resolved session actually lives.
#[derive(Clone, Debug)]
enum Target {
    Local {
        pane_id: String,
        instance_id: String,
        name: String,
        agent: AgentKind,
        resume_lease: Option<String>,
    },
    Remote {
        machine: Arc<RemoteMachine>,
        pane_id: String,
        instance_id: String,
        name: String,
    },
}

#[derive(Debug, Default)]
struct ResumeLeaseState {
    /// Reservations made before a pane exists. Refreshes must not discard them.
    in_flight: BTreeSet<String>,
    /// Leases reconstructed from owner-local tmux pane metadata.
    active: BTreeSet<String>,
    /// Cross-process advisory locks held until tmux metadata is durable.
    process_locks: HashMap<String, File>,
}

impl ResumeLeaseState {
    fn reserve(&mut self, lease: &str) -> bool {
        if self.in_flight.contains(lease) || self.active.contains(lease) {
            return false;
        }
        self.in_flight.insert(lease.to_owned())
    }

    fn activate(&mut self, lease: &str) {
        self.in_flight.remove(lease);
        self.process_locks.remove(lease);
        self.active.insert(lease.to_owned());
    }

    fn release(&mut self, lease: &str) {
        self.in_flight.remove(lease);
        self.process_locks.remove(lease);
        self.active.remove(lease);
    }

    fn observe(&mut self, sessions: &[Session]) {
        self.active = sessions
            .iter()
            .filter_map(|session| session.resume_lease.clone())
            .collect();
    }

    fn hold_process_lock(&mut self, lease: &str, lock: File) {
        self.process_locks.insert(lease.to_owned(), lock);
    }
}

/// Cancellation-safe ownership of an in-flight saved-conversation launch.
///
/// The guard is moved into the blocking tmux operation before it is awaited.
/// Dropping the async caller therefore cannot release the reservation while a
/// detached blocking launch is still running. A successful launch commits only
/// after tmux has published the persistent lease metadata.
struct ResumeLeaseGuard {
    inner: Arc<Inner>,
    lease: Option<String>,
}

impl ResumeLeaseGuard {
    fn new(inner: Arc<Inner>, lease: &str) -> Self {
        Self {
            inner,
            lease: Some(lease.to_owned()),
        }
    }

    fn activate(mut self) {
        let lease = self
            .lease
            .take()
            .expect("a resume lease guard is activated only once");
        self.inner
            .resume_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .activate(&lease);
    }
}

impl Drop for ResumeLeaseGuard {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        self.inner
            .resume_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(&lease);
    }
}

#[derive(Debug)]
enum ResumeLeaseAcquireError {
    Busy,
    Unsafe,
    Tmux(anyhow::Error),
}

#[derive(Debug)]
struct Inner {
    events: Option<Arc<crate::events::EventService>>,
    config: Config,
    #[cfg(test)]
    registry_test_socket: Mutex<Option<String>>,
    summarizer: Option<Arc<crate::summarizer::Summarizer>>,
    registry: Option<Arc<crate::registry::Registry>>,
    registry_peer_token: Option<crate::machine::Secret>,
    registry_watchers: Mutex<BTreeMap<String, AbortHandle>>,
    local_id: String,
    local_label: String,
    /// With no `[[machines]]` configured or discovery enabled there is nothing
    /// to disambiguate, so local panes keep the bare identities they had before
    /// federation. Opting into discovery switches to stable composite ids before
    /// the first remote arrives.
    bare_local_ids: bool,
    /// Configured and dynamically discovered remotes, keyed by stable machine
    /// id. Explicit configuration always wins over a discovery advertisement.
    machines: RwLock<BTreeMap<String, Arc<RemoteMachine>>>,
    configured_machine_ids: BTreeSet<String>,
    watchers: std::sync::Mutex<BTreeMap<String, AbortHandle>>,
    state: RwLock<State>,
    hardware: std::sync::Mutex<HardwareSampler>,
    /// Coordinator-side cache so N browsers watching one remote pane produce one
    /// fetch per change instead of N fetches per revision.
    outputs: RwLock<HashMap<String, CachedOutput>>,
    /// One lock per remote pane, so simultaneous cache misses collapse into a
    /// single request to the owning node instead of one request per caller.
    fetches: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Owner-node prompt mutation lock per local pane. Only weak references are
    /// retained so dead panes and idle locks do not accumulate indefinitely.
    prompt_locks: Mutex<HashMap<String, Weak<PaneMutationGate>>>,
    /// Per-process keyed hasher for opaque native-session handles. Handles
    /// become invalid after restart and reveal neither provider ids nor paths.
    resume_ids: RandomState,
    /// Owner-local single-flight reservations plus active leases rebuilt from
    /// the protected tmux server after every refresh/restart.
    resume_leases: Mutex<ResumeLeaseState>,
    revisions: watch::Sender<u64>,
    refresh_now: Notify,
    /// Owner-local, fixed-command restart recovery. Remote coordinators proxy
    /// to this runner; they never receive a script path or command line.
    recovery: RecoveryRunner,
    /// Owner-local signed self-update. A coordinator forwards a fixed verb to
    /// the owning node; it never hands a node a version, URL, or path.
    updater: Arc<SelfUpdater>,
    /// Unit-test controls use synthetic pane records. If a regression crosses
    /// the native-restart validation boundary, stop at this in-memory seam
    /// rather than consulting the developer's default tmux server.
    #[cfg(test)]
    deny_local_agent_restart: bool,
    #[cfg(test)]
    local_agent_restart_attempts: AtomicU64,
    /// Synthetic live pane generations for owner-local message race tests.
    #[cfg(test)]
    test_message_live_instances: Mutex<HashMap<String, Option<String>>>,
}

#[derive(Clone, Debug)]
pub struct ControlPlane {
    inner: Arc<Inner>,
}

#[derive(Clone)]
pub(crate) struct WeakControlPlane(Weak<Inner>);
impl WeakControlPlane {
    pub(crate) fn upgrade(&self) -> Option<ControlPlane> {
        self.0.upgrade().map(|inner| ControlPlane { inner })
    }
}

struct CliMaintenanceRuntime<'a> {
    control: &'a ControlPlane,
    lock: &'a auto_update::OwnerLock,
}

impl auto_update::MaintenanceRuntime for CliMaintenanceRuntime<'_> {
    fn inspect(
        &self,
        harness: UpdateHarness,
    ) -> auto_update::MaintenanceFuture<'_, Option<auto_update::ExecutableIdentity>> {
        Box::pin(auto_update::inspect(harness))
    }

    fn collect(
        &self,
        harness: UpdateHarness,
        before_launcher: PathBuf,
    ) -> auto_update::MaintenanceFuture<'_, Vec<auto_update::PlannedPane>> {
        let worker = self.control.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                worker.collect_cli_update_candidates_blocking(harness, &before_launcher)
            })
            .await
            .context("CLI update candidate preflight panicked")
        })
    }

    fn update(
        &self,
        harness: UpdateHarness,
        before: auto_update::ExecutableIdentity,
        timeout: Duration,
    ) -> auto_update::MaintenanceFuture<'_, auto_update::ExecutableIdentity> {
        Box::pin(async move { auto_update::update(harness, &before, timeout, self.lock).await })
    }

    fn persist(&self, state: &auto_update::MaintenanceState) -> Result<()> {
        self.lock.store(state)
    }

    fn process_pending(
        &self,
        state: auto_update::MaintenanceState,
        limit: usize,
    ) -> auto_update::MaintenanceFuture<'_, (auto_update::MaintenanceState, bool)> {
        let worker = self.control.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                worker.process_cli_update_candidates_blocking(state, limit)
            })
            .await
            .context("CLI update relaunch worker panicked")
        })
    }
}

impl ControlPlane {
    pub(crate) fn downgrade(&self) -> WeakControlPlane {
        WeakControlPlane(Arc::downgrade(&self.inner))
    }

    pub(crate) fn event_session(&self, pane: &str) -> Result<Session> {
        if !pane.starts_with('%')
            || pane.len() > 12
            || !pane[1..].bytes().all(|v| v.is_ascii_digit())
        {
            return Err(bad_request("invalid hook pane"));
        }
        let cached = self
            .read_state()
            .sessions
            .iter()
            .find(|v| v.pane_id == pane)
            .cloned();
        if let Some(mut session) = cached
            && let Some(live) = Tmux::live_pane_identity(pane)?
            && live.pane_pid == session.pane_pid
            && live.pane_identity == session.pane_identity
        {
            session.path = live.path;
            return Ok(session);
        }
        Tmux.sessions(&HashMap::new(), &self.inner.config.status)?
            .into_iter()
            .find(|v| v.pane_id == pane)
            .ok_or_else(|| not_found("hook pane no longer exists"))
    }

    /// Shared owner emission seam for summaries, registry and resume events.
    /// # Errors
    /// Rejects disabled telemetry, foreign owners or failed durable writes.
    pub fn emit_agent_event(&self, event: crate::events::AgentEvent) -> Result<()> {
        self.inner
            .events
            .as_ref()
            .ok_or_else(|| not_found("agent events are disabled"))?
            .emit(event)
    }

    /// Durably appends coordinator-authored metadata/lifecycle events about
    /// local or remote sessions to the fleet log, never the owner feed.
    /// # Errors
    /// Rejects disabled telemetry, owner-only nodes or invalid/failed writes.
    pub fn append_fleet_agent_event(&self, event: crate::events::AgentEvent) -> Result<()> {
        self.inner
            .events
            .as_ref()
            .ok_or_else(|| not_found("agent events are disabled"))?
            .append_fleet(event)
    }

    /// Success means a bounded byte publication is durably queued. The
    /// coordinator sink handles broker availability, retries and ordering.
    /// # Errors
    /// Rejects disabled storage, owner-only nodes, invalid records or full/I/O failure.
    pub fn enqueue_publication(&self, topic: &str, key: &[u8], value: &[u8]) -> Result<()> {
        self.inner
            .events
            .as_ref()
            .ok_or_else(|| not_found("agent events are disabled"))?
            .enqueue_publication(topic, key, value)
    }

    /// Reads the owner feed for federation, or the fleet feed for MCP/UI.
    /// # Errors
    /// Rejects disabled telemetry, invalid cursors or failed spool reads.
    pub async fn agent_events(
        &self,
        query: crate::events::EventQuery,
        owner: bool,
    ) -> Result<crate::events::EventPage> {
        query
            .validate()
            .map_err(|error| bad_request(error.to_string()))?;
        let service = self
            .inner
            .events
            .as_ref()
            .ok_or_else(|| not_found("agent events are disabled"))?;
        let log = if owner {
            &service.owner
        } else {
            &service.fleet
        };
        log.read(&query).await
    }
    /// Starts the shared tmux monitor used by the web and MCP interfaces.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux is unavailable or the initial scan fails.
    pub async fn start(mut config: Config) -> Result<Self> {
        config.intake.validate(&config)?;
        crate::events::configure_profiles(&mut config)?;
        config.validate_coordinator_only()?;
        if !config.node.coordinator_only {
            Tmux::check()?;
        }
        config.validate_federation()?;
        config.validate_auto_compact()?;
        config.maintenance.validate()?;
        let machines = remote_machine_handles(&config)?;
        let remotes = machines
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    RemoteState {
                        online: false,
                        health: Some("connecting".to_owned()),
                        ..RemoteState::default()
                    },
                )
            })
            .collect();
        let (revisions, _) = watch::channel(0);
        let bare_local_ids = machines.is_empty() && !config.discovery.enabled;
        let configured_machine_ids = machines.keys().cloned().collect();
        let summarizer = crate::summarizer::Summarizer::configured(&config)?;
        let recovery = RecoveryRunner::production(&config);
        let updater = SelfUpdater::production(&config.self_update)?;
        let coordinator =
            config.node.coordinator_only || !config.machines.is_empty() || config.discovery.enabled;
        let (events, registry, registry_peer_token) = open_node_stores(&config, coordinator)?;
        let control = Self {
            inner: Arc::new(Inner {
                #[cfg(test)]
                registry_test_socket: Mutex::new(None),
                events,
                registry,
                registry_peer_token,
                registry_watchers: Mutex::new(BTreeMap::new()),
                local_id: config.node.id.clone(),
                local_label: config.node_label(),
                bare_local_ids,
                machines: RwLock::new(machines),
                configured_machine_ids,
                watchers: std::sync::Mutex::new(BTreeMap::new()),
                config,
                summarizer,
                state: RwLock::new(State {
                    revision: 0,
                    sessions: Vec::new(),
                    health: None,
                    metrics: MachineMetrics::default(),
                    remotes,
                    machine_revisions: BTreeMap::new(),
                }),
                hardware: std::sync::Mutex::new(HardwareSampler::default()),
                outputs: RwLock::new(HashMap::new()),
                fetches: RwLock::new(HashMap::new()),
                prompt_locks: Mutex::new(HashMap::new()),
                resume_ids: RandomState::new(),
                resume_leases: Mutex::new(ResumeLeaseState::default()),
                revisions,
                refresh_now: Notify::new(),
                recovery,
                updater,
                #[cfg(test)]
                deny_local_agent_restart: false,
                #[cfg(test)]
                local_agent_restart_attempts: AtomicU64::new(0),
                #[cfg(test)]
                test_message_live_instances: Mutex::new(HashMap::new()),
            }),
        };
        crate::registry_integration::install(&control, control.inner.events.is_some())?;
        if !control.inner.config.node.coordinator_only {
            control.refresh().await?;
            control.spawn_monitor();
            control.spawn_auto_compact();
            control.spawn_maintenance();
        }
        control.start_background_services(coordinator)?;
        Ok(control)
    }

    /// Starts the node-wide background services that run on every node:
    /// self-update, agent events, federation watchers, and the summarizer.
    fn start_background_services(&self, coordinator: bool) -> Result<()> {
        crate::intake::spawn(self.clone(), self.inner.config.clone())?;
        self.inner.updater.spawn_background();
        if let Some(events) = &self.inner.events {
            events.start(self, coordinator, !self.inner.config.node.coordinator_only)?;
        }
        for machine in self.remote_machines() {
            self.start_watcher(machine);
        }
        if let Some(service) = &self.inner.summarizer {
            tokio::spawn(service.clone().run(self.clone()));
        }
        crate::supervisor::start(self.clone(), &self.inner.config, coordinator)?;
        self.spawn_restore_reconciler();
        Ok(())
    }

    /// Reads safe Quick Resume state from the owning machine.
    ///
    /// # Errors
    ///
    /// Returns an error when the machine is unknown, offline, or rejects the
    /// owner-scoped status request.
    pub async fn recovery_status(&self, machine: &str) -> Result<RecoveryStatus> {
        if machine == self.inner.local_id {
            self.ensure_local_owner_enabled()?;
            return Ok(self.inner.recovery.status().await);
        }
        let remote = self.remote_machine(machine)?;
        self.ensure_online(&remote.id)?;
        remote
            .get_json(&format!(
                "/api/v1/machines/{}/quick-resume",
                encode_segment(machine)
            ))
            .await
            .map_err(|error| upstream(&error))
    }

    /// Starts the fixed Quick Resume operation on its owning machine.
    ///
    /// # Errors
    ///
    /// Returns an error when recovery is unavailable or already running, or
    /// when the owning machine is unknown, offline, or rejects the request.
    pub async fn start_recovery(&self, machine: &str) -> Result<RecoveryStatus> {
        if machine == self.inner.local_id {
            self.ensure_local_owner_enabled()?;
            return self
                .inner
                .recovery
                .start()
                .await
                .map_err(|error| match error {
                    RecoveryStartError::Running | RecoveryStartError::Unavailable(_) => {
                        conflict(error.to_string())
                    }
                });
        }
        let remote = self.remote_machine(machine)?;
        self.ensure_online(&remote.id)?;
        remote
            .post_json_response(
                &format!("/api/v1/machines/{}/quick-resume", encode_segment(machine)),
                &serde_json::json!({}),
            )
            .await
            .map_err(|error| upstream(&error))
    }

    /// Every machine's Quick Resume state, read concurrently.
    ///
    /// Like [`Self::fleet_updates`], one slow or broken node bounds its own
    /// entry.  A coordinator-only local node reports an error entry rather
    /// than a runner, because it owns no tmux server to restore.
    pub async fn fleet_recovery(&self) -> Vec<FleetRecovery> {
        let local_id = self.inner.local_id.clone();
        let local_label = self.inner.local_label.clone();
        let mut summaries = self.machines();
        if !summaries.iter().any(|machine| machine.id == local_id) {
            summaries.insert(
                0,
                MachineSummary {
                    id: local_id.clone(),
                    label: local_label,
                    kind: MachineKind::Local,
                    online: true,
                    sessions: 0,
                    health: None,
                    last_seen_ms: None,
                    address: None,
                    metrics: MachineMetrics::default(),
                },
            );
        }
        let mut tasks = Vec::with_capacity(summaries.len());
        for machine in summaries {
            if machine.id == local_id {
                let local = self.clone();
                tasks.push(tokio::spawn(async move {
                    let (recovery, error) = match local.recovery_status(&machine.id).await {
                        Ok(status) => (Some(status), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                    FleetRecovery {
                        id: machine.id,
                        label: machine.label,
                        online: true,
                        recovery,
                        error,
                    }
                }));
                continue;
            }
            let remote = self.remote_machine(&machine.id).ok();
            tasks.push(tokio::spawn(async move {
                let Some(remote) = remote.filter(|_| machine.online) else {
                    return FleetRecovery {
                        error: Some(
                            machine
                                .health
                                .clone()
                                .unwrap_or_else(|| "machine is offline".to_owned()),
                        ),
                        id: machine.id,
                        label: machine.label,
                        online: machine.online,
                        recovery: None,
                    };
                };
                let path = format!(
                    "/api/v1/machines/{}/quick-resume",
                    encode_segment(&machine.id)
                );
                let read = tokio::time::timeout(
                    FLEET_UPDATE_TIMEOUT,
                    remote.get_json::<RecoveryStatus>(&path),
                )
                .await;
                let (recovery, error) = match read {
                    Ok(Ok(status)) => (Some(status), None),
                    Ok(Err(error)) => (None, Some(format!("{error:#}"))),
                    Err(_) => (
                        None,
                        Some("machine did not answer the Quick Resume read in time".to_owned()),
                    ),
                };
                FleetRecovery {
                    id: machine.id,
                    label: machine.label,
                    online: true,
                    recovery,
                    error,
                }
            }));
        }
        let mut entries = Vec::with_capacity(tasks.len());
        for task in tasks {
            if let Ok(entry) = task.await {
                entries.push(entry);
            }
        }
        entries
    }

    /// Identifier this coordinator uses for its own tmux server.
    #[must_use]
    pub fn local_id(&self) -> &str {
        &self.inner.local_id
    }

    /// This node's own signed self-update document.
    #[must_use]
    pub fn update_status(&self) -> UpdateStatus {
        self.inner.updater.status()
    }

    /// Runs one fixed update verb against this node's own executable.
    ///
    /// # Errors
    ///
    /// Returns a conflict when the verb does not apply to this node right now,
    /// and an internal error when the updater itself failed.
    pub async fn local_update_action(&self, action: self_update::Action) -> Result<UpdateStatus> {
        let outcome = match action {
            self_update::Action::Check => self.inner.updater.check(true).await,
            self_update::Action::Apply => self.inner.updater.apply().await,
            self_update::Action::Rollback => self.inner.updater.rollback(),
        };
        outcome.map_err(|error| {
            if error.conflict {
                conflict(error.to_string())
            } else {
                internal(&anyhow::Error::new(error))
            }
        })
    }

    /// Reads the update document from the machine that owns that executable.
    ///
    /// # Errors
    ///
    /// Returns an error when the machine is unknown, offline, or rejects the
    /// owner-scoped read.
    pub async fn machine_update_status(&self, machine: &str) -> Result<UpdateStatus> {
        if machine == self.inner.local_id {
            return Ok(self.update_status());
        }
        let remote = self.remote_machine(machine)?;
        self.ensure_online(&remote.id)?;
        remote
            .get_json(UPDATE_STATUS_PATH)
            .await
            .map_err(|error| upstream(&error))
    }

    /// Forwards one fixed update verb to the machine that owns the executable.
    ///
    /// The forwarded path is chosen from a closed set of literals, so no part
    /// of a request can steer a coordinator at a URL of its choosing.
    ///
    /// # Errors
    ///
    /// Returns an error when the machine is unknown, offline, or refuses the
    /// verb.
    pub async fn machine_update_action(
        &self,
        machine: &str,
        action: self_update::Action,
    ) -> Result<UpdateStatus> {
        if machine == self.inner.local_id {
            return self.local_update_action(action).await;
        }
        let remote = self.remote_machine(machine)?;
        self.ensure_online(&remote.id)?;
        let path = match action {
            self_update::Action::Check => "/api/v1/update/check",
            self_update::Action::Apply => "/api/v1/update/apply",
            self_update::Action::Rollback => "/api/v1/update/rollback",
        };
        remote
            .post_json_response(path, &serde_json::json!({}))
            .await
            .map_err(|error| remote_mutation_error(&error))
    }

    /// Every machine's update document, read concurrently.
    ///
    /// One slow or broken node bounds its own entry instead of the whole
    /// answer: a per-node timeout turns into that entry's `error`.
    pub async fn fleet_updates(&self) -> Vec<FleetUpdate> {
        let local_id = self.inner.local_id.clone();
        let local_status = self.update_status();
        let local_label = self.inner.local_label.clone();
        let mut summaries = self.machines();
        if !summaries.iter().any(|machine| machine.id == local_id) {
            summaries.insert(
                0,
                MachineSummary {
                    id: local_id.clone(),
                    label: local_label,
                    kind: MachineKind::Local,
                    online: true,
                    sessions: 0,
                    health: None,
                    last_seen_ms: None,
                    address: None,
                    metrics: MachineMetrics::default(),
                },
            );
        }
        let mut tasks = Vec::with_capacity(summaries.len());
        for machine in summaries {
            if machine.id == local_id {
                tasks.push(tokio::spawn(std::future::ready(FleetUpdate {
                    id: machine.id,
                    label: machine.label,
                    online: true,
                    update: Some(local_status.clone()),
                    error: None,
                })));
                continue;
            }
            let remote = self.remote_machine(&machine.id).ok();
            tasks.push(tokio::spawn(async move {
                let Some(remote) = remote.filter(|_| machine.online) else {
                    return FleetUpdate {
                        error: Some(
                            machine
                                .health
                                .clone()
                                .unwrap_or_else(|| "machine is offline".to_owned()),
                        ),
                        id: machine.id,
                        label: machine.label,
                        online: machine.online,
                        update: None,
                    };
                };
                let read = tokio::time::timeout(
                    FLEET_UPDATE_TIMEOUT,
                    remote.get_json::<UpdateStatus>(UPDATE_STATUS_PATH),
                )
                .await;
                let (update, error) = match read {
                    Ok(Ok(update)) => (Some(update), None),
                    Ok(Err(error)) => (None, Some(format!("{error:#}"))),
                    Err(_) => (
                        None,
                        Some("timed out reading this machine's update state".to_owned()),
                    ),
                };
                FleetUpdate {
                    id: machine.id,
                    label: machine.label,
                    online: true,
                    update,
                    error,
                }
            }));
        }
        let mut entries = Vec::with_capacity(tasks.len());
        for task in tasks {
            if let Ok(entry) = task.await {
                entries.push(entry);
            }
        }
        entries
    }

    pub(crate) fn remote_machines(&self) -> Vec<Arc<RemoteMachine>> {
        self.inner
            .machines
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn start_watcher(&self, machine: Arc<RemoteMachine>) {
        if let Some(events) = &self.inner.events {
            events.federate(machine.clone());
        }
        let id = machine.id.clone();
        if let Some(registry) = &self.inner.registry {
            let handle = crate::registry::spawn_pull(Arc::clone(registry), Arc::clone(&machine));
            if let Some(previous) = self
                .inner
                .registry_watchers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(id.clone(), handle)
            {
                previous.abort();
            }
        }
        let watcher = remote::spawn_watcher(self.clone(), machine);
        self.inner
            .watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, watcher);
    }

    /// Adds or refreshes a machine learned through LAN discovery. Explicit
    /// `[[machines]]` configuration takes precedence over multicast records.
    pub fn upsert_discovered_machine(&self, machine: RemoteMachine) {
        if machine.id == self.inner.local_id
            || self.inner.configured_machine_ids.contains(&machine.id)
        {
            return;
        }
        let id = machine.id.clone();
        let machine = Arc::new(machine);
        if self
            .inner
            .machines
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .is_some_and(|existing| {
                existing.address() == machine.address() && existing.label == machine.label
            })
        {
            return;
        }
        let replaced = self
            .inner
            .machines
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), Arc::clone(&machine));
        if replaced.is_some() {
            if let Some(watcher) = self
                .inner
                .watchers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
            {
                watcher.abort();
            }
            let mut state = self.write_state();
            if let Some(remote) = state.remotes.get_mut(&id) {
                remote.online = false;
                remote.health = Some("connecting".to_owned());
                remote.sessions.clear();
                remote.launch = None;
                remote.launch_note = None;
                let revision = state.bump(&id);
                self.inner.revisions.send_replace(revision);
            }
            drop(state);
            self.evict_outputs(&id, &[]);
        } else {
            let mut state = self.write_state();
            state.remotes.insert(
                id.clone(),
                RemoteState {
                    online: false,
                    health: Some("connecting".to_owned()),
                    ..RemoteState::default()
                },
            );
            let revision = state.bump(&id);
            self.inner.revisions.send_replace(revision);
        }
        self.start_watcher(machine);
    }

    /// Removes a vanished LAN-discovered machine. Configured machines are
    /// durable and are never affected by a DNS-SD removal event.
    pub fn remove_discovered_machine(&self, id: &str) {
        if id == self.inner.local_id || self.inner.configured_machine_ids.contains(id) {
            return;
        }
        let removed = self
            .inner
            .machines
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if removed.is_none() {
            return;
        }
        if let Some(watcher) = self
            .inner
            .registry_watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            watcher.abort();
        }
        if let Some(watcher) = self
            .inner
            .watchers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            watcher.abort();
        }
        let mut state = self.write_state();
        state.remotes.remove(id);
        let revision = state.bump(id);
        self.inner.revisions.send_replace(revision);
        drop(state);
        self.evict_outputs(id, &[]);
    }

    /// Whether an id names this coordinator or one of its configured machines.
    #[must_use]
    pub fn has_machine(&self, machine: &str) -> bool {
        (machine == self.inner.local_id && !self.inner.config.node.coordinator_only)
            || self
                .inner
                .machines
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(machine)
    }

    /// The observable cursor for one machine, or `None` when it is unknown.
    ///
    /// A machine that has never published a change reports `0`, matching the
    /// shared revision's starting value.
    #[must_use]
    pub fn machine_revision(&self, machine: &str) -> Option<u64> {
        if !self.has_machine(machine) {
            return None;
        }
        Some(
            self.read_state()
                .machine_revisions
                .get(machine)
                .copied()
                .unwrap_or(0),
        )
    }

    /// The overview narrowed to one machine, carrying that machine's own cursor.
    ///
    /// Returns `None` for a machine this coordinator does not know.
    #[must_use]
    pub fn machine_overview(&self, machine: &str) -> Option<Overview> {
        let revision = self.machine_revision(machine)?;
        let mut overview = self.overview();
        overview.revision = revision;
        overview
            .sessions
            .retain(|session| session.machine == machine);
        overview
            .machines
            .retain(|candidate| candidate.id == machine);
        Some(overview)
    }

    /// Waits until one machine's own cursor advances past `after`.
    ///
    /// Changes on other machines wake this task and are then ignored, so a
    /// machine-scoped observer only returns when its own machine moved.
    pub async fn wait_for_machine_revision(
        &self,
        machine: &str,
        after: u64,
        timeout: Duration,
    ) -> bool {
        let mut receiver = self.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match self.machine_revision(machine) {
                Some(revision) if revision > after => return true,
                Some(_) => {}
                None => return false,
            }
            match tokio::time::timeout_at(deadline, receiver.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return false,
            }
        }
    }

    /// Replaces one machine's mirrored sessions, waking listeners only when the
    /// observable state actually changed.
    pub fn apply_machine_sessions(
        &self,
        machine: &str,
        sessions: Vec<SessionSummary>,
        health: Option<String>,
    ) {
        // A remote may run an older atmux build which still reports its own
        // dashboard service session. Keep that infrastructure pane hidden at
        // every coordinator, not only on the host that owns it.
        let sessions = sessions
            .into_iter()
            .filter(|session| session.name != RESERVED_SERVICE_SESSION)
            .collect::<Vec<_>>();
        let mut state = self.write_state();
        let Some(remote) = state.remotes.get_mut(machine) else {
            return;
        };
        let changed = !remote.online
            || remote.health != health
            || !observable_summaries_equal(&remote.sessions, &sessions);
        remote.online = true;
        remote.health = health;
        remote.last_seen_ms = Some(now_ms());
        remote.sessions = sessions;
        let live = remote
            .sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        if changed {
            let revision = state.bump(machine);
            self.inner.revisions.send_replace(revision);
        }
        drop(state);
        self.evict_outputs(machine, &live);
    }

    /// Records that one machine is unreachable. Local and other machines are
    /// untouched, so a single offline node degrades only its own group.
    pub fn mark_machine_offline(&self, machine: &str, health: &str) {
        let mut state = self.write_state();
        let Some(remote) = state.remotes.get_mut(machine) else {
            return;
        };
        let changed = remote.online
            || remote.health.as_deref() != Some(health)
            || !remote.sessions.is_empty();
        remote.online = false;
        remote.health = Some(health.to_owned());
        remote.sessions.clear();
        remote.launch = None;
        if changed {
            let revision = state.bump(machine);
            self.inner.revisions.send_replace(revision);
        }
        drop(state);
        self.evict_outputs(machine, &[]);
    }

    /// Stores launch inputs fetched once per successful connection.
    pub fn set_machine_launch_options(&self, machine: &str, options: LaunchOptions) {
        let mut state = self.write_state();
        if let Some(remote) = state.remotes.get_mut(machine) {
            remote.launch = Some(options);
            remote.launch_note = None;
        }
    }

    /// Records why one machine could not report launch inputs.
    pub fn set_machine_launch_note(&self, machine: &str, note: &str) {
        let mut state = self.write_state();
        if let Some(remote) = state.remotes.get_mut(machine) {
            remote.launch = None;
            remote.launch_note = Some(note.to_owned());
        }
    }

    /// Updates telemetry mirrored from one remote node. It is independently
    /// observable, so a resource change refreshes the machine detail view even
    /// when no tmux session changed.
    pub fn set_machine_metrics(&self, machine: &str, metrics: MachineMetrics) {
        let mut state = self.write_state();
        let Some(remote) = state.remotes.get_mut(machine) else {
            return;
        };
        if remote.metrics != metrics {
            remote.metrics = metrics;
            let revision = state.bump(machine);
            self.inner.revisions.send_replace(revision);
        }
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drops cached output and fetch locks for panes one machine no longer
    /// reports, so a long-lived coordinator never accumulates state for panes
    /// or machines that are gone.
    fn evict_outputs(&self, machine: &str, live: &[String]) {
        let prefix = format!("{machine}{}", crate::machine::COMPOSITE_SEPARATOR);
        let keep = |id: &String| !id.starts_with(&prefix) || live.iter().any(|live| live == id);
        self.inner
            .outputs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id, _| keep(id));
        self.inner
            .fetches
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|id, _| keep(id));
    }

    fn spawn_monitor(&self) {
        let control = self.clone();
        let refresh_ms = self.inner.config.general.refresh_ms;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(refresh_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The initial scan already happened in `start`.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = control.inner.refresh_now.notified() => {}
                }
                if let Err(error) = control.refresh().await {
                    control.record_health_error(error.to_string());
                }
            }
        });
    }

    fn spawn_auto_compact(&self) {
        if !self.inner.config.auto_compact.enabled {
            return;
        }
        let control = self.clone();
        let poll = Duration::from_secs(self.inner.config.auto_compact.poll_seconds);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(poll);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Startup refreshes identity first. Do not mutate on the immediate
            // first interval tick; wait one complete configured cadence.
            interval.tick().await;
            loop {
                interval.tick().await;
                let worker = control.clone();
                let result = tokio::task::spawn_blocking(move || {
                    worker.auto_compact_once_blocking(now_epoch_seconds())
                })
                .await;
                match result {
                    Ok(true) => control.inner.refresh_now.notify_one(),
                    Ok(false) => {}
                    Err(error) => eprintln!("atmux auto-compact worker failed: {error}"),
                }
            }
        });
    }

    /// Evaluates only owner-local panes. Remote summaries live under
    /// `state.remotes` and can never enter this mutation path, so a federation
    /// coordinator cannot compact an owner's pane a second time.
    fn auto_compact_once_blocking(&self, now: u64) -> bool {
        let candidates = self
            .read_state()
            .sessions
            .iter()
            .filter(|session| {
                session.status == AgentStatus::Waiting
                    && matches!(session.agent, AgentKind::Claude | AgentKind::Codex)
            })
            .map(|session| session.pane_id.clone())
            .collect::<Vec<_>>();
        let mut mutated = false;
        for pane_id in candidates {
            // The in-memory gate serializes every mutation in this process;
            // this advisory owner-local lock also covers a briefly overlapping
            // old/new atmux process during service restart.
            let Ok(_process_lock) = auto_update::PaneProcessLock::acquire(&pane_id) else {
                continue;
            };
            let prompt_lock = self.prompt_lock(&pane_id);
            let mut guard = prompt_lock
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous_hashes = self
                .read_state()
                .sessions
                .iter()
                .map(|session| (session.pane_id.clone(), session.content_hash))
                .collect::<HashMap<_, _>>();
            let Ok(fresh) = Tmux.sessions_with_capture(
                &previous_hashes,
                &self.inner.config.status,
                self.inner.config.general.preview_lines,
            ) else {
                continue;
            };
            let Some(session) = fresh.into_iter().find(|session| session.pane_id == pane_id) else {
                continue;
            };
            if !crate::status::automation_idle(
                session.agent,
                &session.content,
                &session.title,
                &self.inner.config.status,
            ) {
                continue;
            }
            let Some(context) = crate::transcript::native_context(&session) else {
                continue;
            };
            let Ok(existing) = Tmux::auto_compact_marker(&pane_id) else {
                continue;
            };
            match auto_compact::decide(
                &self.inner.config.auto_compact,
                now,
                &session,
                &context,
                existing.as_deref(),
            ) {
                AutoCompactDecision::Skip => {}
                AutoCompactDecision::ClearMarker => {
                    // A failed clear remains fail-closed because the marker is
                    // still present; a later poll can retry it safely.
                    if Tmux::clear_auto_compact_marker(&pane_id).is_ok() {
                        mutated = true;
                    }
                }
                AutoCompactDecision::Compact { marker } => {
                    // Claim durably before sending. A process crash between
                    // these operations can miss one compact but can never send
                    // it twice after restart.
                    if Tmux::set_auto_compact_marker(&pane_id, &marker).is_err() {
                        continue;
                    }
                    if begin_pane_mutation(&pane_id, &prompt_lock, &mut guard).is_err() {
                        // The auto-compact claim is already durable. Missing
                        // one compact is safer than retrying after ambiguity.
                        mutated = true;
                        continue;
                    }
                    mutated = true;
                    match Tmux::deliver_auto_compact(&pane_id) {
                        Ok(()) => {}
                        Err(error) => {
                            // The one tmux request may have reached the server.
                            // Keep the durable claim so no restart or later
                            // poll can append another `/compact`.
                            eprintln!(
                                "atmux auto-compact delivery for {pane_id} was ambiguous: {error:#}"
                            );
                        }
                    }
                }
            }
        }
        mutated
    }

    fn spawn_maintenance(&self) {
        if !self.inner.config.maintenance.enabled {
            return;
        }
        let control = self.clone();
        tokio::spawn(async move {
            // Pending update generations are reconsidered promptly when a
            // working pane becomes exactly Waiting. Actual network checks
            // remain on their independently persisted 30-minute cadence.
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                if let Err(error) = control.maintenance_once().await {
                    eprintln!("atmux CLI maintenance failed: {error:#}");
                }
            }
        });
    }

    async fn maintenance_once(&self) -> Result<()> {
        let Some(lock) = tokio::task::spawn_blocking(auto_update::OwnerLock::try_acquire)
            .await
            .context("CLI maintenance lock task panicked")??
        else {
            // Another owner-local atmux service is the one scheduler. A
            // federation coordinator never contacts remote update endpoints.
            return Ok(());
        };
        let settings = auto_update::CycleSettings {
            now_ms: auto_update::now_ms(),
            interval_ms: self
                .inner
                .config
                .maintenance
                .interval_minutes
                .saturating_mul(60_000),
            update_timeout: Duration::from_secs(
                self.inner.config.maintenance.update_timeout_seconds,
            ),
            relaunch_limit: self.inner.config.maintenance.relaunch_limit,
        };
        let runtime = CliMaintenanceRuntime {
            control: self,
            lock: &lock,
        };
        let (_, resumed) =
            auto_update::run_maintenance_cycle(&runtime, lock.load()?, settings).await?;
        if resumed {
            self.inner.refresh_now.notify_one();
        }
        Ok(())
    }

    /// Captures exact old-process plans before a vendor updater can mutate its
    /// launcher. Other/Grok, wrappers, unmapped sessions, and incomplete mode
    /// metadata never enter durable state.
    fn collect_cli_update_candidates_blocking(
        &self,
        harness: UpdateHarness,
        before_launcher: &Path,
    ) -> Vec<auto_update::PlannedPane> {
        let pane_ids = self
            .read_state()
            .sessions
            .iter()
            .filter(|session| maintenance_harness(session.agent) == Some(harness))
            .map(|session| session.pane_id.clone())
            .collect::<Vec<_>>();
        let mut planned = Vec::new();
        for pane_id in pane_ids {
            let Ok(_process_lock) = auto_update::PaneProcessLock::acquire(&pane_id) else {
                continue;
            };
            let gate = self.prompt_lock(&pane_id);
            let _guard = gate
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some((session, profile, mode, target)) =
                self.fresh_cli_update_preflight(&pane_id, harness)
            else {
                continue;
            };
            if !profile_bound_to_native(&profile, harness, before_launcher) {
                continue;
            }
            let Ok(mutation_sequence) = Tmux::pane_mutation_sequence(&pane_id) else {
                continue;
            };
            planned.push(auto_update::PlannedPane {
                pane_id,
                session_fingerprint: target.session_fingerprint,
                mutation_sequence,
                profile: session.profile,
                mode_id: mode.id,
            });
        }
        planned
    }

    /// Repairs partial marking and sequentially claims/resumes exact idle panes.
    /// A Claimed marker is an at-most-once tombstone: ambiguous respawn errors
    /// are never retried, including after service restart.
    #[allow(clippy::too_many_lines)] // Keeps each pane's lock/claim/respawn protocol contiguous.
    fn process_cli_update_candidates_blocking(
        &self,
        mut state: auto_update::MaintenanceState,
        limit: usize,
    ) -> (auto_update::MaintenanceState, bool) {
        let mut attempts = 0_usize;
        let mut any_respawn = false;
        for harness in UpdateHarness::ALL {
            let Some(harness_state) = state.harnesses.get(&harness).cloned() else {
                continue;
            };
            let Some(launcher) = harness_state
                .after
                .as_ref()
                .or(harness_state.applied.as_ref())
                .map(|identity| identity.canonical_path.clone())
            else {
                continue;
            };
            let mut remaining = Vec::new();
            for plan in harness_state.pending {
                let pane_id = plan.pane_id.clone();
                let Ok(_process_lock) = auto_update::PaneProcessLock::acquire(&pane_id) else {
                    remaining.push(plan);
                    continue;
                };
                let gate = self.prompt_lock(&pane_id);
                let mut guard = gate
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let fresh = self.fresh_cli_update_session(&pane_id);
                let marker = match Tmux::cli_update_marker(&pane_id) {
                    Ok(None) => None,
                    Ok(Some(raw)) => {
                        if let Some(marker) = PendingMarker::parse(&raw) {
                            Some(marker)
                        } else {
                            remaining.push(plan);
                            continue;
                        }
                    }
                    Err(_) => {
                        remaining.push(plan);
                        continue;
                    }
                };
                let mutation_sequence = Tmux::pane_mutation_sequence(&pane_id).ok();
                let target = fresh
                    .as_ref()
                    .and_then(crate::transcript::native_resume_target);
                let mode_observation = fresh.as_ref().map(|session| {
                    Tmux.model_observation(&pane_id, session.agent, &session.content)
                });
                let definitively_stale = fresh.as_ref().is_some_and(|session| {
                    session.agent != update_agent(harness)
                        || session.profile != plan.profile
                        || mode_observation
                            .as_ref()
                            .and_then(|observation| observation.mode.as_deref())
                            .is_some_and(|mode| mode != plan.mode_id)
                });
                let exact = fresh
                    .as_ref()
                    .and_then(|session| self.cli_update_preflight_for_session(session, harness));
                let observation = auto_update::PaneObservation {
                    exists: fresh.is_some(),
                    identity_definitively_stale: definitively_stale,
                    session_fingerprint: target
                        .as_ref()
                        .map(|target| target.session_fingerprint.clone()),
                    mutation_sequence,
                    exact_idle: exact.as_ref().is_some_and(|_| {
                        fresh.as_ref().is_some_and(|session| {
                            session.status == AgentStatus::Waiting
                                && crate::status::automation_idle(
                                    session.agent,
                                    &session.content,
                                    &session.title,
                                    &self.inner.config.status,
                                )
                        })
                    }),
                };
                match auto_update::pane_action(
                    &plan,
                    marker.as_ref(),
                    &observation,
                    harness,
                    harness_state.generation,
                ) {
                    auto_update::PaneAction::MaterializeReady => {
                        let ready = PendingMarker {
                            harness,
                            generation: harness_state.generation,
                            session_fingerprint: plan.session_fingerprint.clone(),
                            phase: auto_update::MarkerPhase::Ready,
                        };
                        let _ = Tmux::set_cli_update_marker(&pane_id, &ready.encode());
                        remaining.push(plan);
                    }
                    auto_update::PaneAction::Defer => remaining.push(plan),
                    auto_update::PaneAction::Forget => {
                        if marker
                            .is_some_and(|marker| marker.phase == auto_update::MarkerPhase::Ready)
                        {
                            let _ = Tmux::clear_cli_update_marker(&pane_id);
                        }
                    }
                    auto_update::PaneAction::AlreadyClaimed => {}
                    auto_update::PaneAction::ClaimBeforeRespawn => {
                        if attempts >= limit {
                            remaining.push(plan);
                            continue;
                        }
                        // A configured cgroup policy must be usable before
                        // the durable at-most-once claim. If the user manager
                        // is temporarily unavailable, retain the plan so a
                        // later maintenance pass can retry without ever
                        // launching an unbounded replacement.
                        let scope = match systemd_scope::prepare_override(
                            &self.inner.config.agent_resources,
                            fresh.as_ref().and_then(|session| session.memory_max_bytes),
                            &pane_id,
                        ) {
                            Ok(scope) => scope,
                            Err(error) => {
                                eprintln!(
                                    "atmux CLI update scope preflight for {pane_id} failed: {error:#}"
                                );
                                remaining.push(plan);
                                continue;
                            }
                        };
                        attempts += 1;
                        let claimed = PendingMarker {
                            harness,
                            generation: harness_state.generation,
                            session_fingerprint: plan.session_fingerprint.clone(),
                            phase: auto_update::MarkerPhase::Claimed,
                        };
                        if Tmux::set_cli_update_marker(&pane_id, &claimed.encode()).is_err() {
                            remaining.push(plan);
                            continue;
                        }
                        // Claim is durable before sequence advance and before
                        // the ambiguous destructive tmux respawn boundary.
                        if Tmux::advance_pane_mutation_sequence(&pane_id).is_err() {
                            continue;
                        }
                        mark_gate_mutated(&gate, &mut guard);
                        let Some((profile, mode, exact_target)) = exact else {
                            continue;
                        };
                        let Some(session) = fresh else {
                            continue;
                        };
                        match Tmux::resume_after_cli_update(
                            &pane_id,
                            &session.path,
                            &launcher,
                            harness,
                            &profile,
                            &mode,
                            &exact_target,
                            scope,
                        ) {
                            Ok(()) => {
                                let _ = Tmux::clear_cli_update_marker(&pane_id);
                                any_respawn = true;
                            }
                            Err(error) => {
                                eprintln!(
                                    "atmux CLI update respawn for {} was claimed and will not retry: {error:#}",
                                    session.name
                                );
                            }
                        }
                    }
                }
            }
            state.harness_mut(harness).pending = remaining;
        }
        (state, any_respawn)
    }

    fn fresh_cli_update_session(&self, pane_id: &str) -> Option<Session> {
        Tmux.sessions_with_capture(
            &HashMap::new(),
            &self.inner.config.status,
            self.inner.config.general.preview_lines,
        )
        .ok()?
        .into_iter()
        .find(|session| session.pane_id == pane_id)
    }

    fn fresh_cli_update_preflight(
        &self,
        pane_id: &str,
        harness: UpdateHarness,
    ) -> Option<(
        Session,
        AgentProfile,
        ProfileMode,
        crate::transcript::NativeResumeTarget,
    )> {
        let session = self.fresh_cli_update_session(pane_id)?;
        let (profile, mode, target) = self.cli_update_preflight_for_session(&session, harness)?;
        Some((session, profile, mode, target))
    }

    fn cli_update_preflight_for_session(
        &self,
        session: &Session,
        harness: UpdateHarness,
    ) -> Option<(
        AgentProfile,
        ProfileMode,
        crate::transcript::NativeResumeTarget,
    )> {
        native_restart_preflight(&self.inner.config.profiles, session, harness)
    }

    async fn refresh(&self) -> Result<()> {
        let control = self.clone();
        tokio::task::spawn_blocking(move || control.refresh_blocking())
            .await
            .context("tmux monitor task panicked")?
    }

    fn refresh_blocking(&self) -> Result<()> {
        let previous_hashes = {
            let state = self
                .inner
                .state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .sessions
                .iter()
                .map(|session| (session.pane_id.clone(), session.content_hash))
                .collect::<HashMap<_, _>>()
        };
        let capture_lines = self.inner.config.general.preview_lines;
        let mut sessions =
            Tmux.sessions_with_capture(&previous_hashes, &self.inner.config.status, capture_lines)?;
        for session in &mut sessions {
            truncate_front(&mut session.content, MAX_CAPTURE_BYTES);
            if self.handle_startup_prompt(session) {
                session.content = Tmux.capture(&session.pane_id, capture_lines)?;
                truncate_front(&mut session.content, MAX_CAPTURE_BYTES);
                let mut hasher = DefaultHasher::new();
                session.content.hash(&mut hasher);
                session.content_hash = hasher.finish();
                session.status = crate::status::classify(
                    session.agent,
                    &session.content,
                    &session.title,
                    "",
                    true,
                    &self.inner.config.status,
                );
            }
        }

        if let Some(registry) = &self.inner.registry {
            let changed =
                registry.observe_owner(&sessions, now_ms(), crate::resume_anywhere::server_id())?;
            for session in &sessions {
                self.bind_registry_session(session)?;
            }
            if changed {
                self.emit_registry_node_started(registry)?;
            }
            registry.checkpoint_live(&sessions, now_ms())?;
        }
        self.apply_refresh(sessions);
        let metrics = self
            .inner
            .hardware
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sample();
        self.apply_local_metrics(metrics);
        Ok(())
    }

    pub(crate) fn apply_refresh(&self, sessions: Vec<Session>) -> bool {
        if self.inner.config.node.coordinator_only {
            return false;
        }
        if let Some(events) = &self.inner.events {
            events.observe(&sessions);
        }
        self.inner
            .resume_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(&sessions);
        let mut state = self
            .inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // tmux activity and terminal titles can change for every spinner frame.
        // Keep their newest values for snapshots without waking every listener.
        let changed =
            !observable_sessions_equal(&state.sessions, &sessions) || state.health.is_some();
        state.sessions = sessions;
        state.health = None;
        if changed {
            let revision = state.bump(&self.inner.local_id);
            self.inner.revisions.send_replace(revision);
        }
        changed
    }

    fn record_health_error(&self, message: String) {
        if self.inner.config.node.coordinator_only {
            return;
        }
        let mut state = self
            .inner
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.health.as_deref() != Some(&message) {
            state.health = Some(message);
            let revision = state.bump(&self.inner.local_id);
            self.inner.revisions.send_replace(revision);
        }
    }

    fn apply_local_metrics(&self, metrics: MachineMetrics) {
        if self.inner.config.node.coordinator_only {
            return;
        }
        let mut state = self.write_state();
        if state.metrics != metrics {
            state.metrics = metrics;
            let revision = state.bump(&self.inner.local_id);
            self.inner.revisions.send_replace(revision);
        }
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.revisions.subscribe()
    }

    /// The identity this coordinator emits for one of its own panes.
    ///
    /// With no `[[machines]]` configured, the bare tmux pane id is emitted, so
    /// saved dashboard URLs and MCP clients that stored `%7` before federation
    /// keep working unchanged. Both the bare and the `machine~pane` forms are
    /// always accepted on input.
    fn local_identity(&self, pane_id: &str) -> String {
        if self.inner.bare_local_ids {
            pane_id.to_owned()
        } else {
            composite_id(&self.inner.local_id, pane_id)
        }
    }

    #[must_use]
    pub fn overview(&self) -> Overview {
        let state = self.read_state();
        let mut sessions = Vec::new();
        let mut machines = Vec::new();
        if !self.inner.config.node.coordinator_only {
            sessions.extend(state.sessions.iter().map(|session| {
                SessionSummary::from_local(
                    &self.inner.local_id,
                    self.local_identity(&session.pane_id),
                    session,
                )
            }));
            machines.push(MachineSummary {
                id: self.inner.local_id.clone(),
                label: self.inner.local_label.clone(),
                kind: MachineKind::Local,
                online: state.health.is_none(),
                sessions: sessions.len(),
                health: state.health.clone(),
                last_seen_ms: None,
                address: None,
                metrics: state.metrics.clone(),
            });
        }
        for machine in self.remote_machines() {
            let remote = state.remotes.get(&machine.id);
            let mirrored = remote
                .map(|remote| remote.sessions.clone())
                .unwrap_or_default();
            machines.push(MachineSummary {
                id: machine.id.clone(),
                label: machine.label.clone(),
                kind: MachineKind::Remote,
                online: remote.is_some_and(|remote| remote.online),
                sessions: mirrored.len(),
                health: remote.and_then(|remote| remote.health.clone()),
                last_seen_ms: remote.and_then(|remote| remote.last_seen_ms),
                address: Some(machine.address()),
                metrics: remote
                    .map_or_else(MachineMetrics::default, |remote| remote.metrics.clone()),
            });
            sessions.extend(mirrored);
        }
        Overview {
            revision: state.revision,
            sessions,
            health: (!self.inner.config.node.coordinator_only)
                .then(|| state.health.clone())
                .flatten(),
            machines,
        }
    }

    /// Every federated machine and its current health.
    #[must_use]
    pub fn machines(&self) -> Vec<MachineSummary> {
        self.overview().machines
    }

    /// Reads a bounded tail of one pane, routing to the owning machine.
    ///
    /// Returns `Ok(None)` when no session matches. Remote reads are served from
    /// the coordinator's hash-keyed cache whenever the owning machine still
    /// reports the cached content hash.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is ambiguous across machines or when the
    /// owning machine is offline or rejects the read.
    pub async fn pane_output(
        &self,
        id: &str,
        known_hash: Option<&str>,
        max_lines: usize,
    ) -> Result<Option<PaneOutput>> {
        let max_lines = max_lines.clamp(1, REMOTE_FETCH_LINES);
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let state = self.read_state();
                let Some(session) = find_session(&state.sessions, &pane_id) else {
                    return Ok(None);
                };
                let content_hash = format!("{:016x}", observable_content_hash(&session.content));
                let changed = known_hash != Some(content_hash.as_str());
                Ok(Some(PaneOutput {
                    revision: state.revision,
                    pane_id: self.local_identity(&session.pane_id),
                    session: session.name.clone(),
                    content_hash,
                    content: changed.then(|| tail_lines(&session.content, max_lines)),
                    changed,
                }))
            }
            Target::Remote {
                machine,
                pane_id,
                name,
                ..
            } => self
                .remote_pane_output(&machine, &pane_id, &name, known_hash, max_lines)
                .await
                .map(Some),
        }
    }

    /// Reads the bounded agent-native conversation for one pane, routing the
    /// request to the owning machine without exposing a log path.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is ambiguous, the owning machine is
    /// offline, or its agent log cannot be read.
    pub async fn transcript(
        &self,
        id: &str,
        known_hash: Option<&str>,
    ) -> Result<Option<Transcript>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let session = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).cloned()
                };
                let Some(session) = session else {
                    return Ok(None);
                };
                let known_hash = known_hash.map(ToOwned::to_owned);
                let transcript = tokio::task::spawn_blocking(move || {
                    crate::transcript::read(&session, known_hash.as_deref())
                })
                .await
                .context("agent transcript task panicked")??;
                Ok(Some(transcript))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let mut path = format!("/api/v1/panes/{}/transcript", encode_segment(&pane_id));
                if let Some(hash) = known_hash.filter(|hash| {
                    hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                }) {
                    path.push_str("?known_hash=");
                    path.push_str(hash);
                }
                machine
                    .get_json(&path)
                    .await
                    .map(Some)
                    .map_err(|error| upstream(&error))
            }
        }
    }

    /// Reads one bounded directory or regular UTF-8 file from the exact live
    /// pane's owner-derived project root.
    ///
    /// The browser never supplies a filesystem root. A federated coordinator
    /// forwards only the owner-local pane id and a validated relative path.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/hidden paths, unavailable panes or
    /// project roots, offline owners, or bounded filesystem failures.
    pub async fn pane_files(&self, id: &str, path: Option<&str>) -> Result<Option<FilesResponse>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let pane_cwd = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).map(|session| session.path.clone())
                };
                let Some(pane_cwd) = pane_cwd else {
                    return Ok(None);
                };
                let response = crate::workspace::files(
                    pane_cwd,
                    self.inner.config.launch_roots(),
                    path.map(ToOwned::to_owned),
                )
                .await
                .map_err(|error| workspace_error(&error))?;
                Ok(Some(response.with_pane_id(self.local_identity(&pane_id))))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let mut route = format!("/api/v1/panes/{}/files", encode_segment(&pane_id));
                if let Some(path) = path {
                    route.push_str("?path=");
                    route.push_str(&encode_segment(path));
                }
                let response: FilesResponse = machine
                    .get_json(&route)
                    .await
                    .map_err(|error| upstream(&error))?;
                Ok(Some(
                    response.with_pane_id(composite_id(&machine.id, &pane_id)),
                ))
            }
        }
    }

    /// Optimistically replaces one existing UTF-8 file below the exact live
    /// pane owner's project root. The owner, not the browser or coordinator,
    /// derives and validates the filesystem root.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/unsafe paths or content, stale hashes,
    /// unavailable panes/owners, or a failed atomic owner-local replacement.
    pub async fn write_pane_file(
        &self,
        id: &str,
        request: FileWriteRequest,
    ) -> Result<Option<FilesResponse>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let cached = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).cloned()
                };
                let Some(cached) = cached else {
                    return Ok(None);
                };
                // Test controls use a synthetic zero identity because they do
                // not own a real tmux server. A production pane pid and tmux
                // creation epoch are always non-zero and must match a fresh
                // owner-tmux observation immediately before filesystem work.
                let pane_cwd = if cached.pane_pid == 0 && cached.pane_identity.is_empty() {
                    cached.path.clone()
                } else {
                    let observed_pane = pane_id.clone();
                    let live = tokio::task::spawn_blocking(move || {
                        Tmux::live_pane_identity(&observed_pane)
                    })
                    .await
                    .map_err(|_| {
                        ControlError::new(ErrorKind::Internal, "live pane identity task failed")
                    })?
                    .map_err(|error| internal(&error))?;
                    let Some(live) = live else {
                        return Ok(None);
                    };
                    if live.pane_id != cached.pane_id
                        || live.pane_pid != cached.pane_pid
                        || live.pane_identity != cached.pane_identity
                    {
                        return Err(conflict(
                            "agent pane changed before the project file could be saved",
                        ));
                    }
                    live.path
                };
                let response = crate::workspace::write_file(
                    pane_cwd,
                    self.inner.config.launch_roots(),
                    request,
                )
                .await
                .map_err(|error| workspace_error(&error))?;
                Ok(Some(response.with_pane_id(self.local_identity(&pane_id))))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let route = format!("/api/v1/panes/{}/files", encode_segment(&pane_id));
                let response: FilesResponse = machine
                    .put_json_response(&route, &request)
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
                Ok(Some(
                    response.with_pane_id(composite_id(&machine.id, &pane_id)),
                ))
            }
        }
    }

    /// Answers one source-navigation question (definitions, references, or
    /// an import target) inside the pane's project, on the owning machine.
    /// The owner derives the project root exactly as it does for Files.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid symbol, specifier, or path, an
    /// unavailable project root, or an offline/refusing owner.
    pub async fn pane_code(
        &self,
        id: &str,
        request: CodeNavRequest,
    ) -> Result<Option<CodeNavResponse>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let pane_cwd = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).map(|session| session.path.clone())
                };
                let Some(pane_cwd) = pane_cwd else {
                    return Ok(None);
                };
                let response =
                    crate::code_nav::navigate(pane_cwd, self.inner.config.launch_roots(), request)
                        .await
                        .map_err(|error| workspace_error(&error))?;
                Ok(Some(response.with_pane_id(self.local_identity(&pane_id))))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let query = request
                    .query_pairs()
                    .into_iter()
                    .map(|(key, value)| format!("{key}={}", encode_segment(value)))
                    .collect::<Vec<_>>()
                    .join("&");
                let route = format!(
                    "/api/v1/panes/{}/code/{}?{query}",
                    encode_segment(&pane_id),
                    request.operation(),
                );
                let response: CodeNavResponse = machine
                    .get_json(&route)
                    .await
                    .map_err(|error| upstream(&error))?;
                Ok(Some(
                    response.with_pane_id(composite_id(&machine.id, &pane_id)),
                ))
            }
        }
    }

    /// Reads a bounded Git summary or one selected changed-file diff from the
    /// exact pane owner. Diff paths must have been freshly issued by the
    /// owner's filtered status response.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or unreported paths, unavailable panes,
    /// offline owners, or bounded Git failures.
    pub async fn pane_git(&self, id: &str, path: Option<&str>) -> Result<Option<GitResponse>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, .. } => {
                let pane_cwd = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).map(|session| session.path.clone())
                };
                let Some(pane_cwd) = pane_cwd else {
                    return Ok(None);
                };
                let response = crate::workspace::git(
                    pane_cwd,
                    self.inner.config.launch_roots(),
                    path.map(ToOwned::to_owned),
                )
                .await
                .map_err(|error| workspace_error(&error))?;
                Ok(Some(response.with_pane_id(self.local_identity(&pane_id))))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let mut route = format!("/api/v1/panes/{}/git", encode_segment(&pane_id));
                if let Some(path) = path {
                    route.push_str("?path=");
                    route.push_str(&encode_segment(path));
                }
                let response: GitResponse = machine
                    .get_json(&route)
                    .await
                    .map_err(|error| upstream(&error))?;
                Ok(Some(
                    response.with_pane_id(composite_id(&machine.id, &pane_id)),
                ))
            }
        }
    }

    /// Reports the model controls discovered by the pane's owning machine.
    /// Remote capabilities are forwarded verbatim; the coordinator never
    /// invents another host's models.
    ///
    /// # Errors
    ///
    /// Returns an error when the id is ambiguous or its owner is offline.
    pub async fn pane_models(&self, id: &str) -> Result<Option<PaneModels>> {
        let Some(target) = self.find_target(id)? else {
            return Ok(None);
        };
        match target {
            Target::Local { pane_id, agent, .. } => {
                let session = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id).cloned()
                };
                let Some(session) = session else {
                    return Ok(None);
                };
                let observed_pane = pane_id.clone();
                let profile = session.profile.clone();
                let content = session.content.clone();
                let profiles = self.inner.config.profiles.clone();
                let status = self.inner.config.status.clone();
                let observed = tokio::task::spawn_blocking(move || {
                    (
                        Tmux.model_observation(&observed_pane, agent, &content),
                        agent_restart_capability(&profiles, &status, &session),
                        agent_restart_token(&session),
                    )
                })
                .await
                .map_err(|error| {
                    internal(
                        &anyhow::Error::new(error).context("a model observation task panicked"),
                    )
                })?;
                let (observation, resume, restart_token) = observed;
                let mut models = model_capabilities(
                    self.local_identity(&pane_id),
                    agent,
                    &profile,
                    observation,
                    &self.inner.config.profiles,
                );
                models.resume_available = resume.available;
                models.resume_note = resume.note;
                models.restart_token = restart_token;
                Ok(Some(models))
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let mut models: PaneModels = machine
                    .get_json(&format!(
                        "/api/v1/panes/{}/models",
                        encode_segment(&pane_id)
                    ))
                    .await
                    .map_err(|error| upstream(&error))?;
                models.pane_id = composite_id(&machine.id, &pane_id);
                Ok(Some(models))
            }
        }
    }

    async fn remote_pane_output(
        &self,
        machine: &Arc<RemoteMachine>,
        pane_id: &str,
        name: &str,
        known_hash: Option<&str>,
        max_lines: usize,
    ) -> Result<PaneOutput> {
        let composite = composite_id(&machine.id, pane_id);
        self.ensure_online(&machine.id)?;
        let (content_hash, content) =
            if let Some(cached) = self.fresh_output(&composite, machine, pane_id) {
                (cached.content_hash, cached.content)
            } else {
                // Eight browsers opening the same pane must not become eight
                // requests to the node. The first caller through this lock fetches;
                // the rest wait and then find the result already cached.
                let lock = self.fetch_lock(&composite);
                let _permit = lock.lock().await;
                if let Some(cached) = self.fresh_output(&composite, machine, pane_id) {
                    (cached.content_hash, cached.content)
                } else {
                    // The machine may have gone offline while this caller queued.
                    self.ensure_online(&machine.id)?;
                    let fetched: PaneOutput = machine
                        .get_json(&format!(
                            "/api/v1/panes/{}?lines={REMOTE_FETCH_LINES}",
                            encode_segment(pane_id)
                        ))
                        .await
                        .map_err(|error| upstream(&error))?;
                    let content = fetched.content.unwrap_or_default();
                    self.store_output(&composite, &fetched.content_hash, &content);
                    (fetched.content_hash, content)
                }
            };
        let changed = known_hash != Some(content_hash.as_str());
        Ok(PaneOutput {
            revision: self.read_state().revision,
            pane_id: composite,
            session: name.to_owned(),
            content_hash,
            content: changed.then(|| tail_lines(&content, max_lines)),
            changed,
        })
    }

    /// Cheap, purely local check for whether a pane's advertised content hash
    /// still matches what a streaming client already has.
    ///
    /// Returns `true` when the answer is unknown so callers stay correct.
    #[must_use]
    pub fn pane_may_have_changed(&self, id: &str, known_hash: &str) -> bool {
        let Ok(Some(target)) = self.find_target(id) else {
            return true;
        };
        let state = self.read_state();
        let advertised = match &target {
            Target::Local { pane_id, .. } => find_session(&state.sessions, pane_id)
                .map(|session| format!("{:016x}", observable_content_hash(&session.content))),
            Target::Remote {
                machine, pane_id, ..
            } => state.remotes.get(&machine.id).and_then(|remote| {
                remote
                    .sessions
                    .iter()
                    .find(|session| &session.pane_id == pane_id)
                    .map(|session| session.content_hash.clone())
            }),
        };
        advertised.is_none_or(|advertised| advertised != known_hash)
    }

    fn mirrored_content_hash(&self, machine: &str, pane_id: &str) -> Option<String> {
        self.read_state()
            .remotes
            .get(machine)?
            .sessions
            .iter()
            .find(|session| session.pane_id == pane_id)
            .map(|session| session.content_hash.clone())
    }

    /// The cached output for one remote pane, but only while it still matches
    /// the hash the owning machine currently advertises.
    fn fresh_output(
        &self,
        composite: &str,
        machine: &Arc<RemoteMachine>,
        pane_id: &str,
    ) -> Option<CachedOutput> {
        let expected = self.mirrored_content_hash(&machine.id, pane_id)?;
        self.cached_output(composite, &expected)
    }

    /// The fetch lock for one remote pane, creating it on first use.
    fn fetch_lock(&self, composite: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.inner
                .fetches
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(composite.to_owned())
                .or_default(),
        )
    }

    fn prompt_lock(&self, pane_id: &str) -> Arc<PaneMutationGate> {
        let mut locks = self
            .inner
            .prompt_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(pane_id).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(PaneMutationGate::default());
        locks.insert(pane_id.to_owned(), Arc::downgrade(&lock));
        lock
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)] // Test builds consult the owner-live race seam below.
    fn validate_live_message_instance(&self, pane_id: &str, expected: Option<&str>) -> Result<()> {
        validate_live_pane_instance(pane_id, expected)
    }

    /// Owner-live pane generation, or `None` once the pane is gone.
    #[cfg(not(test))]
    #[allow(clippy::unused_self)] // Test builds consult the owner-live race seam below.
    fn live_pane_instance(&self, pane_id: &str) -> Result<Option<String>> {
        Ok(Tmux::live_pane_identity(pane_id)?.map(|live| live.pane_identity))
    }

    #[cfg(test)]
    fn live_pane_instance(&self, pane_id: &str) -> Result<Option<String>> {
        if let Some(live) = self
            .inner
            .test_message_live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .cloned()
        {
            return Ok(live);
        }
        Ok(Tmux::live_pane_identity(pane_id)?.map(|live| live.pane_identity))
    }

    #[cfg(test)]
    fn validate_live_message_instance(&self, pane_id: &str, expected: Option<&str>) -> Result<()> {
        if let Some(live) = self
            .inner
            .test_message_live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .cloned()
        {
            return live.map_or_else(
                || Err(conflict("agent pane disappeared before message delivery")),
                |instance| validate_expected_pane_instance(expected, &instance),
            );
        }
        validate_live_pane_instance(pane_id, expected)
    }

    #[cfg(test)]
    pub(crate) fn test_set_message_live_instance(
        &self,
        pane_id: &str,
        instance_id: Option<String>,
    ) {
        self.inner
            .test_message_live_instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pane_id.to_owned(), instance_id);
    }

    fn cached_output(&self, composite: &str, expected_hash: &str) -> Option<CachedOutput> {
        self.inner
            .outputs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(composite)
            .filter(|cached| cached.content_hash == expected_hash)
            .cloned()
    }

    fn store_output(&self, composite: &str, content_hash: &str, content: &str) {
        self.inner
            .outputs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                composite.to_owned(),
                CachedOutput {
                    content_hash: content_hash.to_owned(),
                    content: content.to_owned(),
                },
            );
    }

    fn ensure_online(&self, machine: &str) -> Result<()> {
        let state = self.read_state();
        let Some(remote) = state.remotes.get(machine) else {
            return Err(bad_request(format!("unknown machine {machine}")));
        };
        if remote.online {
            return Ok(());
        }
        let reason = remote
            .health
            .clone()
            .unwrap_or_else(|| "offline".to_owned());
        Err(offline(format!("machine {machine} is offline: {reason}")))
    }

    #[must_use]
    pub fn launch_options(&self) -> LaunchOptions {
        let directory_paths = if self.inner.config.node.coordinator_only {
            Vec::new()
        } else {
            self.inner.config.directories()
        };
        let directories = directory_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let project_preferences = project_preferences(&directory_paths);
        let memory = if self.inner.config.node.coordinator_only {
            None
        } else {
            Some(agent_memory_launch_options(
                &self.inner.config.agent_resources,
            ))
        };
        let profiles = if self.inner.config.node.coordinator_only {
            Vec::new()
        } else {
            self.inner
                .config
                .profiles
                .iter()
                .enumerate()
                .map(|(index, profile)| ProfileOption {
                    id: format!("profile-{index}"),
                    name: profile.name.clone(),
                    harness: profile.harness.clone(),
                    modes: profile.modes.iter().map(profile_mode_option).collect(),
                })
                .collect::<Vec<_>>()
        };
        let mut machines = Vec::new();
        if !self.inner.config.node.coordinator_only {
            machines.push(MachineLaunchOptions {
                id: self.inner.local_id.clone(),
                label: self.inner.local_label.clone(),
                online: true,
                directories: directories.clone(),
                profiles: profiles.clone(),
                project_preferences: project_preferences.clone(),
                memory: memory.clone(),
                note: None,
            });
        }
        let state = self.read_state();
        for machine in self.remote_machines() {
            let remote = state.remotes.get(&machine.id);
            let options = remote.and_then(|remote| remote.launch.clone());
            machines.push(MachineLaunchOptions {
                id: machine.id.clone(),
                label: machine.label.clone(),
                online: remote.is_some_and(|remote| remote.online),
                directories: options
                    .as_ref()
                    .map(|options| options.directories.clone())
                    .unwrap_or_default(),
                profiles: options
                    .as_ref()
                    .map(|options| options.profiles.clone())
                    .unwrap_or_default(),
                project_preferences: options
                    .as_ref()
                    .map(|options| options.project_preferences.clone())
                    .unwrap_or_default(),
                memory: options.as_ref().and_then(|options| options.memory.clone()),
                note: remote.and_then(|remote| {
                    remote
                        .launch_note
                        .clone()
                        .or_else(|| (!remote.online).then(|| remote_offline_note(remote)))
                }),
            });
        }
        LaunchOptions {
            directories,
            profiles,
            project_preferences,
            memory,
            machines,
        }
    }

    /// Browses one machine's configured launch roots without exposing an
    /// arbitrary filesystem read primitive.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown/offline machine, a path outside its
    /// configured roots, or a failed bounded directory read.
    pub async fn browse_launch_directories(
        &self,
        machine: Option<&str>,
        path: Option<&str>,
    ) -> Result<LaunchDirectoryListing> {
        let target = machine.unwrap_or(&self.inner.local_id);
        if target != self.inner.local_id {
            let remote = self.remote_machine(target)?;
            self.ensure_online(&remote.id)?;
            let endpoint = path.map_or_else(
                || "/api/v1/launch-directories".to_owned(),
                |path| format!("/api/v1/launch-directories?path={}", encode_segment(path)),
            );
            return remote
                .get_json(&endpoint)
                .await
                .map_err(|error| upstream(&error));
        }

        self.ensure_local_owner_enabled()?;

        let config = self.inner.config.clone();
        let machine = self.inner.local_id.clone();
        let path = path.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            browse_configured_directories(&config, machine, path.as_deref())
        })
        .await
        .map_err(|error| internal(&error.into()))?
    }

    /// Creates one new folder on the selected machine and refreshes its
    /// bounded parent listing.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown/offline machine, an outside-root
    /// parent, a non-component name, an existing target, or a filesystem
    /// failure on the owner.
    pub async fn create_launch_directory(
        &self,
        request: CreateLaunchDirectoryRequest,
    ) -> Result<LaunchDirectoryActionResult> {
        let target = request.machine.as_deref().unwrap_or(&self.inner.local_id);
        if target != self.inner.local_id {
            let remote = self.remote_machine(target)?;
            self.ensure_online(&remote.id)?;
            let forwarded = CreateLaunchDirectoryRequest {
                machine: None,
                ..request
            };
            return remote
                .post_json_response("/api/v1/launch-directories/folders", &forwarded)
                .await
                .map_err(|error| remote_mutation_error(&error));
        }

        self.ensure_local_owner_enabled()?;
        let config = self.inner.config.clone();
        let machine = self.inner.local_id.clone();
        tokio::task::spawn_blocking(move || {
            let created =
                launch_directory::create_folder(&config, &request.directory, &request.name)
                    .map_err(|error| launch_directory_error(&error))?;
            launch_directory_action_result(&config, machine, &request.directory, &created)
        })
        .await
        .map_err(|error| internal(&anyhow::Error::new(error).context("folder creation panicked")))?
    }

    /// Clones one repository into a new child folder on the selected machine.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown/offline machine, unsafe repository or
    /// destination input, an existing target, or a failed owner-side clone.
    pub async fn clone_launch_repository(
        &self,
        request: CloneLaunchRepositoryRequest,
    ) -> Result<LaunchDirectoryActionResult> {
        let target = request.machine.as_deref().unwrap_or(&self.inner.local_id);
        if target != self.inner.local_id {
            let remote = self.remote_machine(target)?;
            self.ensure_online(&remote.id)?;
            let forwarded = CloneLaunchRepositoryRequest {
                machine: None,
                ..request
            };
            return remote
                .post_json_response_with_timeout(
                    "/api/v1/launch-directories/clone",
                    &forwarded,
                    Duration::from_secs(10 * 60 + 30),
                )
                .await
                .map_err(|error| remote_mutation_error(&error));
        }

        self.ensure_local_owner_enabled()?;
        let config = self.inner.config.clone();
        let machine = self.inner.local_id.clone();
        tokio::task::spawn_blocking(move || {
            let created = launch_directory::clone_repository(
                &config,
                &request.directory,
                &request.repository,
                request.destination.as_deref(),
            )
            .map_err(|error| launch_directory_error(&error))?;
            launch_directory_action_result(&config, machine, &request.directory, &created)
        })
        .await
        .map_err(|error| internal(&anyhow::Error::new(error).context("git clone task panicked")))?
    }

    /// Lists bounded native conversations for one exact launch selection.
    ///
    /// The owning node derives provider storage from the selected configured
    /// profile. Native session ids and configuration paths remain owner-local.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown/offline machine, invalid profile or
    /// directory, unsupported harness, or unsafe/unreadable native store.
    pub async fn resumable_launch_sessions(
        &self,
        machine: Option<&str>,
        directory: &str,
        profile_id: &str,
    ) -> Result<ResumableLaunchSessions> {
        let target = machine.unwrap_or(&self.inner.local_id);
        if target != self.inner.local_id {
            let remote = self.remote_machine(target)?;
            self.ensure_online(&remote.id)?;
            let endpoint = format!(
                "/api/v1/launch-sessions?directory={}&profile_id={}",
                encode_segment(directory),
                encode_segment(profile_id),
            );
            return remote
                .get_json(&endpoint)
                .await
                .map_err(|error| upstream(&error));
        }
        self.ensure_local_owner_enabled()?;
        if directory.is_empty()
            || directory.len() > MAX_BROWSE_PATH_BYTES
            || directory.chars().any(char::is_control)
        {
            return Err(bad_request("launch directory is malformed"));
        }
        let directory = self
            .inner
            .config
            .resolve_launch_directory(Path::new(directory))
            .ok_or_else(|| bad_request("launch directory must be an exact configured folder"))?;
        let profile = profile_by_id(&self.inner.config, profile_id)?;
        if !matches!(
            profile.harness.to_ascii_lowercase().as_str(),
            "claude" | "codex"
        ) {
            return Err(bad_request(
                "saved-session launch is supported only for Claude and Codex profiles",
            ));
        }
        let opaque = self.inner.resume_ids.clone();
        let profile_id = profile_id.to_owned();
        let directory_text = directory.to_string_lossy().into_owned();
        let opaque_directory = directory_text.clone();
        let discovery = tokio::task::spawn_blocking(move || {
            old_sessions::discover(&profile, &directory, DiscoveryLimits::default())
        })
        .await
        .map_err(|error| {
            internal(&anyhow::Error::new(error).context("saved-session discovery panicked"))
        })?
        .map_err(|error| internal(&error))?;
        let sessions = discovery
            .sessions
            .iter()
            .map(|candidate| ResumableLaunchSession {
                id: opaque_resume_id(&opaque, &profile_id, &opaque_directory, candidate),
                harness: candidate.harness().as_str().to_owned(),
                updated_ms: candidate.updated_ms,
                preview: candidate.preview.clone(),
            })
            .collect();
        Ok(ResumableLaunchSessions {
            machine: self.inner.local_id.clone(),
            directory: directory_text,
            profile_id,
            sessions,
            truncated: discovery.truncated,
        })
    }

    /// Sends literal text to one agent pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the agent is unknown or tmux rejects the input.
    pub async fn send_text(&self, id: &str, text: String, submit: bool) -> Result<()> {
        self.send_text_for_instance(id, text, submit, None).await
    }

    /// Sends text only while the owner still reports the caller's pane generation.
    ///
    /// # Errors
    ///
    /// Returns a conflict when the pane was deleted or reincarnated before the
    /// owner-local tmux mutation boundary.
    pub async fn send_text_for_instance(
        &self,
        id: &str,
        text: String,
        submit: bool,
        expected_instance_id: Option<String>,
    ) -> Result<()> {
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                ..
            } => {
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                let prompt_lock = self.prompt_lock(&pane_id);
                let expected_instance_id = expected_instance_id.clone();
                let live_control = self.clone();
                local_message_mutation(
                    tokio::task::spawn_blocking(move || {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        live_control.validate_live_message_instance(
                            &pane_id,
                            expected_instance_id.as_deref(),
                        )?;
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Tmux::send_text_checked(&pane_id, &text, submit, || {
                            live_control.validate_live_message_instance(
                                &pane_id,
                                expected_instance_id.as_deref(),
                            )
                        })?;
                        Ok(())
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                self.ensure_online(&machine.id)?;
                let mut request = serde_json::json!({ "text": text, "submit": submit });
                if let Some(instance_id) = expected_instance_id {
                    request["instance_id"] = serde_json::Value::String(instance_id);
                }
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/messages", encode_segment(&pane_id)),
                        &request,
                    )
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
            }
        }
        Ok(())
    }

    /// Switches one running agent's model, reasoning effort, or fast mode to
    /// owner-reported choices through the harness's fixed native control path.
    ///
    /// Each control the request names is applied on its own, so changing one
    /// never resets the others.
    ///
    /// Returns the pane's capabilities as observed *after* the switch settled.
    /// The background pane poll is up to a second behind, so a caller that
    /// re-read the pane immediately would race it and redisplay the controls
    /// the pane had before the change.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed/unavailable choice, an unknown/offline
    /// pane, an unsupported CLI picker, or a failed tmux operation.
    pub async fn switch_model(&self, id: &str, request: ModelSwitchRequest) -> Result<PaneModels> {
        validate_model_switch(&request)?;
        match self.resolve(id)? {
            Target::Local { pane_id, agent, .. } => {
                let session = {
                    let state = self.read_state();
                    find_session(&state.sessions, &pane_id)
                        .cloned()
                        .ok_or_else(|| not_found(format!("no agent session matches {id}")))?
                };
                let content = session.content.clone();
                let profile_name = session.profile.clone();
                let prompt_lock = self.prompt_lock(&pane_id);
                let identity = self.local_identity(&pane_id);
                let inner = Arc::clone(&self.inner);
                let switch = request.clone();
                let settled = local_model_switch(
                    tokio::task::spawn_blocking(move || {
                        // Observing the pane runs several tmux subprocesses, so
                        // it stays on this blocking thread ahead of the gate.
                        let observation = Tmux.model_observation(&pane_id, agent, &content);
                        let capabilities = model_capabilities(
                            identity.clone(),
                            agent,
                            &profile_name,
                            observation,
                            &inner.config.profiles,
                        );
                        let version = authorize_model_switch(capabilities, &switch)?;
                        let mode = switch
                            .mode_id
                            .as_deref()
                            .map(|mode_id| {
                                profile_for_session(&inner.config.profiles, agent, &profile_name)
                                    .and_then(|profile| {
                                        profile.modes.iter().find(|mode| mode.id == mode_id)
                                    })
                                    .cloned()
                                    .ok_or_else(|| {
                                        bad_request("the pane profile no longer defines that mode")
                                    })
                            })
                            .transpose()?;
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        // Each control is driven separately so an omitted one
                        // keeps whatever the running harness already has.
                        if let Some(mode) = &mode {
                            Tmux.switch_model(&pane_id, agent, &version, mode)?;
                        }
                        if let Some(model) = switch.model.as_deref() {
                            Tmux.switch_model_choice(&pane_id, agent, &version, model)?;
                        }
                        if let Some(effort) = switch.effort.as_deref() {
                            Tmux.switch_effort(&pane_id, agent, &version, effort)?;
                        }
                        if let Some(fast) = switch.fast {
                            Tmux.switch_fast(&pane_id, agent, &version, fast)?;
                        }
                        // Each split switch clears the recorded mode, so the
                        // pane's resulting controls are matched back against
                        // the configured modes. Without this a pane that lands
                        // on a configured mode piecewise would stay unnamed and
                        // CLI-update maintenance would skip it forever.
                        if switch.model.is_some()
                            || switch.effort.is_some()
                            || switch.fast.is_some()
                        {
                            let modes =
                                profile_for_session(&inner.config.profiles, agent, &profile_name)
                                    .map(|profile| profile.modes.clone())
                                    .unwrap_or_default();
                            Tmux::reconcile_recorded_mode(&pane_id, &modes)?;
                        }
                        Ok(settled_model_capabilities(
                            &inner, &pane_id, identity, agent, &session, content,
                        ))
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
                Ok(settled)
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let mut models: PaneModels = machine
                    .post_json_response(
                        &format!("/api/v1/panes/{}/model", encode_segment(&pane_id)),
                        &request,
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
                // The owner names the pane by its local id; this aggregator's
                // callers only know the federated one.
                models.pane_id = composite_id(&machine.id, &pane_id);
                Ok(models)
            }
        }
    }

    /// Restarts an eligible Claude or Codex pane in place with the current
    /// native launcher and its own saved conversation.
    ///
    /// Every launch value is freshly derived on the owning machine. The
    /// browser cannot select a session id, configuration root, executable, or
    /// raw command line, and the existing tmux pane is respawned rather than
    /// replacing its session or server.
    ///
    /// # Errors
    ///
    /// Returns a conflict unless the pane passes its provider-specific
    /// readiness checks and can be unambiguously tied to its native saved
    /// conversation. Other agent panes are rejected, and owner/offline
    /// failures propagate for federated panes.
    pub async fn restart_current_agent(&self, id: &str) -> Result<()> {
        self.restart_agent(id, None).await
    }

    /// Restarts only the process generation confirmed by the caller.
    ///
    /// # Errors
    ///
    /// Rejects malformed or stale identities before mutation and propagates
    /// the same provider readiness errors as `restart_current_agent`.
    pub async fn restart_agent_instance(
        &self,
        id: &str,
        instance_id: &str,
        restart_token: &str,
    ) -> Result<()> {
        if !valid_pane_identity(instance_id) || !valid_restart_token(restart_token) {
            return Err(bad_request("invalid restart process binding"));
        }
        self.restart_agent(
            id,
            Some(RestartBinding {
                instance_id: instance_id.to_owned(),
                restart_token: restart_token.to_owned(),
            }),
        )
        .await
    }

    async fn restart_agent(&self, id: &str, expected: Option<RestartBinding>) -> Result<()> {
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                agent: AgentKind::Claude,
                ..
            } => {
                validate_expected_pane_instance(
                    expected
                        .as_ref()
                        .map(|binding| binding.instance_id.as_str()),
                    &instance_id,
                )?;
                // Keep the second resolution owner-scoped even if the local
                // pane disappears and a remote node has the same bare id.
                return self
                    .resume_claude_bound(&composite_id(&self.inner.local_id, &pane_id), expected)
                    .await;
            }
            Target::Local {
                pane_id,
                instance_id,
                agent: AgentKind::Codex,
                ..
            } => {
                validate_expected_pane_instance(
                    expected
                        .as_ref()
                        .map(|binding| binding.instance_id.as_str()),
                    &instance_id,
                )?;
                self.restart_local_codex(pane_id, expected).await?;
            }
            Target::Local { .. } => {
                return Err(bad_request(
                    "only Claude and Codex panes can restart a saved conversation",
                ));
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                self.ensure_online(&machine.id)?;
                validate_expected_pane_instance(
                    expected
                        .as_ref()
                        .map(|binding| binding.instance_id.as_str()),
                    &instance_id,
                )?;
                let (action, body) = expected.map_or_else(
                    || ("restart", serde_json::json!({})),
                    |binding| {
                        (
                            "restart-instance",
                            serde_json::json!({ "instance_id": binding.instance_id, "restart_token": binding.restart_token }),
                        )
                    },
                );
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/{action}", encode_segment(&pane_id)),
                        &body,
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
            }
        }
        Ok(())
    }

    async fn restart_local_codex(
        &self,
        pane_id: String,
        expected: Option<RestartBinding>,
    ) -> Result<()> {
        #[cfg(test)]
        if self.inner.deny_local_agent_restart {
            self.inner
                .local_agent_restart_attempts
                .fetch_add(1, Ordering::Relaxed);
            return Err(internal(&anyhow::anyhow!(
                "synthetic test control blocked a local Codex restart"
            )));
        }
        let prompt_lock = self.prompt_lock(&pane_id);
        let expected_generation = prompt_lock.generation.load(Ordering::Acquire);
        let resources = self.inner.config.agent_resources;
        let live_control = self.clone();
        local_agent_restart(
            tokio::task::spawn_blocking(move || {
                restart_local_codex_blocking(
                    &live_control,
                    &pane_id,
                    &prompt_lock,
                    expected_generation,
                    &resources,
                    expected.as_ref(),
                )
            })
            .await,
        )?;
        self.inner.refresh_now.notify_one();
        Ok(())
    }

    /// Restarts an eligible, non-working Claude pane in place with the current
    /// `claude` launcher and its own native saved conversation.
    ///
    /// The Claude session id and configuration directory are resolved only on
    /// the owning machine; callers cannot choose either one or replay a raw
    /// tmux start command.
    ///
    /// # Errors
    ///
    /// Returns a conflict if the pane is working or cannot be unambiguously
    /// tied to one live Claude session log, and propagates owner/offline
    /// failures for federated panes.
    pub async fn resume_current_claude(&self, id: &str) -> Result<()> {
        self.resume_claude_bound(id, None).await
    }

    async fn resume_claude_bound(&self, id: &str, expected: Option<RestartBinding>) -> Result<()> {
        match self.resolve(id)? {
            Target::Local { pane_id, agent, .. } => {
                // Reject from the already-resolved control-plane snapshot
                // before taking a mutation lock or consulting tmux. Besides
                // being the correct capability boundary, this keeps a stale or
                // synthetic non-Claude pane id from ever resolving to an
                // unrelated live Claude pane in the owner tmux server.
                if agent != AgentKind::Claude {
                    return Err(bad_request(
                        "only Claude panes can relaunch a saved Claude conversation",
                    ));
                }
                #[cfg(test)]
                if self.inner.deny_local_agent_restart {
                    self.inner
                        .local_agent_restart_attempts
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(internal(&anyhow::anyhow!(
                        "synthetic test control blocked a local Claude resume"
                    )));
                }
                let prompt_lock = self.prompt_lock(&pane_id);
                // This is only a request-order token.  It intentionally
                // reads no pane state: fresh pane/process/status/log
                // validation begins only after this mutation gate is held.
                // If a message or another resume reaches the pane first, its
                // completed mutation changes the generation and this stale
                // request rejects instead of acting on the replacement.
                let expected_generation = prompt_lock.generation.load(Ordering::Acquire);
                let status = self.inner.config.status.clone();
                let capture_lines = self.inner.config.general.preview_lines;
                let resources = self.inner.config.agent_resources;
                let inject_hooks = self
                    .inner
                    .config
                    .events
                    .as_ref()
                    .is_some_and(|events| events.inject_hooks);
                local_agent_restart(
                    tokio::task::spawn_blocking(move || {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        validate_live_pane_instance(&pane_id, expected.as_ref().map(|binding| binding.instance_id.as_str()))?;
                        if !resume_request_is_current(&guard, expected_generation) {
                            return Err(ResumeRejected(
                                "this pane changed after the relaunch was requested; review it and try again"
                                    .to_owned(),
                            )
                            .into());
                        }
                        let (session, resume, claude_program) =
                            fresh_claude_resume_target(&pane_id, &status, capture_lines)?;
                        validate_restart_binding(expected.as_ref(), &session)?;
                        let scope = systemd_scope::prepare_override(
                            &resources,
                            session.memory_max_bytes,
                            &pane_id,
                        )?;
                        if expected.is_some() {
                            let (latest, _, _) = fresh_claude_resume_target(&pane_id, &status, capture_lines)?;
                            validate_restart_binding(expected.as_ref(), &latest)?;
                        }
                        validate_live_pane_instance(&pane_id, expected.as_ref().map(|binding| binding.instance_id.as_str()))?;
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Tmux::resume_claude(
                            &pane_id,
                            &session.path,
                            &claude_program,
                            &resume.config_dir,
                            &resume.session_id,
                            scope,
                            inject_hooks,
                        )?;
                        Ok(())
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                if expected.is_some() {
                    return Err(conflict(
                        "the confirmed local Claude pane is no longer available",
                    ));
                }
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/resume", encode_segment(&pane_id)),
                        &serde_json::json!({}),
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
            }
        }
        Ok(())
    }

    /// Stores validated images on the pane's owning machine and submits one
    /// literal prompt containing their local paths.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed images, an unknown/offline pane, or a
    /// storage/tmux failure on the owning node.
    pub async fn send_image_message(&self, id: &str, request: ImageMessageRequest) -> Result<()> {
        let expected_instance_id = request.instance_id.clone();
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                agent,
                ..
            } => {
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                if let Err(error) = attachment::validate_request(&request) {
                    return if error.kind() == DeliveryErrorKind::Invalid {
                        Err(bad_request(error.to_string()))
                    } else {
                        Err(internal(&error.into()))
                    };
                }
                let prompt_lock = self.prompt_lock(&pane_id);
                let expected_instance_id = expected_instance_id.clone();
                let live_control = self.clone();
                let delivered = local_message_mutation(
                    tokio::task::spawn_blocking(move || -> Result<_> {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        live_control.validate_live_message_instance(
                            &pane_id,
                            expected_instance_id.as_deref(),
                        )?;
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Ok(attachment::deliver(
                            &pane_id,
                            request,
                            agent == AgentKind::Claude,
                        ))
                    })
                    .await,
                )?;
                match delivered {
                    Ok(()) => {}
                    Err(error) if error.kind() == DeliveryErrorKind::Invalid => {
                        return Err(bad_request(error.to_string()));
                    }
                    Err(error) if error.kind() == DeliveryErrorKind::Conflict => {
                        return Err(conflict(error.to_string()));
                    }
                    Err(error) => {
                        return Err(internal(
                            &anyhow::Error::new(error)
                                .context("failed to deliver an image attachment"),
                        ));
                    }
                }
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/image-messages", encode_segment(&pane_id)),
                        &request,
                    )
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
            }
        }
        Ok(())
    }

    /// Sends the legacy fixed `Ctrl+B`, `Ctrl+B` sequence to one agent pane.
    ///
    /// This compatibility path intentionally has no browser-supplied pane
    /// generation. New clients must call [`Self::send_special_key_for_instance`]
    /// through the generation-bound `/input-keys` route instead.
    ///
    /// # Errors
    ///
    /// Returns an error when the agent is unknown, offline, or tmux rejects
    /// either key.
    pub async fn tmux_prefix_twice(&self, id: &str) -> Result<()> {
        match self.resolve(id)? {
            Target::Local { pane_id, .. } => {
                let prompt_lock = self.prompt_lock(&pane_id);
                local_tmux(
                    tokio::task::spawn_blocking(move || {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Tmux.tmux_prefix_twice(&pane_id)?;
                        Ok(())
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/special-keys", encode_segment(&pane_id)),
                        &serde_json::json!({ "action": "tmux_prefix_twice" }),
                    )
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
            }
        }
        Ok(())
    }

    /// Sends one allowlisted interactive tmux key to an exact pane generation
    /// on its expected owning machine.
    ///
    /// # Errors
    ///
    /// Returns a conflict when the machine, pane generation, or live pane
    /// changes before delivery. Offline/upstream and tmux failures retain
    /// their normal control-plane classifications.
    pub async fn send_special_key_for_instance(
        &self,
        id: &str,
        key: PaneSpecialKey,
        expected_machine_id: Option<String>,
        expected_instance_id: Option<String>,
    ) -> Result<()> {
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                ..
            } => {
                // An omitted machine id means "wherever this pane already
                // lives", which for a local pane is always this node.
                let checked_machine_id = expected_machine_id
                    .as_deref()
                    .unwrap_or(&self.inner.local_id);
                validate_expected_pane_machine(checked_machine_id, &self.inner.local_id)?;
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                let prompt_lock = self.prompt_lock(&pane_id);
                let live_control = self.clone();
                local_message_mutation(
                    tokio::task::spawn_blocking(move || {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        live_control.validate_live_message_instance(
                            &pane_id,
                            expected_instance_id.as_deref(),
                        )?;
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Tmux.send_special_key(&pane_id, key)?;
                        Ok(())
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                // Validate against this node's own id when unset, but still
                // forward the caller's original (possibly absent) ids rather
                // than this node's id, which would not describe the owner.
                let checked_machine_id = expected_machine_id
                    .as_deref()
                    .unwrap_or(&self.inner.local_id);
                validate_expected_pane_machine(checked_machine_id, &machine.id)?;
                validate_expected_pane_instance(expected_instance_id.as_deref(), &instance_id)?;
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/input-keys", encode_segment(&pane_id)),
                        &serde_json::json!({
                            "action": key.wire_name(),
                            "machine": expected_machine_id,
                            "instance_id": expected_instance_id,
                        }),
                    )
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
            }
        }
        Ok(())
    }

    /// Interrupts one agent pane.
    ///
    /// # Errors
    ///
    /// Returns an error when the agent is unknown or tmux rejects the key.
    pub async fn interrupt(&self, id: &str) -> Result<()> {
        match self.resolve(id)? {
            Target::Local { pane_id, .. } => {
                let prompt_lock = self.prompt_lock(&pane_id);
                local_tmux(
                    tokio::task::spawn_blocking(move || {
                        let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                        let mut guard = prompt_lock
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                        Tmux.interrupt(&pane_id)?;
                        Ok(())
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!("/api/v1/panes/{}/interrupt", encode_segment(&pane_id)),
                        &serde_json::json!({}),
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
            }
        }
        Ok(())
    }

    /// Sizes a detached pane's window to the browser's raw view, on the
    /// pane's owning machine. See [`Tmux::fit_detached_window`].
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range size, an unknown pane, or a
    /// failed tmux or owner request.
    pub async fn fit_pane(&self, id: &str, request: PaneSizeRequest) -> Result<PaneSizeResponse> {
        if !(MIN_FIT_COLS..=MAX_FIT_COLS).contains(&request.cols)
            || !(MIN_FIT_ROWS..=MAX_FIT_ROWS).contains(&request.rows)
        {
            return Err(bad_request(format!(
                "pane size must be {MIN_FIT_COLS}-{MAX_FIT_COLS} columns and {MIN_FIT_ROWS}-{MAX_FIT_ROWS} rows"
            )));
        }
        match self.resolve(id)? {
            Target::Local { pane_id, .. } => {
                let (cols, rows) = (request.cols, request.rows);
                let fit = tokio::task::spawn_blocking(move || {
                    Tmux::fit_detached_window(&pane_id, cols, rows)
                })
                .await
                .map_err(|error| {
                    internal(&anyhow::Error::new(error).context("a tmux task panicked"))
                })?
                .map_err(|error| internal(&error))?;
                if fit == crate::tmux::PaneFit::Resized {
                    self.inner.refresh_now.notify_one();
                }
                Ok(PaneSizeResponse {
                    fit: fit.as_str().to_owned(),
                })
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                let response: PaneSizeResponse = machine
                    .post_json_response(
                        &format!("/api/v1/panes/{}/size", encode_segment(&pane_id)),
                        &request,
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
                if !matches!(
                    response.fit.as_str(),
                    "resized" | "unchanged" | "attached" | "split"
                ) {
                    return Err(upstream(&anyhow::anyhow!(
                        "the owner returned an unknown pane fit"
                    )));
                }
                Ok(response)
            }
        }
    }

    /// Renames and/or describes one agent session on its owning machine.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, a replaced pane, a name that is
    /// already taken, or a failed tmux or owner request.
    pub async fn update_session(&self, id: &str, request: SessionUpdateRequest) -> Result<()> {
        let (requested_name, description) = validate_session_update(&request)?;
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                name,
                ..
            } => {
                validate_session_update_instance(&request.instance_id, &instance_id)?;
                let rename = requested_name.filter(|requested| *requested != name);
                if let Some(requested) = rename.as_deref() {
                    self.ensure_launch_name_available(requested)?;
                }
                let expected_instance_id = request.instance_id;
                let live_control = self.clone();
                local_message_mutation(
                    tokio::task::spawn_blocking(move || {
                        let live = live_control
                            .live_pane_instance(&pane_id)?
                            .ok_or_else(|| conflict("the session no longer exists"))?;
                        validate_session_update_instance(&expected_instance_id, &live)?;
                        Tmux.update_session_metadata(&pane_id, rename.as_deref(), &description)
                            .map_err(|error| {
                                if format!("{error:#}").contains("duplicate session") {
                                    conflict(format!(
                                        "a tmux session named {} already exists",
                                        rename.as_deref().unwrap_or_default()
                                    ))
                                } else {
                                    error
                                }
                            })
                    })
                    .await,
                )?;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                name,
            } => {
                validate_session_update_instance(&request.instance_id, &instance_id)?;
                self.ensure_online(&machine.id)?;
                if let Some(requested) = requested_name.filter(|requested| *requested != name) {
                    self.ensure_remote_session_name_available(&machine.id, &requested)?;
                }
                machine
                    .patch_json(
                        &format!("/api/v1/sessions/{}", encode_segment(&pane_id)),
                        &request,
                    )
                    .await
                    .map_err(|error| {
                        if remote::rejected_status(&error) == Some(405) {
                            conflict(format!(
                                "update atmux on {} before renaming its sessions",
                                machine.id
                            ))
                        } else {
                            remote_mutation_error(&error)
                        }
                    })?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_summarizer(&self) -> Option<Arc<crate::summarizer::Summarizer>> {
        self.inner.summarizer.clone()
    }

    /// Reads a cached digest and schedules stale work without waiting for Qwen.
    ///
    /// # Errors
    /// Returns an error for an unknown or ambiguous session reference.
    pub fn agent_summary(&self, id: &str) -> Result<crate::summarizer::AgentSummary> {
        let session = self.conversation_session(id)?;
        let mut summary = self.inner.summarizer.as_ref().map_or_else(
            || crate::summarizer::AgentSummary {
                title: String::new(),
                description: session.description.clone().unwrap_or_default(),
                digest: String::new(),
                digest_updated_at: None,
                status: session.status.clone(),
                needs_input_reason: None,
                stale: false,
                enabled: false,
            },
            |service| service.cached(&session),
        );
        summary.needs_input_reason = self.inner.events.as_ref().map_or_else(
            || (session.status == "waiting").then(|| "idle_prompt".into()),
            |events| events.needs_input_reason(&session),
        );
        Ok(summary)
    }

    /// Ranks live and recently summarized sessions using bounded term matching.
    ///
    /// # Errors
    /// Returns an error for an oversized or empty search query.
    pub fn sessions_find(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::summarizer::FoundSession>> {
        let recent = self
            .inner
            .summarizer
            .as_ref()
            .map_or_else(Vec::new, |service| service.recent());
        crate::summarizer::find_sessions(&self.overview().sessions, recent, query, limit)
    }

    pub(crate) async fn apply_automatic_description(
        &self,
        session: &SessionSummary,
        description: &str,
    ) -> Result<()> {
        let Some(key) = &session.session_key else {
            return Ok(());
        };
        self.automatic_description(
            &session.id,
            AutomaticDescriptionRequest {
                instance_id: session.instance_id.clone(),
                session_key: key.clone(),
                description: description.into(),
            },
        )
        .await
    }

    /// Owner endpoint for coordinator-generated metadata. Source and generation
    /// are checked inside tmux's conditional command list; user edits win races.
    ///
    /// # Errors
    /// Returns an error for invalid text, stale identity, offline owners, or tmux failures.
    pub async fn automatic_description(
        &self,
        id: &str,
        request: AutomaticDescriptionRequest,
    ) -> Result<()> {
        if !crate::tmux::valid_session_description(&request.description)
            || !crate::tmux::valid_session_key(&request.session_key)
        {
            return Err(bad_request("invalid automatic description"));
        }
        let session = self.conversation_session(id)?;
        if session.session_key.as_deref() != Some(&request.session_key) {
            return Err(conflict("session key changed"));
        }
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                ..
            } => {
                validate_session_update_instance(&request.instance_id, &instance_id)?;
                tokio::task::spawn_blocking(move || {
                    Tmux::apply_automatic_description(
                        &pane_id,
                        &request.instance_id,
                        &request.session_key,
                        &request.description,
                    )
                })
                .await??;
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                validate_session_update_instance(&request.instance_id, &instance_id)?;
                self.ensure_online(&machine.id)?;
                machine
                    .post_json(
                        &format!(
                            "/api/v1/sessions/{}/automatic-description",
                            encode_segment(&pane_id)
                        ),
                        &request,
                    )
                    .await
                    .map_err(|error| remote_mutation_error(&error))?;
            }
        }
        Ok(())
    }

    /// Called after durable digest persistence. Coordinator-authored summary
    /// events enter the fleet feed for local and remote sessions alike.
    /// Disabled telemetry remains a no-op; append failures do not discard a digest.
    pub(crate) fn summary_updated_hook(&self, session: &SessionSummary, detail: serde_json::Value) {
        if self.inner.events.is_none() {
            return;
        }
        let result =
            crate::events::AgentEvent::from_summary(session, "agent.summary_updated", None)
                .and_then(|mut event| {
                    event.detail = detail;
                    self.append_fleet_agent_event(event)
                });
        if result.is_err() {
            eprintln!("atmux summary event append failed");
        }
    }

    /// Durable ordered enqueue; broker errors are handled asynchronously.
    /// Enqueue failure leaves A2's timestamp high-water mark eligible for retry.
    pub(crate) fn summary_search_document_hook(
        &self,
        publication: &crate::session_search::SearchPublication,
    ) -> Result<()> {
        self.enqueue_publication(
            publication.topic,
            publication.key.as_bytes(),
            &publication.value()?,
        )
    }

    /// Retrieves bounded cached digest context for A3 archive/resume indexing.
    #[must_use]
    pub fn session_digest_record(&self, key: &str) -> Option<crate::summarizer::DigestRecord> {
        self.inner
            .summarizer
            .as_ref()
            .and_then(|service| service.digest_record(key))
    }

    /// Ordered full-snapshot search publication seam for registry lifecycle changes.
    ///
    /// # Errors
    /// Returns an error for invalid identity/tenant/time or failed durable publication.
    pub fn publish_digest_search(
        &self,
        record: &crate::summarizer::DigestRecord,
        change: crate::session_search::SearchChange,
        timestamp_ms: u64,
    ) -> Result<bool> {
        self.inner.summarizer.as_ref().map_or(Ok(false), |service| {
            service.publish_search(self, record, change, timestamp_ms)
        })
    }

    /// Applies a supervisor action only to the unchanged owner pane.
    ///
    /// # Errors
    ///
    /// Returns an error when identity, output or status changed, or the owner rejects the action.
    #[allow(clippy::too_many_lines)] // Keep guard, locks and registry close intent in one owner transaction.
    pub async fn supervisor_mutation(
        &self,
        id: &str,
        guard: crate::supervisor::Guard,
        text: Option<String>,
    ) -> Result<()> {
        if !crate::tmux::valid_session_key(&guard.session_key)
            || guard.instance_id.is_empty()
            || guard.instance_id.len() > 4096
            || guard.content_hash.is_empty()
            || guard.content_hash.len() > 128
            || !matches!(guard.status.as_str(), "waiting" | "working")
            || text
                .as_ref()
                .is_some_and(|t| t.is_empty() || t.len() > 2048)
            || (text.is_none() && guard.status != "waiting")
        {
            return Err(bad_request("invalid supervisor mutation guard"));
        }
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                instance_id,
                resume_lease,
                ..
            } => {
                validate_expected_pane_instance(Some(&guard.instance_id), &instance_id)?;
                let prompt_lock = self.prompt_lock(&pane_id);
                let control = self.clone();
                let closing = text.is_none();
                local_tmux(
                    tokio::task::spawn_blocking(move || {
                        control.with_registry_tmux(|| {
                            let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                            let mut state = prompt_lock.state.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let check = || -> Result<()> {
                                let previous_hashes = control.read_state().sessions.iter()
                                    .map(|s| (s.pane_id.clone(), s.content_hash)).collect();
                                let mut sessions = Tmux.sessions_with_capture(
                                    &previous_hashes, &control.inner.config.status,
                                    control.inner.config.general.preview_lines,
                                )?;
                                for session in &mut sessions {
                                    truncate_front(&mut session.content, MAX_CAPTURE_BYTES);
                                }
                                let live = sessions.iter().find(|s| s.pane_id == pane_id)
                                    .ok_or_else(|| conflict("supervised pane disappeared"))?;
                                if live.pane_identity != guard.instance_id
                                    || live.session_key.as_deref() != Some(&guard.session_key)
                                    || format!("{:016x}", observable_content_hash(&live.content)) != guard.content_hash
                                    || live.status.label() != guard.status
                                {
                                    return Err(conflict("supervised pane changed before mutation"));
                                }
                                if closing && (live.attached || live.windows != 1
                                    || !Tmux::supervisor_close_allowed(&pane_id)?)
                                {
                                    return Err(conflict("supervisor only closes an unattached, single-pane idle session"));
                                }
                                Ok(())
                            };
                            check()?;
                            begin_pane_mutation(&pane_id, &prompt_lock, &mut state)?;
                            // Our own paste changes the hash. Evidence is checked before
                            // paste, then generation is checked again before Enter.
                            if let Some(text) = &text {
                                Tmux::send_text_checked(&pane_id, text, true, || {
                                    control.validate_live_message_instance(&pane_id, Some(&guard.instance_id))
                                })?;
                            } else {
                                check()?;
                                // An intentional supervisor close must defeat A4's
                                // desired-running restore before the pane disappears.
                                if let Some(registry) = &control.inner.registry {
                                    let fresh = Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
                                    registry.observe_owner(&fresh, now_ms(), crate::resume_anywhere::server_id())?;
                                    let live = fresh.iter().find(|s| s.pane_id == pane_id)
                                        .ok_or_else(|| conflict("supervised pane disappeared before close"))?;
                                    control.bind_registry_session(live)?;
                                    check()?;
                                    registry.record_close(std::slice::from_ref(&guard.session_key), now_ms())?;
                                }
                                Tmux.kill_pane_session(&pane_id)?;
                            }
                            Ok(())
                        })
                    }).await,
                )?;
                if closing && let Some(lease) = resume_lease {
                    self.release_resume_lease(&lease);
                }
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine,
                pane_id,
                instance_id,
                ..
            } => {
                validate_expected_pane_instance(Some(&guard.instance_id), &instance_id)?;
                self.ensure_online(&machine.id)?;
                let (operation, body) = match text {
                    Some(text) => (
                        "message",
                        serde_json::to_value(crate::supervisor::GuardedMessage { guard, text })?,
                    ),
                    None => ("close", serde_json::to_value(guard)?),
                };
                machine
                    .post_json(
                        &format!(
                            "/api/v1/supervisor/panes/{}/{operation}",
                            encode_segment(&pane_id)
                        ),
                        &body,
                    )
                    .await
                    .map_err(|error| upstream(&error))?;
            }
        }
        Ok(())
    }

    /// Closes the tmux session containing the selected pane.
    /// # Errors
    /// Returns an error for an unknown pane or rejected owner mutation.
    pub async fn kill(&self, id: &str) -> Result<()> {
        match self.resolve(id)? {
            Target::Local {
                pane_id,
                resume_lease,
                ..
            } => {
                let control = self.clone();
                let prompt_lock = self.prompt_lock(&pane_id);
                local_tmux(
                    tokio::task::spawn_blocking(move || {
                        control.with_registry_tmux(|| {
                            let _process_lock = auto_update::PaneProcessLock::acquire(&pane_id)?;
                            let mut guard = prompt_lock
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            begin_pane_mutation(&pane_id, &prompt_lock, &mut guard)?;
                            if let Some(registry) = &control.inner.registry {
                                let fresh =
                                    Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
                                registry.observe_owner(
                                    &fresh,
                                    now_ms(),
                                    crate::resume_anywhere::server_id(),
                                )?;
                                let name = &fresh
                                    .iter()
                                    .find(|session| session.pane_id == pane_id)
                                    .ok_or_else(|| conflict("pane disappeared before close"))?
                                    .name;
                                let mut keys = Vec::new();
                                for session in fresh.iter().filter(|session| &session.name == name)
                                {
                                    control.bind_registry_session(session)?;
                                    if let Some(key) = &session.session_key {
                                        keys.push(key.clone());
                                    }
                                }
                                registry.record_close(&keys, now_ms())?;
                            }
                            Tmux.kill_pane_session(&pane_id)?;
                            Ok(())
                        })
                    })
                    .await,
                )?;
                if let Some(lease) = resume_lease {
                    self.release_resume_lease(&lease);
                }
                self.inner.refresh_now.notify_one();
            }
            Target::Remote {
                machine, pane_id, ..
            } => {
                self.ensure_online(&machine.id)?;
                machine
                    .delete(&format!("/api/v1/sessions/{}", encode_segment(&pane_id)))
                    .await
                    .map_err(|error| upstream(&error))?;
            }
        }
        Ok(())
    }

    /// Launches a configured profile in an allowlisted project directory.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/duplicate input or a failed tmux launch.
    pub async fn launch(&self, request: LaunchRequest) -> Result<()> {
        validate_session_name(&request.name)?;
        let machine = request.machine.clone();
        let target = machine.as_deref().unwrap_or(&self.inner.local_id);
        if target != self.inner.local_id {
            return self.forward_launch(target, request).await;
        }
        self.ensure_local_owner_enabled()?;
        self.ensure_launch_name_available(&request.name)?;
        let directory = self
            .inner
            .config
            .resolve_launch_directory(Path::new(&request.directory))
            .ok_or_else(|| {
                bad_request(format!(
                    "directory must exist within an atmux project root: {}",
                    request.directory
                ))
            })?;
        let profile = profile_by_id(&self.inner.config, &request.profile_id)?;
        let mode = select_launch_mode(&profile, request.mode_id.as_deref())?;
        let requested_memory_max_bytes = request.memory_max_bytes;
        validate_launch_memory(
            &self.inner.config.agent_resources,
            requested_memory_max_bytes,
        )?;
        let resume = self
            .revalidate_resume_candidate(
                request.resume_session_id.as_deref(),
                &profile,
                &request.profile_id,
                &directory,
            )
            .await?;
        let resume_lease = resume
            .as_ref()
            .map(|candidate| persistent_resume_lease(&profile, &directory, candidate));
        let resume_guard = match resume_lease.as_deref() {
            Some(lease) => Some(self.acquire_resume_lease(lease).await?),
            None => None,
        };
        // Summarize before anything is created. A failed summary must leave no
        // half-configured tmux session behind.
        let handover = self
            .prepare_handover(request.summarize_pane_id.as_deref(), &profile, &directory)
            .await?;
        let name = request.name;
        let launched_name = name.clone();
        let resources = self.inner.config.agent_resources;
        let launch_lease = resume_lease.clone();
        let launched = local_tmux(
            tokio::task::spawn_blocking(move || {
                let launched = (|| {
                    let scope = systemd_scope::prepare_override(
                        &resources,
                        requested_memory_max_bytes,
                        &name,
                    )?;
                    match (resume.as_ref(), launch_lease.as_deref()) {
                        (Some(candidate), Some(lease)) => Tmux::launch_resumed(
                            &name,
                            &directory,
                            &profile,
                            mode.as_ref(),
                            candidate,
                            lease,
                            scope,
                        ),
                        (None, None) => {
                            Tmux::launch(&name, &directory, &profile, mode.as_ref(), scope)
                        }
                        _ => Err(anyhow::anyhow!(
                            "saved-conversation launch invariant was violated"
                        )),
                    }
                })();
                if launched.is_ok()
                    && let Some(guard) = resume_guard
                {
                    guard.activate();
                }
                // A launch that never happened must not overwrite the project's
                // recorded launch preference.
                launched.and_then(|()| project::remember_launch(&directory, &name, &profile))
            })
            .await,
        );
        launched?;
        self.inner.refresh_now.notify_one();
        self.spawn_handover_delivery(launched_name, handover);
        Ok(())
    }

    /// Hands one launch to the machine that owns the tmux server.
    async fn forward_launch(&self, target: &str, request: LaunchRequest) -> Result<()> {
        let machine = self.remote_machine(target)?;
        self.ensure_online(&machine.id)?;
        // Summarizing runs a full CLI turn on the node, which outlasts the
        // federated request window. Say so instead of reporting a timeout for
        // a duplicate the node may still be launching.
        if request.summarize_pane_id.is_some() {
            return Err(bad_request(format!(
                "summarizing a previous session takes longer than a launch to {} may stay open; duplicate from that machine's own atmux",
                machine.id
            )));
        }
        if let Some(requested) = request.memory_max_bytes {
            self.ensure_remote_memory_request_advertised(&machine.id, requested)?;
        }
        // The node validates its own directory and profile allowlists; this
        // coordinator never forwards a caller-supplied URL or machine hop.
        let forwarded = LaunchRequest {
            machine: None,
            ..request
        };
        let launch_path = remote_launch_path(&forwarded);
        machine
            .post_json(launch_path, &forwarded)
            .await
            .map_err(|error| upstream(&error))
    }

    /// Runs the target profile's CLI non-interactively over one local pane's
    /// conversation so a duplicate can start where that pane stopped.
    ///
    /// The source pane must live on this machine: a coordinator never reads a
    /// remote machine's transcript, and a forwarded launch summarizes on the
    /// node that owns both the pane and the credentials.
    async fn prepare_handover(
        &self,
        pane_id: Option<&str>,
        profile: &AgentProfile,
        directory: &Path,
    ) -> Result<Option<String>> {
        let Some(pane_id) = pane_id else {
            return Ok(None);
        };
        let session = {
            let state = self.read_state();
            find_session(&state.sessions, pane_id).cloned()
        }
        .ok_or_else(|| not_found(format!("no agent session matches {pane_id}")))?;
        let source = session.clone();
        let transcript =
            tokio::task::spawn_blocking(move || crate::transcript::read(&source, None))
                .await
                .map_err(|error| {
                    internal(&anyhow::Error::new(error).context("transcript read panicked"))
                })?
                .map_err(|error| internal(&error))?;
        let prompt = transcript
            .messages
            .as_deref()
            .and_then(summary::summary_prompt)
            .ok_or_else(|| {
                bad_request(format!(
                    "{} has no readable conversation to summarize",
                    session.name
                ))
            })?;
        let summary = summary::summarize(profile, directory, &prompt)
            .await
            .map_err(|error| upstream(&error))?;
        Ok(Some(summary::initial_prompt(&summary)))
    }

    /// Delivers a duplicate's summarized context once its new CLI reaches an
    /// empty top-level composer.
    ///
    /// The launch itself has already succeeded, so this is deliberately
    /// out-of-band: it never submits into an approval prompt or a working
    /// agent, and it gives up rather than retrying forever.
    fn spawn_handover_delivery(&self, name: String, handover: Option<String>) {
        let Some(text) = handover else {
            return;
        };
        let control = self.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + HANDOVER_DELIVERY_TIMEOUT;
            loop {
                tokio::time::sleep(HANDOVER_DELIVERY_POLL).await;
                if tokio::time::Instant::now() >= deadline {
                    eprintln!("atmux could not hand the summary to {name}: it never became idle");
                    return;
                }
                if control.refresh().await.is_err() {
                    continue;
                }
                let ready = handover_target(
                    &control.read_state().sessions,
                    &name,
                    &control.inner.config.status,
                );
                let Some(pane_id) = ready else {
                    continue;
                };
                if let Err(error) = control.send_text(&pane_id, text, true).await {
                    eprintln!("atmux could not hand the summary to {name}: {error:#}");
                }
                return;
            }
        });
    }

    fn ensure_launch_name_available(&self, name: &str) -> Result<()> {
        if self
            .read_state()
            .sessions
            .iter()
            .any(|session| session.name == name)
        {
            return Err(conflict(format!(
                "a tmux session named {name} already exists"
            )));
        }
        Ok(())
    }

    /// The owner repeats this check against live tmux. This cached pass exists
    /// only so the coordinator can explain a taken name, because it relays an
    /// owner's refusal as a fixed message rather than the owner's own text.
    fn ensure_remote_session_name_available(&self, machine: &str, name: &str) -> Result<()> {
        let taken = self
            .read_state()
            .remotes
            .get(machine)
            .is_some_and(|remote| remote.sessions.iter().any(|session| session.name == name));
        if taken {
            return Err(conflict(format!(
                "a tmux session named {name} already exists on {machine}"
            )));
        }
        Ok(())
    }

    /// Prevents a new coordinator from sending an override to an older owner
    /// which would deserialize the additive field as unknown and silently
    /// launch with its default (or no cap). This cached capability is only a
    /// compatibility gate; the owner repeats current policy and host checks.
    fn ensure_remote_memory_request_advertised(&self, machine: &str, requested: u64) -> Result<()> {
        let state = self.read_state();
        let advertised = state
            .remotes
            .get(machine)
            .and_then(|remote| remote.launch.as_ref())
            .and_then(|options| options.memory.as_ref())
            .filter(|memory| memory.supported);
        let Some(memory) = advertised else {
            return Err(bad_request(format!(
                "machine {machine} has not advertised per-agent memory override support; update that owner or use its Default limit"
            )));
        };
        let resources = crate::config::AgentResourcesConfig {
            memory_max_bytes: memory.default_bytes,
            memory_override_max_bytes: memory.override_max_bytes,
        };
        systemd_scope::resolve_memory_max(&resources, Some(requested))
            .map(|_| ())
            .map_err(|error| {
                bad_request(format!(
                    "machine {machine} did not advertise that memory limit: {error}"
                ))
            })
    }

    async fn revalidate_resume_candidate(
        &self,
        resume_id: Option<&str>,
        profile: &AgentProfile,
        profile_id: &str,
        directory: &Path,
    ) -> Result<Option<ResumeCandidate>> {
        let Some(resume_id) = resume_id else {
            return Ok(None);
        };
        if !valid_opaque_resume_id(resume_id) {
            return Err(bad_request("saved-session id is malformed"));
        }
        let requested = resume_id.to_owned();
        let resume_ids = self.inner.resume_ids.clone();
        let resume_profile = profile.clone();
        let resume_profile_id = profile_id.to_owned();
        let resume_directory = directory.to_path_buf();
        let directory_text = directory.to_string_lossy().into_owned();
        let discovery = tokio::task::spawn_blocking(move || {
            old_sessions::discover(
                &resume_profile,
                &resume_directory,
                DiscoveryLimits::default(),
            )
        })
        .await
        .map_err(|error| {
            internal(&anyhow::Error::new(error).context("saved-session revalidation panicked"))
        })?
        .map_err(|error| internal(&error))?;
        resolve_opaque_resume(
            discovery.sessions,
            &resume_ids,
            &resume_profile_id,
            &directory_text,
            &requested,
        )
        .map(Some)
        .ok_or_else(|| {
            conflict(
                "the selected saved conversation is no longer available; refresh Launch and choose it again",
            )
        })
    }

    async fn acquire_resume_lease(&self, lease: &str) -> Result<ResumeLeaseGuard> {
        {
            let mut leases = self
                .inner
                .resume_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !leases.reserve(lease) {
                return Err(conflict(
                    "that saved conversation is already running or being launched on this machine",
                ));
            }
        }
        let guard = ResumeLeaseGuard::new(Arc::clone(&self.inner), lease);

        let checked = lease.to_owned();
        let acquired = tokio::task::spawn_blocking(move || {
            let lock = acquire_persistent_resume_lock(&checked)?;
            let active =
                Tmux::resume_lease_active(&checked).map_err(ResumeLeaseAcquireError::Tmux)?;
            Ok::<_, ResumeLeaseAcquireError>((lock, active))
        })
        .await;
        match acquired {
            Ok(Ok((lock, false))) => {
                self.inner
                    .resume_leases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .hold_process_lock(lease, lock);
                Ok(guard)
            }
            Ok(Ok((_lock, true))) => {
                guard.activate();
                Err(conflict(
                    "that saved conversation is already running on this machine",
                ))
            }
            Ok(Err(ResumeLeaseAcquireError::Busy)) => Err(conflict(
                "that saved conversation is already being launched by another atmux process",
            )),
            Ok(Err(ResumeLeaseAcquireError::Unsafe)) => Err(internal(&anyhow::anyhow!(
                "the secure saved-conversation launch lock is unavailable"
            ))),
            Ok(Err(ResumeLeaseAcquireError::Tmux(error))) => Err(internal(&error)),
            Err(error) => Err(internal(
                &anyhow::Error::new(error).context("saved-conversation lease lookup panicked"),
            )),
        }
    }

    fn release_resume_lease(&self, lease: &str) {
        self.inner
            .resume_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release(lease);
    }

    /// Waits until the shared observable revision advances.
    pub async fn wait_for_revision(&self, after: u64, timeout: Duration) -> bool {
        wait_for_revision_receiver(self.subscribe(), after, timeout).await
    }

    /// Combines an optional machine selector with a caller-supplied id into one
    /// routable reference.
    ///
    /// Callers may pass an opaque `machine~pane` reference straight from
    /// `agents_list`, or a bare name plus an explicit machine. No caller may
    /// supply a URL or an unconfigured machine.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown machine or a reference that contradicts
    /// the requested machine.
    pub fn reference(&self, id: &str, machine: Option<&str>) -> Result<String> {
        let Some(machine) = machine else {
            return Ok(id.to_owned());
        };
        if !self.has_machine(machine) {
            return Err(bad_request(format!(
                "unknown machine {machine}; call machines_list for valid ids"
            )));
        }
        if let Some((owner, _)) = split_composite(id)
            && self.has_machine(owner)
        {
            if owner != machine {
                return Err(bad_request(format!(
                    "{id} belongs to machine {owner}, not {machine}"
                )));
            }
            return Ok(id.to_owned());
        }
        Ok(composite_id(machine, id))
    }

    fn remote_machine(&self, id: &str) -> Result<Arc<RemoteMachine>> {
        self.inner
            .machines
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| {
                bad_request(format!(
                    "unknown machine {id}; it is neither discovered nor configured"
                ))
            })
    }

    fn ensure_local_owner_enabled(&self) -> Result<()> {
        if self.inner.config.node.coordinator_only {
            return Err(bad_request(format!(
                "machine {} is a coordinator-only node and has no local owner capabilities",
                self.inner.local_id
            )));
        }
        Ok(())
    }

    fn resolve(&self, id: &str) -> Result<Target> {
        self.find_target(id)?
            .ok_or_else(|| not_found(format!("no agent session matches {id}")))
    }

    pub(crate) fn conversation_reference(&self, id: &str) -> Result<String> {
        match self.resolve(id)? {
            Target::Local { pane_id, .. } => Ok(self.local_identity(&pane_id)),
            Target::Remote {
                machine, pane_id, ..
            } => Ok(composite_id(&machine.id, &pane_id)),
        }
    }

    /// Resolves a composite id, a bare pane id, or a session name to its owner.
    ///
    /// Bare identifiers keep working exactly as they did before federation: a
    /// match on this machine always wins, and a bare id that matches several
    /// remote machines is rejected instead of guessing. Callers that want to
    /// scope a bare id to one machine build a composite id with
    /// [`ControlPlane::reference`] first.
    fn find_target(&self, id: &str) -> Result<Option<Target>> {
        if self.inner.config.node.coordinator_only
            && split_composite(id).is_some_and(|(machine, _)| machine == self.inner.local_id)
        {
            self.ensure_local_owner_enabled()?;
        }
        let (scope, bare) = match split_composite(id) {
            Some((machine, pane)) if self.has_machine(machine) => (Some(machine), pane),
            _ => (None, id),
        };
        let mut matches = Vec::new();
        let state = self.read_state();
        if scope.is_none_or(|scope| scope == self.inner.local_id)
            && let Some(session) = find_session(&state.sessions, bare)
        {
            matches.push(Target::Local {
                pane_id: session.pane_id.clone(),
                instance_id: session.pane_identity.clone(),
                name: session.name.clone(),
                agent: session.agent,
                resume_lease: session.resume_lease.clone(),
            });
        }
        for machine in self.remote_machines() {
            if scope.is_some_and(|scope| scope != machine.id) {
                continue;
            }
            let Some(remote) = state.remotes.get(&machine.id) else {
                continue;
            };
            if let Some(session) = remote
                .sessions
                .iter()
                .find(|session| session.pane_id == bare || session.name == bare)
            {
                matches.push(Target::Remote {
                    machine: Arc::clone(&machine),
                    pane_id: session.pane_id.clone(),
                    instance_id: session.instance_id.clone(),
                    name: session.name.clone(),
                });
            }
        }
        let offline = matches.is_empty()
            && scope.is_some_and(|scope| {
                state
                    .remotes
                    .get(scope)
                    .is_some_and(|remote| !remote.online)
            });
        drop(state);
        if offline {
            // Say why the machine is unreachable instead of claiming the
            // session never existed.
            self.ensure_online(scope.unwrap_or_default())?;
        }
        match matches.len() {
            0 => Ok(None),
            // A local match always wins, preserving pre-federation behaviour.
            _ if matches!(matches[0], Target::Local { .. }) => Ok(Some(matches.remove(0))),
            1 => Ok(Some(matches.remove(0))),
            _ => Err(bad_request(format!(
                "{id} matches sessions on several machines; use a machine~pane id such as {}",
                target_hint(&matches)
            ))),
        }
    }
}

fn project_preferences(paths: &[PathBuf]) -> BTreeMap<String, ProjectPreferences> {
    paths
        .iter()
        .filter_map(|path| {
            project::load(path)
                .ok()
                .flatten()
                .map(|preferences| (path.to_string_lossy().into_owned(), preferences))
        })
        .collect()
}

fn browse_configured_directories(
    config: &Config,
    machine: String,
    requested: Option<&str>,
) -> Result<LaunchDirectoryListing> {
    let Some(requested) = requested else {
        let roots = config.launch_roots();
        let truncated = roots.len() > MAX_BROWSE_DIRECTORIES;
        let directories = roots
            .into_iter()
            .take(MAX_BROWSE_DIRECTORIES)
            .filter_map(|path| browse_directory(&path))
            .collect();
        return Ok(LaunchDirectoryListing {
            machine,
            current: None,
            parent: None,
            directories,
            truncated,
        });
    };

    let requested = requested.trim();
    if requested.is_empty()
        || requested.len() > MAX_BROWSE_PATH_BYTES
        || requested.chars().any(char::is_control)
    {
        return Err(bad_request(
            "browse path is empty, oversized, or contains control characters",
        ));
    }
    let current = config
        .resolve_launch_directory(Path::new(requested))
        .ok_or_else(|| bad_request("browse path must exist within an atmux project root"))?;
    let parent = current
        .parent()
        .and_then(|parent| config.resolve_launch_directory(parent))
        .and_then(|path| path.to_str().map(str::to_owned));
    let mut directories = Vec::new();
    let mut truncated = false;
    let entries = fs::read_dir(&current)
        .with_context(|| format!("failed to browse {}", current.display()))?;
    for (index, result) in entries.enumerate() {
        if index >= MAX_BROWSE_SCAN_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(entry) = result else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(path) = config.resolve_launch_directory(&entry.path()) else {
            continue;
        };
        let Some(directory) = browse_directory(&path) else {
            continue;
        };
        directories.push(directory);
    }
    directories.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    directories.dedup_by(|left, right| left.path == right.path);
    if directories.len() > MAX_BROWSE_DIRECTORIES {
        directories.truncate(MAX_BROWSE_DIRECTORIES);
        truncated = true;
    }
    Ok(LaunchDirectoryListing {
        machine,
        current: current.to_str().map(str::to_owned),
        parent,
        directories,
        truncated,
    })
}

fn browse_directory(path: &Path) -> Option<BrowseDirectory> {
    let path = path.to_str()?.to_owned();
    let name = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&path)
        .to_owned();
    Some(BrowseDirectory { path, name })
}

fn launch_directory_action_result(
    config: &Config,
    machine: String,
    parent: &str,
    created: &Path,
) -> Result<LaunchDirectoryActionResult> {
    let directory = browse_directory(created)
        .ok_or_else(|| internal(&anyhow::anyhow!("created directory path is not UTF-8")))?;
    let listing = browse_configured_directories(config, machine, Some(parent))?;
    Ok(LaunchDirectoryActionResult { directory, listing })
}

fn model_capabilities(
    pane_id: String,
    agent: AgentKind,
    profile_name: &str,
    observation: crate::tmux::ModelObservation,
    profiles: &[AgentProfile],
) -> PaneModels {
    let harness = agent.to_string().to_lowercase();
    let profile = profile_for_session(profiles, agent, profile_name);
    let known = observation
        .version
        .as_deref()
        .map_or(&[][..], |version| known_models(agent, version));
    let models: Vec<PaneModelOption> = profile
        .map(|profile| {
            profile
                .modes
                .iter()
                .map(|mode| PaneModelOption {
                    id: mode.id.clone(),
                    label: mode.display_label(),
                    switchable: mode_switchable(agent, mode, known),
                })
                .collect()
        })
        .unwrap_or_default();
    let known_efforts = observation
        .version
        .as_deref()
        .map_or(&[][..], |version| known_efforts(agent, version));
    let model_options = distinct_options(profile, known, |mode| Some(mode.model.as_str()));
    let effort_options = effort_choices(
        distinct_options(profile, known_efforts, |mode| mode.effort.as_deref()),
        known_efforts,
    );
    let fast_supported = observation
        .version
        .as_deref()
        .is_some_and(|version| fast_toggle_verified(agent, version))
        // A harness that already refused `/fast` cannot be talked into it, so
        // the control is withdrawn rather than left to fail on every press.
        && !observation.fast_unavailable;
    let note = capability_note(&harness, agent, profile, profile_name, known, &observation);
    let current_mode = observation
        .mode
        .filter(|mode_id| {
            profile.is_some_and(|profile| {
                profile.modes.iter().any(|mode| {
                    mode.id == *mode_id
                        && observation
                            .current
                            .as_deref()
                            .is_none_or(|current| current == mode.model)
                })
            })
        })
        .or_else(|| {
            let current = observation.current.as_deref()?;
            let matches = profile?
                .modes
                .iter()
                .filter(|mode| mode.model == current)
                .filter(|mode| mode.effort == observation.effort)
                .collect::<Vec<_>>();
            (matches.len() == 1).then(|| matches[0].id.clone())
        });
    PaneModels {
        pane_id,
        harness,
        current: observation.current,
        effort: observation.effort,
        current_mode,
        fast: observation.fast,
        version: observation.version,
        models,
        model_options,
        effort_options,
        fast_supported,
        note,
        resume_available: false,
        resume_note: None,
        restart_token: None,
    }
}

/// This pane's controls as they stand once a switch has settled.
///
/// The pane is read again rather than reusing the poll's snapshot: the
/// confirmation the harness just printed is what proves which controls the
/// session now has, and the poll is up to a second behind. The answer stands in
/// for a `GET /models` read, so it carries that endpoint's resume fields too
/// rather than making the dashboard drop the resume control.
fn settled_model_capabilities(
    inner: &Inner,
    pane_id: &str,
    identity: String,
    agent: AgentKind,
    session: &Session,
    fallback: String,
) -> PaneModels {
    let after = Tmux
        .capture(pane_id, inner.config.general.preview_lines)
        .unwrap_or(fallback);
    let mut settled = model_capabilities(
        identity,
        agent,
        &session.profile,
        Tmux.model_observation(pane_id, agent, &after),
        &inner.config.profiles,
    );
    let mut restarted_session = session.clone();
    restarted_session.content = after;
    let resume = agent_restart_capability(
        &inner.config.profiles,
        &inner.config.status,
        &restarted_session,
    );
    settled.resume_available = resume.available;
    settled.resume_note = resume.note;
    settled.restart_token = agent_restart_token(&restarted_session);
    settled
}

/// The one thing most worth telling the owner about this pane's controls.
///
/// Obstructions are ranked rather than concatenated: a pane whose harness or
/// profile offers nothing at all is explained ahead of one that merely lost its
/// fast toggle, because that is the problem the owner has to act on first.
fn capability_note(
    harness: &str,
    agent: AgentKind,
    profile: Option<&AgentProfile>,
    profile_name: &str,
    known: &[crate::tmux::KnownModel],
    observation: &crate::tmux::ModelObservation,
) -> Option<String> {
    if agent != AgentKind::Other && profile.is_none() {
        return Some(format!(
            "No configured {harness} profile matches this pane's profile {profile_name:?}"
        ));
    }
    if profile.is_some_and(|profile| profile.modes.is_empty()) {
        return Some(format!(
            "Profile {profile_name:?} defines no selectable modes; add [[profiles.modes]]"
        ));
    }
    match agent {
        AgentKind::Other => {
            return Some("This pane is not a recognized Claude or Codex CLI".to_owned());
        }
        AgentKind::Claude | AgentKind::Codex if observation.version.is_none() => {
            return Some(format!(
                "The running {harness} CLI version is not visible yet; model switching is unavailable"
            ));
        }
        AgentKind::Claude | AgentKind::Codex if known.is_empty() => {
            return Some(format!(
                "{} {} has an unsupported interactive model picker",
                harness,
                observation.version.as_deref().unwrap_or("unknown")
            ));
        }
        AgentKind::Claude | AgentKind::Codex if observation.current.is_none() => {
            return Some(format!(
                "The running {harness} model is not visible yet; choices are owner-reported but switching may be rejected"
            ));
        }
        AgentKind::Claude | AgentKind::Codex => {}
    }
    // Nothing more fundamental is wrong, so a withdrawn fast toggle is worth
    // explaining rather than leaving the control to vanish unaccounted for.
    observation
        .fast_unavailable
        .then(|| format!("{harness} fast mode is disabled for this account or organization"))
}

#[derive(Default)]
struct AgentRestartCapability {
    available: bool,
    note: Option<String>,
}

fn agent_restart_capability(
    profiles: &[AgentProfile],
    status: &crate::config::StatusConfig,
    session: &Session,
) -> AgentRestartCapability {
    match session.agent {
        AgentKind::Claude => {
            claude_resume_capability(session, crate::config::resume_claude_program().as_deref())
        }
        AgentKind::Codex => codex_restart_capability(profiles, status, session),
        AgentKind::Other => AgentRestartCapability::default(),
    }
}

fn claude_resume_capability(
    session: &Session,
    claude_program: Option<&Path>,
) -> AgentRestartCapability {
    if session.agent != AgentKind::Claude {
        return AgentRestartCapability::default();
    }
    if session.status == AgentStatus::Working {
        return AgentRestartCapability {
            available: false,
            note: Some("Claude is working; wait or interrupt before relaunching".to_owned()),
        };
    }
    if claude_program.is_none() {
        return AgentRestartCapability {
            available: false,
            note: Some("The current Claude launcher is unavailable on this machine".to_owned()),
        };
    }
    if crate::transcript::claude_resume_target(session).is_none() {
        return AgentRestartCapability {
            available: false,
            note: Some(
                "This Claude pane cannot be safely matched to one saved conversation".to_owned(),
            ),
        };
    }
    AgentRestartCapability {
        available: true,
        note: None,
    }
}

fn codex_restart_capability(
    profiles: &[AgentProfile],
    status: &crate::config::StatusConfig,
    session: &Session,
) -> AgentRestartCapability {
    if session.status != AgentStatus::Waiting
        || !crate::status::automation_idle(session.agent, &session.content, &session.title, status)
    {
        return AgentRestartCapability {
            available: false,
            note: Some(if session.status == AgentStatus::Working {
                "Codex is working; wait or interrupt before restarting".to_owned()
            } else {
                "Codex is not at a confirmed empty top-level prompt".to_owned()
            }),
        };
    }
    let launcher = auto_update::current_launcher(UpdateHarness::Codex);
    let Some(launcher) = launcher else {
        return AgentRestartCapability {
            available: false,
            note: Some("The current Codex launcher is unavailable on this machine".to_owned()),
        };
    };
    let Some((profile, _, _)) = native_restart_preflight(profiles, session, UpdateHarness::Codex)
    else {
        return AgentRestartCapability {
            available: false,
            note: Some(
                "This Codex pane cannot be safely matched to its configured controls and saved conversation"
                    .to_owned(),
            ),
        };
    };
    if !profile_bound_to_native(&profile, UpdateHarness::Codex, &launcher) {
        return AgentRestartCapability {
            available: false,
            note: Some("This Codex profile is not bound to the current native launcher".to_owned()),
        };
    }
    AgentRestartCapability {
        available: true,
        note: None,
    }
}

fn restart_local_codex_blocking(
    control: &ControlPlane,
    pane_id: &str,
    prompt_lock: &PaneMutationGate,
    expected_generation: u64,
    resources: &crate::config::AgentResourcesConfig,
    expected: Option<&RestartBinding>,
) -> Result<()> {
    let _process_lock = auto_update::PaneProcessLock::acquire(pane_id)?;
    let mut guard = prompt_lock
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    validate_live_pane_instance(
        pane_id,
        expected.map(|binding| binding.instance_id.as_str()),
    )?;
    if !resume_request_is_current(&guard, expected_generation) {
        return Err(ResumeRejected(
            "this pane changed after the restart was requested; review it and try again".to_owned(),
        )
        .into());
    }
    let session = control
        .fresh_cli_update_session(pane_id)
        .ok_or_else(|| ResumeRejected("this pane no longer exists".to_owned()))?;
    validate_restart_binding(expected, &session)?;
    if session.agent != AgentKind::Codex {
        return Err(ResumeRejected("this pane is no longer running Codex".to_owned()).into());
    }
    if session.status != AgentStatus::Waiting
        || !crate::status::automation_idle(
            session.agent,
            &session.content,
            &session.title,
            &control.inner.config.status,
        )
    {
        let message = if session.status == AgentStatus::Working {
            "Codex is working; wait or interrupt before restarting"
        } else {
            "Codex is not at a confirmed empty top-level prompt"
        };
        return Err(ResumeRejected(message.to_owned()).into());
    }
    let launcher = auto_update::current_launcher(UpdateHarness::Codex).ok_or_else(|| {
        ResumeRejected("the current Codex launcher is unavailable on this machine".to_owned())
    })?;
    let (profile, mode, target) = control
        .cli_update_preflight_for_session(&session, UpdateHarness::Codex)
        .ok_or_else(|| {
            ResumeRejected(
                "this Codex pane cannot be safely matched to its configured controls and saved conversation"
                    .to_owned(),
            )
        })?;
    if !profile_bound_to_native(&profile, UpdateHarness::Codex, &launcher) {
        return Err(ResumeRejected(
            "this Codex profile is not bound to the current native launcher".to_owned(),
        )
        .into());
    }
    let scope = systemd_scope::prepare_override(resources, session.memory_max_bytes, pane_id)?;
    if expected.is_some() {
        let latest = control
            .fresh_cli_update_session(pane_id)
            .ok_or_else(|| ResumeRejected("this pane no longer exists".to_owned()))?;
        validate_restart_binding(expected, &latest)?;
        if latest.agent != AgentKind::Codex
            || latest.status != AgentStatus::Waiting
            || !crate::status::automation_idle(
                latest.agent,
                &latest.content,
                &latest.title,
                &control.inner.config.status,
            )
        {
            return Err(ResumeRejected(
                "Codex is no longer at an empty top-level prompt".to_owned(),
            )
            .into());
        }
    }
    validate_live_pane_instance(
        pane_id,
        expected.map(|binding| binding.instance_id.as_str()),
    )?;
    begin_pane_mutation(pane_id, prompt_lock, &mut guard)?;
    Tmux::resume_after_cli_update(
        pane_id,
        &session.path,
        &launcher,
        UpdateHarness::Codex,
        &profile,
        &mode,
        &target,
        scope,
    )?;
    Ok(())
}

/// Re-reads one exact pane while its mutation gate is held, then derives the
/// native Claude target from that current process only.  Nothing cached in the
/// overview is trusted for a destructive respawn.
fn fresh_claude_resume_target(
    pane_id: &str,
    status: &crate::config::StatusConfig,
    capture_lines: usize,
) -> Result<(Session, crate::transcript::ClaudeResumeTarget, PathBuf)> {
    let sessions = Tmux
        .sessions_with_capture(&HashMap::new(), status, capture_lines)
        .context("could not re-scan the Claude pane before relaunching")?;
    let session = sessions
        .into_iter()
        .find(|session| session.pane_id == pane_id)
        .ok_or_else(|| ResumeRejected("this pane no longer exists".to_owned()))?;
    let session = validate_fresh_claude_resume_session(session)?;
    let target = crate::transcript::claude_resume_target(&session).ok_or_else(|| {
        ResumeRejected(
            "this Claude process can no longer be safely matched to one saved conversation"
                .to_owned(),
        )
    })?;
    let claude_program = crate::config::resume_claude_program().ok_or_else(|| {
        ResumeRejected("the current Claude launcher is unavailable on this machine".to_owned())
    })?;
    Ok((session, target, claude_program))
}

/// Checks the volatile properties which make an in-place Claude respawn safe.
/// Kept separate from the tmux scan so the fail-closed conditions are directly
/// unit-testable without touching a real tmux server.
fn validate_fresh_claude_resume_session(session: Session) -> Result<Session> {
    if session.agent != AgentKind::Claude {
        return Err(ResumeRejected("this pane is no longer running Claude".to_owned()).into());
    }
    if session.status == AgentStatus::Working {
        return Err(ResumeRejected(
            "Claude is working; wait or interrupt before relaunching".to_owned(),
        )
        .into());
    }
    Ok(session)
}

fn begin_pane_mutation(
    pane_id: &str,
    gate: &PaneMutationGate,
    state: &mut PaneMutationState,
) -> Result<()> {
    Tmux::advance_pane_mutation_sequence(pane_id)?;
    mark_gate_mutated(gate, state);
    Ok(())
}

/// `agent_started_ms` is an approximate `now - ps etime` and drifts on every
/// scan. Bind destructive restart confirmation to the OS creation stamp instead.
fn agent_restart_token(session: &Session) -> Option<String> {
    let stamp = native_process_start_stamp(session.agent_pid?)?;
    restart_token_from_stamp(session, &stamp)
}

fn restart_token_from_stamp(session: &Session, stamp: &str) -> Option<String> {
    let agent_pid = session.agent_pid.filter(|pid| *pid > 0)?;
    if !valid_pane_identity(&session.pane_identity) || session.pane_pid == 0 || stamp.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(session.pane_identity.as_bytes());
    hash.update(session.pane_pid.to_be_bytes());
    hash.update(agent_pid.to_be_bytes());
    hash.update(stamp.as_bytes());
    Some(format!("restart-v1-{:x}", hash.finalize()))
}

/// Logs an unresolvable profile session store once per profile, since the
/// registry snapshot that encounters it repeats on every refresh.
fn warn_unbound_registry_profile(profile: &str, error: &anyhow::Error) {
    static WARNED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    if WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(profile.to_owned())
    {
        eprintln!(
            "atmux registry: sessions of profile {profile:?} stay unbound from native logs: {error:#}"
        );
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn native_process_start_stamp(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    linux_process_start_ticks(&stat).map(|ticks| format!("linux:{ticks}"))
}

#[cfg(target_os = "linux")]
fn linux_process_start_ticks(stat: &str) -> Option<u64> {
    // comm (field 2) may contain spaces and parentheses; everything after its
    // final ')' starts at field 3, putting starttime (field 22) at index 19.
    let (_, fields) = stat.rsplit_once(')')?;
    fields
        .split_whitespace()
        .nth(19)?
        .parse::<u64>()
        .ok()
        .filter(|ticks| *ticks > 0)
}

#[cfg(target_os = "macos")]
pub(crate) fn native_process_start_stamp(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let output = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?.trim();
    if text.is_empty()
        || text.len() > 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b' ' || byte == b':')
    {
        return None;
    }
    Some(format!("macos:{text}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn native_process_start_stamp(_pid: u32) -> Option<String> {
    None
}

fn valid_restart_token(token: &str) -> bool {
    token.strip_prefix("restart-v1-").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn validate_restart_binding(expected: Option<&RestartBinding>, session: &Session) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    validate_expected_pane_instance(Some(&expected.instance_id), &session.pane_identity)?;
    if agent_restart_token(session).as_deref() != Some(expected.restart_token.as_str()) {
        return Err(conflict(
            "agent process changed after restart confirmation; refresh and try again",
        ));
    }
    Ok(())
}

fn validate_expected_pane_instance(expected: Option<&str>, actual: &str) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if !valid_pane_identity(expected) {
        return Err(bad_request("invalid pane instance id"));
    }
    if expected != actual {
        return Err(conflict(
            "agent pane restarted before the message could be delivered",
        ));
    }
    Ok(())
}

fn validate_session_update_instance(expected: &str, actual: &str) -> Result<()> {
    if !valid_pane_identity(expected) {
        return Err(bad_request("invalid pane instance id"));
    }
    if expected != actual {
        return Err(conflict(
            "the session's agent pane was replaced; reopen it and try again",
        ));
    }
    Ok(())
}

fn validate_expected_pane_machine(expected: &str, actual: &str) -> Result<()> {
    validate_machine_id(expected).map_err(|_| bad_request("invalid pane machine id"))?;
    if expected != actual {
        return Err(conflict(
            "agent owning machine changed before the key could be delivered",
        ));
    }
    Ok(())
}

fn validate_live_pane_instance(pane_id: &str, expected: Option<&str>) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let live = Tmux::live_pane_identity(pane_id)?
        .ok_or_else(|| conflict("agent pane disappeared before message delivery"))?;
    validate_expected_pane_instance(Some(expected), &live.pane_identity)
}

fn mark_gate_mutated(gate: &PaneMutationGate, state: &mut PaneMutationState) {
    state.generation = state.generation.wrapping_add(1);
    gate.generation.store(state.generation, Ordering::Release);
}

fn resume_request_is_current(state: &PaneMutationState, expected_generation: u64) -> bool {
    state.generation == expected_generation
}

fn profile_for_session<'a>(
    profiles: &'a [AgentProfile],
    agent: AgentKind,
    profile_name: &str,
) -> Option<&'a AgentProfile> {
    let harness = agent.to_string();
    profiles.iter().find(|profile| {
        profile.harness.eq_ignore_ascii_case(&harness)
            && profile.name.eq_ignore_ascii_case(profile_name)
    })
}

/// Resolves the configured controls and native saved conversation needed to
/// replace one pane without replaying its raw tmux start command.
fn native_restart_preflight(
    profiles: &[AgentProfile],
    session: &Session,
    harness: UpdateHarness,
) -> Option<(
    AgentProfile,
    ProfileMode,
    crate::transcript::NativeResumeTarget,
)> {
    if session.agent != update_agent(harness) {
        return None;
    }
    let profile = profile_for_session(profiles, session.agent, &session.profile)?.clone();
    let observation = Tmux.model_observation(&session.pane_id, session.agent, &session.content);
    let service_tier = Tmux::cli_update_service_tier(&session.pane_id).ok()?;
    // A model/effort/fast switch applied on its own leaves no configured mode,
    // so the pane's recorded controls stand in for one.
    let recorded = match observation.mode {
        Some(_) => None,
        None => Tmux::recorded_pane_mode(&session.pane_id).ok()?,
    };
    let mode = preflight_mode(&profile, &observation, recorded, service_tier.as_deref())?;
    let target = crate::transcript::native_resume_target(session)?;
    Some((profile, mode, target))
}

/// The exact mode a pane is running, taken from the configured mode it still
/// records or, once a split model/effort/fast switch cleared that, from the
/// controls the pane recorded for itself. Either candidate must still match
/// what the pane visibly runs, and a pane that records neither fails closed so
/// maintenance never relaunches it with a mode it was not using.
fn preflight_mode(
    profile: &AgentProfile,
    observation: &crate::tmux::ModelObservation,
    recorded: Option<ProfileMode>,
    service_tier: Option<&str>,
) -> Option<ProfileMode> {
    let mode = match observation.mode.as_deref() {
        Some(mode_id) => profile
            .modes
            .iter()
            .find(|mode| mode.id == mode_id)?
            .clone(),
        None => recorded?,
    };
    (observation.current.as_deref() == Some(mode.model.as_str())
        && observation.effort == mode.effort
        && service_tier == mode.service_tier.as_deref())
    .then_some(mode)
}

const fn update_agent(harness: UpdateHarness) -> AgentKind {
    match harness {
        UpdateHarness::Claude => AgentKind::Claude,
        UpdateHarness::Codex => AgentKind::Codex,
    }
}

const fn maintenance_harness(agent: AgentKind) -> Option<UpdateHarness> {
    match agent {
        AgentKind::Claude => Some(UpdateHarness::Claude),
        AgentKind::Codex => Some(UpdateHarness::Codex),
        AgentKind::Other => None,
    }
}

fn profile_bound_to_native(
    profile: &AgentProfile,
    harness: UpdateHarness,
    launcher: &Path,
) -> bool {
    profile.harness.eq_ignore_ascii_case(harness.name())
        && (profile.command == harness.name()
            || Path::new(&profile.command).canonicalize().ok().as_deref() == Some(launcher))
}

/// Rejects a switch request whose ids are not data-only or that names no
/// control at all, before any pane is resolved.
fn validate_model_switch(request: &ModelSwitchRequest) -> Result<()> {
    let opaque_id = |value: &Option<String>| {
        value
            .as_deref()
            .is_some_and(|value| !valid_profile_mode_id(value))
    };
    if opaque_id(&request.mode_id) || opaque_id(&request.effort) {
        return Err(bad_request(
            "mode and effort ids must be 1-80 ASCII letters, digits, dashes, or underscores",
        ));
    }
    if request
        .model
        .as_deref()
        .is_some_and(|model| !crate::tmux::valid_model_id(model))
    {
        return Err(bad_request(
            "model ids must be 1-80 ASCII letters, digits, dots, dashes, underscores, or colons",
        ));
    }
    if request.mode_id.is_none()
        && request.model.is_none()
        && request.effort.is_none()
        && request.fast.is_none()
    {
        return Err(bad_request(
            "a model switch must name a mode, model, effort, or fast state",
        ));
    }
    Ok(())
}

/// Checks every requested control against the pane's own owner-reported
/// capabilities and returns the running harness version to drive them with.
fn authorize_model_switch(
    capabilities: PaneModels,
    request: &ModelSwitchRequest,
) -> Result<String> {
    let harness = capabilities.harness;
    if let Some(mode_id) = request.mode_id.as_deref() {
        require_switchable(&capabilities.models, mode_id, "mode", &harness)?;
    }
    if let Some(model) = request.model.as_deref() {
        require_switchable(&capabilities.model_options, model, "model", &harness)?;
    }
    if let Some(effort) = request.effort.as_deref() {
        require_switchable(&capabilities.effort_options, effort, "effort", &harness)?;
    }
    if request.fast.is_some() && !capabilities.fast_supported {
        return Err(conflict(format!(
            "this {harness} version has no fast mode atmux can switch"
        )));
    }
    capabilities.version.ok_or_else(|| {
        conflict(
            capabilities
                .note
                .unwrap_or_else(|| "the running CLI version is not observable".to_owned()),
        )
    })
}

/// Rejects a requested control value the pane's owning machine did not report
/// as switchable, so a browser value never escapes the owner's allowlist.
fn require_switchable(
    options: &[PaneModelOption],
    id: &str,
    control: &str,
    harness: &str,
) -> Result<()> {
    let choice = options
        .iter()
        .find(|choice| choice.id == id)
        .ok_or_else(|| {
            bad_request(format!(
                "{control} {id} is not reported by this pane's owning machine"
            ))
        })?;
    if !choice.switchable {
        return Err(conflict(format!(
            "{control} {id} is available only when launching a new {harness} session"
        )));
    }
    Ok(())
}

/// The reasoning levels this pane can actually be switched to.
///
/// Efforts differ from models: a model is an entitlement the profile has to
/// grant, but every level of the installed harness's own effort control is
/// already reachable from the running session. No shipped profile mode sets
/// `effort`, so deriving the choices from modes alone left the control dead for
/// every Claude pane. Configured levels stay first so an owner's preferred
/// ordering still leads, with the rest of the verified list appended.
fn effort_choices(
    configured: Vec<PaneModelOption>,
    known: &[crate::tmux::KnownModel],
) -> Vec<PaneModelOption> {
    let mut options = configured;
    for effort in known {
        if options.iter().any(|option| option.id == effort.id) {
            continue;
        }
        options.push(PaneModelOption {
            id: effort.id.to_owned(),
            label: effort.label.to_owned(),
            switchable: true,
        });
    }
    options
}

/// One control's distinct owner-configured values, in configuration order and
/// switchable only where the installed harness has a verified row for them.
fn distinct_options(
    profile: Option<&AgentProfile>,
    known: &[crate::tmux::KnownModel],
    value: impl Fn(&ProfileMode) -> Option<&str>,
) -> Vec<PaneModelOption> {
    let mut options: Vec<PaneModelOption> = Vec::new();
    for mode in profile.map_or(&[][..], |profile| profile.modes.as_slice()) {
        let Some(id) = value(mode) else { continue };
        if options.iter().any(|option| option.id == id) {
            continue;
        }
        let verified = known.iter().find(|candidate| candidate.id == id);
        options.push(PaneModelOption {
            id: id.to_owned(),
            label: verified.map_or_else(|| id.to_owned(), |known| known.label.to_owned()),
            switchable: verified.is_some(),
        });
    }
    options
}

fn mode_switchable(
    agent: AgentKind,
    mode: &ProfileMode,
    known: &[crate::tmux::KnownModel],
) -> bool {
    known.iter().any(|candidate| candidate.id == mode.model)
        && match agent {
            AgentKind::Claude => {
                mode.service_tier.is_none()
                    && mode
                        .effort
                        .as_deref()
                        .is_none_or(crate::tmux::valid_claude_effort)
            }
            AgentKind::Codex => {
                mode.service_tier.is_none()
                    && mode.effort.as_deref().is_none_or(|effort| effort != "none")
            }
            AgentKind::Other => false,
        }
}

fn profile_mode_option(mode: &ProfileMode) -> ProfileModeOption {
    ProfileModeOption {
        id: mode.id.clone(),
        label: mode.display_label(),
        model: mode.model.clone(),
        effort: mode.effort.clone(),
        service_tier: mode.service_tier.clone(),
    }
}

fn valid_profile_mode_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Classifies the result of a blocking tmux call made on this coordinator.
///
/// A tmux failure is never the caller's fault, so it stays [`ErrorKind::Internal`].
/// One authenticated client per configured remote machine.
fn remote_machine_handles(config: &Config) -> Result<BTreeMap<String, Arc<RemoteMachine>>> {
    let mut machines = BTreeMap::new();
    for machine in &config.machines {
        let machine = Arc::new(match &config.node.tls {
            Some(tls) => RemoteMachine::from_config_with_tls(machine, tls)?,
            None => RemoteMachine::from_config(machine)?,
        });
        machines.insert(machine.id.clone(), machine);
    }
    Ok(machines)
}

type NodeStores = (
    Option<Arc<crate::events::EventService>>,
    Option<Arc<crate::registry::Registry>>,
    Option<crate::machine::Secret>,
);

/// Opens this node's durable event spool and session registry, when configured.
fn open_node_stores(config: &Config, coordinator: bool) -> Result<NodeStores> {
    let events = config
        .events
        .clone()
        .map(|events| {
            crate::events::EventService::open(events, config.node.id.clone(), coordinator)
        })
        .transpose()?;
    let registry = crate::registry::Registry::open(&config.registry, &config.node.id)?;
    let registry_peer_token = if registry.is_some() {
        crate::machine::resolve_token(
            &config.node.id,
            config.node.token_env.as_deref(),
            config.node.token_file.as_deref(),
        )?
    } else {
        None
    };
    Ok((events, registry, registry_peer_token))
}

fn local_tmux(joined: std::result::Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(internal(&error)),
        Err(error) => Err(internal(
            &anyhow::Error::new(error).context("a tmux task panicked"),
        )),
    }
}

/// Classifies a blocking message mutation while preserving an intentional
/// caller-visible conflict from its owner-local generation checks.
fn local_message_mutation<T>(
    joined: std::result::Result<Result<T>, tokio::task::JoinError>,
) -> Result<T> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if error_kind(&error) != ErrorKind::Internal => Err(error),
        Ok(Err(error)) => Err(internal(&error)),
        Err(error) => Err(internal(
            &anyhow::Error::new(error).context("a message mutation task panicked"),
        )),
    }
}

fn local_model_switch<T>(
    joined: std::result::Result<Result<T>, tokio::task::JoinError>,
) -> Result<T> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        // A refused `/fast` is the account's own policy, not this node
        // malfunctioning, so it reaches the dashboard as a stated conflict
        // rather than an opaque internal error.
        Ok(Err(error))
            if error.chain().any(|cause| {
                cause.is::<UnsupportedModelControl>()
                    || cause.is::<crate::tmux::ClaudeFastUnavailable>()
            }) =>
        {
            Err(conflict(format!("{error:#}")))
        }
        // The pre-flight capability checks now run on the blocking thread, so
        // their already-classified rejections keep their caller-facing status.
        Ok(Err(error)) if !matches!(error_kind(&error), ErrorKind::Internal) => Err(error),
        Ok(Err(error)) => Err(internal(&error)),
        Err(error) => Err(internal(
            &anyhow::Error::new(error).context("a model switch task panicked"),
        )),
    }
}

fn local_agent_restart(
    joined: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error))
            if error.chain().any(|cause| {
                cause.is::<ResumeRejected>() || cause.is::<crate::tmux::ClaudeResumeUnavailable>()
            }) =>
        {
            Err(conflict(format!("{error:#}")))
        }
        Ok(Err(error)) if error_kind(&error) != ErrorKind::Internal => Err(error),
        Ok(Err(error)) => Err(internal(&error)),
        Err(error) => Err(internal(
            &anyhow::Error::new(error).context("an agent restart task panicked"),
        )),
    }
}

fn target_hint(matches: &[Target]) -> String {
    matches
        .iter()
        .filter_map(|target| match target {
            Target::Remote {
                machine, pane_id, ..
            } => Some(composite_id(&machine.id, pane_id)),
            Target::Local { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

fn remote_offline_note(remote: &RemoteState) -> String {
    remote.health.clone().map_or_else(
        || "offline".to_owned(),
        |health| format!("offline: {health}"),
    )
}

/// Names the pane a pending handover may be submitted into.
///
/// The duplicate's CLI has to have reached its own empty, top-level composer:
/// the footer strip both harnesses now draw beneath that composer is chrome,
/// not output, so it must not be mistaken for a busy or questioning pane.
fn handover_target(
    sessions: &[Session],
    name: &str,
    config: &crate::config::StatusConfig,
) -> Option<String> {
    find_session(sessions, name).and_then(|session| {
        crate::status::automation_idle(session.agent, &session.content, &session.title, config)
            .then(|| session.pane_id.clone())
    })
}

fn find_session<'a>(sessions: &'a [Session], id: &str) -> Option<&'a Session> {
    sessions
        .iter()
        .find(|session| session.pane_id == id || session.name == id)
}

fn observable_sessions_equal(previous: &[Session], current: &[Session]) -> bool {
    if previous.len() != current.len() {
        return false;
    }
    let old = previous
        .iter()
        .map(|session| (session.pane_id.as_str(), session))
        .collect::<HashMap<_, _>>();
    current.iter().all(|session| {
        old.get(session.pane_id.as_str())
            .is_some_and(|prior| observable_session_equal(prior, session))
    })
}

fn observable_session_equal(left: &Session, right: &Session) -> bool {
    left.name == right.name
        && left.description == right.description
        && left.description_source == right.description_source
        && left.pane_id == right.pane_id
        && left.pane_identity == right.pane_identity
        && left.status == right.status
        && left.agent == right.agent
        && left.attached == right.attached
        && left.path == right.path
        && left.command == right.command
        && left.launch_command == right.launch_command
        && left.systemd_scope == right.systemd_scope
        && left.memory_max_bytes == right.memory_max_bytes
        && left.windows == right.windows
        && left.window_index == right.window_index
        && left.pane_index == right.pane_index
        && (left.content_hash == right.content_hash
            || observable_content_equal(&left.content, &right.content))
}

fn observable_summaries_equal(previous: &[SessionSummary], current: &[SessionSummary]) -> bool {
    if previous.len() != current.len() {
        return false;
    }
    let old = previous
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    current.iter().all(|session| {
        old.get(session.id.as_str())
            .is_some_and(|prior| observable_summary_equal(prior, session))
    })
}

fn observable_summary_equal(left: &SessionSummary, right: &SessionSummary) -> bool {
    left.id == right.id
        && left.instance_id == right.instance_id
        && left.machine == right.machine
        && left.name == right.name
        && left.description == right.description
        && left.pane_id == right.pane_id
        && left.status == right.status
        && left.agent == right.agent
        && left.attached == right.attached
        && left.path == right.path
        && left.command == right.command
        && left.launch_command == right.launch_command
        && left.systemd_scope == right.systemd_scope
        && left.memory_max_bytes == right.memory_max_bytes
        && left.windows == right.windows
        && left.window_index == right.window_index
        && left.pane_index == right.pane_index
        && left.content_hash == right.content_hash
}

fn observable_content_equal(left: &str, right: &str) -> bool {
    let mut left_lines = left.split_inclusive('\n');
    let mut right_lines = right.split_inclusive('\n');
    loop {
        match (left_lines.next(), right_lines.next()) {
            (Some(left), Some(right)) if observable_line_equal(left, right) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn observable_line_equal(left: &str, right: &str) -> bool {
    left == right || normalize_observable_line(left) == normalize_observable_line(right)
}

fn is_spinner_frame(character: char) -> bool {
    ('\u{2801}'..='\u{28ff}').contains(&character)
        || matches!(character, '✢' | '✳' | '✶' | '✻' | '✽' | '✷')
}

fn observable_content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    for line in content.split_inclusive('\n') {
        normalize_observable_line(line).hash(&mut hasher);
    }
    hasher.finish()
}

fn normalize_observable_line(line: &str) -> String {
    let mut normalized = line.to_owned();
    let trimmed = normalized.trim_start();
    if trimmed.starts_with("• Working (")
        && let Some(start) = normalized.find("Working (")
    {
        let elapsed_start = start + "Working (".len();
        if let Some(end) = normalized[elapsed_start..].find(" • esc to interrupt") {
            normalized.replace_range(elapsed_start..elapsed_start + end, "elapsed");
        }
    }
    if let Some((offset, spinner)) = normalized
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        && is_spinner_frame(spinner)
    {
        normalized.replace_range(offset..offset + spinner.len_utf8(), "\u{2800}");
    }
    normalized
}

pub(crate) fn overview_patch(previous: &Overview, current: &Overview) -> OverviewPatch {
    let old = previous
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let new = current
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    OverviewPatch {
        base_revision: previous.revision,
        revision: current.revision,
        upsert: current
            .sessions
            .iter()
            .filter(|session| {
                old.get(session.id.as_str())
                    .is_none_or(|prior| !observable_summary_equal(prior, session))
            })
            .cloned()
            .collect(),
        remove: previous
            .sessions
            .iter()
            .filter(|session| !new.contains_key(session.id.as_str()))
            .map(|session| session.id.clone())
            .collect(),
        health: current.health.clone(),
        machines: current.machines.clone(),
    }
}

pub(crate) fn pane_patch(
    previous: &PaneOutput,
    current: &PaneOutput,
    old_content: &str,
    new_content: &str,
) -> PanePatch {
    let old = content_lines(old_content);
    let new = content_lines(new_content);
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    PanePatch {
        base_revision: previous.revision,
        revision: current.revision,
        pane_id: current.pane_id.clone(),
        content_hash: current.content_hash.clone(),
        start_line: prefix,
        delete_lines: old.len().saturating_sub(prefix + suffix),
        lines: new[prefix..new.len().saturating_sub(suffix)]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
    }
}

async fn wait_for_revision_receiver(
    mut receiver: watch::Receiver<u64>,
    after: u64,
    timeout: Duration,
) -> bool {
    if *receiver.borrow() > after {
        return true;
    }
    tokio::time::timeout(timeout, async move {
        loop {
            if receiver.changed().await.is_err() {
                return false;
            }
            if *receiver.borrow() > after {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

fn profile_by_id(config: &Config, id: &str) -> Result<AgentProfile> {
    let index = id
        .strip_prefix("profile-")
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| bad_request("invalid profile id"))?;
    config
        .profiles
        .get(index)
        .cloned()
        .ok_or_else(|| bad_request("profile no longer exists"))
}

fn validate_launch_memory(
    resources: &crate::config::AgentResourcesConfig,
    requested_memory_max_bytes: Option<u64>,
) -> Result<()> {
    systemd_scope::resolve_memory_max(resources, requested_memory_max_bytes)
        .map(|_| ())
        .map_err(|error| bad_request(error.to_string()))
}

/// Never sends an additive memory field to the legacy launch route. An owner
/// downgraded after capability discovery could otherwise ignore that field and
/// still launch. Old owners do not implement the versioned route, so a stale
/// capability fails before launch and there is deliberately no fallback.
fn remote_launch_path(request: &LaunchRequest) -> &'static str {
    if request.memory_max_bytes.is_some() {
        "/api/v1/memory-launches/v1"
    } else {
        "/api/v1/sessions"
    }
}

fn agent_memory_launch_options(
    resources: &crate::config::AgentResourcesConfig,
) -> AgentMemoryLaunchOptions {
    let default_bytes = resources.memory_max_bytes;
    let (override_max_bytes, override_check_failed) =
        match systemd_scope::advertised_override_ceiling(resources) {
            Ok(ceiling) => (ceiling, false),
            Err(_) => (None, resources.memory_override_max_bytes.is_some()),
        };
    let supported = cfg!(target_os = "linux") && default_bytes.is_some();
    let mut presets = BTreeSet::new();
    if let Some(ceiling) = override_max_bytes {
        for gib in [2_u64, 4, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            let bytes = gib.saturating_mul(systemd_scope::GIBIBYTE);
            if bytes <= ceiling {
                presets.insert(bytes);
            }
        }
        if let Some(default) = default_bytes
            && default <= ceiling
        {
            presets.insert(default);
        }
    }
    let note = if !cfg!(target_os = "linux") {
        Some(
            "Per-agent memory limits require Linux and apply on the next launch or relaunch."
                .to_owned(),
        )
    } else if override_check_failed {
        Some("Per-launch memory overrides are unavailable because the effective host/cgroup ceiling could not be verified.".to_owned())
    } else if default_bytes.is_none() {
        Some("Per-agent memory limits are not enabled on this machine.".to_owned())
    } else if override_max_bytes.is_none() {
        Some(
            "This machine enforces its default cap; per-launch overrides are not enabled."
                .to_owned(),
        )
    } else {
        Some("Changes apply on the next launch or relaunch; running cgroups are never mutated in place.".to_owned())
    };
    AgentMemoryLaunchOptions {
        supported,
        default_bytes,
        override_max_bytes,
        presets_bytes: presets.into_iter().collect(),
        note,
    }
}

fn opaque_resume_id(
    hasher: &RandomState,
    profile_id: &str,
    directory: &str,
    candidate: &ResumeCandidate,
) -> String {
    let harness = candidate.harness().as_str();
    let session_id = candidate.session_id();
    let first = hasher.hash_one((0_u8, profile_id, directory, harness, session_id));
    let second = hasher.hash_one((1_u8, profile_id, directory, harness, session_id));
    format!("saved-{first:016x}{second:016x}")
}

fn persistent_resume_lease(
    profile: &AgentProfile,
    directory: &Path,
    candidate: &ResumeCandidate,
) -> String {
    let mut digest = Sha256::new();
    let directory = directory.to_string_lossy();
    for component in [
        "atmux-resume-lease-v1",
        profile.name.as_str(),
        profile.harness.as_str(),
        directory.as_ref(),
        candidate.harness().as_str(),
        candidate.session_id(),
    ] {
        digest.update(component.len().to_be_bytes());
        digest.update(component.as_bytes());
    }
    let mut lease = String::with_capacity("lease-v1-".len() + 64);
    lease.push_str("lease-v1-");
    for byte in digest.finalize() {
        write!(&mut lease, "{byte:02x}").expect("writing to a String cannot fail");
    }
    lease
}

fn acquire_persistent_resume_lock(
    lease: &str,
) -> std::result::Result<File, ResumeLeaseAcquireError> {
    let euid = geteuid().as_raw();
    let base = if cfg!(target_os = "linux") {
        PathBuf::from(format!("/run/user/{euid}"))
    } else {
        std::env::temp_dir()
    };
    let runtime = if cfg!(target_os = "linux") {
        base.join("atmux")
    } else {
        base.join(format!("atmux-{euid}"))
    };
    acquire_persistent_resume_lock_at(lease, &base, &runtime, euid)
}

fn acquire_persistent_resume_lock_at(
    lease: &str,
    base: &Path,
    runtime: &Path,
    euid: u32,
) -> std::result::Result<File, ResumeLeaseAcquireError> {
    if lease.strip_prefix("lease-v1-").is_none_or(|digest| {
        digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    }) {
        return Err(ResumeLeaseAcquireError::Unsafe);
    }
    validate_resume_lock_base(base, euid)?;
    ensure_private_resume_lock_directory(runtime, euid)?;
    let directory = runtime.join("resume-leases");
    ensure_private_resume_lock_directory(&directory, euid)?;
    let descriptor = rustix::fs::open(
        directory.join(format!("{lease}.lock")),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|_| ResumeLeaseAcquireError::Unsafe)?;
    let lock = File::from(descriptor);
    let metadata = lock
        .metadata()
        .map_err(|_| ResumeLeaseAcquireError::Unsafe)?;
    if !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.mode() & 0o077 != 0
    {
        return Err(ResumeLeaseAcquireError::Unsafe);
    }
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            ResumeLeaseAcquireError::Busy
        } else {
            ResumeLeaseAcquireError::Unsafe
        }
    })?;
    Ok(lock)
}

fn validate_resume_lock_base(
    path: &Path,
    euid: u32,
) -> std::result::Result<(), ResumeLeaseAcquireError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ResumeLeaseAcquireError::Unsafe)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ResumeLeaseAcquireError::Unsafe);
    }
    let private = metadata.uid() == euid && metadata.mode().trailing_zeros() >= 6;
    let shared_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
    if !private && !shared_sticky {
        return Err(ResumeLeaseAcquireError::Unsafe);
    }
    Ok(())
}

fn ensure_private_resume_lock_directory(
    path: &Path,
    euid: u32,
) -> std::result::Result<(), ResumeLeaseAcquireError> {
    match rustix::fs::mkdir(path, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(_) => return Err(ResumeLeaseAcquireError::Unsafe),
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| ResumeLeaseAcquireError::Unsafe)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != euid
        || metadata.mode() & 0o077 != 0
    {
        return Err(ResumeLeaseAcquireError::Unsafe);
    }
    Ok(())
}

fn valid_opaque_resume_id(value: &str) -> bool {
    value.strip_prefix("saved-").is_some_and(|digest| {
        digest.len() == 32
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

fn resolve_opaque_resume(
    candidates: Vec<ResumeCandidate>,
    hasher: &RandomState,
    profile_id: &str,
    directory: &str,
    requested: &str,
) -> Option<ResumeCandidate> {
    candidates
        .into_iter()
        .find(|candidate| opaque_resume_id(hasher, profile_id, directory, candidate) == requested)
}

fn select_launch_mode(
    profile: &AgentProfile,
    requested: Option<&str>,
) -> Result<Option<ProfileMode>> {
    match (profile.modes.as_slice(), requested) {
        ([], None) => Ok(None),
        ([], Some(_)) => Err(bad_request(format!(
            "profile {} does not define launch modes",
            profile.name
        ))),
        ([mode], None) => Ok(Some(mode.clone())),
        (modes, Some(id)) => modes
            .iter()
            .find(|mode| mode.id == id)
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                bad_request(format!(
                    "profile {} does not define mode {id}",
                    profile.name
                ))
            }),
        (_, None) => Err(bad_request(format!(
            "profile {} has multiple modes; choose mode_id",
            profile.name
        ))),
    }
}

fn validate_session_name(name: &str) -> Result<()> {
    if name == RESERVED_SERVICE_SESSION {
        return Err(bad_request(
            "session name is reserved for the atmux web service",
        ));
    }
    if name.is_empty() || name.len() > 100 {
        return Err(bad_request("session name must contain 1 to 100 characters"));
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(bad_request(
            "session name may contain only letters, numbers, '-' and '_'",
        ));
    }
    Ok(())
}

/// Normalizes a session edit: the name follows launch rules, and the
/// description is trimmed, with an empty one meaning "clear it".
fn validate_session_update(
    request: &SessionUpdateRequest,
) -> Result<(Option<String>, DescriptionUpdate)> {
    if request.name.is_none() && request.description.is_none() {
        return Err(bad_request("provide a new session name or description"));
    }
    if let Some(name) = request.name.as_deref() {
        validate_session_name(name)?;
    }
    let description = match request.description.as_deref().map(str::trim) {
        None => DescriptionUpdate::Keep,
        Some("") => DescriptionUpdate::Clear,
        Some(text) if crate::tmux::valid_session_description(text) => {
            DescriptionUpdate::Set(text.to_owned())
        }
        Some(_) => {
            return Err(bad_request(format!(
                "session description must be one line of at most {} characters",
                crate::tmux::MAX_SESSION_DESCRIPTION_CHARS
            )));
        }
    };
    Ok((request.name.clone(), description))
}

fn tail_lines(content: &str, lines: usize) -> String {
    let all = content_lines(content);
    let tail = all[all.len().saturating_sub(lines)..].join("\n");
    bounded_tail(&tail, MAX_OUTPUT_BYTES).to_owned()
}

fn content_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        Vec::new()
    } else {
        content.split('\n').collect()
    }
}

fn bounded_tail(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }
    let mut start = content.len() - max_bytes;
    while !content.is_char_boundary(start) {
        start += 1;
    }
    if start > 0
        && !content[..start].ends_with('\n')
        && let Some(newline) = content[start..].find('\n')
    {
        start += newline + 1;
    }
    &content[start..]
}

fn truncate_front(content: &mut String, max_bytes: usize) {
    if content.len() <= max_bytes {
        return;
    }
    *content = bounded_tail(content, max_bytes).to_owned();
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

// Native resume uses the same owner registry, peer archive transport and event spool.
impl ControlPlane {
    #[allow(clippy::unused_self)]
    fn with_registry_tmux<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        #[cfg(test)]
        if let Some(socket) = self
            .inner
            .registry_test_socket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Tmux::with_socket_for_test(&socket, operation);
        }
        operation()
    }

    fn bind_registry_session(&self, session: &Session) -> Result<()> {
        let Some(key) = &session.session_key else {
            return Ok(());
        };
        let Some(profile) =
            profile_for_session(&self.inner.config.profiles, session.agent, &session.profile)
        else {
            return Ok(());
        };
        let provider = match session.agent {
            AgentKind::Claude => crate::old_sessions::ResumeHarness::Claude,
            AgentKind::Codex => crate::old_sessions::ResumeHarness::Codex,
            AgentKind::Other => return Ok(()),
        };
        // Native binding enriches the registry record but is optional: a profile
        // whose session store cannot be resolved (for example a wrapper command
        // without CLAUDE_CONFIG_DIR/CODEX_HOME) stays unbound instead of failing
        // the snapshot, close and resume paths for every other session.
        let root = match crate::old_sessions::profile_config_directory(profile, provider) {
            Ok(root) => root,
            Err(error) => {
                warn_unbound_registry_profile(&session.profile, &error);
                return Ok(());
            }
        };
        if let Some(native) = crate::transcript::native_resume_target_in_store(session, &root) {
            self.registry()?.bind_native(
                key,
                crate::registry::NativeIdentity {
                    config_root: root,
                    session_id: native.session_id,
                    log_path: native.log_path,
                },
                session.agent_pid.and_then(native_process_start_stamp),
            )?;
        }
        Ok(())
    }

    fn emit_registry_node_started(&self, registry: &crate::registry::Registry) -> Result<()> {
        if self.inner.events.is_some() {
            let boot = registry.owner_boot_id();
            let mut event = crate::events::AgentEvent::node_started(&self.inner.local_id)?;
            event.detail["boot_id"] = boot.clone().into();
            event.session_key.clone_from(&boot);
            event.instance_id = boot;
            self.emit_agent_event(event)?;
        }
        Ok(())
    }

    pub(crate) fn registry_owner_boot(&self) -> Option<String> {
        self.inner
            .registry
            .as_ref()
            .map(|registry| registry.owner_boot_id())
    }

    /// Takes a fresh owner snapshot using A3's checksummed archive format.
    pub(crate) async fn registry_export_record(
        &self,
        key: &str,
    ) -> Result<crate::registry::StoredRecord> {
        self.ensure_local_owner_enabled()?;
        if !crate::tmux::valid_session_key(key) {
            return Err(bad_request("invalid session key"));
        }
        let registry = self.registry()?;
        let control = self.clone();
        let key = key.to_owned();
        tokio::task::spawn_blocking(move || {
            control.with_registry_tmux(|| {
                let fresh = Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
                if registry.observe_owner(&fresh, now_ms(), crate::resume_anywhere::server_id())? {
                    control.emit_registry_node_started(&registry)?;
                }
                if let Some(session) = fresh
                    .iter()
                    .find(|session| session.session_key.as_deref() == Some(&key))
                {
                    control.bind_registry_session(session)?;
                }
                // Retained native files can contain turns newer than the last archive.
                // Missing native files use the already checksummed retained bundle.
                if let Some(native) = registry.native_identity(&key)? {
                    match fs::symlink_metadata(&native.log_path) {
                        Ok(_) => {
                            registry.capture_bundle(&key)?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let record = registry
                    .stored(&key)?
                    .ok_or_else(|| not_found("unknown saved session"))?;
                registry.bundle_file(&key)?;
                Ok(record)
            })
        })
        .await?
    }

    /// Imports and launches inside one cancellation-safe owner transaction.
    pub(crate) async fn registry_import(
        &self,
        file: std::fs::File,
        generation: u64,
        restoring: bool,
    ) -> Result<crate::resume_anywhere::ImportResponse> {
        self.ensure_local_owner_enabled()?;
        let registry = self.registry()?;
        let control = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            control.with_registry_tmux(|| {
                control.import_registry_blocking(&registry, file, generation, restoring)
            })
        })
        .await??;
        self.inner.refresh_now.notify_one();
        Ok(result)
    }

    #[allow(clippy::too_many_lines)] // The launch lease, verification and durable commit share a transaction.
    fn import_registry_blocking(
        &self,
        registry: &Arc<crate::registry::Registry>,
        file: std::fs::File,
        generation: u64,
        restoring: bool,
    ) -> Result<crate::resume_anywhere::ImportResponse> {
        use crate::resume_anywhere::{ImportResponse, ResumeResult};
        let control = self;
        let _transaction = registry.resume_transaction();
        let archive = crate::resume_anywhere::decode(registry, file)?;
        let key = archive.manifest.session.record.session_key.clone();
        let source_machine = archive.manifest.session.record.machine.clone();
        let native_id = archive
            .manifest
            .session
            .native
            .as_ref()
            .ok_or_else(|| conflict("missing native identity"))?
            .session_id
            .clone();
        if restoring && !registry.restore_desired(&key)? {
            return Err(conflict("session was intentionally closed before restore"));
        }
        if generation < archive.manifest.session.resume_generation {
            return Err(conflict("stale resume generation"));
        }
        let fresh = Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
        let prepared = crate::resume_anywhere::import(&control.inner.config, registry, archive)?;
        if let Some(existing) = fresh
            .iter()
            .find(|session| session.session_key.as_deref() == Some(&key))
        {
            if existing.agent_pid.is_none()
                || existing.profile != prepared.profile.name
                || existing.path != prepared.directory
                || crate::transcript::native_resume_target_in_store(
                    existing,
                    &prepared.native.config_root,
                )
                .is_none_or(|native| native.session_id != native_id)
            {
                return Err(conflict(
                    "target pane does not verify the imported conversation",
                ));
            }
            let record = registry
                .stored(&key)?
                .ok_or_else(|| conflict("target registry identity missing"))?;
            let generation = generation.max(record.resume_generation);
            registry.commit_resume(
                record,
                existing,
                prepared.native,
                generation,
                now_ms(),
                restoring,
            )?;
            registry.capture_bundle(&key)?;
            let record = registry
                .stored(&key)?
                .ok_or_else(|| conflict("target registry identity missing"))?;
            return Ok(ImportResponse {
                result: ResumeResult {
                    session_key: key,
                    machine: control.inner.local_id.clone(),
                    name: existing.name.clone(),
                    pane_id: existing.pane_id.clone(),
                    verified: true,
                },
                record,
            });
        }
        let lease =
            persistent_resume_lease(&prepared.profile, &prepared.directory, &prepared.candidate);
        let _lease_lock = acquire_persistent_resume_lock(&lease)
            .map_err(|_| conflict("native conversation launch already reserved"))?;
        if Tmux::resume_lease_active(&lease)? {
            return Err(conflict("native conversation already running on target"));
        }
        let desired_name = &prepared.stored.record.name;
        let name = if fresh.iter().any(|session| &session.name == desired_name) {
            format!(
                "{}-{}",
                desired_name.chars().take(60).collect::<String>(),
                &key[..8]
            )
        } else {
            desired_name.clone()
        };
        let scope = crate::systemd_scope::prepare(&control.inner.config.agent_resources, &name)?;
        Tmux::launch_imported(
            &name,
            &prepared.directory,
            &prepared.profile,
            prepared.mode.as_ref(),
            &prepared.candidate,
            &lease,
            &key,
            scope,
        )?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let launched = loop {
            let sessions = Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
            if let Some(session) = sessions.into_iter().find(|session| {
                session.session_key.as_deref() == Some(&key)
                    && session.name == name
                    && session.path == prepared.directory
                    && session.agent_pid.is_some()
                    && session
                        .agent
                        .to_string()
                        .eq_ignore_ascii_case(&prepared.profile.harness)
            }) {
                control.handle_startup_prompt(&session);
                if crate::transcript::native_resume_target_in_store(
                    &session,
                    &prepared.native.config_root,
                )
                .is_some_and(|native| native.session_id == native_id)
                {
                    break session;
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(conflict("target CLI did not verify; source left running"));
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        registry.commit_resume(
            prepared.stored,
            &launched,
            prepared.native,
            generation,
            now_ms(),
            restoring,
        )?;
        registry.capture_bundle(&key)?;
        let record = registry
            .stored(&key)?
            .ok_or_else(|| conflict("target registry identity missing"))?;
        if control.inner.events.is_some() {
            let mut event = crate::events::AgentEvent::from_session(
                &control.inner.local_id,
                &launched,
                "session.resumed",
                None,
            )?;
            event.detail = serde_json::json!({"from_machine":source_machine,"to_machine":control.inner.local_id,"resume_generation":generation});
            control.emit_agent_event(event)?;
        }
        Ok(ImportResponse {
            result: ResumeResult {
                session_key: key,
                machine: control.inner.local_id.clone(),
                name,
                pane_id: launched.pane_id,
                verified: true,
            },
            record,
        })
    }

    /// Restores only durable desired entries; never reopens intentional closes.
    /// # Errors
    /// Returns invalid-key, closed-session, archive or launch errors.
    pub async fn registry_restore(
        &self,
        request: crate::resume_anywhere::RestoreRequest,
    ) -> Result<Vec<crate::resume_anywhere::ResumeResult>> {
        self.ensure_local_owner_enabled()?;
        let registry = self.registry()?;
        if request.session_keys.len() > 512 {
            return Err(bad_request("restore request exceeds limit"));
        }
        let mut results = Vec::new();
        for key in request.session_keys.into_iter().collect::<BTreeSet<_>>() {
            if !registry.restore_desired(&key)? {
                return Err(conflict("session is not desired on this owner"));
            }
            let record = self.registry_export_record(&key).await?;
            let (file, _) = registry.bundle_file(&key)?;
            results.push(
                self.registry_import(file, record.resume_generation, true)
                    .await?
                    .result,
            );
        }
        Ok(results)
    }

    /// Resumes on a configured owner, leaving the source running by default.
    /// # Errors
    /// Returns errors for unknown owners, missing archives or unsafe imports.
    #[allow(clippy::too_many_lines)] // One source/target verification transaction.
    pub async fn session_resume(
        &self,
        request: crate::resume_anywhere::SessionResumeRequest,
    ) -> Result<crate::resume_anywhere::ResumeResult> {
        let registry = self.registry()?;
        if !crate::tmux::valid_session_key(&request.session_key)
            || !self.has_machine(&request.machine)
        {
            return Err(bad_request("invalid session key or target machine"));
        }
        let saved = registry
            .stored(&request.session_key)?
            .ok_or_else(|| not_found("unknown saved session"))?;
        let source_machine = self
            .overview()
            .sessions
            .iter()
            .find(|session| {
                session.session_key.as_deref() == Some(&request.session_key)
                    && session.machine != request.machine
            })
            .map_or_else(
                || saved.record.machine.clone(),
                |session| session.machine.clone(),
            );
        let source = if source_machine == self.inner.local_id {
            self.registry_export_record(&request.session_key).await?
        } else {
            let remote = self.remote_machine(&source_machine)?;
            match remote
                .get_json::<crate::registry::StoredRecord>(&format!(
                    "/api/v1/registry/{}/record",
                    request.session_key
                ))
                .await
            {
                Ok(record) => {
                    if record.record.session_key != request.session_key
                        || record.record.machine != source_machine
                    {
                        return Err(conflict("source returned a different session"));
                    }
                    let info = record
                        .record
                        .bundle
                        .as_ref()
                        .ok_or_else(|| conflict("source archive unavailable"))?;
                    let (name, file) = registry.incoming_file()?;
                    if let Err(error) = remote
                        .download_registry_bundle(
                            &format!("/api/v1/registry/{}/bundle", request.session_key),
                            file,
                            info.bytes,
                        )
                        .await
                    {
                        registry.discard_incoming(&name);
                        return Err(error);
                    }
                    if let Err(error) = registry.finish_incoming(&name, info) {
                        registry.discard_incoming(&name);
                        return Err(error);
                    }
                    registry.import_page(
                        &source_machine,
                        &crate::registry::RegistryPage {
                            cursor: String::new(),
                            reset: false,
                            more: false,
                            records: vec![record.clone()],
                        },
                    )?;
                    record
                }
                Err(error) => {
                    if request.move_source || crate::remote::rejected_status(&error).is_some() {
                        return Err(error);
                    }
                    registry.bundle_file(&request.session_key)?;
                    saved.clone()
                }
            }
        };
        let (file, _) = registry.bundle_file_info(
            source
                .record
                .bundle
                .as_ref()
                .ok_or_else(|| conflict("source archive missing"))?,
        )?;
        let proof = if request.move_source && source_machine != request.machine {
            Some(crate::resume_anywhere::source_proof(
                &crate::resume_anywhere::decode(&registry, file.try_clone()?)?.manifest,
            )?)
        } else {
            None
        };
        let generation = source
            .resume_generation
            .max(saved.resume_generation)
            .checked_add(1)
            .ok_or_else(|| conflict("resume generation exhausted"))?;
        let response = if request.machine == self.inner.local_id {
            self.registry_import(file, generation, false).await?
        } else {
            self.remote_machine(&request.machine)?
                .upload_registry_bundle(file, generation)
                .await?
        };
        if !response.result.verified
            || response.result.session_key != request.session_key
            || response.result.machine != request.machine
            || response.record.record.session_key != request.session_key
            || response.record.record.machine != request.machine
        {
            return Err(conflict(
                "target did not verify requested session; source left running",
            ));
        }
        if request.machine != self.inner.local_id {
            registry.import_page(
                &request.machine,
                &crate::registry::RegistryPage {
                    cursor: String::new(),
                    reset: false,
                    more: false,
                    records: vec![response.record],
                },
            )?;
        }
        if let Some(proof) = proof {
            self.registry_stop_source(proof, &source_machine).await?;
        }
        Ok(response.result)
    }

    /// Stops only the verified exported source generation, after target success.
    /// # Errors
    /// Refuses changed processes, advanced logs and failed owner requests.
    pub async fn registry_stop_source(
        &self,
        request: crate::resume_anywhere::StopSourceRequest,
        machine: &str,
    ) -> Result<()> {
        if machine != self.inner.local_id {
            return self
                .remote_machine(machine)?
                .post_json("/api/v1/registry/stop-source", &request)
                .await;
        }
        self.ensure_local_owner_enabled()?;
        let registry = self.registry()?;
        let control = self.clone();
        tokio::task::spawn_blocking(move || {
            control.with_registry_tmux(|| {
                let fresh = Tmux.sessions(&HashMap::new(), &control.inner.config.status)?;
                let session = fresh
                    .iter()
                    .find(|session| {
                        session.session_key.as_deref() == Some(&request.session_key)
                            && session.pane_identity == request.binding.instance_id
                    })
                    .ok_or_else(|| conflict("source generation changed"))?;
                let _lock = auto_update::PaneProcessLock::acquire(&session.pane_id)?;
                let live = Tmux::live_pane_identity(&session.pane_id)?
                    .ok_or_else(|| conflict("source disappeared"))?;
                if live.pane_identity != session.pane_identity
                    || live.pane_pid != session.pane_pid
                    || session.agent_pid != Some(request.binding.agent_pid)
                    || native_process_start_stamp(request.binding.agent_pid).as_deref()
                        != Some(&request.binding.process_start)
                {
                    return Err(conflict("source process generation changed"));
                }
                control.bind_registry_session(session)?;
                if registry
                    .native_identity(&request.session_key)?
                    .is_none_or(|native| native.session_id != request.native_id)
                {
                    return Err(conflict("source conversation changed"));
                }
                let (file, _) = registry.capture_bundle(&request.session_key)?;
                let archive = crate::resume_anywhere::decode(&registry, file)?;
                if crate::resume_anywhere::content_digest(&archive.manifest)
                    != request.binding.content_digest
                {
                    return Err(conflict(
                        "source advanced after export; source left running",
                    ));
                }
                registry.record_close(std::slice::from_ref(&request.session_key), now_ms())?;
                Tmux::output(["kill-pane", "-t", &session.pane_id]).map(|_| ())
            })
        })
        .await??;
        self.inner.refresh_now.notify_one();
        Ok(())
    }

    async fn reconcile_restore_owner(&self, remote: &Arc<RemoteMachine>) -> Result<()> {
        if !self.inner.config.registry.restore_on_start
            || !self
                .inner
                .config
                .registry
                .restore_machines
                .contains(&remote.id)
        {
            return Ok(());
        }
        let (Some(events), Some(_registry)) = (&self.inner.events, &self.inner.registry) else {
            return Ok(());
        };
        let Some(boot) = events.latest_node_boot(&remote.id) else {
            return Ok(());
        };
        // A3's primary location can be another still-running copy. Read the
        // signaled owner's filtered A3 feed, so its durable close intent wins.
        let mut keys = BTreeSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..32 {
            let mut path = format!("/api/v1/registry?restore_boot={boot}&wait_ms=0");
            if let Some(after) = &cursor {
                path.push_str("&after=");
                path.push_str(after);
            }
            let page: crate::registry::RegistryPage = remote.get_json(&path).await?;
            if page.records.len() > 16 {
                return Err(conflict("oversized owner restore page"));
            }
            for mut stored in page.records {
                stored.validate()?;
                if stored.record.machine != remote.id
                    || stored.desired_running != Some(true)
                    || stored.close_reason.as_deref() != Some("node_loss")
                    || stored.owner_boot_id.as_deref() == Some(&boot)
                {
                    return Err(conflict("invalid owner restore record"));
                }
                keys.insert(stored.record.session_key);
            }
            if !page.more || keys.len() >= 512 {
                break;
            }
            if cursor.as_ref() == Some(&page.cursor)
                || page.cursor.len() > 100
                || !page
                    .cursor
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b'-' | b':'))
            {
                return Err(conflict("invalid owner restore cursor"));
            }
            cursor = Some(page.cursor);
        }
        if !keys.is_empty() {
            let _: Vec<crate::resume_anywhere::ResumeResult> = remote
                .post_json_response_with_timeout(
                    "/api/v1/registry/restore",
                    &crate::resume_anywhere::RestoreRequest {
                        session_keys: keys.into_iter().collect(),
                    },
                    Duration::from_secs(630),
                )
                .await?;
        }
        Ok(())
    }

    fn spawn_restore_reconciler(&self) {
        if !self.inner.config.registry.restore_on_start {
            return;
        }
        let control = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                for remote in control.remote_machines() {
                    if let Err(error) = control.reconcile_restore_owner(&remote).await {
                        eprintln!("atmux registry restore {}: {error}", remote.id);
                    }
                }
            }
        });
    }

    fn handle_startup_prompt(&self, session: &Session) -> bool {
        crate::startup_prompts::handle_with_events(
            &self.inner.config,
            session,
            self.inner.events.as_deref(),
        )
    }
}

#[cfg(test)]
pub(crate) fn test_control(machines: &[&str]) -> ControlPlane {
    test_control_with_config(machines, Config::default())
}

#[cfg(test)]
#[allow(clippy::too_many_lines)] // One literal mirrors every Inner field.
pub(crate) fn test_control_with_config(machines: &[&str], config: Config) -> ControlPlane {
    let local_id = config.node.id.clone();
    let local_label = config.node_label();
    let remotes = machines
        .iter()
        .map(|id| {
            (
                (*id).to_owned(),
                RemoteState {
                    online: false,
                    health: Some("connecting".to_owned()),
                    ..RemoteState::default()
                },
            )
        })
        .collect();
    let handles: BTreeMap<String, Arc<RemoteMachine>> = machines
        .iter()
        .map(|id| {
            let definition = config
                .machines
                .iter()
                .find(|m| m.id == *id)
                .cloned()
                .unwrap_or_else(|| crate::config::MachineConfig {
                    id: (*id).into(),
                    label: Some(format!("{id} label")),
                    url: format!("http://{id}.invalid:7345"),
                    token_env: None,
                    token_file: None,
                });
            (
                (*id).to_owned(),
                Arc::new(RemoteMachine::from_config(&definition).unwrap()),
            )
        })
        .collect();
    let summarizer = crate::summarizer::Summarizer::configured(&config).unwrap();
    let (events, registry, registry_peer_token) = open_node_stores(
        &config,
        !machines.is_empty() || config.node.coordinator_only || config.discovery.enabled,
    )
    .unwrap();
    let (revisions, _) = watch::channel(0);
    let control = ControlPlane {
        inner: Arc::new(Inner {
            registry_test_socket: Mutex::new(None),
            events,
            config,
            summarizer,
            registry,
            registry_peer_token,
            registry_watchers: Mutex::new(BTreeMap::new()),
            local_id: local_id.clone(),
            local_label,
            bare_local_ids: handles.is_empty(),
            configured_machine_ids: handles.keys().cloned().collect(),
            machines: RwLock::new(handles),
            watchers: std::sync::Mutex::new(BTreeMap::new()),
            state: RwLock::new(State {
                revision: 0,
                sessions: Vec::new(),
                health: None,
                metrics: MachineMetrics::default(),
                remotes,
                machine_revisions: BTreeMap::new(),
            }),
            hardware: std::sync::Mutex::new(HardwareSampler::default()),
            outputs: RwLock::new(HashMap::new()),
            fetches: RwLock::new(HashMap::new()),
            prompt_locks: Mutex::new(HashMap::new()),
            resume_ids: RandomState::new(),
            resume_leases: Mutex::new(ResumeLeaseState::default()),
            revisions,
            refresh_now: Notify::new(),
            recovery: RecoveryRunner::detached(&local_id),
            updater: SelfUpdater::with_environment(
                &crate::self_update::SelfUpdateConfig::default(),
                self_update::Environment {
                    exe: std::path::PathBuf::from("/nonexistent/atmux"),
                    state_dir: std::path::PathBuf::from("/nonexistent/state"),
                    launch: self_update::Launch::capture(),
                    container: false,
                    restart: false,
                },
            )
            .expect("the default self-update policy must be valid"),
            deny_local_agent_restart: true,
            local_agent_restart_attempts: AtomicU64::new(0),
            test_message_live_instances: Mutex::new(HashMap::new()),
        }),
    };
    crate::registry_integration::install(&control, control.inner.events.is_some()).unwrap();
    control
}

/// Builds a `Session` fixture for tests in this crate.
#[cfg(test)]
pub(crate) fn test_session(name: &str, pane_id: &str, content: &str) -> Session {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Session {
        name: name.to_owned(),
        description: None,
        description_source: None,
        attached: false,
        windows: 1,
        activity: 1,
        output_activity: 1,
        window_index: 0,
        pane_index: 0,
        pane_id: pane_id.to_owned(),
        pane_pid: 0,
        pane_identity: String::new(),
        agent_pid: Some(100),
        agent_started_ms: None,
        path: Path::new("/tmp").to_path_buf(),
        command: "codex".to_owned(),
        launch_command: "codex".to_owned(),
        title: name.to_owned(),
        content: content.to_owned(),
        content_hash: hasher.finish(),
        agent: crate::status::AgentKind::Codex,
        profile: "Default".to_owned(),
        resume_lease: None,
        session_key: None,
        systemd_scope: None,
        memory_max_bytes: None,
        status: crate::status::AgentStatus::Working,
    }
}

// A3 history/federation integration. Keep these independent of live pane routing.
impl ControlPlane {
    /// # Errors
    /// Returns `NotFound` while the explicitly configured registry is disabled.
    pub fn registry(&self) -> Result<Arc<crate::registry::Registry>> {
        self.inner
            .registry
            .clone()
            .ok_or_else(|| not_found("session registry is disabled"))
    }

    pub(crate) fn registry_peer_authorized(&self, credential: &str) -> bool {
        self.inner
            .registry_peer_token
            .as_ref()
            .is_some_and(|token| {
                let expected = token.expose().as_bytes();
                let supplied = credential.as_bytes();
                expected.len() == supplied.len()
                    && expected
                        .iter()
                        .zip(supplied)
                        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                        == 0
            })
    }

    /// # Errors
    /// Returns disabled/invalid-filter errors; native identity is never returned.
    pub fn sessions_search(
        &self,
        query: &crate::registry::SessionsSearch,
    ) -> Result<crate::registry::SessionsPage> {
        self.registry()?
            .search(query)
            .map_err(|_| bad_request("invalid session history filters"))
    }

    /// # Errors
    /// Returns disabled/invalid-key errors; native identity is never returned.
    pub fn session_get(&self, key: &str) -> Result<Option<crate::registry::SessionRecord>> {
        self.registry()?
            .get(key)
            .map_err(|_| bad_request("invalid session key"))
    }
}

impl ControlPlane {
    /// Read-only owner-scoped lookup, bounded by project roots and scan limits.
    /// # Errors
    /// Rejects invalid remotes, unavailable owners, or incomplete scans.
    pub async fn find_launch_repository(
        &self,
        machine: &str,
        repo: &str,
    ) -> Result<Option<String>> {
        if machine != self.local_id() {
            let remote = self.remote_machine(machine)?;
            self.ensure_online(machine)?;
            return remote
                .get_json(&format!(
                    "/api/v1/launch-repository?remote={}",
                    encode_segment(repo)
                ))
                .await
                .map_err(|e| upstream(&e));
        }
        self.ensure_local_owner_enabled()?;
        let config = self.inner.config.clone();
        let repo = repo.to_owned();
        tokio::task::spawn_blocking(move || crate::intake::pool_lookup(&config, &repo)).await?
    }
    /// Local mirror of the channel ledger for the Work dashboard; no platform request.
    /// # Errors
    /// Rejects corrupted or oversized persisted state.
    pub fn intake_work(&self) -> Result<crate::intake::WorkView> {
        crate::intake::work(&self.inner.config.intake)
    }
}

#[cfg(test)]
mod resume_federation_tests {
    use super::*;
    use crate::resume_anywhere::{RestoreRequest, SessionResumeRequest};
    use axum::{
        Router,
        extract::Request,
        http::{StatusCode, header},
        middleware::{self, Next},
        response::Response,
    };
    use std::{io::Write as _, os::unix::fs::PermissionsExt, process::Command};

    struct DisposableOwner {
        root: PathBuf,
        socket: String,
    }
    impl DisposableOwner {
        fn new(id: &str) -> Self {
            let key = crate::tmux::new_session_key().unwrap();
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("atmux-a4-owner-{id}-{key}"));
            fs::create_dir_all(&root).unwrap();
            Self {
                root,
                socket: format!("atmux-test-a4-{id}-{key}"),
            }
        }
        fn control(&self, id: &str, home: &str) -> ControlPlane {
            let project = self.root.join(home).join("IdeaProjects/atmux");
            let native = self.root.join(home).join("source-native");
            fs::create_dir_all(&project).unwrap();
            fs::create_dir_all(&native).unwrap();
            for args in [
                vec!["init", "-q", "-b", "main"],
                vec![
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/ryanmurf/atmux.git",
                ],
            ] {
                assert!(
                    Command::new("git")
                        .arg("-C")
                        .arg(&project)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
            let script = self.root.join("claude");
            fs::write(&script, "#!/bin/bash\nif [ \"${ATMUX_FIXTURE_REEXEC:-}\" != yes ]; then export ATMUX_FIXTURE_REEXEC=yes; exec -a claude /bin/bash \"$0\" \"$@\"; fi\nprintf '%s\\n' \"$PWD\" \"$@\" > \"$CLAUDE_CONFIG_DIR/received\"\nmkdir -p \"$CLAUDE_CONFIG_DIR/sessions\"\nrecorded_ms=$(($(date +%s)*1000))\nprintf '{\"pid\":%s,\"cwd\":\"%s\",\"startedAt\":%s,\"sessionId\":\"019a06d9-8341-7654-8abc-0123456789ab\"}' \"$$\" \"$PWD\" \"$recorded_ms\" > \"$CLAUDE_CONFIG_DIR/sessions/$$.json\"\nwhile true; do sleep 1; done\n").unwrap();
            let mut config = Config::default();
            config.node.id = id.to_owned();
            config.general.project_roots = vec![project.parent().unwrap().to_owned()];
            config.general.favorite_dirs.clear();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            config.profiles = vec![AgentProfile {
                name: "hd".to_owned(),
                harness: "claude".to_owned(),
                command: script.to_string_lossy().into_owned(),
                args: Vec::new(),
                env: BTreeMap::from([(
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    native.to_string_lossy().into_owned(),
                )]),
                inherit_discovered: false,
                claude_relaunch_permissions: None,
                modes: Vec::new(),
            }];
            config.registry = crate::registry::RegistryConfig {
                enabled: true,
                directory: Some(self.root.join("registry")),
                ..crate::registry::RegistryConfig::default()
            };
            let token = self.root.join("token");
            fs::write(&token, "a4-fixture-token").unwrap();
            fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
            config.node.token_file = Some(token);
            config.events = Some(crate::events::EventsConfig {
                directory: Some(self.root.join("events")),
                inject_hooks: false,
                ..crate::events::EventsConfig::default()
            });
            let control = test_control_with_config(&[], config);
            *control.inner.registry_test_socket.lock().unwrap() = Some(self.socket.clone());
            control
        }
    }
    impl Drop for DisposableOwner {
        fn drop(&mut self) {
            assert!(self.socket.starts_with("atmux-test-"));
            let _ = Command::new("tmux")
                .args(["-L", &self.socket, "kill-server"])
                .output();
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn an_unresolvable_profile_store_leaves_the_session_unbound_instead_of_failing() {
        let key = crate::tmux::new_session_key().unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("atmux-unbound-profile-{key}"));
        fs::create_dir_all(&root).unwrap();
        let mut config = Config::default();
        config.node.id = "fixture".to_owned();
        config.general.favorite_dirs.clear();
        // A wrapper launcher without CLAUDE_CONFIG_DIR has no resolvable store.
        config.profiles = vec![AgentProfile {
            name: "wrapped".to_owned(),
            harness: "claude".to_owned(),
            command: root.join("claude-wrapper").to_string_lossy().into_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        }];
        config.registry = crate::registry::RegistryConfig {
            enabled: true,
            directory: Some(root.join("registry")),
            ..crate::registry::RegistryConfig::default()
        };
        let token = root.join("token");
        fs::write(&token, "unbound-fixture-token").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        config.node.token_file = Some(token);
        let control = test_control_with_config(&[], config);
        let mut session = super::tests::session("ready");
        session.agent = AgentKind::Claude;
        session.profile = "wrapped".to_owned();
        session.session_key = Some(key);

        control.bind_registry_session(&session).unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    async fn authenticated_fixture(control: ControlPlane) -> (String, tokio::task::JoinHandle<()>) {
        async fn auth(request: Request, next: Next) -> Response {
            if request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                != Some("Bearer a4-fixture-token")
            {
                return axum::response::IntoResponse::into_response(StatusCode::UNAUTHORIZED);
            }
            next.run(request).await
        }
        let (_, shutdown) = watch::channel(false);
        let app: Router =
            crate::web::api_router(control, vec!["http://fixture".to_owned()], shutdown)
                .layer(middleware::from_fn(auth));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }
    fn sync_owner(control: &ControlPlane) -> Result<()> {
        control.with_registry_tmux(|| control.refresh_blocking())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn federation_exports_imports_a3_archives_in_another_home_and_restores_from_node_events()
    -> Result<()> {
        let source_fixture = DisposableOwner::new("tron");
        let target_fixture = DisposableOwner::new("mac");
        let source = source_fixture.control("tron", "home/ryan");
        let target = target_fixture.control("mac", "Users/ryan");
        let cwd = source.inner.config.general.project_roots[0].join("atmux");
        let key = crate::tmux::new_session_key()?;
        let file = crate::resume_anywhere::tests::seed(
            &source.registry()?,
            &source_fixture.root.join("home/ryan"),
            &cwd,
            &key,
            crate::old_sessions::ResumeHarness::Claude,
        )?;
        source.registry_import(file, 1, false).await?;
        sync_owner(&source)?;
        let record = source.registry_export_record(&key).await?;
        let native = record.native.as_ref().unwrap().log_path.clone();
        let original = fs::read(&native)?;
        let (file, _) = source.registry()?.bundle_file(&key)?;
        let proof = crate::resume_anywhere::source_proof(
            &crate::resume_anywhere::decode(&source.registry()?, file)?.manifest,
        )?;
        let mut stale = proof.clone();
        stale.binding.agent_pid += 1;
        assert!(source.registry_stop_source(stale, "tron").await.is_err());
        let (source_url, source_server) = authenticated_fixture(source.clone()).await;
        let (target_url, target_server) = authenticated_fixture(target.clone()).await;
        let coordinator_fixture = DisposableOwner::new("home");
        let mut config = Config::default();
        config.node.id = "home".into();
        config.node.coordinator_only = true;
        config.registry.enabled = true;
        config.registry.directory = Some(coordinator_fixture.root.join("registry"));
        config.registry.restore_on_start = true;
        config.registry.restore_machines = vec!["tron".into(), "mac".into()];
        config.events = Some(crate::events::EventsConfig {
            directory: Some(coordinator_fixture.root.join("events")),
            inject_hooks: false,
            ..crate::events::EventsConfig::default()
        });
        let coordinator = test_control_with_config(&["tron", "mac"], config);
        for (id, url) in [("tron", source_url), ("mac", target_url)] {
            let remote = RemoteMachine::from_config(&crate::config::MachineConfig {
                id: id.into(),
                label: None,
                url,
                token_env: None,
                token_file: source.inner.config.node.token_file.clone(),
            })?;
            coordinator
                .inner
                .machines
                .write()
                .unwrap()
                .insert(id.into(), Arc::new(remote));
        }
        coordinator.registry()?.import_page(
            "tron",
            &crate::registry::RegistryPage {
                cursor: String::new(),
                reset: false,
                more: false,
                records: vec![record],
            },
        )?;
        coordinator.apply_machine_sessions("tron", source.overview().sessions, None);
        coordinator.apply_machine_sessions("mac", Vec::new(), None);
        let result = coordinator
            .session_resume(SessionResumeRequest {
                session_key: key.clone(),
                machine: "mac".into(),
                move_source: false,
            })
            .await?;
        assert!(result.verified);
        sync_owner(&target)?;
        let received = fs::read_to_string(
            PathBuf::from(&target.inner.config.profiles[0].env["CLAUDE_CONFIG_DIR"])
                .join("received"),
        )?;
        assert!(received.contains("Users/ryan/IdeaProjects/atmux"));
        assert!(received.contains("--resume\n019a06d9-8341-7654-8abc-0123456789ab\n"));
        assert!(received.contains("--model\nopus\n--effort\nhigh\n"));
        coordinator.apply_machine_sessions("mac", target.overview().sessions, None);
        // The primary moved to mac, but a source copy still has durable intent.
        // A3's owner feed must restore that copy without changing the primary.
        source.with_registry_tmux(|| Tmux::output(["kill-server"]).map(|_| ()))?;
        sync_owner(&source)?;
        let source_events = source
            .inner
            .events
            .as_ref()
            .unwrap()
            .owner
            .read(&crate::events::EventQuery::default())
            .await?;
        coordinator
            .inner
            .events
            .as_ref()
            .unwrap()
            .import_page("tron", &source_events)?;
        assert!(
            coordinator
                .registry()?
                .restore_keys("tron", &source.registry()?.owner_boot_id())
                .is_empty()
        );
        coordinator
            .reconcile_restore_owner(&coordinator.remote_machine("tron")?)
            .await?;
        sync_owner(&source)?;
        assert_eq!(
            source.overview().sessions[0].session_key.as_deref(),
            Some(key.as_str())
        );
        assert_eq!(coordinator.registry()?.get(&key)?.unwrap().machine, "mac");
        coordinator.apply_machine_sessions("tron", source.overview().sessions, None);
        source.registry_export_record(&key).await?;
        let (file, _) = source.registry()?.bundle_file(&key)?;
        let proof = crate::resume_anywhere::source_proof(
            &crate::resume_anywhere::decode(&source.registry()?, file)?.manifest,
        )?;
        fs::OpenOptions::new()
            .append(true)
            .open(&native)?
            .write_all(b"{\"type\":\"assistant\",\"message\":\"new source turn\"}\n")?;
        assert!(source.registry_stop_source(proof, "tron").await.is_err());
        assert!(
            coordinator
                .session_resume(SessionResumeRequest {
                    session_key: key.clone(),
                    machine: "mac".into(),
                    move_source: true
                })
                .await
                .is_err()
        );
        fs::write(&native, original)?;
        let source_pane = source.overview().sessions[0].pane_id.clone();
        let neighbor = source.with_registry_tmux(|| {
            Tmux::output([
                "split-window",
                "-d",
                "-t",
                &source_pane,
                "-P",
                "-F",
                "#{pane_id}",
                "sleep 30",
            ])
        })?;
        let repeated = coordinator
            .session_resume(SessionResumeRequest {
                session_key: key.clone(),
                machine: "mac".into(),
                move_source: true,
            })
            .await?;
        assert_eq!(repeated.pane_id, result.pane_id);
        assert_eq!(
            source.with_registry_tmux(|| Tmux::output([
                "display-message",
                "-p",
                "-t",
                neighbor.trim(),
                "#{pane_id}"
            ]))?,
            neighbor.trim()
        );
        assert!(
            source
                .with_registry_tmux(|| Tmux.sessions(&HashMap::new(), &source.inner.config.status))?
                .iter()
                .all(|session| session.session_key.as_deref() != Some(&key))
        );
        let target_native = target.registry()?.native_identity(&key)?.unwrap().log_path;
        fs::OpenOptions::new()
            .append(true)
            .open(&target_native)?
            .write_all(b"{\"type\":\"assistant\",\"message\":\"turn after archive\"}\n")?;
        target.with_registry_tmux(|| Tmux::output(["kill-server"]).map(|_| ()))?;
        sync_owner(&target)?;
        assert!(target.registry()?.restore_desired(&key)?);
        target
            .registry_restore(RestoreRequest {
                session_keys: vec![key.clone()],
            })
            .await?;
        sync_owner(&target)?;
        assert!(fs::read_to_string(&target_native)?.contains("turn after archive"));
        target.with_registry_tmux(|| Tmux::output(["kill-server"]).map(|_| ()))?;
        sync_owner(&target)?;
        let remote = coordinator.remote_machine("mac")?;
        crate::registry::pull_once(&coordinator.registry()?, &remote, None).await?;
        let events = target
            .inner
            .events
            .as_ref()
            .unwrap()
            .owner
            .read(&crate::events::EventQuery::default())
            .await?;
        coordinator
            .inner
            .events
            .as_ref()
            .unwrap()
            .import_page("mac", &events)?;
        assert!(
            !coordinator
                .registry()?
                .restore_keys("mac", &target.registry()?.owner_boot_id())
                .is_empty()
        );
        coordinator.reconcile_restore_owner(&remote).await?;
        sync_owner(&target)?;
        assert_eq!(
            target.overview().sessions[0].session_key.as_deref(),
            Some(key.as_str())
        );
        let target_pane = target.overview().sessions[0].pane_id.clone();
        target.kill(&composite_id("mac", &target_pane)).await?;
        assert!(!target.registry()?.restore_desired(&key)?);
        assert!(
            target
                .registry_restore(RestoreRequest {
                    session_keys: vec![key]
                })
                .await
                .is_err()
        );
        source_server.abort();
        target_server.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        machine::LOCAL_MACHINE_ID,
        status::{AgentKind, AgentStatus},
    };

    fn summary(id: &str, status: &str) -> SessionSummary {
        SessionSummary {
            id: composite_id(LOCAL_MACHINE_ID, id),
            instance_id: String::new(),
            session_key: None,
            machine: LOCAL_MACHINE_ID.to_owned(),
            name: format!("session-{id}"),
            description: None,
            description_source: None,
            pane_id: id.to_owned(),
            status: status.to_owned(),
            agent: "codex".to_owned(),
            profile: "Default".to_owned(),
            attached: false,
            activity: 1,
            path: "/tmp".to_owned(),
            title: "title".to_owned(),
            command: "codex".to_owned(),
            launch_command: "codex".to_owned(),
            systemd_scope: None,
            memory_max_bytes: None,
            windows: 1,
            window_index: 0,
            pane_index: 0,
            content_hash: "0000000000000001".to_owned(),
        }
    }

    fn overview(revision: u64, sessions: Vec<SessionSummary>, health: Option<&str>) -> Overview {
        Overview {
            revision,
            sessions,
            health: health.map(str::to_owned),
            machines: Vec::new(),
        }
    }

    fn pane(revision: u64, content: &str) -> PaneOutput {
        PaneOutput {
            revision,
            pane_id: "%1".to_owned(),
            session: "test".to_owned(),
            content_hash: revision.to_string(),
            content: Some(content.to_owned()),
            changed: true,
        }
    }

    fn apply_pane_patch(content: &str, patch: &PanePatch) -> String {
        let mut lines = content_lines(content)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.splice(
            patch.start_line..patch.start_line + patch.delete_lines,
            patch.lines.clone(),
        );
        lines.join("\n")
    }

    pub(super) fn session(content: &str) -> Session {
        Session {
            name: "agent".to_owned(),
            description: None,
            description_source: None,
            attached: false,
            windows: 1,
            activity: 1,
            output_activity: 1,
            window_index: 0,
            pane_index: 0,
            pane_id: "%1".to_owned(),
            pane_pid: 100,
            pane_identity: format!("pane-v1-{}", "a".repeat(64)),
            agent_pid: Some(100),
            agent_started_ms: None,
            path: Path::new("/tmp").to_path_buf(),
            command: "codex".to_owned(),
            launch_command: "codex".to_owned(),
            title: "agent".to_owned(),
            content: content.to_owned(),
            content_hash: content_hash(content),
            agent: AgentKind::Codex,
            profile: "Default".to_owned(),
            resume_lease: None,
            session_key: None,
            systemd_scope: None,
            memory_max_bytes: None,
            status: AgentStatus::Working,
        }
    }

    fn content_hash(value: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn scope_metadata_is_observable_and_reported_without_breaking_old_payloads() {
        let unmanaged = session("ready");
        let mut managed = unmanaged.clone();
        managed.systemd_scope = Some("atmux-tmux-spawn-1-2-0123456789abcdef.scope".to_owned());
        managed.memory_max_bytes = Some(34_359_738_368);
        assert!(!observable_session_equal(&unmanaged, &managed));

        let summary = SessionSummary::from_local(LOCAL_MACHINE_ID, "%1".to_owned(), &managed);
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            json["systemd_scope"],
            "atmux-tmux-spawn-1-2-0123456789abcdef.scope"
        );
        assert_eq!(json["memory_max_bytes"], 34_359_738_368_u64);
        assert_eq!(json["instance_id"], format!("pane-v1-{}", "a".repeat(64)));

        let mut legacy = json;
        legacy.as_object_mut().unwrap().remove("instance_id");
        legacy.as_object_mut().unwrap().remove("systemd_scope");
        legacy.as_object_mut().unwrap().remove("memory_max_bytes");
        let decoded: SessionSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.systemd_scope, None);
        assert_eq!(decoded.memory_max_bytes, None);
        assert!(decoded.instance_id.is_empty());

        let mut replacement = managed.clone();
        replacement.pane_identity = format!("pane-v1-{}", "b".repeat(64));
        assert!(!observable_session_equal(&managed, &replacement));
    }

    fn local_control() -> ControlPlane {
        control_with_machines(&[])
    }

    fn control_with_machines(machines: &[&str]) -> ControlPlane {
        super::test_control(machines)
    }

    fn coordinator_only_config() -> Config {
        let mut config = Config::default();
        config.node.id = "home".to_owned();
        config.node.label = Some("Home".to_owned());
        config.node.coordinator_only = true;
        config.profiles.clear();
        config.general.project_roots.clear();
        config.general.favorite_dirs.clear();
        config.general.switch_on_launch = false;
        config
    }

    fn coordinator_only_control(machines: &[&str]) -> ControlPlane {
        super::test_control_with_config(machines, coordinator_only_config())
    }

    #[test]
    fn prompt_mutations_share_a_lock_only_while_the_pane_is_active() {
        let control = local_control();
        let first = control.prompt_lock("%1");
        let same_pane = control.prompt_lock("%1");
        let other_pane = control.prompt_lock("%2");
        assert!(Arc::ptr_eq(&first, &same_pane));
        assert!(!Arc::ptr_eq(&first, &other_pane));

        let guard = first
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&same_pane);
        let waiter = std::thread::spawn(move || {
            let _guard = waiting
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            acquired_tx.send(()).unwrap();
        });
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();

        let old = Arc::downgrade(&first);
        drop(first);
        drop(same_pane);
        assert!(old.upgrade().is_none());
        let replacement = control.prompt_lock("%1");
        assert!(old.upgrade().is_none());
        assert_eq!(Arc::strong_count(&replacement), 1);
    }

    fn remote_summary(machine: &str, pane: &str, name: &str, hash: &str) -> SessionSummary {
        SessionSummary {
            id: composite_id(machine, pane),
            session_key: None,
            instance_id: format!("pane-v1-{}", "a".repeat(64)),
            machine: machine.to_owned(),
            name: name.to_owned(),
            description: None,
            description_source: None,
            pane_id: pane.to_owned(),
            status: "working".to_owned(),
            agent: "claude".to_owned(),
            profile: "claude-max".to_owned(),
            attached: false,
            activity: 1,
            path: "/srv".to_owned(),
            title: name.to_owned(),
            command: "claude".to_owned(),
            launch_command: "claude".to_owned(),
            systemd_scope: None,
            memory_max_bytes: None,
            windows: 1,
            window_index: 0,
            pane_index: 0,
            content_hash: hash.to_owned(),
        }
    }

    #[test]
    fn tails_unicode_by_lines() {
        assert_eq!(tail_lines("zero\none 🍰\ntwo", 2), "one 🍰\ntwo");
        assert_eq!(tail_lines("zero\n", 1), "");
    }

    #[test]
    fn front_truncation_keeps_utf8_valid() {
        let mut value = "abc🍰def".to_owned();
        truncate_front(&mut value, 5);
        assert_eq!(value, "def");
    }

    #[test]
    fn front_truncation_prefers_a_complete_line() {
        let mut value = "partial text\nwhole line\nlast".to_owned();
        truncate_front(&mut value, 20);
        assert_eq!(value, "whole line\nlast");
        assert!(value.len() <= 20);
    }

    #[test]
    fn output_tail_is_strictly_byte_bounded() {
        let value = format!("header\n{}", "🍰".repeat(MAX_OUTPUT_BYTES));
        let tail = tail_lines(&value, 2_000);
        assert!(tail.len() <= MAX_OUTPUT_BYTES);
        assert!(tail.is_char_boundary(0));
    }

    #[test]
    fn session_names_are_narrowly_validated() {
        assert!(validate_session_name("review_2-good").is_ok());
        assert!(validate_session_name(RESERVED_SERVICE_SESSION).is_err());
        assert!(validate_session_name("bad name").is_err());
        assert!(validate_session_name("").is_err());
    }

    #[test]
    fn launch_browser_is_root_bounded_and_skips_symlink_escapes() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("atmux-launch-browser-{nonce}"));
        let root = base.join("projects");
        let child = root.join("group");
        let nested = child.join("project");
        let outside = base.join("outside");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let mut config = Config::default();
        config.general.project_roots = vec![root.clone()];
        config.general.favorite_dirs.clear();
        let canonical_root = root.canonicalize().unwrap();
        let canonical_child = child.canonicalize().unwrap();

        let roots = browse_configured_directories(&config, "tron".to_owned(), None).unwrap();
        assert_eq!(roots.current, None);
        assert_eq!(
            roots.directories,
            vec![BrowseDirectory {
                path: canonical_root.to_string_lossy().into_owned(),
                name: "projects".to_owned(),
            }]
        );

        let listing =
            browse_configured_directories(&config, "tron".to_owned(), root.to_str()).unwrap();
        assert_eq!(listing.current.as_deref(), canonical_root.to_str());
        assert_eq!(listing.parent, None);
        assert_eq!(
            listing.directories,
            vec![BrowseDirectory {
                path: canonical_child.to_string_lossy().into_owned(),
                name: "group".to_owned(),
            }]
        );
        let nested_listing =
            browse_configured_directories(&config, "tron".to_owned(), child.to_str()).unwrap();
        assert_eq!(nested_listing.parent.as_deref(), canonical_root.to_str());

        config.general.favorite_dirs = vec![child.clone()];
        let overlapping_root =
            browse_configured_directories(&config, "tron".to_owned(), child.to_str()).unwrap();
        assert_eq!(
            overlapping_root.parent.as_deref(),
            canonical_root.to_str(),
            "a nested configured root must still navigate up when an outer root allows it"
        );
        config.general.project_roots = vec![child.clone()];
        config.general.favorite_dirs.clear();
        let actual_root =
            browse_configured_directories(&config, "tron".to_owned(), child.to_str()).unwrap();
        assert_eq!(actual_root.parent, None);

        let error = browse_configured_directories(&config, "tron".to_owned(), outside.to_str())
            .unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::BadRequest);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn overview_patch_covers_add_remove_change_health_and_reorder() {
        let empty = overview(1, Vec::new(), None);
        let added = overview(2, vec![summary("%1", "working")], None);
        let patch = overview_patch(&empty, &added);
        assert_eq!(patch.upsert, added.sessions);
        assert!(patch.remove.is_empty());

        let removed = overview(3, Vec::new(), None);
        let patch = overview_patch(&added, &removed);
        assert!(patch.upsert.is_empty());
        assert_eq!(patch.remove, vec!["local~%1"]);

        let changed = overview(3, vec![summary("%1", "waiting")], None);
        let patch = overview_patch(&added, &changed);
        assert_eq!(patch.upsert, changed.sessions);
        assert!(patch.remove.is_empty());

        let mut volatile = added.sessions.clone();
        volatile[0].activity = 99;
        volatile[0].title = "⠙ spinner".to_owned();
        let patch = overview_patch(&added, &overview(3, volatile, None));
        assert!(patch.upsert.is_empty());
        assert!(patch.remove.is_empty());

        let unhealthy = overview(4, changed.sessions.clone(), Some("tmux unavailable"));
        let patch = overview_patch(&changed, &unhealthy);
        assert!(patch.upsert.is_empty());
        assert!(patch.remove.is_empty());
        assert_eq!(patch.health.as_deref(), Some("tmux unavailable"));

        let first = summary("%1", "working");
        let second = summary("%2", "waiting");
        let ordered = overview(5, vec![first.clone(), second.clone()], None);
        let reordered = overview(6, vec![second, first], None);
        let patch = overview_patch(&ordered, &reordered);
        assert!(patch.upsert.is_empty());
        assert!(patch.remove.is_empty());
    }

    #[test]
    fn pane_patch_handles_empty_append_truncate_repeated_and_unchanged_lines() {
        for (old_content, new_content) in [
            ("", "\n"),
            ("one", "one\ntwo"),
            ("one\ntwo\nthree", "one"),
            ("same\nold\nsame\ntail", "same\nnew\nsame\ntail"),
            ("one\n\nthree", "one\n\nthree"),
        ] {
            let old = pane(1, old_content);
            let new = pane(2, new_content);
            let patch = pane_patch(&old, &new, old_content, new_content);
            assert_eq!(apply_pane_patch(old_content, &patch), new_content);
        }

        let appended = pane_patch(&pane(1, "one"), &pane(2, "one\ntwo"), "one", "one\ntwo");
        assert_eq!(appended.start_line, 1);
        assert_eq!(appended.delete_lines, 0);
        assert_eq!(appended.lines, vec!["two"]);

        let unchanged = pane_patch(&pane(1, "one"), &pane(2, "one"), "one", "one");
        assert_eq!(unchanged.start_line, 1);
        assert_eq!(unchanged.delete_lines, 0);
        assert!(unchanged.lines.is_empty());
    }

    #[test]
    fn refresh_revisions_ignore_volatile_spinner_churn_but_track_real_changes() {
        let control = local_control();
        let mut initial = session("answer\n⠋ Thinking");
        assert!(control.apply_refresh(vec![initial.clone()]));
        let initial_overview = control.overview();
        assert_eq!(initial_overview.revision, 1);
        let stable_hash = initial_overview.sessions[0].content_hash.clone();

        initial.activity += 1;
        initial.title = "⠙ agent".to_owned();
        initial.content = "answer\n⠙ Thinking".to_owned();
        initial.content_hash = content_hash(&initial.content);
        assert!(!control.apply_refresh(vec![initial.clone()]));
        let snapshot = control.overview();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.sessions[0].activity, 2);
        assert_eq!(snapshot.sessions[0].title, "⠙ agent");
        assert_eq!(snapshot.sessions[0].content_hash, stable_hash);

        initial.content.push_str("\nmeaningful result");
        initial.content_hash = content_hash(&initial.content);
        assert!(control.apply_refresh(vec![initial.clone()]));
        let meaningful = control.overview();
        assert_eq!(meaningful.revision, 2);
        assert_ne!(meaningful.sessions[0].content_hash, stable_hash);

        control.record_health_error("tmux unavailable".to_owned());
        assert_eq!(control.overview().revision, 3);
        control.record_health_error("tmux unavailable".to_owned());
        assert_eq!(control.overview().revision, 3);
        assert!(control.apply_refresh(vec![initial.clone()]));
        assert_eq!(control.overview().revision, 4);
        assert!(!control.apply_refresh(vec![initial]));
        assert_eq!(control.overview().revision, 4);
    }

    #[test]
    fn codex_elapsed_working_timer_is_not_observable_churn() {
        let first = "result\n• Working (48s • esc to interrupt)\n";
        let second = "result\n• Working (1m 02s • esc to interrupt)\n";
        assert!(observable_content_equal(first, second));
        assert_eq!(
            observable_content_hash(first),
            observable_content_hash(second)
        );
        assert!(!observable_content_equal(
            first,
            "changed\n• Working (49s • esc to interrupt)\n"
        ));
    }

    #[test]
    fn refresh_revisions_ignore_activity_only_reordering() {
        let control = local_control();
        let first = session("first");
        let mut second = session("second");
        second.name = "other".to_owned();
        second.pane_id = "%2".to_owned();
        assert!(control.apply_refresh(vec![first.clone(), second.clone()]));

        let mut reordered_first = first;
        let mut reordered_second = second;
        reordered_first.activity = 10;
        reordered_second.activity = 20;
        assert!(!control.apply_refresh(vec![reordered_second, reordered_first]));
        assert_eq!(control.overview().revision, 1);
    }

    #[tokio::test]
    async fn revision_wait_distinguishes_advance_timeout_and_sender_drop() {
        let (sender, receiver) = watch::channel(1_u64);
        assert!(wait_for_revision_receiver(receiver, 0, Duration::from_millis(20)).await);
        drop(sender);

        let (sender, receiver) = watch::channel(0_u64);
        assert!(!wait_for_revision_receiver(receiver, 0, Duration::from_millis(10)).await);
        drop(sender);

        let (sender, receiver) = watch::channel(0_u64);
        drop(sender);
        assert!(!wait_for_revision_receiver(receiver, 0, Duration::from_millis(20)).await);
    }

    #[test]
    fn a_control_plane_without_machines_behaves_exactly_like_before_federation() {
        let control = local_control();
        control.apply_refresh(vec![session("hello")]);
        let overview = control.overview();

        assert_eq!(overview.sessions.len(), 1);
        // With no [[machines]] there is nothing to disambiguate, so the exact
        // pre-federation identity is emitted and saved URLs keep resolving.
        assert_eq!(overview.sessions[0].id, "%1");
        assert_eq!(overview.sessions[0].machine, LOCAL_MACHINE_ID);
        assert_eq!(overview.sessions[0].pane_id, "%1");
        assert_eq!(overview.machines.len(), 1);
        assert_eq!(overview.machines[0].kind, MachineKind::Local);
        assert!(overview.machines[0].online);
        assert_eq!(overview.machines[0].sessions, 1);
        assert!(overview.machines[0].address.is_none());

        // Bare pane ids, bare names, and composite ids all still resolve.
        for id in ["%1", "agent", "local~%1"] {
            assert!(
                matches!(control.resolve(id).unwrap(), Target::Local { .. }),
                "{id} must resolve locally"
            );
        }
        assert!(control.resolve("nope").is_err());

        let options = control.launch_options();
        assert_eq!(options.machines.len(), 1);
        assert_eq!(options.machines[0].id, LOCAL_MACHINE_ID);
        assert_eq!(options.machines[0].directories, options.directories);
        assert!(options.machines[0].online);
    }

    #[tokio::test]
    async fn local_pane_output_keeps_its_hash_contract_under_composite_ids() {
        let control = local_control();
        control.apply_refresh(vec![session("first\nsecond")]);
        // A composite id is still accepted, but a local-only node keeps
        // emitting the bare pane id it emitted before federation.
        let output = control
            .pane_output("local~%1", None, 80)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.pane_id, "%1");
        assert!(output.changed);
        assert_eq!(output.content.as_deref(), Some("first\nsecond"));

        let unchanged = control
            .pane_output("%1", Some(&output.content_hash), 80)
            .await
            .unwrap()
            .unwrap();
        assert!(!unchanged.changed);
        assert!(unchanged.content.is_none());
        assert!(!control.pane_may_have_changed("%1", &output.content_hash));
        assert!(control.pane_may_have_changed("%1", "0000000000000000"));

        assert!(
            control
                .pane_output("missing", None, 80)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn local_files_and_git_use_the_live_pane_path_for_bare_and_composite_ids() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("atmux-pane-files-{nonce}"));
        let project = base.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
        let mut config = Config::default();
        config.general.project_roots = vec![base.clone()];
        config.general.favorite_dirs.clear();
        let control = test_control_with_config(&[], config);
        let mut live = session("working");
        live.path = project;
        control.apply_refresh(vec![live]);

        for id in ["%1", "local~%1"] {
            let FilesResponse::File {
                pane_id, content, ..
            } = control
                .pane_files(id, Some("main.rs"))
                .await
                .unwrap()
                .expect("local file")
            else {
                panic!("expected file")
            };
            assert_eq!(pane_id, "%1");
            assert_eq!(content, "fn main() {}\n");
        }
        let GitResponse::Summary(summary) = control
            .pane_git("local~%1", None)
            .await
            .unwrap()
            .expect("local Git status")
        else {
            panic!("expected summary")
        };
        assert!(!summary.available);
        assert_eq!(summary.pane_id, "%1");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sessions_group_by_machine_with_health_and_last_seen() {
        let control = control_with_machines(&["gpu-box", "mini"]);
        control.apply_refresh(vec![session("local work")]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        control.mark_machine_offline("mini", "connection refused");

        let overview = control.overview();
        assert_eq!(
            overview
                .machines
                .iter()
                .map(|machine| (machine.id.as_str(), machine.online, machine.sessions))
                .collect::<Vec<_>>(),
            [("local", true, 1), ("gpu-box", true, 1), ("mini", false, 0)]
        );
        assert_eq!(
            overview
                .sessions
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["local~%1", "gpu-box~%4"]
        );
        let mini = &overview.machines[2];
        assert_eq!(mini.health.as_deref(), Some("connection refused"));
        assert_eq!(mini.address.as_deref(), Some("mini.invalid:7345"));
        let gpu = &overview.machines[1];
        assert!(gpu.last_seen_ms.is_some_and(|seen| seen > 0));
        assert_eq!(gpu.label, "gpu-box label");

        // An offline machine leaves local and healthy machines fully usable.
        assert!(matches!(
            control.resolve("local~%1").unwrap(),
            Target::Local { .. }
        ));
        assert!(matches!(
            control.resolve("gpu-box~%4").unwrap(),
            Target::Remote { .. }
        ));
        assert!(control.resolve("mini~%9").is_err());
        assert!(control.ensure_online("gpu-box").is_ok());
        let offline = control.ensure_online("mini").unwrap_err().to_string();
        assert!(offline.contains("offline"), "{offline}");
    }

    #[test]
    fn remote_dashboard_service_session_is_hidden() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![
                remote_summary("gpu-box", "%1", RESERVED_SERVICE_SESSION, "aaaa"),
                remote_summary("gpu-box", "%2", "agent", "bbbb"),
            ],
            None,
        );

        let overview = control.overview();
        assert_eq!(overview.sessions.len(), 1);
        assert_eq!(overview.sessions[0].name, "agent");
        assert_eq!(overview.machines[1].sessions, 1);
    }

    #[test]
    fn routing_prefers_local_and_refuses_to_guess_between_machines() {
        let control = control_with_machines(&["gpu-box", "mini"]);
        control.apply_refresh(vec![session("local")]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%1", "agent", "aaaa")],
            None,
        );
        control.apply_machine_sessions(
            "mini",
            vec![remote_summary("mini", "%1", "agent", "bbbb")],
            None,
        );

        // The local tmux server keeps winning a bare id, exactly as before.
        assert!(matches!(
            control.resolve("agent").unwrap(),
            Target::Local { .. }
        ));

        // Without a local session the same bare id is genuinely ambiguous.
        control.apply_refresh(Vec::new());
        let ambiguous = control.resolve("%1").unwrap_err().to_string();
        assert!(ambiguous.contains("several machines"), "{ambiguous}");
        assert!(ambiguous.contains("gpu-box~%1"), "{ambiguous}");

        // Composite ids stay unambiguous.
        match control.resolve("mini~%1").unwrap() {
            Target::Remote {
                machine, pane_id, ..
            } => {
                assert_eq!(machine.id, "mini");
                assert_eq!(pane_id, "%1");
            }
            Target::Local { .. } => panic!("expected a remote target"),
        }

        // A caller that wants to scope a bare id qualifies it first; there is
        // no second, hint-shaped resolution path to keep consistent.
        let scoped = control.reference("%1", Some("gpu-box")).unwrap();
        assert!(matches!(
            control.resolve(&scoped).unwrap(),
            Target::Remote { .. }
        ));
        assert!(control.reference("%1", Some("nowhere")).is_err());
        assert!(control.reference("gpu-box~%1", Some("mini")).is_err());
    }

    #[test]
    fn references_never_accept_a_url_or_an_unconfigured_machine() {
        let control = control_with_machines(&["gpu-box"]);
        assert_eq!(control.reference("%1", None).unwrap(), "%1");
        assert_eq!(
            control.reference("%1", Some("gpu-box")).unwrap(),
            "gpu-box~%1"
        );
        assert_eq!(
            control.reference("gpu-box~%1", Some("gpu-box")).unwrap(),
            "gpu-box~%1"
        );
        assert!(control.reference("%1", Some("mini")).is_err());
        assert!(control.reference("gpu-box~%1", Some("local")).is_err());
        // A URL is never a machine selector, so it can never become a request target.
        for hostile in ["http://evil.example", "169.254.169.254", "../../etc"] {
            assert!(
                control.reference("%1", Some(hostile)).is_err(),
                "{hostile} must not select a machine"
            );
        }
    }

    #[test]
    fn remote_updates_coalesce_and_only_wake_listeners_on_observable_change() {
        let control = control_with_machines(&["gpu-box"]);
        let baseline = control.overview().revision;

        let first = vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")];
        control.apply_machine_sessions("gpu-box", first.clone(), None);
        let after_first = control.overview().revision;
        assert!(
            after_first > baseline,
            "coming online must publish a revision"
        );

        // Volatile churn the node already filtered out must not wake listeners.
        let mut volatile = first.clone();
        volatile[0].activity = 99;
        volatile[0].title = "⠙ spinner".to_owned();
        control.apply_machine_sessions("gpu-box", volatile, None);
        assert_eq!(control.overview().revision, after_first);

        let mut changed = first.clone();
        changed[0].content_hash = "bbbb".to_owned();
        control.apply_machine_sessions("gpu-box", changed, None);
        let after_change = control.overview().revision;
        assert_eq!(after_change, after_first + 1);

        // Going offline publishes once and then stays quiet while retrying.
        control.mark_machine_offline("gpu-box", "connection refused");
        let after_offline = control.overview().revision;
        assert_eq!(after_offline, after_change + 1);
        control.mark_machine_offline("gpu-box", "connection refused");
        assert_eq!(control.overview().revision, after_offline);
        assert!(control.overview().sessions.is_empty());

        // An unknown machine can never mutate shared state.
        control.apply_machine_sessions("ghost", first, None);
        assert_eq!(control.overview().revision, after_offline);
    }

    #[test]
    fn remote_output_cache_serves_one_fetch_per_change_and_evicts_dead_panes() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        control.store_output("gpu-box~%4", "aaaa", "captured output");

        assert_eq!(
            control.mirrored_content_hash("gpu-box", "%4").as_deref(),
            Some("aaaa")
        );
        let cached = control.cached_output("gpu-box~%4", "aaaa").unwrap();
        assert_eq!(cached.content, "captured output");
        // A stale hash never serves stale bytes; the caller must refetch.
        assert!(control.cached_output("gpu-box~%4", "bbbb").is_none());

        // Panes that disappear from a machine drop out of the cache.
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%5", "other", "cccc")],
            None,
        );
        assert!(control.cached_output("gpu-box~%4", "aaaa").is_none());

        control.store_output("gpu-box~%5", "cccc", "still here");
        control.mark_machine_offline("gpu-box", "gone");
        assert!(control.cached_output("gpu-box~%5", "cccc").is_none());
    }

    #[test]
    fn launch_options_report_offline_machines_without_hiding_local_inputs() {
        let control = control_with_machines(&["gpu-box"]);
        control.mark_machine_offline("gpu-box", "connection refused");
        let options = control.launch_options();
        assert_eq!(options.machines.len(), 2);
        assert_eq!(options.machines[0].id, "local");
        assert!(options.machines[0].online);
        assert!(!options.machines[0].profiles.is_empty());
        let remote = &options.machines[1];
        assert!(!remote.online);
        assert!(remote.directories.is_empty());
        assert_eq!(remote.note.as_deref(), Some("offline: connection refused"));

        control.set_machine_launch_options(
            "gpu-box",
            LaunchOptions {
                directories: vec!["/srv/models".to_owned()],
                profiles: vec![ProfileOption {
                    id: "profile-0".to_owned(),
                    name: "Default".to_owned(),
                    harness: "claude".to_owned(),
                    modes: Vec::new(),
                }],
                project_preferences: BTreeMap::new(),
                memory: None,
                machines: Vec::new(),
            },
        );
        control.apply_machine_sessions("gpu-box", Vec::new(), None);
        let options = control.launch_options();
        assert_eq!(options.machines[1].directories, ["/srv/models"]);
        assert_eq!(options.machines[1].profiles[0].harness, "claude");
        assert!(options.machines[1].note.is_none());
    }

    #[test]
    fn launch_memory_options_are_owner_scoped_and_legacy_payloads_stay_valid() {
        let mut config = Config::default();
        config.agent_resources.memory_max_bytes = Some(16 * systemd_scope::GIBIBYTE);
        config.agent_resources.memory_override_max_bytes = Some(24 * systemd_scope::GIBIBYTE);
        let control = super::test_control_with_config(&[], config);
        let options = control.launch_options();
        let memory = options.memory.as_ref().unwrap();
        assert_eq!(memory.default_bytes, Some(16 * systemd_scope::GIBIBYTE));
        if let Some(ceiling) = memory.override_max_bytes {
            assert!(ceiling <= 24 * systemd_scope::GIBIBYTE);
            assert!(memory.presets_bytes.iter().all(|preset| *preset <= ceiling));
        } else {
            assert!(memory.presets_bytes.is_empty());
            assert!(memory.note.as_deref().is_some_and(|note| !note.is_empty()));
        }

        let legacy: LaunchRequest = serde_json::from_value(serde_json::json!({
            "name": "legacy",
            "directory": "/tmp",
            "profile_id": "profile-0"
        }))
        .unwrap();
        assert_eq!(legacy.memory_max_bytes, None);

        let mut legacy_options = serde_json::to_value(&options).unwrap();
        legacy_options.as_object_mut().unwrap().remove("memory");
        for machine in legacy_options["machines"].as_array_mut().unwrap() {
            machine.as_object_mut().unwrap().remove("memory");
        }
        let decoded: LaunchOptions = serde_json::from_value(legacy_options).unwrap();
        assert!(decoded.memory.is_none());
        assert!(
            decoded
                .machines
                .iter()
                .all(|machine| machine.memory.is_none())
        );
    }

    #[tokio::test]
    async fn owner_rejects_a_spoofed_memory_override_before_tmux_or_systemd() {
        let mut config = Config::default();
        config.general.project_roots = vec![PathBuf::from("/tmp")];
        config.agent_resources.memory_max_bytes = Some(16 * systemd_scope::GIBIBYTE);
        config.agent_resources.memory_override_max_bytes = Some(24 * systemd_scope::GIBIBYTE);
        let control = super::test_control_with_config(&[], config);
        for (index, requested) in [0, u64::MAX, 25 * systemd_scope::GIBIBYTE]
            .into_iter()
            .enumerate()
        {
            let error = control
                .launch(LaunchRequest {
                    name: format!("malicious-memory-{index}"),
                    directory: "/tmp".to_owned(),
                    profile_id: "profile-0".to_owned(),
                    mode_id: None,
                    machine: None,
                    resume_session_id: None,
                    memory_max_bytes: Some(requested),
                    summarize_pane_id: None,
                })
                .await
                .unwrap_err();
            assert_eq!(error_kind(&error), ErrorKind::BadRequest);
        }
    }

    #[tokio::test]
    async fn coordinator_only_omits_and_rejects_every_local_owner_surface() {
        let control = coordinator_only_control(&["tron"]);
        assert!(!control.apply_refresh(vec![session("must stay hidden")]));
        control.apply_machine_sessions(
            "tron",
            vec![remote_summary("tron", "%4", "remote-agent", "aaaa")],
            None,
        );

        let overview = control.overview();
        assert_eq!(overview.machines.len(), 1);
        assert_eq!(overview.machines[0].id, "tron");
        assert_eq!(overview.sessions.len(), 1);
        assert_eq!(overview.sessions[0].machine, "tron");
        assert!(overview.health.is_none());
        assert!(!control.has_machine("home"));
        assert!(control.has_machine("tron"));
        assert_eq!(control.machine_revision("home"), None);

        let options = control.launch_options();
        assert!(options.directories.is_empty());
        assert!(options.profiles.is_empty());
        assert_eq!(options.machines.len(), 1);
        assert_eq!(options.machines[0].id, "tron");

        let launch = control
            .launch(LaunchRequest {
                name: "local-agent".to_owned(),
                directory: "/tmp".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: None,
                resume_session_id: None,
                memory_max_bytes: None,
                summarize_pane_id: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(launch.contains("coordinator-only"), "{launch}");

        let mutation = control
            .send_text("home~%1", "hello".to_owned(), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(mutation.contains("coordinator-only"), "{mutation}");
        assert!(control.reference("%1", Some("home")).is_err());
        assert!(control.recovery_status("home").await.is_err());
    }

    #[tokio::test]
    async fn coordinator_only_start_succeeds_without_initializing_a_local_owner() {
        let control = ControlPlane::start(coordinator_only_config())
            .await
            .unwrap();
        assert_eq!(control.local_id(), "home");
        assert!(control.overview().machines.is_empty());
        assert!(control.overview().sessions.is_empty());
        assert!(control.overview().health.is_none());
        assert!(!control.has_machine("home"));
    }

    #[tokio::test]
    async fn an_offline_machine_fails_only_its_own_operations() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_refresh(vec![session("local work")]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        control.mark_machine_offline("gpu-box", "connection refused");

        // Local reads and routing keep working while the remote is down.
        assert!(
            control
                .pane_output("local~%1", None, 80)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(control.overview().sessions.len(), 1);
        assert!(control.overview().machines.iter().any(|m| m.online));

        // Remote commands fail fast with an explanation instead of hanging.
        let error = control.send_text("gpu-box~%4", "hi".to_owned(), true).await;
        assert!(error.is_err());
        assert!(control.tmux_prefix_twice("gpu-box~%4").await.is_err());
        assert!(control.interrupt("gpu-box~%4").await.is_err());
        assert!(control.kill("gpu-box~%4").await.is_err());
        assert!(
            control
                .launch(LaunchRequest {
                    name: "new-agent".to_owned(),
                    directory: "/srv".to_owned(),
                    profile_id: "profile-0".to_owned(),
                    mode_id: None,
                    machine: Some("gpu-box".to_owned()),
                    resume_session_id: None,
                    memory_max_bytes: None,
                    summarize_pane_id: None,
                })
                .await
                .is_err()
        );
        // An unconfigured machine is rejected before any request is attempted.
        let unknown = control
            .launch(LaunchRequest {
                name: "new-agent".to_owned(),
                directory: "/srv".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: Some("ghost".to_owned()),
                resume_session_id: None,
                memory_max_bytes: None,
                summarize_pane_id: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("unknown machine ghost"), "{unknown}");
    }

    #[tokio::test]
    async fn a_local_only_node_emits_bare_ids_while_a_coordinator_emits_composites() {
        let local_only = local_control();
        local_only.apply_refresh(vec![session("hello")]);
        assert_eq!(local_only.overview().sessions[0].id, "%1");
        for id in ["%1", "agent", "local~%1"] {
            let output = local_only.pane_output(id, None, 80).await.unwrap().unwrap();
            assert_eq!(output.pane_id, "%1", "{id} must resolve to the bare id");
        }

        // Once a machine is configured the composite namespace is required,
        // because two machines may share a pane id.
        let federated = control_with_machines(&["gpu-box"]);
        federated.apply_refresh(vec![session("hello")]);
        assert_eq!(federated.overview().sessions[0].id, "local~%1");
        for id in ["%1", "agent", "local~%1"] {
            let output = federated.pane_output(id, None, 80).await.unwrap().unwrap();
            assert_eq!(output.pane_id, "local~%1", "{id} must still resolve");
        }
    }

    #[tokio::test]
    async fn synthetic_non_claude_resume_never_reaches_the_local_tmux_seam() {
        let control = test_control(&[]);
        // Deliberately use the pane id that exposed the old test bug. This is
        // safe even when a developer's default server has a real %1: the
        // synthetic control's mutation seam fails closed before any tmux call.
        control.apply_refresh(vec![test_session(
            "synthetic-codex",
            "%1",
            "OpenAI Codex (v0.147.0)",
        )]);
        let error = control.resume_current_claude("%1").await.unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::BadRequest);
        assert_eq!(
            control
                .inner
                .local_agent_restart_attempts
                .load(Ordering::Relaxed),
            0,
            "a non-Claude target must be rejected before the tmux mutation seam",
        );
    }

    #[tokio::test]
    async fn synthetic_codex_restart_reaches_only_the_guarded_native_restart_seam() {
        let control = test_control(&[]);
        control.apply_refresh(vec![test_session(
            "synthetic-codex",
            "%1",
            "OpenAI Codex (v0.147.0)",
        )]);
        let error = control.restart_current_agent("%1").await.unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::Internal);
        assert_eq!(
            control
                .inner
                .local_agent_restart_attempts
                .load(Ordering::Relaxed),
            1,
            "a Codex target must reach the guarded owner-local restart seam",
        );
    }

    #[tokio::test]
    async fn restart_confirmation_rejects_recycled_pane_before_mutation() {
        for content in ["OpenAI Codex (v0.153.4)", "Claude Code v2.1.227"] {
            let control = test_control(&[]);
            control.apply_refresh(vec![test_session("synthetic", "%1", content)]);
            let stale = format!("pane-v1-{}", "a".repeat(64));
            let error = control
                .restart_agent_instance("%1", &stale, &format!("restart-v1-{}", "a".repeat(64)))
                .await
                .unwrap_err();
            assert_eq!(error_kind(&error), ErrorKind::Conflict);
            assert_eq!(
                control
                    .inner
                    .local_agent_restart_attempts
                    .load(Ordering::Relaxed),
                0
            );
            let invalid = control
                .restart_agent_instance(
                    "%1",
                    "untrusted",
                    &format!("restart-v1-{}", "a".repeat(64)),
                )
                .await
                .unwrap_err();
            assert_eq!(error_kind(&invalid), ErrorKind::BadRequest);
        }
    }

    #[test]
    fn restart_preserves_conflicts_from_live_identity_check() {
        let error = local_agent_restart(Ok(Err(conflict("pane was replaced")))).unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::Conflict);
    }

    #[test]
    fn restart_tokens_bind_pane_and_native_process_creation_without_elapsed_time_drift() {
        let mut pane = test_session("agent", "%1", "Claude Code v2.1.227");
        pane.pane_identity = format!("pane-v1-{}", "a".repeat(64));
        pane.pane_pid = 10;
        pane.agent_pid = Some(11);
        pane.agent_started_ms = Some(1_000);
        let token = restart_token_from_stamp(&pane, "linux:9000").unwrap();
        assert!(valid_restart_token(&token));
        pane.agent_started_ms = Some(1_900);
        assert_eq!(
            restart_token_from_stamp(&pane, "linux:9000").unwrap(),
            token
        );
        for changed in [
            {
                let mut changed = pane.clone();
                changed.pane_identity = format!("pane-v1-{}", "b".repeat(64));
                changed
            },
            {
                let mut changed = pane.clone();
                changed.pane_pid += 1;
                changed
            },
            {
                let mut changed = pane.clone();
                changed.agent_pid = Some(12);
                changed
            },
        ] {
            assert_ne!(
                restart_token_from_stamp(&changed, "linux:9000").unwrap(),
                token
            );
        }
        assert_ne!(
            restart_token_from_stamp(&pane, "linux:9001").unwrap(),
            token,
            "PID reuse changes its OS creation stamp"
        );
        assert!(restart_token_from_stamp(&pane, "").is_none());
        pane.pane_pid = 0;
        assert!(restart_token_from_stamp(&pane, "linux:9000").is_none());
        pane.pane_pid = 10;
        pane.agent_pid = None;
        assert!(restart_token_from_stamp(&pane, "linux:9000").is_none());
        pane.agent_pid = Some(0);
        assert!(restart_token_from_stamp(&pane, "linux:9000").is_none());
        pane.agent_pid = Some(11);
        pane.pane_identity.clear();
        assert!(restart_token_from_stamp(&pane, "linux:9000").is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn native_restart_creation_stamp_is_stable_and_binding_rejects_replacements() {
        let pid = std::process::id();
        let stamp = native_process_start_stamp(pid)
            .expect("the test process must have an OS creation stamp");
        assert_eq!(
            native_process_start_stamp(pid).as_deref(),
            Some(stamp.as_str())
        );
        assert!(native_process_start_stamp(0).is_none());
        assert!(native_process_start_stamp(u32::MAX).is_none());
        let mut pane = test_session("agent", "%1", "OpenAI Codex (v0.153.4)");
        pane.pane_identity = format!("pane-v1-{}", "a".repeat(64));
        pane.pane_pid = 10;
        pane.agent_pid = Some(pid);
        let expected = RestartBinding {
            instance_id: pane.pane_identity.clone(),
            restart_token: agent_restart_token(&pane).unwrap(),
        };
        assert!(validate_restart_binding(Some(&expected), &pane).is_ok());
        pane.pane_pid = 11;
        assert_eq!(
            error_kind(&validate_restart_binding(Some(&expected), &pane).unwrap_err()),
            ErrorKind::Conflict
        );
        pane.agent_pid = None;
        assert_eq!(
            error_kind(&validate_restart_binding(Some(&expected), &pane).unwrap_err()),
            ErrorKind::Conflict
        );
        assert!(
            validate_restart_binding(None, &pane).is_ok(),
            "legacy unbound routes remain compatible"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_restart_start_stamp_handles_spaces_and_parentheses_in_process_names() {
        let mut fields = vec!["S"; 20];
        fields[19] = "123456";
        assert_eq!(
            linux_process_start_ticks(&format!("99 (native (agent) runner) {}", fields.join(" "))),
            Some(123_456)
        );
        assert!(linux_process_start_ticks("99 (native) S 1 2").is_none());
        fields[19] = "0";
        assert!(linux_process_start_ticks(&format!("99 (native) {}", fields.join(" "))).is_none());
    }

    #[tokio::test]
    async fn bound_local_claude_restart_cannot_fall_through_to_a_remote_pane() {
        let control = test_control(&["gpu-box"]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%1", "remote", "aaaa")],
            None,
        );
        let binding = RestartBinding {
            instance_id: format!("pane-v1-{}", "a".repeat(64)),
            restart_token: format!("restart-v1-{}", "b".repeat(64)),
        };
        let local = composite_id(&control.inner.local_id, "%1");
        let error = control
            .resume_claude_bound(&local, Some(binding.clone()))
            .await
            .unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::NotFound);
        let error = control
            .resume_claude_bound("%1", Some(binding))
            .await
            .unwrap_err();
        assert_eq!(
            error_kind(&error),
            ErrorKind::Conflict,
            "even an accidentally bare bound delegation must never forward legacy resume"
        );
    }

    #[tokio::test]
    async fn restart_binding_is_forwarded_exactly_and_old_owners_never_get_a_legacy_fallback() {
        use axum::{Json, Router, extract::OriginalUri, http::StatusCode};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let router = Router::new().fallback(
            move |OriginalUri(uri): OriginalUri, Json(body): Json<serde_json::Value>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push((uri.path().to_owned(), body));
                    StatusCode::NOT_FOUND
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let control = test_control(&["gpu-box"]);
        control.inner.machines.write().unwrap().insert(
            "gpu-box".to_owned(),
            Arc::new(
                RemoteMachine::from_config(&crate::config::MachineConfig {
                    id: "gpu-box".to_owned(),
                    label: None,
                    url: format!("http://{address}"),
                    token_env: None,
                    token_file: None,
                })
                .unwrap(),
            ),
        );
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%1", "remote", "aaaa")],
            None,
        );
        let instance = format!("pane-v1-{}", "a".repeat(64));
        let token = format!("restart-v1-{}", "b".repeat(64));
        let result = control
            .restart_agent_instance("gpu-box~%1", &instance, &token)
            .await;
        server.abort();
        assert!(result.is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(
                "/api/v1/panes/%251/restart-instance".to_owned(),
                serde_json::json!({ "instance_id": instance, "restart_token": token })
            )]
        );
    }

    #[tokio::test]
    async fn machine_cursors_advance_only_for_the_machine_that_changed() {
        let control = control_with_machines(&["gpu-box", "mini"]);
        assert_eq!(control.machine_revision("gpu-box"), Some(0));
        assert_eq!(control.machine_revision("local"), Some(0));
        assert_eq!(control.machine_revision("ghost"), None);

        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        let gpu = control.machine_revision("gpu-box").unwrap();
        assert!(gpu > 0);
        assert_eq!(
            control.machine_revision("mini"),
            Some(0),
            "an unrelated machine must not appear to have changed"
        );

        // A machine-scoped overview carries that machine's own cursor and only
        // its own sessions.
        let scoped = control.machine_overview("gpu-box").unwrap();
        assert_eq!(scoped.revision, gpu);
        assert_eq!(scoped.machines.len(), 1);
        assert_eq!(scoped.sessions.len(), 1);
        assert!(control.machine_overview("ghost").is_none());

        // Waiting on mini is not satisfied by gpu-box churn.
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "bbbb")],
            None,
        );
        assert!(
            !control
                .wait_for_machine_revision("mini", 0, Duration::from_millis(50))
                .await
        );
        assert_eq!(control.machine_revision("mini"), Some(0));

        // mini's own change wakes it immediately.
        let waiter = {
            let control = control.clone();
            tokio::spawn(async move {
                control
                    .wait_for_machine_revision("mini", 0, Duration::from_secs(5))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        control.mark_machine_offline("mini", "connection refused");
        assert!(waiter.await.unwrap());
        assert!(control.machine_revision("mini").unwrap() > 0);
        assert!(
            !control
                .wait_for_machine_revision("ghost", 0, Duration::from_millis(50))
                .await
        );
    }

    #[tokio::test]
    async fn remote_pane_reincarnation_advances_revision_and_notifies_subscribers() {
        let control = control_with_machines(&["gpu-box"]);
        let mut original = remote_summary("gpu-box", "%4", "trainer", "aaaa");
        original.instance_id = format!("pane-v1-{}", "a".repeat(64));
        control.apply_machine_sessions("gpu-box", vec![original.clone()], None);
        let prior_revision = control.machine_revision("gpu-box").unwrap();
        let mut updates = control.subscribe();

        let mut replacement = original;
        replacement.instance_id = format!("pane-v1-{}", "b".repeat(64));
        control.apply_machine_sessions("gpu-box", vec![replacement.clone()], None);

        tokio::time::timeout(Duration::from_secs(1), updates.changed())
            .await
            .expect("pane reincarnation must wake federation observers")
            .expect("revision sender must remain live");
        assert!(control.machine_revision("gpu-box").unwrap() > prior_revision);
        assert_eq!(
            control.machine_overview("gpu-box").unwrap().sessions[0].instance_id,
            replacement.instance_id
        );
    }

    #[test]
    fn expected_pane_instances_fail_closed_on_malformed_or_recycled_generations() {
        let current = format!("pane-v1-{}", "a".repeat(64));
        assert!(validate_expected_pane_instance(None, &current).is_ok());
        assert!(validate_expected_pane_instance(Some(&current), &current).is_ok());

        let recycled =
            validate_expected_pane_instance(Some(&format!("pane-v1-{}", "b".repeat(64))), &current)
                .unwrap_err();
        assert_eq!(error_kind(&recycled), ErrorKind::Conflict);
        let malformed = validate_expected_pane_instance(Some("%7"), &current).unwrap_err();
        assert_eq!(error_kind(&malformed), ErrorKind::BadRequest);
    }

    #[test]
    fn message_task_preserves_generation_conflicts_but_not_raw_tmux_failures() {
        let changed = local_message_mutation::<()>(Ok(Err(conflict("pane changed"))))
            .expect_err("a changed pane must fail");
        assert_eq!(error_kind(&changed), ErrorKind::Conflict);

        let tmux = local_message_mutation::<()>(Ok(Err(anyhow::anyhow!("tmux paste failed"))))
            .expect_err("a tmux failure must fail");
        assert_eq!(error_kind(&tmux), ErrorKind::Internal);
    }

    #[tokio::test]
    async fn failures_are_classified_for_transport_agnostic_reporting() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_refresh(vec![session("local work")]);
        control.mark_machine_offline("gpu-box", "connection refused");

        let kind = |error: anyhow::Error| error_kind(&error);
        assert_eq!(
            kind(
                control
                    .send_text("nope", "hi".to_owned(), true)
                    .await
                    .unwrap_err()
            ),
            ErrorKind::NotFound
        );
        assert_eq!(
            kind(
                control
                    .pane_output("gpu-box~%4", None, 80)
                    .await
                    .unwrap_err()
            ),
            ErrorKind::Offline
        );
        assert_eq!(
            kind(
                control
                    .send_text("gpu-box~%4", "hi".to_owned(), true)
                    .await
                    .unwrap_err()
            ),
            ErrorKind::Offline
        );
        assert_eq!(
            kind(control.tmux_prefix_twice("gpu-box~%4").await.unwrap_err()),
            ErrorKind::Offline
        );
        assert_eq!(
            kind(control.reference("%1", Some("ghost")).unwrap_err()),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(
                control
                    .launch(LaunchRequest {
                        name: "bad name".to_owned(),
                        directory: "/srv".to_owned(),
                        profile_id: "profile-0".to_owned(),
                        mode_id: None,
                        machine: None,
                        resume_session_id: None,
                        memory_max_bytes: None,
                        summarize_pane_id: None,
                    })
                    .await
                    .unwrap_err()
            ),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind(
                control
                    .launch(LaunchRequest {
                        name: "agent".to_owned(),
                        directory: "/srv".to_owned(),
                        profile_id: "profile-0".to_owned(),
                        mode_id: None,
                        machine: None,
                        resume_session_id: None,
                        memory_max_bytes: None,
                        summarize_pane_id: None,
                    })
                    .await
                    .unwrap_err()
            ),
            ErrorKind::Conflict,
            "a duplicate session name is a conflict, not a bad request"
        );

        // An unclassified failure is this coordinator's fault, never the caller's.
        assert_eq!(
            error_kind(&anyhow::anyhow!("something went wrong")),
            ErrorKind::Internal
        );
        assert_eq!(
            error_kind(&upstream(&anyhow::anyhow!("node said no"))),
            ErrorKind::Upstream
        );
        // A classification survives added context.
        assert_eq!(
            error_kind(&offline("machine gpu-box is offline: x").context("while reading")),
            ErrorKind::Offline
        );
    }

    #[tokio::test]
    async fn concurrent_remote_reads_share_one_fetch_lock_per_pane() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        // The same pane always hands back the same lock, and a different pane
        // never shares it, so unrelated panes are not serialized.
        let first = control.fetch_lock("gpu-box~%4");
        let again = control.fetch_lock("gpu-box~%4");
        let other = control.fetch_lock("gpu-box~%5");
        assert!(Arc::ptr_eq(&first, &again));
        assert!(!Arc::ptr_eq(&first, &other));

        // Lock state is dropped along with the pane it belonged to.
        control.apply_machine_sessions("gpu-box", Vec::new(), None);
        assert_eq!(control.inner.fetches.read().unwrap().len(), 0);
        control.fetch_lock("gpu-box~%4");
        control.mark_machine_offline("gpu-box", "gone");
        assert_eq!(control.inner.fetches.read().unwrap().len(), 0);
    }

    #[test]
    fn overview_patches_carry_machine_health_for_streaming_clients() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_refresh(vec![session("work")]);
        let before = control.overview();
        control.mark_machine_offline("gpu-box", "connection refused");
        let after = control.overview();

        let patch = overview_patch(&before, &after);
        assert!(patch.upsert.is_empty());
        assert!(patch.remove.is_empty());
        assert_ne!(patch.machines, before.machines);
        assert_eq!(
            patch.machines[1].health.as_deref(),
            Some("connection refused")
        );

        // Patches round-trip so a coordinator can mirror another coordinator's node.
        let encoded = serde_json::to_string(&patch).unwrap();
        let decoded: OverviewPatch = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, patch);
        assert!(!encoded.contains("token"));
    }

    #[tokio::test]
    async fn discovered_machines_are_added_removed_and_expose_telemetry() {
        let control = test_control(&[]);
        let machine = RemoteMachine::from_config(&crate::config::MachineConfig {
            id: "gpu-box".to_owned(),
            label: Some("GPU box".to_owned()),
            url: "http://192.168.1.8:7345".to_owned(),
            token_env: None,
            token_file: None,
        })
        .unwrap();
        control.upsert_discovered_machine(machine);
        let overview = control.overview();
        assert_eq!(overview.machines.len(), 2);
        assert_eq!(overview.machines[1].id, "gpu-box");
        assert_eq!(
            overview.machines[1].address.as_deref(),
            Some("192.168.1.8:7345")
        );

        let metrics = MachineMetrics {
            cpu_percent: Some(42),
            memory_used_bytes: 4,
            memory_total_bytes: 8,
            uptime_seconds: Some(183_840),
            kernel_version: Some("6.8.0-48-generic".to_owned()),
            os_version: Some("Linux (Ubuntu 24.04)".to_owned()),
            ..MachineMetrics::default()
        };
        control.set_machine_metrics("gpu-box", metrics.clone());
        assert_eq!(control.overview().machines[1].metrics, metrics);

        // The browser/API model exposes owner-local system telemetry without a
        // separate coordinator lookup.
        let encoded = serde_json::to_value(control.overview()).unwrap();
        let api_metrics = &encoded["machines"][1]["metrics"];
        assert_eq!(api_metrics["uptime_seconds"], 183_840);
        assert_eq!(api_metrics["kernel_version"], "6.8.0-48-generic");
        assert_eq!(api_metrics["os_version"], "Linux (Ubuntu 24.04)");
        let decoded: Overview = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.machines[1].metrics, metrics);

        control.remove_discovered_machine("gpu-box");
        assert_eq!(control.overview().machines.len(), 1);
        assert!(!control.has_machine("gpu-box"));
    }

    #[test]
    fn owning_node_reports_only_the_running_profiles_configured_modes() {
        let profiles = vec![AgentProfile {
            name: "Pinned".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![
                ProfileMode {
                    id: "sonnet".to_owned(),
                    label: Some("Sonnet".to_owned()),
                    model: "sonnet".to_owned(),
                    effort: None,
                    service_tier: None,
                },
                ProfileMode {
                    id: "fable".to_owned(),
                    label: Some("Fable".to_owned()),
                    model: "fable".to_owned(),
                    effort: None,
                    service_tier: None,
                },
            ],
        }];
        let models = model_capabilities(
            "%1".to_owned(),
            AgentKind::Claude,
            "Pinned",
            crate::tmux::ModelObservation {
                version: Some("2.1.226".to_owned()),
                current: Some("sonnet".to_owned()),
                effort: None,
                fast: None,
                fast_unavailable: false,
                mode: None,
            },
            &profiles,
        );
        assert_eq!(models.harness, "claude");
        assert_eq!(models.current.as_deref(), Some("sonnet"));
        assert_eq!(models.current_mode.as_deref(), Some("sonnet"));
        assert_eq!(models.models.len(), 2);
        assert!(models.models.iter().all(|model| model.switchable));
        assert_eq!(
            models.models.last(),
            Some(&PaneModelOption {
                id: "fable".to_owned(),
                label: "Fable".to_owned(),
                switchable: true,
            })
        );
        assert!(models.note.is_none());
    }

    #[test]
    fn model_effort_and_fast_are_reported_as_independent_owner_scoped_controls() {
        let profiles = vec![AgentProfile {
            name: "Pinned".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![
                ProfileMode {
                    id: "sol-xhigh".to_owned(),
                    label: None,
                    model: "gpt-5.6-sol".to_owned(),
                    effort: Some("xhigh".to_owned()),
                    service_tier: None,
                },
                ProfileMode {
                    id: "sol-high".to_owned(),
                    label: None,
                    model: "gpt-5.6-sol".to_owned(),
                    effort: Some("high".to_owned()),
                    service_tier: None,
                },
                ProfileMode {
                    id: "terra-max".to_owned(),
                    label: None,
                    model: "gpt-5.6-terra".to_owned(),
                    effort: Some("max".to_owned()),
                    service_tier: None,
                },
            ],
        }];
        let models = model_capabilities(
            "%3".to_owned(),
            AgentKind::Codex,
            "Pinned",
            crate::tmux::ModelObservation {
                version: Some("0.153.4".to_owned()),
                current: Some("gpt-5.6-sol".to_owned()),
                effort: Some("xhigh".to_owned()),
                fast: Some(true),
                fast_unavailable: false,
                mode: None,
            },
            &profiles,
        );
        // Each model and effort appears once, never as a combined row.
        assert_eq!(
            models
                .model_options
                .iter()
                .map(|option| (option.id.as_str(), option.switchable))
                .collect::<Vec<_>>(),
            [("gpt-5.6-sol", true), ("gpt-5.6-terra", true)]
        );
        assert_eq!(
            models
                .effort_options
                .iter()
                .map(|option| (option.id.as_str(), option.switchable))
                .collect::<Vec<_>>(),
            // The profile's own levels lead, then the rest of the installed
            // harness's verified slider: every level is already reachable from
            // a running session, so a profile that names none must not leave
            // the reasoning control dead.
            [
                ("xhigh", true),
                ("high", true),
                ("max", false),
                ("low", true),
                ("medium", true)
            ]
        );
        assert!(models.fast_supported);
        assert_eq!(models.fast, Some(true));
        assert_eq!(models.current_mode.as_deref(), Some("sol-xhigh"));

        let older = model_capabilities(
            "%4".to_owned(),
            AgentKind::Codex,
            "Pinned",
            crate::tmux::ModelObservation {
                version: Some("0.147.0".to_owned()),
                current: Some("gpt-5.6-sol".to_owned()),
                effort: None,
                fast: None,
                fast_unavailable: false,
                mode: None,
            },
            &profiles,
        );
        assert!(!older.fast_supported);
        assert!(older.effort_options.iter().any(|option| option.switchable));
    }

    #[test]
    fn claude_offers_its_whole_effort_slider_and_drops_a_refused_fast_mode() {
        // No shipped Claude profile sets `effort`, so deriving the choices from
        // profile modes alone left the reasoning control permanently disabled
        // and every switch rejected as unreported.
        let profiles = vec![AgentProfile {
            name: "Claude".to_owned(),
            harness: "claude".to_owned(),
            command: "claude".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![ProfileMode {
                id: "sonnet".to_owned(),
                label: None,
                model: "sonnet".to_owned(),
                effort: None,
                service_tier: None,
            }],
        }];
        let observed = |fast_unavailable| crate::tmux::ModelObservation {
            version: Some("2.1.261".to_owned()),
            current: Some("sonnet".to_owned()),
            effort: Some("high".to_owned()),
            fast: Some(false),
            fast_unavailable,
            mode: None,
        };
        let models = model_capabilities(
            "%5".to_owned(),
            AgentKind::Claude,
            "Claude",
            observed(false),
            &profiles,
        );
        assert_eq!(
            models
                .effort_options
                .iter()
                .map(|option| (option.id.as_str(), option.switchable))
                .collect::<Vec<_>>(),
            [
                ("low", true),
                ("medium", true),
                ("high", true),
                ("xhigh", true),
                ("max", true)
            ]
        );
        assert!(models.fast_supported);

        // An organization that forbids fast mode makes every press fail, so the
        // control is withdrawn rather than offered and rejected.
        let refused = model_capabilities(
            "%5".to_owned(),
            AgentKind::Claude,
            "Claude",
            observed(true),
            &profiles,
        );
        assert!(!refused.fast_supported);
        assert!(!refused.effort_options.is_empty());
    }

    #[test]
    fn split_switched_panes_still_preflight_for_a_cli_update_relaunch() {
        let mode = ProfileMode {
            id: "sol-xhigh".to_owned(),
            label: None,
            model: "gpt-5.6-sol".to_owned(),
            effort: Some("xhigh".to_owned()),
            service_tier: None,
        };
        let profile = AgentProfile {
            name: "Codex".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![mode.clone()],
        };
        let running = |model: &str, effort: &str, recorded_mode: Option<&str>| {
            crate::tmux::ModelObservation {
                version: Some("0.153.4".to_owned()),
                current: Some(model.to_owned()),
                effort: Some(effort.to_owned()),
                fast: None,
                fast_unavailable: false,
                mode: recorded_mode.map(str::to_owned),
            }
        };
        // A pane still on its configured mode preflights as before.
        assert_eq!(
            preflight_mode(
                &profile,
                &running("gpt-5.6-sol", "xhigh", Some("sol-xhigh")),
                None,
                None
            ),
            Some(mode)
        );

        // A split model switch clears the recorded mode, and the pane's own
        // recorded controls carry the relaunch instead of failing closed.
        let observation = running("gpt-5.6-terra", "high", None);
        let recorded = ProfileMode {
            id: "recorded:gpt-5.6-terra".to_owned(),
            label: None,
            model: "gpt-5.6-terra".to_owned(),
            effort: Some("high".to_owned()),
            service_tier: None,
        };
        assert_eq!(
            preflight_mode(&profile, &observation, Some(recorded.clone()), None),
            Some(recorded.clone())
        );

        // Nothing usable recorded still fails closed, as does a recording the
        // running pane contradicts.
        assert!(preflight_mode(&profile, &observation, None, None).is_none());
        assert!(
            preflight_mode(
                &profile,
                &running("gpt-5.6-luna", "high", None),
                Some(recorded.clone()),
                None
            )
            .is_none()
        );
        assert!(preflight_mode(&profile, &observation, Some(recorded), Some("fast")).is_none());
    }

    #[test]
    fn unsupported_cli_versions_fail_closed_but_keep_configured_models_visible() {
        let profiles = vec![AgentProfile {
            name: "Pinned".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![ProfileMode {
                id: "pinned".to_owned(),
                label: None,
                model: "gpt-5.4".to_owned(),
                effort: Some("xhigh".to_owned()),
                service_tier: None,
            }],
        }];
        let models = model_capabilities(
            "%2".to_owned(),
            AgentKind::Codex,
            "Pinned",
            crate::tmux::ModelObservation {
                version: Some("0.99.0".to_owned()),
                current: Some("gpt-5.4".to_owned()),
                effort: None,
                fast: None,
                fast_unavailable: false,
                mode: None,
            },
            &profiles,
        );
        assert_eq!(models.models.len(), 1);
        assert!(!models.models[0].switchable);
        assert!(models.note.as_deref().unwrap().contains("unsupported"));
    }

    #[test]
    fn claude_resume_capability_never_restarts_working_or_unmatched_panes() {
        let mut pane = session("Claude Code v2.1.227");
        pane.agent = AgentKind::Claude;
        pane.status = AgentStatus::Working;
        let working = claude_resume_capability(&pane, None);
        assert!(!working.available);
        assert!(working.note.unwrap().contains("working"));

        pane.status = AgentStatus::Waiting;
        let unavailable = claude_resume_capability(&pane, None);
        assert!(!unavailable.available);
        assert!(
            unavailable
                .note
                .unwrap()
                .contains("launcher is unavailable")
        );

        let unmatched = claude_resume_capability(&pane, Some(Path::new("/bin/sh")));
        assert!(!unmatched.available);
        assert!(unmatched.note.unwrap().contains("cannot be safely matched"));

        pane.agent = AgentKind::Codex;
        let non_claude = claude_resume_capability(&pane, None);
        assert!(!non_claude.available);
        assert!(non_claude.note.is_none());
    }

    #[test]
    fn codex_restart_capability_requires_the_empty_top_level_prompt() {
        let mut pane = session("OpenAI Codex (v0.153.4)\nmodel: gpt-5.4");
        pane.agent = AgentKind::Codex;
        pane.status = AgentStatus::Waiting;
        let capability = codex_restart_capability(&[], &Config::default().status, &pane);
        assert!(!capability.available);
        assert!(capability.note.unwrap().contains("empty top-level prompt"));
    }

    #[test]
    fn execution_side_launcher_recheck_is_a_conflict() {
        let joined = Ok(Err(anyhow::Error::new(
            crate::tmux::ClaudeResumeUnavailable(
                "the current Claude launcher is unavailable on this machine".to_owned(),
            ),
        )));
        let error = local_agent_restart(joined).unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::Conflict);
    }

    #[test]
    fn fresh_claude_resume_validation_fails_closed_for_working_or_replaced_panes() {
        let mut pane = session("Claude Code v2.1.227");
        pane.agent = AgentKind::Claude;
        pane.status = AgentStatus::Working;
        let working = validate_fresh_claude_resume_session(pane.clone()).unwrap_err();
        assert!(
            working
                .chain()
                .any(<dyn std::error::Error>::is::<ResumeRejected>)
        );

        pane.status = AgentStatus::Waiting;
        pane.agent = AgentKind::Codex;
        let replaced = validate_fresh_claude_resume_session(pane).unwrap_err();
        assert!(
            replaced
                .chain()
                .any(<dyn std::error::Error>::is::<ResumeRejected>)
        );
    }

    #[test]
    fn a_message_mutation_makes_an_earlier_resume_request_stale() {
        let gate = PaneMutationGate::default();
        let resume_generation = gate.generation.load(Ordering::Acquire);
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // This is the same generation advance performed after `send_text`.
        mark_gate_mutated(&gate, &mut state);
        assert!(
            !resume_request_is_current(&state, resume_generation),
            "a resume queued before a message must reject before it re-scans or respawns"
        );
    }

    #[test]
    fn only_the_first_of_two_queued_resume_requests_can_proceed() {
        let gate = PaneMutationGate::default();
        let first_resume = gate.generation.load(Ordering::Acquire);
        let second_resume = gate.generation.load(Ordering::Acquire);
        let mut state = gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        assert!(resume_request_is_current(&state, first_resume));
        // This is the successful first respawn while holding the same gate.
        mark_gate_mutated(&gate, &mut state);
        assert!(
            !resume_request_is_current(&state, second_resume),
            "a second request queued before the first completed must not respawn the new Claude process"
        );
    }

    #[test]
    fn maintenance_accepts_only_native_profile_bindings_not_wrappers() {
        let profile = |command: &str| AgentProfile {
            name: "Default".to_owned(),
            harness: "claude".to_owned(),
            command: command.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };
        let launcher = Path::new("/owner/.local/share/claude/versions/2.1.0");
        assert!(profile_bound_to_native(
            &profile("claude"),
            UpdateHarness::Claude,
            launcher
        ));
        assert!(!profile_bound_to_native(
            &profile("claude-max-wrapper"),
            UpdateHarness::Claude,
            launcher
        ));
        assert!(!profile_bound_to_native(
            &profile("claude"),
            UpdateHarness::Codex,
            launcher
        ));
        assert_eq!(
            maintenance_harness(AgentKind::Claude),
            Some(UpdateHarness::Claude)
        );
        assert_eq!(
            maintenance_harness(AgentKind::Codex),
            Some(UpdateHarness::Codex)
        );
        assert_eq!(
            maintenance_harness(AgentKind::Other),
            None,
            "Other includes Grok and unsupported wrappers and must never be collected"
        );
    }

    #[test]
    fn a_handover_waits_for_a_composer_under_its_status_footer() {
        // Real Claude Code 2.1.261 pane tails. A freshly launched duplicate
        // never ends on a bare prompt glyph any more: the composer carries a
        // dim placeholder and a status strip sits beneath it.
        let idle = concat!(
            "────────────────────────────────\n",
            "❯\u{a0}Try \"fix lint errors\"\n",
            "────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents\n",
        );
        let streaming = concat!(
            "  twenty-nine\n",
            "────────────────────────────────\n",
            "❯\u{a0}\n",
            "────────────────────────────────\n",
            "  ⏸ manual mode on · esc to interrupt · ← for agents\n",
        );
        let drafting = concat!(
            "────────────────────────────────\n",
            "❯\u{a0}hello there\n",
            "────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        let claude = |content: &str| {
            let mut session = test_session("dup", "%7", content);
            session.agent = AgentKind::Claude;
            vec![session]
        };
        let config = crate::config::StatusConfig::default();
        assert_eq!(
            handover_target(&claude(idle), "dup", &config),
            Some("%7".to_owned())
        );
        assert_eq!(handover_target(&claude(streaming), "dup", &config), None);
        assert_eq!(handover_target(&claude(drafting), "dup", &config), None);
        assert_eq!(handover_target(&claude(idle), "other", &config), None);
    }

    #[tokio::test]
    async fn a_duplicate_that_cannot_be_summarized_never_launches() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("atmux-duplicate-summary-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let mut config = Config::default();
        config.general.project_roots = vec![root.clone()];
        config.profiles = vec![AgentProfile {
            name: "max".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        }];
        let control = super::test_control_with_config(&[], config);
        control.apply_refresh(vec![session("working")]);

        let request = |pane: &str| LaunchRequest {
            name: "agent-copy".to_owned(),
            directory: root.to_string_lossy().into_owned(),
            profile_id: "profile-0".to_owned(),
            mode_id: None,
            machine: None,
            resume_session_id: None,
            memory_max_bytes: None,
            summarize_pane_id: Some(pane.to_owned()),
        };
        assert_eq!(
            error_kind(&control.launch(request("%404")).await.unwrap_err()),
            ErrorKind::NotFound,
            "a summary source that is gone must not silently launch a bare duplicate"
        );
        // The fixture pane owns no native log, so no CLI is ever run: the
        // request is refused before tmux is touched.
        let unreadable = control.launch(request("%1")).await.unwrap_err();
        assert_eq!(error_kind(&unreadable), ErrorKind::BadRequest);
        assert!(
            unreadable.to_string().contains("no readable conversation"),
            "{unreadable}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_federated_duplicate_reports_that_it_cannot_summarize_remotely() {
        let control = control_with_machines(&["gpu-box"]);
        control.apply_machine_sessions(
            "gpu-box",
            vec![remote_summary("gpu-box", "%4", "trainer", "aaaa")],
            None,
        );
        let error = control
            .launch(LaunchRequest {
                name: "trainer-copy".to_owned(),
                directory: "/srv".to_owned(),
                profile_id: "profile-0".to_owned(),
                mode_id: None,
                machine: Some("gpu-box".to_owned()),
                resume_session_id: None,
                memory_max_bytes: None,
                summarize_pane_id: Some("%4".to_owned()),
            })
            .await
            .unwrap_err();
        assert_eq!(error_kind(&error), ErrorKind::BadRequest);
        assert!(
            error.to_string().contains("duplicate from that machine"),
            "{error}"
        );
    }

    #[test]
    fn launch_mode_requires_an_explicit_choice_when_a_profile_has_several() {
        let profile = AgentProfile {
            name: "test".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: vec![
                ProfileMode {
                    id: "terra".to_owned(),
                    label: None,
                    model: "gpt-5.6-terra".to_owned(),
                    effort: Some("high".to_owned()),
                    service_tier: None,
                },
                ProfileMode {
                    id: "sol".to_owned(),
                    label: None,
                    model: "gpt-5.6-sol".to_owned(),
                    effort: Some("xhigh".to_owned()),
                    service_tier: Some("fast".to_owned()),
                },
            ],
        };
        assert!(select_launch_mode(&profile, None).is_err());
        assert_eq!(
            select_launch_mode(&profile, Some("sol"))
                .unwrap()
                .unwrap()
                .service_tier
                .as_deref(),
            Some("fast")
        );
    }

    #[test]
    fn saved_session_handles_are_opaque_scoped_and_revalidated() {
        const NATIVE_ID: &str = "11111111-2222-4333-8444-555555555555";
        let hasher = RandomState::new();
        let candidate = crate::old_sessions::ResumeCandidate::fixture(
            crate::old_sessions::ResumeHarness::Claude,
            NATIVE_ID,
        );
        let handle = opaque_resume_id(&hasher, "profile-2", "/work/exact", &candidate);
        assert!(valid_opaque_resume_id(&handle));
        assert!(!handle.contains(NATIVE_ID));
        assert!(!handle.contains("work"));
        assert!(
            resolve_opaque_resume(
                vec![candidate.clone()],
                &hasher,
                "profile-2",
                "/work/exact",
                &handle,
            )
            .is_some()
        );
        assert!(
            resolve_opaque_resume(
                vec![candidate.clone()],
                &hasher,
                "profile-3",
                "/work/exact",
                &handle,
            )
            .is_none()
        );
        assert!(
            resolve_opaque_resume(
                vec![candidate],
                &hasher,
                "profile-2",
                "/work/other",
                &handle,
            )
            .is_none()
        );
        assert!(
            resolve_opaque_resume(Vec::new(), &hasher, "profile-2", "/work/exact", &handle,)
                .is_none(),
            "a disappeared native session must fail closed at launch revalidation"
        );
    }

    #[test]
    fn saved_session_leases_are_stable_opaque_and_scoped() {
        const NATIVE_ID: &str = "11111111-2222-4333-8444-555555555555";
        let candidate = crate::old_sessions::ResumeCandidate::fixture(
            crate::old_sessions::ResumeHarness::Codex,
            NATIVE_ID,
        );
        let mut profile = AgentProfile {
            name: "Work".to_owned(),
            harness: "codex".to_owned(),
            command: "codex".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            inherit_discovered: false,
            claude_relaunch_permissions: None,
            modes: Vec::new(),
        };

        let lease = persistent_resume_lease(&profile, Path::new("/work/exact"), &candidate);
        assert_eq!(lease.len(), "lease-v1-".len() + 64);
        assert!(lease.starts_with("lease-v1-"));
        assert!(!lease.contains(NATIVE_ID));
        assert!(!lease.contains("work"));
        assert_eq!(
            lease,
            persistent_resume_lease(&profile, Path::new("/work/exact"), &candidate)
        );
        assert_ne!(
            lease,
            persistent_resume_lease(&profile, Path::new("/work/other"), &candidate)
        );
        profile.name = "Personal".to_owned();
        assert_ne!(
            lease,
            persistent_resume_lease(&profile, Path::new("/work/exact"), &candidate)
        );
    }

    #[test]
    fn saved_session_lease_state_rejects_duplicates_and_rebuilds_after_refresh() {
        let lease = format!("lease-v1-{}", "a".repeat(64));
        let mut leases = ResumeLeaseState::default();

        assert!(leases.reserve(&lease));
        assert!(
            !leases.reserve(&lease),
            "a concurrent launch must be rejected"
        );
        leases.release(&lease);
        assert!(
            leases.reserve(&lease),
            "a failed launch must release its lease"
        );
        leases.activate(&lease);
        assert!(
            !leases.reserve(&lease),
            "an active pane must reject duplicates"
        );

        let mut resumed = session("resumed");
        resumed.resume_lease = Some(lease.clone());
        let mut after_restart = ResumeLeaseState::default();
        after_restart.observe(&[resumed]);
        assert!(
            !after_restart.reserve(&lease),
            "tmux metadata must rebuild the active lease after restart"
        );
        after_restart.observe(&[]);
        assert!(
            after_restart.reserve(&lease),
            "session deletion must make the conversation resumable again"
        );
    }

    #[test]
    fn saved_session_process_lock_is_single_flight_and_released_on_drop() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::current_dir().unwrap().join(format!(
            ".atmux-resume-lock-test-{}-{nonce}",
            std::process::id()
        ));
        rustix::fs::mkdir(&base, Mode::RWXU).unwrap();
        let runtime = base.join("atmux");
        let lease = format!("lease-v1-{}", "b".repeat(64));
        let other_lease = format!("lease-v1-{}", "c".repeat(64));
        let euid = geteuid().as_raw();

        let first = acquire_persistent_resume_lock_at(&lease, &base, &runtime, euid).unwrap();
        assert!(matches!(
            acquire_persistent_resume_lock_at(&lease, &base, &runtime, euid),
            Err(ResumeLeaseAcquireError::Busy)
        ));
        let unrelated =
            acquire_persistent_resume_lock_at(&other_lease, &base, &runtime, euid).unwrap();
        drop(unrelated);
        drop(first);
        let acquired_after_drop =
            acquire_persistent_resume_lock_at(&lease, &base, &runtime, euid).unwrap();
        drop(acquired_after_drop);
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn cancelled_async_wait_keeps_lease_owned_until_blocking_launch_finishes() {
        let control = local_control();
        let lease = format!("lease-v1-{}", "d".repeat(64));
        {
            let mut leases = control
                .inner
                .resume_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(leases.reserve(&lease));
            leases.hold_process_lock(
                &lease,
                File::open(std::env::current_exe().unwrap()).unwrap(),
            );
        }
        let guard = ResumeLeaseGuard::new(Arc::clone(&control.inner), &lease);
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let (lock, wake) = &*worker_gate;
                let mut ready = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*ready {
                    ready = wake
                        .wait(ready)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                drop(guard);
            })
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .unwrap()
            .unwrap();
        waiting.abort();
        assert!(
            control
                .inner
                .resume_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .in_flight
                .contains(&lease),
            "cancelling the async waiter must not release a detached blocking launch"
        );
        assert!(
            control
                .inner
                .resume_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .process_locks
                .contains_key(&lease),
            "the process lock must remain owned by the detached blocking launch"
        );

        let (lock, wake) = &*gate;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_all();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !control
                    .inner
                    .resume_leases
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .in_flight
                    .contains(&lease)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !control
                .inner
                .resume_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .process_locks
                .contains_key(&lease)
        );
    }
}
