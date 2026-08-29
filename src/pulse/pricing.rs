//! Authoritative, settings-aware list-price equivalents for Pulse reports.
//!
//! Rates are USD per one million tokens. They represent API list-price
//! equivalents for subscription traffic, not the operator's subscription bill.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    PulseError, PulseResult, Vendor,
    model::{AgentSettings, TokenGrain},
    store::{PricingRule, Store},
};

pub const PRICING_AS_OF: &str = "2026-07-10";
pub const ANTHROPIC_PRICING_SOURCE: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing";
pub const OPENAI_PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";
pub const DEEPSEEK_PRICING_SOURCE: &str = "https://api-docs.deepseek.com/quick_start/pricing";
pub const GEMINI_PRICING_SOURCE: &str = "https://ai.google.dev/gemini-api/docs/pricing";

/// The five independently billed token classes, in USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PricingRate {
    pub input: f64,
    pub output: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl PricingRate {
    #[must_use]
    pub const fn fallback() -> Self {
        Self {
            input: 3.0,
            output: 15.0,
            cache_write_5m: 3.75,
            cache_write_1h: 6.0,
            cache_read: 0.3,
        }
    }

    fn from_rule(rule: &PricingRule) -> Self {
        Self {
            input: rule.input_per_million_usd,
            output: rule.output_per_million_usd,
            cache_write_5m: rule.cache_write_5m_per_million_usd,
            cache_write_1h: rule.cache_write_1h_per_million_usd,
            cache_read: rule.cache_read_per_million_usd,
        }
    }
}

/// Provenance retained beside each built-in pricing rule.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthoritativePricingRule {
    pub rule: PricingRule,
    pub source_url: &'static str,
    pub as_of: &'static str,
}

/// Where an effective rate came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingOrigin {
    AccountOverride,
    AuthoritativeDefault,
    Fallback,
}

/// Effective price selected for one model/settings combination.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedPricing {
    pub rate: PricingRate,
    pub known: bool,
    pub origin: PricingOrigin,
    pub rule_key: Option<String>,
}

/// Returns the single built-in pricing table and its primary-source provenance.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn authoritative_pricing() -> Vec<AuthoritativePricingRule> {
    let anthropic = [
        spec("claude-fable-5", 10.0, 50.0, 12.5, 20.0, 1.0),
        spec("claude-mythos-5", 10.0, 50.0, 12.5, 20.0, 1.0),
        spec("claude-opus-4", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-sonnet-4", 3.0, 15.0, 3.75, 6.0, 0.3),
        spec("claude-haiku-4", 1.0, 5.0, 1.25, 2.0, 0.1),
    ];
    let mut rules = anthropic
        .into_iter()
        .map(|specification| {
            priced_rule(
                Vendor::AnthropicOauth,
                specification,
                &[],
                ANTHROPIC_PRICING_SOURCE,
            )
        })
        .collect::<Vec<_>>();
    rules.push(priced_rule(
        Vendor::AnthropicOauth,
        spec("claude-fable-5", 5.0, 25.0, 6.25, 10.0, 0.5),
        &[("service_tier", "batch")],
        ANTHROPIC_PRICING_SOURCE,
    ));
    rules.push(priced_rule(
        Vendor::AnthropicOauth,
        spec("claude-opus-4", 2.5, 12.5, 3.125, 5.0, 0.25),
        &[("service_tier", "batch")],
        ANTHROPIC_PRICING_SOURCE,
    ));

    for specification in [
        spec("gpt-5.5-pro", 30.0, 180.0, 0.0, 0.0, 0.0),
        spec("gpt-5.5", 5.0, 30.0, 0.0, 0.0, 0.5),
        spec("gpt-5.4-nano", 0.2, 1.25, 0.0, 0.0, 0.02),
        spec("gpt-5.4-mini", 0.75, 4.5, 0.0, 0.0, 0.075),
        spec("gpt-5.4", 2.5, 15.0, 0.0, 0.0, 0.25),
        spec("gpt-5.3-codex", 1.75, 14.0, 0.0, 0.0, 0.175),
        spec("gpt-5.3-codex-spark", 1.75, 14.0, 0.0, 0.0, 0.175),
    ] {
        rules.push(priced_rule(
            Vendor::OpenaiCodex,
            specification,
            &[],
            OPENAI_PRICING_SOURCE,
        ));
    }
    rules.push(priced_rule(
        Vendor::OpenaiCodex,
        spec("gpt-5.5", 2.5, 15.0, 0.0, 0.0, 0.25),
        &[("service_tier", "batch")],
        OPENAI_PRICING_SOURCE,
    ));

    for specification in [
        spec("deepseek-v4-pro", 0.435, 0.87, 0.0, 0.0, 0.003_625),
        spec("deepseek-v4-flash", 0.14, 0.28, 0.0, 0.0, 0.0028),
        spec("deepseek", 0.14, 0.28, 0.0, 0.0, 0.0028),
    ] {
        rules.push(priced_rule(
            Vendor::DeepseekBalance,
            specification,
            &[],
            DEEPSEEK_PRICING_SOURCE,
        ));
    }

    for specification in [
        spec("gemini-3.1-pro", 2.0, 12.0, 0.0, 0.0, 0.2),
        spec("gemini-3.5-flash", 1.5, 9.0, 0.0, 0.0, 0.15),
        spec("gemini-2.5-pro", 1.25, 10.0, 0.0, 0.0, 0.125),
        spec("gemini-2.5-flash", 0.3, 2.5, 0.0, 0.0, 0.03),
        spec("gemini-3-flash", 1.5, 9.0, 0.0, 0.0, 0.15),
    ] {
        rules.push(priced_rule(
            Vendor::Gemini,
            specification,
            &[],
            GEMINI_PRICING_SOURCE,
        ));
    }
    rules.push(priced_rule(
        Vendor::Antigravity,
        spec("antigravity-unknown", 2.0, 12.0, 0.0, 0.0, 0.2),
        &[],
        GEMINI_PRICING_SOURCE,
    ));
    rules
}

