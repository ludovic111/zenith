//! Golden comparison against the TypeScript GitHub pull request provider: the same fake `gh`
//! (`golden/fake_gh.mjs`, answers keyed by substrings of argv and stdin) answers the TS
//! `GitHubPullRequestProvider` (run from source over the real `VcsProcess`,
//! `golden/github_oracle.mjs`) and the Rust one, and for every case
//!
//! - the result (projected onto the neutral provider interface, camelCase, absent vs `null`) or
//!   the `PullRequestProviderError` wire JSON must be identical, and
//! - the `gh` invocations each side made (argv and stdin, in any order) must be identical.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Nested error causes are compared by
//! name below the first level, as in zc-sourcecontrol's golden test.

#![allow(clippy::result_large_err)]
#![recursion_limit = "256"]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_pullrequest::github::cli::GitHubPullRequestCli;
use zc_pullrequest::github::provider::GitHubPullRequestProvider;
use zc_pullrequest::provider::*;
use zc_sourcecontrol::github::GitHubCli;
use zc_sourcecontrol::util::system_clock;

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

// ---------------------------------------------------------------------------------------------
// The fake gh
// ---------------------------------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("zc-github-golden-").tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for sub in ["bin", "cases", "cwd", "log"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/fake_gh.mjs")).unwrap();
        std::fs::write(root.join("fake_gh.mjs"), script.replace("__ROOT__", root.to_str().unwrap())).unwrap();
        let gh = root.join("bin/gh");
        std::fs::write(&gh, format!("#!/bin/sh\nexec node \"{}\" \"$@\"\n", root.join("fake_gh.mjs").display())).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The quota probe GitHubCli sends before `pr list`/`pr view` runs in the process's own
        // directory: answered with nothing to learn.
        std::fs::write(root.join("cases/default.json"), json!([{"when": ["rate_limit"], "stdout": "{}"}]).to_string()).unwrap();
        Self { _dir: dir, root }
    }

    fn add_case(&self, case: &Case) -> String {
        let cwd = self.root.join("cwd").join(&case.id);
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            self.root.join("cases").join(format!("{}.json", case.id)),
            Value::Array(case.gh.clone()).to_string(),
        )
        .unwrap();
        cwd.to_string_lossy().into_owned()
    }

    fn side(&self, side: &str) {
        std::fs::write(self.root.join("side"), side).unwrap();
    }

    /// The invocations one side made for one case, sorted (concurrent reads land in any order).
    fn log(&self, side: &str, case: &str) -> Vec<Value> {
        let mut lines: Vec<Value> = std::fs::read_to_string(self.root.join("log").join(side).join(format!("{case}.jsonl")))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines.sort_by_key(|line| line.to_string());
        lines
    }
}

/// Spawns the fake `gh` for `gh`, everything else for real.
struct FakeGhRunner {
    gh: PathBuf,
}

#[async_trait]
impl ProcessRunner for FakeGhRunner {
    async fn run(&self, mut input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        if input.command == "gh" {
            input.command = self.gh.to_string_lossy().into_owned();
        }
        SystemProcessRunner.run(input).await
    }
}

// ---------------------------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------------------------

struct Case {
    id: String,
    method: &'static str,
    /// The provider input, without `cwd` (each case runs in a directory of its own).
    input: Value,
    gh: Vec<Value>,
}

fn case(id: &str, method: &'static str, input: Value, gh: Vec<Value>) -> Case {
    Case {
        id: format!("case-{id}"),
        method,
        input,
        gh,
    }
}

fn answer(when: &[&str], stdout: impl Into<Value>) -> Value {
    let stdout = match stdout.into() {
        Value::String(text) => text,
        other => other.to_string(),
    };
    json!({"when": when, "stdout": stdout})
}

fn answer_unless(when: &[&str], unless: &[&str], stdout: Value) -> Value {
    json!({"when": when, "unless": unless, "stdout": stdout.to_string()})
}

fn failure(when: &[&str], stderr: &str) -> Value {
    json!({"when": when, "stderr": stderr, "code": 1})
}

fn pr_ref(number: i64) -> Value {
    json!({"repository": "acme/widgets", "host": "github.com", "number": number})
}

fn with(mut base: Value, extra: Value) -> Value {
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        base[key] = value;
    }
    base
}

fn row(number: i64, extra: Value) -> Value {
    with(
        json!({
            "number": number,
            "title": format!("Widget change {number}"),
            "url": format!("https://github.com/acme/widgets/pull/{number}"),
            "author": {"login": format!("author-{number}"), "id": format!("U_{number}"), "name": null},
            "headRefName": format!("feat/{number}"),
            "baseRefName": "main",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "reviewDecision": "REVIEW_REQUIRED",
            "additions": 3,
            "deletions": 1,
            "createdAt": "2026-07-01T00:00:00Z",
            "updatedAt": format!("2026-07-0{}T00:00:00Z", 9 - number.min(8)),
            "reviewRequests": [{"login": "octo-viewer"}, {"slug": "core", "name": "Core"}],
            "labels": [{"name": "bug", "color": "ff0000"}],
            "statusCheckRollup": [{"__typename": "CheckRun", "name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        }),
        extra,
    )
}

fn core(extra: Value, repository: Value) -> Value {
    let pull_request = with(
        json!({
            "number": 7,
            "title": "Widget change 7",
            "url": "https://github.com/acme/widgets/pull/7",
            "body": "Makes widgets wider.",
            "state": "OPEN",
            "isDraft": false,
            "mergeable": "MERGEABLE",
            "reviewDecision": "APPROVED",
            "additions": 12,
            "deletions": 3,
            "changedFiles": 2,
            "createdAt": "2026-07-01T00:00:00Z",
            "updatedAt": "2026-07-02T00:00:00Z",
            "mergedAt": null,
            "closedAt": null,
            "headRefName": "feat/wide",
            "baseRefName": "main",
            "headRefOid": "abc1234",
            "isCrossRepository": true,
            "headRepositoryOwner": {"login": "fork-owner"},
            "author": {"login": "fork-owner", "avatarUrl": null, "id": "U_1", "name": "Fork Owner"},
            "autoMergeRequest": {"mergeMethod": "SQUASH"},
            "viewerCanUpdate": true,
            "viewerDidAuthor": false,
            "viewerCanUpdateBranch": true,
            "baseRef": {"compare": {"behindBy": 2}},
            "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer-one"}}, {"requestedReviewer": {"slug": "core", "name": "Core"}}]},
            "labels": {"nodes": [{"name": "enhancement", "color": "00ff00"}]},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                "nodes": [
                    {"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": "https://github.com/acme/widgets/actions/runs/1/job/2", "checkSuite": {"workflowRun": {"workflow": {"name": "ci"}}}},
                    {"__typename": "StatusContext", "context": "deploy", "state": "PENDING", "targetUrl": "https://deploy.example.test/7", "description": "Deploying"},
                ],
                "pageInfo": {"hasNextPage": false},
            }}}}]},
        }),
        extra,
    );
    let repository = with(
        json!({"mergeCommitAllowed": true, "squashMergeAllowed": true, "rebaseMergeAllowed": false, "viewerPermission": "WRITE"}),
        repository,
    );
    json!({"data": {"repository": with(repository, json!({"pullRequest": pull_request}))}})
}

