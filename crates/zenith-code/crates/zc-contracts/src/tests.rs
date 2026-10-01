//! Round-trip tests against fixtures encoded by the TypeScript schemas (the oracle).
//!
//! - `fixtures/samples.jsonl`: values sampled from every schema with Effect's `Arbitrary` and
//!   encoded with `Schema.toCodecJson` (fixed seeds, regenerated with the code).
//! - `fixtures/curated.jsonl`: hand-written edge cases, decoded and re-encoded by TS.
//! - `fixtures/harvested/*.jsonl`: real payloads from a copy of the live database (git-ignored,
//!   produced by `gen-rust-contracts.ts --harvest <db copy>`); skipped when absent.
//!
//! Each fixture is decoded into its Rust type and encoded back; the result must equal the
//! fixture after normalization: object key order is ignored and numbers compare by value
//! (`1` and `1.0` are the same JavaScript number).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::generated::registry;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

struct Fixture {
    origin: String,
    schema: String,
    value: Value,
}

fn read_jsonl(path: &Path) -> Vec<Fixture> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, line)| {
            let mut v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), i + 1));
            Fixture {
                origin: format!("{}:{}", path.file_name().unwrap().to_string_lossy(), i + 1),
                schema: v["schema"].as_str().expect("schema id").to_owned(),
                value: v["value"].take(),
            }
        })
        .collect()
}

/// Numbers compare by value; object key order is ignored (`serde_json`'s map equality already
/// ignores order).
fn same(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(l), Value::Number(r)) => l.as_f64() == r.as_f64(),
        (Value::Array(l), Value::Array(r)) => l.len() == r.len() && l.iter().zip(r).all(|(a, b)| same(a, b)),
        (Value::Object(l), Value::Object(r)) => l.len() == r.len() && l.iter().all(|(key, value)| r.get(key).is_some_and(|other| same(value, other))),
        _ => left == right,
    }
}

fn first_difference(before: &Value, after: &Value, path: &str) -> String {
    match (before, after) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in old {
                match new.get(key) {
                    None => return format!("{path}.{key}: missing after round-trip"),
                    Some(other) if !same(value, other) => {
                        return first_difference(value, other, &format!("{path}.{key}"));
                    }
                    _ => {}
                }
            }
            for key in new.keys() {
                if !old.contains_key(key) {
                    return format!("{path}.{key}: added by round-trip ({})", new[key]);
                }
            }
            format!("{path}: objects differ")
        }
        (Value::Array(old), Value::Array(new)) if old.len() == new.len() => {
            for (index, (value, other)) in old.iter().zip(new).enumerate() {
                if !same(value, other) {
                    return first_difference(value, other, &format!("{path}[{index}]"));
                }
            }
            format!("{path}: arrays differ")
        }
        _ => format!("{path}: {before} became {after}"),
    }
}

fn check_roundtrip(fixtures: &[Fixture]) -> Vec<String> {
    let mut failures = Vec::new();
    for f in fixtures {
        match registry::roundtrip(&f.schema, f.value.clone()) {
            None => failures.push(format!("{} [{}]: unknown schema id", f.origin, f.schema)),
            Some(Err(e)) => failures.push(format!("{} [{}]: {e}", f.origin, f.schema)),
            Some(Ok(out)) if !same(&f.value, &out) => {
                failures.push(format!("{} [{}]: {}", f.origin, f.schema, first_difference(&f.value, &out, "$")));
            }
            Some(Ok(_)) => {}
        }
    }
    failures
}

fn assert_no_failures(what: &str, total: usize, failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{what}: {} of {total} fixtures failed:\n{}",
        failures.len(),
        failures.iter().take(40).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn sampled_fixtures_roundtrip() {
    let fixtures = read_jsonl(&fixtures_dir().join("samples.jsonl"));
    assert!(fixtures.len() > 3000, "expected the sampled fixtures, got {}", fixtures.len());
    assert_no_failures("samples.jsonl", fixtures.len(), &check_roundtrip(&fixtures));
}

#[test]
fn curated_fixtures_roundtrip() {
    let fixtures = read_jsonl(&fixtures_dir().join("curated.jsonl"));
    assert_no_failures("curated.jsonl", fixtures.len(), &check_roundtrip(&fixtures));
}

#[test]
fn harvested_fixtures_roundtrip() {
    let dir = fixtures_dir().join("harvested");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no harvested fixtures in {} (run gen-rust-contracts.ts --harvest)", dir.display());
        return;
    };
    let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths.iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")) {
        let fixtures = read_jsonl(path);
        assert_no_failures(&path.display().to_string(), fixtures.len(), &check_roundtrip(&fixtures));
    }
}

#[test]
fn every_schema_id_is_covered_by_a_fixture() {
    let mut covered = std::collections::BTreeSet::new();
    for file in ["samples.jsonl", "curated.jsonl"] {
        for f in read_jsonl(&fixtures_dir().join(file)) {
            covered.insert(f.schema);
        }
    }
    // Schemas without values (`Schema.Never` errors of methods that cannot fail) need no fixture.
    let missing: Vec<_> = registry::IDS
        .iter()
        .filter(|id| !covered.contains(**id))
        .filter(|id| !registry::NO_VALUE_IDS.contains(id))
        .collect();
    assert!(missing.is_empty(), "schema ids without a fixture: {missing:?}");
}

