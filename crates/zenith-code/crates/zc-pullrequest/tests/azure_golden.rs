//! Golden comparison against the TypeScript Azure DevOps pull request code: the same fake `az`
//! (recorded outputs keyed by argv) answers both sides, the TS provider runs over the real
//! `AzureDevOpsPullRequestCli` → `AzureDevOpsCli` → `VcsProcess` (`golden/azure_oracle.mjs`,
//! through node and the real effect/contracts/diff packages), and every result and error must
//! match as JSON, with the neutral Rust types mapped to the TS object shapes (camelCase, absent
//! vs `null`) by the serializers below. Both sides must also have run exactly the same `az`
//! command lines.
//!
//! Beside the provider methods, the JSON decoders run over a corpus of payloads, and the diff
//! synthesis over a corpus of file pairs (insertions, deletions, CRLF, missing trailing
//! newlines, repeated lines, large files, empty files, pairs past the edit ceiling): every
//! section must be byte for byte what `azureDevOpsFilePatch` writes, and every hunk what jsdiff's
//! `structuredPatch` returns.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Error causes are compared by name and
//! message at the first level and by name below it.

#![allow(clippy::result_large_err)]

mod support_azure;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use support_azure::FakeAz;
use zc_contracts::{PullRequestDiffFileContentsInputChangeType, PullRequestReviewVerdict};
use zc_pullrequest::azure::diff::{
    azure_devops_file_patch, azure_devops_unreadable_file_patch, parse_azure_devops_diff_cursor, structured_patch, AzureDevOpsFileTexts, PatchOptions,
};
use zc_pullrequest::azure::json::*;
use zc_pullrequest::azure::util::locale_compare;
use zc_pullrequest::azure::AzureDevOpsPullRequestProvider;
use zc_pullrequest::provider::*;
use zc_sourcecontrol::azure::AzureDevOpsCli;

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    let server = server_dir();
    if !server.join("node_modules/effect").exists() || !server.join("node_modules/diff").exists() {
        return Err(format!("{} has no node_modules", server.display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(cases: &[Value], bin: &Path, home: &Path) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/azure_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = json!({ "cases": cases }).to_string();
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        stdin.write_all(input.as_bytes()).unwrap();
    });
    let output = child.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Keeps the first-level cause's name and message; deeper causes by name (a failed decode keeps
/// an Effect `Cause` there in TS, a `SchemaError` in Rust).
fn normalize(value: &mut Value, depth: usize) {
    if let Value::Object(map) = value {
        if let Some(cause) = map.get_mut("cause") {
            if depth >= 1 {
                let name = if cause.get("_id").and_then(Value::as_str) == Some("Cause") {
                    json!("SchemaError")
                } else {
                    cause.get("name").cloned().unwrap_or(Value::Null)
                };
                *cause = json!({ "name": name });
            } else {
                normalize(cause, depth + 1);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The fake `az`.
// ---------------------------------------------------------------------------------------------

const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const HEAD1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD2: &str = "cccccccccccccccccccccccccccccccccccccccc";
const JSON_FLAGS: &str = "--only-show-errors --output json";

fn show_args(number: i64) -> String {
    format!("repos pr show --detect true --id {number} {JSON_FLAGS}")
}

fn invoke_args(resource: &str, route: &[String], query: Option<&[String]>) -> String {
    let mut args = format!(
        "devops invoke --detect true --area git --resource {resource} --api-version 7.1 --route-parameters {}",
        route.join(" ")
    );
    if let Some(query) = query {
        args.push_str(" --query-parameters ");
        args.push_str(&query.join(" "));
    }
    format!("{args} {JSON_FLAGS}")
}

fn pr_route(number: i64) -> Vec<String> {
    vec!["project=platform".into(), "repositoryId=web".into(), format!("pullRequestId={number}")]
}

fn threads_args(number: i64) -> String {
    invoke_args("pullRequestThreads", &pr_route(number), None)
}

fn iterations_args(number: i64) -> String {
    invoke_args("pullRequestIterations", &pr_route(number), None)
}

fn changes_args(number: i64, iteration: i64, skip: i64) -> String {
    let mut route = pr_route(number);
    route.push(format!("iterationId={iteration}"));
    invoke_args("pullRequestIterationChanges", &route, Some(&["$top=2000".into(), format!("$skip={skip}")]))
}

fn item_args(path: &str, commit: &str) -> String {
    invoke_args(
        "items",
        &["project=platform".into(), "repositoryId=web".into()],
        Some(&[
            format!("path=/{path}"),
            "versionDescriptor.versionType=commit".into(),
            format!("versionDescriptor.version={commit}"),
            "includeContent=true".into(),
            "includeContentMetadata=true".into(),
            "$format=json".into(),
        ]),
    )
}

fn list_args(rest: &str) -> String {
    format!("repos pr list --detect true --repository web {rest} {JSON_FLAGS}")
}

const REST_URL: &str = "https://dev.azure.com/acme/_apis/git/repositories/web/pullRequests";

fn pr_json(number: i64, extra: Value) -> Value {
    let mut row = json!({
        "pullRequestId": number,
        "title": format!("Pull request {number}"),
        "status": "active",
        "sourceRefName": "refs/heads/feat/page",
        "targetRefName": "refs/heads/main",
        "creationDate": "2026-07-01T00:00:00Z",
        "url": format!("{REST_URL}/{number}"),
        "repository": {"name": "web", "project": {"name": "platform"}},
    });
    for (key, value) in extra.as_object().unwrap() {
        row[key] = value.clone();
    }
    row
}

fn content(text: &str) -> String {
    json!({ "content": text }).to_string()
}

const README_OLD: &str =
    "# Demo\n\nOne line.\nTwo lines.\nThree lines.\nFour lines.\nFive lines.\nSix lines.\nSeven lines.\nEight lines.\nNine lines.\nTen lines.\n";
const README_NEW: &str = "# Demo\n\nOne line.\nTwo lines, changed.\nThree lines.\nFour lines.\nFive lines.\nSix lines.\nSeven lines.\nEight lines.\nNine lines.\nTen lines.\nEleven lines.\n";

/// The files of PR 42's latest iteration, in Azure's order: `(changeType, path, sourceServerItem,
/// objectId)`.
fn pr42_changes() -> Value {
    json!({"changeEntries": [
        {"changeType": "edit", "item": {"path": "/README.md", "objectId": "8f80", "originalObjectId": "0ca4"}},
        {"changeType": "add", "item": {"path": "/DEMO.md", "objectId": "ec00"}},
        {"changeType": "delete", "item": {"path": "/OLD.md", "originalObjectId": "1111"}},
        {"changeType": "rename", "sourceServerItem": "/docs/old.md", "item": {"path": "/docs/new.md", "objectId": "aaaa", "originalObjectId": "aaaa"}},
        {"changeType": "edit, rename", "originalPath": "/src/old.ts", "item": {"path": "/src/new.ts", "objectId": "bbbb", "originalObjectId": "cccc"}},
        {"changeType": "add", "item": {"path": "/docs", "isFolder": true, "gitObjectType": "tree"}},
        {"changeType": "edit", "item": {"path": "/logo.png", "objectId": "dddd"}},
        {"changeType": "edit", "item": {"path": "/broken.txt", "objectId": "eeee"}},
        {"changeType": "edit", "item": {"path": "/garbled.txt", "objectId": "ffff"}},
        {"changeType": "edit", "item": {"path": "/crlf.txt", "objectId": "1212"}},
        {"changeType": "edit", "item": {"path": "/nonl.txt", "objectId": "3434"}},
        {"changeType": "edit", "item": {"path": "/notes\ttab.md", "objectId": "5656"}},
        {"changeType": "edit", "item": {"path": "/émoji 😀.md", "objectId": "7878"}},
    ]})
}

fn record_fake_outputs(az: &FakeAz) {
    az.respond(
        &format!("account show --query user {JSON_FLAGS}"),
        r#"{"name":" sam@example.test ","type":"user"}"#,
        "",
        0,
    );

    // PR 42: open, located, conversation, two pushes, a change of every kind.
    az.respond(
        &show_args(42),
        &pr_json(
            42,
            json!({
                "title": "Add the change requests page",
                "description": "- ships the page\n- and its tests",
                "isDraft": false,
                "mergeStatus": "conflicts",
                "autoCompleteSetBy": {"displayName": "Sam Example"},
                "completionOptions": {"mergeStrategy": "squash"},
                "createdBy": {"displayName": "Sam Example", "uniqueName": "sam@example.test", "imageUrl": "https://avatars.example.test/sam"},
                "reviewers": [{"displayName": "Riley Example", "uniqueName": "riley@example.test", "vote": 10}, {"displayName": "  "}, {"displayName": "Team Only"}],
                "sourceRefName": " refs/heads/feat/page ",
                "repository": {"name": "web", "project": {"name": "platform"}, "webUrl": "https://dev.azure.com/acme/platform/_git/web/"},
            }),
        )
        .to_string(),
        "",
        0,
    );
    az.respond(
        &threads_args(42),
        &json!({"count": 4, "continuation_token": null, "value": [
            {"id": 7, "comments": [{"id": 1, "content": "Later remark.", "author": {"displayName": "Riley Example", "uniqueName": "riley@example.test"}, "publishedDate": "2026-07-04T00:00:00.000Z"}]},
            {"id": 3, "threadContext": {"filePath": "/src/new.ts"}, "comments": [
                {"id": 1, "content": "Rename this.", "publishedDate": "2026-07-02T00:00:00Z", "author": {"uniqueName": "sam@example.test"}},
                {"id": 2, "content": "Renamed.", "publishedDate": "2026-07-02T01:00:00Z", "isDeleted": false},
                {"id": 3, "content": "gone", "publishedDate": "2026-07-02T02:00:00Z", "isDeleted": true},
                {"content": "no id", "publishedDate": " 2026-07-02T03:00:00Z "},
                {"id": 5, "content": "no date"},
            ]},
            {"id": 1, "comments": [{"id": 1, "content": "Sam voted 10", "commentType": "system", "publishedDate": "2026-07-01T00:00:00Z"}]},
            {"id": 2, "isDeleted": true, "comments": [{"id": 1, "content": "deleted thread", "publishedDate": "2026-07-01T00:00:00Z"}]},
            {"id": "malformed"},
            {"id": 4, "comments": [{"id": "bad", "content": "malformed comment", "publishedDate": "2026-07-01T00:00:00Z"}]},
        ]})
        .to_string(),
        "",
        0,
    );
    az.respond(
        &iterations_args(42),
        &json!({"value": [
            {"id": 2, "sourceRefCommit": {"commitId": HEAD2}, "commonRefCommit": {"commitId": BASE}},
            {"id": 1, "sourceRefCommit": {"commitId": HEAD1}, "commonRefCommit": {"commitId": BASE}},
            {"id": 9, "sourceRefCommit": {"commitId": HEAD2}},
        ]})
        .to_string(),
        "",
        0,
    );
    az.respond(&changes_args(42, 2, 0), &pr42_changes().to_string(), "", 0);
    az.respond(
        &changes_args(42, 1, 0),
        &json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/README.md", "objectId": "1f1f"}}]}).to_string(),
        "",
        0,
    );
    for (path, commit, body) in [
        ("README.md", BASE, content(README_OLD)),
        ("README.md", HEAD2, content(README_NEW)),
        ("README.md", HEAD1, content("# Demo\n\nOne line.\n")),
        ("DEMO.md", HEAD2, content("hello\nworld\n")),
        ("OLD.md", BASE, content("gone\nfor good")),
        ("docs/old.md", BASE, content("same\n")),
        ("docs/new.md", HEAD2, content("same\n")),
        ("src/old.ts", BASE, content("export const a = 1;\nexport const b = 2;\n")),
        ("src/new.ts", HEAD2, content("export const a = 1;\nexport const b = 3;\n")),
        ("logo.png", BASE, content("b2xk")),
        ("logo.png", HEAD2, json!({"content": "bmV3", "contentMetadata": {"isBinary": true}}).to_string()),
        ("broken.txt", BASE, content("x\n")),
        ("garbled.txt", BASE, content("x\n")),
        ("garbled.txt", HEAD2, "not json".to_owned()),
        ("crlf.txt", BASE, content("one\r\ntwo\r\nthree\r\n")),
        ("crlf.txt", HEAD2, content("one\r\ntwo again\r\nthree\r\n")),
        ("nonl.txt", BASE, content("a\nb\nc")),
        ("nonl.txt", HEAD2, content("a\nb\nc\nd")),
        ("notes\ttab.md", BASE, content("tab\n")),
        ("notes\ttab.md", HEAD2, content("tabs\n")),
        ("émoji 😀.md", BASE, content("é\n")),
        ("émoji 😀.md", HEAD2, content("😀\n")),
    ] {
        az.respond(&item_args(path, commit), &body, "", 0);
    }
    az.respond(
        &item_args("broken.txt", HEAD2),
        "",
        "ERROR: TF401174: The item could not be found in the repository.",
        1,
    );

    // PR 43: completed, a web link, a draft flag and no auto-complete.
    az.respond(
        &show_args(43),
        &pr_json(
            43,
            json!({"status": "completed", "closedDate": "2026-07-05T00:00:00Z", "isDraft": true, "mergeStatus": "succeeded",
                   "_links": {"web": {"href": " https://dev.azure.com/acme/platform/_git/web/pullrequest/43 "}},
                   "completionOptions": {"squashMerge": true}}),
        )
        .to_string(),
        "",
        0,
    );
    az.respond(&iterations_args(43), r#"{"value": []}"#, "", 0);
    az.respond(&threads_args(43), r#"{"value": []}"#, "", 0);

    // PR 44: abandoned, placed by its web link only, so nothing can be read past it.
    az.respond(
        &show_args(44),
        &pr_json(
            44,
            json!({"status": "abandoned", "closedDate": "2026-07-06T00:00:00Z", "url": null, "repository": null,
                   "_links": {"web": {"href": "https://dev.azure.com/acme/platform/_git/web/pullrequest/44"}}}),
        )
        .to_string(),
        "",
        0,
    );
    az.respond(&show_args(45), "", "ERROR: The requested pull request 45 does not exist.", 1);
    az.respond(&show_args(46), "not json", "", 0);
    az.respond(
        &show_args(47),
        r#"{"pullRequestId": 47, "title": "Incomplete", "sourceRefName": "refs/heads/x", "targetRefName": "refs/heads/main", "creationDate": "2026-07-01T00:00:00Z"}"#,
        "",
        0,
    );
    az.respond(&show_args(48), "", "ERROR: Please run az login to setup account.", 1);
    az.respond(&show_args(49), "", "ERROR: HTTP 429 Too Many Requests", 1);

    // PR 50: the conversation route fails; PR 51: paged changes that stop moving; PR 52: a read
    // rate-limited mid-diff.
    for number in [50, 51, 52, 53] {
        az.respond(&show_args(number), &pr_json(number, json!({})).to_string(), "", 0);
        az.respond(
            &iterations_args(number),
            &json!({"value": [{"id": 1, "sourceRefCommit": {"commitId": HEAD1}, "commonRefCommit": {"commitId": BASE}}]}).to_string(),
            "",
            0,
        );
    }
    az.respond(&threads_args(50), "", "ERROR: something broke", 1);
    az.respond(
        &changes_args(50, 1, 0),
        &json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/c.ts", "objectId": "0101"}}]}).to_string(),
        "",
        0,
    );
    az.respond(&item_args("c.ts", BASE), &content("one\n"), "", 0);
    az.respond(&item_args("c.ts", HEAD1), &content("two\n"), "", 0);
    az.respond(
        &changes_args(51, 1, 0),
        &json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/a.ts", "objectId": "0101"}}], "nextSkip": 2000}).to_string(),
        "",
        0,
    );
    az.respond(
        &changes_args(51, 1, 2000),
        &json!({"changeEntries": [{"changeType": "edit", "item": {"path": "/b.ts", "objectId": "0202"}}], "nextSkip": 2000}).to_string(),
        "",
        0,
    );
    az.respond(
        &changes_args(52, 1, 0),
        &json!({"changeEntries": [{"changeType": "add", "item": {"path": "/a.ts", "objectId": "0101"}}]}).to_string(),
        "",
        0,
    );
    az.respond(&item_args("a.ts", HEAD1), "", "ERROR: HTTP 429 Too Many Requests", 1);
    // PR 53: eight heavy files, so the slice budgets, the batch narrowing and the cursor decide
    // which files each side reads.
    let entries: Vec<Value> = (0..8)
        .map(|file| json!({"changeType": "edit", "item": {"path": format!("/big/f{file}.ts"), "objectId": format!("{file}")}}))
        .collect();
    az.respond(&changes_args(53, 1, 0), &json!({ "changeEntries": entries }).to_string(), "", 0);
    let heavy = |prefix: &str| -> String { (0..50).map(|line| format!("{prefix} {line} {}\n", "z".repeat(450))).collect() };
    for file in 0..8 {
        az.respond(&item_args(&format!("big/f{file}.ts"), BASE), &content(&heavy("old")), "", 0);
        az.respond(&item_args(&format!("big/f{file}.ts"), HEAD1), &content(&heavy("new")), "", 0);
    }

    // Listings.
    let rows = |numbers: &[i64]| Value::Array(numbers.iter().map(|n| pr_json(*n, json!({}))).collect());
    az.respond(&list_args("--status active --include-links --top 11"), &rows(&[1, 2, 3]).to_string(), "", 0);
    az.respond(
        &list_args("--status completed --creator sam@example.test --include-links --top 3"),
        &json!([{"pullRequestId": "malformed"}, pr_json(1, json!({})), {"pullRequestId": "also malformed"}]).to_string(),
        "",
        0,
    );
    az.respond(
        &list_args("--status completed --creator sam@example.test --include-links --skip 3 --top 2"),
        &rows(&[2, 3]).to_string(),
        "",
        0,
    );
    az.respond(
        &list_args("--status abandoned --reviewer sam@example.test --include-links --skip 20 --top 6"),
        "",
        "",
        0,
    );
    az.respond(
        &list_args("--status all --include-links --top 2"),
        "",
        "ERROR: Please run az login to setup account.",
        1,
    );
    az.respond(&list_args("--status active --include-links --top 3"), &rows(&[1, 2, 3, 4]).to_string(), "", 0);

    // Writes.
    for write in [
        "repos pr update --detect true --id 42 --status completed --squash true",
        "repos pr update --detect true --id 42 --status completed --squash false",
        "repos pr update --detect true --id 42 --auto-complete true",
        "repos pr update --detect true --id 42 --auto-complete true --squash false",
        "repos pr update --detect true --id 42 --draft false",
        "repos pr update --detect true --id 42 --status abandoned",
        "repos pr update --detect true --id 42 --status active",
        "repos pr update --detect true --id 42 --title=New title --description=- first\n- second",
        "repos pr update --detect true --id 42 --description=Only the body",
        "repos pr reviewer add --detect true --id 42 --reviewers riley@example.test 6f9c9b7f-0000-0000-0000-000000000000",
        "repos pr reviewer remove --detect true --id 42 --reviewers riley@example.test",
    ] {
        az.respond(&format!("{write} {JSON_FLAGS}"), "{}", "", 0);
    }
}

// ---------------------------------------------------------------------------------------------
// The cases.
// ---------------------------------------------------------------------------------------------

struct Cases {
    cases: Vec<Value>,
}

impl Cases {
    fn provider(&mut self, method: &str, input: Value) {
        let id = format!("{method}#{}", self.cases.len());
        self.cases.push(json!({"id": id, "op": "provider", "method": method, "input": input}));
    }

    fn pure(&mut self, function: &str, args: Value) {
        let id = format!("{function}#{}", self.cases.len());
        self.cases.push(json!({"id": id, "op": "pure", "fn": function, "args": args}));
    }
}

fn reference(cwd: &str, number: i64, extra: Value) -> Value {
    let mut input = json!({"cwd": cwd, "repository": "platform/web", "host": "dev.azure.com", "number": number});
    for (key, value) in extra.as_object().unwrap() {
        input[key] = value.clone();
    }
    input
}

fn provider_cases(cases: &mut Cases, cwd: &str) {
    cases.provider("getViewer", json!({"cwd": cwd}));
    let list = |state: &str, involvement: &str, limit: i64, cursor: Option<i64>| {
        let mut input = json!({"cwd": cwd, "repository": "web", "host": "dev.azure.com", "state": state, "involvement": involvement, "viewer": "sam@example.test", "limit": limit, "query": "page"});
        if let Some(delivered) = cursor {
            input["cursor"] = json!({"updatedBefore": "2026-07-02T00:00:00Z", "delivered": delivered});
        }
        input
    };
    cases.provider("listChangeRequests", list("open", "all", 10, None));
    cases.provider("listChangeRequests", list("merged", "authored", 2, None));
    cases.provider("listChangeRequests", list("closed", "reviewing", 5, Some(20)));
    cases.provider("listChangeRequests", list("all", "all", 1, None));
    cases.provider("listChangeRequests", list("open", "all", 2, None));
    for number in [42, 43, 44, 45, 46, 47, 48, 49, 50] {
        cases.provider("getChangeRequest", reference(cwd, number, json!({})));
        cases.provider("getChangeRequestSummary", reference(cwd, number, json!({})));
        cases.provider("getChangeRequestActivity", reference(cwd, number, json!({})));
    }
    cases.provider("getViewerPermissions", reference(cwd, 42, json!({"includeUpdateBranch": true})));
    for cursor in [
        Value::Null,
        json!("2:3"),
        json!("2:12"),
        json!("2:99"),
        json!("1:0"),
        json!("9:0"),
        json!("garbage"),
    ] {
        let extra = if cursor.is_null() { json!({}) } else { json!({ "cursor": cursor }) };
        cases.provider("getDiff", reference(cwd, 42, extra));
    }
    for number in [43, 44, 45, 50, 52, 53] {
        cases.provider("getDiff", reference(cwd, number, json!({})));
    }
    cases.provider("getDiff", reference(cwd, 53, json!({"cursor": "1:6"})));
    cases.provider("getDiff", reference(cwd, 53, json!({"cursor": "1:2"})));
    for (change_type, old_path, new_path) in [
        ("change", "README.md", "README.md"),
        ("new", "DEMO.md", "DEMO.md"),
        ("deleted", "OLD.md", "OLD.md"),
        ("rename-changed", "src/old.ts", "src/new.ts"),
        ("change", "logo.png", "logo.png"),
        ("change", "broken.txt", "broken.txt"),
    ] {
        cases.provider(
            "getDiffFileContents",
            reference(cwd, 42, json!({"changeType": change_type, "oldPath": old_path, "newPath": new_path})),
        );
    }
    cases.provider(
        "getDiffFileContents",
        reference(cwd, 44, json!({"changeType": "change", "oldPath": "a", "newPath": "a"})),
    );
    cases.provider(
        "getDiffFileContents",
        reference(cwd, 43, json!({"changeType": "change", "oldPath": "a", "newPath": "a"})),
    );
    cases.provider(
        "getFileRevisions",
        reference(cwd, 42, json!({"paths": ["README.md", "OLD.md", "gone.md", "docs", "DEMO.md"]})),
    );
    cases.provider("getFileRevisions", reference(cwd, 42, json!({"paths": []})));
    cases.provider("getFileRevisions", reference(cwd, 51, json!({"paths": ["a.ts", "b.ts", "c.ts"]})));
    cases.provider("getFileRevisions", reference(cwd, 44, json!({"paths": ["a.ts"]})));
    cases.provider("getFileRevisions", reference(cwd, 45, json!({"paths": ["a.ts"]})));

    // Writes.
    cases.provider("runAction", reference(cwd, 42, json!({"action": "merge", "mergeMethod": "squash"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "merge"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "enable-auto-merge"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "enable-auto-merge", "mergeMethod": "merge"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "ready"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "close"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "reopen"})));
    cases.provider("runAction", reference(cwd, 42, json!({"action": "draft"})));
    cases.provider(
        "updateChangeRequest",
        reference(cwd, 42, json!({"title": "New title", "body": "- first\n- second"})),
    );
    cases.provider("updateChangeRequest", reference(cwd, 42, json!({"body": "Only the body"})));
    cases.provider(
        "setReviewerRequest",
        reference(cwd, 42, json!({"reviewers": [{"id": "riley@example.test", "kind": "user"}, {"id": "6f9c9b7f-0000-0000-0000-000000000000", "kind": "user"}], "requested": true})),
    );
    cases.provider(
        "setReviewerRequest",
        reference(
            cwd,
            42,
            json!({"reviewers": [{"id": "riley@example.test", "kind": "user"}], "requested": false}),
        ),
    );
    cases.provider(
        "setReviewerRequest",
        reference(cwd, 42, json!({"reviewers": [{"id": " --query", "kind": "user"}], "requested": true})),
    );
    cases.provider("listReviewerCandidates", reference(cwd, 42, json!({})));
    cases.provider("comment", reference(cwd, 42, json!({"body": "hello"})));
    cases.provider("submitReview", reference(cwd, 42, json!({"verdict": "approve", "body": "", "comments": []})));
    cases.provider("replyToThread", reference(cwd, 42, json!({"threadId": "3", "body": "ok"})));
    cases.provider("setReaction", reference(cwd, 42, json!({"content": "laugh", "reacted": true})));
    cases.provider("setThreadResolution", reference(cwd, 42, json!({"threadId": "3", "resolved": true})));
}

/// Payloads for the decoders, malformed ones included.
fn decode_cases(cases: &mut Cases) {
    let pr = |extra: Value| pr_json(42, extra);
    let rows = vec![
        pr(json!({})),
        pr(json!({"status": " COMPLETED ", "closedDate": " 2026-07-05T00:00:00Z "})),
        pr(json!({"status": null, "isDraft": null, "mergeStatus": "rejectedByPolicy"})),
        pr(json!({"mergeStatus": "failure", "description": null})),
        pr(json!({"autoCompleteSetBy": {}, "completionOptions": {"mergeStrategy": " rebaseMerge "}})),
        pr(json!({"autoCompleteSetBy": {"displayName": "x"}, "completionOptions": {"mergeStrategy": "noFastForward"}})),
        pr(json!({"autoCompleteSetBy": {"displayName": "x"}, "completionOptions": {"mergeStrategy": "unknown", "squashMerge": true}})),
        pr(json!({"autoCompleteSetBy": null, "completionOptions": {"mergeStrategy": "squash"}})),
        pr(json!({"autoCompleteSetBy": "nope"})),
        pr(json!({"createdBy": {"displayName": " Only Name "}})),
        pr(json!({"createdBy": {"displayName": " ", "uniqueName": ""}})),
        pr(json!({"createdBy": []})),
        pr(json!({"reviewers": [{"uniqueName": "a@example.test", "imageUrl": " "}, {"displayName": 3}]})),
        pr(json!({"reviewers": null})),
        pr(json!({"sourceRefName": "  "})),
        pr(json!({"sourceRefName": "refs/heads/"})),
        pr(json!({"sourceRefName": "refs/heads/refs/heads/x", "targetRefName": "main"})),
        pr(json!({"creationDate": 5})),
        pr(json!({"pullRequestId": 4.5})),
        pr(json!({"pullRequestId": -3})),
        pr(json!({"title": null})),
        pr(json!({"title": "  spaced  "})),
        pr(json!({"_links": {"web": {"href": null}}})),
        pr(json!({"_links": {"web": {}}, "url": "https://example.test/not-azure"})),
        pr(json!({"_links": null, "url": "https://acme.visualstudio.com/_apis/git/pullRequests/42"})),
        pr(json!({"repository": {"name": "web app", "project": {"name": "plat form"}}})),
        pr(json!({"repository": {"name": "web", "project": null}})),
        pr(json!({"repository": {"name": "web", "webUrl": "https://dev.azure.com/acme/p/_git/web///"}})),
        pr(json!({"url": null, "repository": null})),
        pr(json!({"extra": {"anything": [1, 2]}})),
        json!([]),
        json!("text"),
        json!(null),
    ];
    for row in &rows {
        cases.pure("decode", json!(["pullRequest", row.to_string()]));
    }
    cases.pure("decode", json!(["pullRequestList", Value::Array(rows.clone()).to_string()]));
    for raw in ["[]", "", "not json", "{}", "[1, null, {}]", " [ ] "] {
        cases.pure("decode", json!(["pullRequestList", raw]));
        cases.pure("decode", json!(["pullRequest", raw]));
    }
    for raw in [
        r#"{"user":{"name":" someone@example.test "}}"#,
        r#"{"user":{"name":"  "}}"#,
        r#"{"user":{}}"#,
        r#"{"user":null}"#,
        r#"{"user":"x"}"#,
        r#"{"user":{"name":3}}"#,
        "{}",
        "[]",
        "nope",
    ] {
        cases.pure("decode", json!(["viewer", raw]));
    }
    for raw in [
        r#"{"value": []}"#,
        r#"{"value": null}"#,
        r#"{}"#,
        r#"{"value": [{"id": 1, "threadContext": {"filePath": "  "}, "comments": [{"id": 1, "content": " x ", "publishedDate": "b"}, {"id": 2, "content": "y", "publishedDate": "a", "commentType": " System "}, {"id": 3, "content": "z", "publishedDate": "A"}, {"id": 4, "content": "w", "publishedDate": "2026-07-01T00:00:00Z", "commentType": "text"}]}]}"#,
        r#"{"value": [{"id": 2, "threadContext": null, "comments": null}, {"id": 3, "comments": [{"content": "q", "publishedDate": "2026-01-01", "author": {"displayName": "D"}}]}, {"id": 1.5, "comments": []}]}"#,
        r#"{"value": [{"id": 1, "comments": [{"id": 1, "content": "a", "publishedDate": "2026-07-02T00:00:00Z"}, {"id": 2, "content": "b", "publishedDate": "2026-07-02T00:00:00.5Z"}, {"id": 3, "content": "c", "publishedDate": "2026-07-02T00:00:00+01:00"}, {"id": 4, "content": "d", "publishedDate": "2026-07-01T23:59:59Z"}]}]}"#,
    ] {
        cases.pure("decode", json!(["threads", raw]));
    }
    for raw in [
        r#"{"value": [{"id": 3, "sourceRefCommit": {"commitId": " h3 "}, "commonRefCommit": {"commitId": "b"}}, {"id": 1, "sourceRefCommit": {"commitId": "h1"}, "commonRefCommit": {"commitId": "b"}}, {"id": 2, "sourceRefCommit": {"commitId": ""}, "commonRefCommit": {"commitId": "b"}}, {"id": "x"}, {"id": 4, "sourceRefCommit": null, "commonRefCommit": {"commitId": "b"}}]}"#,
        r#"{"value": "x"}"#,
        "[]",
    ] {
        cases.pure("decode", json!(["iterations", raw]));
    }
    for raw in [
        r#"{"changeEntries": [{"changeType": "Add", "item": {"path": "//a.md", "objectId": " 1 "}}, {"changeType": "delete, edit", "item": {"path": "/b.md"}}, {"changeType": "rename,edit", "sourceServerItem": "/", "originalPath": "/old.md", "item": {"path": "/new.md"}}, {"changeType": "sourceRename", "sourceServerItem": "/x.md", "item": {"path": "/x.md"}}, {"changeType": "undelete", "item": {"path": "/c.md", "gitObjectType": "BLOB"}}, {"changeType": "add", "item": {"path": "/sub", "gitObjectType": "commit"}}, {"changeType": "add", "item": {"path": "/"}}, {"changeType": "add"}, {"changeType": 3, "item": {"path": "/bad.md"}}, {"changeType": null, "item": {"path": "/null.md", "isFolder": false, "gitObjectType": null}}], "nextSkip": 2000.5}"#,
        r#"{"changeEntries": [], "nextSkip": 0}"#,
        r#"{"changeEntries": [], "nextSkip": -2}"#,
        r#"{"changeEntries": [], "nextSkip": null}"#,
        r#"{"changeEntries": [], "nextSkip": 1e3}"#,
        r#"{"changeEntries": [], "nextSkip": "5"}"#,
        r#"{"nextSkip": 5}"#,
    ] {
        cases.pure("decode", json!(["iterationChanges", raw]));
    }
    for raw in [
        r#"{"content": "x", "contentMetadata": {"isBinary": false}}"#,
        r#"{"content": null, "contentMetadata": null}"#,
        r#"{"content": 3}"#,
        r#"{"contentMetadata": {"isBinary": "yes"}}"#,
        r#"[]"#,
        "",
    ] {
        cases.pure("decode", json!(["itemContent", raw]));
    }
    for raw in [
        "1:0",
        "3:12",
        "0:4",
        "1:-2",
        "1:2:3",
        "1:",
        ":4",
        "1: ",
        " 1:4",
        "1:0x2",
        "0x1:2",
        "1e2:0",
        "1:4.0",
        "",
        "abc",
        "9007199254740993:1",
        "01:02",
    ] {
        cases.pure("parseCursor", json!([raw]));
    }
    cases.pure("parseCursor", json!([null]));
    for (left, right) in [
        ("2026-07-02T00:00:00Z", "2026-07-03T00:00:00Z"),
        ("2026-07-02T00:00:00.5Z", "2026-07-02T00:00:00Z"),
        ("2026-07-02T00:00:00+01:00", "2026-07-02T00:00:00Z"),
        ("2026-07-02T00:00:00-01:00", "2026-07-02T00:00:00+01:00"),
        ("2026-07-02", "2026-07-02T00:00:00Z"),
        ("a", "B"),
        ("a", "A"),
        ("x", "x"),
        ("1", "a"),
        ("_", "-"),
        (":", "."),
    ] {
        cases.pure("localeCompare", json!([left, right]));
    }
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Old/new file pairs: random edits over small vocabularies (where ties between equally short
/// scripts decide the hunks), line-ending and trailing-newline variants, large sparse edits, and
/// pairs either side of the edit ceiling.
fn diff_corpus() -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = [
        ("", ""),
        ("", "a\n"),
        ("a\n", ""),
        ("", "a"),
        ("a", ""),
        ("a", "a\n"),
        ("a\n", "a"),
        ("a", "b"),
        ("\n", "\n\n"),
        ("\n\n\n", "\n"),
        ("a\na\na\n", "a\na\n"),
        ("a\na\n", "a\na\na\n"),
        ("x\na\nb\na\nb\n", "x\na\nb\n"),
        ("a\nb\na\nb\na\n", "b\na\nb\na\nb\n"),
        ("one\r\ntwo\r\n", "one\r\ntwo again\r\n"),
        ("one\r\ntwo\r\n", "one\ntwo\n"),
        ("one\r\ntwo", "one\r\ntwo\r\n"),
        ("a\rb\nc\n", "a\rb\nd\n"),
        ("\r\n\r\n", "\r\n"),
        ("a\r", "a\r\n"),
        ("é\n😀\n中文\n", "é\n😁\n中文\n"),
        ("  \n\t\n", "\t\n  \n"),
        ("same\n", "same\n"),
        ("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\n", "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\nm\n"),
        ("1\n2\n3\n4\n5\n6\n7\n8\n", "1\n2\n3\nX\n5\n6\n7\nY\n"),
        ("1\n2\n3\n4\n5\n6\n7\n8\n9\n", "1\nX\n3\n4\n5\n6\n7\n8\nY\n"),
    ]
    .iter()
    .map(|(old, new)| ((*old).to_owned(), (*new).to_owned()))
    .collect();

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for seed in 0..240 {
        let vocabulary = [2, 3, 4, 6, 12, 40][seed % 6];
        let ending = match seed % 7 {
            0 => "\r\n",
            _ => "\n",
        };
        let count = rng.below(40);
        let mut old: Vec<String> = (0..count).map(|_| format!("line {}", rng.below(vocabulary))).collect();
        if seed % 11 == 0 {
            old.iter_mut().for_each(|line| *line = line.replace("line ", ""));
        }
        let mut new = old.clone();
        for _ in 0..rng.below(8) {
            let at = rng.below(new.len() + 1);
            match rng.below(3) {
                0 => new.insert(at, format!("line {}", rng.below(vocabulary + 2))),
                1 if at < new.len() => {
                    new.remove(at);
                }
                _ if at < new.len() => new[at] = format!("line {}", rng.below(vocabulary + 2)),
                _ => new.push(format!("tail {}", rng.below(3))),
            }
        }
        let join = |lines: &[String], trailing: bool| {
            let mut text = lines.join(ending);
            if trailing && !lines.is_empty() {
                text.push_str(ending);
            }
            text
        };
        let old_trailing = rng.below(5) != 0;
        let new_trailing = rng.below(5) != 0;
        pairs.push((join(&old, old_trailing), join(&new, new_trailing)));
    }

    // Large files with sparse edits.
    for (size, edits) in [(3_000usize, 5usize), (6_000, 30), (12_000, 3)] {
        let old: Vec<String> = (0..size).map(|line| format!("const value{line} = {};", line % 17)).collect();
        let mut new = old.clone();
        for _ in 0..edits {
            let at = rng.below(new.len());
            match rng.below(3) {
                0 => new.insert(at, format!("// inserted {}", rng.below(100))),
                1 => {
                    new.remove(at);
                }
                _ => new[at] = format!("const changed{at} = 0;"),
            }
        }
        pairs.push((format!("{}\n", old.join("\n")), format!("{}\n", new.join("\n"))));
    }
    // Appended at the end of a long file (the case jsdiff's diagonal pruning is for).
    let long: String = (0..5_000).map(|line| format!("row {line}\n")).collect();
    pairs.push((long.clone(), format!("{long}extra 1\nextra 2\n")));
    // Either side of the edit ceiling: 2,000 edits exactly, then 2,001.
    let side = |prefix: &str, count: usize| -> String { (0..count).map(|line| format!("{prefix} {line}\n")).collect() };
    pairs.push((side("old", 1_000), side("new", 1_000)));
    pairs.push((side("old", 1_000), side("new", 1_001)));
    pairs.push((side("old", 1_500), side("new", 1_500)));
    // Past the size ceiling, and a wholly new file just under it.
    pairs.push(("a\n".repeat(300_000), "b\n".repeat(10)));
    pairs.push((String::new(), "x\n".repeat(200_000)));
    pairs
}

fn diff_cases(cases: &mut Cases) -> usize {
    let corpus = diff_corpus();
    for (index, (old, new)) in corpus.iter().enumerate() {
        let kind = match (old.is_empty(), new.is_empty(), index % 9) {
            (true, false, _) => "new",
            (false, true, _) => "deleted",
            (_, _, 3) => "rename-changed",
            _ => "change",
        };
        let (path, old_path) = if kind == "rename-changed" {
            ("src/next name.ts", "src/prev\tname.ts")
        } else {
            ("dir/file.txt", "dir/file.txt")
        };
        let change = json!({"path": path, "oldPath": old_path, "changeKind": kind, "objectId": "1", "originalObjectId": null});
        cases.pure("filePatch", json!([change, {"oldContents": old, "newContents": new, "binary": false}]));
        if old.len() + new.len() < 400_000 {
            cases.pure("structuredPatch", json!([old, new, 2000]));
        }
        if index % 25 == 0 && old.len() + new.len() < 20_000 {
            cases.pure("structuredPatch", json!([old, new, null]));
        }
    }
    let change = json!({"path": "docs/new.md", "oldPath": "docs/old.md", "changeKind": "rename-pure", "objectId": null, "originalObjectId": null});
    cases.pure(
        "filePatch",
        json!([change, {"oldContents": "same\n", "newContents": "same\n", "binary": false}]),
    );
    cases.pure("filePatch", json!([change, {"oldContents": "PNG\u{0}", "newContents": "PNG", "binary": false}]));
    cases.pure("filePatch", json!([change, {"oldContents": "a", "newContents": "b", "binary": true}]));
    cases.pure("unreadableFilePatch", json!([change]));
    corpus.len()
}

// ---------------------------------------------------------------------------------------------
// The Rust side, mapped to the TS shapes.
// ---------------------------------------------------------------------------------------------

fn put(map: &mut Map<String, Value>, key: &str, value: impl serde::Serialize) {
    map.insert(key.into(), serde_json::to_value(value).unwrap());
}

fn put_some<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        put(map, key, value);
    }
}

fn change_request_json(cr: &ProviderChangeRequest) -> Map<String, Value> {
    let mut map = Map::new();
    put_some(&mut map, "stack", &cr.stack);
    put(&mut map, "number", cr.number);
    put(&mut map, "title", &cr.title);
    put(&mut map, "url", &cr.url);
    put(&mut map, "author", &cr.author);
    put(&mut map, "headBranch", &cr.head_branch);
    put_some(&mut map, "headRepositoryNameWithOwner", &cr.head_repository_name_with_owner);
    put(&mut map, "baseBranch", &cr.base_branch);
    put(&mut map, "state", cr.state);
    put(&mut map, "isDraft", cr.is_draft);
    put(&mut map, "mergeability", cr.mergeability);
    put(&mut map, "additions", cr.additions);
    put(&mut map, "deletions", cr.deletions);
    put(&mut map, "createdAt", &cr.created_at);
    put_some(&mut map, "closedAt", &cr.closed_at);
    put_some(&mut map, "mergedAt", &cr.merged_at);
    put(&mut map, "updatedAt", &cr.updated_at);
    put(&mut map, "reviewRequestLogins", &cr.review_request_logins);
    put(&mut map, "labels", &cr.labels);
    put_some(&mut map, "reviewDecision", &cr.review_decision);
    put_some(&mut map, "checksState", &cr.checks_state);
    map
}

fn detail_json(detail: &ProviderChangeRequestDetail) -> Value {
    let mut map = change_request_json(&detail.change_request);
    put(&mut map, "body", &detail.body);
    put(&mut map, "changedFiles", detail.changed_files);
    put(&mut map, "mergedAt", &detail.merged_at);
    put(&mut map, "closedAt", &detail.closed_at);
    put(&mut map, "reviewers", &detail.reviewers);
    put(&mut map, "checks", &detail.checks);
    put(&mut map, "mergeCapabilities", &detail.merge_capabilities);
    put(&mut map, "viewerPermissions", &detail.viewer_permissions);
    put_some(&mut map, "baseComparison", &detail.base_comparison);
    put_some(&mut map, "behindBy", &detail.behind_by);
    put_some(&mut map, "autoMergeEnabled", &detail.auto_merge_enabled);
    put_some(&mut map, "autoMergeMethod", &detail.auto_merge_method);
    put_some(&mut map, "workflowApprovalsRequired", &detail.workflow_approvals_required);
    Value::Object(map)
}

fn summary_json(summary: &ProviderChangeRequestSummary) -> Value {
    let mut map = Map::new();
    put(&mut map, "number", summary.number);
    put(&mut map, "title", &summary.title);
    put(&mut map, "url", &summary.url);
    put_some(&mut map, "author", &summary.author);
    put(&mut map, "headBranch", &summary.head_branch);
    put(&mut map, "baseBranch", &summary.base_branch);
    put(&mut map, "state", summary.state);
    put_some(&mut map, "isDraft", &summary.is_draft);
    put_some(&mut map, "mergeability", &summary.mergeability);
    put_some(&mut map, "closedAt", &summary.closed_at);
    put_some(&mut map, "mergedAt", &summary.merged_at);
    put(&mut map, "updatedAt", &summary.updated_at);
    put_some(&mut map, "additions", &summary.additions);
    put_some(&mut map, "deletions", &summary.deletions);
    put_some(&mut map, "changedFiles", &summary.changed_files);
    put_some(&mut map, "reviewDecision", &summary.review_decision);
    put_some(&mut map, "checksState", &summary.checks_state);
    Value::Object(map)
}

fn activity_json(activity: &ProviderChangeRequestActivity) -> Value {
    let mut map = Map::new();
    put_some(&mut map, "author", &activity.author);
    put_some(&mut map, "reviewers", &activity.reviewers);
    put(&mut map, "comments", &activity.comments);
    put(&mut map, "commentCount", activity.comment_count);
    put(&mut map, "commentsTruncated", activity.comments_truncated);
    put(&mut map, "reviewThreads", &activity.review_threads);
    put(&mut map, "commits", &activity.commits);
    put_some(&mut map, "reactions", &activity.reactions);
    Value::Object(map)
}

fn slice_json(slice: &ProviderDiffSlice) -> Value {
    let mut map = Map::new();
    put(&mut map, "patch", &slice.patch);
    put(&mut map, "truncated", slice.truncated);
    put(&mut map, "nextCursor", &slice.next_cursor);
    put_some(&mut map, "omittedFileStats", &slice.omitted_file_stats);
    Value::Object(map)
}

fn revisions_json(revisions: &ProviderFileRevisions) -> Value {
    let mut map = Map::new();
    put(&mut map, "revisions", &revisions.revisions);
    put_some(&mut map, "complete", &revisions.complete);
    Value::Object(map)
}

fn pull_request_json(pull_request: &AzureDevOpsPullRequest) -> Value {
    let mut map = Map::new();
    put(&mut map, "number", pull_request.number);
    put(&mut map, "title", &pull_request.title);
    put(&mut map, "url", &pull_request.url);
    put(&mut map, "author", &pull_request.author);
    put(&mut map, "headBranch", &pull_request.head_branch);
    put(&mut map, "baseBranch", &pull_request.base_branch);
    put(&mut map, "state", pull_request.state);
    put(&mut map, "isDraft", pull_request.is_draft);
    put(&mut map, "mergeability", pull_request.mergeability);
    put(&mut map, "createdAt", &pull_request.created_at);
    put(&mut map, "updatedAt", &pull_request.updated_at);
    put(&mut map, "closedAt", &pull_request.closed_at);
    put(&mut map, "body", &pull_request.body);
    put(&mut map, "reviewRequestLogins", &pull_request.review_request_logins);
    put(&mut map, "reviewers", &pull_request.reviewers);
    put(
        &mut map,
        "location",
        pull_request
            .location
            .as_ref()
            .map(|location| json!({"project": location.project, "repository": location.repository})),
    );
    put(&mut map, "autoMergeEnabled", pull_request.auto_merge_enabled);
    put_some(&mut map, "autoMergeMethod", &pull_request.auto_merge_method);
    Value::Object(map)
}

fn change_entry_json(change: &AzureDevOpsChangeEntry) -> Value {
    json!({"path": change.path, "oldPath": change.old_path, "changeKind": change.change_kind, "objectId": change.object_id, "originalObjectId": change.original_object_id})
}

fn decoded<T>(result: Result<T, String>, to_json: impl Fn(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({ "ok": to_json(value) }),
        Err(_) => json!({ "failed": true }),
    }
}

fn str_arg(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

fn rust_pure(function: &str, args: &[Value]) -> Value {
    match function {
        "decode" => {
            let raw = str_arg(&args[1]);
            match args[0].as_str().unwrap() {
                "pullRequestList" => decoded(
                    decode_pull_request_list_json(&raw),
                    |batch| json!({"items": batch.items.iter().map(pull_request_json).collect::<Vec<_>>(), "rawIndexes": batch.raw_indexes, "rawCount": batch.raw_count}),
                ),
                "pullRequest" => decoded(decode_pull_request_json(&raw), |pr| pr.as_ref().map_or(Value::Null, pull_request_json)),
                "viewer" => decoded(decode_viewer_json(&raw), |viewer| json!(viewer)),
                "threads" => decoded(decode_threads_json(&raw), |comments| serde_json::to_value(comments).unwrap()),
                "iterations" => decoded(decode_iterations_json(&raw), |iterations| {
                    Value::Array(
                        iterations
                            .iter()
                            .map(|i| json!({"id": i.id, "headCommit": i.head_commit, "mergeBaseCommit": i.merge_base_commit}))
                            .collect(),
                    )
                }),
                "iterationChanges" => decoded(
                    decode_iteration_changes_json(&raw),
                    |page| json!({"changes": page.changes.iter().map(change_entry_json).collect::<Vec<_>>(), "nextSkip": page.next_skip}),
                ),
                "itemContent" => decoded(
                    decode_item_content_json(&raw),
                    |item| json!({"contents": item.contents, "isBinary": item.is_binary}),
                ),
                other => panic!("unknown decoder {other}"),
            }
        }
        "filePatch" | "unreadableFilePatch" => {
            let change = &args[0];
            let entry = AzureDevOpsChangeEntry {
                path: str_arg(&change["path"]),
                old_path: str_arg(&change["oldPath"]),
                change_kind: serde_json::from_value(change["changeKind"].clone()).unwrap(),
                object_id: change["objectId"].as_str().map(Into::into),
                original_object_id: change["originalObjectId"].as_str().map(Into::into),
            };
            let patch = if function == "unreadableFilePatch" {
                azure_devops_unreadable_file_patch(&entry)
            } else {
                let texts = &args[1];
                azure_devops_file_patch(
                    &entry,
                    &AzureDevOpsFileTexts {
                        old_contents: str_arg(&texts["oldContents"]),
                        new_contents: str_arg(&texts["newContents"]),
                        binary: texts["binary"].as_bool().unwrap(),
                    },
                )
            };
            json!({"section": patch.section, "truncated": patch.truncated, "abandoned": patch.abandoned, "edits": patch.edits})
        }
        "structuredPatch" => {
            let options = PatchOptions {
                context: 3,
                max_edit_length: args[2].as_u64().map(|limit| limit as usize),
                timeout: None,
            };
            match structured_patch(args[0].as_str().unwrap(), args[1].as_str().unwrap(), &options) {
                None => Value::Null,
                Some(hunks) => Value::Array(
                    hunks
                        .iter()
                        .map(|h| json!({"oldStart": h.old_start, "oldLines": h.old_lines, "newStart": h.new_start, "newLines": h.new_lines, "lines": h.lines}))
                        .collect(),
                ),
            }
        }
        "parseCursor" => match parse_azure_devops_diff_cursor(args[0].as_str()) {
            None => Value::Null,
            Some(cursor) => json!({"iterationId": cursor.iteration_id, "fileIndex": cursor.file_index}),
        },
        "localeCompare" => json!(locale_compare(args[0].as_str().unwrap(), args[1].as_str().unwrap()) as i32),
        other => panic!("unknown pure fn {other}"),
    }
}

fn change_request_ref(input: &Value) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: str_arg(&input["cwd"]),
        repository: str_arg(&input["repository"]),
        host: str_arg(&input["host"]),
        number: input["number"].as_i64().unwrap(),
    }
}

fn from<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn outcome<T>(result: ProviderResult<T>, to_json: impl Fn(&T) -> Value) -> Value {
    match result {
        Ok(value) => json!({ "ok": to_json(&value) }),
        Err(error) => json!({ "error": error.to_wire() }),
    }
}

async fn rust_provider(az: &Arc<FakeAz>, method: &str, input: &Value) -> Value {
    // A fresh provider per case, like the oracle.
    let provider = AzureDevOpsPullRequestProvider::new(AzureDevOpsCli::new(az.process()));
    let unit = |_: &()| Value::Null;
    match method {
        "getViewer" => outcome(
            provider
                .get_viewer(ProviderHostRef {
                    cwd: str_arg(&input["cwd"]),
                    host: None,
                })
                .await,
            |viewer| json!(viewer),
        ),
        "listChangeRequests" => outcome(
            provider
                .list_change_requests(ListChangeRequestsInput {
                    cwd: str_arg(&input["cwd"]),
                    repository: str_arg(&input["repository"]),
                    host: str_arg(&input["host"]),
                    state: from(&input["state"]),
                    involvement: from(&input["involvement"]),
                    viewer: str_arg(&input["viewer"]),
                    limit: input["limit"].as_i64().unwrap(),
                    query: input["query"].as_str().map(Into::into),
                    cursor: input.get("cursor").map(|cursor| ProviderListCursor {
                        updated_before: str_arg(&cursor["updatedBefore"]),
                        delivered: cursor["delivered"].as_i64().unwrap(),
                    }),
                    filters: None,
                })
                .await,
            |page| {
                let mut map = Map::new();
                put(
                    &mut map,
                    "items",
                    page.items.iter().map(|item| Value::Object(change_request_json(item))).collect::<Vec<_>>(),
                );
                put(&mut map, "truncated", page.truncated);
                put_some(&mut map, "cursorAdvance", &page.cursor_advance);
                put(&mut map, "continues", page.continues);
                Value::Object(map)
            },
        ),
        "getChangeRequest" => outcome(provider.get_change_request(change_request_ref(input)).await, detail_json),
        "getChangeRequestSummary" => outcome(provider.get_change_request_summary(change_request_ref(input)).await, summary_json),
        "getChangeRequestActivity" => outcome(provider.get_change_request_activity(change_request_ref(input)).await, activity_json),
        "getViewerPermissions" => outcome(
            provider
                .get_viewer_permissions(ViewerPermissionsInput {
                    change_request: change_request_ref(input),
                    include_update_branch: input["includeUpdateBranch"].as_bool(),
                })
                .await,
            |permissions| serde_json::to_value(permissions).unwrap(),
        ),
        "getDiff" => outcome(
            provider
                .get_diff(GetDiffInput {
                    change_request: change_request_ref(input),
                    cursor: input["cursor"].as_str().map(Into::into),
                    commit: None,
                })
                .await,
            slice_json,
        ),
        "getDiffFileContents" => outcome(
            provider
                .get_diff_file_contents(DiffFileContentsInput {
                    change_request: change_request_ref(input),
                    commit: None,
                    change_type: from::<PullRequestDiffFileContentsInputChangeType>(&input["changeType"]),
                    old_path: str_arg(&input["oldPath"]),
                    new_path: str_arg(&input["newPath"]),
                })
                .await,
            |contents| json!({"oldContents": contents.old_contents, "newContents": contents.new_contents}),
        ),
        "getFileRevisions" => outcome(
            provider
                .get_file_revisions(FileRevisionsInput {
                    change_request: change_request_ref(input),
                    paths: from(&input["paths"]),
                })
                .await,
            revisions_json,
        ),
        "runAction" => outcome(
            provider
                .run_action(RunActionInput {
                    change_request: change_request_ref(input),
                    action: from(&input["action"]),
                    stack_number: None,
                    expected_stack_heads: None,
                    merge_method: input.get("mergeMethod").map(from),
                    update_method: None,
                })
                .await,
            unit,
        ),
        "updateChangeRequest" => outcome(
            provider
                .update_change_request(UpdateChangeRequestInput {
                    change_request: change_request_ref(input),
                    title: input["title"].as_str().map(Into::into),
                    body: input["body"].as_str().map(Into::into),
                })
                .await,
            unit,
        ),
        "setReviewerRequest" => outcome(
            provider
                .set_reviewer_request(SetReviewerRequestInput {
                    change_request: change_request_ref(input),
                    reviewers: input["reviewers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|reviewer| ReviewerRef {
                            id: str_arg(&reviewer["id"]),
                            kind: from(&reviewer["kind"]),
                        })
                        .collect(),
                    requested: input["requested"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        "listReviewerCandidates" => outcome(provider.list_reviewer_candidates(change_request_ref(input)).await, |list| {
            serde_json::to_value(list).unwrap()
        }),
        "comment" => outcome(
            provider
                .comment(CommentInput {
                    change_request: change_request_ref(input),
                    body: str_arg(&input["body"]),
                })
                .await,
            unit,
        ),
        "submitReview" => outcome(
            provider
                .submit_review(SubmitReviewInput {
                    change_request: change_request_ref(input),
                    verdict: from::<PullRequestReviewVerdict>(&input["verdict"]),
                    body: str_arg(&input["body"]),
                    comments: Vec::new(),
                })
                .await,
            unit,
        ),
        "replyToThread" => outcome(
            provider
                .reply_to_thread(ReplyToThreadInput {
                    change_request: change_request_ref(input),
                    thread_id: str_arg(&input["threadId"]),
                    body: str_arg(&input["body"]),
                })
                .await,
            unit,
        ),
        "setReaction" => outcome(
            provider
                .set_reaction(SetReactionInput {
                    change_request: change_request_ref(input),
                    subject_id: None,
                    content: from(&input["content"]),
                    reacted: input["reacted"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        "setThreadResolution" => outcome(
            provider
                .set_thread_resolution(SetThreadResolutionInput {
                    change_request: change_request_ref(input),
                    thread_id: str_arg(&input["threadId"]),
                    resolved: input["resolved"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        other => panic!("unknown provider method {other}"),
    }
}

fn read_log(az: &FakeAz) -> Vec<String> {
    let path = az.path().join("az.log");
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, "").unwrap();
    let mut lines: Vec<String> = log.lines().map(str::to_owned).collect();
    lines.sort();
    lines
}

/// The first line where two texts differ, for a readable failure.
fn first_difference(expected: &Value, actual: &Value) -> String {
    let (expected, actual) = (serde_json::to_string_pretty(expected).unwrap(), serde_json::to_string_pretty(actual).unwrap());
    for (index, (left, right)) in expected.lines().zip(actual.lines()).enumerate() {
        if left != right {
            return format!("line {index}: ts {left:?} / rust {right:?}");
        }
    }
    format!("lengths differ: ts {} / rust {} lines", expected.lines().count(), actual.lines().count())
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_matches_the_typescript_azure_devops_code() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let az = FakeAz::new();
    record_fake_outputs(&az);
    let cwd = tempfile::Builder::new().prefix("zc-azure-golden-cwd-").tempdir().unwrap();
    let cwd_path = std::fs::canonicalize(cwd.path()).unwrap();
    let home = tempfile::Builder::new().prefix("zc-azure-golden-home-").tempdir().unwrap();

    let mut cases = Cases { cases: Vec::new() };
    provider_cases(&mut cases, cwd_path.to_str().unwrap());
    let provider_count = cases.cases.len();
    decode_cases(&mut cases);
    let pairs = diff_cases(&mut cases);

    let expected = run_oracle(&cases.cases, &az.path(), home.path());
    let ts_calls = read_log(&az);
    // `ZC_AZURE_GOLDEN_DUMP=<file>` keeps the oracle's answers for a look at what was compared.
    if let Ok(dump) = std::env::var("ZC_AZURE_GOLDEN_DUMP") {
        std::fs::write(dump, serde_json::to_string_pretty(&expected).unwrap()).unwrap();
    }

    let mut mismatches = Vec::new();
    let mut compared = 0;
    for case in &cases.cases {
        let id = case["id"].as_str().unwrap();
        let mut actual = match case["op"].as_str().unwrap() {
            "provider" => rust_provider(&az, case["method"].as_str().unwrap(), &case["input"]).await,
            _ => json!({ "ok": rust_pure(case["fn"].as_str().unwrap(), case["args"].as_array().unwrap()) }),
        };
        let mut wanted = expected.get(id).cloned().unwrap_or_else(|| panic!("the oracle has no result for {id}"));
        // TS hands `getDiffFileContents` the whole `AzureDevOpsFileTexts`; the binary flag is not
        // part of the provider contract (`ProviderDiffFileContents`).
        if case["method"] == "getDiffFileContents" {
            if let Some(ok) = wanted.get_mut("ok").and_then(Value::as_object_mut) {
                ok.remove("binary");
            }
        }
        normalize(wanted.get_mut("error").unwrap_or(&mut Value::Null), 0);
        normalize(actual.get_mut("error").unwrap_or(&mut Value::Null), 0);
        compared += 1;
        if wanted != actual {
            mismatches.push(format!(
                "{id} {}: {}",
                case.get("input")
                    .map(Value::to_string)
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect::<String>(),
                first_difference(&wanted, &actual)
            ));
        }
    }
    let rust_calls = read_log(&az);
    eprintln!(
        "compared {compared} cases ({provider_count} provider calls, {pairs} diff pairs) and {} az invocations",
        ts_calls.len()
    );
    assert!(
        mismatches.is_empty(),
        "{} of {compared} cases differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    assert_eq!(ts_calls, rust_calls, "the two sides ran different az command lines");
}
