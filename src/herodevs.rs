//! Shared authenticated herodevs GraphQL and MCP clients.
#![allow(clippy::missing_errors_doc)]
use anyhow::{Result, bail, ensure};
use base64::Engine as _;
use fs2::FileExt as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fmt,
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

#[derive(Clone)]
pub struct Secret(String);
impl Secret {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(value: String) -> Result<Self> {
        ensure!(
            !value.trim().is_empty()
                && value.len() <= 16384
                && value.trim().bytes().all(|b| b.is_ascii_graphic()),
            "invalid credential"
        );
        Ok(Self(value.trim().into()))
    }
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
pub fn read_secret(path: &Path) -> Result<Secret> {
    let file =
        std::fs::File::open(path).map_err(|_| anyhow::anyhow!("credential file unavailable"))?;
    read_secret_handle(file)
}
fn read_secret_handle(file: std::fs::File) -> Result<Secret> {
    ensure!(
        file.metadata()?.is_file(),
        "credential must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "credential exceeded bound");
    Secret::new(String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("credential is not UTF-8"))?)
}
pub type TokenFuture<'a> = Pin<Box<dyn Future<Output = Result<Secret>> + Send + 'a>>;
/// Implementations must return a validated bearer, never a refresh token.
pub trait AuthProvider: Send + Sync {
    fn access_token(&self) -> TokenFuture<'_>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret_file: PathBuf,
    pub refresh_token_file: PathBuf,
    pub expected_subject: String,
    pub tenant_slug: String,
    pub audiences: Vec<String>,
    pub allow_http_hosts: Vec<String>,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            issuer: String::new(),
            client_id: "hd-atmux".into(),
            client_secret_file: PathBuf::new(),
            refresh_token_file: PathBuf::new(),
            expected_subject: String::new(),
            tenant_slug: "hq".into(),
            audiences: vec![
                "hd-subgraphs".into(),
                "https://hq.herodevs.dev/hd-mcp".into(),
            ],
            allow_http_hosts: vec![],
        }
    }
}
impl AuthConfig {
    pub fn validate(&self) -> Result<()> {
        crate::platform_http::endpoint(&self.issuer, &self.allow_http_hosts)?;
        ensure!(
            !self.expected_subject.is_empty()
                && !self.client_id.is_empty()
                && !self.tenant_slug.is_empty()
                && !self.audiences.is_empty()
                && self.client_secret_file.is_absolute()
                && self.refresh_token_file.is_absolute(),
            "incomplete herodevs auth configuration"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}
#[derive(Deserialize)]
struct Claims {
    sub: String,
    azp: Option<String>,
    tenant_id: Option<String>,
    identity_type: Option<String>,
    aud: Value,
    exp: u64,
    nonce: Option<String>,
}
struct Access {
    token: Secret,
    expires: u64,
}
pub struct DeviceAuth {
    config: AuthConfig,
    cache: tokio::sync::Mutex<Option<Access>>,
}
/// Only public device approval fields may be displayed.
pub struct DeviceLogin {
    pub verification_uri: String,
    pub user_code: String,
    code: Secret,
    nonce: String,
    interval: u64,
    expires: u64,
}
impl DeviceAuth {
    pub fn new(config: AuthConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            cache: tokio::sync::Mutex::new(None),
        })
    }
    fn endpoint(&self, target: &str) -> Result<url::Url> {
        let url = crate::platform_http::endpoint(target, &self.config.allow_http_hosts)?;
        let issuer =
            crate::platform_http::endpoint(&self.config.issuer, &self.config.allow_http_hosts)?;
        ensure!(
            url.origin() == issuer.origin(),
            "OIDC endpoint changed pinned origin"
        );
        Ok(url)
    }
    async fn get(&self, target: &str) -> Result<Value> {
        let (status, bytes) = crate::platform_http::request(
            &self.endpoint(target)?,
            "GET",
            &[],
            vec![],
            30,
            128 * 1024,
        )
        .await?;
        ensure!(status == 200, "OIDC HTTP status {status}");
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid OIDC JSON"))
    }
    async fn discovery(&self) -> Result<Discovery> {
        let d: Discovery = serde_json::from_value(
            self.get(&format!(
                "{}/.well-known/openid-configuration",
                self.config.issuer.trim_end_matches('/')
            ))
            .await?,
        )
        .map_err(|_| anyhow::anyhow!("invalid OIDC discovery"))?;
        ensure!(d.issuer == self.config.issuer, "OIDC issuer mismatch");
        for target in [
            &d.token_endpoint,
            &d.device_authorization_endpoint,
            &d.jwks_uri,
        ] {
            self.endpoint(target)?;
        }
        Ok(d)
    }
    async fn form(&self, target: &str, mut fields: Vec<(&str, String)>) -> Result<Value> {
        fields.extend([
            ("client_id", self.config.client_id.clone()),
            (
                "client_secret",
                read_secret(&self.config.client_secret_file)?
                    .expose()
                    .into(),
            ),
            ("tenant_slug", self.config.tenant_slug.clone()),
        ]);
        let body = {
            let mut encoded = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in fields {
                encoded.append_pair(key, &value);
            }
            encoded.finish().into_bytes()
        };
        let (status, bytes) = crate::platform_http::request(
            &self.endpoint(target)?,
            "POST",
            &[("Content-Type", "application/x-www-form-urlencoded".into())],
            body,
            30,
            128 * 1024,
        )
        .await?;
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid OIDC response"))?;
        ensure!(
            status == 200 || value.get("error").is_some(),
            "OIDC HTTP status {status}"
        );
        Ok(value)
    }
    async fn verify(&self, token: &str, jwks: &str, id_nonce: Option<&str>) -> Result<Claims> {
        let header = decode_header(token).map_err(|_| anyhow::anyhow!("invalid JWT header"))?;
        ensure!(header.alg == Algorithm::RS256, "JWT algorithm not allowed");
        let keys: JwkSet = serde_json::from_value(self.get(jwks).await?)
            .map_err(|_| anyhow::anyhow!("invalid JWKS"))?;
        let key = keys
            .find(
                header
                    .kid
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("JWT missing kid"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("JWT signing key missing"))?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.config.issuer]);
        if id_nonce.is_some() {
            validation.set_audience(&[&self.config.client_id]);
        } else {
            validation.set_audience(&self.config.audiences);
        }
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_jwk(key).map_err(|_| anyhow::anyhow!("invalid signing key"))?,
            &validation,
        )
        .map_err(|_| anyhow::anyhow!("JWT validation failed"))?
        .claims;
        ensure!(
            claims.sub == self.config.expected_subject,
            "JWT subject mismatch"
        );
        if let Some(nonce) = id_nonce {
            ensure!(
                claims.nonce.as_deref() == Some(nonce),
                "ID token nonce mismatch"
            );
        } else {
            ensure!(
                claims.azp.as_deref() == Some(self.config.client_id.as_str())
                    && claims.tenant_id.as_deref() == Some(self.config.tenant_slug.as_str())
                    && claims.identity_type.as_deref().is_none_or(|v| v == "USER"),
                "JWT identity or tenant mismatch"
            );
            ensure!(
                self.config
                    .audiences
                    .iter()
                    .all(|a| claims.aud.as_str() == Some(a.as_str())
                        || claims.aud.as_array().is_some_and(|values| values
                            .iter()
                            .any(|v| v.as_str() == Some(a.as_str())))),
                "JWT audience mismatch"
            );
        }
        Ok(claims)
    }
    pub async fn begin_login(&self) -> Result<DeviceLogin> {
        let d = self.discovery().await?;
        let mut random = [0u8; 32];
        getrandom::fill(&mut random)?;
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        let value = self
            .form(
                &d.device_authorization_endpoint,
                vec![
                    ("scope", "openid profile offline_access".into()),
                    ("nonce", nonce.clone()),
                ],
            )
            .await?;
        let text = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("device response missing field"))
        };
        let uri = text("verification_uri")?;
        self.endpoint(&uri)?;
        let expires = value["expires_in"]
            .as_u64()
            .filter(|v| (1..=1800).contains(v))
            .ok_or_else(|| anyhow::anyhow!("invalid device lifetime"))?;
        Ok(DeviceLogin {
            verification_uri: uri,
            user_code: text("user_code")?,
            code: Secret::new(text("device_code")?)?,
            nonce,
            interval: value["interval"].as_u64().unwrap_or(5).clamp(5, 300),
            expires,
        })
    }
    pub async fn finish_login(&self, login: DeviceLogin) -> Result<()> {
        let mut cache = self.cache.lock().await;
        let _lock = credential_lock(&self.config.refresh_token_file)?;
        let d = self.discovery().await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(login.expires);
        let mut interval = login.interval;
        loop {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "device code expired"
            );
            let next = tokio::time::Instant::now() + std::time::Duration::from_secs(interval);
            ensure!(next < deadline, "device code expired before next poll");
            tokio::time::sleep_until(next).await;
            let value = self
                .form(
                    &d.token_endpoint,
                    vec![
                        (
                            "grant_type",
                            "urn:ietf:params:oauth:grant-type:device_code".into(),
                        ),
                        ("device_code", login.code.expose().into()),
                    ],
                )
                .await;
            let Ok(value) = value else {
                interval = interval.saturating_mul(2);
                continue;
            };
            match value["error"].as_str() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    interval = interval.saturating_add(5);
                    continue;
                }
                Some(_) => bail!("device login rejected; start explicit login again"),
                None => {}
            }
            ensure!(
                value["scope"]
                    .as_str()
                    .is_some_and(|v| v.split_whitespace().any(|s| s == "offline_access")),
                "offline_access was not granted"
            );
            self.verify(
                value["id_token"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("initial ID token missing"))?,
                &d.jwks_uri,
                Some(&login.nonce),
            )
            .await?;
            *cache = Some(self.accept(&value, &d.jwks_uri).await?);
            return Ok(());
        }
    }
    async fn accept(&self, value: &Value, jwks: &str) -> Result<Access> {
        ensure!(
            value["token_type"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case("bearer")),
            "OIDC response is not Bearer"
        );
        let token = Secret::new(
            value["access_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("access token missing"))?
                .into(),
        )?;
        let claims = self.verify(token.expose(), jwks, None).await?;
        let refresh = Secret::new(
            value["refresh_token"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("rotated refresh token missing"))?
                .into(),
        )?;
        write_credential(&self.config.refresh_token_file, refresh.expose())?;
        Ok(Access {
            token,
            expires: claims.exp,
        })
    }
}
impl AuthProvider for DeviceAuth {
    fn access_token(&self) -> TokenFuture<'_> {
        Box::pin(async {
            let mut cache = self.cache.lock().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            if let Some(access) = cache.as_ref().filter(|a| a.expires > now + 60) {
                return Ok(access.token.clone());
            }
            let _lock = credential_lock(&self.config.refresh_token_file)?;
            let refresh = read_refresh(&self.config.refresh_token_file)?;
            ensure!(
                refresh.expose() != "LOGIN_REQUIRED",
                "explicit herodevs login required"
            );
            let d = self.discovery().await?;
            // A crash or ambiguous refresh must never replay a possibly consumed token.
            write_credential(&self.config.refresh_token_file, "LOGIN_REQUIRED")?;
            let value = self
                .form(
                    &d.token_endpoint,
                    vec![
                        ("grant_type", "refresh_token".into()),
                        ("refresh_token", refresh.expose().into()),
                    ],
                )
                .await?;
            ensure!(
                value.get("error").is_none(),
                "refresh rejected; explicit herodevs login required"
            );
            let access = self.accept(&value, &d.jwks_uri).await?;
            let token = access.token.clone();
            *cache = Some(access);
            Ok(token)
        })
    }
}
fn credential_lock(path: &Path) -> Result<std::fs::File> {
    let file = private_file(&path.with_extension("lock"))?;
    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("credential store is busy"))?;
    Ok(file)
}
#[allow(clippy::verbose_bit_mask)]
fn private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())?);
    }
    let file = options
        .open(path)
        .map_err(|_| anyhow::anyhow!("credential store unavailable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        ensure!(
            file.metadata()?.permissions().mode() & 0o077 == 0
                && file.metadata()?.uid() == rustix::process::geteuid().as_raw(),
            "credential file must be owner-only"
        );
    }
    Ok(file)
}
fn read_refresh(path: &Path) -> Result<Secret> {
    read_secret_handle(private_file(path)?)
}
fn write_credential(path: &Path, value: &str) -> Result<()> {
    let temporary = path.with_extension("pending");
    let mut file = private_file(&temporary)?;
    file.set_len(0)?;
    file.write_all(value.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerodevsConfig {
    pub graphql_url: String,
    pub mcp_url: String,
    pub tenant_id: String,
    pub auth: AuthConfig,
    pub allow_http_hosts: Vec<String>,
}
impl Default for HerodevsConfig {
    fn default() -> Self {
        Self {
            graphql_url: "https://hq.herodevs.dev/graphql".into(),
            mcp_url: "https://hq.herodevs.dev/hd-mcp/mcp".into(),
            tenant_id: "95efe33d-fa71-53ce-8e0a-3fe45ac0e58a".into(),
            auth: AuthConfig::default(),
            allow_http_hosts: vec![],
        }
    }
}
#[derive(Clone)]
pub struct HerodevsClient {
    config: HerodevsConfig,
    auth: Arc<dyn AuthProvider>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessage {
    pub id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub job_type: Option<String>,
    #[serde(default)]
    pub job_state: Option<String>,
    #[serde(default)]
    pub fence_token: Option<i64>,
    #[serde(default)]
    pub ordinal: u64,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub claimed_by_agent_id: Option<String>,
    #[serde(default)]
    pub lease_expires_at: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostJob {
    pub channel_id: String,
    pub content: String,
    pub job_type: String,
    pub client_request_id: String,
    pub metadata: Value,
    pub priority: u8,
}
const FIELDS: &str = "id channelId content jobId jobType jobState fenceToken ordinal metadata claimedByAgentId leaseExpiresAt";
impl HerodevsClient {
    pub fn new(config: HerodevsConfig, auth: Arc<dyn AuthProvider>) -> Result<Self> {
        crate::platform_http::endpoint(&config.graphql_url, &config.allow_http_hosts)?;
        crate::platform_http::endpoint(&config.mcp_url, &config.allow_http_hosts)?;
        Ok(Self { config, auth })
    }
    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let token = self.auth.access_token().await?;
        let value = crate::platform_http::json(
            &crate::platform_http::endpoint(
                &self.config.graphql_url,
                &self.config.allow_http_hosts,
            )?,
            &[
                ("Authorization", format!("Bearer {}", token.expose())),
                ("x-hd-tenant", self.config.auth.tenant_slug.clone()),
            ],
            &json!({"query":query,"variables":variables}),
            60,
        )
        .await?;
        ensure!(
            value
                .get("errors")
                .is_none_or(|e| e.as_array().is_some_and(Vec::is_empty)),
            "herodevs GraphQL rejected operation"
        );
        value
            .get("data")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("GraphQL data missing"))
    }
    pub async fn post_job(&self, input: &PostJob) -> Result<ChannelMessage> {
        validate_metadata(&input.metadata, false)?;
        ensure!(
            crate::session_search::valid_tenant(&input.client_request_id),
            "clientRequestId must be a UUID"
        );
        ensure!(
            !input.content.trim().is_empty()
                && input.content.len() <= 65536
                && input.priority <= 10
                && [
                    "MERGE", "DEPLOY", "CI_WATCH", "REVIEW", "IMPL", "PLAN", "OTHER"
                ]
                .contains(&input.job_type.as_str()),
            "invalid job input"
        );
        let data = self.graphql(&format!("mutation($sender:ID!,$input:PostChannelJobInput!){{channelMutations{{postJob(senderAgentId:$sender,input:$input){{{FIELDS}}}}}}}"), json!({"sender":self.config.auth.expected_subject,"input":input})).await?;
        message(data.pointer("/channelMutations/postJob"))
    }
    pub async fn list_jobs(
        &self,
        channel: &str,
        states: &[String],
        metadata: Value,
        limit: usize,
    ) -> Result<Vec<ChannelMessage>> {
        validate_metadata(&metadata, true)?;
        ensure!((1..=100).contains(&limit), "invalid job limit");
        let data = self.graphql(&format!("query($channel:ID!,$states:[JobState!],$metadata:Object,$first:Int){{tenant{{listJobs(channelId:$channel,jobStates:$states,metadata:$metadata,first:$first){{{FIELDS}}}}}}}"), json!({"channel":channel,"states":if states.is_empty(){None}else{Some(states)},"metadata":metadata,"first":limit})).await?;
        serde_json::from_value(
            data.pointer("/tenant/listJobs")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("jobs missing"))?,
        )
        .map_err(|_| anyhow::anyhow!("invalid jobs"))
    }
    pub async fn claim_job(&self, id: &str, ttl: u32) -> Result<Option<ChannelMessage>> {
        ensure!((1..=3600).contains(&ttl), "invalid lease TTL");
        let data = self.graphql(&format!("mutation($id:ID!,$ttl:Int){{channelMutations{{claimJob(messageId:$id,leaseTtlSeconds:$ttl){{{FIELDS}}}}}}}"), json!({"id":id,"ttl":ttl})).await?;
        let value = data
            .pointer("/channelMutations/claimJob")
            .ok_or_else(|| anyhow::anyhow!("claim response missing"))?;
        if value.is_null() {
            Ok(None)
        } else {
            message(Some(value)).map(Some)
        }
    }
    pub async fn transition(
        &self,
        operation: &str,
        id: &str,
        fence: i64,
        detail: Value,
    ) -> Result<ChannelMessage> {
        ensure!(
            [
                "startJob",
                "completeJob",
                "failJob",
                "escalateJob",
                "releaseClaim",
                "renewClaim"
            ]
            .contains(&operation),
            "invalid job transition"
        );
        let (declaration, argument) = match operation {
            "completeJob" | "failJob" => (",$detail:Object", ",outcome:$detail"),
            "escalateJob" => (",$detail:String", ",reason:$detail"),
            "renewClaim" => (",$detail:Int", ",leaseTtlSeconds:$detail"),
            _ => ("", ""),
        };
        let data = self.graphql(&format!("mutation($id:ID!,$fence:Int!{declaration}){{channelMutations{{{operation}(messageId:$id,fenceToken:$fence{argument}){{{FIELDS}}}}}}}"), json!({"id":id,"fence":fence,"detail":detail})).await?;
        message(data.pointer(&format!("/channelMutations/{operation}")))
    }
    pub async fn send_message(
        &self,
        channel: &str,
        content: &str,
        metadata: Value,
        request_id: &str,
    ) -> Result<ChannelMessage> {
        validate_metadata(&metadata, false)?;
        ensure!(content.len() <= 65536, "channel message exceeded bound");
        let data = self.graphql(&format!("mutation($sender:ID!,$input:SendChannelMessageInput!){{channelMutations{{sendMessage(senderAgentId:$sender,input:$input){{{FIELDS}}}}}}}"), json!({"sender":self.config.auth.expected_subject,"input":{"channelId":channel,"content":content,"metadata":metadata,"clientRequestId":request_id}})).await?;
        message(data.pointer("/channelMutations/sendMessage"))
    }
    pub async fn history(&self, channel: &str, after: Option<&str>, limit: usize) -> Result<Value> {
        ensure!((1..=100).contains(&limit), "invalid history limit");
        self.graphql(&format!("query($channel:ID!,$after:String,$first:Int){{tenant{{channel(id:$channel){{messages(first:$first,after:$after){{edges{{cursor node{{{FIELDS}}}}}pageInfo{{endCursor hasNextPage}}}}}}}}}}"), json!({"channel":channel,"after":after,"first":limit})).await
    }
    /// Stateless MCP tools/call; supports JSON and bounded SSE responses.
    pub async fn search(&self, arguments: Value) -> Result<Value> {
        let token = self.auth.access_token().await?;
        let (status, bytes) = crate::platform_http::request(&crate::platform_http::endpoint(&self.config.mcp_url, &self.config.allow_http_hosts)?, "POST", &[("Authorization",format!("Bearer {}",token.expose())),("x-hd-tenant",self.config.auth.tenant_slug.clone()),("Content-Type","application/json".into()),("Accept","application/json, text/event-stream".into()),("MCP-Protocol-Version","2025-03-26".into())], serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search.query","arguments":arguments}}))?,60,2*1024*1024).await?;
        ensure!(status == 200, "MCP HTTP status {status}");
        let value: Value = serde_json::from_slice(&bytes)
            .or_else(|_| {
                let data = std::str::from_utf8(&bytes)
                    .unwrap_or("")
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .find(|s| s.contains("\"id\""))
                    .unwrap_or("");
                serde_json::from_str(data)
            })
            .map_err(|_| anyhow::anyhow!("invalid MCP response"))?;
        ensure!(
            value.get("error").is_none()
                && value.pointer("/result/isError") != Some(&Value::Bool(true)),
            "MCP search rejected"
        );
        let result = value
            .get("result")
            .ok_or_else(|| anyhow::anyhow!("MCP result missing"))?;
        if let Some(value) = result.get("structuredContent") {
            return Ok(value.clone());
        }
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("MCP search content missing"))?;
        serde_json::from_str(text).map_err(|_| anyhow::anyhow!("invalid MCP search content"))
    }
}
fn message(value: Option<&Value>) -> Result<ChannelMessage> {
    serde_json::from_value(
        value
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("job response missing"))?,
    )
    .map_err(|_| anyhow::anyhow!("invalid job response"))
}
pub fn validate_metadata(value: &Value, filter: bool) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("metadata must be an object"))?;
    ensure!(
        object.len() <= if filter { 8 } else { 64 } && serde_json::to_vec(value)?.len() <= 8192,
        "metadata exceeded bound"
    );
    let mut stack = vec![(value, 0)];
    let mut count = 0;
    while let Some((v, depth)) = stack.pop() {
        count += 1;
        ensure!(
            depth <= 8 && count <= 256,
            "metadata nesting exceeded bound"
        );
        match v {
            Value::Object(map) => {
                for (k, v) in map {
                    ensure!(
                        !k.trim().is_empty() && k.chars().count() <= 128,
                        "invalid metadata key"
                    );
                    stack.push((v, depth + 1));
                }
            }
            Value::Array(values) => {
                for v in values {
                    stack.push((v, depth + 1));
                }
            }
            _ => {}
        }
    }
    ensure!(
        !filter || object.values().all(|v| !v.is_array() && !v.is_object()),
        "metadata filters must be scalar"
    );
    Ok(())
}

