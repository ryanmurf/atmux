//! Native usage, context, token, and alert monitoring.
//!
//! The domain types in this module deliberately contain neither raw provider
//! responses nor credential values. Collectors project responses into these
//! types before persistence or transport.

#![forbid(unsafe_code)]

pub mod alerts;
pub mod api;
pub mod collect;
pub mod config;
pub mod context;
pub mod credentials;
pub mod delivery;
pub mod error;
pub mod federation;
pub mod health;
pub mod import;
pub mod ingest;
pub mod invalidation;
pub mod model;
pub mod native;
pub mod ops;
pub mod preflight;
pub mod pricing;
pub mod reporter;
pub mod reports;
pub mod reset;
pub mod scheduler;
pub mod service;
pub mod store;
pub mod time;
pub mod token;

pub use config::{
    PulseAccountConfig, PulseConfig, PulseCredentialConfig, PulseDatabaseConfig,
    PulseProfileConfig, PulseRetentionConfig, PulseScheduleConfig,
};
pub use error::{PulseError, PulseErrorKind, PulseResult};
pub use model::{
    Account, AccountId, AgentSettings, AlertDelivery, AlertSubscription, AlertType,
    CollectionOutcome, ContextSession, Fraction, GeminiQuota, Machine, MachineName, Percent,
    Profile, ProfileName, ProfileOrigin, QuotaWindow, QuotaWindowKind, RefreshPolicy, SessionId,
    TokenGrain, TokenSource, UsageContributor, UsageSnapshot, Vendor,
};
pub use time::Instant;
