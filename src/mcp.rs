use std::{borrow::Cow, sync::Arc, time::Duration};

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
    },
};
use serde::Deserialize;

use crate::{
    MAX_REQUEST_BODY_BYTES,
    control::{ControlPlane, LaunchRequest, Overview},
};

const MODERN_PROTOCOLS: &[ProtocolVersion] = &[ProtocolVersion::V_2026_07_28];
const DEFAULT_OUTPUT_LINES: usize = 80;
const MAX_OUTPUT_LINES: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentOutputRequest {
    /// Opaque session reference from `agents_list`, or a session name or pane id.
    id: String,
    /// Machine that owns the session. Optional when the reference is already
    /// machine-qualified or the name is unique across machines.
    machine: Option<String>,
    /// A hash returned by an earlier call. Matching output is omitted.
    known_hash: Option<String>,
    /// Maximum tail lines to return. Defaults to 80 and is clamped to 1..=2000.
    max_lines: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentSendRequest {
    /// Opaque session reference from `agents_list`, or a session name or pane id.
    id: String,
    /// Machine that owns the session.
    machine: Option<String>,
    /// Literal text to paste into the agent composer.
    message: String,
    /// Submit the composer after pasting. Defaults to true.
    submit: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AgentIdRequest {
    /// Opaque session reference from `agents_list`, or a session name or pane id.
    id: String,
    /// Machine that owns the session.
    machine: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ObserveRequest {
    /// Last revision observed by the caller. When `machine` is set this is that
    /// machine's own cursor, which is what the previous filtered call returned.
    after_revision: u64,
    /// Long-poll duration in milliseconds (maximum 30000).
    wait_ms: Option<u64>,
    /// Restrict the observation to one machine. The returned revision is then
    /// that machine's own cursor, and unrelated machines never wake the call.
    machine: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AtmuxMcp {
    control: ControlPlane,
    #[cfg(feature = "pulse")]
    pulse: Option<crate::pulse::api::PulseApi>,
    tool_router: ToolRouter<Self>,
}

impl AtmuxMcp {
    fn new(control: ControlPlane) -> Self {
        #[cfg(feature = "pulse")]
        let tool_router = {
            let mut router = Self::tool_router();
            router.merge(Self::pulse_tool_router());
            router
        };
        #[cfg(not(feature = "pulse"))]
        let tool_router = Self::tool_router();
        Self {
            control,
            #[cfg(feature = "pulse")]
            pulse: None,
            tool_router,
        }
    }

    #[cfg(feature = "pulse")]
    fn new_with_pulse(control: ControlPlane, pulse: Option<crate::pulse::api::PulseApi>) -> Self {
        let mut tool_router = Self::tool_router();
        tool_router.merge(Self::pulse_tool_router());
        Self {
            control,
            pulse,
            tool_router,
        }
    }
}

#[cfg(feature = "pulse")]
#[tool_router(router = pulse_tool_router)]
impl AtmuxMcp {
    #[tool(
        name = "pulse_read",
        description = "Read bounded account-scoped Pulse usage, health, history, pace, context, Gemini, reports, profiles, alerts, subscriptions, pricing, limits, or machines. account_id is always explicit; cursors, page sizes, and report days are bounded."
    )]
    async fn pulse_read(
        &self,
        Parameters(request): Parameters<crate::pulse::api::PulseMcpReadRequest>,
    ) -> Result<String, String> {
        let pulse = self
            .pulse
            .as_ref()
            .ok_or_else(|| "Pulse serving is disabled".to_owned())?;
        let response = pulse
            .read_mcp(request)
            .await
            .map_err(|error| pulse_mcp_error(&error))?;
        serde_json::to_string(&response).map_err(|_| "Pulse response failed".to_owned())
    }

    #[tool(
        name = "pulse_mutate",
        description = "Mutate one explicit Pulse account: bounded profile settings/visibility, coalesced account- or profile-scoped force-poll, alert acknowledgement/reply, alert subscriptions, or pricing overrides. Raw secrets, paths, and SQL are not accepted."
    )]
    async fn pulse_mutate(
        &self,
        Parameters(request): Parameters<crate::pulse::api::PulseMcpMutationRequest>,
    ) -> Result<String, String> {
        let pulse = self
            .pulse
            .as_ref()
            .ok_or_else(|| "Pulse serving is disabled".to_owned())?;
        let response = pulse
            .mutate_mcp(request)
            .await
            .map_err(|error| pulse_mcp_error(&error))?;
        serde_json::to_string(&response).map_err(|_| "Pulse response failed".to_owned())
    }
}