fn heads_answer() -> Value {
    answer(
        &["pr\u{0}list", "--head"],
        json!([{"number": 7, "headRefOid": "abc1234", "isCrossRepository": true, "headRepositoryOwner": {"login": "fork-owner"}}]),
    )
}

fn runs_answer() -> Value {
    answer(
        &["run\u{0}list"],
        json!([
            {"databaseId": 1, "workflowName": "ci", "url": "https://github.com/acme/widgets/actions/runs/1"},
            {"databaseId": 31, "workflowName": "contributor tests", "url": "https://github.com/acme/widgets/actions/runs/31"},
        ]),
    )
}

fn thread_node(id: &str, comments: &[(&str, &str)], has_next: bool) -> Value {
    json!({
        "id": id,
        "isResolved": false,
        "isOutdated": false,
        "path": "src/widget.ts",
        "line": 4,
        "diffSide": "RIGHT",
        "comments": {
            "totalCount": comments.len() + usize::from(has_next),
            "pageInfo": {"hasNextPage": has_next, "endCursor": if has_next { json!("comments-2") } else { Value::Null }},
            "nodes": comments.iter().map(|(id, login)| json!({
                "id": id,
                "author": {"__typename": "User", "login": login, "avatarUrl": format!("https://avatars.example.test/{login}")},
                "body": format!("{id} says hello"),
                "createdAt": "2026-07-03T00:00:00Z",
                "url": format!("https://github.com/acme/widgets/pull/7#{id}"),
                "reactionGroups": [{"content": "HEART", "viewerHasReacted": true, "reactors": {"totalCount": 1, "nodes": [{"login": "octo-viewer"}]}}],
            })).collect::<Vec<_>>(),
        },
    })
}

fn threads_page(nodes: Vec<Value>, next: Option<&str>, first: bool) -> Value {
    let mut pull_request = json!({
        "reviewThreads": {"totalCount": 3, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}, "nodes": nodes},
        "viewerCanUpdate": false,
        "viewerDidAuthor": false,
    });
    if first {
        pull_request = with(
            pull_request,
            json!({
                "author": {"__typename": "User", "login": "author-7", "avatarUrl": "https://avatars.example.test/author-7"},
                "reactionGroups": [{"content": "THUMBS_UP", "viewerHasReacted": false, "reactors": {"totalCount": 2, "nodes": [{"login": "reviewer-one"}, {"login": "bot-helper"}]}}],
                "comments": {"nodes": [{"id": "IC_1", "author": {"__typename": "Bot", "login": "bot-helper", "avatarUrl": "https://avatars.example.test/bot"}, "reactionGroups": [{"content": "ROCKET", "viewerHasReacted": false, "reactors": {"totalCount": 1, "nodes": [{"login": "author-7"}]}}]}]},
                "reviews": {"nodes": [{"id": "PRR_1", "author": {"__typename": "User", "login": "reviewer-one", "avatarUrl": "https://avatars.example.test/reviewer-one"}}]},
                "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer-two", "name": "Reviewer Two", "avatarUrl": "https://avatars.example.test/reviewer-two"}}]},
                "latestReviews": {"nodes": [{"state": "DISMISSED", "author": {"__typename": "User", "login": "reviewer-one", "avatarUrl": "https://avatars.example.test/reviewer-one"}}]},
                "reviewDismissals": {"pageInfo": {"hasNextPage": true, "endCursor": "dismissals-2"}, "nodes": []},
                "commits": {"nodes": [
                    {"commit": {"oid": "c0ffee1", "messageHeadline": "Widen widgets", "committedDate": "2026-07-01T10:00:00Z", "additions": 10, "deletions": 2, "parents": {"totalCount": 1}, "authors": {"nodes": [{"name": "Author Seven", "avatarUrl": null, "user": {"login": "author-7"}}]}}},
                    {"commit": {"oid": "c0ffee2", "messageHeadline": "Merge main", "committedDate": "2026-07-02T10:00:00Z", "additions": 99, "deletions": 99, "parents": {"totalCount": 2}, "authors": {"nodes": []}}},
                ]},
            }),
        );
    }
    json!({"data": {"viewer": {"login": "octo-viewer"}, "repository": {"pullRequest": pull_request}}})
}

fn stacks(include_titles: bool) -> Value {
    let layer = |number: i64, sha: &str, state: &str, merged: Value| {
        let mut layer = json!({"number": number, "head": {"ref": format!("stack/{number}"), "sha": sha}, "state": state, "merged_at": merged});
        if include_titles {
            layer["title"] = json!(format!("Layer {number}"));
            layer["draft"] = json!(false);
        }
        layer
    };
    json!({
        "id": 42,
        "number": 5,
        "url": "https://api.github.com/repos/acme/widgets/stacks/5",
        "html_url": "https://github.com/acme/widgets/stacks/5",
        "base": {"ref": "main"},
        "pull_requests": [
            layer(6, "aaa1111", "closed", json!("2026-07-01T00:00:00Z")),
            layer(7, "bbb2222", "open", Value::Null),
            layer(8, "ccc3333", "open", Value::Null),
        ],
    })
}

fn node_id_answer(id: &str) -> Value {
    answer(
        &["{ pullRequest(number: $number) { id } }"],
        json!({"data": {"repository": {"pullRequest": {"id": id}}}}),
    )
}

fn subject_answer(owner_id: &str) -> Value {
    answer(
        &["subjectId="],
        json!({"data": {"repository": {"pullRequest": {"id": "PR_7"}}, "node": {"id": "IC_1", "pullRequest": {"id": owner_id}}}}),
    )
}

fn files_page(names: &[&str], next: Option<&str>) -> Value {
    const STATES: [&str; 3] = ["VIEWED", "DISMISSED", "UNVIEWED"];
    let nodes: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(index, name)| json!({"path": name, "viewerViewedState": STATES[index % 3]}))
        .collect();
    json!({"data": {"repository": {"pullRequest": {"files": {
        "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
        "nodes": nodes,
    }}}}})
}

