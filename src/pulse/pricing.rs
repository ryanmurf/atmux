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

pub const PRICING_AS_OF: &str = "2026-09-07";
pub const ANTHROPIC_PRICING_SOURCE: &str =
    "https://platform.claude.com/docs/en/about-claude/pricing";
pub const OPENAI_PRICING_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";
pub const DEEPSEEK_PRICING_SOURCE: &str = "https://api-docs.deepseek.com/quick_start/pricing/";
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
pub fn authoritative_pricing() -> Vec<AuthoritativePricingRule> {
    let mut rules = Vec::new();
    anthropic_pricing(&mut rules);
    openai_pricing(&mut rules);
    deepseek_pricing(&mut rules);
    gemini_pricing(&mut rules);
    rules
}

fn anthropic_pricing(rules: &mut Vec<AuthoritativePricingRule>) {
    for specification in [
        spec("claude-fable-5-1", 10.0, 50.0, 12.5, 20.0, 0.25),
        spec("claude-mythos-5-1", 10.0, 50.0, 12.5, 20.0, 0.25),
        spec("claude-fable-5", 10.0, 50.0, 12.5, 20.0, 1.0),
        spec("claude-mythos-5", 10.0, 50.0, 12.5, 20.0, 1.0),
        spec("claude-opus-5", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-opus-4-8", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-opus-4-7", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-opus-4-6", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-opus-4-5", 5.0, 25.0, 6.25, 10.0, 0.5),
        spec("claude-opus-4", 15.0, 75.0, 18.75, 30.0, 1.5),
        spec("claude-sonnet-5", 2.0, 10.0, 2.5, 4.0, 0.2),
        spec("claude-sonnet-4", 3.0, 15.0, 3.75, 6.0, 0.3),
        spec("claude-haiku-4", 1.0, 5.0, 1.25, 2.0, 0.1),
        spec("claude-3-5-haiku", 0.8, 4.0, 1.0, 1.6, 0.08),
    ] {
        for (settings, multiplier) in [(&[][..], 1.0), (&[("service_tier", "batch")][..], 0.5)] {
            rules.push(priced_rule(
                Vendor::AnthropicOauth,
                specification.scaled(multiplier, multiplier),
                settings,
                ANTHROPIC_PRICING_SOURCE,
            ));
        }
        // Claude reports actual delivered speed separately from service_tier.
        if matches!(specification.model, "claude-opus-5" | "claude-opus-4-8") {
            rules.push(priced_rule(
                Vendor::AnthropicOauth,
                specification.scaled(2.0, 2.0),
                &[("speed", "fast")],
                ANTHROPIC_PRICING_SOURCE,
            ));
        }
    }
}

fn openai_pricing(rules: &mut Vec<AuthoritativePricingRule>) {
    // The provider publishes one cache-write rate, with no TTL distinction.
    // Apply it to either write bucket; don't invent a one-hour surcharge.
    for specification in [
        spec("gpt-6-astra", 10.0, 50.0, 12.5, 12.5, 1.0),
        spec("gpt-5.6-sol", 4.0, 20.0, 5.0, 5.0, 0.4),
        spec("gpt-5.6-terra", 2.0, 12.0, 2.5, 2.5, 0.2),
        spec("gpt-5.6-luna", 0.2, 1.2, 0.25, 0.25, 0.02),
    ] {
        for (tier, multiplier) in [
            (None, 1.0),
            (Some("batch"), 0.5),
            (Some("flex"), 0.5),
            (Some("fast"), 2.0),
            (Some("priority"), 2.0),
        ] {
            let settings = tier
                .map(|tier| ("service_tier", tier))
                .into_iter()
                .collect::<Vec<_>>();
            openai_context_rules(
                rules,
                specification.scaled(multiplier, multiplier),
                &settings,
            );
        }
    }
    for specification in [
        spec("gpt-5.5", 5.0, 30.0, 0.0, 0.0, 0.5),
        spec("gpt-5.4", 2.5, 15.0, 0.0, 0.0, 0.25),
    ] {
        openai_context_rules(rules, specification, &[]);
    }
    for specification in [
        // No cached-input discount is not the same as free cached input.
        spec("gpt-5.5-pro", 30.0, 180.0, 0.0, 0.0, 30.0),
        spec("gpt-5.4-nano", 0.2, 1.25, 0.0, 0.0, 0.02),
        spec("gpt-5.4-mini", 0.75, 4.5, 0.0, 0.0, 0.075),
        spec("gpt-5.3-codex", 1.75, 14.0, 0.0, 0.0, 0.175),
    ] {
        rules.push(priced_rule(
            Vendor::OpenaiCodex,
            specification,
            &[],
            OPENAI_PRICING_SOURCE,
        ));
    }
    openai_context_rules(
        rules,
        spec("gpt-5.5", 2.5, 15.0, 0.0, 0.0, 0.25),
        &[("service_tier", "batch")],
    );
}