#[tool_router]
impl AtmuxMcp {
    #[tool(
        name = "agents_list",
        description = "List compact live agent/session state for every federated machine. Each session carries an opaque machine-qualified id plus its machine, and the machines array reports online/offline health. Save revision and content_hash values for efficient follow-up calls."
    )]
    async fn agents_list(&self) -> Result<String, String> {
        serde_json::to_string(&self.control.overview()).map_err(|error| error.to_string())
    }

    #[tool(
        name = "machines_list",
        description = "List federated machines with their online state, session counts, health, and last contact time. An offline machine never blocks the others."
    )]
    async fn machines_list(&self) -> Result<String, String> {
        serde_json::to_string(&self.control.machines()).map_err(|error| error.to_string())
    }

    #[tool(
        name = "agents_observe",
        description = "Wait efficiently for agent state to change. Without machine, observes the whole federation on the shared revision. With machine, observes only that machine on its own revision, so other machines never return a change. A change returns the current overview; an unchanged timeout returns only changed=false and revision. Each call is stateless; pass the revision the previous call returned."
    )]
    async fn agents_observe(
        &self,
        Parameters(request): Parameters<ObserveRequest>,
    ) -> Result<String, String> {
        let wait = Duration::from_millis(request.wait_ms.unwrap_or(20_000).min(30_000));
        let Some(machine) = request.machine.as_deref() else {
            if self.control.overview().revision <= request.after_revision {
                let _ = self
                    .control
                    .wait_for_revision(request.after_revision, wait)
                    .await;
            }
            let overview = self.control.overview();
            return observation_json(overview.revision > request.after_revision, &overview);
        };
        // Reject an unknown machine immediately. Waiting first would make a
        // typo cost the caller a full long-poll before the error appears.
        if !self.control.has_machine(machine) {
            return Err(format!(
                "unknown machine {machine}; call machines_list for valid ids"
            ));
        }
        if self
            .control
            .machine_revision(machine)
            .is_some_and(|revision| revision <= request.after_revision)
        {
            let _ = self
                .control
                .wait_for_machine_revision(machine, request.after_revision, wait)
                .await;
        }
        let overview = self
            .control
            .machine_overview(machine)
            .ok_or_else(|| format!("unknown machine {machine}"))?;
        observation_json(overview.revision > request.after_revision, &overview)
    }

    #[tool(
        name = "agent_output",
        description = "Read a tail of one agent pane bounded to 1..=2000 lines and 64 KiB. Pass known_hash from a previous response; unchanged output is returned without content."
    )]
    async fn agent_output(
        &self,
        Parameters(request): Parameters<AgentOutputRequest>,
    ) -> Result<String, String> {
        let reference = self
            .control
            .reference(&request.id, request.machine.as_deref())
            .map_err(|error| error.to_string())?;
        let output = self
            .control
            .pane_output(
                &reference,
                request.known_hash.as_deref(),
                output_line_limit(request.max_lines),
            )
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("no agent pane matches {}", request.id))?;
        serde_json::to_string(&output).map_err(|error| error.to_string())
    }

    #[tool(
        name = "agent_send",
        description = "Send a literal message to an existing agent pane and normally submit it. Use this to communicate with another agent."
    )]
    async fn agent_send(
        &self,
        Parameters(request): Parameters<AgentSendRequest>,
    ) -> Result<String, String> {
        let reference = self
            .control
            .reference(&request.id, request.machine.as_deref())
            .map_err(|error| error.to_string())?;
        self.control
            .send_text(&reference, request.message, request.submit.unwrap_or(true))
            .await
            .map_err(|error| error.to_string())?;
        Ok(format!("message sent to {reference}"))
    }

    #[tool(
        name = "agent_interrupt",
        description = "Interrupt the current operation in an agent pane by sending Escape."
    )]
    async fn agent_interrupt(
        &self,
        Parameters(request): Parameters<AgentIdRequest>,
    ) -> Result<String, String> {
        let reference = self
            .control
            .reference(&request.id, request.machine.as_deref())
            .map_err(|error| error.to_string())?;
        self.control
            .interrupt(&reference)
            .await
            .map_err(|error| error.to_string())?;
        Ok(format!("interrupt sent to {reference}"))
    }

    #[tool(
        name = "agent_launch",
        description = "Launch an allowlisted atmux profile and project on a chosen machine. Get valid machine, directory, profile_id, and optional memory_max_bytes values from agents_launch_options; omitting machine uses the coordinator's own tmux server. The owning machine revalidates every memory limit."
    )]
    async fn agent_launch(
        &self,
        Parameters(request): Parameters<LaunchRequest>,
    ) -> Result<String, String> {
        let name = format!(
            "{} on {}",
            request.name,
            request
                .machine
                .as_deref()
                .unwrap_or(self.control.local_id())
        );
        self.control
            .launch(request)
            .await
            .map_err(|error| error.to_string())?;
        Ok(format!("launched {name}"))
    }

    #[tool(
        name = "agents_launch_options",
        description = "List per-machine project directories, agent profiles, and bounded memory choices accepted by agent_launch. Offline machines report a note instead of inputs."
    )]
    async fn agents_launch_options(&self) -> Result<String, String> {
        serde_json::to_string(&self.control.launch_options()).map_err(|error| error.to_string())
    }

    #[tool(
        name = "agent_stop",
        description = "Permanently kill one tmux agent session. This terminates the running agent process."
    )]
    async fn agent_stop(
        &self,
        Parameters(request): Parameters<AgentIdRequest>,
    ) -> Result<String, String> {
        let reference = self
            .control
            .reference(&request.id, request.machine.as_deref())
            .map_err(|error| error.to_string())?;
        self.control
            .kill(&reference)
            .await
            .map_err(|error| error.to_string())?;
        Ok(format!("stopped {reference}"))
    }
}