fn cases() -> Vec<Case> {
    let auth = || {
        vec![
            answer(&["auth\u{0}token"], "golden-token\n"),
            answer(&["api\u{0}user"], json!({"id": 4242, "login": "octo-viewer"})),
        ]
    };
    let list_input = |extra: Value| {
        with(
            json!({"repository": "acme/widgets", "host": "github.com", "state": "open", "involvement": "all", "viewer": "octo-viewer", "limit": 2}),
            extra,
        )
    };
    let comparison = answer(
        &["headRef=fork-owner:feat/wide"],
        json!({"data": {"repository": {"pullRequest": {"viewerCanUpdateBranch": true, "baseRef": {"compare": {"behindBy": 2}}}}}}),
    );
    let permissions = answer_unless(
        &["viewerDidAuthor"],
        &["headRef=", "reviewThreads"],
        json!({"data": {"repository": {"mergeCommitAllowed": true, "squashMergeAllowed": false, "rebaseMergeAllowed": true, "viewerPermission": "TRIAGE", "pullRequest": {"viewerCanUpdate": true, "viewerDidAuthor": true}}}}),
    );
    vec![
        case("viewer", "getViewer", json!({"host": "github.com"}), auth()),
        case("routing", "getRoutingIdentity", json!({"host": "GitHub.com"}), auth()),
        case(
            "list",
            "listChangeRequests",
            list_input(json!({})),
            vec![
                answer(
                    &["pr\u{0}list", "--search"],
                    json!([
                        row(1, json!({})),
                        row(2, json!({"isDraft": true, "reviewDecision": null, "statusCheckRollup": []})),
                        row(3, json!({}))
                    ]),
                ),
                answer(
                    &["query PullRequestStackMemberships"],
                    json!({"data": {"s1": {"pullRequest": {"stack": {"number": 5, "size": 3, "baseRefName": "main"}, "stackEntry": {"position": 2}}}}}),
                ),
                answer(
                    &["nodes(ids: $ids)"],
                    json!({"data": {"nodes": [{"login": "author-1", "avatarUrl": "https://avatars.example.test/author-1"}, null]}}),
                ),
            ],
        ),
        case(
            "list-fallback",
            "listChangeRequests",
            list_input(json!({"host": "ghe.example.test", "state": "closed", "involvement": "reviewing", "limit": 5})),
            vec![
                answer(&["pr\u{0}list", "--search"], "[]"),
                answer(
                    &["pr\u{0}list"],
                    json!([
                        row(1, json!({"state": "CLOSED", "author": {"login": "dependabot[bot]", "id": "B_1"}})),
                        row(2, json!({"state": "MERGED", "mergedAt": "2026-07-03T00:00:00Z"})),
                        row(3, json!({"state": "CLOSED", "reviewRequests": [{"login": "someone-else"}]})),
                    ]),
                ),
                failure(&["nodes(ids: $ids)"], "HTTP 502: Bad Gateway"),
            ],
        ),
        case(
            "list-query",
            "listChangeRequests",
            list_input(json!({
                "query": "wider \"widgets\"",
                "cursor": {"updatedBefore": "2026-07-02T00:00:00Z", "delivered": 2},
                "filters": {"draft": "hide", "review": "approved", "checks": "passing", "labels": [["bug", "size:S"]], "excludedLabels": ["wip"], "author": "me"},
            })),
            vec![answer(&["pr\u{0}list"], "[]")],
        ),
        case(
            "across",
            "listChangeRequestsAcross",
            json!({"host": "github.com", "repositories": ["acme/widgets", "acme/gadgets"], "state": "merged", "involvement": "authored", "viewer": "octo-viewer", "limit": 2, "query": "speed"}),
            vec![answer(
                &["search(query: $q"],
                json!({"data": {"search": {"pageInfo": {"hasNextPage": false}, "nodes": [
                    {"number": 4, "title": "Faster gadgets", "url": "https://github.com/acme/gadgets/pull/4", "author": {"__typename": "User", "login": "octo-viewer", "avatarUrl": "https://avatars.example.test/octo-viewer", "name": "Octo"}, "headRefName": "speed", "baseRefName": "main", "state": "MERGED", "isDraft": false, "mergeable": "UNKNOWN", "reviewDecision": "APPROVED", "createdAt": "2026-06-01T00:00:00Z", "updatedAt": "2026-07-05T00:00:00Z", "mergedAt": "2026-07-05T00:00:00Z", "repository": {"nameWithOwner": "acme/gadgets"}, "reviewRequests": {"nodes": []}, "labels": {"nodes": []}, "stack": {"number": 2, "size": 2, "baseRefName": "main"}, "stackEntry": {"position": 1}},
                    {"number": 9, "title": "Faster widgets", "url": "https://github.com/acme/widgets/pull/9", "author": {"__typename": "User", "login": "octo-viewer", "avatarUrl": null}, "headRefName": "speed", "baseRefName": "main", "state": "MERGED", "isDraft": false, "mergeable": "UNKNOWN", "createdAt": "2026-06-01T00:00:00Z", "updatedAt": "2026-07-04T00:00:00Z", "repository": {"nameWithOwner": "acme/widgets"}, "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}},
                    {},
                    {"number": 10, "title": "Third", "url": "https://github.com/acme/widgets/pull/10", "headRefName": "x", "baseRefName": "main", "createdAt": "2026-06-01T00:00:00Z", "updatedAt": "2026-07-03T00:00:00Z", "repository": {"nameWithOwner": "acme/widgets"}},
                ]}}}),
            )],
        ),
        case(
            "across-unaddressable",
            "listChangeRequestsAcross",
            json!({"host": "github.com", "repositories": ["acme/widgets is:merged"], "state": "open", "involvement": "all", "viewer": "octo-viewer", "limit": 2}),
            vec![],
        ),
        case(
            "stats",
            "listChangeRequestStats",
            json!({"host": "github.com", "changeRequests": [{"repository": "acme/widgets", "number": 1}, {"repository": "acme/gadgets", "number": 4}, {"repository": "acme/widgets", "number": 9}]}),
            vec![answer(
                &["s0: repository"],
                json!({"data": {"s0": {"pullRequest": {"additions": 4, "deletions": 1}}, "s1": null, "s2": {"pullRequest": {"additions": 0, "deletions": 7}}}}),
            )],
        ),
        case(
            "detail",
            "getChangeRequest",
            pr_ref(7),
            vec![answer(&["headRef=refs/pull/7/head"], core(json!({}), json!({}))), heads_answer(), runs_answer()],
        ),
        case(
            "detail-merged",
            "getChangeRequest",
            pr_ref(7),
            vec![answer(
                &["headRef=refs/pull/7/head"],
                core(
                    json!({"state": "MERGED", "mergedAt": "2026-07-04T00:00:00Z", "closedAt": "2026-07-04T00:00:00Z", "isCrossRepository": false, "headRepositoryOwner": {"login": "acme"}, "baseRef": null, "autoMergeRequest": null}),
                    json!({"viewerPermission": "READ"}),
                ),
            )],
        ),
        case(
            "detail-unknown-head",
            "getChangeRequest",
            pr_ref(7),
            vec![answer(
                &["headRef=refs/pull/7/head"],
                core(json!({"headRepositoryOwner": null, "viewerDidAuthor": true}), json!({})),
            )],
        ),
        case(
            "detail-checks-paged",
            "getChangeRequest",
            pr_ref(7),
            vec![
                answer(
                    &["headRef=refs/pull/7/head"],
                    core(
                        json!({"isCrossRepository": false, "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                            "nodes": [{"__typename": "CheckRun", "name": "first", "status": "COMPLETED", "conclusion": "SUCCESS"}],
                            "pageInfo": {"hasNextPage": true},
                        }}}}]}}),
                        json!({}),
                    ),
                ),
                answer(
                    &["pr\u{0}view"],
                    row(
                        7,
                        json!({"headRefOid": "abc1234", "body": "", "changedFiles": 2, "closedAt": null, "isCrossRepository": false, "statusCheckRollup": [
                            {"__typename": "CheckRun", "name": "first", "status": "COMPLETED", "conclusion": "SUCCESS"},
                            {"__typename": "CheckRun", "name": "last", "status": "COMPLETED", "conclusion": "FAILURE", "detailsUrl": "https://ci.example.test/last"},
                        ]}),
                    ),
                ),
            ],
        ),
        case(
            "detail-unreadable",
            "getChangeRequest",
            pr_ref(7),
            vec![answer(&["headRef=refs/pull/7/head"], json!({"message": "not found"}))],
        ),
        case(
            "summary",
            "getChangeRequestSummary",
            pr_ref(7),
            vec![answer(
                &["query PullRequestSummaries"],
                json!({"data": {"s0": {"pullRequest": {
                    "number": 7, "title": "Widget change 7", "url": "https://github.com/acme/widgets/pull/7",
                    "author": {"__typename": "User", "login": "author-7", "avatarUrl": null, "name": "Author Seven"},
                    "baseRefName": "main", "headRefName": "feat/7", "state": "OPEN", "isDraft": true, "mergeable": "CONFLICTING",
                    "reviewDecision": null, "latestReviews": {"nodes": [{"state": "CHANGES_REQUESTED", "author": {"login": "reviewer-one"}}]},
                    "additions": 12, "deletions": 3, "changedFiles": 2, "updatedAt": "2026-08-24T12:34:56.000Z", "mergedAt": null, "closedAt": null,
                    "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING"}}}]},
                }}}}),
            )],
        ),
        case(
            "summary-fallback",
            "getChangeRequestSummary",
            pr_ref(7),
            vec![
                answer(&["query PullRequestSummaries"], json!({"data": {"s0": {"pullRequest": null}}})),
                answer(
                    &["pr\u{0}view"],
                    row(
                        7,
                        json!({"body": "", "changedFiles": 2, "closedAt": null, "isCrossRepository": false, "headRepositoryOwner": {"login": "acme"}, "headRefOid": "abc1234"}),
                    ),
                ),
            ],
        ),
        case(
            "preview",
            "getChangeRequestPreview",
            pr_ref(7),
            vec![answer(
                &["isDraft createdAt"],
                json!({"data": {"repository": {"pullRequest": {"number": 7, "title": "Widget change 7", "url": "https://github.com/acme/widgets/pull/7", "state": "CLOSED", "isDraft": false, "createdAt": "2026-07-01T00:00:00Z", "author": {"login": "author-7", "name": "Author Seven", "avatarUrl": "https://avatars.example.test/author-7"}}}}}),
            )],
        ),
        case(
            "preview-rate-limited",
            "getChangeRequestPreview",
            pr_ref(7),
            vec![failure(&["api\u{0}graphql"], "HTTP 403: API rate limit exceeded for user ID 1")],
        ),
        case(
            "stack",
            "getChangeRequestStack",
            with(pr_ref(7), json!({"includeDetails": true})),
            vec![
                answer(&["stacks?pull_request=7"], json!([stacks(false)])),
                answer(&["repos/acme/widgets/stacks/5"], stacks(true)),
            ],
        ),
        case(
            "stack-none",
            "getChangeRequestStack",
            pr_ref(7),
            vec![failure(&["stacks?pull_request=7"], "GraphQL: pull request not found")],
        ),
        case(
            "activity",
            "getChangeRequestActivity",
            pr_ref(7),
            vec![
                answer(
                    &["author,comments,reviews,commits"],
                    json!({
                        "author": {"login": "author-7"},
                        "comments": [{"id": "IC_1", "author": {"login": "bot-helper"}, "body": "Thanks!", "createdAt": "2026-07-02T00:00:00Z", "url": "https://github.com/acme/widgets/pull/7#issuecomment-1"}],
                        "reviews": [
                            {"id": "PRR_1", "author": {"login": "reviewer-one"}, "body": "<!-- marker -->", "state": "DISMISSED", "submittedAt": "2026-07-04T00:00:00Z"},
                            {"id": "PRR_2", "author": {"login": "reviewer-two"}, "body": "Nice.", "state": "APPROVED", "submittedAt": "2026-07-01T12:00:00Z"},
                        ],
                        "commits": [{"oid": "c0ffee0", "messageHeadline": "Old", "committedDate": "2026-06-30T00:00:00Z", "authors": [{"login": "author-7", "name": "Author Seven"}]}],
                    }),
                ),
                answer(
                    &["reviewThreads(first:", "cursor=page-2"],
                    threads_page(vec![thread_node("PRRT_2", &[("RC_3", "reviewer-two")], false)], None, false),
                ),
                answer(
                    &["reviewThreads(first:"],
                    threads_page(
                        vec![thread_node("PRRT_1", &[("RC_1", "reviewer-one"), ("RC_2", "author-7")], true)],
                        Some("page-2"),
                        true,
                    ),
                ),
                answer(
                    &["REVIEW_DISMISSED_EVENT", "cursor=dismissals-2"],
                    json!({"data": {"repository": {"pullRequest": {"timelineItems": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [{"dismissalMessage": "Re-evaluating after the rewrite", "review": {"id": "PRR_1"}}]}}}}}),
                ),
            ],
        ),
        case(
            "thread-comments",
            "getReviewThreadComments",
            with(pr_ref(7), json!({"threadId": "PRRT_1", "cursor": "comments-2"})),
            vec![answer(
                &["threadId=PRRT_1"],
                json!({"data": {"viewer": {"login": "octo-viewer"}, "repository": {"pullRequest": {"id": "PR_7"}}, "node": {"pullRequest": {"id": "PR_7"}, "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": [{"id": "RC_9", "author": {"__typename": "User", "login": "reviewer-one", "avatarUrl": null}, "body": "Later", "createdAt": "2026-07-05T00:00:00Z", "url": null}]}}}}),
            )],
        ),
        case(
            "thread-foreign",
            "getReviewThreadComments",
            with(pr_ref(7), json!({"threadId": "PRRT_9", "cursor": "comments-2"})),
            vec![answer(
                &["threadId=PRRT_9"],
                json!({"data": {"repository": {"pullRequest": {"id": "PR_7"}}, "node": {"pullRequest": {"id": "PR_8"}, "comments": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []}}}}),
            )],
        ),
        case(
            "viewer-permissions",
            "getViewerPermissions",
            pr_ref(7),
            vec![
                answer(&["headRef=refs/pull/7/head"], core(json!({}), json!({}))),
                comparison.clone(),
                permissions.clone(),
            ],
        ),
        case(
            "viewer-permissions-unrelated",
            "getViewerPermissions",
            with(pr_ref(7), json!({"includeUpdateBranch": false})),
            vec![permissions.clone()],
        ),
        case(
            "diff",
            "getDiff",
            pr_ref(7),
            vec![answer(
                &["pr\u{0}diff"],
                "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1 @@\n-old\n+new\n",
            )],
        ),
        case(
            "diff-refused",
            "getDiff",
            pr_ref(7),
            vec![
                failure(&["pr\u{0}diff"], "HTTP 406: Sorry, the diff exceeded the maximum number of files (300)."),
                answer(
                    &["pulls/7/files?per_page=100&page=1"],
                    json!([
                        {"filename": "src/a.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new", "additions": 1, "deletions": 1},
                        {"filename": "assets/logo.png", "status": "added", "additions": 0, "deletions": 0},
                        {"filename": "src/big.ts", "status": "modified", "additions": 4000, "deletions": 12},
                        {"filename": "src/b c.ts", "previous_filename": "src/b.ts", "status": "renamed", "patch": "@@ -2 +2 @@\n-x\n+y", "additions": 1, "deletions": 1},
                    ]),
                ),
            ],
        ),
        case(
            "diff-commit",
            "getDiff",
            with(pr_ref(7), json!({"commit": "abcdef1", "cursor": "2"})),
            vec![answer(
                &["commits/abcdef1?per_page=100&page=2"],
                json!([{"filename": "src/a.ts", "status": "removed", "patch": "@@ -1 +0,0 @@\n-gone"}]),
            )],
        ),
        case("diff-bad-cursor", "getDiff", with(pr_ref(7), json!({"cursor": "1&per_page=1"})), vec![]),
        case(
            "file-contents",
            "getDiffFileContents",
            with(
                pr_ref(7),
                json!({"changeType": "rename-changed", "oldPath": "src/b.ts", "newPath": "src/b c.ts"}),
            ),
            vec![
                answer(&["repos/acme/widgets/pulls/7\u{0}--jq"], "abc1234\tdef5678\n"),
                answer(&["contents/src/b.ts?ref=abc1234"], "old b\n"),
                answer(&["contents/src/b%20c.ts?ref=def5678"], "new b\n"),
            ],
        ),
        case(
            "file-contents-root",
            "getDiffFileContents",
            with(
                pr_ref(7),
                json!({"commit": "abcdef1", "changeType": "new", "oldPath": "src/root.ts", "newPath": "src/root.ts"}),
            ),
            vec![
                answer(&["commits/abcdef1\u{0}--jq"], "\tabcdef1\n"),
                answer(&["contents/src/root.ts?ref=abcdef1"], "root\n"),
            ],
        ),
        case(
            "file-contents-binary",
            "getDiffFileContents",
            with(
                pr_ref(7),
                json!({"changeType": "deleted", "oldPath": "assets/logo.png", "newPath": "assets/logo.png"}),
            ),
            vec![
                answer(&["repos/acme/widgets/pulls/7\u{0}--jq"], "abc1234\tdef5678\n"),
                answer(&["contents/assets/logo.png"], "PNG\u{0}data"),
            ],
        ),
        case(
            "files-viewed",
            "getFilesViewed",
            pr_ref(7),
            vec![
                answer(&["after=cursor-1"], files_page(&["src/d.ts"], None)),
                answer(&["viewerViewedState"], files_page(&["src/a.ts", "src/b.ts", "src/c.ts"], Some("cursor-1"))),
            ],
        ),
        case(
            "set-files-viewed",
            "setFilesViewed",
            with(
                pr_ref(7),
                json!({"files": [{"path": "src/a.ts", "viewed": true}, {"path": "src/b c.ts", "viewed": false}]}),
            ),
            vec![node_id_answer("PR_7"), answer(&["markFileAsViewed"], "{}")],
        ),
        case(
            "comment",
            "comment",
            with(pr_ref(7), json!({"body": "Looks good — ship it.\n\n`code`"})),
            vec![answer(&["pr\u{0}comment"], "")],
        ),
        case(
            "comment-unauthenticated",
            "comment",
            with(pr_ref(7), json!({"body": "Hello"})),
            vec![failure(&["pr\u{0}comment"], "To get started with GitHub CLI, please run:  gh auth login")],
        ),
        case(
            "review",
            "submitReview",
            with(
                pr_ref(7),
                json!({"verdict": "request-changes", "body": "A few things.", "comments": [
                    {"path": "src/a.ts", "position": {"kind": "added", "newLine": 4}, "body": "nit"},
                    {"path": "src/a.ts", "position": {"kind": "deleted", "oldLine": 2}, "body": "why?"},
                    {"path": "src/b.ts", "position": {"kind": "context", "oldLine": 3, "newLine": 5, "side": "left"}, "body": "hm"},
                ]}),
            ),
            vec![answer(&["pulls/7/reviews"], "{}")],
        ),
        case(
            "reaction-subject",
            "setReaction",
            with(pr_ref(7), json!({"subjectId": "IC_1", "content": "thumbs-up", "reacted": true})),
            vec![subject_answer("PR_7"), answer(&["addReaction"], "{}")],
        ),
        case(
            "reaction-foreign",
            "setReaction",
            with(pr_ref(7), json!({"subjectId": "IC_1", "content": "eyes", "reacted": true})),
            vec![subject_answer("PR_8")],
        ),
        case(
            "reaction-pull-request",
            "setReaction",
            with(pr_ref(7), json!({"content": "hooray", "reacted": false})),
            vec![node_id_answer("PR_7"), answer(&["removeReaction"], "{}")],
        ),
        case(
            "labels-add",
            "setLabels",
            with(pr_ref(7), json!({"labels": ["bug", "size:XL"], "applied": true})),
            vec![answer(&["issues/7/labels"], "[]")],
        ),
        case(
            "labels-remove",
            "setLabels",
            with(pr_ref(7), json!({"labels": ["good first issue", "area/web"], "applied": false})),
            vec![answer(&["issues/7/labels/"], "[]")],
        ),
        case(
            "reviewers",
            "setReviewerRequest",
            with(
                pr_ref(7),
                json!({"reviewers": [{"id": "reviewer-one", "kind": "user"}, {"id": "core", "kind": "team"}], "requested": true}),
            ),
            vec![answer(&["requested_reviewers"], "{}")],
        ),
        case(
            "reviewer-candidates",
            "listReviewerCandidates",
            pr_ref(7),
            vec![answer(
                &["assignableUsers"],
                json!({"data": {"repository": {
                    "assignableUsers": {"pageInfo": {"hasNextPage": true}, "nodes": [{"login": "author-7", "name": "Author Seven", "avatarUrl": null}, {"login": "reviewer-one", "name": null, "avatarUrl": "https://avatars.example.test/reviewer-one"}, {"login": "reviewer-two"}]},
                    "pullRequest": {"author": {"login": "author-7"}, "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "reviewer-two"}}, {"requestedReviewer": {"slug": "core", "name": "Core"}}]}},
                }}}),
            )],
        ),
        case(
            "label-candidates",
            "listLabelCandidates",
            pr_ref(7),
            vec![answer(
                &["labels(first:"],
                json!({"data": {"repository": {
                    "labels": {"pageInfo": {"hasNextPage": false}, "nodes": [{"name": "bug", "color": "ff0000", "description": "Broken"}, {"name": "docs", "color": null, "description": null}]},
                    "pullRequest": {"labels": {"nodes": [{"name": "bug"}]}},
                }}}),
            )],
        ),
        case(
            "action-merge",
            "runAction",
            with(pr_ref(7), json!({"action": "merge", "mergeMethod": "squash"})),
            vec![answer(&["pr\u{0}merge"], "")],
        ),
        case(
            "action-update-branch",
            "runAction",
            with(pr_ref(7), json!({"action": "update-branch", "updateMethod": "rebase"})),
            vec![answer(&["pr\u{0}update-branch"], "")],
        ),
        case(
            "action-draft",
            "runAction",
            with(pr_ref(7), json!({"action": "draft"})),
            vec![answer(&["pr\u{0}ready"], "")],
        ),
        case(
            "action-revert",
            "runAction",
            with(pr_ref(7), json!({"action": "revert"})),
            vec![node_id_answer("PR_7"), answer(&["revertPullRequest"], "{}")],
        ),
        case(
            "action-approve-workflows",
            "runAction",
            with(pr_ref(7), json!({"action": "approve-workflows"})),
            vec![
                answer(&["headRef=refs/pull/7/head"], core(json!({}), json!({}))),
                heads_answer(),
                runs_answer(),
                answer(&["/approve"], ""),
            ],
        ),
        case(
            "action-stack-merge",
            "runAction",
            with(
                pr_ref(7),
                json!({"action": "merge", "stackNumber": 5, "expectedStackHeads": [{"number": 7, "headSha": "bbb2222"}], "mergeMethod": "rebase"}),
            ),
            vec![
                answer(&["stacks?pull_request=7"], json!([stacks(false)])),
                answer(&["merge-async"], json!({"status": "merged", "details": {}})),
            ],
        ),
        case(
            "action-stack-changed",
            "runAction",
            with(
                pr_ref(8),
                json!({"action": "update-branch", "stackNumber": 5, "expectedStackHeads": [{"number": 7, "headSha": "old"}, {"number": 8, "headSha": "ccc3333"}]}),
            ),
            vec![answer(&["stacks?pull_request=8"], json!([stacks(false)]))],
        ),
        case(
            "reply",
            "replyToThread",
            with(pr_ref(7), json!({"threadId": "PRRT_1", "body": "Fixed in c0ffee1."})),
            vec![answer(&["addPullRequestReviewThreadReply"], "{}")],
        ),
        case(
            "resolve",
            "setThreadResolution",
            with(pr_ref(7), json!({"threadId": "PRRT_1", "resolved": true})),
            vec![answer(&["resolveReviewThread"], "{}")],
        ),
        case(
            "update-change-request",
            "updateChangeRequest",
            with(pr_ref(7), json!({"title": "Wider widgets", "body": "Now with \"quotes\" and ünïcode."})),
            vec![node_id_answer("PR_7"), answer(&["updatePullRequest("], "{}")],
        ),
        case(
            "update-comment",
            "updateComment",
            with(pr_ref(7), json!({"commentId": "IC_1", "kind": "review-comment", "body": "Reworded."})),
            vec![subject_answer("PR_7"), answer(&["updatePullRequestReviewComment"], "{}")],
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The Rust side: inputs from the case JSON, results in the TS shape
// ---------------------------------------------------------------------------------------------

fn field<T: DeserializeOwned>(input: &Value, key: &str) -> T {
    serde_json::from_value(input[key].clone()).unwrap_or_else(|error| panic!("{key}: {error}"))
}

fn optional<T: DeserializeOwned>(input: &Value, key: &str) -> Option<T> {
    input
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()).unwrap())
}

