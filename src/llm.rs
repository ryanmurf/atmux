//! Shared OpenAI-compatible chat client. Output is data; callers validate actions.
#![allow(clippy::missing_errors_doc)]
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key_file: Option<PathBuf>,
    pub allow_http_hosts: Vec<String>,
    pub timeout_seconds: u64,
    pub max_tokens: u32,
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://192.168.0.124:8091/v1".into(),
            model: "qwen3.8-flash-next".into(),
            api_key_file: None,
            allow_http_hosts: vec![],
            timeout_seconds: 90,
            max_tokens: 4096,
        }
    }
}
impl LlmConfig {
    pub fn validate(&self) -> Result<()> {
        crate::platform_http::endpoint(&self.endpoint, &self.allow_http_hosts)?;
        ensure!(
            !self.model.trim().is_empty()
                && self.model.len() <= 200
                && !self.model.chars().any(char::is_control)
                && (1..=300).contains(&self.timeout_seconds)
                && (1..=8192).contains(&self.max_tokens),
            "invalid LLM bounds"
        );
        Ok(())
    }
}
pub struct LlmClient {
    config: LlmConfig,
    endpoint: url::Url,
    key: Option<crate::herodevs::Secret>,
}
impl LlmClient {
    pub fn new(config: LlmConfig) -> Result<Self> {
        config.validate()?;
        let endpoint = crate::platform_http::endpoint(
            &format!("{}/chat/completions", config.endpoint.trim_end_matches('/')),
            &config.allow_http_hosts,
        )?;
        let key = config
            .api_key_file
            .as_ref()
            .map(|p| crate::herodevs::read_secret(p))
            .transpose()?;
        Ok(Self {
            config,
            endpoint,
            key,
        })
    }
    pub fn with_key(config: LlmConfig, key: crate::herodevs::Secret) -> Result<Self> {
        let mut client = Self::new(config)?;
        client.key = Some(key);
        Ok(client)
    }
    pub async fn complete(&self, system: &str, prompt: &str) -> Result<String> {
        ensure!(
            system.len() <= 8192 && prompt.len() <= 96 * 1024,
            "LLM prompt exceeded bound"
        );
        let headers = self
            .key
            .as_ref()
            .map(|k| vec![("Authorization", format!("Bearer {}", k.expose()))])
            .unwrap_or_default();
        let value = crate::platform_http::json(&self.endpoint, &headers, &serde_json::json!({"model":self.config.model,"temperature":0.1,"max_tokens":self.config.max_tokens,"messages":[{"role":"system","content":system},{"role":"user","content":prompt}]}), self.config.timeout_seconds).await?;
        let content = value
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("LLM response missing content"))?;
        ensure!(content.len() <= 64 * 1024, "LLM output exceeded bound");
        Ok(content.to_owned())
    }
    pub async fn json<T: DeserializeOwned>(&self, system: &str, prompt: &str) -> Result<T> {
        let content = self.complete(system, prompt).await?;
        serde_json::from_str(&content).map_err(|_| {
            anyhow::anyhow!("LLM must return strict JSON matching the requested schema")
        })
    }
}
