//! Pure parsers for the provider CLIs' session transcripts (`usageTranscripts.ts`).
//!
//! Each parser reads one JSON record (already projected to the fields it needs, see
//! [`crate::json`]); none touches the filesystem. Numbers are JS doubles throughout, so sums
//! and costs come out bit-for-bit like the TS server's.

use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::UsageProviderKind;

use crate::json::{self, Sel, J};
use crate::time::date_parse;

/// `UsageTokenTotals`. `reasoning_tokens` is a subset of `output_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Totals {
    pub uncached_input_tokens: f64,
    pub cached_input_tokens: f64,
    pub cache_creation_tokens: f64,
    pub output_tokens: f64,
    pub reasoning_tokens: f64,
}

impl Totals {
    /// `addTotals`.
    pub fn add(&self, other: &Totals) -> Totals {
        Totals {
            uncached_input_tokens: self.uncached_input_tokens + other.uncached_input_tokens,
            cached_input_tokens: self.cached_input_tokens + other.cached_input_tokens,
            cache_creation_tokens: self.cache_creation_tokens + other.cache_creation_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            reasoning_tokens: self.reasoning_tokens + other.reasoning_tokens,
        }
    }

    /// `totalTokens`: reasoning is inside output and is not added again.
    pub fn total(&self) -> f64 {
        self.uncached_input_tokens + self.cached_input_tokens + self.cache_creation_tokens + self.output_tokens
    }

    /// The encoded `UsageTokenTotals`.
    pub fn to_value(&self) -> Value {
        json!({
            "uncachedInputTokens": json::num(self.uncached_input_tokens),
            "cachedInputTokens": json::num(self.cached_input_tokens),
            "cacheCreationTokens": json::num(self.cache_creation_tokens),
            "outputTokens": json::num(self.output_tokens),
            "reasoningTokens": json::num(self.reasoning_tokens),
        })
    }

    /// `JSON.stringify(totals)`.
    pub fn stringify(&self) -> String {
        let n = zc_providers::js_json::format_js_number;
        format!(
            "{{\"uncachedInputTokens\":{},\"cachedInputTokens\":{},\"cacheCreationTokens\":{},\"outputTokens\":{},\"reasoningTokens\":{}}}",
            n(self.uncached_input_tokens),
            n(self.cached_input_tokens),
            n(self.cache_creation_tokens),
            n(self.output_tokens),
            n(self.reasoning_tokens)
        )
    }
}

/// One priced unit of usage (`UsageRecord`).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    pub provider: UsageProviderKind,
    pub timestamp_ms: f64,
    pub model: Arc<str>,
    /// Rate-table key when the display name carries tiers the table does not know (Cursor).
    pub rate_model: Option<String>,
    pub session_id: Arc<str>,
    pub totals: Totals,
    pub reported_cost_usd: Option<f64>,
    /// Fast mode bills at the model's published multiple (Claude Code only).
    pub fast: bool,
    /// Cross-file de-duplication key; `None` for inherently unique records.
    pub dedupe_key: Option<String>,
}

/// `int(value)`: positive finite numbers, truncated; anything else 0.
pub fn int(value: Option<&J>) -> f64 {
    match value.and_then(J::as_finite) {
        Some(number) if number > 0.0 => number.trunc(),
        _ => 0.0,
    }
}

fn timestamp_ms(value: Option<&J>) -> Option<f64> {
    date_parse(value?.as_str()?)
}

fn string_or_empty(value: Option<&J>) -> &str {
    value.and_then(J::as_str).unwrap_or("")
}

/// The Claude transcript fields the parser reads (`USAGE_FIELDS.claude`).
pub const CLAUDE_FIELDS: Sel = Sel::Fields(&[
    ("type", Sel::All),
    ("timestamp", Sel::All),
    ("requestId", Sel::All),
    ("sessionId", Sel::All),
    ("costUSD", Sel::All),
    ("message", Sel::Fields(&[("id", Sel::All), ("model", Sel::All), ("usage", Sel::All)])),
]);

/// The Codex rollout fields (`USAGE_FIELDS.codex`).
pub const CODEX_FIELDS: Sel = Sel::Fields(&[
    ("type", Sel::All),
    ("timestamp", Sel::All),
    (
        "payload",
        Sel::Fields(&[
            ("type", Sel::All),
            ("id", Sel::All),
            ("session_id", Sel::All),
            ("model", Sel::All),
            ("forked_from_id", Sel::All),
            (
                "source",
                Sel::Fields(&[("subagent", Sel::Fields(&[("thread_spawn", Sel::Fields(&[("parent_thread_id", Sel::All)]))]))]),
            ),
            ("info", Sel::Fields(&[("last_token_usage", Sel::All)])),
        ]),
    ),
]);

