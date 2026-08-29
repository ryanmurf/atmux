use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::control::ErrorKind;

/// Stable categories shared by collectors, stores, and API adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PulseErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    Offline,
    Authentication,
    RateLimited,
    Upstream,
    Storage,
    Configuration,
    Internal,
}

impl PulseErrorKind {
    /// Whether retrying later can succeed without changing the request.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Offline | Self::RateLimited | Self::Upstream | Self::Storage
        )
    }

    /// Maps Pulse-specific categories to the existing control-plane contract.
    #[must_use]
    pub const fn control_kind(self) -> ErrorKind {
        match self {
            Self::InvalidInput => ErrorKind::BadRequest,
            Self::NotFound => ErrorKind::NotFound,
            Self::Conflict => ErrorKind::Conflict,
            Self::Offline => ErrorKind::Offline,
            Self::Authentication | Self::RateLimited | Self::Upstream => ErrorKind::Upstream,
            Self::Storage | Self::Configuration | Self::Internal => ErrorKind::Internal,
        }
    }
}

/// A classified Pulse failure with a secret-free, user-facing message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PulseError {
    kind: PulseErrorKind,
    message: String,
}

impl PulseError {
    #[must_use]
    pub fn new(kind: PulseErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> PulseErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn control_kind(&self) -> ErrorKind {
        self.kind.control_kind()
    }

    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(PulseErrorKind::InvalidInput, message)
    }

    #[must_use]
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::new(PulseErrorKind::Configuration, message)
    }
}

impl fmt::Display for PulseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PulseError {}

impl From<&PulseError> for ErrorKind {
    fn from(error: &PulseError) -> Self {
        error.control_kind()
    }
}

pub type PulseResult<T> = Result<T, PulseError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_maps_to_existing_control_errors() {
        assert_eq!(
            PulseErrorKind::InvalidInput.control_kind(),
            ErrorKind::BadRequest
        );
        assert_eq!(PulseErrorKind::NotFound.control_kind(), ErrorKind::NotFound);
        assert_eq!(PulseErrorKind::Conflict.control_kind(), ErrorKind::Conflict);
        assert_eq!(PulseErrorKind::Offline.control_kind(), ErrorKind::Offline);
        assert_eq!(
            PulseErrorKind::Authentication.control_kind(),
            ErrorKind::Upstream
        );
        assert_eq!(
            PulseErrorKind::RateLimited.control_kind(),
            ErrorKind::Upstream
        );
        assert_eq!(PulseErrorKind::Upstream.control_kind(), ErrorKind::Upstream);
        assert_eq!(PulseErrorKind::Storage.control_kind(), ErrorKind::Internal);
        assert_eq!(
            PulseErrorKind::Configuration.control_kind(),
            ErrorKind::Internal
        );
        assert_eq!(PulseErrorKind::Internal.control_kind(), ErrorKind::Internal);
    }

    #[test]
    fn retryability_is_explicit() {
        assert!(PulseErrorKind::RateLimited.is_retryable());
        assert!(PulseErrorKind::Storage.is_retryable());
        assert!(!PulseErrorKind::Authentication.is_retryable());
        assert!(!PulseErrorKind::InvalidInput.is_retryable());
    }
}
