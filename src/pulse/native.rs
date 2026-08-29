//! Production adapters that drive the pure Pulse collectors on this machine.
//!
//! Only explicitly configured local profiles enter this module. Provider
//! credentials remain in the collector/transport boundary and every persisted
//! value is a typed, secret-free Pulse domain row.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
};

use hyper::Method;
use serde::Deserialize;
use tokio::{sync::Semaphore, task::JoinSet};

use super::{
    CollectionOutcome, ContextSession, GeminiQuota, Instant, MachineName, Profile, ProfileOrigin,
    PulseError, PulseErrorKind, PulseResult, RefreshPolicy, TokenGrain, UsageSnapshot, Vendor,
    collect::{
        HttpResponse, HttpsJsonClient, SecretRef,
        claude::{ClaudeAction, ClaudeCollector, ClaudeHttpMethod, ClaudeRequest, ClaudeResponse},
        codex::{
            CodexAction, CodexCollector, CodexLiveResponse, DiscoveryLimits,
            collect_rollout_fallback,
        },
        deepseek::DeepSeekCollector,
        gemini::{GeminiCollector, GeminiConfig},
        grok::{GrokCollector, collect_transcript_usage},
    },
    config::PulseCredentialConfig,
    context::{DEFAULT_CONTEXT_MAX_AGE, collect_profile_contexts},
    credentials::{
        ClaudeOauthTokens, RefreshGrant, RefreshOptions, RefreshResult, SecretString,
        cooperative_refresh_with, read_claude_credentials, read_codex_credentials,
    },
    service::{
        Collected, CollectionFuture, CompletionFuture, PulseCollectors, TokenCollectionRequest,
    },
    token::{TallyOptions, tally_profile},
};

const MAX_PROVIDER_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_PROFILES: usize = 8;
const MAX_NATIVE_COLLECTION_ITEMS: usize = 10_000;
const ANTHROPIC_TOKEN_ENDPOINT: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const GROK_CLIENT_VERSION: &str = "1.0.0";
const GEMINI_OAUTH_FILE: &str = "oauth_creds.json";
const MAX_PROFILE_STATE_ENTRIES: usize = 1_024;

type TransportFuture =
    Pin<Box<dyn Future<Output = PulseResult<TransportResponse>> + Send + 'static>>;

#[derive(Clone)]
struct TransportRequest {
    method: Method,
    endpoint: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    content_type: Option<String>,
}

impl std::fmt::Debug for TransportRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportRequest")
            .field("method", &self.method)
            .field("endpoint", &self.endpoint)
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .field("content_type", &self.content_type)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TransportResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl std::fmt::Debug for TransportResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

trait ProviderTransport: Send + Sync + 'static {
    fn request(&self, request: TransportRequest) -> TransportFuture;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProfileStateKey {
    account_id: super::AccountId,
    name: super::ProfileName,
    vendor: Vendor,
    config_dir: Option<PathBuf>,
}

impl From<&Profile> for ProfileStateKey {
    fn from(profile: &Profile) -> Self {
        Self {
            account_id: profile.account_id,
            name: profile.name.clone(),
            vendor: profile.vendor,
            config_dir: profile.config_dir.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum NativePollKind {
    Usage,
    Gemini,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PollKey {
    profile: ProfileStateKey,
    kind: NativePollKind,
}

struct PollState {
    interval_millis: i64,
    last_completed_millis: Option<i64>,
    in_flight_generation: Option<u64>,
}

struct ClaudeProfileTokens {
    authoritative: ClaudeOauthTokens,
    active: ClaudeOauthTokens,
}

#[derive(Default)]
struct NativeState {
    polls: HashMap<PollKey, PollState>,
    claude_tokens: HashMap<ProfileStateKey, ClaudeProfileTokens>,
    gemini_collectors: HashMap<ProfileStateKey, Arc<GeminiCollector>>,
    next_generation: u64,
}

struct PollLease {
    state: Arc<Mutex<NativeState>>,
    key: PollKey,
    generation: u64,
    completed_at_millis: Option<i64>,
}

impl PollLease {
    fn complete(mut self, completed_at: Instant) {
        self.completed_at_millis = Some(completed_at.epoch_millis());
    }
}

impl Drop for PollLease {
    fn drop(&mut self) {
        let mut state = lock_native_state(&self.state);
        let Some(poll) = state.polls.get_mut(&self.key) else {
            return;
        };
        if poll.in_flight_generation != Some(self.generation) {
            return;
        }
        poll.in_flight_generation = None;
        if let Some(completed_at) = self.completed_at_millis {
            poll.last_completed_millis = Some(completed_at);
        }
    }
}

fn lock_native_state(state: &Mutex<NativeState>) -> MutexGuard<'_, NativeState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug)]
struct NativeHttpsTransport {
    client: HttpsJsonClient,
}

impl NativeHttpsTransport {
    fn new() -> PulseResult<Self> {
        Ok(Self {
            client: HttpsJsonClient::new(MAX_PROVIDER_RESPONSE_BYTES)?,
        })
    }
}

impl ProviderTransport for NativeHttpsTransport {
    fn request(&self, request: TransportRequest) -> TransportFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let headers = request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.clone()))
                .collect::<Vec<_>>();
            let response = client
                .request(
                    request.method,
                    &request.endpoint,
                    &headers,
                    request.body,
                    request.content_type.as_deref(),
                )
                .await?;
            Ok(project_http_response(response))
        })
    }
}

fn project_http_response(response: HttpResponse) -> TransportResponse {
    TransportResponse {
        status: response.status.as_u16(),
        headers: response.headers,
        body: response.body,
    }
}

/// Native collector set owned by the embedded atmux web process.
#[derive(Clone)]
pub struct NativeCollectors {
    machine: MachineName,
    credentials: PulseCredentialConfig,
    transport: Arc<dyn ProviderTransport>,
    concurrency: usize,
    state: Arc<Mutex<NativeState>>,
}

impl std::fmt::Debug for NativeCollectors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeCollectors")
            .field("machine", &self.machine)
            .field("credentials", &self.credentials)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

impl NativeCollectors {
    /// Creates the certificate-validating production transport.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when no usable system TLS roots exist.
    pub fn new(machine: MachineName, credentials: PulseCredentialConfig) -> PulseResult<Self> {
        Ok(Self {
            machine,
            credentials,
            transport: Arc::new(NativeHttpsTransport::new()?),
            concurrency: MAX_CONCURRENT_PROFILES,
            state: Arc::new(Mutex::new(NativeState::default())),
        })
    }

    #[cfg(test)]
    fn with_transport(
        machine: MachineName,
        credentials: PulseCredentialConfig,
        transport: Arc<dyn ProviderTransport>,
    ) -> Self {
        Self {
            machine,
            credentials,
            transport,
            concurrency: 2,
            state: Arc::new(Mutex::new(NativeState::default())),
        }
    }

