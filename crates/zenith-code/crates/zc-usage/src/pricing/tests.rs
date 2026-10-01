//! `usagePricing.test.ts`.

use serde_json::{json, Value};

use super::*;
use crate::cursor::cursor_rate_model;

fn table(document: Value) -> RateTable {
    parse_rate_table(&json::parse(document.to_string().as_bytes()).unwrap())
}

fn rate(input: f64, cache_read: Option<f64>) -> Value {
    let mut value = json!({"input_cost_per_token": input, "output_cost_per_token": input * 5.0});
    if let Some(cache_read) = cache_read {
        value["cache_read_input_token_cost"] = json!(cache_read);
    }
    value
}

const TOTALS: Totals = Totals {
    uncached_input_tokens: 1_000_000.0,
    cached_input_tokens: 1_000_000.0,
    cache_creation_tokens: 1_000_000.0,
    output_tokens: 1_000_000.0,
    reasoning_tokens: 500_000.0,
};

fn record(model: &str, reported_cost_usd: Option<f64>, fast: bool) -> PricedRecord<'_> {
    PricedRecord {
        model,
        rate_model: None,
        totals: &TOTALS,
        fast,
        reported_cost_usd,
    }
}

fn overrides(value: Value) -> RateTable {
    create_override_rate_table(Some(&value))
}

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
}

#[test]
fn custom_rates_win_over_public_and_reported_costs() {
    let rates = table(json!({"example-model": rate(1.0, None)}));
    let custom = overrides(json!({"example-model": {
        "inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8, "cacheReadCostPerMillionTokens": 0.5, "cacheWriteCostPerMillionTokens": 3,
    }}));
    for reported in [None, Some(99.0)] {
        assert_eq!(
            price_usage(&rates, record("example-model", reported, false), Some(&custom)),
            PricedUsage {
                cost_usd: 13.5,
                cost_source: UsageCostSource::ModelPriced
            }
        );
    }
    assert_eq!(cache_savings_usd(&rates, record("example-model", None, false), Some(&custom)), 1.5);
}

#[test]
fn cursor_cache_savings_use_the_base_model_rate() {
    let rates = table(json!({
        "claude-fable-5-1": rate(10e-6, Some(1e-6)),
        "xai/grok-4.7": rate(2e-6, Some(0.5e-6)),
        "openrouter/x-ai/grok-4.7": rate(3e-6, Some(0.5e-6)),
    }));
    let savings = |model: &str| {
        let rate_model = cursor_rate_model(model);
        cache_savings_usd(
            &rates,
            PricedRecord {
                rate_model: Some(&rate_model),
                ..record(model, Some(0.25), false)
            },
            None,
        )
    };
    close(savings("claude-fable-5-1-thinking-high"), 9.0);
    close(savings("cursor-grok-4.7-high-fast"), 1.5);
    assert_eq!(savings("default"), 0.0);
    let rate_model = cursor_rate_model("grok-4.7-xhigh-fast");
    assert_eq!(
        price_usage(
            &rates,
            PricedRecord {
                rate_model: Some(&rate_model),
                ..record("grok-4.7-xhigh-fast", Some(0.25), false)
            },
            None
        ),
        PricedUsage {
            cost_usd: 0.25,
            cost_source: UsageCostSource::ProviderReported
        }
    );
}

#[test]
fn unknown_models_price_offline_with_input_prices_for_omitted_cache_rates() {
    let rates = table(json!({}));
    let custom = overrides(json!({"example-model": {"inputCostPerMillionTokens": 2, "outputCostPerMillionTokens": 8}}));
    assert_eq!(
        price_usage(&rates, record("example-model", None, false), Some(&custom)),
        PricedUsage {
            cost_usd: 14.0,
            cost_source: UsageCostSource::ModelPriced
        }
    );
    assert_eq!(cache_savings_usd(&rates, record("example-model", None, false), Some(&custom)), 0.0);
}

#[test]
fn explicit_zero_rates_and_exact_trimmed_ids() {
    let rates = table(json!({}));
    let custom = overrides(json!({" vendor/example-model[1m] ": {"inputCostPerMillionTokens": 0, "outputCostPerMillionTokens": 0}}));
    assert_eq!(
        price_usage(&rates, record(" vendor/example-model[1m] ", Some(99.0), false), Some(&custom)),
        PricedUsage {
            cost_usd: 0.0,
            cost_source: UsageCostSource::ModelPriced
        }
    );
    for model in [
        "example-model[1m]",
        "vendor/example-model",
        "vendor/Example-model[1m]",
        "other/example-model[1m]",
    ] {
        assert_eq!(
            price_usage(&rates, record(model, None, false), Some(&custom)).cost_source,
            UsageCostSource::Unpriced
        );
        assert_eq!(
            price_usage(&rates, record(model, Some(99.0), false), Some(&custom)),
            PricedUsage {
                cost_usd: 99.0,
                cost_source: UsageCostSource::ProviderReported
            }
        );
    }
}

