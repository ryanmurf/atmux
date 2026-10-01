//! Shared byte producer for lifecycle and entity-change publications.
use super::{AgentEvent, EventLog, EventQuery, MAX_EVENT_BYTES};
use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use rskafka::{
    client::{
        Client, ClientBuilder, Credentials, SaslConfig,
        partition::{Compression, UnknownTopicHandling},
    },
    record::Record,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RedpandaConfig {
    pub brokers: Vec<String>,
    pub topic: String,
    pub tenant_id: String,
    pub tls: bool,
    pub ca_file: Option<PathBuf>,
    pub sasl: Option<String>,
    pub username_env: Option<String>,
    pub username_file: Option<PathBuf>,
    pub password_env: Option<String>,
    pub password_file: Option<PathBuf>,
}
impl Default for RedpandaConfig {
    fn default() -> Self {
        Self {
            brokers: vec!["redpanda.herodevs.svc.cluster.local:9092".into()],
            topic: "atmux.agent.events.v1".into(),
            tenant_id: "95efe33d-fa71-53ce-8e0a-3fe45ac0e58a".into(),
            tls: false,
            ca_file: None,
            sasl: None,
            username_env: None,
            username_file: None,
            password_env: None,
            password_file: None,
        }
    }
}
impl RedpandaConfig {
    /// # Errors
    /// Rejects malformed brokers, auth references, topic or tenant id.
    pub fn validate(&self) -> Result<()> {
        if self.brokers.is_empty()
            || self.brokers.len() > 16
            || self.brokers.iter().any(|v| {
                v.len() > 255
                    || !v.contains(':')
                    || v.contains(['/', '@', '?', '#'])
                    || v.chars().any(char::is_whitespace)
            })
        {
            bail!("invalid Redpanda brokers");
        }
        validate_topic(&self.topic)?;
        let bytes = self.tenant_id.as_bytes();
        if bytes.len() != 36
            || !bytes.iter().enumerate().all(|(i, c)| {
                if [8, 13, 18, 23].contains(&i) {
                    *c == b'-'
                } else {
                    c.is_ascii_hexdigit()
                }
            })
        {
            bail!("Redpanda tenant_id must be a UUID");
        }
        if self
            .sasl
            .as_deref()
            .is_some_and(|v| !["plain", "scram-sha-256", "scram-sha-512"].contains(&v))
            || self.username_env.is_some() && self.username_file.is_some()
            || self.password_env.is_some() && self.password_file.is_some()
        {
            bail!("invalid Redpanda auth references");
        }
        let user = self.username_env.is_some() || self.username_file.is_some();
        let pass = self.password_env.is_some() || self.password_file.is_some();
        if self.sasl.is_some() != user || user != pass || self.ca_file.is_some() && !self.tls {
            bail!("incomplete Redpanda authentication configuration");
        }
        Ok(())
    }
}

pub(crate) fn validate_topic(topic: &str) -> Result<()> {
    if topic.is_empty()
        || topic.len() > 249
        || matches!(topic, "." | "..")
        || !topic
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'.' | b'_' | b'-'))
    {
        bail!("invalid Kafka topic");
    }
    Ok(())
}

pub(crate) fn validate_publication(topic: &str, key: &[u8], value: &[u8]) -> Result<()> {
    validate_topic(topic)?;
    if key.len() > 4096 || value.len() > 128 * 1024 {
        bail!("Kafka publication exceeds bounds");
    }
    Ok(())
}

pub type PublishFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
pub trait Producer: Send + Sync {
    /// Publishes one bounded value. Callers decide retry/checkpoint policy.
    fn publish<'a>(&'a self, topic: &'a str, key: &'a [u8], value: &'a [u8]) -> PublishFuture<'a>;
}

