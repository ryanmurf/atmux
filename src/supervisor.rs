//! Opt-in coordinator supervision. Model output selects a bounded policy action;
//! it never supplies commands, destinations, credentials, or completion evidence.
mod config;
mod owner;
mod policy;
mod runtime;
pub use owner::{Guard, GuardedMessage};
mod store;
pub use config::{Action, SupervisorConfig};
pub use policy::{
    Classification, Completion, Job, JobFact, PullRequest, classify, completion,
    permission_allowed, verify,
};
pub use runtime::{Agents, Audit, BoxFuture, Context, Model, Platform, Supervisor};

#[cfg(test)]
mod tests;
