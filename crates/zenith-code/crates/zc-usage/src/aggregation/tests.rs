//! `usageAggregation.test.ts`.

use super::*;
use crate::pricing::ModelRate;
use crate::time::date_parse;

fn rates() -> Arc<RateTable> {
    let mut table = RateTable::new();
    table.insert(
        "claude-fable-5".into(),
        ModelRate {
            input_cost_per_token: 1e-5,
            output_cost_per_token: 5e-5,
            cache_read_cost_per_token: 1e-6,
            cache_creation_cost_per_token: 1.25e-5,
            fast_multiplier: 1.0,
        },
    );
    Arc::new(table)
}

fn at(text: &str) -> f64 {
    date_parse(text).unwrap()
}

/// 2026-08-07T04:05Z is still Aug 6 in Los Angeles.
fn record() -> UsageRecord {
    UsageRecord {
        provider: UsageProviderKind::Claude,
        timestamp_ms: at("2026-08-07T04:05:13.944Z"),
        model: "claude-fable-5".into(),
        rate_model: None,
        session_id: "session-a".into(),
        totals: Totals {
            uncached_input_tokens: 100.0,
            cached_input_tokens: 1000.0,
            cache_creation_tokens: 10.0,
            output_tokens: 50.0,
            reasoning_tokens: 0.0,
        },
        reported_cost_usd: None,
        fast: false,
        dedupe_key: None,
    }
}

fn options(time_zone: &str, hourly: bool) -> AggregateOptions {
    AggregateOptions {
        time_zone: time_zone.into(),
        since_day: "2026-08-01".into(),
        until_day: "2026-08-31".into(),
        rates: rates(),
        price_overrides: None,
        hourly_window: hourly.then(|| (at("2026-08-06T04:37:00.000Z"), at("2026-08-07T04:37:00.000Z"))),
    }
}

fn aggregate(records: &[UsageRecord], time_zone: &str, hourly: bool) -> AggregateResult {
    let mut aggregator = UsageAggregator::new(options(time_zone, hourly));
    for record in records {
        aggregator.add(record, None);
    }
    aggregator.finish()
}

fn keyed(key: &str) -> UsageRecord {
    UsageRecord {
        dedupe_key: Some(key.into()),
        ..record()
    }
}

fn at_time(text: &str) -> UsageRecord {
    UsageRecord {
        timestamp_ms: at(text),
        ..record()
    }
}

#[test]
fn keeps_only_the_first_record_per_dedupe_key() {
    let result = aggregate(&[keyed("msg_1:"), keyed("msg_1:"), keyed("msg_1:")], "UTC", false);
    assert_eq!(result.duplicates_dropped, 2);
    assert_eq!(result.buckets.len(), 1);
    assert_eq!(result.buckets[0].records, 1);
    assert_eq!(result.buckets[0].totals.output_tokens, 50.0);
}

#[test]
fn sums_records_without_a_dedupe_key() {
    let result = aggregate(&[record(), record()], "UTC", false);
    assert_eq!(result.duplicates_dropped, 0);
    assert_eq!(result.buckets[0].totals.output_tokens, 100.0);
}

#[test]
fn buckets_by_the_day_in_the_requested_zone() {
    assert_eq!(aggregate(&[record()], "UTC", false).buckets[0].day, "2026-08-07");
    assert_eq!(aggregate(&[record()], "America/Los_Angeles", false).buckets[0].day, "2026-08-06");
}

#[test]
fn hourly_buckets_anchor_to_the_exact_start() {
    let result = aggregate(
        &[at_time("2026-08-07T02:40:13.944Z"), at_time("2026-08-07T03:40:13.944Z")],
        "America/Los_Angeles",
        true,
    );
    let cells: Vec<(&str, Option<&str>)> = result
        .buckets
        .iter()
        .map(|bucket| (bucket.day.as_str(), bucket.hour_start.as_deref()))
        .collect();
    assert_eq!(
        cells,
        [
            ("2026-08-06", Some("2026-08-07T02:37:00.000Z")),
            ("2026-08-06", Some("2026-08-07T03:37:00.000Z"))
        ]
    );
}

#[test]
fn rolling_windows_are_start_inclusive_end_exclusive() {
    let result = aggregate(
        &[
            at_time("2026-08-06T04:36:59.999Z"),
            at_time("2026-08-06T04:37:00.000Z"),
            at_time("2026-08-07T04:36:59.999Z"),
            at_time("2026-08-07T04:37:00.000Z"),
        ],
        "UTC",
        true,
    );
    assert_eq!(result.out_of_window, 2);
    let starts: Vec<Option<&str>> = result.buckets.iter().map(|bucket| bucket.hour_start.as_deref()).collect();
    assert_eq!(starts, [Some("2026-08-06T04:37:00.000Z"), Some("2026-08-07T03:37:00.000Z")]);
}