    fn take_due_profiles(
        &self,
        profiles: Vec<Profile>,
        kind: NativePollKind,
        collected_at: Instant,
    ) -> (Vec<(Profile, PollLease)>, usize) {
        let keyed = profiles
            .into_iter()
            .map(|profile| (ProfileStateKey::from(&profile), profile))
            .collect::<Vec<_>>();
        let active = keyed
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<HashSet<_>>();
        let mut state = lock_native_state(&self.state);
        state.polls.retain(|key, poll| {
            key.kind != kind || active.contains(&key.profile) || poll.in_flight_generation.is_some()
        });
        let mut due = Vec::new();
        let mut failures = 0_usize;
        for (profile_key, profile) in keyed {
            let Some(interval_millis) = i64::from(profile.poll_interval_minutes)
                .checked_mul(60 * 1_000)
                .filter(|value| *value > 0)
            else {
                failures = failures.saturating_add(1);
                continue;
            };
            let poll_key = PollKey {
                profile: profile_key,
                kind,
            };
            let kind_entries = state
                .polls
                .keys()
                .filter(|existing| existing.kind == kind)
                .count();
            if !state.polls.contains_key(&poll_key) && kind_entries >= MAX_PROFILE_STATE_ENTRIES {
                failures = failures.saturating_add(1);
                continue;
            }
            let poll = state.polls.entry(poll_key.clone()).or_insert(PollState {
                interval_millis,
                last_completed_millis: None,
                in_flight_generation: None,
            });
            poll.interval_millis = interval_millis;
            let is_due = poll.last_completed_millis.is_none_or(|last| {
                collected_at.epoch_millis() >= last.saturating_add(interval_millis)
            });
            if !is_due || poll.in_flight_generation.is_some() {
                continue;
            }
            state.next_generation = state.next_generation.saturating_add(1).max(1);
            let generation = state.next_generation;
            state
                .polls
                .get_mut(&poll_key)
                .expect("poll entry was inserted")
                .in_flight_generation = Some(generation);
            due.push((
                profile,
                PollLease {
                    state: Arc::clone(&self.state),
                    key: poll_key,
                    generation,
                    completed_at_millis: None,
                },
            ));
        }
        (due, failures)
    }

    /// Marks only the supplied profile/kind keys immediately due. Existing
    /// in-flight leases remain authoritative, so a force request can never
    /// duplicate provider work already running.
    fn force_profiles_due(&self, profiles: &[Profile], kind: NativePollKind) {
        let keys = profiles
            .iter()
            .map(ProfileStateKey::from)
            .collect::<HashSet<_>>();
        let mut state = lock_native_state(&self.state);
        for (key, poll) in &mut state.polls {
            if key.kind == kind
                && keys.contains(&key.profile)
                && poll.in_flight_generation.is_none()
            {
                poll.last_completed_millis = None;
            }
        }
    }

    fn reconcile_claude_token_profiles(&self, profiles: &[Profile]) {
        let active = profiles
            .iter()
            .filter(|profile| profile.vendor == Vendor::AnthropicOauth)
            .map(ProfileStateKey::from)
            .collect::<HashSet<_>>();
        lock_native_state(&self.state)
            .claude_tokens
            .retain(|key, _| active.contains(key));
    }

    fn prepare_claude_tokens(
        &self,
        key: &ProfileStateKey,
        observed: ClaudeOauthTokens,
    ) -> (ClaudeOauthTokens, ClaudeOauthTokens) {
        let mut state = lock_native_state(&self.state);
        if state.claude_tokens.len() >= MAX_PROFILE_STATE_ENTRIES
            && !state.claude_tokens.contains_key(key)
        {
            return (observed.clone(), observed);
        }
        let entry = state
            .claude_tokens
            .entry(key.clone())
            .or_insert_with(|| ClaudeProfileTokens {
                authoritative: observed.clone(),
                active: observed.clone(),
            });
        if entry.authoritative != observed {
            entry.authoritative = observed.clone();
            entry.active = observed;
        }
        (entry.active.clone(), entry.authoritative.clone())
    }

    fn record_claude_refresh(&self, key: &ProfileStateKey, refreshed: &RefreshResult) {
        let mut state = lock_native_state(&self.state);
        if state.claude_tokens.len() >= MAX_PROFILE_STATE_ENTRIES
            && !state.claude_tokens.contains_key(key)
        {
            return;
        }
        state.claude_tokens.insert(
            key.clone(),
            ClaudeProfileTokens {
                // Every grant is persisted before it is returned. An adopted
                // value was just re-read under the same profile lock.
                authoritative: refreshed.tokens.clone(),
                active: refreshed.tokens.clone(),
            },
        );
    }

    fn reconcile_gemini_collectors(&self, profiles: &[Profile]) {
        let active = profiles
            .iter()
            .filter(|profile| profile.vendor == Vendor::Gemini)
            .map(ProfileStateKey::from)
            .collect::<HashSet<_>>();
        lock_native_state(&self.state)
            .gemini_collectors
            .retain(|key, _| active.contains(key));
    }

    fn gemini_collector(
        &self,
        key: &ProfileStateKey,
        config: GeminiConfig,
    ) -> PulseResult<Arc<GeminiCollector>> {
        if let Some(collector) = lock_native_state(&self.state)
            .gemini_collectors
            .get(key)
            .cloned()
        {
            return Ok(collector);
        }
        let collector = Arc::new(GeminiCollector::new(config)?);
        let mut state = lock_native_state(&self.state);
        if let Some(existing) = state.gemini_collectors.get(key) {
            return Ok(Arc::clone(existing));
        }
        if state.gemini_collectors.len() >= MAX_PROFILE_STATE_ENTRIES {
            return Err(PulseError::configuration(
                "too many active Gemini collector profiles",
            ));
        }
        state
            .gemini_collectors
            .insert(key.clone(), Arc::clone(&collector));
        Ok(collector)
    }

