//! Prints the transcript files under a root and the records each parses to, in the format of
//! a TS debug script, for diffing the two readers.
//!
//!   cargo run -p zc-usage --example list_records -- <root> <claude|codex|grok> <since-ms>

use std::path::Path;

use zc_contracts::UsageProviderKind;
use zc_usage::reader::{list_transcript_files, read_transcript_records};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let provider = match args[2].as_str() {
        "codex" => UsageProviderKind::Codex,
        "grok" => UsageProviderKind::Grok,
        _ => UsageProviderKind::Claude,
    };
    let since: f64 = args[3].parse().unwrap();
    for file in list_transcript_files(Path::new(&args[1]), since, None) {
        let parsed = read_transcript_records(&file.path, provider, None);
        println!(
            "{} {} {} {} {}",
            file.path.display(),
            file.size,
            zc_providers::js_json::format_js_number(file.mtime_ms),
            parsed.as_ref().map_or("undefined".to_owned(), |parsed| parsed.records.len().to_string()),
            parsed.as_ref().map_or("undefined".to_owned(), |parsed| parsed.tail_records.len().to_string()),
        );
        for record in parsed.map(|parsed| parsed.records).unwrap_or_default() {
            println!(
                "  {} {} {} {} {} {}",
                zc_providers::js_json::format_js_number(record.timestamp_ms),
                record.model,
                record.dedupe_key.as_deref().unwrap_or("null"),
                record.totals.stringify(),
                record.fast,
                record.reported_cost_usd.map_or("null".to_owned(), zc_providers::js_json::format_js_number)
            );
        }
    }
}