#[derive(Clone, Copy)]
struct RateSpec {
    model: &'static str,
    rate: PricingRate,
}

const fn spec(
    model: &'static str,
    input: f64,
    output: f64,
    short_cache_write: f64,
    hourly_cache_write: f64,
    cache_read: f64,
) -> RateSpec {
    RateSpec {
        model,
        rate: PricingRate {
            input,
            output,
            cache_write_5m: short_cache_write,
            cache_write_1h: hourly_cache_write,
            cache_read,
        },
    }
}

fn priced_rule(
    vendor: Vendor,
    specification: RateSpec,
    settings: &[(&str, &str)],
    source_url: &'static str,
) -> AuthoritativePricingRule {
    let settings_match = settings
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    let suffix = settings
        .iter()
        .fold(String::new(), |mut suffix, (key, value)| {
            suffix.push('-');
            suffix.push_str(key);
            suffix.push('-');
            suffix.push_str(value);
            suffix
        });
    AuthoritativePricingRule {
        rule: PricingRule {
            key: format!("{}{}", specification.model, suffix),
            vendor,
            model_pattern: specification.model.to_owned(),
            settings_match,
            input_per_million_usd: specification.rate.input,
            output_per_million_usd: specification.rate.output,
            cache_write_5m_per_million_usd: specification.rate.cache_write_5m,
            cache_write_1h_per_million_usd: specification.rate.cache_write_1h,
            cache_read_per_million_usd: specification.rate.cache_read,
        },
        source_url,
        as_of: PRICING_AS_OF,
    }
}

/// Idempotently refreshes the built-in default rows in a store.
///
/// # Errors
///
/// Returns the first validation or persistence error.
pub async fn seed_authoritative_pricing(store: &dyn Store) -> PulseResult<usize> {
    let rules = authoritative_pricing();
    for item in &rules {
        item.rule.validate()?;
        store.upsert_pricing_default(item.rule.clone()).await?;
    }
    Ok(rules.len())
}

/// Completes a possibly partially seeded store table with built-in rows, while
/// letting a validated stored row replace the same stable key.
#[must_use]
pub fn effective_default_pricing(stored: &[PricingRule]) -> Vec<PricingRule> {
    let mut rules = authoritative_pricing()
        .into_iter()
        .map(|item| (item.rule.key.clone(), item.rule))
        .collect::<BTreeMap<_, _>>();
    for rule in stored {
        rules.insert(rule.key.clone(), rule.clone());
    }
    rules.into_values().collect()
}

/// Resolves overrides before defaults, using longest model prefix and the most
/// specific settings-subset match within that model.
#[must_use]
pub fn resolve_pricing(
    model: &str,
    settings: &AgentSettings,
    defaults: &[PricingRule],
    overrides: &[PricingRule],
) -> ResolvedPricing {
    resolve_pricing_inner(None, model, settings, defaults, overrides)
}

