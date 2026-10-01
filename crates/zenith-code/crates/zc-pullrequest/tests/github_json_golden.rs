//! Golden comparison against the TypeScript `gitHubPullRequestJson.ts`: every fixture of its test
//! file plus edge cases (`support_github_json::decoder_corpus`) goes through the TS decoders
//! (`golden/github_json_oracle.mjs`, from source through node) and the Rust ones, and the results
//! must be the same JSON, failures included (compared by `formatSchemaError` message). Every
//! exported constant and every request builder's output is compared byte for byte.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously.

#![recursion_limit = "512"]

mod support_github_json;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::{json, Map, Value};
use zc_contracts::{PullRequestReactionContent, PullRequestReviewCommentDraft, PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewerKind};
use zc_pullrequest::github::json::*;
use zc_pullrequest::provider::ReviewerRef;

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    let server = server_dir();
    if !server.join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server.display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(cases: &[Value]) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/github_json_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(json!({ "cases": cases }).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn decoded<T: Serialize>(result: Result<T, DecodeFailure>) -> Value {
    match result {
        Ok(value) => json!({ "ok": serde_json::to_value(value).unwrap() }),
        Err(failure) => json!({ "error": failure.message() }),
    }
}

fn rust_decode(decoder: &str, raw: &str) -> Value {
    match decoder {
        "decodeActorAvatarsJson" => decoded(decode_actor_avatars_json(raw)),
        "decodePullRequestPreviewJson" => decoded(decode_pull_request_preview_json(raw)),
        "decodePullRequestNodeIdJson" => decoded(decode_pull_request_node_id_json(raw)),
        "decodeReactionSubjectScopeJson" => decoded(decode_reaction_subject_scope_json(raw)),
        "decodePullRequestListJson" => decoded(decode_pull_request_list_json(raw)),
        "decodePullRequestSearchJson" => decoded(decode_pull_request_search_json(raw)),
        "decodePullRequestStackMembershipsJson" => decoded(decode_pull_request_stack_memberships_json(raw)),
        "decodePullRequestStatsJson" => decoded(decode_pull_request_stats_json(raw)),
        "decodePullRequestSummariesJson" => decoded(decode_pull_request_summaries_json(raw)),
        "decodePullRequestCoreJson" => decoded(decode_pull_request_core_json(raw)),
        "decodePullRequestDetailJson" => decoded(decode_pull_request_detail_json(raw)),
        "decodeWorkflowRunApprovalsJson" => decoded(decode_workflow_run_approvals_json(raw)),
        "decodePullRequestHeadsJson" => decoded(decode_pull_request_heads_json(raw)),
        "decodePullRequestActivityJson" => decoded(decode_pull_request_activity_json(raw)),
        "decodeReviewDismissalsJson" => decoded(decode_review_dismissals_json(raw)),
        "decodeReviewThreadsJson" => decoded(decode_review_threads_json(raw)),
        "decodeReviewThreadCommentsJson" => decoded(decode_review_thread_comments_json(raw)),
        "decodeBaseComparisonJson" => decoded(decode_base_comparison_json(raw)),
        "decodeReviewerCandidatesJson" => decoded(decode_reviewer_candidates_json(raw)),
        "decodeLabelCandidatesJson" => decoded(decode_label_candidates_json(raw)),
        "decodeViewerPermissionsJson" => decoded(decode_viewer_permissions_json(raw)),
        "decodePullRequestFilesJson" => decoded(decode_pull_request_files_json(raw)),
        "decodePullRequestFilesViewedJson" => decoded(decode_pull_request_files_viewed_json(raw)),
        "decodePullRequestStacksJson" => decoded(decode_pull_request_stacks_json(raw)),
        other => panic!("no Rust decoder for {other}"),
    }
}

/// Every exported constant, by its TS name.
fn rust_constants() -> Value {
    json!({
        "ACTOR_AVATARS_GRAPHQL_QUERY": ACTOR_AVATARS_GRAPHQL_QUERY,
        "PULL_REQUEST_LIST_JSON_FIELDS": PULL_REQUEST_LIST_JSON_FIELDS,
        "PULL_REQUEST_DETAIL_JSON_FIELDS": PULL_REQUEST_DETAIL_JSON_FIELDS,
        "PULL_REQUEST_CORE_GRAPHQL_QUERY": PULL_REQUEST_CORE_GRAPHQL_QUERY,
        "PULL_REQUEST_PREVIEW_GRAPHQL_QUERY": PULL_REQUEST_PREVIEW_GRAPHQL_QUERY,
        "PULL_REQUEST_ACTIVITY_JSON_FIELDS": PULL_REQUEST_ACTIVITY_JSON_FIELDS,
        "PULL_REQUEST_SEARCH_MAX_ROWS": PULL_REQUEST_SEARCH_MAX_ROWS,
        "REVIEW_THREADS_GRAPHQL_QUERY": REVIEW_THREADS_GRAPHQL_QUERY,
        "REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY": REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY,
        "REVIEW_THREAD_REPLY_GRAPHQL_MUTATION": REVIEW_THREAD_REPLY_GRAPHQL_MUTATION,
        "PULL_REQUEST_NODE_ID_GRAPHQL_QUERY": PULL_REQUEST_NODE_ID_GRAPHQL_QUERY,
        "REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY": REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY,
        "ADD_REACTION_GRAPHQL_MUTATION": ADD_REACTION_GRAPHQL_MUTATION,
        "REMOVE_REACTION_GRAPHQL_MUTATION": REMOVE_REACTION_GRAPHQL_MUTATION,
        "RESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION": RESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION,
        "UNRESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION": UNRESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION,
        "UPDATE_PULL_REQUEST_GRAPHQL_MUTATION": UPDATE_PULL_REQUEST_GRAPHQL_MUTATION,
        "REVERT_PULL_REQUEST_GRAPHQL_MUTATION": REVERT_PULL_REQUEST_GRAPHQL_MUTATION,
        "UPDATE_ISSUE_COMMENT_GRAPHQL_MUTATION": UPDATE_ISSUE_COMMENT_GRAPHQL_MUTATION,
        "UPDATE_REVIEW_COMMENT_GRAPHQL_MUTATION": UPDATE_REVIEW_COMMENT_GRAPHQL_MUTATION,
        "REVIEW_DISMISSALS_GRAPHQL_QUERY": REVIEW_DISMISSALS_GRAPHQL_QUERY,
        "BASE_COMPARISON_GRAPHQL_QUERY": BASE_COMPARISON_GRAPHQL_QUERY,
        "REVIEWER_CANDIDATES_GRAPHQL_QUERY": REVIEWER_CANDIDATES_GRAPHQL_QUERY,
        "LABEL_CANDIDATES_GRAPHQL_QUERY": LABEL_CANDIDATES_GRAPHQL_QUERY,
        "VIEWER_PERMISSIONS_GRAPHQL_QUERY": VIEWER_PERMISSIONS_GRAPHQL_QUERY,
        "PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY": PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY,
    })
}

fn review_position(value: &Value) -> PullRequestReviewCommentDraft {
    serde_json::from_value(value.clone()).unwrap()
}

/// The builder cases, each with the Rust result next to its input.
fn builder_cases() -> Vec<(Value, Value)> {
    let mut cases = Vec::new();
    let mut add = |case: Value, rust: Value| {
        let id = format!("builder#{}", cases.len());
        let mut case = case;
        case["id"] = json!(id);
        cases.push((case, rust));
    };
    for (rows, include_stacks) in [(20, true), (20, false), (0, false), (-4, true), (100, false), (101, true), (37, false)] {
        add(
            json!({ "op": "pullRequestSearchGraphQlQuery", "rows": rows, "includeStacks": include_stacks }),
            json!(pull_request_search_graph_ql_query(rows, include_stacks)),
        );
    }
    let variables: Vec<(&str, &str)> = vec![
        ("owner", "acme"),
        ("name", "widgets"),
        ("number", "7"),
        ("body", "line \"one\"\n\ttab \u{1} é \u{2028} </script>"),
        ("owner", "override"),
    ];
    add(
        json!({ "op": "encodeGraphQlRequestJson", "query": REVIEW_THREADS_GRAPHQL_QUERY, "variables": variables }),
        json!(encode_graph_ql_request_json(REVIEW_THREADS_GRAPHQL_QUERY, &variables)),
    );
    add(
        json!({ "op": "encodeGraphQlRequestJson", "query": "q", "variables": [] }),
        json!(encode_graph_ql_request_json::<&str, &str>("q", &[])),
    );
    let comments = json!([
        { "path": "src/a.ts", "position": { "kind": "added", "newLine": 12 }, "body": "rename this" },
        { "path": "src/b.ts", "position": { "kind": "deleted", "oldLine": 3 }, "body": "why remove?" },
        { "path": "src/c.ts", "oldPath": "src/old.ts", "position": { "kind": "context", "oldLine": 4, "newLine": 5, "side": "left" }, "body": "left" },
        { "path": "src/c.ts", "position": { "kind": "context", "oldLine": 4, "newLine": 5, "side": "right" }, "body": "right \"quoted\"" },
    ]);
    let drafts: Vec<PullRequestReviewCommentDraft> = comments.as_array().unwrap().iter().map(review_position).collect();
    for (verdict, wire) in [
        (PullRequestReviewVerdict::RequestChanges, "request-changes"),
        (PullRequestReviewVerdict::Comment, "comment"),
    ] {
        add(
            json!({ "op": "buildReviewSubmissionJson", "verdict": wire, "body": "Two things.", "comments": comments }),
            json!(build_review_submission_json(verdict, "Two things.", &drafts)),
        );
    }
    add(
        json!({ "op": "buildReviewSubmissionJson", "verdict": "approve", "body": "", "comments": [] }),
        json!(build_review_submission_json(PullRequestReviewVerdict::Approve, "", &[])),
    );
    let reviewers = vec![
        ReviewerRef {
            id: "ada-example".into(),
            kind: PullRequestReviewerKind::User,
        },
        ReviewerRef {
            id: "reviewers".into(),
            kind: PullRequestReviewerKind::Team,
        },
        ReviewerRef {
            id: "helper-bot".into(),
            kind: PullRequestReviewerKind::User,
        },
    ];
    add(
        json!({ "op": "buildReviewerRequestJson", "reviewers": [{ "id": "ada-example", "kind": "user" }, { "id": "reviewers", "kind": "team" }, { "id": "helper-bot", "kind": "user" }] }),
        json!(build_reviewer_request_json(&reviewers)),
    );
    add(
        json!({ "op": "buildReviewerRequestJson", "reviewers": [{ "id": "ada-example", "kind": "user" }] }),
        json!(build_reviewer_request_json(&reviewers[..1])),
    );
    add(
        json!({ "op": "buildLabelRequestJson", "labels": ["bug", "size:XL", "\"quoted\""] }),
        json!(build_label_request_json(&["bug", "size:XL", "\"quoted\""])),
    );
    add(
        json!({ "op": "buildLabelRequestJson", "labels": [] }),
        json!(build_label_request_json::<&str>(&[])),
    );
    let batches: Vec<Vec<(&str, i64)>> = vec![
        vec![("acme/widgets", 1), (" acme/web ", 22)],
        vec![("acme/web\") { x } #", 1)],
        vec![("acme/web", 0)],
        vec![("acme/web", -3)],
        vec![("acme", 1)],
        vec![("acme/web/extra", 1)],
        vec![("/web", 1)],
        vec![("acme.io/we_b-2", 9_007_199_254_740_991)],
        vec![("acme/web", 9_007_199_254_740_992)],
        vec![],
    ];
    for batch in &batches {
        add(
            json!({ "op": "buildPullRequestStatsGraphQlQuery", "changeRequests": batch }),
            json!(build_pull_request_stats_graph_ql_query(batch)),
        );
        add(
            json!({ "op": "buildPullRequestSummariesGraphQlQuery", "changeRequests": batch }),
            json!(build_pull_request_summaries_graph_ql_query(batch)),
        );
    }
    let memberships: Vec<(&str, Vec<i64>)> = vec![
        ("acme/web", vec![7, 8]),
        ("acme/web\") { x } #", vec![1]),
        ("acme/web", vec![0]),
        ("acme/web", vec![]),
        (" acme/web ", vec![1]),
        ("bad repo", vec![]),
    ];
    for (repository, numbers) in &memberships {
        add(
            json!({ "op": "buildPullRequestStackMembershipsGraphQlQuery", "repository": repository, "numbers": numbers }),
            json!(build_pull_request_stack_memberships_graph_ql_query(repository, numbers)),
        );
    }
    let file_batches: Vec<Vec<(&str, bool)>> = vec![
        vec![],
        vec![("src/a.ts", true), ("src/b.ts", false)],
        vec![("\") { __typename } evil: markFileAsViewed(input: { path: \"x", true)],
    ];
    for files in &file_batches {
        let rust = build_set_files_viewed_graph_ql_mutation(files).map(|mutation| {
            json!({ "query": mutation.query, "variables": mutation.variables.into_iter().map(|(name, value)| (name, Value::String(value))).collect::<Map<_, _>>() })
        });
        add(json!({ "op": "buildSetFilesViewedGraphQlMutation", "files": files }), json!(rust));
    }
    let threads = json!([
        { "id": "PRRT_1", "path": "src/a.ts", "line": 3, "side": "left", "isResolved": true, "isOutdated": false, "comments": [
            { "id": "c1", "author": { "login": "ada-example", "name": null, "avatarUrl": null }, "body": "x", "createdAt": "2026-07-01T00:00:00Z", "url": null },
            { "id": "c2", "author": null, "body": "y", "createdAt": "2026-07-02T00:00:00Z", "url": "u", "reactions": [{ "content": "heart", "count": 1, "actors": [], "viewerHasReacted": true }] },
        ] },
        { "id": "PRRT_2", "path": "src/b.ts", "line": null, "side": "right", "isResolved": false, "isOutdated": true, "comments": [] },
    ]);
    let parsed: Vec<PullRequestReviewThread> = serde_json::from_value(threads.clone()).unwrap();
    add(
        json!({ "op": "reviewThreadConversation", "threads": threads }),
        serde_json::to_value(review_thread_conversation(&parsed)).unwrap(),
    );
    for content in PullRequestReactionContent::ALL {
        add(
            json!({ "op": "gitHubReactionContent", "content": content.as_str() }),
            json!(git_hub_reaction_content(*content)),
        );
    }
    for path in [
        "a/src/app.ts",
        "a/src\\notes.ts",
        "b/tab\there",
        "b/new\nline",
        "a/\"q\"",
        "a/bell\u{7}\u{8}\u{b}\u{c}\r",
        "a/ctl\u{1}\u{1f}\u{7f}",
        "a/é 漢字 😀",
        "",
    ] {
        add(json!({ "op": "quoteGitPatchPath", "path": path }), json!(quote_git_patch_path(path)));
    }
    cases
}

#[test]
fn matches_the_typescript_json_module() {
    if let Err(reason) = oracle_available() {
        println!("skipping the golden comparison: {reason}");
        return;
    }
    let corpus = support_github_json::decoder_corpus();
    let builders = builder_cases();
    let mut cases: Vec<Value> = corpus.iter().map(|(id, decoder, raw)| json!({ "id": id, "op": decoder, "raw": raw })).collect();
    cases.push(json!({ "id": "constants", "op": "constants" }));
    cases.extend(builders.iter().map(|(case, _)| case.clone()));
    let oracle = run_oracle(&cases);

    let mut mismatches = Vec::new();
    for (id, decoder, raw) in &corpus {
        let rust = rust_decode(decoder, raw);
        let ts = &oracle[id];
        if &rust != ts {
            mismatches.push(format!("{id}\n  input: {raw}\n  ts:    {ts}\n  rust:  {rust}"));
        }
    }
    let ts_constants = oracle["constants"].as_object().unwrap();
    let rust_constants = rust_constants();
    let rust_constants = rust_constants.as_object().unwrap();
    let mut ts_names: Vec<&String> = ts_constants.keys().collect();
    let mut rust_names: Vec<&String> = rust_constants.keys().collect();
    ts_names.sort();
    rust_names.sort();
    assert_eq!(ts_names, rust_names, "every exported constant is ported");
    for (name, value) in ts_constants {
        if rust_constants.get(name) != Some(value) {
            mismatches.push(format!("constant {name}\n  ts:   {value}\n  rust: {}", rust_constants[name]));
        }
    }
    for (case, rust) in &builders {
        let ts = &oracle[case["id"].as_str().unwrap()];
        if rust != ts {
            mismatches.push(format!("{}\n  input: {case}\n  ts:    {ts}\n  rust:  {rust}", case["id"]));
        }
    }
    println!(
        "compared {} decoder inputs, {} constants and {} builder calls",
        corpus.len(),
        ts_constants.len(),
        builders.len()
    );
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n\n"));
}