fn change_request_ref(input: &Value) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: field(input, "cwd"),
        repository: field(input, "repository"),
        host: field(input, "host"),
        number: field(input, "number"),
    }
}

fn cursor(input: &Value) -> Option<ProviderListCursor> {
    input.get("cursor").map(|cursor| ProviderListCursor {
        updated_before: field(cursor, "updatedBefore"),
        delivered: field(cursor, "delivered"),
    })
}

/// `Option<Option<T>>` as a TS optional nullable key: absent, `null` or the value.
fn put<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: &Option<Option<T>>) {
    if let Some(value) = value {
        map.insert(key.into(), serde_json::to_value(value).unwrap());
    }
}

/// An optional key.
fn put_some<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), serde_json::to_value(value).unwrap());
    }
}

fn to<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn change_request(change_request: &ProviderChangeRequest) -> Map<String, Value> {
    let mut map = Map::new();
    put_some(&mut map, "stack", &change_request.stack);
    map.insert("number".into(), to(&change_request.number));
    map.insert("title".into(), to(&change_request.title));
    map.insert("url".into(), to(&change_request.url));
    map.insert("author".into(), to(&change_request.author));
    map.insert("headBranch".into(), to(&change_request.head_branch));
    put(&mut map, "headRepositoryNameWithOwner", &change_request.head_repository_name_with_owner);
    map.insert("baseBranch".into(), to(&change_request.base_branch));
    map.insert("state".into(), to(&change_request.state));
    map.insert("isDraft".into(), to(&change_request.is_draft));
    map.insert("mergeability".into(), to(&change_request.mergeability));
    map.insert("additions".into(), to(&change_request.additions));
    map.insert("deletions".into(), to(&change_request.deletions));
    map.insert("createdAt".into(), to(&change_request.created_at));
    put(&mut map, "closedAt", &change_request.closed_at);
    put(&mut map, "mergedAt", &change_request.merged_at);
    map.insert("updatedAt".into(), to(&change_request.updated_at));
    map.insert("reviewRequestLogins".into(), to(&change_request.review_request_logins));
    map.insert("labels".into(), to(&change_request.labels));
    put(&mut map, "reviewDecision", &change_request.review_decision);
    put(&mut map, "checksState", &change_request.checks_state);
    map
}

