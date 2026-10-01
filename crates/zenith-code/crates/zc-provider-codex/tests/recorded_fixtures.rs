//! Gate 2 of WP-14: recorded Codex threads. The provider event logs
//! (`~/.zenith/code/userdata/logs/provider/events.<thread>.log`) record each native event the
//! TS adapter consumed (`NTIVE:`) and each canonical event it produced (`CANON:`). Feeding the
//! NTIVE frames of every Codex thread to the Rust mapper must reproduce the CANON frames.
//!
//! The logs hold personal data, so they are not in the repository: point
//! `ZC_CODEX_RECORDED_LOGS` at a COPY of the directory (never the live one):
//!
//!   cp -R ~/.zenith/code/userdata/logs/provider /tmp/provider-logs
//!   ZC_CODEX_RECORDED_LOGS=/tmp/provider-logs cargo test -p zc-provider-codex --test recorded_fixtures -- --nocapture
//!
//! Comparison rules (what the logger does to frames): transient canonical types (deltas,
//! `item.updated`, `task.progress`, …) are never logged, nor are transient native methods;
//! frames the logger summarized (`"truncated": true`) are skipped; `providerInstanceId` is
//! stamped by the provider service, so it is copied from the native event. Numbers compare by
//! value. Each produced event is also decoded into `ProviderRuntimeEvent` and re-encoded, which
//! must give the same JSON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use zc_contracts::ProviderEvent;
use zc_provider_codex::mapping::{to_runtime_event, CodexEventMapper};

const TRANSIENT_CANONICAL: &[&str] = &[
    "content.delta",
    "hook.progress",
    "item.updated",
    "task.progress",
    "thread.realtime.audio.delta",
    "tool.progress",
    "turn.proposed.delta",
];

fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y)),
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| same(x, y))),
        _ => left == right,
    }
}

fn diff(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => diff(&format!("{path}.{key}"), x, y, out),
                    (Some(x), None) => out.push(format!("{path}.{key}: only rust = {}", short(x))),
                    (None, Some(y)) => out.push(format!("{path}.{key}: only ts = {}", short(y))),
                    (None, None) => {}
                }
            }
        }
        _ if !same(left, right) => out.push(format!("{path}: rust {} != ts {}", short(left), short(right))),
        _ => {}
    }
}

fn short(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 160 {
        format!("{}…", &text[..text.char_indices().nth(160).map_or(text.len(), |(index, _)| index)])
    } else {
        text
    }
}

fn truncated(value: &Value) -> bool {
    value.to_string().contains("\"truncated\":true")
}

#[derive(Default)]
struct Totals {
    threads: usize,
    native: usize,
    canonical: usize,
    matched: usize,
    skipped: usize,
    mismatches: Vec<String>,
}