/// Stable opaque retry key scoped by the platform to channel and sender.
#[must_use]
pub fn client_request_id(key: &str) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut h = String::with_capacity(32);
    for byte in bytes {
        let _ = write!(h, "{byte:02x}");
    }
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        routing::{get, post},
    };
    use std::sync::Mutex;
    #[derive(Clone)]
    struct Fixture {
        issuer: Arc<Mutex<String>>,
        nonce: Arc<Mutex<String>>,
        polls: Arc<Mutex<usize>>,
    }
    fn signed(value: &Value) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some("fixture".into());
        jsonwebtoken::encode(
            &header,
            value,
            &jsonwebtoken::EncodingKey::from_rsa_der(include_bytes!(
                "../tests/fixtures/intake/test-only-key.der"
            )),
        )
        .unwrap()
    }
    async fn keys() -> Json<Value> {
        Json(serde_json::from_str(include_str!("../tests/fixtures/intake/jwks.json")).unwrap())
    }
    async fn discovery(State(f): State<Fixture>) -> Json<Value> {
        let issuer = f.issuer.lock().unwrap().clone();
        Json(
            json!({"issuer":issuer,"device_authorization_endpoint":format!("{issuer}/device"),"token_endpoint":format!("{issuer}/token"),"jwks_uri":format!("{issuer}/keys")}),
        )
    }
    async fn device(State(f): State<Fixture>, body: String) -> Json<Value> {
        let fields: std::collections::HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields["tenant_slug"], "hq");
        assert_eq!(fields["scope"], "openid profile offline_access");
        *f.nonce.lock().unwrap() = fields["nonce"].clone();
        Json(
            json!({"verification_uri":format!("{}/verify",f.issuer.lock().unwrap()),"user_code":"PUBLIC-CODE","device_code":"PRIVATE-CODE","expires_in":60,"interval":5}),
        )
    }
    fn access(f: &Fixture) -> Value {
        json!({"iss":f.issuer.lock().unwrap().clone(),"sub":"ryan","azp":"hd-atmux","tenant_id":"hq","identity_type":"USER","aud":["hd-subgraphs","https://hq.herodevs.dev/hd-mcp"],"exp":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()+300})
    }
    async fn token(State(f): State<Fixture>, body: String) -> Json<Value> {
        let fields: std::collections::HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(fields["tenant_slug"], "hq");
        assert_eq!(fields["client_secret"], "fixture-secret");
        let mut count = f.polls.lock().unwrap();
        *count += 1;
        if *count == 1 {
            return Json(json!({"error":"authorization_pending"}));
        }
        let mut id = access(&f);
        id["aud"] = json!("hd-atmux");
        id["nonce"] = json!(f.nonce.lock().unwrap().clone());
        Json(
            json!({"access_token":signed(&access(&f)),"id_token":signed(&id),"refresh_token":"offline-refresh","scope":"openid profile offline_access","token_type":"Bearer"}),
        )
    }
    #[tokio::test]
    async fn device_login_and_all_jwt_identity_pins() {
        let f = Fixture {
            issuer: Arc::new(Mutex::new(String::new())),
            nonce: Arc::new(Mutex::new(String::new())),
            polls: Arc::new(Mutex::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        *f.issuer.lock().unwrap() = issuer.clone();
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/keys", get(keys))
            .route("/device", post(device))
            .route("/token", post(token))
            .with_state(f.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let directory = std::env::temp_dir().join(format!(
            "atmux-device-test-{}",
            crate::tmux::new_session_key().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let secret = directory.join("secret");
        std::fs::write(&secret, "fixture-secret").unwrap();
        let auth = DeviceAuth::new(AuthConfig {
            issuer: issuer.clone(),
            client_secret_file: secret,
            refresh_token_file: directory.join("refresh"),
            expected_subject: "ryan".into(),
            allow_http_hosts: vec!["127.0.0.1".into()],
            ..AuthConfig::default()
        })
        .unwrap();
        let login = auth.begin_login().await.unwrap();
        assert_eq!(login.user_code, "PUBLIC-CODE");
        auth.finish_login(login).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(directory.join("refresh")).unwrap(),
            "offline-refresh"
        );
        auth.access_token().await.unwrap();
        assert_eq!(*f.polls.lock().unwrap(), 2);
        let jwks = format!("{issuer}/keys");
        for (field, value) in [
            ("sub", json!("someone-else")),
            ("iss", json!("https://other.test")),
            ("tenant_id", json!("other")),
            ("azp", json!("other-client")),
            ("identity_type", json!("AGENT")),
            ("aud", json!(["hd-subgraphs"])),
            ("exp", json!(1)),
            (
                "nbf",
                json!(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 3600
                ),
            ),
        ] {
            let mut claims = access(&f);
            claims[field] = value;
            assert!(
                auth.verify(&signed(&claims), &jwks, None).await.is_err(),
                "pin {field}"
            );
        }
        let mut claims = access(&f);
        claims["aud"] = json!("hd-atmux");
        claims["nonce"] = json!("wrong-nonce");
        assert!(
            auth.verify(&signed(&claims), &jwks, Some("original-nonce"))
                .await
                .is_err()
        );
        task.abort();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