fn detail(detail: &ProviderChangeRequestDetail) -> Value {
    let mut map = change_request(&detail.change_request);
    map.insert("body".into(), to(&detail.body));
    map.insert("changedFiles".into(), to(&detail.changed_files));
    map.insert("mergedAt".into(), to(&detail.merged_at));
    map.insert("closedAt".into(), to(&detail.closed_at));
    map.insert("reviewers".into(), to(&detail.reviewers));
    map.insert("checks".into(), to(&detail.checks));
    map.insert("mergeCapabilities".into(), to(&detail.merge_capabilities));
    map.insert("viewerPermissions".into(), to(&detail.viewer_permissions));
    put_some(&mut map, "baseComparison", &detail.base_comparison);
    put_some(&mut map, "behindBy", &detail.behind_by);
    put_some(&mut map, "autoMergeEnabled", &detail.auto_merge_enabled);
    put_some(&mut map, "autoMergeMethod", &detail.auto_merge_method);
    put_some(&mut map, "workflowApprovalsRequired", &detail.workflow_approvals_required);
    Value::Object(map)
}

fn summary(summary: &ProviderChangeRequestSummary) -> Value {
    let mut map = Map::new();
    map.insert("number".into(), to(&summary.number));
    map.insert("title".into(), to(&summary.title));
    map.insert("url".into(), to(&summary.url));
    map.insert("headBranch".into(), to(&summary.head_branch));
    map.insert("baseBranch".into(), to(&summary.base_branch));
    map.insert("state".into(), to(&summary.state));
    put_some(&mut map, "isDraft", &summary.is_draft);
    put(&mut map, "closedAt", &summary.closed_at);
    put(&mut map, "mergedAt", &summary.merged_at);
    map.insert("updatedAt".into(), to(&summary.updated_at));
    put(&mut map, "author", &summary.author);
    put_some(&mut map, "additions", &summary.additions);
    put_some(&mut map, "deletions", &summary.deletions);
    put_some(&mut map, "changedFiles", &summary.changed_files);
    put(&mut map, "reviewDecision", &summary.review_decision);
    put(&mut map, "checksState", &summary.checks_state);
    put_some(&mut map, "mergeability", &summary.mergeability);
    Value::Object(map)
}

