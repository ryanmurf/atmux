//! Native, secret-free collector and gauge diagnostics.

use super::{
    CollectionOutcome, Instant, MachineName, Profile, PulseResult, UsageSnapshot, Vendor,
    collect::SecretRef,
    credentials::{
        CodexCredentialState, CredentialState, inspect_claude_credentials,
        inspect_codex_credentials,
    },
    store::{Store, StoredUsageSnapshot},
};
use schemars::JsonSchema;
use serde::Serialize;

const MIN_FRESH_MILLIS: i64 = 30 * 60 * 1_000;
const HISTORY_LIMIT: usize = 256;

/// Credential health without any credential value or provider identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "provider", content = "state", rename_all = "snake_case")]
pub enum ProfileCredentialHealth {
    Claude(CredentialState),
    Codex(CodexCredentialState),
    ExternalReferenceConfigured,
    ExternalReferenceUnavailable,
    NotApplicable,
    MissingConfiguration,
}

/// Distinct gauge states surfaced by `pulse doctor` and the Usage UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GaugeHealth {
    NotApplicable,
    DeadNoObservation,
    AuthenticationFailed,
    NullSignal,
    Stale,
    AuthenticatedUnchanged,
    Healthy,
}

/// One local machine/profile diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProfileGaugeHealth {
    pub profile: super::ProfileName,
    pub vendor: Vendor,
    pub machine: MachineName,
    pub credential: ProfileCredentialHealth,
    pub gauge: GaugeHealth,
    pub last_polled_at: Option<Instant>,
}

/// Builds bounded local-machine health for configured profiles.
///
/// # Errors
///
/// Returns an account-scoped store failure. Credential inspection remains
/// fail-soft and only contributes a typed state.
pub async fn collect_gauge_health(
    store: &dyn Store,
    profiles: &[Profile],
    machine: &MachineName,
    now: Instant,
) -> PulseResult<Vec<ProfileGaugeHealth>> {
    let mut output = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let history = if profile.vendor.emits_usage_snapshots() {
            store
                .usage_history(
                    profile.account_id,
                    profile.name.clone(),
                    None,
                    HISTORY_LIMIT,
                )
                .await?
                .into_iter()
                .filter(|stored| stored.snapshot.machine == *machine)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let credential_profile = profile.clone();
        let credential = tokio::task::spawn_blocking(move || {
            inspect_profile_credentials(&credential_profile, now)
        })
        .await
        .unwrap_or(ProfileCredentialHealth::MissingConfiguration);
        let gauge = classify_gauge(profile, &history, now);
        output.push(ProfileGaugeHealth {
            profile: profile.name.clone(),
            vendor: profile.vendor,
            machine: machine.clone(),
            credential,
            gauge,
            last_polled_at: history.first().map(|stored| stored.snapshot.polled_at),
        });
    }
    Ok(output)
}

/// Classifies gauge liveness without conflating authentication, no signal,
/// stale scheduling, and a successfully polled but unchanged allowance.
#[must_use]
pub fn classify_gauge(
    profile: &Profile,
    history: &[StoredUsageSnapshot],
    now: Instant,
) -> GaugeHealth {
    if !profile.vendor.emits_usage_snapshots() {
        return GaugeHealth::NotApplicable;
    }
    let Some(latest) = history.first().map(|stored| &stored.snapshot) else {
        return GaugeHealth::DeadNoObservation;
    };
    match latest.outcome {
        CollectionOutcome::AuthenticationFailed { .. } => {
            return GaugeHealth::AuthenticationFailed;
        }
        CollectionOutcome::Success => {}
        CollectionOutcome::Disabled { .. }
        | CollectionOutcome::RateLimited { .. }
        | CollectionOutcome::Unavailable { .. }
        | CollectionOutcome::InvalidResponse { .. } => return GaugeHealth::NullSignal,
    }
    let fresh_millis = i64::from(profile.poll_interval_minutes)
        .saturating_mul(2)
        .saturating_mul(60 * 1_000)
        .max(MIN_FRESH_MILLIS);
    if now
        .epoch_millis()
        .saturating_sub(latest.polled_at.epoch_millis())
        > fresh_millis
    {
        return GaugeHealth::Stale;
    }
    if let Some(previous) = previous_success(history)
        && latest
            .polled_at
            .epoch_millis()
            .saturating_sub(previous.polled_at.epoch_millis())
            >= fresh_millis
        && latest.windows == previous.windows
    {
        return GaugeHealth::AuthenticatedUnchanged;
    }
    GaugeHealth::Healthy
}

fn previous_success(history: &[StoredUsageSnapshot]) -> Option<&UsageSnapshot> {
    history
        .iter()
        .skip(1)
        .map(|stored| &stored.snapshot)
        .find(|snapshot| snapshot.outcome.is_success())
}

