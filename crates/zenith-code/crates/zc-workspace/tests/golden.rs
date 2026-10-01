//! Golden comparison with the TypeScript search (`tests/golden/search-oracle.ts`): the same
//! scripted tree, the same requests, through `WorkspaceSearchIndex.ts` on `@ff-labs/fff-node`
//! 0.9.4 and through `zc_workspace::WorkspaceSearchIndex` on `fff-search` v0.9.4. Results must
//! be identical, order included (ranking, truncation flags, ranges, regex fallback).
//!
//! Needs `node` (≥ 22, type stripping) and `code/node_modules` with the server's dependencies
//! (symlink the main checkout's in a worktree). Without them the test prints why and passes;
//! set `ZC_REQUIRE_GOLDEN=1` to make that a failure instead.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::ProjectEntryKind;
use zc_workspace::entries::normalize_search_query;
use zc_workspace::{ContentSearch, FffFactory, IndexVariant, WorkspaceSearchIndex};

fn code_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code").canonicalize().unwrap()
}

fn oracle_available(code: &Path) -> Result<(), String> {
    let node = Command::new("node").arg("--version").output().map_err(|e| format!("node not found: {e}"))?;
    if !node.status.success() {
        return Err("node --version failed".into());
    }
    let fff = code.join("apps/server/node_modules/@ff-labs/fff-node/package.json");
    if !fff.exists() {
        return Err(format!("{} is missing (install or symlink code/node_modules)", fff.display()));
    }
    Ok(())
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Ada Example")
        .env("GIT_AUTHOR_EMAIL", "ada@example.test")
        .env("GIT_COMMITTER_NAME", "Ada Example")
        .env("GIT_COMMITTER_EMAIL", "ada@example.test")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

/// A made-up project: components, libraries, docs, images, generated modules with near-equal
/// names (ranking ties), ignored output, a dense file, unicode lines.
fn build_tree(root: &Path) {
    let files: &[(&str, &str)] = &[
        ("README.md", "# Orbit\nA search demo. TODO: write the guide.\n"),
        ("package.json", "{ \"name\": \"orbit\", \"version\": \"1.0.0\" }\n"),
        (".gitignore", "dist/\n*.log\ncoverage/\n"),
        (
            "src/components/Composer.tsx",
            "export function Composer() {\n  // search the composer\n  return null;\n}\n",
        ),
        ("src/components/ComposerToolbar.tsx", "export const ComposerToolbar = () => null; // TODO\n"),
        ("src/components/composePrompt.ts", "export const composePrompt = (search: string) => search;\n"),
        ("src/components/Button.tsx", "export const Button = () => 'Search';\n"),
        ("src/components/button.test.tsx", "test('button', () => {});\n"),
        (
            "src/lib/search.ts",
            "export function search(query: string) {\n  return query.trim();\n}\nexport const SEARCH_LIMIT = 200;\n",
        ),
        ("src/lib/searchRanking.ts", "// ranking for search results\nexport const rank = 1;\n"),
        ("src/lib/fuzzy.ts", "export const fuzzy = 'fuzzy search';\n"),
        ("src/server/routes.ts", "export const routes = ['router', 'route'];\n"),
        ("src/server/router.ts", "export class Router { route() {} }\n"),
        ("src/server/http.ts", "// TODO: http server\nexport const port = 3000;\n"),
        ("docs/composition.md", "Composition over inheritance.\nnote notes denote footnote note\n"),
        ("docs/guide/setup.md", "Setup: héllo wörld, déjà vu.\n"),
        ("docs/guide/search.md", "How search works.\nSearch is fuzzy.\n"),
        ("public/logo.svg", "<svg/>\n"),
        ("public/icon.png", "png\n"),
        ("public/banner.webp", "webp\n"),
        ("assets/Photo.JPG", "jpg\n"),
        ("scripts/build.sh", "#!/bin/sh\necho build\n"),
        ("tests/search.test.ts", "test('search', () => { expect(search('x')).toBe('x') });\n"),
        ("tests/router.test.ts", "test('router', () => {});\n"),
        ("packages/core/src/index.ts", "export * from './util';\n"),
        ("packages/core/src/util.ts", "export const util = () => 'needle';\n"),
        ("packages/ui/src/index.ts", "export * from './theme';\n"),
        ("packages/ui/src/theme.ts", "export const theme = { search: true };\n"),
        ("dist/bundle.js", "search search search\n"),
        ("debug.log", "search\n"),
    ];
    for (path, contents) in files {
        write(root, path, contents);
    }
    write(root, "src/lib/dense.ts", &"const needle = 1; // needle\n".repeat(150));
    for index in 0..60 {
        write(
            root,
            &format!("gen/module-{index}.ts"),
            &format!("export const module{index} = 'needle {index}';\n"),
        );
    }
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial"]);
    // Some modified and untracked files, so git status is part of the ranking input.
    write(
        root,
        "src/lib/search.ts",
        "export function search(query: string) {\n  return query.trim(); // changed\n}\n",
    );
    write(root, "src/lib/untracked.ts", "export const untracked = 'search';\n");
}

fn path_queries() -> Vec<Value> {
    let mut queries = Vec::new();
    for (query, limit) in [
        ("", 200),
        ("", 10),
        ("compo", 5),
        ("compo", 20),
        ("cmp", 10),
        ("cmp", 1),
        ("Composer.tsx", 5),
        ("src/comp", 10),
        ("  @./search ", 20),
        ("srch", 20),
        ("rout", 10),
        ("index", 10),
        ("readme", 5),
        ("ui theme", 10),
        ("module-1", 20),
        ("module", 200),
        ("compoesr", 10),
        ("xyz-nothing", 10),
        ("test", 20),
        ("guide", 10),
    ] {
        queries.push(json!({ "query": query, "limit": limit }));
        queries.push(json!({ "query": query, "limit": limit, "kind": "file" }));
        queries.push(json!({ "query": query, "limit": limit, "kind": "directory" }));
    }
    for (query, limit) in [("", 10), ("logo", 10), ("p", 2), ("photo", 5)] {
        queries.push(json!({ "query": query, "limit": limit, "imageOnly": true }));
    }
    queries
}

fn content_queries() -> Vec<Value> {
    let query = |query: &str, limit: u32, case_sensitive: bool, whole_word: bool, use_regex: bool| json!({ "query": query, "limit": limit, "caseSensitive": case_sensitive, "wholeWord": whole_word, "useRegex": use_regex });
    vec![
        query("search", 100, false, false, false),
        query("Search", 100, true, false, false),
        query("search", 5, false, false, false),
        query("search", 100, false, true, false),
        query("needle", 50, true, false, false),
        query("needle", 500, true, false, false),
        query("rout(e|er)", 100, false, false, true),
        query("ROUTE", 100, false, false, true),
        query("TODO", 100, true, true, false),
        query("note", 100, true, true, false),
        query("wörld", 100, true, false, false),
        query("déjà", 100, false, false, false),
        query("foo)bar(", 100, false, false, true),
        query(" search", 100, false, false, false),
        query("export const", 20, true, false, false),
        query("nothing-matches-this", 100, false, false, false),
    ]
}

async fn rust_results(cwd: &str, request: &Value) -> Value {
    let factory = Arc::new(FffFactory);
    let paths = WorkspaceSearchIndex::make(factory.clone(), cwd, IndexVariant::Paths).await.unwrap();
    let content = WorkspaceSearchIndex::make(factory, cwd, IndexVariant::Content).await.unwrap();
    let list = paths.list().await.unwrap();
    let mut search = Vec::new();
    for query in request["pathQueries"].as_array().unwrap() {
        let kind = match query.get("kind").and_then(Value::as_str) {
            Some("file") => Some(ProjectEntryKind::File),
            Some("directory") => Some(ProjectEntryKind::Directory),
            _ => None,
        };
        let normalized = normalize_search_query(query["query"].as_str().unwrap());
        let image_only = query.get("imageOnly").and_then(Value::as_bool).unwrap_or(false);
        let result = paths
            .search(&normalized, query["limit"].as_u64().unwrap() as usize, kind, image_only)
            .await
            .unwrap();
        search.push(serde_json::to_value(result).unwrap());
    }
    let mut contents = Vec::new();
    for query in request["contentQueries"].as_array().unwrap() {
        let input = ContentSearch {
            query: query["query"].as_str().unwrap().to_owned(),
            limit: query["limit"].as_u64().unwrap() as usize,
            case_sensitive: query["caseSensitive"].as_bool().unwrap(),
            whole_word: query["wholeWord"].as_bool().unwrap(),
            use_regex: query["useRegex"].as_bool().unwrap(),
        };
        contents.push(serde_json::to_value(content.search_contents(&input).await.unwrap()).unwrap());
    }
    json!({ "list": list, "search": search, "contents": contents })
}

fn first_difference(path: &str, left: &Value, right: &Value) -> Option<String> {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for key in l.keys().chain(r.keys()) {
                let (a, b) = (l.get(key).unwrap_or(&Value::Null), r.get(key).unwrap_or(&Value::Null));
                if let Some(diff) = first_difference(&format!("{path}.{key}"), a, b) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Array(l), Value::Array(r)) => {
            for index in 0..l.len().max(r.len()) {
                let (a, b) = (l.get(index).unwrap_or(&Value::Null), r.get(index).unwrap_or(&Value::Null));
                if let Some(diff) = first_difference(&format!("{path}[{index}]"), a, b) {
                    return Some(diff);
                }
            }
            None
        }
        _ if left == right => None,
        _ => Some(format!("{path}: rust {left} vs ts {right}")),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_results_match_the_typescript_server() {
    let code = code_dir();
    if let Err(reason) = oracle_available(&code) {
        assert!(std::env::var("ZC_REQUIRE_GOLDEN").is_err(), "golden oracle unavailable: {reason}");
        eprintln!("skipping the golden comparison: {reason}");
        return;
    }
    let dir = tempfile::Builder::new().prefix("zc-ws-golden-").tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    build_tree(&root);
    let cwd = root.to_string_lossy().into_owned();
    let request = json!({ "cwd": cwd, "pathQueries": path_queries(), "contentQueries": content_queries() });
    let request_path = dir.path().with_extension("request.json");
    std::fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();

    let oracle = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/search-oracle.ts");
    let output = Command::new("node").arg(&oracle).arg(&code).arg(&request_path).output().unwrap();
    let _ = std::fs::remove_file(&request_path);
    assert!(output.status.success(), "the TS oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();

    let actual = rust_results(&cwd, &request).await;
    if let Some(diff) = first_difference("$", &actual, &expected) {
        let dump = dir.path().with_extension("diff.json");
        std::fs::write(&dump, serde_json::to_vec_pretty(&json!({ "rust": actual, "ts": expected })).unwrap()).unwrap();
        panic!("Rust and TS search results differ at {diff} (both dumped to {})", dump.display());
    }
    // Sanity: the comparison covered real results.
    assert!(expected["list"]["entries"].as_array().unwrap().len() > 60);
    assert!(expected["search"].as_array().unwrap().iter().any(|r| r["truncated"] == json!(true)));
    assert!(expected["contents"].as_array().unwrap().iter().any(|r| r.get("regexFallbackError").is_some()));
    eprintln!(
        "golden: {} path queries, {} content queries, {} listed entries identical",
        request["pathQueries"].as_array().unwrap().len(),
        request["contentQueries"].as_array().unwrap().len(),
        expected["list"]["entries"].as_array().unwrap().len()
    );
}