fn stack(stack: &Option<ProviderChangeRequestStack>) -> Value {
    let Some(stack) = stack else {
        return Value::Null;
    };
    let layers: Vec<Value> = stack
        .layers
        .iter()
        .map(|layer| {
            let mut map = Map::new();
            put_some(&mut map, "title", &layer.title);
            put_some(&mut map, "isDraft", &layer.is_draft);
            put_some(&mut map, "headSha", &layer.head_sha);
            map.insert("number".into(), to(&layer.number));
            map.insert("headBranch".into(), to(&layer.head_branch));
            map.insert("state".into(), to(&layer.state));
            Value::Object(map)
        })
        .collect();
    json!({"id": stack.id, "number": stack.number, "url": stack.url, "base": stack.base, "layers": layers})
}

fn activity(activity: &ProviderChangeRequestActivity) -> Value {
    let mut map = Map::new();
    put(&mut map, "author", &activity.author);
    put_some(&mut map, "reviewers", &activity.reviewers);
    map.insert("comments".into(), to(&activity.comments));
    map.insert("commentCount".into(), to(&activity.comment_count));
    map.insert("commentsTruncated".into(), to(&activity.comments_truncated));
    map.insert("reviewThreads".into(), to(&activity.review_threads));
    map.insert("commits".into(), to(&activity.commits));
    put_some(&mut map, "reactions", &activity.reactions);
    Value::Object(map)
}