    async fn collect_usage(
        self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> Collected<UsageSnapshot> {
        let profiles = profiles
            .into_iter()
            .filter(is_local_usage_profile)
            .collect::<Vec<_>>();
        self.reconcile_claude_token_profiles(&profiles);
        let (profiles, mut failures) =
            self.take_due_profiles(profiles, NativePollKind::Usage, collected_at);
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut tasks = JoinSet::new();
        for (profile, lease) in profiles {
            let collector = self.clone();
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(|_| ())?;
                let snapshot = collector.collect_usage_profile(profile, collected_at).await;
                lease.complete(collected_at);
                Ok::<_, ()>(snapshot)
            });
        }
        let mut items = Vec::new();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(snapshot)) => items.push(snapshot),
                Ok(Err(())) | Err(_) => failures = failures.saturating_add(1),
            }
        }
        items.sort_by(|left, right| left.profile.as_str().cmp(right.profile.as_str()));
        capped_collection(items, failures)
    }

    async fn collect_usage_profile(
        &self,
        profile: Profile,
        collected_at: Instant,
    ) -> UsageSnapshot {
        match profile.vendor {
            Vendor::AnthropicOauth => self.collect_claude(profile, collected_at).await,
            Vendor::OpenaiCodex => self.collect_codex(profile, collected_at).await,
            Vendor::DeepseekBalance => self.collect_deepseek(profile, collected_at).await,
            Vendor::XaiGrok => self.collect_grok(profile, collected_at).await,
            Vendor::Gemini | Vendor::Antigravity => unreachable!("filtered usage vendor"),
        }
    }

    async fn collect_claude(&self, profile: Profile, collected_at: Instant) -> UsageSnapshot {
        let Some(config_dir) = profile.config_dir.clone() else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Configuration,
            );
        };
        let tokens = match read_claude_tokens_async(config_dir.clone()).await {
            Ok(tokens) => tokens,
            Err(error) => {
                return failure_snapshot(&profile, &self.machine, collected_at, error.kind());
            }
        };
        let profile_key = ProfileStateKey::from(&profile);
        let (tokens, rejected) = self.prepare_claude_tokens(&profile_key, tokens);
        let mut collector =
            ClaudeCollector::new(tokens, self.credentials.anthropic_inference_fallback);
        let mut action = collector.start();
        let mut retry_at = None;
        for _ in 0..8 {
            action = match action {
                ClaudeAction::Request(request) => match self.send_claude_request(request).await {
                    Ok(response) => {
                        if response.status == 429 {
                            retry_at = provider_retry_at(&response.headers, collected_at);
                        }
                        collector.handle_response(&response)
                    }
                    Err(error) => {
                        return failure_snapshot_with_retry(
                            &profile,
                            &self.machine,
                            collected_at,
                            error.kind(),
                            None,
                        );
                    }
                },
                ClaudeAction::RefreshRequired => {
                    match self
                        .refresh_claude(
                            &config_dir,
                            rejected.clone(),
                            profile.refresh,
                            collected_at,
                        )
                        .await
                    {
                        Ok(refreshed) => {
                            self.record_claude_refresh(&profile_key, &refreshed);
                            collector.resume_after_refresh(refreshed.tokens)
                        }
                        Err(error) => {
                            return failure_snapshot_with_retry(
                                &profile,
                                &self.machine,
                                collected_at,
                                error.kind(),
                                None,
                            );
                        }
                    }
                }
                ClaudeAction::Complete(reading) => {
                    return usage_snapshot(
                        &profile,
                        &self.machine,
                        reading.windows,
                        CollectionOutcome::Success,
                        collected_at,
                    );
                }
                ClaudeAction::Failed(error) => {
                    return failure_snapshot_with_retry(
                        &profile,
                        &self.machine,
                        collected_at,
                        error.kind(),
                        retry_at,
                    );
                }
            };
        }
        failure_snapshot_with_retry(
            &profile,
            &self.machine,
            collected_at,
            PulseErrorKind::Internal,
            None,
        )
    }

    async fn send_claude_request(&self, request: ClaudeRequest) -> PulseResult<ClaudeResponse> {
        if !request.delay.is_zero() {
            tokio::time::sleep(request.delay).await;
        }
        let response = self
            .transport
            .request(TransportRequest {
                method: match request.method {
                    ClaudeHttpMethod::Get => Method::GET,
                    ClaudeHttpMethod::Post => Method::POST,
                },
                endpoint: request.endpoint.to_owned(),
                headers: request
                    .headers()
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
                body: request.body().to_vec(),
                content_type: (request.method == ClaudeHttpMethod::Post)
                    .then(|| "application/json".to_owned()),
            })
            .await?;
        Ok(ClaudeResponse {
            status: response.status,
            headers: response.headers,
            body: response.body,
        })
    }

    async fn refresh_claude(
        &self,
        config_dir: &Path,
        rejected: ClaudeOauthTokens,
        policy: RefreshPolicy,
        collected_at: Instant,
    ) -> PulseResult<RefreshResult> {
        // cooperative_refresh_with locks and re-reads before spending a
        // rotating token. Keeping the exact rejected value lets it adopt a
        // newer token written by a concurrent Claude session.
        let config_dir = config_dir.to_path_buf();
        let transport = Arc::clone(&self.transport);
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Handle::current();
            cooperative_refresh_with(
                &config_dir,
                &rejected,
                true,
                policy,
                collected_at.epoch_millis(),
                RefreshOptions::default(),
                |refresh_token| {
                    runtime.block_on(refresh_grant(
                        Arc::clone(&transport),
                        refresh_token,
                        collected_at,
                    ))
                },
            )
        })
        .await
        .map_err(|_| PulseError::new(PulseErrorKind::Internal, "credential refresh task failed"))?
    }

    async fn collect_codex(&self, profile: Profile, collected_at: Instant) -> UsageSnapshot {
        let Some(config_dir) = profile.config_dir.clone() else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Configuration,
            );
        };
        let credential_path = config_dir.clone();
        let credentials =
            tokio::task::spawn_blocking(move || read_codex_credentials(&credential_path)).await;
        let credentials = match credentials {
            Ok(Ok(credentials)) => credentials,
            Ok(Err(error)) => {
                return failure_snapshot(&profile, &self.machine, collected_at, error.kind());
            }
            Err(_) => {
                return failure_snapshot(
                    &profile,
                    &self.machine,
                    collected_at,
                    PulseErrorKind::Internal,
                );
            }
        };
        let mut collector = CodexCollector::new(credentials);
        let CodexAction::Request(request) = collector.start() else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Internal,
            );
        };
        let response = self
            .transport
            .request(TransportRequest {
                method: Method::GET,
                endpoint: request.endpoint.to_owned(),
                headers: request
                    .headers()
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), value))
                    .collect(),
                body: Vec::new(),
                content_type: None,
            })
            .await;
        let action = match response {
            Ok(response) => collector.handle_live(
                &CodexLiveResponse {
                    status: response.status,
                    body: response.body,
                },
                collected_at,
            ),
            Err(error) => CodexAction::FallbackRequired(error),
        };
        match action {
            CodexAction::Complete(reading) => usage_snapshot(
                &profile,
                &self.machine,
                reading.windows,
                CollectionOutcome::Success,
                collected_at,
            ),
            CodexAction::FallbackRequired(error) => {
                let fallback_path = config_dir;
                match tokio::task::spawn_blocking(move || {
                    collect_rollout_fallback(
                        &fallback_path,
                        collected_at,
                        DiscoveryLimits::default(),
                    )
                })
                .await
                {
                    Ok(Ok(reading)) => usage_snapshot(
                        &profile,
                        &self.machine,
                        reading.windows,
                        CollectionOutcome::Success,
                        collected_at,
                    ),
                    _ => failure_snapshot(&profile, &self.machine, collected_at, error.kind()),
                }
            }
            CodexAction::Failed(error) => {
                failure_snapshot(&profile, &self.machine, collected_at, error.kind())
            }
            CodexAction::Request(_) => failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Internal,
            ),
        }
    }

    async fn collect_deepseek(&self, profile: Profile, collected_at: Instant) -> UsageSnapshot {
        let Some(secret) = profile_secret_ref(&profile) else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Authentication,
            );
        };
        let Some(budget) = profile.monthly_budget_usd else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Configuration,
            );
        };
        let Ok(collector) = DeepSeekCollector::new() else {
            return failure_snapshot(
                &profile,
                &self.machine,
                collected_at,
                PulseErrorKind::Configuration,
            );
        };
        collector
            .collect(
                profile.account_id,
                profile.name,
                self.machine.clone(),
                &secret,
                budget,
                collected_at,
            )
            .await
    }

    async fn collect_grok(&self, profile: Profile, collected_at: Instant) -> UsageSnapshot {
        if let Some(secret) = profile_secret_ref(&profile)
            && let Ok(collector) = GrokCollector::new(GROK_CLIENT_VERSION)
        {
            return collector
                .collect(
                    profile.account_id,
                    profile.name,
                    self.machine.clone(),
                    &secret,
                    profile.config_dir.as_deref(),
                    collected_at,
                )
                .await;
        }
        if let Some(config_dir) = profile.config_dir.clone()
            && let Ok(Ok(Some(window))) = tokio::task::spawn_blocking(move || {
                collect_transcript_usage(&config_dir, collected_at)
            })
            .await
        {
            return usage_snapshot(
                &profile,
                &self.machine,
                vec![window],
                CollectionOutcome::Success,
                collected_at,
            );
        }
        failure_snapshot(
            &profile,
            &self.machine,
            collected_at,
            PulseErrorKind::Authentication,
        )
    }

    async fn collect_context(
        self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> Collected<ContextSession> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut tasks = JoinSet::new();
        for profile in profiles.into_iter().filter(|profile| {
            profile.origin == ProfileOrigin::Local && profile.vendor == Vendor::AnthropicOauth
        }) {
            let machine = self.machine.clone();
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(|_| ())?;
                tokio::task::spawn_blocking(move || {
                    collect_profile_contexts(
                        &profile,
                        &machine,
                        collected_at,
                        DEFAULT_CONTEXT_MAX_AGE,
                    )
                })
                .await
                .map_err(|_| ())?
                .map_err(|_| ())
            });
        }
        let mut items = Vec::new();
        let mut failures = 0_usize;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(collection)) => {
                    items.extend(collection.sessions);
                    failures = failures.saturating_add(collection.failures);
                }
                Ok(Err(())) | Err(_) => failures = failures.saturating_add(1),
            }
        }
        capped_collection(items, failures)
    }

    async fn collect_tokens(
        self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> Collected<TokenGrain> {
        let since_day = recent_day(request).unwrap_or_else(|| "1970-01-01".to_owned());
        let profiles = profiles
            .into_iter()
            .filter(|profile| {
                profile.origin == ProfileOrigin::Local
                    && !matches!(profile.vendor, Vendor::Gemini | Vendor::XaiGrok)
            })
            .collect::<Vec<_>>();
        let rows_per_profile = MAX_NATIVE_COLLECTION_ITEMS
            .checked_div(profiles.len().max(1))
            .unwrap_or(1)
            .max(1);
        let options = TallyOptions {
            window: super::token::TallyWindow::Recent { since_day },
            max_rows: rows_per_profile,
        };
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut tasks = JoinSet::new();
        for profile in profiles {
            let machine = self.machine.clone();
            let options = options.clone();
            let semaphore = Arc::clone(&semaphore);
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.map_err(|_| ())?;
                tokio::task::spawn_blocking(move || tally_profile(&profile, machine, &options))
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())
            });
        }
        let mut items = Vec::new();
        let mut failures = 0_usize;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(tally)) => items.extend(tally.grains),
                Ok(Err(())) | Err(_) => failures = failures.saturating_add(1),
            }
        }
        capped_collection(items, failures)
    }

    async fn collect_gemini(
        self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> Collected<GeminiQuota> {
        let profiles = profiles
            .into_iter()
            .filter(|profile| {
                profile.origin == ProfileOrigin::Local && profile.vendor == Vendor::Gemini
            })
            .collect::<Vec<_>>();
        self.reconcile_gemini_collectors(&profiles);
        let (profiles, mut failures) =
            self.take_due_profiles(profiles, NativePollKind::Gemini, collected_at);
        let mut items = Vec::new();
        for (profile, lease) in profiles {
            let Some(oauth_path) = gemini_oauth_path(&profile) else {
                failures = failures.saturating_add(1);
                continue;
            };
            let Ok(config) = GeminiConfig::from_environment(
                oauth_path,
                self.credentials.gemini_oauth_client_id_env.as_deref(),
                self.credentials.gemini_oauth_client_secret_env.as_deref(),
            ) else {
                failures = failures.saturating_add(1);
                continue;
            };
            let profile_key = ProfileStateKey::from(&profile);
            let Ok(collector) = self.gemini_collector(&profile_key, config) else {
                failures = failures.saturating_add(1);
                continue;
            };
            let collection = collector.collect(profile.account_id, collected_at).await;
            lease.complete(collected_at);
            items.extend(collection.quotas);
            if !matches!(
                collection.outcome,
                CollectionOutcome::Success | CollectionOutcome::Disabled { .. }
            ) {
                failures = failures.saturating_add(1);
            }
        }
        capped_collection(items, failures)
    }
}