#[test]
fn every_rpc_and_endpoint_is_covered() {
    let mut covered = std::collections::BTreeSet::new();
    for file in ["samples.jsonl", "curated.jsonl"] {
        for f in read_jsonl(&fixtures_dir().join(file)) {
            covered.insert(f.schema);
        }
    }
    for spec in &crate::METHODS {
        for part in ["payload", "success", "error"] {
            let id = format!("rpc:{}:{part}", spec.tag);
            if part == "error" && spec.error == "Never" {
                continue;
            }
            assert!(covered.contains(&id), "no fixture for {id}");
        }
    }
    for spec in &crate::ENDPOINTS {
        let base = format!("http:{}.{}", spec.group, spec.name);
        assert!(covered.contains(&format!("{base}:success:0")), "no fixture for {base}:success:0");
        if spec.payload.is_some() {
            assert!(covered.contains(&format!("{base}:payload")), "no fixture for {base}:payload");
        }
        if !spec.errors.is_empty() {
            assert!(covered.contains(&format!("{base}:error")), "no fixture for {base}:error");
        }
    }
}

#[test]
fn decoding_defaults_are_valid() {
    registry::check_defaults();
}

#[test]
fn wire_rules() {
    use serde_json::json;
    let enc = |v: &dyn erased::Encode| v.to_json();

    // withDecodingDefault: missing and null keys decode to the default, which is always written
    let from_empty: crate::ServerSettings = serde_json::from_value(json!({})).unwrap();
    let from_null: crate::ServerSettings = serde_json::from_value(json!({"enableProviderUpdateChecks": null})).unwrap();
    assert_eq!(from_empty, from_null);
    assert_eq!(enc(&from_empty)["enableProviderUpdateChecks"], json!(true));

    // optionalKey(NullOr(X)): absent, null and a value are three different patches
    let absent: crate::ServerSettingsPatch = serde_json::from_value(json!({})).unwrap();
    let null: crate::ServerSettingsPatch = serde_json::from_value(json!({"worktreeCleanup": null})).unwrap();
    assert_eq!(absent.worktree_cleanup, None);
    assert_eq!(null.worktree_cleanup, Some(None));
    assert_eq!(enc(&absent), json!({}));
    assert_eq!(enc(&null), json!({"worktreeCleanup": null}));

    // NullOr(X): the key is required
    assert!(serde_json::from_value::<crate::OrchestrationEvent>(json!({"type": "thread.deleted"})).is_err());

    // tagged unions report the unknown tag
    let err = serde_json::from_value::<crate::OrchestrationShellStreamItem>(json!({"kind": "bogus"})).unwrap_err();
    assert!(err.to_string().contains("bogus"), "{err}");

    // single-literal fields only accept their literal
    assert!(serde_json::from_value::<crate::LitSynchronized>(json!("synchronized")).is_ok());
    assert!(serde_json::from_value::<crate::LitSynchronized>(json!("snapshot")).is_err());
}

/// Serialization through `&dyn`, so `wire_rules` can encode any type with one closure.
mod erased {
    pub trait Encode {
        fn to_json(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Encode for T {
        fn to_json(&self) -> serde_json::Value {
            serde_json::to_value(self).expect("encodes")
        }
    }
}

#[test]
fn rpc_table_is_consistent() {
    use crate::RpcMethod;
    macro_rules! count {
        ($(($t:ident, $tag:literal, $kind:ident)),* $(,)?) => {
            [$($tag),*].len()
        };
    }
    assert_eq!(crate::METHODS.len(), 148);
    assert_eq!(crate::METHODS.iter().filter(|m| m.kind == crate::RpcKind::Stream).count(), 25);
    for (i, spec) in crate::METHODS.iter().enumerate() {
        assert_eq!(spec.rpc as usize, i);
        assert_eq!(crate::Rpc::from_tag(spec.tag), Some(spec.rpc));
        assert_eq!(spec.rpc.tag(), spec.tag);
    }
    assert_eq!(crate::zc_for_each_rpc!(count), 148);
    assert_eq!(crate::methods::OrchestrationSubscribeShell::TAG, "orchestration.subscribeShell");
    assert_eq!(crate::methods::OrchestrationSubscribeShell::KIND, crate::RpcKind::Stream);
    assert_eq!(crate::ENDPOINTS.len(), 24);
}

/// Pipes every fixture, decoded and re-encoded by Rust, through the TypeScript schemas
/// (`code/scripts/contracts-oracle.ts`). Needs Node and the contracts' `node_modules`:
///
/// ```sh
/// cargo test -p zc-contracts -- --ignored oracle
/// ```
#[test]
#[ignore = "needs node and code/node_modules"]
fn oracle_accepts_rust_encoded_values() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let script = repo.join("code/scripts/contracts-oracle.ts");
    let mut fixtures = Vec::new();
    for file in ["samples.jsonl", "curated.jsonl"] {
        fixtures.extend(read_jsonl(&fixtures_dir().join(file)));
    }
    if let Ok(entries) = std::fs::read_dir(fixtures_dir().join("harvested")) {
        for e in entries.filter_map(Result::ok) {
            fixtures.extend(read_jsonl(&e.path()));
        }
    }
    let mut lines = Vec::new();
    for f in &fixtures {
        let encoded = registry::roundtrip(&f.schema, f.value.clone())
            .expect("known id")
            .unwrap_or_else(|e| panic!("{}: {e}", f.origin));
        lines.push(serde_json::json!({ "schema": f.schema, "value": encoded }).to_string());
    }
    let mut child = Command::new("node")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn node");
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    });
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut failures = Vec::new();
    let mut count = 0;
    for (i, line) in stdout.lines().enumerate() {
        let line = line.unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        count += 1;
        if v["ok"] != Value::Bool(true) {
            failures.push(format!("{} [{}]: {}", fixtures[i].origin, fixtures[i].schema, v["issue"]));
        }
    }
    writer.join().unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(count, fixtures.len());
    assert_no_failures("oracle", fixtures.len(), &failures);
}