#[test]
fn daily_payloads_stay_collapsed() {
    let result = aggregate(&[at_time("2026-08-07T04:05:13.944Z"), at_time("2026-08-07T05:05:13.944Z")], "UTC", false);
    assert_eq!(result.buckets.len(), 1);
    assert_eq!(result.buckets[0].hour_start, None);
    assert_eq!(result.buckets[0].records, 2);
}

#[test]
fn prices_against_the_rate_table() {
    let result = aggregate(&[record()], "UTC", false);
    assert!((result.buckets[0].cost_usd - 0.004625).abs() < 1e-12);
    assert_eq!(result.buckets[0].cost_source, UsageCostSource::ModelPriced);
}

#[test]
fn unpriced_models_count_tokens_but_not_cost() {
    let result = aggregate(
        &[UsageRecord {
            model: "kimi-k3".into(),
            ..record()
        }],
        "UTC",
        false,
    );
    let bucket = &result.buckets[0];
    assert_eq!(bucket.cost_usd, 0.0);
    assert_eq!(bucket.cost_source, UsageCostSource::Unpriced);
    assert_eq!(bucket.unpriced_records, 1);
    assert_eq!(bucket.totals.output_tokens, 50.0);
}

#[test]
fn reported_costs_win_over_the_table() {
    let result = aggregate(
        &[UsageRecord {
            reported_cost_usd: Some(1.25),
            ..record()
        }],
        "UTC",
        false,
    );
    assert_eq!(result.buckets[0].cost_usd, 1.25);
    assert_eq!(result.buckets[0].cost_source, UsageCostSource::ProviderReported);
}

#[test]
fn drops_records_outside_the_window() {
    let result = aggregate(&[at_time("2026-07-01T12:00:00Z")], "UTC", false);
    assert_eq!(result.out_of_window, 1);
    assert!(result.buckets.is_empty());
}

#[test]
fn reports_whether_a_record_contributed() {
    let mut aggregator = UsageAggregator::new(options("UTC", false));
    assert!(aggregator.add(&keyed("msg_1:"), None));
    assert!(!aggregator.add(&keyed("msg_1:"), None));
    assert!(!aggregator.add(&at_time("2026-07-01T12:00:00Z"), None));
}

#[test]
fn providers_and_models_get_their_own_buckets() {
    let result = aggregate(
        &[
            record(),
            UsageRecord {
                provider: UsageProviderKind::Codex,
                model: "gpt-5.6-sol".into(),
                ..record()
            },
            UsageRecord {
                model: "claude-opus-5".into(),
                ..record()
            },
        ],
        "UTC",
        false,
    );
    assert_eq!(result.buckets.len(), 3);
}

#[test]
fn sorts_by_day_hour_provider_and_model_with_locale_order() {
    let result = aggregate(
        &[
            UsageRecord {
                model: "b-model".into(),
                ..record()
            },
            UsageRecord {
                model: "B-model".into(),
                ..record()
            },
            UsageRecord {
                model: "a_model".into(),
                ..record()
            },
            UsageRecord {
                provider: UsageProviderKind::Codex,
                model: "a".into(),
                ..record()
            },
        ],
        "UTC",
        false,
    );
    let order: Vec<(&str, &str)> = result.buckets.iter().map(|bucket| (bucket.provider.as_str(), &*bucket.model)).collect();
    assert_eq!(order, [("claude", "a_model"), ("claude", "b-model"), ("claude", "B-model"), ("codex", "a")]);
}

#[test]
fn encodes_like_the_contract() {
    let mut aggregator = UsageAggregator::new(options("UTC", false));
    aggregator.add(&record(), Some("/home/projects"));
    let value = aggregator.finish().buckets[0].to_value();
    assert_eq!(
        value.to_string(),
        r#"{"day":"2026-08-07","provider":"claude","model":"claude-fable-5","sourcePath":"/home/projects","totals":{"uncachedInputTokens":100,"cachedInputTokens":1000,"cacheCreationTokens":10,"outputTokens":50,"reasoningTokens":0},"costUsd":0.004625000000000001,"cacheSavingsUsd":0.009000000000000001,"costSource":"modelPriced","records":1,"unpricedRecords":0,"sessions":1}"#
    );
}