pub struct KafkaProducer {
    client: Client,
    partitions: Mutex<HashMap<String, Vec<i32>>>,
}
impl KafkaProducer {
    /// # Errors
    /// Rejects invalid settings, unavailable secrets, TLS roots or broker errors.
    pub async fn connect(config: &RedpandaConfig) -> Result<Self> {
        config.validate()?;
        let mut builder = ClientBuilder::new(config.brokers.clone())
            .client_id("atmux")
            .max_message_size(800 * 1024);
        if config.tls {
            let mut roots = rustls::RootCertStore::empty();
            if let Some(path) = &config.ca_file {
                if std::fs::metadata(path)?.len() > 1024 * 1024 {
                    bail!("Redpanda CA exceeds bounds");
                }
                for cert in
                    rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(path)?))
                {
                    roots.add(cert?)?;
                }
            } else {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            let provider = rustls::crypto::aws_lc_rs::default_provider();
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
            builder = builder.tls_config(Arc::new(tls));
        }
        if let Some(mechanism) = &config.sasl {
            let credentials = Credentials {
                username: secret(
                    config.username_env.as_deref(),
                    config.username_file.as_deref(),
                )?,
                password: secret(
                    config.password_env.as_deref(),
                    config.password_file.as_deref(),
                )?,
            };
            builder = builder.sasl_config(match mechanism.as_str() {
                "plain" => SaslConfig::Plain(credentials),
                "scram-sha-256" => SaslConfig::ScramSha256(credentials),
                _ => SaslConfig::ScramSha512(credentials),
            });
        }
        Ok(Self {
            client: tokio::time::timeout(Duration::from_secs(10), builder.build()).await??,
            partitions: Mutex::new(HashMap::new()),
        })
    }
}
fn secret(env: Option<&str>, file: Option<&std::path::Path>) -> Result<String> {
    let value = if let Some(env) = env {
        std::env::var(env).context("Redpanda secret environment variable is unavailable")?
    } else {
        let path = file.context("Redpanda secret reference is missing")?;
        super::spool::private_file(path)?;
        if std::fs::metadata(path)?.len() > 4096 {
            bail!("Redpanda secret exceeds bounds");
        }
        std::fs::read_to_string(path)?
    };
    let value = value.trim();
    if value.is_empty() || value.len() > 4096 {
        bail!("invalid Redpanda secret length");
    }
    Ok(value.to_owned())
}
impl Producer for KafkaProducer {
    fn publish<'a>(&'a self, topic: &'a str, key: &'a [u8], value: &'a [u8]) -> PublishFuture<'a> {
        Box::pin(async move {
            validate_publication(topic, key, value)?;
            let operation = async {
                let mut partitions = self.partitions.lock().await;
                if !partitions.contains_key(topic) {
                    if partitions.len() >= 16 {
                        partitions.clear();
                    }
                    let found = self
                        .client
                        .list_topics()
                        .await?
                        .into_iter()
                        .find(|v| v.name == topic)
                        .context("Kafka topic is not provisioned")?;
                    if found.partitions.is_empty() {
                        bail!("Kafka topic has no partitions");
                    }
                    partitions.insert(topic.into(), found.partitions.into_iter().collect());
                }
                let choices = &partitions[topic];
                // Deterministic routing keeps every session on one partition.
                let hash = key.iter().fold(2_166_136_261_u32, |hash, byte| {
                    (hash ^ u32::from(*byte)).wrapping_mul(16_777_619)
                });
                let partition = choices[hash as usize % choices.len()];
                drop(partitions);
                let client = self
                    .client
                    .partition_client(topic, partition, UnknownTopicHandling::Error)
                    .await?;
                client
                    .produce(
                        vec![Record {
                            key: Some(key.to_vec()),
                            value: Some(value.to_vec()),
                            headers: BTreeMap::new(),
                            timestamp: Utc::now(),
                        }],
                        Compression::NoCompression,
                    )
                    .await?;
                Ok::<_, anyhow::Error>(())
            };
            tokio::time::timeout(Duration::from_secs(10), operation).await??;
            Ok(())
        })
    }
}

/// Platform v2 wrapper: tenantId must occur in both security and payload.
/// # Errors
/// Rejects malformed envelopes or tenant configuration.
pub fn platform_envelope(event: &AgentEvent, config: &RedpandaConfig) -> Result<Vec<u8>> {
    event.validate()?;
    config.validate()?;
    let mut payload = serde_json::to_value(event)?;
    payload
        .as_object_mut()
        .context("event payload is not an object")?
        .insert("tenantId".into(), json!(config.tenant_id));
    let envelope = json!({"envelopeVersion":2, "eventType":"atmux.agent.event.v1", "publishedAt":Utc::now().to_rfc3339(), "securityContext":{"tenantId":config.tenant_id, "token":null, "platform":"SYSTEM", "userId":null, "sessionId":null}, "mdc":{}, "payloadRef":null, "payloadSummary":null, "eventPayload":payload});
    let bytes = serde_json::to_vec(&envelope)?;
    if bytes.len() > MAX_EVENT_BYTES + 4096 {
        bail!("platform event exceeds bounds");
    }
    Ok(bytes)
}

pub(crate) async fn publish_page(
    log: &EventLog,
    config: &RedpandaConfig,
    producer: &dyn Producer,
    after: Option<String>,
) -> Result<String> {
    let page = log
        .read(&EventQuery {
            after,
            limit: Some(25),
            ..EventQuery::default()
        })
        .await?;
    for record in &page.events {
        let value = platform_envelope(&record.event, config)?;
        producer
            .publish(&config.topic, record.event.session_key.as_bytes(), &value)
            .await?;
    }
    // Only a completely acknowledged page advances the durable checkpoint.
    Ok(page.next)
}