/// The Grok Build fields (`USAGE_FIELDS.grok`).
pub const GROK_FIELDS: Sel = Sel::Fields(&[
    ("timestamp", Sel::All),
    (
        "params",
        Sel::Fields(&[
            ("sessionId", Sel::All),
            ("_meta", Sel::Fields(&[("agentTimestampMs", Sel::All)])),
            (
                "update",
                Sel::Fields(&[("sessionUpdate", Sel::All), ("prompt_id", Sel::All), ("usage", Sel::All)]),
            ),
        ]),
    ),
]);

/* ---------------------------------------------------------------------------------------- */
/* Claude Code                                                                              */
/* ---------------------------------------------------------------------------------------- */

/// `parseClaudeLine`.
pub fn parse_claude_line(line: &[u8]) -> Option<UsageRecord> {
    parse_claude_record(&json::parse_selected(line, &CLAUDE_FIELDS)?)
}

/// `parseClaudeRecord`: one record per assistant content block, each repeating the
/// message's full usage; the caller drops repeats by `dedupe_key`.
pub fn parse_claude_record(record: &J) -> Option<UsageRecord> {
    if !record.is_object_like() || record.get("type").and_then(J::as_str) != Some("assistant") {
        return None;
    }
    let message = record.get("message").filter(|value| value.is_object_like())?;
    let usage = message.get("usage").filter(|value| value.is_object_like())?;
    let timestamp_ms = timestamp_ms(record.get("timestamp"))?;
    let model = string_or_empty(message.get("model"));
    if model.is_empty() {
        return None;
    }
    let message_id = message.get("id").and_then(J::as_str);
    let request_id = record.get("requestId").and_then(J::as_str);
    // Matches ccusage: the message/request pair, or whichever half exists.
    let dedupe_key = if message_id.is_none() && request_id.is_none() {
        None
    } else {
        Some(format!("{}:{}", message_id.unwrap_or(""), request_id.unwrap_or("")))
    };
    Some(UsageRecord {
        provider: UsageProviderKind::Claude,
        timestamp_ms,
        model: Arc::from(model),
        rate_model: None,
        session_id: Arc::from(string_or_empty(record.get("sessionId"))),
        totals: Totals {
            uncached_input_tokens: int(usage.get("input_tokens")),
            cached_input_tokens: int(usage.get("cache_read_input_tokens")),
            cache_creation_tokens: int(usage.get("cache_creation_input_tokens")),
            output_tokens: int(usage.get("output_tokens")),
            // Anthropic folds thinking tokens into output and does not break them out.
            reasoning_tokens: 0.0,
        },
        reported_cost_usd: record.get("costUSD").and_then(J::as_finite),
        fast: usage.get("speed").and_then(J::as_str) == Some("fast"),
        dedupe_key,
    })
}

/* ---------------------------------------------------------------------------------------- */
/* Codex                                                                                    */
/* ---------------------------------------------------------------------------------------- */

/// Rolling state for one Codex rollout (`CodexScanState`): `token_count` events carry no
/// model, so the model of the latest `turn_context` carries forward.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodexScanState {
    pub model: String,
    pub session_id: String,
    pub last_usage_signature: Option<String>,
    pub saw_session_meta: bool,
    /// While true, leading usage events are re-stamped copies of the parent's history.
    pub suppressing_fork_copies: bool,
    pub fork_copy_anchor_ms: f64,
}

/// Copied parent history lands in one burst (0-40 ms gaps); a child's first real turn
/// comes seconds later. ccusage uses the same threshold.
const FORK_COPY_MAX_GAP_MS: f64 = 1000.0;

fn is_forked_session_meta(payload: &J) -> bool {
    if payload.get("forked_from_id").and_then(J::as_str).is_some() {
        return true;
    }
    let spawn = json::get(json::get(payload.get("source"), "subagent"), "thread_spawn");
    spawn.and_then(|spawn| spawn.get("parent_thread_id")).and_then(J::as_str).is_some()
}

/// `parseCodexLine`.
pub fn parse_codex_line(line: &[u8], state: &mut CodexScanState) -> Option<UsageRecord> {
    parse_codex_record(&json::parse_selected(line, &CODEX_FIELDS)?, state)
}

