//! `DeepSeek` account-balance collection.

use hyper::{Method, StatusCode, header};
use serde::Deserialize;

use super::{HttpsJsonClient, SecretRef};
use crate::pulse::{
    AccountId, CollectionOutcome, Instant, MachineName, Percent, ProfileName, PulseError,
    PulseResult, QuotaWindow, QuotaWindowKind, UsageSnapshot, Vendor,
};

const BALANCE_ENDPOINT: &str = "https://api.deepseek.com/user/balance";
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Pure, secret-free projection of the `DeepSeek` balance response.
#[derive(Clone, Debug, PartialEq)]
pub struct DeepSeekReading {
    pub balance_usd: f64,
    pub available: bool,
    pub window: QuotaWindow,
}

#[derive(Debug, Deserialize)]
struct BalanceResponse {
    is_available: bool,
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Debug, Deserialize)]
struct BalanceInfo {
    currency: String,
    total_balance: String,
}

/// Parses one bounded `DeepSeek` balance document and normalizes its monthly
/// reset to midnight UTC on the first day of the next month.
///
/// # Errors
///
/// Returns an invalid-response error for malformed JSON, missing USD balance,
/// invalid numeric values, or an invalid budget.
pub fn parse_balance_response(
    body: &[u8],
    monthly_budget_usd: f64,
    collected_at: Instant,
) -> PulseResult<DeepSeekReading> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(PulseError::invalid_input(
            "DeepSeek response exceeded its size bound",
        ));
    }
    if !monthly_budget_usd.is_finite() || monthly_budget_usd <= 0.0 {
        return Err(PulseError::invalid_input(
            "DeepSeek monthly budget must be finite and positive",
        ));
    }
    let response: BalanceResponse = serde_json::from_slice(body)
        .map_err(|_| PulseError::invalid_input("DeepSeek response shape was invalid"))?;
    if response.balance_infos.len() > 32 {
        return Err(PulseError::invalid_input(
            "DeepSeek response contained too many balances",
        ));
    }
    let balance_usd = response
        .balance_infos
        .iter()
        .find(|balance| balance.currency == "USD")
        .ok_or_else(|| PulseError::invalid_input("DeepSeek response had no USD balance"))?
        .total_balance
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| PulseError::invalid_input("DeepSeek USD balance was invalid"))?;
    let used_percent =
        ((monthly_budget_usd - balance_usd) / monthly_budget_usd * 100.0).clamp(0.0, 100.0);
    Ok(DeepSeekReading {
        balance_usd,
        available: response.is_available,
        window: QuotaWindow {
            kind: QuotaWindowKind::MonthlyBudget,
            used_percent: Percent::new(used_percent)?,
            resets_at: first_of_next_month_utc(collected_at)?,
        },
    })
}

fn first_of_next_month_utc(now: Instant) -> PulseResult<Instant> {
    let value = now.to_iso8601();
    let year = value
        .get(0..4)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| PulseError::invalid_input("collection time year was invalid"))?;
    let month = value
        .get(5..7)
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|month| (1..=12).contains(month))
        .ok_or_else(|| PulseError::invalid_input("collection time month was invalid"))?;
    let (year, month) = if month == 12 {
        (year.saturating_add(1), 1)
    } else {
        (year, month + 1)
    };
    Instant::from_iso8601(&format!("{year:04}-{month:02}-01T00:00:00Z"))
}

/// HTTPS adapter for `DeepSeek`. The API key can only enter through an external
/// environment/file reference and never enters the returned snapshot.
#[derive(Debug)]
pub struct DeepSeekCollector {
    client: HttpsJsonClient,
}

impl DeepSeekCollector {
    /// Builds the certificate-validating transport.
    ///
    /// # Errors
    ///
    /// Returns a configuration error when system TLS roots are unavailable.
    pub fn new() -> PulseResult<Self> {
        Ok(Self {
            client: HttpsJsonClient::new(MAX_RESPONSE_BYTES)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn collect(
        &self,
        account_id: AccountId,
        profile: ProfileName,
        machine: MachineName,
        key: &SecretRef,
        monthly_budget_usd: f64,
        polled_at: Instant,
    ) -> UsageSnapshot {
        let Ok(secret) = key.resolve() else {
            return snapshot(
                account_id,
                profile,
                machine,
                Vec::new(),
                CollectionOutcome::AuthenticationFailed {
                    code: "deepseek_credential_unavailable".to_owned(),
                },
                polled_at,
            );
        };
        let response = self
            .client
            .request(
                Method::GET,
                BALANCE_ENDPOINT,
                &[(
                    header::AUTHORIZATION.as_str(),
                    format!("Bearer {}", secret.expose()),
                )],
                Vec::new(),
                None,
            )
            .await;
        let (windows, outcome) = match response {
            Ok(response) if response.status == StatusCode::OK => {
                match parse_balance_response(&response.body, monthly_budget_usd, polled_at) {
                    Ok(reading) if reading.available => {
                        (vec![reading.window], CollectionOutcome::Success)
                    }
                    Ok(_) => (
                        Vec::new(),
                        CollectionOutcome::Unavailable {
                            code: "deepseek_balance_unavailable".to_owned(),
                        },
                    ),
                    Err(_) => (
                        Vec::new(),
                        CollectionOutcome::InvalidResponse {
                            code: "deepseek_response_invalid".to_owned(),
                        },
                    ),
                }
            }
            Ok(response)
                if matches!(
                    response.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                (
                    Vec::new(),
                    CollectionOutcome::AuthenticationFailed {
                        code: "deepseek_auth_rejected".to_owned(),
                    },
                )
            }
            Ok(response) if response.status == StatusCode::TOO_MANY_REQUESTS => (
                Vec::new(),
                CollectionOutcome::RateLimited { retry_at: None },
            ),
            Ok(_) => (
                Vec::new(),
                CollectionOutcome::Unavailable {
                    code: "deepseek_upstream_unavailable".to_owned(),
                },
            ),
            Err(_) => (
                Vec::new(),
                CollectionOutcome::Unavailable {
                    code: "deepseek_transport_unavailable".to_owned(),
                },
            ),
        };
        snapshot(account_id, profile, machine, windows, outcome, polled_at)
    }
}

fn snapshot(
    account_id: AccountId,
    profile: ProfileName,
    machine: MachineName,
    windows: Vec<QuotaWindow>,
    outcome: CollectionOutcome,
    polled_at: Instant,
) -> UsageSnapshot {
    UsageSnapshot {
        account_id,
        profile,
        machine,
        vendor: Vendor::DeepseekBalance,
        windows,
        outcome,
        polled_at,
        reporter_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}
