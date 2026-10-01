//! Byte-exact checks against fixtures generated from the TypeScript originals
//! (`fixtures/gen-fixtures.mjs`): Node's UTF-8 `StringDecoder`, `sanitizeTerminalHistoryChunk`
//! and `BoundedTerminalHistory`.

use serde_json::Value;
use zc_terminal::decoder::Utf8StreamDecoder;
use zc_terminal::history::{BoundedTerminalHistory, DEFAULT_HISTORY_BYTE_LIMIT};
use zc_terminal::sanitizer::sanitize_terminal_history_chunk;

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn cases(value: &Value) -> &Vec<Value> {
    value["cases"].as_array().unwrap()
}

#[test]
fn decoder_matches_node_string_decoder() {
    let fixture = fixture("decoder.json");
    let mut checked = 0;
    for case in cases(&fixture) {
        let mut decoder = Utf8StreamDecoder::new();
        let chunks = case["chunks"].as_array().unwrap();
        let outputs = case["outputs"].as_array().unwrap();
        for (chunk, expected) in chunks.iter().zip(outputs) {
            let got = decoder.write(&hex(chunk.as_str().unwrap()));
            assert_eq!(got, expected.as_str().unwrap(), "{}: chunk {chunk}", case["name"]);
            checked += 1;
        }
    }
    assert!(checked > 500, "only {checked} chunks checked");
}

#[test]
fn sanitizer_matches_typescript() {
    let fixture = fixture("sanitizer.json");
    let mut checked = 0;
    for case in cases(&fixture) {
        let mut decoder = Utf8StreamDecoder::new();
        let mut pending = String::new();
        for (chunk, step) in case["chunks"].as_array().unwrap().iter().zip(case["steps"].as_array().unwrap()) {
            let data = decoder.write(&hex(chunk.as_str().unwrap()));
            assert_eq!(data, step["data"].as_str().unwrap(), "{}: decoded", case["name"]);
            if data.is_empty() {
                continue;
            }
            let result = sanitize_terminal_history_chunk(&pending, &data);
            assert_eq!(
                result.visible_text,
                step["visible"].as_str().unwrap(),
                "{}: visible text of chunk {chunk}",
                case["name"]
            );
            assert_eq!(
                result.pending_control_sequence,
                step["pending"].as_str().unwrap(),
                "{}: pending of chunk {chunk}",
                case["name"]
            );
            pending = result.pending_control_sequence;
            checked += 1;
        }
    }
    assert!(checked > 3_000, "only {checked} chunks checked");
}

fn fnv1a64(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn check_value(case: &str, step: usize, got: &str, expected: &Value) {
    if let Some(value) = expected.get("value") {
        assert_eq!(got, value.as_str().unwrap(), "{case}: step {step}");
    } else {
        assert_eq!(got.len() as u64, expected["bytes"].as_u64().unwrap(), "{case}: step {step} byte length");
        assert_eq!(fnv1a64(got), expected["fnv1a64"].as_str().unwrap(), "{case}: step {step} hash");
    }
}

#[test]
fn history_matches_typescript() {
    let fixture = fixture("history.json");
    for case in cases(&fixture) {
        let name = case["name"].as_str().unwrap();
        let max_lines = case["maxLines"].as_u64().unwrap() as usize;
        let max_bytes = case["maxBytes"].as_u64().map_or(DEFAULT_HISTORY_BYTE_LIMIT, |bytes| bytes as usize);
        let mut history = BoundedTerminalHistory::new(max_lines, case["initial"].as_str().unwrap(), max_bytes);
        let expected = case["expected"].as_array().unwrap();
        check_value(name, 0, history.value(), &expected[0]);
        for (index, op) in case["ops"].as_array().unwrap().iter().enumerate() {
            if op.get("clear").is_some() {
                history.clear();
            } else if let Some(parts) = op.get("append") {
                let text: String = parts
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|part| part[0].as_str().unwrap().repeat(part[1].as_u64().unwrap() as usize))
                    .collect();
                history.append(&text);
            } else {
                let batch = op["batchLines"][0].as_u64().unwrap();
                let count = op["batchLines"][1].as_u64().unwrap();
                let text: String = (0..count).map(|line| format!("{batch}:{line}\n")).collect();
                history.append(&text);
            }
            check_value(name, index + 1, history.value(), &expected[index + 1]);
        }
    }
}