/// Vendor-aware resolution for stored profile reports. Antigravity may resolve
/// to an underlying provider model, so its model ids intentionally cross the
/// vendor boundary while ordinary profiles remain isolated.
#[must_use]
pub fn resolve_vendor_pricing(
    vendor: Vendor,
    model: &str,
    settings: &AgentSettings,
    defaults: &[PricingRule],
    overrides: &[PricingRule],
) -> ResolvedPricing {
    resolve_pricing_inner(Some(vendor), model, settings, defaults, overrides)
}

fn resolve_pricing_inner(
    vendor: Option<Vendor>,
    model: &str,
    settings: &AgentSettings,
    defaults: &[PricingRule],
    overrides: &[PricingRule],
) -> ResolvedPricing {
    let target = settings_map(settings);
    if let Some(rule) = select_rule(overrides, vendor, model, &target) {
        return resolved(rule, PricingOrigin::AccountOverride);
    }
    if let Some(rule) = select_rule(defaults, vendor, model, &target) {
        return resolved(rule, PricingOrigin::AuthoritativeDefault);
    }
    ResolvedPricing {
        rate: PricingRate::fallback(),
        known: false,
        origin: PricingOrigin::Fallback,
        rule_key: None,
    }
}

fn resolved(rule: &PricingRule, origin: PricingOrigin) -> ResolvedPricing {
    ResolvedPricing {
        rate: PricingRate::from_rule(rule),
        known: true,
        origin,
        rule_key: Some(rule.key.clone()),
    }
}

fn settings_map(settings: &AgentSettings) -> BTreeMap<String, String> {
    let mut values = settings.additional.clone();
    if let Some(service_tier) = &settings.service_tier {
        values.insert("service_tier".to_owned(), service_tier.clone());
    }
    if let Some(effort) = &settings.effort {
        values.insert("effort".to_owned(), effort.clone());
    }
    values
}

fn select_rule<'a>(
    rules: &'a [PricingRule],
    vendor: Option<Vendor>,
    model: &str,
    settings: &BTreeMap<String, String>,
) -> Option<&'a PricingRule> {
    let model = model.to_ascii_lowercase();
    let exact = rules
        .iter()
        .filter(|rule| vendor.is_none_or(|vendor| vendor_matches(vendor, rule.vendor)))
        .filter(|rule| rule.model_pattern.eq_ignore_ascii_case(&model))
        .collect::<Vec<_>>();
    let candidates = if exact.is_empty() {
        let longest = rules
            .iter()
            .filter(|rule| vendor.is_none_or(|vendor| vendor_matches(vendor, rule.vendor)))
            .filter(|rule| model.starts_with(&rule.model_pattern.to_ascii_lowercase()))
            .map(|rule| rule.model_pattern.len())
            .max()?;
        rules
            .iter()
            .filter(|rule| {
                vendor.is_none_or(|vendor| vendor_matches(vendor, rule.vendor))
                    && rule.model_pattern.len() == longest
                    && model.starts_with(&rule.model_pattern.to_ascii_lowercase())
            })
            .collect::<Vec<_>>()
    } else {
        exact
    };
    candidates
        .into_iter()
        .filter(|rule| {
            rule.settings_match
                .iter()
                .all(|(key, value)| settings.get(key) == Some(value))
        })
        .max_by(|left, right| {
            left.settings_match
                .len()
                .cmp(&right.settings_match.len())
                .then_with(|| right.key.cmp(&left.key))
        })
}

fn vendor_matches(usage_vendor: Vendor, pricing_vendor: Vendor) -> bool {
    usage_vendor == Vendor::Antigravity || usage_vendor == pricing_vendor
}

/// Computes a five-class token cost and rounds it to six decimal places.
///
/// # Errors
///
/// Returns invalid-input if a caller supplies non-finite/negative rates or the
/// result exceeds finite `f64` range.
pub fn cost_for_grain(grain: &TokenGrain, rate: PricingRate) -> PulseResult<f64> {
    let rates = [
        rate.input,
        rate.output,
        rate.cache_write_5m,
        rate.cache_write_1h,
        rate.cache_read,
    ];
    if rates
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
    {
        return Err(PulseError::invalid_input(
            "pricing rates must be finite and nonnegative",
        ));
    }
    let cost = scaled(grain.tokens_in, rate.input)
        + scaled(grain.tokens_out, rate.output)
        + scaled(grain.cache_write_5m, rate.cache_write_5m)
        + scaled(grain.cache_write_1h, rate.cache_write_1h)
        + scaled(grain.cache_read, rate.cache_read);
    if !cost.is_finite() {
        return Err(PulseError::invalid_input("computed token cost overflowed"));
    }
    Ok((cost * 1_000_000.0).round() / 1_000_000.0)
}

