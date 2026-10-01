//! Folding records into `(day, hourStart?, provider, model, sourcePath?)` buckets
//! (`usageAggregation.ts`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use jiff::tz::TimeZone;
use serde_json::{Map, Value};
use zc_contracts::{UsageCostSource, UsageProviderKind};

use crate::collate::locale_compare;
use crate::json::num;
use crate::pricing::{cache_savings_usd, price_usage, PricedRecord, RateTable};
use crate::records::{Totals, UsageRecord};
use crate::time::{format_day, iso_from_millis, resolve_time_zone};

const HOUR_MS: f64 = 60.0 * 60.0 * 1000.0;

/// `AggregateOptions`.
#[derive(Debug, Clone)]
pub struct AggregateOptions {
    pub time_zone: String,
    pub since_day: String,
    pub until_day: String,
    pub rates: Arc<RateTable>,
    pub price_overrides: Option<RateTable>,
    /// `Some((sinceTimeMs, untilTimeMs))` for hourly resolution.
    pub hourly_window: Option<(f64, f64)>,
}

/// One finished bucket (`UsageBucket`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub day: String,
    pub hour_start: Option<String>,
    pub provider: UsageProviderKind,
    pub model: Arc<str>,
    pub source_path: Option<String>,
    pub totals: Totals,
    pub cost_usd: f64,
    pub cache_savings_usd: f64,
    pub cost_source: UsageCostSource,
    pub records: u64,
    pub unpriced_records: u64,
    pub sessions: usize,
}

impl Bucket {
    /// The encoded `UsageBucket`.
    pub fn to_value(&self) -> Value {
        let mut bucket = Map::new();
        bucket.insert("day".into(), Value::String(self.day.clone()));
        if let Some(hour_start) = &self.hour_start {
            bucket.insert("hourStart".into(), Value::String(hour_start.clone()));
        }
        bucket.insert("provider".into(), Value::String(self.provider.as_str().to_owned()));
        bucket.insert("model".into(), Value::String(self.model.to_string()));
        if let Some(source_path) = &self.source_path {
            bucket.insert("sourcePath".into(), Value::String(source_path.clone()));
        }
        bucket.insert("totals".into(), self.totals.to_value());
        bucket.insert("costUsd".into(), num(self.cost_usd));
        bucket.insert("cacheSavingsUsd".into(), num(self.cache_savings_usd));
        bucket.insert("costSource".into(), Value::String(self.cost_source.as_str().to_owned()));
        #[allow(clippy::cast_precision_loss)]
        {
            bucket.insert("records".into(), num(self.records as f64));
            bucket.insert("unpricedRecords".into(), num(self.unpriced_records as f64));
            bucket.insert("sessions".into(), num(self.sessions as f64));
        }
        Value::Object(bucket)
    }
}

#[derive(Debug)]
struct MutableBucket {
    day: String,
    hour_start: String,
    provider: UsageProviderKind,
    model: Arc<str>,
    source_path: String,
    totals: Totals,
    cost_usd: f64,
    cache_savings_usd: f64,
    records: u64,
    unpriced_records: u64,
    provider_reported_records: u64,
    sessions: HashSet<Arc<str>>,
}

/// `AggregateResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateResult {
    pub buckets: Vec<Bucket>,
    /// Records dropped because an earlier record had the same dedupe key.
    pub duplicates_dropped: u64,
    /// Records outside the requested window.
    pub out_of_window: u64,
}

/// `UsageAggregator`: de-duplication is global across the whole scan (Claude Code copies a
/// message's records forward when a session is resumed or forked).
pub struct UsageAggregator {
    options: AggregateOptions,
    zone: TimeZone,
    buckets: Vec<MutableBucket>,
    index: HashMap<(String, String, UsageProviderKind, Arc<str>, String), usize>,
    seen: HashSet<String>,
    duplicates_dropped: u64,
    out_of_window: u64,
}

impl UsageAggregator {
    pub fn new(options: AggregateOptions) -> Self {
        let zone = resolve_time_zone(&options.time_zone);
        Self {
            options,
            zone,
            buckets: Vec::new(),
            index: HashMap::new(),
            seen: HashSet::new(),
            duplicates_dropped: 0,
            out_of_window: 0,
        }
    }

