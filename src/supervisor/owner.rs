use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Guard {
    pub session_key: String,
    pub instance_id: String,
    pub content_hash: String,
    /// Nudges may target a working pane; closing always requires waiting.
    pub status: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuardedMessage {
    pub guard: Guard,
    pub text: String,
}