/// `parseCodexRecord`: feeds one rollout record into `state`, returning a record for a
/// usage event. Deltas come from `last_token_usage`; consecutive duplicates are dropped.
pub fn parse_codex_record(record: &J, state: &mut CodexScanState) -> Option<UsageRecord> {
    if !record.is_object_like() {
        return None;
    }
    let payload = record.get("payload").filter(|value| value.is_object_like())?;
    let record_type = record.get("type").and_then(J::as_str);

    if record_type == Some("session_meta") {
        // Only the first meta describes this file's own session; a fork repeats its
        // ancestors' metas right after it.
        if state.saw_session_meta {
            return None;
        }
        state.saw_session_meta = true;
        let id = match payload.get("id") {
            None | Some(J::Null) => payload.get("session_id"),
            some => some,
        };
        if let Some(id) = id.and_then(J::as_str) {
            id.clone_into(&mut state.session_id);
        }
        if let Some(meta_timestamp) = timestamp_ms(record.get("timestamp")) {
            if is_forked_session_meta(payload) {
                state.suppressing_fork_copies = true;
                state.fork_copy_anchor_ms = meta_timestamp;
            }
        }
        return None;
    }

    if record_type == Some("turn_context") {
        if let Some(model) = payload.get("model").and_then(J::as_str) {
            model.clone_into(&mut state.model);
        }
        return None;
    }

    if payload.get("type").and_then(J::as_str) != Some("token_count") {
        return None;
    }
    let info = payload.get("info").filter(|value| value.is_object_like())?;
    let last = info.get("last_token_usage").filter(|value| value.is_object_like())?;

    // Only an otherwise eligible event may consume the duplicate signature.
    let timestamp_ms = timestamp_ms(record.get("timestamp"))?;
    if state.model.is_empty() {
        return None;
    }
    let signature = json::stringify(last);
    if state.last_usage_signature.as_deref() == Some(signature.as_str()) {
        return None;
    }
    state.last_usage_signature = Some(signature);

    // A fork's copied history was counted from the parent's own file.
    if state.suppressing_fork_copies {
        if timestamp_ms - state.fork_copy_anchor_ms < FORK_COPY_MAX_GAP_MS {
            state.fork_copy_anchor_ms = timestamp_ms;
            return None;
        }
        state.suppressing_fork_copies = false;
    }

    let input_tokens = int(last.get("input_tokens"));
    let cached_input_tokens = int(last.get("cached_input_tokens"));
    let cache_creation_tokens = int(last.get("cache_write_input_tokens"));
    let output_tokens = int(last.get("output_tokens"));
    let totals = Totals {
        // Codex reports input inclusive of the cached portion.
        uncached_input_tokens: f64::max(0.0, input_tokens - cached_input_tokens - cache_creation_tokens),
        cached_input_tokens,
        cache_creation_tokens,
        output_tokens,
        reasoning_tokens: f64::min(output_tokens, int(last.get("reasoning_output_tokens"))),
    };
    if totals.total() == 0.0 {
        return None;
    }
    Some(UsageRecord {
        provider: UsageProviderKind::Codex,
        timestamp_ms,
        model: Arc::from(state.model.as_str()),
        rate_model: None,
        session_id: Arc::from(state.session_id.as_str()),
        totals,
        reported_cost_usd: None,
        fast: false,
        dedupe_key: None,
    })
}

/* ---------------------------------------------------------------------------------------- */
/* Grok Build                                                                               */
/* ---------------------------------------------------------------------------------------- */

/// Grok reports cost in ticks: 1 USD = 10^10 ticks.
pub const GROK_COST_USD_TICKS_PER_DOLLAR: f64 = 10_000_000_000.0;

fn grok_cost_ticks_to_usd(ticks: Option<f64>) -> Option<f64> {
    let ticks = ticks?;
    (ticks.is_finite() && ticks >= 0.0).then(|| ticks / GROK_COST_USD_TICKS_PER_DOLLAR)
}

#[derive(Debug, Clone, Copy)]
struct GrokTotals {
    input_tokens: f64,
    output_tokens: f64,
    cached_read_tokens: f64,
    cache_creation_tokens: f64,
    reasoning_tokens: f64,
    cost_usd_ticks: Option<f64>,
}

fn read_grok_totals(value: &J) -> Option<GrokTotals> {
    if !value.is_object_like() {
        return None;
    }
    Some(GrokTotals {
        input_tokens: int(value.get("inputTokens")),
        output_tokens: int(value.get("outputTokens")),
        cached_read_tokens: int(value.get("cachedReadTokens")),
        cache_creation_tokens: int(value.get("cacheCreationTokens")),
        reasoning_tokens: int(value.get("reasoningTokens")),
        cost_usd_ticks: value.get("costUsdTicks").and_then(J::as_finite),
    })
}

fn grok_usage(totals: &GrokTotals) -> Totals {
    let cached_input_tokens = totals.cached_read_tokens;
    let cache_creation_tokens = totals.cache_creation_tokens;
    let output_tokens = totals.output_tokens;
    Totals {
        // Inclusive of the cached portion, like Codex.
        uncached_input_tokens: f64::max(0.0, totals.input_tokens - cached_input_tokens - cache_creation_tokens),
        cached_input_tokens,
        cache_creation_tokens,
        output_tokens,
        reasoning_tokens: f64::min(output_tokens, totals.reasoning_tokens),
    }
}

