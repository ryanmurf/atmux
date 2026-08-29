use std::{fmt, str::FromStr};

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use super::error::{PulseError, PulseResult};

/// An absolute instant stored as Unix epoch milliseconds.
///
/// `SQLite` persists the integer returned by [`Self::epoch_millis`]. JSON and
/// TOML use an ISO-8601 string, preventing timestamp text from leaking into
/// ordering logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(with = "String")]
pub struct Instant(i64);

impl Instant {
    /// Creates an instant after checking it is in Jiff's supported range.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for an out-of-range epoch value.
    pub fn from_epoch_millis(epoch_millis: i64) -> PulseResult<Self> {
        Timestamp::from_millisecond(epoch_millis).map_err(|error| {
            PulseError::invalid_input(format!("invalid epoch millisecond instant: {error}"))
        })?;
        Ok(Self(epoch_millis))
    }

    /// Parses an ISO-8601 timestamp and normalizes it to millisecond precision.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error when `value` is not a timestamp.
    pub fn from_iso8601(value: &str) -> PulseResult<Self> {
        let timestamp = Timestamp::from_str(value).map_err(|error| {
            PulseError::invalid_input(format!("invalid ISO-8601 instant: {error}"))
        })?;
        Self::from_epoch_millis(timestamp.as_millisecond())
    }

    /// Current wall-clock time, normalized to milliseconds.
    #[must_use]
    pub fn now() -> Self {
        // Every timestamp the platform can produce is inside Jiff's supported
        // range. Keeping this infallible makes call sites no less safe than
        // `SystemTime::now` while construction from storage remains checked.
        Self(Timestamp::now().as_millisecond())
    }

    #[must_use]
    pub const fn epoch_millis(self) -> i64 {
        self.0
    }

    /// Formats the instant as canonical UTC ISO-8601 text.
    #[must_use]
    pub fn to_iso8601(self) -> String {
        Timestamp::from_millisecond(self.0)
            // `Instant` has private fields and every constructor validates the
            // range. The fallback keeps formatting infallible even if that
            // invariant is broken by a future internal change.
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_string()
    }
}

impl fmt::Display for Instant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_iso8601())
    }
}

impl FromStr for Instant {
    type Err = PulseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_iso8601(value)
    }
}

impl Serialize for Instant {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_iso8601())
    }
}

impl<'de> Deserialize<'de> for Instant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_iso8601(&value).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_iso_but_storage_value_is_epoch_millis() {
        let instant = Instant::from_epoch_millis(1_786_214_400_123).expect("valid instant");
        assert_eq!(instant.epoch_millis(), 1_786_214_400_123);
        assert_eq!(
            serde_json::to_string(&instant).expect("serialize"),
            "\"2026-08-08T18:40:00.123Z\""
        );
        assert_eq!(
            serde_json::from_str::<Instant>("\"2026-08-08T18:40:00.123456Z\"")
                .expect("deserialize")
                .epoch_millis(),
            1_786_214_400_123
        );
    }

    #[test]
    fn serde_rejects_numeric_or_invalid_timestamp_text() {
        assert!(serde_json::from_str::<Instant>("1786214400123").is_err());
        assert!(serde_json::from_str::<Instant>("\"yesterday\"").is_err());
    }

    #[test]
    fn rejects_epochs_outside_supported_range() {
        assert!(Instant::from_epoch_millis(i64::MAX).is_err());
        assert!(Instant::from_epoch_millis(i64::MIN).is_err());
    }
}