fn openai_context_rules(
    rules: &mut Vec<AuthoritativePricingRule>,
    specification: RateSpec,
    settings: &[(&str, &str)],
) {
    rules.push(priced_rule(
        Vendor::OpenaiCodex,
        specification,
        settings,
        OPENAI_PRICING_SOURCE,
    ));
    let mut long_settings = settings.to_vec();
    long_settings.push(("context_tier", "long"));
    rules.push(priced_rule(
        Vendor::OpenaiCodex,
        specification.scaled(2.0, 1.5),
        &long_settings,
        OPENAI_PRICING_SOURCE,
    ));
}

fn deepseek_pricing(rules: &mut Vec<AuthoritativePricingRule>) {
    // Daily grains cannot reconstruct the request's UTC billing hour. Use peak
    // unless the ingesting collector explicitly supplies billing_period.
    for specification in [
        spec("deepseek-v4-pro", 1.32, 3.96, 0.0, 0.0, 0.044),
        spec("deepseek-v4-flash", 0.44, 1.32, 0.0, 0.0, 0.014),
        spec("deepseek-v4-flash-vision-exp", 0.44, 1.32, 0.0, 0.0, 0.014),
    ] {
        rules.push(priced_rule(
            Vendor::DeepseekBalance,
            specification,
            &[],
            DEEPSEEK_PRICING_SOURCE,
        ));
        rules.push(priced_rule(
            Vendor::DeepseekBalance,
            specification.scaled(0.5, 0.5),
            &[("billing_period", "off_peak")],
            DEEPSEEK_PRICING_SOURCE,
        ));
    }
}

fn gemini_pricing(rules: &mut Vec<AuthoritativePricingRule>) {
    // Promotional rates for 3.6/3.7/3.8 Flash run through 2026-12-31.
    // Cache storage is time-based and is NOT a token cache-write charge.
    for model in ["gemini-3.8-flash", "gemini-3.7-flash", "gemini-3.6-flash"] {
        let specification = spec(model, 0.75, 3.75, 0.0, 0.0, 0.075);
        for (tier, multiplier) in [
            (None, 1.0),
            (Some("batch"), 0.5),
            (Some("flex"), 0.5),
            (Some("priority"), 1.8),
        ] {
            let settings = tier
                .map(|tier| ("service_tier", tier))
                .into_iter()
                .collect::<Vec<_>>();
            rules.push(priced_rule(
                Vendor::Gemini,
                specification.scaled(multiplier, multiplier),
                &settings,
                GEMINI_PRICING_SOURCE,
            ));
        }
    }
    for specification in [
        spec("gemini-3.1-pro", 2.0, 12.0, 0.0, 0.0, 0.2),
        spec("gemini-3.5-flash", 1.5, 9.0, 0.0, 0.0, 0.15),
        spec("gemini-3.5-flash-lite", 0.3, 2.5, 0.0, 0.0, 0.03),
        spec("gemini-3.1-flash-lite", 0.25, 1.5, 0.0, 0.0, 0.025),
        spec("gemini-2.5-pro", 1.25, 10.0, 0.0, 0.0, 0.125),
        spec("gemini-2.5-flash", 0.3, 2.5, 0.0, 0.0, 0.03),
        spec("gemini-3-flash", 0.5, 3.0, 0.0, 0.0, 0.05),
    ] {
        rules.push(priced_rule(
            Vendor::Gemini,
            specification,
            &[],
            GEMINI_PRICING_SOURCE,
        ));
    }
}

#[derive(Clone, Copy)]
struct RateSpec {
    model: &'static str,
    rate: PricingRate,
}