impl PulseCollectors for NativeCollectors {
    fn token_observation_scopes(
        &self,
        profiles: &[Profile],
    ) -> Vec<super::service::TokenObservationScope> {
        profiles
            .iter()
            .filter(|profile| {
                profile.origin == ProfileOrigin::Local
                    && !matches!(profile.vendor, Vendor::Gemini | Vendor::XaiGrok)
            })
            .map(|profile| super::service::TokenObservationScope {
                account_id: profile.account_id,
                profile: profile.name.clone(),
                machine: self.machine.clone(),
            })
            .collect()
    }

    fn usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_usage(profiles, collected_at).await) })
    }

    fn context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_context(profiles, collected_at).await) })
    }

    fn tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_tokens(profiles, request).await) })
    }

    fn gemini(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_gemini(profiles, collected_at).await) })
    }

    fn force_usage(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<UsageSnapshot> {
        let collector = self.clone();
        Box::pin(async move {
            collector.force_profiles_due(&profiles, NativePollKind::Usage);
            Ok(collector.collect_usage(profiles, collected_at).await)
        })
    }

    fn force_context(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<ContextSession> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_context(profiles, collected_at).await) })
    }

    fn force_tokens(
        &self,
        profiles: Vec<Profile>,
        request: TokenCollectionRequest,
    ) -> CollectionFuture<TokenGrain> {
        let collector = self.clone();
        Box::pin(async move { Ok(collector.collect_tokens(profiles, request).await) })
    }

    fn force_gemini(
        &self,
        profiles: Vec<Profile>,
        collected_at: Instant,
    ) -> CollectionFuture<GeminiQuota> {
        let collector = self.clone();
        Box::pin(async move {
            collector.force_profiles_due(&profiles, NativePollKind::Gemini);
            Ok(collector.collect_gemini(profiles, collected_at).await)
        })
    }

    fn completion_push(&self, _completed_at: Instant) -> CompletionFuture {
        Box::pin(async { Ok(()) })
    }
}