/// `parseGrokLine`.
pub fn parse_grok_line(line: &[u8]) -> Vec<UsageRecord> {
    json::parse_selected(line, &GROK_FIELDS)
        .map(|record| parse_grok_record(&record))
        .unwrap_or_default()
}

/// `parseGrokRecord`: usage of a `turn_completed` update, one record per model of
/// `usage.modelUsage` (or one `grok` record without it).
pub fn parse_grok_record(record: &J) -> Vec<UsageRecord> {
    let Some(params) = record.get("params").filter(|value| value.is_object_like()) else {
        return Vec::new();
    };
    let Some(update) = params.get("update").filter(|value| value.is_object_like()) else {
        return Vec::new();
    };
    if update.get("sessionUpdate").and_then(J::as_str) != Some("turn_completed") {
        return Vec::new();
    }
    let Some(usage) = update.get("usage").filter(|value| value.is_object_like()) else {
        return Vec::new();
    };
    let session_id = string_or_empty(params.get("sessionId"));
    let prompt_id = update.get("prompt_id").and_then(J::as_str);

    // The high-resolution agent clock first, then the outer unix seconds.
    let mut timestamp_ms = json::get(params.get("_meta").filter(|meta| meta.is_object_like()), "agentTimestampMs").and_then(J::as_finite);
    if timestamp_ms.is_none() {
        if let Some(timestamp) = record.get("timestamp").and_then(J::as_finite) {
            timestamp_ms = Some(if timestamp > 1e12 { timestamp } else { timestamp * 1000.0 });
        }
    }
    let Some(timestamp_ms) = timestamp_ms else {
        return Vec::new();
    };
    let Some(top_level) = read_grok_totals(usage) else {
        return Vec::new();
    };

    let mut model_entries: Vec<(&str, GrokTotals)> = Vec::new();
    if let Some(model_usage) = usage.get("modelUsage").and_then(J::as_obj) {
        for (model, raw) in model_usage.entries() {
            if model.is_empty() {
                continue;
            }
            if let Some(totals) = read_grok_totals(raw) {
                model_entries.push((model, totals));
            }
        }
    }
    let session: Arc<str> = Arc::from(session_id);

    if model_entries.is_empty() {
        let totals = grok_usage(&top_level);
        if totals.total() == 0.0 {
            return Vec::new();
        }
        return vec![UsageRecord {
            provider: UsageProviderKind::Grok,
            timestamp_ms,
            model: Arc::from("grok"),
            rate_model: None,
            session_id: session,
            totals,
            reported_cost_usd: grok_cost_ticks_to_usd(top_level.cost_usd_ticks),
            fast: false,
            // Without a prompt id two same-second updates cannot be told apart.
            dedupe_key: prompt_id.map(|prompt| format!("{session_id}:{prompt}:grok")),
        }];
    }

    // Models with their own ticks keep them; the remaining aggregate cost is pro-rated
    // over the unticked models by token share. Zero-token rows never count.
    let top_level_cost_usd = grok_cost_ticks_to_usd(top_level.cost_usd_ticks);
    let mut used_ticked_cost_usd = 0.0;
    let mut unticked_token_denominator = 0.0;
    for (_, totals) in &model_entries {
        let tokens = grok_usage(totals).total();
        if tokens == 0.0 {
            continue;
        }
        if totals.cost_usd_ticks.is_some() {
            used_ticked_cost_usd += grok_cost_ticks_to_usd(totals.cost_usd_ticks).unwrap_or(0.0);
        } else {
            unticked_token_denominator += tokens;
        }
    }
    let remaining_cost_usd = top_level_cost_usd.map(|top| f64::max(0.0, top - used_ticked_cost_usd));

    let mut results = Vec::new();
    for (model, entry) in &model_entries {
        let totals = grok_usage(entry);
        if totals.total() == 0.0 {
            continue;
        }
        let mut reported_cost_usd = grok_cost_ticks_to_usd(entry.cost_usd_ticks);
        if reported_cost_usd.is_none() && unticked_token_denominator > 0.0 {
            if let Some(remaining) = remaining_cost_usd {
                reported_cost_usd = Some(remaining * (totals.total() / unticked_token_denominator));
            }
        }
        results.push(UsageRecord {
            provider: UsageProviderKind::Grok,
            timestamp_ms,
            model: Arc::from(*model),
            rate_model: None,
            session_id: session.clone(),
            totals,
            reported_cost_usd,
            fast: false,
            dedupe_key: prompt_id.map(|prompt| format!("{session_id}:{prompt}:{model}")),
        });
    }
    results
}

#[cfg(test)]
mod tests;