impl RateSpec {
    fn scaled(self, input: f64, output: f64) -> Self {
        Self {
            model: self.model,
            rate: PricingRate {
                input: self.rate.input * input,
                output: self.rate.output * output,
                cache_write_5m: self.rate.cache_write_5m * input,
                cache_write_1h: self.rate.cache_write_1h * input,
                cache_read: self.rate.cache_read * input,
            },
        }
    }
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

/// Completes a possibly partially seeded store table with current built-ins.
/// Built-ins replace stale seeded copies; custom default keys and account
/// overrides remain intact. Retired, unsupported estimates are not exposed.
#[must_use]
pub fn effective_default_pricing(stored: &[PricingRule]) -> Vec<PricingRule> {
    let mut rules = stored
        .iter()
        .filter(|rule| !unpriced_builtin_model(&rule.model_pattern))
        .map(|rule| (rule.key.clone(), rule.clone()))
        .collect::<BTreeMap<_, _>>();
    for item in authoritative_pricing() {
        rules.insert(item.rule.key.clone(), item.rule);
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
    // Spark has no published API price. Never inherit the ordinary Codex
    // prefix's rate (including the unsupported row persisted by old builds).
    if !unpriced_builtin_model(model) {
        let model = if model.eq_ignore_ascii_case("gpt-5.6") {
            "gpt-5.6-sol"
        } else {
            model
        };
        if let Some(rule) = select_rule(defaults, vendor, model, &target) {
            return resolved(rule, PricingOrigin::AuthoritativeDefault);
        }
    }
    ResolvedPricing {
        rate: PricingRate::fallback(),
        known: false,
        origin: PricingOrigin::Fallback,
        rule_key: None,
    }
}

fn unpriced_builtin_model(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("gpt-5.3-codex-spark")
        || model == "deepseek"
        || model == "antigravity-unknown"
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
    fn september_catalog_rates_and_aliases_match_published_prices() {
        let defaults = effective_default_pricing(&[]);
        for specification in [
            spec("gpt-6-astra", 10.0, 50.0, 12.5, 12.5, 1.0),
            spec("gpt-5.6-sol", 4.0, 20.0, 5.0, 5.0, 0.4),
            spec("gpt-5.6", 4.0, 20.0, 5.0, 5.0, 0.4),
            spec("gpt-5.6-terra", 2.0, 12.0, 2.5, 2.5, 0.2),
            spec("gpt-5.6-luna", 0.2, 1.2, 0.25, 0.25, 0.02),
            spec("claude-fable-5-1-20260827", 10.0, 50.0, 12.5, 20.0, 0.25),
            spec("claude-mythos-5-1", 10.0, 50.0, 12.5, 20.0, 0.25),
            spec("claude-fable-5", 10.0, 50.0, 12.5, 20.0, 1.0),
            spec("claude-opus-5", 5.0, 25.0, 6.25, 10.0, 0.5),
            spec("claude-opus-4-1-20250805", 15.0, 75.0, 18.75, 30.0, 1.5),
            spec("claude-opus-4-5", 5.0, 25.0, 6.25, 10.0, 0.5),
            spec("claude-sonnet-5", 2.0, 10.0, 2.5, 4.0, 0.2),
            spec("gemini-3.8-flash", 0.75, 3.75, 0.0, 0.0, 0.075),
            spec("gemini-3.7-flash", 0.75, 3.75, 0.0, 0.0, 0.075),
            spec("gemini-3.6-flash", 0.75, 3.75, 0.0, 0.0, 0.075),
            spec("gemini-3.5-flash-lite", 0.3, 2.5, 0.0, 0.0, 0.03),
            spec("gemini-3.1-flash-lite", 0.25, 1.5, 0.0, 0.0, 0.025),
            spec("gemini-3-flash-preview", 0.5, 3.0, 0.0, 0.0, 0.05),
            spec("deepseek-v4-pro", 1.32, 3.96, 0.0, 0.0, 0.044),
            spec("deepseek-v4-flash", 0.44, 1.32, 0.0, 0.0, 0.014),
            spec("deepseek-v4-flash-vision-exp", 0.44, 1.32, 0.0, 0.0, 0.014),
        ] {
            let actual = resolve_pricing(
                specification.model,
                &AgentSettings::default(),
                &defaults,
                &[],
            );
            assert!(actual.known, "{}", specification.model);
            assert_eq!(actual.rate, specification.rate, "{}", specification.model);
        }
        // The exact gpt-5.6 alias must not price every future 5.6 variant as Sol.
        assert!(
            !resolve_pricing(
                "gpt-5.6-unlisted",
                &AgentSettings::default(),
                &defaults,
                &[]
            )
            .known
        );
    }

    #[test]
    fn fast_priority_and_long_context_prices_are_independent_of_effort() {
        let defaults = effective_default_pricing(&[]);
        for model in [
            "gpt-6-astra",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ] {
            let base = resolve_pricing(model, &AgentSettings::default(), &defaults, &[]).rate;
            for (tier, multiplier) in [
                ("fast", 2.0),
                ("priority", 2.0),
                ("batch", 0.5),
                ("flex", 0.5),
            ] {
                let mut settings = AgentSettings {
                    service_tier: Some(tier.to_owned()),
                    effort: Some("ultra".to_owned()),
                    ..AgentSettings::default()
                };
                let short = resolve_pricing(model, &settings, &defaults, &[]).rate;
                assert_close(short.input, base.input * multiplier);
                assert_close(short.output, base.output * multiplier);
                assert_close(short.cache_read, base.cache_read * multiplier);
                assert_close(short.cache_write_5m, base.cache_write_5m * multiplier);
                settings
                    .additional
                    .insert("context_tier".to_owned(), "long".to_owned());
                let long = resolve_pricing(model, &settings, &defaults, &[]).rate;
                assert_close(long.input, short.input * 2.0);
                assert_close(long.output, short.output * 1.5);
                assert_close(long.cache_read, short.cache_read * 2.0);
                assert_close(long.cache_write_1h, short.cache_write_1h * 2.0);
            }
        }
    }

    #[test]
    fn provider_specific_speed_and_billing_period_are_honored() {
        let defaults = effective_default_pricing(&[]);
        let mut settings = AgentSettings::default();
        settings
            .additional
            .insert("speed".to_owned(), "fast".to_owned());
        for model in ["claude-opus-5", "claude-opus-4-8"] {
            let fast = resolve_pricing(model, &settings, &defaults, &[]);
            assert_close(fast.rate.input, 10.0);
            assert_close(fast.rate.cache_read, 1.0);
        }
        // Opus 4.6 falls back to standard speed at the provider.
        assert_close(
            resolve_pricing("claude-opus-4-6", &settings, &defaults, &[])
                .rate
                .input,
            5.0,
        );
        settings
            .additional
            .insert("billing_period".to_owned(), "off_peak".to_owned());
        assert_close(
            resolve_pricing("deepseek-v4-pro", &settings, &defaults, &[])
                .rate
                .input,
            0.66,
        );
        settings.service_tier = Some("priority".to_owned());
        assert_close(
            resolve_pricing("gemini-3.8-flash", &settings, &defaults, &[])
                .rate
                .input,
            1.35,
        );
    }

    #[test]
    fn stale_defaults_are_refreshed_without_losing_custom_rules_or_overrides() {
        let mut stale = authoritative_pricing()
            .into_iter()
            .find(|item| item.rule.key == "deepseek-v4-pro")
            .unwrap()
            .rule;
        stale.input_per_million_usd = 0.435;
        let mut custom = stale.clone();
        custom.key = "custom-model".to_owned();
        custom.model_pattern = "custom-model".to_owned();
        let defaults = effective_default_pricing(&[stale.clone(), custom.clone()]);
        assert!(defaults.contains(&custom));
        let settings = AgentSettings::default();
        assert_close(
            resolve_pricing("deepseek-v4-pro", &settings, &defaults, &[])
                .rate
                .input,
            1.32,
        );
        let overridden = resolve_pricing("deepseek-v4-pro", &settings, &defaults, &[stale]);
        assert_close(overridden.rate.input, 0.435);
        assert_eq!(overridden.origin, PricingOrigin::AccountOverride);
    }

    #[test]
    fn spark_and_unidentified_models_are_not_authoritatively_priced() {
        let mut obsolete = authoritative_pricing()
            .into_iter()
            .find(|item| item.rule.key == "gpt-5.3-codex")
            .unwrap()
            .rule;
        let settings = AgentSettings::default();
        for model in [
            "gpt-5.3-codex-spark",
            "gpt-5.3-codex-spark-preview",
            "deepseek",
            "antigravity-unknown",
        ] {
            obsolete.key = model.to_owned();
            obsolete.model_pattern = model.to_owned();
            let defaults = effective_default_pricing(&[obsolete.clone()]);
            assert!(!defaults.iter().any(|rule| rule.model_pattern == model));
            assert!(!resolve_pricing(model, &settings, &defaults, &[]).known);
            assert!(!resolve_pricing(model, &settings, &[obsolete.clone()], &[]).known);
            assert_eq!(
                resolve_pricing(model, &settings, &defaults, &[obsolete.clone()]).origin,
                PricingOrigin::AccountOverride
            );
        }
    }

    #[test]
    fn authoritative_table_is_valid_and_source_attributed() {
        let rules = authoritative_pricing();
        assert!(rules.len() >= 20);
        let keys = rules
            .iter()
            .map(|item| &item.rule.key)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys.len(), rules.len(), "stable rule keys must be unique");
        for item in rules {
            item.rule.validate().expect("valid rule");
            assert!(item.source_url.starts_with("https://"));
            assert_eq!(item.as_of, PRICING_AS_OF);
        }
    }
}