async fn read_claude_tokens_async(config_dir: PathBuf) -> PulseResult<ClaudeOauthTokens> {
    tokio::task::spawn_blocking(move || read_claude_credentials(&config_dir))
        .await
        .map_err(|_| {
            PulseError::new(
                PulseErrorKind::Internal,
                "Claude credential read task failed",
            )
        })?
}

fn is_local_usage_profile(profile: &Profile) -> bool {
    profile.origin == ProfileOrigin::Local && profile.vendor.emits_usage_snapshots()
}

fn profile_secret_ref(profile: &Profile) -> Option<SecretRef> {
    profile
        .api_key_env
        .as_ref()
        .map(|name| SecretRef::Environment { name: name.clone() })
        .or_else(|| {
            profile
                .api_key_file
                .as_ref()
                .map(|path| SecretRef::File { path: path.clone() })
        })
}

fn gemini_oauth_path(profile: &Profile) -> Option<PathBuf> {
    profile
        .config_dir
        .as_ref()
        .map(|directory| directory.join(GEMINI_OAUTH_FILE))
}

fn capped_collection<T>(mut items: Vec<T>, mut failures: usize) -> Collected<T> {
    failures = failures.min(MAX_NATIVE_COLLECTION_ITEMS);
    let available = MAX_NATIVE_COLLECTION_ITEMS.saturating_sub(failures);
    if items.len() > available {
        let retained = available.saturating_sub(1);
        items.truncate(retained);
        if available > 0 {
            failures = failures.saturating_add(1);
        }
    }
    Collected::new(items, failures).expect("native collection applies the shared hard cap")
}

fn recent_day(request: TokenCollectionRequest) -> Option<String> {
    let lookback_ms = i64::from(request.lookback_days).checked_mul(24 * 60 * 60 * 1_000)?;
    let instant = Instant::from_epoch_millis(
        request
            .collected_at
            .epoch_millis()
            .checked_sub(lookback_ms)?,
    )
    .ok()?;
    instant.to_iso8601().get(..10).map(str::to_owned)
}

fn provider_retry_at(headers: &[(String, String)], now: Instant) -> Option<Instant> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            if name.eq_ignore_ascii_case("retry-after") {
                let seconds = value.parse::<i64>().ok()?;
                let millis = seconds.checked_mul(1_000)?;
                return Instant::from_epoch_millis(now.epoch_millis().checked_add(millis)?).ok();
            }
            if name.eq_ignore_ascii_case("x-ratelimit-reset")
                || name.eq_ignore_ascii_case("anthropic-ratelimit-unified-5h-reset")
                || name.eq_ignore_ascii_case("anthropic-ratelimit-unified-7d-reset")
            {
                return parse_provider_reset(value);
            }
            None
        })
        .filter(|candidate| *candidate > now)
        .min()
}

fn parse_provider_reset(value: &str) -> Option<Instant> {
    if let Ok(numeric) = value.parse::<i64>() {
        let millis = if numeric.unsigned_abs() < 100_000_000_000 {
            numeric.checked_mul(1_000)?
        } else {
            numeric
        };
        return Instant::from_epoch_millis(millis).ok();
    }
    Instant::from_iso8601(value).ok()
}

fn usage_snapshot(
    profile: &Profile,
    machine: &MachineName,
    windows: Vec<super::QuotaWindow>,
    outcome: CollectionOutcome,
    polled_at: Instant,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id: profile.account_id,
        profile: profile.name.clone(),
        machine: machine.clone(),
        vendor: profile.vendor,
        windows,
        outcome,
        polled_at,
        reporter_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}

fn failure_snapshot(
    profile: &Profile,
    machine: &MachineName,
    polled_at: Instant,
    kind: PulseErrorKind,
) -> UsageSnapshot {
    failure_snapshot_with_retry(profile, machine, polled_at, kind, None)
}

fn failure_snapshot_with_retry(
    profile: &Profile,
    machine: &MachineName,
    polled_at: Instant,
    kind: PulseErrorKind,
    retry_at: Option<Instant>,
) -> UsageSnapshot {
    let prefix = match profile.vendor {
        Vendor::AnthropicOauth => "anthropic",
        Vendor::OpenaiCodex => "codex",
        Vendor::DeepseekBalance => "deepseek",
        Vendor::XaiGrok => "grok",
        Vendor::Gemini => "gemini",
        Vendor::Antigravity => "antigravity",
    };
    let outcome = match kind {
        PulseErrorKind::Authentication => CollectionOutcome::AuthenticationFailed {
            code: format!("{prefix}_authentication_failed"),
        },
        PulseErrorKind::RateLimited => CollectionOutcome::RateLimited { retry_at },
        PulseErrorKind::InvalidInput => CollectionOutcome::InvalidResponse {
            code: format!("{prefix}_response_invalid"),
        },
        PulseErrorKind::Offline | PulseErrorKind::Upstream => CollectionOutcome::Unavailable {
            code: format!("{prefix}_upstream_unavailable"),
        },
        PulseErrorKind::NotFound
        | PulseErrorKind::Conflict
        | PulseErrorKind::Storage
        | PulseErrorKind::Configuration
        | PulseErrorKind::Internal => CollectionOutcome::Unavailable {
            code: format!("{prefix}_collector_unavailable"),
        },
    };
    usage_snapshot(profile, machine, Vec::new(), outcome, polled_at)
}

#[derive(Deserialize)]
struct AnthropicRefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u32>,
}

