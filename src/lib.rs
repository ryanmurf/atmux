pub(crate) const MAX_REQUEST_BODY_BYTES: usize = 512 * 1024;

pub mod app;
pub mod attachment;
mod auto_compact;
mod auto_update;
pub mod config;
pub mod control;
pub mod discovery;
mod launch_directory;
pub mod machine;
pub mod mcp;
pub mod metrics;
pub mod old_sessions;
pub mod project;
#[cfg(feature = "pulse")]
pub mod pulse;
pub mod recovery;
pub mod remote;
pub mod status;
mod systemd_scope;
pub mod tls;
pub mod tmux;
pub mod transcript;
pub mod ui;
pub mod web;
pub mod workspace;