#[allow(clippy::cast_precision_loss)]
fn scaled(tokens: u64, per_million: f64) -> f64 {
    (tokens as f64 / 1_000_000.0) * per_million
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pulse::{AccountId, MachineName, ProfileName, SessionId, TokenSource};

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1.0e-9, "{actual} != {expected}");
    }

    fn grain(settings: AgentSettings) -> TokenGrain {
        let settings_hash = settings.sha256().expect("hash settings");
        TokenGrain {
            account_id: AccountId::new(1).expect("account"),
            profile: ProfileName::new("claude").expect("profile"),
            machine: MachineName::new("midnight").expect("machine"),
            session_id: SessionId::new("session").expect("session"),
            model: "claude-opus-4-8".to_owned(),
            settings,
            settings_hash,
            day: "2026-08-08".to_owned(),
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
            cache_read: 1_000_000,
            source: TokenSource::Local,
        }
    }

    #[test]
    fn longest_prefix_and_settings_specific_rules_win() {
        let defaults = authoritative_pricing()
            .into_iter()
            .map(|item| item.rule)
            .collect::<Vec<_>>();
        let base = resolve_pricing(
            "gpt-5.4-mini-preview",
            &AgentSettings::default(),
            &defaults,
            &[],
        );
        assert_close(base.rate.input, 0.75);
        let batch = AgentSettings {
            service_tier: Some("batch".to_owned()),
            ..AgentSettings::default()
        };
        let priced = resolve_pricing("claude-opus-4-8", &batch, &defaults, &[]);
        assert_close(priced.rate.input, 2.5);
        assert_eq!(priced.origin, PricingOrigin::AuthoritativeDefault);
    }

    #[test]
    fn account_override_precedes_default_and_unknown_falls_back() {
        let defaults = authoritative_pricing()
            .into_iter()
            .map(|item| item.rule)
            .collect::<Vec<_>>();
        let mut override_rule = defaults
            .iter()
            .find(|rule| rule.key == "claude-opus-4")
            .expect("opus")
            .clone();
        override_rule.input_per_million_usd = 99.0;
        let resolved = resolve_pricing(
            "claude-opus-4-8",
            &AgentSettings::default(),
            &defaults,
            &[override_rule],
        );
        assert_close(resolved.rate.input, 99.0);
        assert_eq!(resolved.origin, PricingOrigin::AccountOverride);
        assert!(!resolve_pricing("unknown-2099", &AgentSettings::default(), &defaults, &[]).known);
    }

    #[test]
    fn vendor_scoping_prevents_cross_provider_rule_collisions() {
        let defaults = authoritative_pricing()
            .into_iter()
            .map(|item| item.rule)
            .collect::<Vec<_>>();
        let mut wrong_vendor = defaults
            .iter()
            .find(|rule| rule.key == "claude-opus-4")
            .expect("opus")
            .clone();
        wrong_vendor.vendor = Vendor::Gemini;
        wrong_vendor.input_per_million_usd = 99.0;
        let normal = resolve_vendor_pricing(
            Vendor::AnthropicOauth,
            "claude-opus-4-8",
            &AgentSettings::default(),
            &defaults,
            &[wrong_vendor.clone()],
        );
        assert_close(normal.rate.input, 5.0);
        let antigravity = resolve_vendor_pricing(
            Vendor::Antigravity,
            "claude-opus-4-8",
            &AgentSettings::default(),
            &defaults,
            &[wrong_vendor],
        );
        assert_close(antigravity.rate.input, 99.0);
    }

    #[test]
    fn all_five_token_classes_are_costed() {
        let cost = cost_for_grain(
            &grain(AgentSettings::default()),
            PricingRate {
                input: 15.0,
                output: 75.0,
                cache_write_5m: 18.75,
                cache_write_1h: 30.0,
                cache_read: 1.5,
            },
        )
        .expect("cost");
        assert_close(cost, 140.25);
    }

    #[test]
    fn authoritative_table_is_valid_and_source_attributed() {
        let rules = authoritative_pricing();
        assert!(rules.len() >= 20);
        for item in rules {
            item.rule.validate().expect("valid rule");
            assert!(item.source_url.starts_with("https://"));
            assert_eq!(item.as_of, PRICING_AS_OF);
        }
    }
}