fn outcome<T>(result: ProviderResult<T>, shape: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({"ok": shape(value)}),
        Err(error) => json!({"error": error.to_wire()}),
    }
}

async fn run_rust(provider: &GitHubPullRequestProvider, method: &str, input: &Value) -> Value {
    let cwd: String = field(input, "cwd");
    match method {
        "getViewer" => outcome(
            provider
                .get_viewer(ProviderHostRef {
                    cwd,
                    host: optional(input, "host"),
                })
                .await,
            |viewer| json!(viewer),
        ),
        "getRoutingIdentity" => outcome(
            provider.get_routing_identity(&cwd, &field::<String>(input, "host")).await,
            |identity| json!({"accountId": identity.account_id, "viewer": identity.viewer}),
        ),
        "listChangeRequests" => outcome(
            provider
                .list_change_requests(ListChangeRequestsInput {
                    cwd,
                    repository: field(input, "repository"),
                    host: field(input, "host"),
                    state: field(input, "state"),
                    involvement: field(input, "involvement"),
                    viewer: field(input, "viewer"),
                    limit: field(input, "limit"),
                    query: optional(input, "query"),
                    cursor: cursor(input),
                    filters: optional(input, "filters"),
                })
                .await,
            |page| {
                let mut map = Map::new();
                map.insert(
                    "items".into(),
                    Value::Array(page.items.iter().map(|item| Value::Object(change_request(item))).collect()),
                );
                map.insert("truncated".into(), json!(page.truncated));
                put_some(&mut map, "cursorAdvance", &page.cursor_advance);
                map.insert("continues".into(), json!(page.continues));
                Value::Object(map)
            },
        ),
        "listChangeRequestsAcross" => outcome(
            provider
                .list_change_requests_across(ListChangeRequestsAcrossInput {
                    cwd,
                    host: field(input, "host"),
                    repositories: field(input, "repositories"),
                    state: field(input, "state"),
                    involvement: field(input, "involvement"),
                    viewer: field(input, "viewer"),
                    limit: field(input, "limit"),
                    query: optional(input, "query"),
                    cursor: cursor(input),
                    filters: optional(input, "filters"),
                })
                .await,
            |page| {
                let items: Vec<Value> = page
                    .items
                    .iter()
                    .map(|item| {
                        let mut map = change_request(&item.change_request);
                        map.insert("repository".into(), json!(item.repository));
                        Value::Object(map)
                    })
                    .collect();
                json!({"truncated": page.truncated, "items": items})
            },
        ),
        "listChangeRequestStats" => outcome(
            provider
                .list_change_request_stats(ListChangeRequestStatsInput {
                    cwd,
                    host: field(input, "host"),
                    change_requests: input["changeRequests"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|change_request| (field(change_request, "repository"), field(change_request, "number")))
                        .collect(),
                })
                .await,
            |stats| {
                Value::Array(
                    stats
                        .iter()
                        .map(|stat| json!({"repository": stat.repository, "number": stat.number, "additions": stat.additions, "deletions": stat.deletions}))
                        .collect(),
                )
            },
        ),
        "getChangeRequest" => outcome(provider.get_change_request(change_request_ref(input)).await, |value| detail(&value)),
        "getChangeRequestPreview" => outcome(
            provider.get_change_request_preview(change_request_ref(input)).await,
            |preview| json!({"number": preview.number, "title": preview.title, "url": preview.url, "author": preview.author, "state": preview.state, "isDraft": preview.is_draft, "createdAt": preview.created_at}),
        ),
        "getChangeRequestSummary" => outcome(provider.get_change_request_summary(change_request_ref(input)).await, |value| summary(&value)),
        "getChangeRequestStack" => outcome(
            provider
                .get_change_request_stack(GetChangeRequestStackInput {
                    change_request: change_request_ref(input),
                    include_details: optional(input, "includeDetails"),
                })
                .await,
            |value| stack(&value),
        ),
        "getChangeRequestActivity" => outcome(provider.get_change_request_activity(change_request_ref(input)).await, |value| activity(&value)),
        "getReviewThreadComments" => outcome(
            provider
                .get_review_thread_comments(ReviewThreadCommentsInput {
                    change_request: change_request_ref(input),
                    thread_id: field(input, "threadId"),
                    cursor: field(input, "cursor"),
                })
                .await,
            |value| to(&value),
        ),
        "getViewerPermissions" => outcome(
            provider
                .get_viewer_permissions(ViewerPermissionsInput {
                    change_request: change_request_ref(input),
                    include_update_branch: optional(input, "includeUpdateBranch"),
                })
                .await,
            |value| to(&value),
        ),
        "getDiff" => outcome(
            provider
                .get_diff(GetDiffInput {
                    change_request: change_request_ref(input),
                    cursor: optional(input, "cursor"),
                    commit: optional(input, "commit"),
                })
                .await,
            |slice| {
                let mut map = Map::new();
                map.insert("patch".into(), json!(slice.patch));
                map.insert("truncated".into(), json!(slice.truncated));
                map.insert("nextCursor".into(), json!(slice.next_cursor));
                put_some(&mut map, "omittedFileStats", &slice.omitted_file_stats);
                Value::Object(map)
            },
        ),
        "getDiffFileContents" => outcome(
            provider
                .get_diff_file_contents(DiffFileContentsInput {
                    change_request: change_request_ref(input),
                    commit: optional(input, "commit"),
                    change_type: field(input, "changeType"),
                    old_path: field(input, "oldPath"),
                    new_path: field(input, "newPath"),
                })
                .await,
            |contents| json!({"oldContents": contents.old_contents, "newContents": contents.new_contents}),
        ),
        "getFilesViewed" => outcome(
            provider.get_files_viewed(change_request_ref(input)).await,
            |viewed| json!({"files": viewed.files, "truncated": viewed.truncated}),
        ),
        "setFilesViewed" => outcome(
            provider
                .set_files_viewed(SetFilesViewedInput {
                    change_request: change_request_ref(input),
                    files: input["files"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|file| (field(file, "path"), field(file, "viewed")))
                        .collect(),
                })
                .await,
            |()| Value::Null,
        ),
        "comment" => outcome(
            provider
                .comment(CommentInput {
                    change_request: change_request_ref(input),
                    body: field(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "submitReview" => outcome(
            provider
                .submit_review(SubmitReviewInput {
                    change_request: change_request_ref(input),
                    verdict: field(input, "verdict"),
                    body: field(input, "body"),
                    comments: field(input, "comments"),
                })
                .await,
            |()| Value::Null,
        ),
        "setReaction" => outcome(
            provider
                .set_reaction(SetReactionInput {
                    change_request: change_request_ref(input),
                    subject_id: optional(input, "subjectId"),
                    content: field(input, "content"),
                    reacted: field(input, "reacted"),
                })
                .await,
            |()| Value::Null,
        ),
        "setLabels" => outcome(
            provider
                .set_labels(SetLabelsInput {
                    change_request: change_request_ref(input),
                    labels: field(input, "labels"),
                    applied: field(input, "applied"),
                })
                .await,
            |()| Value::Null,
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
                            id: field(reviewer, "id"),
                            kind: field(reviewer, "kind"),
                        })
                        .collect(),
                    requested: field(input, "requested"),
                })
                .await,
            |()| Value::Null,
        ),
        "listReviewerCandidates" => outcome(provider.list_reviewer_candidates(change_request_ref(input)).await, |value| to(&value)),
        "listLabelCandidates" => outcome(provider.list_label_candidates(change_request_ref(input)).await, |value| to(&value)),
        "runAction" => outcome(
            provider
                .run_action(RunActionInput {
                    change_request: change_request_ref(input),
                    action: field(input, "action"),
                    stack_number: optional(input, "stackNumber"),
                    expected_stack_heads: optional(input, "expectedStackHeads"),
                    merge_method: optional(input, "mergeMethod"),
                    update_method: optional(input, "updateMethod"),
                })
                .await,
            |()| Value::Null,
        ),
        "replyToThread" => outcome(
            provider
                .reply_to_thread(ReplyToThreadInput {
                    change_request: change_request_ref(input),
                    thread_id: field(input, "threadId"),
                    body: field(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "setThreadResolution" => outcome(
            provider
                .set_thread_resolution(SetThreadResolutionInput {
                    change_request: change_request_ref(input),
                    thread_id: field(input, "threadId"),
                    resolved: field(input, "resolved"),
                })
                .await,
            |()| Value::Null,
        ),
        "updateChangeRequest" => outcome(
            provider
                .update_change_request(UpdateChangeRequestInput {
                    change_request: change_request_ref(input),
                    title: optional(input, "title"),
                    body: optional(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "updateComment" => outcome(
            provider
                .update_comment(UpdateCommentInput {
                    change_request: change_request_ref(input),
                    comment_id: field(input, "commentId"),
                    kind: field(input, "kind"),
                    body: field(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        other => panic!("no Rust runner for {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------------------------

/// Keeps the first-level cause's name and message; deeper causes are compared by name (the TS
/// side nests Effect internals there, e.g. a schema `Cause`).
fn normalize(value: &mut Value, depth: usize) {
    if let Value::Object(map) = value {
        if let Some(cause) = map.get_mut("cause") {
            if depth >= 1 {
                let name = if cause.get("_id").and_then(Value::as_str) == Some("Cause") {
                    json!("SchemaError")
                } else {
                    cause.get("name").cloned().unwrap_or(Value::Null)
                };
                *cause = json!({"name": name});
            } else {
                normalize(cause, depth + 1);
            }
        }
    }
}

fn normalized(mut outcome: Value) -> Value {
    if let Some(error) = outcome.get_mut("error") {
        normalize(error, 0);
    }
    outcome
}

fn run_oracle(cases: &[Value], bin: &Path) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/github_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
        .env_remove("GH_HOST")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(json!({"cases": cases}).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_matches_the_typescript_github_provider() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the GitHub golden comparison: {reason}");
        return;
    }
    let fixture = Fixture::new();
    let cases = cases();
    let mut requests = Vec::new();
    for case in &cases {
        let cwd = fixture.add_case(case);
        let mut input = case.input.clone();
        input["cwd"] = json!(cwd);
        requests.push((case, input));
    }
    let oracle_cases: Vec<Value> = requests
        .iter()
        .map(|(case, input)| json!({"id": case.id, "method": case.method, "input": input}))
        .collect();

    fixture.side("ts");
    let ts = run_oracle(&oracle_cases, &fixture.root.join("bin"));

    fixture.side("rust");
    let runner: Arc<dyn ProcessRunner> = Arc::new(FakeGhRunner {
        gh: fixture.root.join("bin/gh"),
    });
    let mut differences = Vec::new();
    let mut compared = 0;
    for (case, input) in &requests {
        // A fresh provider per case, like the oracle: no cache, pause or budget carries over.
        let github = GitHubCli::new(VcsProcess::new(runner.clone()), system_clock());
        let provider = GitHubPullRequestProvider::new(GitHubPullRequestCli::new(github, system_clock()));
        let rust = normalized(run_rust(&provider, case.method, input).await);
        let expected = normalized(ts.get(&case.id).cloned().unwrap_or(Value::Null));
        assert!(expected.get("defect").is_none(), "{}: the TS side died: {expected}", case.id);
        if rust != expected {
            differences.push(format!("{} ({}):\n  rust: {rust}\n  ts:   {expected}", case.id, case.method));
        }
        if std::env::var_os("ZC_GOLDEN_DUMP").is_some() {
            eprintln!(
                "{} ({}): {rust}\n  calls: {}",
                case.id,
                case.method,
                Value::Array(fixture.log("rust", &case.id))
            );
        }
        let (rust_calls, ts_calls) = (fixture.log("rust", &case.id), fixture.log("ts", &case.id));
        if rust_calls != ts_calls {
            differences.push(format!(
                "{} ({}) gh calls differ:\n  rust: {}\n  ts:   {}",
                case.id,
                case.method,
                Value::Array(rust_calls),
                Value::Array(ts_calls)
            ));
        }
        compared += 1;
    }
    assert!(
        differences.is_empty(),
        "{} of {compared} cases differ:\n{}",
        differences.len(),
        differences.join("\n")
    );
    eprintln!("{compared} GitHub provider cases identical (results and gh calls)");
}