    /// Folds one record in; whether it contributed (so callers count in-window sessions).
    pub fn add(&mut self, record: &UsageRecord, source_path: Option<&str>) -> bool {
        if let Some(key) = &record.dedupe_key {
            if !self.seen.insert(key.clone()) {
                self.duplicates_dropped += 1;
                return false;
            }
        }
        if let Some((since, until)) = self.options.hourly_window {
            if record.timestamp_ms < since || record.timestamp_ms >= until {
                self.out_of_window += 1;
                return false;
            }
        }
        let day = format_day(&self.zone, record.timestamp_ms);
        if self.options.hourly_window.is_none() && (day < self.options.since_day || day > self.options.until_day) {
            self.out_of_window += 1;
            return false;
        }
        let hour_start = match self.options.hourly_window {
            None => String::new(),
            Some((since, _)) => iso_from_millis(since + ((record.timestamp_ms - since) / HOUR_MS).floor() * HOUR_MS).unwrap_or_default(),
        };
        let key = (day, hour_start, record.provider, record.model.clone(), source_path.unwrap_or("").to_owned());
        let index = match self.index.get(&key) {
            Some(index) => *index,
            None => {
                let index = self.buckets.len();
                self.buckets.push(MutableBucket {
                    day: key.0.clone(),
                    hour_start: key.1.clone(),
                    provider: key.2,
                    model: key.3.clone(),
                    source_path: key.4.clone(),
                    totals: Totals::default(),
                    cost_usd: 0.0,
                    cache_savings_usd: 0.0,
                    records: 0,
                    unpriced_records: 0,
                    provider_reported_records: 0,
                    sessions: HashSet::new(),
                });
                self.index.insert(key, index);
                index
            }
        };
        let priced_record = PricedRecord {
            model: &record.model,
            rate_model: record.rate_model.as_deref(),
            totals: &record.totals,
            fast: record.fast,
            reported_cost_usd: record.reported_cost_usd,
        };
        let overrides = self.options.price_overrides.as_ref();
        let priced = price_usage(&self.options.rates, priced_record, overrides);
        let savings = cache_savings_usd(&self.options.rates, priced_record, overrides);
        let bucket = &mut self.buckets[index];
        bucket.totals = bucket.totals.add(&record.totals);
        bucket.cost_usd += priced.cost_usd;
        bucket.cache_savings_usd += savings;
        bucket.records += 1;
        match priced.cost_source {
            UsageCostSource::Unpriced => bucket.unpriced_records += 1,
            UsageCostSource::ProviderReported => bucket.provider_reported_records += 1,
            UsageCostSource::ModelPriced => {}
        }
        if !record.session_id.is_empty() {
            bucket.sessions.insert(record.session_id.clone());
        }
        true
    }

    /// The buckets, sorted by day, hour, provider and model (`localeCompare`, stable).
    pub fn finish(self) -> AggregateResult {
        let mut buckets: Vec<Bucket> = self
            .buckets
            .into_iter()
            .map(|bucket| Bucket {
                cost_source: if bucket.unpriced_records == bucket.records {
                    UsageCostSource::Unpriced
                } else if bucket.provider_reported_records == bucket.records {
                    UsageCostSource::ProviderReported
                } else {
                    UsageCostSource::ModelPriced
                },
                day: bucket.day,
                hour_start: (!bucket.hour_start.is_empty()).then_some(bucket.hour_start),
                provider: bucket.provider,
                model: bucket.model,
                source_path: (!bucket.source_path.is_empty()).then_some(bucket.source_path),
                totals: bucket.totals,
                cost_usd: bucket.cost_usd,
                cache_savings_usd: bucket.cache_savings_usd,
                records: bucket.records,
                unpriced_records: bucket.unpriced_records,
                sessions: bucket.sessions.len(),
            })
            .collect();
        buckets.sort_by(|a, b| {
            locale_compare(&a.day, &b.day)
                .then_with(|| locale_compare(a.hour_start.as_deref().unwrap_or(""), b.hour_start.as_deref().unwrap_or("")))
                .then_with(|| locale_compare(a.provider.as_str(), b.provider.as_str()))
                .then_with(|| locale_compare(&a.model, &b.model))
        });
        AggregateResult {
            buckets,
            duplicates_dropped: self.duplicates_dropped,
            out_of_window: self.out_of_window,
        }
    }
}

#[cfg(test)]
mod tests;