#[test]
fn fast_mode_prices_at_the_published_multiple() {
    let mut opus = rate(4e-6, Some(2e-7));
    opus["provider_specific_entry"] = json!({"fast": 2, "us": 1.1});
    let mut fable = rate(1e-5, Some(2.5e-7));
    fable["provider_specific_entry"] = json!({"us": 1.1});
    let rates = table(json!({"claude-opus-5-5": opus, "claude-fable-5-1": fable}));
    let custom = overrides(json!({"claude-opus-5-5": {"inputCostPerMillionTokens": 4, "outputCostPerMillionTokens": 20}}));
    let cost = |model: &str, fast: bool, custom: Option<&RateTable>| price_usage(&rates, record(model, None, fast), custom).cost_usd;
    close(cost("claude-opus-5-5", true, None), 2.0 * cost("claude-opus-5-5", false, None));
    close(
        cache_savings_usd(&rates, record("claude-opus-5-5", None, true), None),
        2.0 * cache_savings_usd(&rates, record("claude-opus-5-5", None, false), None),
    );
    assert_eq!(cost("claude-fable-5-1", true, None), cost("claude-fable-5-1", false, None));
    assert_eq!(cost("claude-opus-5-5", true, Some(&custom)), cost("claude-opus-5-5", false, Some(&custom)));
}

#[test]
fn canonical_rate_stays_separate_from_qualified_ones_in_either_order() {
    for canonical_first in [true, false] {
        let mut document = serde_json::Map::new();
        let entries = [
            ("claude-fable-5", rate(1e-5, Some(1e-6))),
            ("deepinfra/anthropic/claude-fable-5", rate(1e-5, None)),
        ];
        let ordered: Vec<_> = if canonical_first {
            entries.to_vec()
        } else {
            entries.iter().rev().cloned().collect()
        };
        for (name, value) in ordered {
            document.insert(name.to_owned(), value);
        }
        let rates = table(Value::Object(document));
        assert_eq!(lookup_rate(&rates, "claude-fable-5").unwrap().cache_read_cost_per_token, 1e-6);
        assert_eq!(
            lookup_rate(&rates, "deepinfra/anthropic/claude-fable-5").unwrap().cache_read_cost_per_token,
            1e-5
        );
        assert!(lookup_rate(&rates, "other/claude-fable-5").is_none());
    }
}

#[test]
fn bracketed_variants_price_at_the_base_rate() {
    let rates = table(json!({"claude-fable-5-1": rate(1e-5, Some(2.5e-7))}));
    assert_eq!(lookup_rate(&rates, "claude-fable-5-1[1m]"), lookup_rate(&rates, "claude-fable-5-1"));
    assert!(lookup_rate(&rates, "anthropic/Claude-Fable-5-1[1m]").is_none());
}

#[test]
fn bare_alias_when_every_qualified_entry_agrees() {
    let rates = table(json!({"provider-a/example-model": rate(1.0, None), "provider-b/example-model": rate(1.0, None)}));
    assert_eq!(lookup_rate(&rates, "example-model"), lookup_rate(&rates, "provider-a/example-model"));
}

#[test]
fn ambiguous_bare_name_stays_unpriced() {
    let rates = table(json!({"provider-a/example-model": rate(1.0, None), "provider-b/example-model": rate(3.0, None)}));
    assert_eq!(lookup_rate(&rates, "provider-a/example-model").unwrap().input_cost_per_token, 1.0);
    assert_eq!(lookup_rate(&rates, "provider-b/example-model").unwrap().input_cost_per_token, 3.0);
    assert!(lookup_rate(&rates, "example-model").is_none());
}

#[test]
fn unpriceable_models() {
    let rates = table(json!({"opus": rate(1.0, None), "<synthetic>": rate(1.0, None)}));
    assert!(lookup_rate(&rates, "opus").is_none());
    assert!(lookup_rate(&rates, "<synthetic>").is_none());
}