#[cfg(feature = "pulse")]
fn pulse_mcp_error(error: &crate::pulse::PulseError) -> String {
    format!("{}: {}", pulse_error_kind(error.kind()), error.message())
}

#[cfg(feature = "pulse")]
const fn pulse_error_kind(kind: crate::pulse::PulseErrorKind) -> &'static str {
    use crate::pulse::PulseErrorKind;
    match kind {
        PulseErrorKind::InvalidInput => "invalid_input",
        PulseErrorKind::NotFound => "not_found",
        PulseErrorKind::Conflict => "conflict",
        PulseErrorKind::Offline => "offline",
        PulseErrorKind::Authentication => "authentication",
        PulseErrorKind::RateLimited => "rate_limited",
        PulseErrorKind::Upstream => "upstream",
        PulseErrorKind::Storage => "storage",
        PulseErrorKind::Configuration => "configuration",
        PulseErrorKind::Internal => "internal",
    }
}

fn output_line_limit(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(DEFAULT_OUTPUT_LINES)
        .clamp(1, MAX_OUTPUT_LINES)
}

fn observation_json(changed: bool, overview: &Overview) -> Result<String, String> {
    let value = if changed {
        serde_json::json!({
            "changed": true,
            "overview": overview,
        })
    } else {
        // A timed-out long poll is common. Avoid repeating the complete session
        // inventory when its revision did not advance.
        serde_json::json!({
            "changed": false,
            "revision": overview.revision,
        })
    };
    serde_json::to_string(&value).map_err(|error| error.to_string())
}

// `tool_handler` generates an async trait method without an await point. Keep
// the exception local to the macro expansion until rmcp emits a ready future.
#[allow(unknown_lints, clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for AtmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("atmux", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Control tmux coding agents across every federated machine efficiently. Start with agents_list, treat each session id as an opaque machine-qualified reference, retain revision/content_hash cursors, use agents_observe for bounded long-polling, and use agent_output only for agents whose hash changed. One machine being offline never blocks the others.",
            )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(MODERN_PROTOCOLS)
    }
}

