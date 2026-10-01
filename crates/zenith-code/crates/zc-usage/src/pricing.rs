//! Model rates and cost arithmetic (`usagePricing.ts`): LiteLLM's
//! `model_prices_and_context_window.json` (what ccusage prices against) plus the custom
//! prices from settings (`usagePriceOverrides`).

use std::collections::HashMap;

use serde_json::Value;
use zc_contracts::UsageCostSource;

use crate::json::{self, J};
use crate::records::Totals;

/// USD per token, base tier (transcripts do not say which tier served a request).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRate {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_cost_per_token: f64,
    pub cache_creation_cost_per_token: f64,
    /// `provider_specific_entry.fast`; 1 without a published fast tier.
    pub fast_multiplier: f64,
}

pub type RateTable = HashMap<String, ModelRate>;

/// `createOverrideRateTable`: exact trimmed ids; omitted cache rates are the input rate.
pub fn create_override_rate_table(overrides: Option<&Value>) -> RateTable {
    let mut table = RateTable::new();
    let Some(overrides) = overrides.and_then(Value::as_object) else {
        return table;
    };
    for (model, prices) in overrides {
        let price = |key: &str| prices.get(key).and_then(Value::as_f64);
        let (Some(input), Some(output)) = (price("inputCostPerMillionTokens"), price("outputCostPerMillionTokens")) else {
            continue;
        };
        table.insert(
            json::js_trim(model).to_owned(),
            ModelRate {
                input_cost_per_token: input / 1_000_000.0,
                output_cost_per_token: output / 1_000_000.0,
                cache_read_cost_per_token: price("cacheReadCostPerMillionTokens").unwrap_or(input) / 1_000_000.0,
                cache_creation_cost_per_token: price("cacheWriteCostPerMillionTokens").unwrap_or(input) / 1_000_000.0,
                fast_multiplier: 1.0,
            },
        );
    }
    table
}

fn fast_multiplier(entry: &J) -> f64 {
    let Some(specific) = entry.get("provider_specific_entry").filter(|value| value.is_object_like()) else {
        return 1.0;
    };
    match specific.get("fast").and_then(J::as_finite) {
        Some(fast) if fast > 0.0 => fast,
        _ => 1.0,
    }
}

fn normalize_rate_key(model: &str) -> String {
    json::js_trim(model).to_lowercase()
}

fn bare_model_name(key: &str) -> &str {
    key.rfind('/').map_or(key, |slash| &key[slash + 1..])
}

/// `parseRateTable`: entries need both an input and an output rate; a bare name is aliased
/// only when no canonical entry exists and every qualified entry agrees on the rate.
pub fn parse_rate_table(document: &J) -> RateTable {
    let mut table = RateTable::new();
    let Some(entries) = document.as_obj() else {
        return table;
    };
    for (name, raw) in entries.entries() {
        if !raw.is_object_like() {
            continue;
        }
        let (Some(input), Some(output)) = (
            raw.get("input_cost_per_token").and_then(J::as_finite),
            raw.get("output_cost_per_token").and_then(J::as_finite),
        ) else {
            continue;
        };
        let key = normalize_rate_key(name);
        if key.is_empty() {
            continue;
        }
        table.insert(
            key,
            ModelRate {
                input_cost_per_token: input,
                output_cost_per_token: output,
                // Cached input without its own rate is priced as plain input, not free.
                cache_read_cost_per_token: raw.get("cache_read_input_token_cost").and_then(J::as_finite).unwrap_or(input),
                cache_creation_cost_per_token: raw.get("cache_creation_input_token_cost").and_then(J::as_finite).unwrap_or(input),
                fast_multiplier: fast_multiplier(raw),
            },
        );
    }

    // `None` marks a bare name claimed at conflicting rates.
    let mut aliases: HashMap<String, Option<ModelRate>> = HashMap::new();
    for (key, rate) in &table {
        let alias = bare_model_name(key);
        if alias.is_empty() || alias == key || table.contains_key(alias) {
            continue;
        }
        match aliases.get(alias) {
            None => {
                aliases.insert(alias.to_owned(), Some(*rate));
            }
            Some(Some(held)) if held != rate => {
                aliases.insert(alias.to_owned(), None);
            }
            Some(_) => {}
        }
    }
    for (alias, rate) in aliases {
        if let Some(rate) = rate {
            table.insert(alias, rate);
        }
    }
    table
}

/// Drops a bracketed variant suffix (`claude-fable-5-1[1m]`).
fn strip_variant_suffix(key: &str) -> &str {
    key.find('[').map_or(key, |bracket| &key[..bracket])
}

/// Never priced: unbilled synthetic messages, and bare family names that are ambiguous.
const UNPRICEABLE_MODELS: &[&str] = &["<synthetic>", "synthetic", "opus", "sonnet", "haiku", "fable"];

/// `lookupRate`.
pub fn lookup_rate(table: &RateTable, model: &str) -> Option<ModelRate> {
    let normalized = normalize_rate_key(model);
    let key = strip_variant_suffix(&normalized);
    let bare = bare_model_name(key);
    if bare.is_empty() || UNPRICEABLE_MODELS.contains(&bare) {
        return None;
    }
    table.get(key).copied()
}

/// The parts of a record that decide its price.
#[derive(Debug, Clone, Copy)]
pub struct PricedRecord<'a> {
    pub model: &'a str,
    pub rate_model: Option<&'a str>,
    pub totals: &'a Totals,
    pub fast: bool,
    pub reported_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricedUsage {
    pub cost_usd: f64,
    pub cost_source: UsageCostSource,
}

/// `priceUsage`: a custom price, else a reported cost, else the rate table. Reasoning is
/// inside output and is not charged again.
pub fn price_usage(table: &RateTable, record: PricedRecord<'_>, overrides: Option<&RateTable>) -> PricedUsage {
    let override_rate = overrides.and_then(|overrides| overrides.get(json::js_trim(record.model))).copied();
    if override_rate.is_none() {
        if let Some(reported) = record.reported_cost_usd.filter(|cost| cost.is_finite()) {
            return PricedUsage {
                cost_usd: reported,
                cost_source: UsageCostSource::ProviderReported,
            };
        }
    }
    let Some(rate) = override_rate.or_else(|| lookup_rate(table, record.rate_model.unwrap_or(record.model))) else {
        return PricedUsage {
            cost_usd: 0.0,
            cost_source: UsageCostSource::Unpriced,
        };
    };
    let totals = record.totals;
    let standard_cost_usd = totals.uncached_input_tokens * rate.input_cost_per_token
        + totals.cached_input_tokens * rate.cache_read_cost_per_token
        + totals.cache_creation_tokens * rate.cache_creation_cost_per_token
        + totals.output_tokens * rate.output_cost_per_token;
    PricedUsage {
        cost_usd: standard_cost_usd * if record.fast { rate.fast_multiplier } else { 1.0 },
        cost_source: UsageCostSource::ModelPriced,
    }
}

/// `cacheSavingsUsd`: cached input at the full input rate minus its actual cost.
pub fn cache_savings_usd(table: &RateTable, record: PricedRecord<'_>, overrides: Option<&RateTable>) -> f64 {
    let rate = overrides
        .and_then(|overrides| overrides.get(json::js_trim(record.model)))
        .copied()
        .or_else(|| lookup_rate(table, record.rate_model.unwrap_or(record.model)));
    let Some(rate) = rate else {
        return 0.0;
    };
    record.totals.cached_input_tokens * (rate.input_cost_per_token - rate.cache_read_cost_per_token) * if record.fast { rate.fast_multiplier } else { 1.0 }
}

#[cfg(test)]
mod tests;