fn replay(files: &[PathBuf], totals: &mut Totals) {
    let mut natives: Vec<Value> = Vec::new();
    let mut canonicals: Vec<Value> = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(file).unwrap();
        for line in text.lines() {
            let Some(rest) = line.split_once("] ").map(|(_, rest)| rest) else { continue };
            if let Some(json) = rest.strip_prefix("NTIVE: ") {
                if let Ok(value) = serde_json::from_str::<Value>(json) {
                    natives.push(value);
                }
            } else if let Some(json) = rest.strip_prefix("CANON: ") {
                if let Ok(value) = serde_json::from_str::<Value>(json) {
                    canonicals.push(value);
                }
            }
        }
    }
    if !natives.iter().any(|event| event["provider"] == "codex") {
        return;
    }
    totals.threads += 1;
    let mut mapper = CodexEventMapper::new(false);
    let mut produced: Vec<Value> = Vec::new();
    let mut skipped_ids: Vec<String> = Vec::new();
    for native in &natives {
        if native["provider"] != "codex" {
            continue;
        }
        totals.native += 1;
        let event: ProviderEvent = match serde_json::from_value(native.clone()) {
            Ok(event) => event,
            Err(error) => {
                totals.mismatches.push(format!("NTIVE {} does not decode: {error}", native["id"]));
                continue;
            }
        };
        let skip = truncated(native);
        for mut value in mapper.process(&event).events {
            if TRANSIENT_CANONICAL.contains(&value["type"].as_str().unwrap_or_default()) {
                continue;
            }
            if let Some(instance) = native.get("providerInstanceId") {
                value["providerInstanceId"] = instance.clone();
            }
            if skip {
                skipped_ids.push(value["eventId"].as_str().unwrap_or_default().to_owned());
            }
            produced.push(value);
        }
    }
    let expected: Vec<&Value> = canonicals.iter().filter(|event| event["provider"] == "codex").collect();
    totals.canonical += expected.len();
    let mut by_id: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for event in &expected {
        by_id.entry(event["eventId"].as_str().unwrap_or_default().to_owned()).or_default().push(event);
    }
    if produced.len() != expected.len() {
        totals.mismatches.push(format!(
            "{}: {} events produced, {} recorded",
            files[0].display(),
            produced.len(),
            expected.len()
        ));
    }
    for (index, actual) in produced.iter().enumerate() {
        let id = actual["eventId"].as_str().unwrap_or_default().to_owned();
        if skipped_ids.contains(&id) || expected.get(index).is_some_and(|event| truncated(event)) {
            totals.skipped += 1;
            continue;
        }
        let Some(recorded) = expected.get(index) else {
            totals.mismatches.push(format!("{id}: extra {} event", actual["type"]));
            continue;
        };
        let mut problems = Vec::new();
        diff("", actual, recorded, &mut problems);
        match to_runtime_event(actual.clone()) {
            Ok(typed) => {
                let reencoded = serde_json::to_value(&typed).unwrap();
                let mut without_instance = actual.clone();
                if reencoded.get("providerInstanceId").is_none() {
                    without_instance.as_object_mut().unwrap().remove("providerInstanceId");
                }
                diff("<typed>", &reencoded, &without_instance, &mut problems);
            }
            Err(error) => problems.push(format!("does not decode as ProviderRuntimeEvent: {error}")),
        }
        if problems.is_empty() {
            totals.matched += 1;
        } else {
            totals.mismatches.push(format!("{id} ({}): {}", actual["type"], problems.join("; ")));
        }
    }
}

fn thread_files(directory: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut threads: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("events.") else { continue };
        let thread = rest.split(".log").next().unwrap_or_default().to_owned();
        threads.entry(thread).or_default().push(path);
    }
    for files in threads.values_mut() {
        // Rotated files are older: `.log.2`, `.log.1`, then `.log`.
        files.sort_by_key(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            std::cmp::Reverse(name.rsplit('.').next().and_then(|suffix| suffix.parse::<u32>().ok()).unwrap_or(0))
        });
    }
    threads
}

#[test]
fn recorded_codex_threads_map_to_the_recorded_canonical_events() {
    let Some(directory) = std::env::var_os("ZC_CODEX_RECORDED_LOGS") else {
        eprintln!("ZC_CODEX_RECORDED_LOGS is not set: skipping the recorded-fixture gate");
        return;
    };
    let mut totals = Totals::default();
    for files in thread_files(Path::new(&directory)).values() {
        replay(files, &mut totals);
    }
    eprintln!(
        "codex threads: {}, native events: {}, canonical events: {}, matched: {}, skipped (truncated): {}, mismatches: {}",
        totals.threads,
        totals.native,
        totals.canonical,
        totals.matched,
        totals.skipped,
        totals.mismatches.len()
    );
    for mismatch in totals.mismatches.iter().take(40) {
        eprintln!("  {mismatch}");
    }
    assert!(totals.threads > 0, "no Codex thread in {}", Path::new(&directory).display());
    assert!(totals.mismatches.is_empty(), "{} mismatches", totals.mismatches.len());
}