async fn refresh_grant(
    transport: Arc<dyn ProviderTransport>,
    refresh_token: &SecretString,
    now: Instant,
) -> PulseResult<RefreshGrant> {
    let body = serde_json::to_vec(&serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token.expose(),
        "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
    }))
    .map_err(|_| {
        PulseError::new(
            PulseErrorKind::Internal,
            "refresh request could not be encoded",
        )
    })?;
    let response = transport
        .request(TransportRequest {
            method: Method::POST,
            endpoint: ANTHROPIC_TOKEN_ENDPOINT.to_owned(),
            headers: Vec::new(),
            body,
            content_type: Some("application/json".to_owned()),
        })
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(PulseError::new(
            PulseErrorKind::Authentication,
            "anthropic credential refresh was rejected",
        ));
    }
    let response: AnthropicRefreshResponse =
        serde_json::from_slice(&response.body).map_err(|_| {
            PulseError::new(
                PulseErrorKind::Authentication,
                "anthropic credential refresh response was invalid",
            )
        })?;
    let lifetime =
        i64::from(response.expires_in.unwrap_or(3_600).clamp(60, 86_400)).saturating_mul(1_000);
    RefreshGrant::new(
        response.access_token,
        response
            .refresh_token
            .unwrap_or_else(|| refresh_token.expose().to_owned()),
        now.epoch_millis().saturating_add(lifetime),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::pulse::{AccountId, ProfileName};

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "atmux-native-lifecycle-{}-{}",
                std::process::id(),
                NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Debug)]
    struct FakeTransport {
        responses: Mutex<VecDeque<PulseResult<TransportResponse>>>,
        requests: Mutex<Vec<TransportRequest>>,
    }

    impl FakeTransport {
        fn new(responses: Vec<PulseResult<TransportResponse>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl ProviderTransport for FakeTransport {
        fn request(&self, request: TransportRequest) -> TransportFuture {
            self.requests.lock().expect("requests").push(request);
            let response = self
                .responses
                .lock()
                .expect("responses")
                .pop_front()
                .expect("fake response");
            Box::pin(async move { response })
        }
    }

    fn profile(vendor: Vendor, origin: ProfileOrigin) -> Profile {
        Profile {
            account_id: AccountId::new(1).expect("account"),
            name: ProfileName::new("default").expect("profile"),
            vendor,
            config_dir: Some(PathBuf::from("/tmp/atmux-pulse-test")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin,
        }
    }

    fn named_profile(name: &str, vendor: Vendor, minutes: u32, path: &str) -> Profile {
        let mut profile = profile(vendor, ProfileOrigin::Local);
        profile.name = ProfileName::new(name).expect("profile name");
        profile.poll_interval_minutes = minutes;
        profile.config_dir = Some(PathBuf::from(path));
        profile
    }

    fn gemini_config(profile: &Profile) -> GeminiConfig {
        GeminiConfig::new(
            gemini_oauth_path(profile).expect("oauth path"),
            "fixture-client-id".to_owned(),
            "fixture-client-secret".to_owned(),
        )
        .expect("Gemini fixture config")
    }

    fn test_collectors() -> NativeCollectors {
        NativeCollectors::with_transport(
            MachineName::new("max").expect("machine"),
            PulseCredentialConfig::default(),
            Arc::new(FakeTransport::new(Vec::new())),
        )
    }

    #[test]
    fn reported_profiles_never_enter_local_usage_collection() {
        assert!(!is_local_usage_profile(&profile(
            Vendor::AnthropicOauth,
            ProfileOrigin::Reported
        )));
        assert!(is_local_usage_profile(&profile(
            Vendor::AnthropicOauth,
            ProfileOrigin::Local
        )));
        assert!(!is_local_usage_profile(&profile(
            Vendor::Gemini,
            ProfileOrigin::Local
        )));
    }

    #[test]
    fn failure_snapshots_are_valid_and_secret_free() {
        let profile = profile(Vendor::OpenaiCodex, ProfileOrigin::Local);
        let snapshot = failure_snapshot(
            &profile,
            &MachineName::new("max").expect("machine"),
            Instant::from_epoch_millis(1_786_214_400_000).expect("instant"),
            PulseErrorKind::Authentication,
        );
        snapshot.validate().expect("valid failure snapshot");
        assert_eq!(
            snapshot.outcome,
            CollectionOutcome::AuthenticationFailed {
                code: "codex_authentication_failed".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn refresh_grant_uses_fixed_endpoint_and_rotated_token() {
        let fake = Arc::new(FakeTransport::new(vec![Ok(TransportResponse {
            status: 200,
            headers: Vec::new(),
            body:
                br#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":600}"#
                    .to_vec(),
        })]));
        let now = Instant::from_epoch_millis(1_786_214_400_000).expect("instant");
        let token = SecretString::new("old-refresh").expect("token");
        let grant = refresh_grant(fake.clone(), &token, now)
            .await
            .expect("refresh");
        assert_eq!(grant.expires_at_millis, now.epoch_millis() + 600_000);
        let requests = fake.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].endpoint, ANTHROPIC_TOKEN_ENDPOINT);
        assert_eq!(requests[0].method, Method::POST);
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("json request");
        assert_eq!(body["client_id"], ANTHROPIC_OAUTH_CLIENT_ID);
        assert_eq!(body["refresh_token"], "old-refresh");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn repeated_polls_reuse_the_durably_rotated_claude_credential() {
        let directory = TestDirectory::new();
        let collected_at = Instant::from_epoch_millis(1_786_214_400_000).expect("instant");
        fs::write(
            directory.0.join(".credentials.json"),
            serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "original-access-canary",
                    "refreshToken": "original-refresh-canary",
                    "expiresAt": collected_at.epoch_millis() + 3_600_000,
                    "scopes": ["user:profile"]
                }
            })
            .to_string(),
        )
        .expect("write credentials");
        let usage = br#"{
            "five_hour":{"utilization":25.0,"resets_at":"2026-08-08T20:00:00Z"},
            "seven_day":{"utilization":50.0,"resets_at":"2026-08-14T00:00:00Z"}
        }"#
        .to_vec();
        let fake = Arc::new(FakeTransport::new(vec![
            Ok(TransportResponse {
                status: 401,
                headers: Vec::new(),
                body: Vec::new(),
            }),
            Ok(TransportResponse {
                status: 200,
                headers: Vec::new(),
                body: br#"{
                    "access_token":"rotated-access-canary",
                    "refresh_token":"rotated-refresh-canary",
                    "expires_in":3600
                }"#
                .to_vec(),
            }),
            Ok(TransportResponse {
                status: 200,
                headers: Vec::new(),
                body: usage.clone(),
            }),
            Ok(TransportResponse {
                status: 200,
                headers: Vec::new(),
                body: usage,
            }),
        ]));
        let collector = NativeCollectors::with_transport(
            MachineName::new("max").expect("machine"),
            PulseCredentialConfig::default(),
            fake.clone(),
        );
        let mut claude = named_profile(
            "claude",
            Vendor::AnthropicOauth,
            5,
            directory.0.to_str().expect("utf8 path"),
        );
        claude.refresh = RefreshPolicy::Persist;

        let _ = collector
            .clone()
            .collect_usage(vec![claude.clone()], collected_at)
            .await;
        let persisted: serde_json::Value = serde_json::from_slice(
            &fs::read(directory.0.join(".credentials.json")).expect("read persisted credentials"),
        )
        .expect("parse persisted credentials");
        assert_eq!(
            persisted["claudeAiOauth"]["accessToken"],
            "rotated-access-canary"
        );
        assert_eq!(
            fake.requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|request| request.endpoint == ANTHROPIC_TOKEN_ENDPOINT)
                .count(),
            1
        );

        let next_poll =
            Instant::from_epoch_millis(collected_at.epoch_millis() + 5 * 60_000).expect("instant");
        let _ = collector.collect_usage(vec![claude], next_poll).await;
        let requests = fake.requests.lock().expect("requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.endpoint == ANTHROPIC_TOKEN_ENDPOINT)
                .count(),
            1,
            "the second scheduled poll must not spend the rotated token again"
        );
    }

    #[test]
    fn recent_day_is_calendar_safe() {
        let request = TokenCollectionRequest {
            collected_at: Instant::from_iso8601("2026-03-01T01:00:00Z").expect("instant"),
            lookback_days: 2,
        };
        assert_eq!(recent_day(request).as_deref(), Some("2026-02-27"));
    }

    #[test]
    fn collection_cap_retains_a_failure_marker() {
        let capped = capped_collection(vec![(); MAX_NATIVE_COLLECTION_ITEMS + 25], 0);
        let expected = Collected::new(vec![(); MAX_NATIVE_COLLECTION_ITEMS - 1], 1)
            .expect("bounded expected collection");
        assert_eq!(capped, expected);

        let failures_only = capped_collection(vec![()], MAX_NATIVE_COLLECTION_ITEMS + 25);
        let expected = Collected::new(Vec::<()>::new(), MAX_NATIVE_COLLECTION_ITEMS)
            .expect("bounded failure collection");
        assert_eq!(failures_only, expected);
    }

    #[test]
    fn injected_transport_constructor_never_resolves_credentials() {
        let fake = Arc::new(FakeTransport::new(Vec::new()));
        let collector = NativeCollectors::with_transport(
            MachineName::new("max").expect("machine"),
            PulseCredentialConfig::default(),
            fake,
        );
        assert_eq!(collector.concurrency, 2);
    }

    #[test]
    fn profile_cadence_is_immediate_heterogeneous_and_single_flight() {
        let collector = test_collectors();
        let fast = named_profile("fast", Vendor::OpenaiCodex, 5, "/tmp/fast");
        let slow = named_profile("slow", Vendor::OpenaiCodex, 30, "/tmp/slow");
        let first_at = Instant::from_epoch_millis(1_000).expect("instant");
        let (first, failures) = collector.take_due_profiles(
            vec![fast.clone(), slow.clone()],
            NativePollKind::Usage,
            first_at,
        );
        assert_eq!(first.len(), 2);
        assert_eq!(failures, 0);

        let (overlap, failures) = collector.take_due_profiles(
            vec![fast.clone(), slow.clone()],
            NativePollKind::Usage,
            first_at,
        );
        assert!(overlap.is_empty());
        assert_eq!(failures, 0);

        for (_, lease) in first {
            lease.complete(first_at);
        }
        let five_minutes_later =
            Instant::from_epoch_millis(first_at.epoch_millis() + 5 * 60_000).expect("instant");
        let (due, failures) = collector.take_due_profiles(
            vec![fast, slow],
            NativePollKind::Usage,
            five_minutes_later,
        );
        assert_eq!(failures, 0);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0.name.as_str(), "fast");
    }

    #[test]
    fn profile_cadence_reacts_to_runtime_changes_and_abandoned_work() {
        let collector = test_collectors();
        let initial = named_profile("runtime", Vendor::OpenaiCodex, 30, "/tmp/runtime-a");
        let first_at = Instant::from_epoch_millis(1_000).expect("instant");
        let (first, _) =
            collector.take_due_profiles(vec![initial.clone()], NativePollKind::Usage, first_at);
        assert_eq!(first.len(), 1);
        first
            .into_iter()
            .next()
            .expect("lease")
            .1
            .complete(first_at);

        let five_minutes_later =
            Instant::from_epoch_millis(first_at.epoch_millis() + 5 * 60_000).expect("instant");
        let mut shortened = initial.clone();
        shortened.poll_interval_minutes = 5;
        let (due, _) =
            collector.take_due_profiles(vec![shortened], NativePollKind::Usage, five_minutes_later);
        assert_eq!(due.len(), 1, "shortening cadence takes effect at runtime");
        drop(due);

        let (retry, _) = collector.take_due_profiles(
            vec![initial.clone()],
            NativePollKind::Usage,
            five_minutes_later,
        );
        assert_eq!(
            retry.len(),
            0,
            "original thirty-minute cadence is restored for the same key"
        );

        let moved = named_profile("runtime", Vendor::OpenaiCodex, 30, "/tmp/runtime-b");
        let (new_path, _) =
            collector.take_due_profiles(vec![moved], NativePollKind::Usage, five_minutes_later);
        assert_eq!(new_path.len(), 1, "a config-path change is a new identity");
    }

    #[test]
    fn abandoned_poll_lease_clears_single_flight_without_advancing_cadence() {
        let collector = test_collectors();
        let profile = named_profile("aborted", Vendor::OpenaiCodex, 15, "/tmp/aborted");
        let now = Instant::from_epoch_millis(1_000).expect("instant");
        let (started, _) =
            collector.take_due_profiles(vec![profile.clone()], NativePollKind::Usage, now);
        assert_eq!(started.len(), 1);
        drop(started);
        let (retried, _) = collector.take_due_profiles(vec![profile], NativePollKind::Usage, now);
        assert_eq!(retried.len(), 1);
    }

    #[test]
    fn force_due_is_account_scoped_and_never_duplicates_an_inflight_lease() {
        let collector = test_collectors();
        let account_one = named_profile("one", Vendor::OpenaiCodex, 30, "/tmp/force-one");
        let mut account_two = named_profile("two", Vendor::OpenaiCodex, 30, "/tmp/force-two");
        account_two.account_id = crate::pulse::AccountId::new(2).expect("account");
        let now = Instant::from_epoch_millis(1_000).expect("instant");
        let (started, _) = collector.take_due_profiles(
            vec![account_one.clone(), account_two.clone()],
            NativePollKind::Usage,
            now,
        );
        assert_eq!(started.len(), 2);
        let mut leases = started.into_iter();
        let (_, one_lease) = leases.next().expect("account one lease");
        let (_, two_lease) = leases.next().expect("account two lease");

        collector.force_profiles_due(std::slice::from_ref(&account_one), NativePollKind::Usage);
        let (overlap, _) = collector.take_due_profiles(
            vec![account_one.clone(), account_two.clone()],
            NativePollKind::Usage,
            now,
        );
        assert!(overlap.is_empty(), "force must retain in-flight refusal");
        one_lease.complete(now);
        two_lease.complete(now);

        collector.force_profiles_due(std::slice::from_ref(&account_one), NativePollKind::Usage);
        let (forced, _) =
            collector.take_due_profiles(vec![account_one, account_two], NativePollKind::Usage, now);
        assert_eq!(forced.len(), 1);
        assert_eq!(forced[0].0.account_id.get(), 1);
    }

    #[test]
    fn per_kind_poll_state_is_hard_bounded() {
        let collector = test_collectors();
        let profiles = (0..=MAX_PROFILE_STATE_ENTRIES)
            .map(|index| {
                named_profile(
                    &format!("profile-{index}"),
                    Vendor::OpenaiCodex,
                    15,
                    &format!("/tmp/profile-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let now = Instant::from_epoch_millis(1_000).expect("instant");
        let (due, failures) = collector.take_due_profiles(profiles, NativePollKind::Usage, now);
        assert_eq!(due.len(), MAX_PROFILE_STATE_ENTRIES);
        assert_eq!(failures, 1);
        assert_eq!(
            lock_native_state(&collector.state).polls.len(),
            MAX_PROFILE_STATE_ENTRIES
        );
    }

    #[test]
    fn gemini_instances_reuse_cache_and_follow_config_path_changes() {
        let collector = test_collectors();
        let first = named_profile("gemini", Vendor::Gemini, 30, "/tmp/gemini-a");
        collector.reconcile_gemini_collectors(std::slice::from_ref(&first));
        let first_key = ProfileStateKey::from(&first);
        let first_instance = collector
            .gemini_collector(&first_key, gemini_config(&first))
            .expect("first collector");
        let reused = collector
            .gemini_collector(&first_key, gemini_config(&first))
            .expect("reused collector");
        assert!(Arc::ptr_eq(&first_instance, &reused));

        let moved = named_profile("gemini", Vendor::Gemini, 30, "/tmp/gemini-b");
        collector.reconcile_gemini_collectors(std::slice::from_ref(&moved));
        let moved_key = ProfileStateKey::from(&moved);
        let moved_instance = collector
            .gemini_collector(&moved_key, gemini_config(&moved))
            .expect("moved collector");
        assert!(!Arc::ptr_eq(&first_instance, &moved_instance));
        assert_eq!(
            lock_native_state(&collector.state).gemini_collectors.len(),
            1
        );
    }

    #[test]
    fn gemini_instance_cache_is_hard_bounded() {
        let collector = test_collectors();
        let template = named_profile("gemini-0", Vendor::Gemini, 30, "/tmp/gemini-0");
        let instance = Arc::new(GeminiCollector::new(gemini_config(&template)).expect("collector"));
        {
            let mut state = lock_native_state(&collector.state);
            for index in 0..MAX_PROFILE_STATE_ENTRIES {
                let profile = named_profile(
                    &format!("gemini-{index}"),
                    Vendor::Gemini,
                    30,
                    &format!("/tmp/gemini-{index}"),
                );
                state
                    .gemini_collectors
                    .insert(ProfileStateKey::from(&profile), Arc::clone(&instance));
            }
        }
        let overflow = named_profile("gemini-overflow", Vendor::Gemini, 30, "/tmp/overflow");
        let error = collector
            .gemini_collector(&ProfileStateKey::from(&overflow), gemini_config(&overflow))
            .expect_err("cache must remain bounded");
        assert_eq!(error.kind(), PulseErrorKind::Configuration);
        assert_eq!(
            lock_native_state(&collector.state).gemini_collectors.len(),
            MAX_PROFILE_STATE_ENTRIES
        );
    }

    #[test]
    fn process_token_state_reuses_persisted_refresh_and_adopts_store_changes() {
        let collector = test_collectors();
        let profile = named_profile("claude", Vendor::AnthropicOauth, 15, "/tmp/claude");
        let key = ProfileStateKey::from(&profile);
        let original = ClaudeOauthTokens::new(
            "original-access",
            Some("original-refresh".to_owned()),
            1_000,
            vec!["user:profile".to_owned()],
        )
        .expect("original tokens");
        let (_, authoritative) = collector.prepare_claude_tokens(&key, original);
        assert_eq!(authoritative.access_token().expose(), "original-access");

        let persisted = ClaudeOauthTokens::new(
            "persisted-access",
            Some("persisted-refresh".to_owned()),
            100_000,
            vec!["user:profile".to_owned()],
        )
        .expect("persisted tokens");
        collector.record_claude_refresh(
            &key,
            &RefreshResult {
                tokens: persisted.clone(),
                source: super::super::credentials::RefreshSource::GrantedAndPersisted,
            },
        );
        let (active, _) = collector.prepare_claude_tokens(&key, persisted);
        assert_eq!(active.access_token().expose(), "persisted-access");

        let sibling = ClaudeOauthTokens::new(
            "sibling-access",
            Some("sibling-refresh".to_owned()),
            200_000,
            vec!["user:profile".to_owned()],
        )
        .expect("sibling tokens");
        let (active, authoritative) = collector.prepare_claude_tokens(&key, sibling);
        assert_eq!(active.access_token().expose(), "sibling-access");
        assert_eq!(authoritative.access_token().expose(), "sibling-access");

        let metadata_only_rotation = ClaudeOauthTokens::new(
            "sibling-access",
            Some("metadata-rotated-refresh".to_owned()),
            300_000,
            vec!["user:profile".to_owned(), "user:inference".to_owned()],
        )
        .expect("metadata-only rotation");
        let (active, authoritative) = collector.prepare_claude_tokens(&key, metadata_only_rotation);
        assert_eq!(active.expires_at_millis(), 300_000);
        assert!(active.has_scope("user:inference"));
        assert_eq!(authoritative.expires_at_millis(), 300_000);
        assert!(!format!("{collector:?}").contains("sibling-access"));
    }

    #[test]
    fn retry_metadata_chooses_earliest_future_safe_value() {
        let now = Instant::from_epoch_millis(1_786_214_400_000).expect("instant");
        let headers = vec![
            ("retry-after".to_owned(), "45".to_owned()),
            (
                "anthropic-ratelimit-unified-5h-reset".to_owned(),
                ((now.epoch_millis() + 60_000) / 1_000).to_string(),
            ),
            ("set-cookie".to_owned(), "credential-canary".to_owned()),
        ];
        assert_eq!(
            provider_retry_at(&headers, now),
            Instant::from_epoch_millis(now.epoch_millis() + 45_000).ok()
        );
        assert_eq!(
            provider_retry_at(
                &[("retry-after".to_owned(), "credential-canary".to_owned())],
                now
            ),
            None
        );
    }

    #[test]
    fn transport_debug_never_formats_headers_or_bodies() {
        let request = TransportRequest {
            method: Method::POST,
            endpoint: ANTHROPIC_TOKEN_ENDPOINT.to_owned(),
            headers: vec![(
                "authorization".to_owned(),
                "bearer-secret-canary".to_owned(),
            )],
            body: b"refresh-secret-canary".to_vec(),
            content_type: Some("application/json".to_owned()),
        };
        let response = TransportResponse {
            status: 200,
            headers: vec![("set-cookie".to_owned(), "cookie-secret-canary".to_owned())],
            body: b"access-secret-canary".to_vec(),
        };
        assert!(!format!("{request:?}").contains("secret-canary"));
        assert!(!format!("{response:?}").contains("secret-canary"));
    }
}