fn inspect_profile_credentials(profile: &Profile, now: Instant) -> ProfileCredentialHealth {
    match profile.vendor {
        Vendor::AnthropicOauth => profile.config_dir.as_deref().map_or(
            ProfileCredentialHealth::MissingConfiguration,
            |directory| {
                ProfileCredentialHealth::Claude(
                    inspect_claude_credentials(directory, now.epoch_millis()).state,
                )
            },
        ),
        Vendor::OpenaiCodex => profile
            .config_dir
            .as_deref()
            .map_or(ProfileCredentialHealth::MissingConfiguration, |directory| {
                ProfileCredentialHealth::Codex(inspect_codex_credentials(directory).state)
            }),
        Vendor::DeepseekBalance | Vendor::XaiGrok => external_reference_health(profile),
        Vendor::Gemini => {
            if profile.config_dir.is_some() {
                ProfileCredentialHealth::ExternalReferenceConfigured
            } else {
                ProfileCredentialHealth::MissingConfiguration
            }
        }
        Vendor::Antigravity => ProfileCredentialHealth::NotApplicable,
    }
}

fn external_reference_health(profile: &Profile) -> ProfileCredentialHealth {
    let reference = profile
        .api_key_env
        .as_ref()
        .map(|name| SecretRef::Environment { name: name.clone() })
        .or_else(|| {
            profile
                .api_key_file
                .as_ref()
                .map(|path| SecretRef::File { path: path.clone() })
        });
    match reference {
        Some(reference) if reference.resolve().is_ok() => {
            ProfileCredentialHealth::ExternalReferenceConfigured
        }
        Some(_) => ProfileCredentialHealth::ExternalReferenceUnavailable,
        None => ProfileCredentialHealth::MissingConfiguration,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::pulse::{
        AccountId, Percent, ProfileName, ProfileOrigin, QuotaWindow, QuotaWindowKind, RefreshPolicy,
    };

    fn instant(value: i64) -> Instant {
        Instant::from_epoch_millis(value).expect("instant")
    }

    fn profile(vendor: Vendor) -> Profile {
        Profile {
            account_id: AccountId::new(1).expect("account"),
            name: ProfileName::new("default").expect("profile"),
            vendor,
            config_dir: Some(PathBuf::from("/tmp/atmux-health")),
            poll_interval_minutes: 15,
            monthly_budget_usd: None,
            api_key_env: None,
            api_key_file: None,
            refresh: RefreshPolicy::InMemory,
            hidden: false,
            origin: ProfileOrigin::Local,
        }
    }

    fn snapshot(
        id: i64,
        outcome: CollectionOutcome,
        polled_at: i64,
        used: f64,
    ) -> StoredUsageSnapshot {
        StoredUsageSnapshot {
            id,
            snapshot: UsageSnapshot {
                account_id: AccountId::new(1).expect("account"),
                profile: ProfileName::new("default").expect("profile"),
                machine: MachineName::new("max").expect("machine"),
                vendor: Vendor::AnthropicOauth,
                windows: if outcome.is_success() {
                    vec![QuotaWindow {
                        kind: QuotaWindowKind::FiveHour,
                        used_percent: Percent::new(used).expect("percent"),
                        resets_at: instant(9_999_999),
                    }]
                } else {
                    Vec::new()
                },
                outcome,
                polled_at: instant(polled_at),
                reporter_version: None,
            },
        }
    }

    #[test]
    fn classifications_do_not_conflate_failure_modes() {
        let profile = profile(Vendor::AnthropicOauth);
        let now = instant(4_000_000);
        assert_eq!(
            classify_gauge(&profile, &[], now),
            GaugeHealth::DeadNoObservation
        );
        assert_eq!(
            classify_gauge(
                &profile,
                &[snapshot(
                    1,
                    CollectionOutcome::AuthenticationFailed {
                        code: "auth_failed".to_owned()
                    },
                    3_900_000,
                    0.0
                )],
                now
            ),
            GaugeHealth::AuthenticationFailed
        );
        assert_eq!(
            classify_gauge(
                &profile,
                &[snapshot(
                    1,
                    CollectionOutcome::Unavailable {
                        code: "offline".to_owned()
                    },
                    3_900_000,
                    0.0
                )],
                now
            ),
            GaugeHealth::NullSignal
        );
        assert_eq!(
            classify_gauge(
                &profile,
                &[snapshot(1, CollectionOutcome::Success, 1_000_000, 25.0)],
                now
            ),
            GaugeHealth::Stale
        );
    }

    #[test]
    fn recent_success_distinguishes_changed_and_long_unchanged() {
        let profile = profile(Vendor::AnthropicOauth);
        let now = instant(4_000_000);
        let unchanged = vec![
            snapshot(2, CollectionOutcome::Success, 3_900_000, 25.0),
            snapshot(1, CollectionOutcome::Success, 2_000_000, 25.0),
        ];
        assert_eq!(
            classify_gauge(&profile, &unchanged, now),
            GaugeHealth::AuthenticatedUnchanged
        );
        let changed = vec![
            snapshot(2, CollectionOutcome::Success, 3_900_000, 30.0),
            snapshot(1, CollectionOutcome::Success, 2_000_000, 25.0),
        ];
        assert_eq!(
            classify_gauge(&profile, &changed, now),
            GaugeHealth::Healthy
        );
    }

    #[test]
    fn non_quota_vendor_is_not_a_dead_gauge() {
        assert_eq!(
            classify_gauge(&profile(Vendor::Antigravity), &[], instant(4_000_000)),
            GaugeHealth::NotApplicable
        );
    }
}