#[must_use]
pub fn service(
    control: ControlPlane,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> StreamableHttpService<AtmuxMcp, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_stateless_protocol_metadata_required(true)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_allowed_hosts(allowed_hosts.iter().cloned())
        .with_allowed_origins(allowed_origins.iter().cloned())
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES);
    StreamableHttpService::new(
        move || Ok(AtmuxMcp::new(control.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

#[cfg(feature = "pulse")]
#[must_use]
pub fn service_with_pulse(
    control: ControlPlane,
    pulse: Option<crate::pulse::api::PulseApi>,
    allowed_hosts: &[String],
    allowed_origins: &[String],
) -> StreamableHttpService<AtmuxMcp, NeverSessionManager> {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_stateless_protocol_metadata_required(true)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_allowed_hosts(allowed_hosts.iter().cloned())
        .with_allowed_origins(allowed_origins.iter().cloned())
        .with_max_request_body_bytes(MAX_REQUEST_BODY_BYTES);
    StreamableHttpService::new(
        move || Ok(AtmuxMcp::new_with_pulse(control.clone(), pulse.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_line_contract_defaults_and_clamps() {
        assert_eq!(output_line_limit(None), 80);
        assert_eq!(output_line_limit(Some(0)), 1);
        assert_eq!(output_line_limit(Some(2_001)), 2_000);
    }

    #[test]
    fn unchanged_observation_is_compact() {
        let overview = Overview {
            revision: 7,
            sessions: Vec::new(),
            health: Some("would be expensive to repeat".to_owned()),
            machines: Vec::new(),
        };
        let value: serde_json::Value =
            serde_json::from_str(&observation_json(false, &overview).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"changed": false, "revision": 7}));
        assert!(value.get("overview").is_none());

        let changed: serde_json::Value =
            serde_json::from_str(&observation_json(true, &overview).unwrap()).unwrap();
        assert_eq!(changed["changed"], true);
        assert_eq!(changed["overview"]["revision"], 7);
    }

    #[cfg(feature = "pulse")]
    #[tokio::test]
    async fn pulse_tools_bind_every_request_to_an_explicit_configured_account() {
        use crate::{
            control::test_control,
            pulse::{
                Account, AccountId,
                api::{PulseApi, PulseCapabilities, PulseMcpReadRequest, PulseReadResource},
                store::{SqliteStore, Store},
            },
        };

        let directory = std::env::temp_dir().join(format!(
            "atmux-pulse-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = directory.join("pulse.sqlite3");
        let store = Arc::new(SqliteStore::open(&path).await.unwrap());
        let account = AccountId::new(7).unwrap();
        store
            .upsert_account(Account {
                id: account,
                identity: "operator@example.test".to_owned(),
                display_name: None,
            })
            .await
            .unwrap();
        let pulse = PulseApi::new(
            store.clone(),
            &[account],
            PulseCapabilities {
                collect: false,
                serve: true,
                receive: false,
            },
        );
        let mcp = AtmuxMcp::new_with_pulse(test_control(&[]), Some(pulse));
        let request = |account_id| PulseMcpReadRequest {
            account_id,
            resource: PulseReadResource::Limits,
            profile: None,
            machine: None,
            since: None,
            through_day: None,
            days: None,
            granularity: None,
            drill: None,
            acknowledged: None,
            alert_id: None,
            cursor: None,
            limit: None,
        };
        let limits: serde_json::Value =
            serde_json::from_str(&mcp.pulse_read(Parameters(request(7))).await.unwrap()).unwrap();
        assert_eq!(limits["capabilities"]["serve"], true);
        let error = mcp.pulse_read(Parameters(request(8))).await.unwrap_err();
        assert!(error.starts_with("not_found:"), "{error}");

        let disabled = AtmuxMcp::new(test_control(&[]));
        assert_eq!(
            disabled
                .pulse_read(Parameters(request(7)))
                .await
                .unwrap_err(),
            "Pulse serving is disabled"
        );
        drop(mcp);
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let _ = std::fs::remove_dir(directory);
    }

    fn federated_overview() -> Overview {
        use crate::{
            control::SessionSummary,
            machine::{MachineKind, MachineSummary},
        };

        let session = |machine: &str, pane: &str, name: &str| SessionSummary {
            id: format!("{machine}~{pane}"),
            machine: machine.to_owned(),
            name: name.to_owned(),
            pane_id: pane.to_owned(),
            status: "working".to_owned(),
            agent: "codex".to_owned(),
            profile: "Default".to_owned(),
            attached: false,
            activity: 1,
            path: "/tmp".to_owned(),
            title: name.to_owned(),
            command: "codex".to_owned(),
            launch_command: "codex".to_owned(),
            systemd_scope: None,
            memory_max_bytes: None,
            windows: 1,
            window_index: 0,
            pane_index: 0,
            content_hash: "aaaa".to_owned(),
        };
        let machine = |id: &str, kind, online| MachineSummary {
            id: id.to_owned(),
            label: id.to_owned(),
            kind,
            online,
            sessions: 1,
            health: (!online).then(|| "connection refused".to_owned()),
            last_seen_ms: None,
            address: (kind == MachineKind::Remote).then(|| format!("{id}:7345")),
            metrics: crate::metrics::MachineMetrics::default(),
        };
        Overview {
            revision: 9,
            sessions: vec![
                session("local", "%1", "alpha"),
                session("gpu-box", "%4", "beta"),
            ],
            health: None,
            machines: vec![
                machine("local", MachineKind::Local, true),
                machine("gpu-box", MachineKind::Remote, true),
                machine("mini", MachineKind::Remote, false),
            ],
        }
    }

    #[test]
    fn mcp_payloads_expose_machine_qualified_references_and_health() {
        let overview = federated_overview();
        let value: serde_json::Value =
            serde_json::from_str(&observation_json(true, &overview).unwrap()).unwrap();
        let overview_json = &value["overview"];

        // Every session carries an opaque reference plus its owning machine.
        for session in overview_json["sessions"].as_array().unwrap() {
            let id = session["id"].as_str().unwrap();
            let machine = session["machine"].as_str().unwrap();
            assert!(id.starts_with(&format!("{machine}~")), "{id}");
            // Summaries must stay output-free; only a hash cursor is exposed.
            assert!(session.get("content").is_none());
            assert!(session["content_hash"].is_string());
        }
        assert_eq!(overview_json["sessions"][1]["id"], "gpu-box~%4");

        // Machine health is discoverable without a second round trip.
        let machines = overview_json["machines"].as_array().unwrap();
        assert_eq!(machines.len(), 3);
        assert_eq!(machines[0]["kind"], "local");
        assert_eq!(machines[2]["online"], false);
        assert_eq!(machines[2]["health"], "connection refused");
        assert_eq!(machines[1]["address"], "gpu-box:7345");

        // No credential can reach an MCP client.
        let encoded = serde_json::to_string(&overview).unwrap();
        assert!(!encoded.contains("token"), "{encoded}");
        assert!(!encoded.contains("Bearer"), "{encoded}");
    }

    #[test]
    fn machine_filtered_observation_narrows_sessions_without_hiding_the_revision() {
        let mut overview = federated_overview();
        overview
            .sessions
            .retain(|session| session.machine == "gpu-box");
        overview.machines.retain(|machine| machine.id == "gpu-box");
        let value: serde_json::Value =
            serde_json::from_str(&observation_json(true, &overview).unwrap()).unwrap();
        assert_eq!(value["overview"]["revision"], 9);
        assert_eq!(value["overview"]["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(value["overview"]["machines"].as_array().unwrap().len(), 1);
    }

    async fn observe(mcp: &AtmuxMcp, machine: Option<&str>, after: u64) -> serde_json::Value {
        let request = ObserveRequest {
            after_revision: after,
            wait_ms: Some(60),
            machine: machine.map(str::to_owned),
        };
        serde_json::from_str(&mcp.agents_observe(Parameters(request)).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_machine_scoped_observation_ignores_every_other_machine() {
        use crate::control::test_control;

        let control = test_control(&["gpu-box", "mini"]);
        let mcp = AtmuxMcp::new(control.clone());

        // An unknown machine is rejected before the caller pays for a wait.
        let started = std::time::Instant::now();
        let error = mcp
            .agents_observe(Parameters(ObserveRequest {
                after_revision: 0,
                wait_ms: Some(20_000),
                machine: Some("ghost".to_owned()),
            }))
            .await
            .unwrap_err();
        assert!(error.contains("unknown machine ghost"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an unknown machine must not be validated only after waiting"
        );

        // gpu-box changes; a mini observer must time out compact, not receive a
        // filtered payload it has already seen.
        control.apply_machine_sessions(
            "gpu-box",
            vec![crate::control::SessionSummary {
                id: "gpu-box~%4".to_owned(),
                machine: "gpu-box".to_owned(),
                name: "trainer".to_owned(),
                pane_id: "%4".to_owned(),
                status: "working".to_owned(),
                agent: "claude".to_owned(),
                profile: "claude-max".to_owned(),
                attached: false,
                activity: 1,
                path: "/srv".to_owned(),
                title: "trainer".to_owned(),
                command: "claude".to_owned(),
                launch_command: "claude".to_owned(),
                systemd_scope: None,
                memory_max_bytes: None,
                windows: 1,
                window_index: 0,
                pane_index: 0,
                content_hash: "aaaa".to_owned(),
            }],
            None,
        );

        let quiet = observe(&mcp, Some("mini"), 0).await;
        assert_eq!(quiet, serde_json::json!({"changed": false, "revision": 0}));
        assert!(quiet.get("overview").is_none());

        // The machine that did change reports it, scoped to itself.
        let changed = observe(&mcp, Some("gpu-box"), 0).await;
        assert_eq!(changed["changed"], true);
        assert_eq!(changed["overview"]["machines"].as_array().unwrap().len(), 1);
        assert_eq!(changed["overview"]["machines"][0]["id"], "gpu-box");
        assert_eq!(changed["overview"]["sessions"].as_array().unwrap().len(), 1);
        let gpu_revision = changed["overview"]["revision"].as_u64().unwrap();
        assert!(gpu_revision > 0);

        // A second call at the returned cursor is quiet again.
        assert_eq!(
            observe(&mcp, Some("gpu-box"), gpu_revision).await,
            serde_json::json!({"changed": false, "revision": gpu_revision})
        );

        // mini's own change wakes a blocked mini observer.
        let waiter = {
            let mcp = mcp.clone();
            tokio::spawn(async move {
                serde_json::from_str::<serde_json::Value>(
                    &mcp.agents_observe(Parameters(ObserveRequest {
                        after_revision: 0,
                        wait_ms: Some(5_000),
                        machine: Some("mini".to_owned()),
                    }))
                    .await
                    .unwrap(),
                )
                .unwrap()
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        control.mark_machine_offline("mini", "connection refused");
        let woken = waiter.await.unwrap();
        assert_eq!(woken["changed"], true);
        assert_eq!(woken["overview"]["machines"][0]["id"], "mini");
        assert_eq!(woken["overview"]["machines"][0]["online"], false);

        // The unfiltered observer still sees the whole federation.
        let global = observe(&mcp, None, 0).await;
        assert_eq!(global["changed"], true);
        assert_eq!(global["overview"]["machines"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn launch_request_defaults_to_the_local_machine_for_older_callers() {
        let request: LaunchRequest = serde_json::from_str(
            r#"{"name":"review","directory":"/tmp","profile_id":"profile-0"}"#,
        )
        .unwrap();
        assert!(request.machine.is_none());
        assert!(request.memory_max_bytes.is_none());

        let targeted: LaunchRequest = serde_json::from_str(
            r#"{"name":"review","directory":"/tmp","profile_id":"profile-0","machine":"gpu-box"}"#,
        )
        .unwrap();
        assert_eq!(targeted.machine.as_deref(), Some("gpu-box"));
    }

    #[test]
    fn tool_requests_accept_an_optional_machine_selector() {
        let bare: AgentIdRequest = serde_json::from_str(r#"{"id":"%1"}"#).unwrap();
        assert!(bare.machine.is_none());
        let scoped: AgentIdRequest =
            serde_json::from_str(r#"{"id":"%1","machine":"gpu-box"}"#).unwrap();
        assert_eq!(scoped.machine.as_deref(), Some("gpu-box"));

        let output: AgentOutputRequest =
            serde_json::from_str(r#"{"id":"gpu-box~%4","known_hash":"aaaa"}"#).unwrap();
        assert_eq!(output.id, "gpu-box~%4");
        assert_eq!(output.known_hash.as_deref(), Some("aaaa"));

        let observe: ObserveRequest =
            serde_json::from_str(r#"{"after_revision":3,"machine":"mini"}"#).unwrap();
        assert_eq!(observe.machine.as_deref(), Some("mini"));
        assert!(observe.wait_ms.is_none());
    }
}
