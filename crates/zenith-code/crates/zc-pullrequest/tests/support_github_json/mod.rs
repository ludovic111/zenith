//! Fixtures of `gitHubPullRequestJson.test.ts` (with made-up people, organizations and
//! repositories), shared by the ported tests (`github_json.rs`) and the golden comparison
//! against the TS decoders (`github_json_golden.rs`).

#![allow(dead_code)]

use serde_json::{json, Map, Value};

/// `{ ...base, ...overrides }`.
pub fn with(base: &Value, overrides: Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    for (key, value) in overrides.as_object().cloned().unwrap_or_default() {
        merged.insert(key, value);
    }
    Value::Object(merged)
}

/// `{ ...base, key: undefined }`, which `JSON.stringify` leaves out.
pub fn without(base: &Value, key: &str) -> Value {
    let mut map: Map<String, Value> = base.as_object().cloned().unwrap_or_default();
    map.shift_remove(key);
    Value::Object(map)
}

pub const REPOSITORY: &str = "acme/widgets";
pub const PULL_URL: &str = "https://github.example.test/acme/widgets/pull/1";

/// `listJson`: rows of `gh pr list --json` around the required fields.
pub fn list_json(entries: &[Value]) -> String {
    let base = json!({
        "number": 1,
        "title": "Add the pull requests page",
        "url": PULL_URL,
        "headRefName": "feat/page",
        "baseRefName": "main",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": "2026-07-02T00:00:00Z",
    });
    Value::Array(entries.iter().map(|entry| with(&base, entry.clone())).collect()).to_string()
}

/// `searchJson`: one search row per rollup state (`None` for a head commit with no rollup).
pub fn search_value(rollup_states: &[Option<&str>]) -> Value {
    json!({
        "data": {
            "search": {
                "pageInfo": { "hasNextPage": false },
                "nodes": rollup_states.iter().enumerate().map(|(index, state)| json!({
                    "number": index + 1,
                    "title": "Add the pull requests page",
                    "url": PULL_URL,
                    "headRefName": "feat/page",
                    "baseRefName": "main",
                    "createdAt": "2026-07-01T00:00:00Z",
                    "updatedAt": "2026-07-02T00:00:00Z",
                    "repository": { "nameWithOwner": REPOSITORY },
                    "commits": { "nodes": [{ "commit": { "statusCheckRollup": state.map(|state| json!({ "state": state })) } }] },
                })).collect::<Vec<_>>(),
            },
        },
    })
}

/// The search rows with the first one in a stack.
pub fn search_with_stack() -> String {
    let mut raw = search_value(&[Some("SUCCESS"), None]);
    raw["data"]["search"]["nodes"][0]["stack"] = json!({ "number": 3, "size": 2, "baseRefName": "main" });
    raw["data"]["search"]["nodes"][0]["stackEntry"] = json!({ "position": 1 });
    raw.to_string()
}

/// `detailJson`: a pull request as `gh pr view --json` answers it.
pub fn detail_value() -> Value {
    json!({
        "number": 7,
        "title": "Detail",
        "url": "https://github.example.test/acme/widgets/pull/7",
        "headRefName": "feat/detail",
        "baseRefName": "main",
        "createdAt": "2026-07-01T00:00:00Z",
        "updatedAt": "2026-07-05T00:00:00Z",
        "body": "Body",
        "statusCheckRollup": [
            { "__typename": "CheckRun", "name": "build", "status": "IN_PROGRESS" },
            { "__typename": "CheckRun", "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" },
            { "__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS" },
        ],
        "comments": [{ "id": "c1", "body": "second", "createdAt": "2026-07-04T00:00:00Z" }],
        "reviews": [
            { "id": "r1", "body": "first", "state": "CHANGES_REQUESTED", "submittedAt": "2026-07-03T00:00:00Z" },
            { "id": "r2", "body": "   ", "state": "APPROVED", "submittedAt": "2026-07-06T00:00:00Z" },
        ],
        "commits": [{
            "oid": "abc1234",
            "messageHeadline": "Ship the timeline",
            "committedDate": "2026-07-05T00:00:00Z",
            "authors": [
                { "login": "ada-example", "name": "Ada Example", "email": "ada@example.test" },
                { "name": "Pair Author", "email": "pair@example.test" },
            ],
        }],
    })
}

pub fn detail_json() -> String {
    detail_value().to_string()
}

/// The detail with fields replaced.
pub fn detail_with(overrides: Value) -> String {
    with(&detail_value(), overrides).to_string()
}

/// The first `threadsJson` of the TS tests: threads and their paging.
pub fn threads_page(nodes: Value, total_count: usize, page_info: Value) -> String {
    json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "totalCount": total_count, "pageInfo": page_info, "nodes": nodes } } } } })
        .to_string()
}

pub fn threads_page_default(nodes: Value) -> String {
    let count = nodes.as_array().map_or(0, Vec::len);
    threads_page(nodes, count, json!({ "hasNextPage": false, "endCursor": null }))
}

/// `reviewJson`: the review roster the thread read carries.
pub fn review_roster(requested: Vec<Value>, reviewed: Vec<Value>) -> String {
    json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": { "totalCount": 0, "nodes": [] },
                    "reviewRequests": { "nodes": requested.into_iter().map(|reviewer| json!({ "requestedReviewer": reviewer })).collect::<Vec<_>>() },
                    "latestReviews": { "nodes": reviewed.into_iter().map(|author| json!({ "author": author })).collect::<Vec<_>>() },
                },
            },
        },
    })
    .to_string()
}

/// A thread read carrying only commits.
pub fn commits_page(commits: Value) -> String {
    json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "totalCount": 0, "nodes": [] }, "commits": { "nodes": commits } } } } }).to_string()
}

/// The second `threadsJson` of the TS tests: a whole pull request around the threads.
pub fn threads_with(nodes: Value, pull_request: Value) -> String {
    let count = nodes.as_array().map_or(0, Vec::len);
    let base = json!({
        "reviewThreads": { "totalCount": count, "nodes": nodes },
        "author": null,
        "comments": { "nodes": [] },
        "reviewRequests": { "nodes": [] },
        "latestReviews": { "nodes": [] },
    });
    json!({ "data": { "repository": { "pullRequest": with(&base, pull_request) } } }).to_string()
}

/// `comment(id, body)` of the second thread suite.
pub fn thread_comment(id: &str, body: &str) -> Value {
    json!({
        "id": id,
        "author": { "login": "bea-example", "avatarUrl": "https://avatars.example.test/b.png" },
        "body": body,
        "createdAt": "2026-07-01T00:00:00Z",
        "url": format!("https://github.example.test/acme/web/pull/1#discussion_r{id}"),
    })
}

/// A thread-comments page (`REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY`'s answer).
pub fn thread_comments_page(viewer: Option<&str>, comments: Value, page_info: Value) -> String {
    let mut data = json!({
        "repository": { "pullRequest": { "id": "PR_1" } },
        "node": { "pullRequest": { "id": "PR_1" }, "comments": { "pageInfo": page_info, "nodes": comments } },
    });
    if let Some(viewer) = viewer {
        data["viewer"] = json!({ "login": viewer });
    }
    json!({ "data": data }).to_string()
}

/// `commentWithGroups`.
pub fn comment_with_groups(groups: Value) -> String {
    thread_comments_page(
        None,
        json!([{ "id": "t1", "body": "nice", "createdAt": "2026-07-01T00:00:00Z", "reactionGroups": groups }]),
        json!({ "hasNextPage": false, "endCursor": null }),
    )
}

/// `repositoryJson`: merge settings with an optional permission and no pull request.
pub fn repository_access(viewer_permission: Option<Value>) -> String {
    let mut repository = json!({ "pullRequest": null, "mergeCommitAllowed": true, "squashMergeAllowed": false, "rebaseMergeAllowed": true });
    if let Some(permission) = viewer_permission {
        repository["viewerPermission"] = permission;
    }
    json!({ "data": { "repository": repository } }).to_string()
}

/// `viewerJson`.
pub fn viewer_permissions(repository: Value) -> String {
    let base = json!({ "mergeCommitAllowed": true, "squashMergeAllowed": false, "rebaseMergeAllowed": true });
    json!({ "data": { "repository": with(&base, repository) } }).to_string()
}

/// `labelsJson`.
pub fn label_candidates(defined: Value, applied: &[&str], has_next_page: bool) -> String {
    json!({
        "data": {
            "repository": {
                "labels": { "pageInfo": { "hasNextPage": has_next_page }, "nodes": defined },
                "pullRequest": { "labels": { "nodes": applied.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>() } },
            },
        },
    })
    .to_string()
}

/// `candidatesJson`.
pub fn reviewer_candidates(assignable: Value, requested: Vec<Value>, author: Option<&str>, has_next_page: bool) -> String {
    json!({
        "data": {
            "repository": {
                "assignableUsers": { "pageInfo": { "hasNextPage": has_next_page }, "nodes": assignable },
                "pullRequest": {
                    "author": author.map(|login| json!({ "login": login })),
                    "reviewRequests": { "nodes": requested.into_iter().map(|reviewer| json!({ "requestedReviewer": reviewer })).collect::<Vec<_>>() },
                },
            },
        },
    })
    .to_string()
}

/// `comparison(pullRequest)`.
pub fn comparison(pull_request: Value) -> String {
    json!({ "data": { "repository": { "pullRequest": pull_request } } }).to_string()
}

/// `page(nodes, pageInfo)` of the viewed-files suite.
pub fn files_viewed_page(nodes: Value, page_info: Value) -> String {
    json!({ "data": { "repository": { "pullRequest": { "files": { "pageInfo": page_info, "nodes": nodes } } } } }).to_string()
}

/// `stack(overrides)`: a stack as the preview lists it, bottom to top.
pub fn stack(overrides: Value) -> Value {
    with(
        &json!({
            "id": 42,
            "number": 3,
            "node_id": "STK_kwDO",
            "url": "https://api.github.example.test/repos/acme/web/stacks/3",
            "base": { "ref": "main", "sha": "abc" },
            "open": true,
            "created_at": "2026-09-01T00:00:00Z",
            "pull_requests": [
                { "number": 10, "head": { "ref": "feat/one" }, "state": "closed", "merged_at": "2026-09-02T00:00:00Z" },
                { "number": 11, "head": { "ref": "feat/two" }, "state": "open", "merged_at": null },
                { "number": 12, "head": { "ref": "feat/three" }, "state": "closed", "merged_at": null },
            ],
        }),
        overrides,
    )
}

pub fn stacks_json(stack: Value) -> String {
    json!([stack]).to_string()
}

pub fn stack_memberships() -> String {
    json!({
        "data": {
            "s0": { "pullRequest": { "stack": { "number": 3, "size": 2, "baseRefName": "main" }, "stackEntry": { "position": 1 } } },
            "s1": null,
            "s2": { "pullRequest": null },
            "s3": { "pullRequest": { "stack": null, "stackEntry": null } },
            "s4": { "pullRequest": { "stack": { "number": 3, "size": 2, "baseRefName": "main" } } },
        },
    })
    .to_string()
}

pub fn summaries() -> String {
    json!({
        "data": {
            "s0": {
                "pullRequest": {
                    "number": 7,
                    "title": "Merged",
                    "url": "https://github.example.test/acme/web/pull/7",
                    "author": { "__typename": "Bot", "login": "deps-bot", "avatarUrl": "https://avatars.example.test/r.png" },
                    "headRefName": "feat/seven",
                    "baseRefName": "main",
                    "state": "MERGED",
                    "mergedAt": "2026-08-24T00:00:00Z",
                    "closedAt": "2026-08-24T00:00:00Z",
                    "updatedAt": "2026-08-24T00:00:00Z",
                    "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": "FAILURE" } } }] },
                },
            },
            "s1": { "pullRequest": null },
            "s2": { "pullRequest": { "number": 9 } },
            "rateLimit": { "cost": 1 },
        },
    })
    .to_string()
}

/// A full `PULL_REQUEST_CORE_GRAPHQL_QUERY` answer.
pub fn core_value() -> Value {
    json!({
        "data": {
            "repository": {
                "mergeCommitAllowed": true,
                "squashMergeAllowed": true,
                "rebaseMergeAllowed": false,
                "viewerPermission": "WRITE",
                "pullRequest": {
                    "number": 12,
                    "title": "Core read",
                    "url": "https://github.example.test/acme/widgets/pull/12",
                    "body": "Words",
                    "state": "OPEN",
                    "isDraft": false,
                    "mergeable": "MERGEABLE",
                    "reviewDecision": "REVIEW_REQUIRED",
                    "additions": 10,
                    "deletions": 2,
                    "changedFiles": 3,
                    "createdAt": "2026-07-01T00:00:00Z",
                    "updatedAt": "2026-07-02T00:00:00Z",
                    "mergedAt": null,
                    "closedAt": null,
                    "headRefName": "feat/core",
                    "baseRefName": "main",
                    "headRefOid": " 0123abcd ",
                    "isCrossRepository": true,
                    "headRepositoryOwner": { "login": "fork-owner" },
                    "author": { "login": "ada-example", "avatarUrl": "https://avatars.example.test/a.png", "id": "U_1", "name": "Ada" },
                    "autoMergeRequest": { "mergeMethod": "REBASE" },
                    "viewerCanUpdate": true,
                    "viewerDidAuthor": false,
                    "viewerCanUpdateBranch": true,
                    "baseRef": { "compare": { "behindBy": 4 } },
                    "reviewRequests": { "nodes": [
                        { "requestedReviewer": { "login": "bea-example", "name": "Bea" } },
                        { "requestedReviewer": { "slug": "reviewers", "name": "Reviewers" } },
                        { "requestedReviewer": null },
                    ] },
                    "labels": { "nodes": [{ "name": " bug ", "color": "d73a4a" }, { "name": "  " }] },
                    "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "contexts": {
                        "nodes": [
                            { "__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS", "targetUrl": "https://ci.example.test/1", "createdAt": "2026-07-01T00:00:00Z", "description": "ok" },
                            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-07-01T00:00:00Z", "completedAt": "2026-07-01T00:01:00Z", "detailsUrl": "https://ci.example.test/2", "checkSuite": { "workflowRun": { "workflow": { "name": "CI" } } } },
                            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "FAILURE", "startedAt": "2026-07-01T00:00:00Z", "completedAt": "2026-07-01T00:01:00Z", "checkSuite": { "workflowRun": { "workflow": { "name": "Release" } } } },
                            { "__typename": "CheckRun", "name": "lint", "status": "QUEUED", "checkSuite": { "workflowRun": null } },
                        ],
                        "pageInfo": { "hasNextPage": true },
                    } } } }] },
                },
            },
        },
    })
}

/// A full review-threads answer exercising every collection it carries.
pub fn full_threads() -> String {
    let groups = json!([
        { "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 3, "nodes": [{ "login": "Bea-Example" }, { "login": "cy-example" }, null, { "login": "  " }] } },
        { "content": " laugh ", "reactors": { "totalCount": 0, "nodes": [] } },
        { "content": null, "reactors": null },
        { "content": "EYES", "reactors": { "nodes": [{ "login": "cy-example" }] } },
    ]);
    json!({
        "data": {
            "viewer": { "login": "bea-example" },
            "repository": { "pullRequest": {
                "reviewThreads": {
                    "totalCount": 3,
                    "pageInfo": { "hasNextPage": true, "endCursor": " next " },
                    "nodes": [
                        { "id": "PRRT_1", "isResolved": true, "isOutdated": false, "path": "src/a.ts", "line": 0, "diffSide": "left",
                          "comments": { "totalCount": 12, "pageInfo": { "hasNextPage": true, "endCursor": "c-next" }, "nodes": [
                              { "id": "c1", "author": { "__typename": "Bot", "login": "lint-bot", "avatarUrl": "https://avatars.example.test/l.png" }, "body": "nit", "createdAt": "2026-07-01T00:00:00Z", "url": " ", "reactionGroups": groups },
                          ] } },
                        { "id": " ", "path": "src/b.ts", "comments": { "nodes": [{ "id": "c2", "createdAt": "2026-07-01T00:00:00Z" }] } },
                        { "id": "PRRT_3", "path": "src/c.ts", "comments": { "nodes": [] } },
                    ],
                },
                "viewerCanUpdate": false,
                "viewerDidAuthor": true,
                "author": { "__typename": "User", "login": "ada-example", "avatarUrl": "https://avatars.example.test/a.png" },
                "reactionGroups": groups,
                "comments": { "nodes": [
                    { "id": "IC_1", "author": { "login": "cy-example", "avatarUrl": "https://avatars.example.test/c.png" }, "reactionGroups": groups },
                    { "id": null, "author": null, "reactionGroups": groups },
                    { "id": "IC_2", "reactionGroups": [] },
                ] },
                "reviews": { "nodes": [{ "id": "PRR_1", "author": { "__typename": "Bot", "login": "review-bot", "avatarUrl": "https://avatars.example.test/r.png" }, "reactionGroups": groups }] },
                "reviewRequests": { "nodes": [{ "requestedReviewer": { "login": "dee-example", "name": "Dee", "avatarUrl": "https://avatars.example.test/d.png" } }, { "requestedReviewer": null }, {}] },
                "latestReviews": { "nodes": [{ "state": "APPROVED", "author": { "__typename": "Bot", "login": "review-bot", "avatarUrl": "https://avatars.example.test/r2.png" } }, { "state": "COMMENTED", "author": { "login": "dee-example" } }] },
                "reviewDismissals": { "pageInfo": { "hasNextPage": true, "endCursor": "d-next" }, "nodes": [
                    { "dismissalMessage": " stale ", "review": { "id": "PRR_0" } },
                    { "dismissalMessage": "", "review": { "id": "PRR_2" } },
                    {},
                ] },
                "commits": { "nodes": [
                    { "commit": { "oid": " abc ", "messageHeadline": null, "committedDate": "2026-07-01T00:00:00Z", "additions": -3, "deletions": 4, "parents": { "totalCount": 1 }, "authors": { "nodes": [{ "name": "Ada", "avatarUrl": "https://avatars.example.test/a.png", "user": { "login": "ada-example" } }, { "name": " ", "user": null }] } } },
                    { "commit": { "oid": "def", "committedDate": " ", "additions": 1 } },
                    { "commit": { "oid": " ", "committedDate": "2026-07-02T00:00:00Z" } },
                    { "commit": { "oid": "ghi", "committedDate": "2026-07-03T00:00:00Z", "additions": 1, "deletions": 1, "parents": null } },
                ] },
            } },
        },
    })
    .to_string()
}

/// One decoder input: `(id, decoder, raw)`. Every fixture of the TS tests, plus edge cases.
pub fn decoder_corpus() -> Vec<(String, &'static str, String)> {
    let mut corpus: Vec<(String, &'static str, String)> = Vec::new();
    let mut add = |decoder: &'static str, raw: String| {
        let id = format!("{decoder}#{}", corpus.len());
        corpus.push((id, decoder, raw));
    };

    // decodePullRequestListJson
    let list = "decodePullRequestListJson";
    add(list, list_json(&[json!({ "state": "CLOSED", "mergedAt": "2026-07-03T00:00:00Z" })]));
    add(
        list,
        list_json(&[json!({ "mergeable": "CONFLICTING" }), json!({ "mergeable": "SOMETHING_NEW" }), json!({})]),
    );
    add(
        list,
        list_json(&[json!({ "reviewRequests": [{ "login": "ada-example" }, { "slug": "web-platform" }] })]),
    );
    add(
        list,
        list_json(&[
            json!({ "reviewDecision": "APPROVED" }),
            json!({ "reviewDecision": "CHANGES_REQUESTED" }),
            json!({ "reviewDecision": "REVIEW_REQUIRED" }),
            json!({ "reviewDecision": null }),
        ]),
    );
    add(
        list,
        list_json(&[
            json!({ "reviewDecision": null, "latestReviews": [{ "author": { "login": "lint-bot" }, "state": "APPROVED" }] }),
            json!({ "reviewDecision": "REVIEW_REQUIRED", "latestReviews": [{ "author": { "login": "ada-example" }, "state": "APPROVED" }, { "author": { "login": "helper-bot" }, "state": "CHANGES_REQUESTED" }] }),
            json!({ "reviewDecision": "APPROVED", "latestReviews": [{ "author": { "login": "helper-bot" }, "state": "CHANGES_REQUESTED" }] }),
            json!({ "reviewDecision": null, "latestReviews": [{ "author": { "login": "ada-example" }, "state": "COMMENTED" }] }),
        ]),
    );
    add(
        list,
        list_json(&[
            json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }, { "name": "build", "status": "IN_PROGRESS" }, { "name": "test", "status": "COMPLETED", "conclusion": "FAILURE" }] }),
            json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }, { "name": "build", "status": "QUEUED" }] }),
            json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }] }),
            json!({ "statusCheckRollup": [{ "context": "ci/legacy", "state": "ERROR" }] }),
            json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SKIPPED" }] }),
            json!({ "statusCheckRollup": [{ "name": "lint", "status": "COMPLETED", "conclusion": "SUCCESS" }, { "name": "test", "status": "COMPLETED", "conclusion": "CANCELLED" }] }),
            json!({ "statusCheckRollup": [] }),
            json!({}),
        ]),
    );
    let one = list_json(&[json!({})]);
    add(list, format!("[{},{{\"number\":\"not-a-number\"}}]", &one[1..one.len() - 1]));
    // Edge cases: authors, bots, labels, teams, drafts, nulls where only absence is allowed.
    add(
        list,
        list_json(&[
            json!({ "author": { "login": " ada-example ", "id": " U_1 ", "name": " ", "is_bot": false }, "isDraft": true, "additions": 3, "deletions": 1.0, "labels": [{ "name": " bug ", "color": " " }, { "name": "  ", "color": "fff" }] }),
            json!({ "author": { "login": "deps-bot", "is_bot": true }, "reviewRequests": [{ "name": "Team" }, { "login": null, "slug": " " }] }),
            json!({ "author": { "__typename": "Bot", "login": "app" }, "statusCheckRollup": null, "latestReviews": null, "state": " merged " }),
            json!({ "author": {} }),
            json!({ "isDraft": null }),
            json!({ "additions": 1.5 }),
            json!({ "title": 7 }),
            json!(["not", "an", "object"]),
            json!({ "statusCheckRollup": [{ "name": "a", "status": "", "conclusion": "TIMED_OUT" }, { "context": " ", "state": "PENDING" }, { "state": "SUCCESS" }] }),
            json!({ "statusCheckRollup": [{ "state": "EXPECTED" }, { "name": "x", "conclusion": "STARTUP_FAILURE" }] }),
            json!({ "statusCheckRollup": [{ "name": "x", "status": "COMPLETED", "conclusion": "", "state": "SUCCESS" }, { "name": "y", "conclusion": null, "state": "success" }] }),
        ]),
    );
    add(list, "{}".into());
    add(list, "not json".into());
    add(list, "[1, null, \"x\"]".into());

    // decodePullRequestSearchJson
    let search = "decodePullRequestSearchJson";
    add(search, search_with_stack());
    add(
        search,
        search_value(&[Some("SUCCESS"), Some("FAILURE"), Some("ERROR"), Some("PENDING"), Some("EXPECTED"), None]).to_string(),
    );
    add(
        search,
        json!({ "data": { "search": { "pageInfo": { "hasNextPage": true }, "nodes": [
            {},
            null,
            { "number": 4, "title": "No repo", "url": "u", "headRefName": "h", "baseRefName": "b", "createdAt": "c", "updatedAt": "u" },
            { "number": 5, "title": "Rich", "url": "u", "headRefName": "h", "baseRefName": "b", "createdAt": "c", "updatedAt": "u",
              "repository": { "nameWithOwner": " acme/widgets " }, "author": { "__typename": "Bot", "login": "deps-bot", "avatarUrl": "https://a.example.test/d.png" },
              "latestReviews": { "nodes": [null, { "state": "CHANGES_REQUESTED", "author": { "login": "ada-example" } }] },
              "reviewRequests": { "nodes": [null, { "requestedReviewer": null }, { "requestedReviewer": { "login": " bea-example " } }, {}] },
              "labels": { "nodes": [null, { "name": "bug", "color": null }] },
              "commits": { "nodes": [null, { "commit": null }, { "commit": { "statusCheckRollup": { "state": " " } } }] },
              "stack": { "number": 2, "size": 3, "baseRefName": "main" }, "stackEntry": null, "additions": 9 },
            { "number": 6, "title": "Bad labels", "url": "u", "headRefName": "h", "baseRefName": "b", "createdAt": "c", "updatedAt": "u", "repository": { "nameWithOwner": "a/b" }, "labels": { "nodes": [{ "color": "x" }] } },
        ] } } })
        .to_string(),
    );
    add(search, json!({ "data": { "search": {} } }).to_string());
    add(search, json!({ "data": { "search": { "nodes": null, "pageInfo": null } } }).to_string());
    add(search, json!({ "data": {} }).to_string());

    // decodePullRequestDetailJson
    let detail = "decodePullRequestDetailJson";
    add(detail, detail_json());
    add(
        detail,
        detail_with(json!({ "statusCheckRollup": [
            { "__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS" },
            { "__typename": "CheckRun", "name": "contributor tests", "status": "COMPLETED", "conclusion": "ACTION_REQUIRED" },
        ] })),
    );
    add(
        detail,
        detail_with(json!({ "autoMergeRequest": { "enabledBy": { "login": "ada-example" }, "mergeMethod": "SQUASH" } })),
    );
    add(detail, detail_with(json!({ "autoMergeRequest": null })));
    add(detail, detail_with(json!({ "autoMergeRequest": {} })));
    add(detail, detail_with(json!({ "autoMergeRequest": { "mergeMethod": "fast-forward" } })));
    add(
        detail,
        detail_with(json!({ "statusCheckRollup": [
            { "__typename": "CheckRun", "name": "Prepare PR size config", "workflowName": "PR Size", "status": "COMPLETED", "conclusion": "SUCCESS", "startedAt": "2026-08-11T16:06:20Z", "completedAt": "2026-08-11T16:06:25Z" },
            { "__typename": "CheckRun", "name": "Prepare PR size config", "workflowName": "PR Size", "status": "IN_PROGRESS", "conclusion": "", "startedAt": "2026-08-11T17:01:04Z", "completedAt": "0001-01-01T00:00:00Z" },
        ] })),
    );
    add(
        detail,
        detail_with(
            json!({ "isCrossRepository": false, "headRepositoryOwner": { "login": " fork " }, "headRefOid": "abc", "changedFiles": 4, "closedAt": " ", "mergedAt": "2026-07-09T00:00:00Z", "body": null }),
        ),
    );
    add(detail, detail_with(json!({ "body": "", "headRepositoryOwner": {} })));
    add(detail, without(&detail_value(), "number").to_string());

    // decodePullRequestActivityJson
    let activity = "decodePullRequestActivityJson";
    add(activity, detail_json());
    add(
        activity,
        detail_with(json!({ "reviews": [
            { "id": "r4", "body": "", "state": "COMMENTED", "submittedAt": "2026-07-07T00:00:00Z" },
            { "id": "r5", "body": "Looks good.", "state": "COMMENTED", "submittedAt": "2026-07-08T00:00:00Z" },
        ] })),
    );
    for state in ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"] {
        add(
            activity,
            detail_with(json!({ "reviews": [{ "id": "r6", "body": "", "state": state, "submittedAt": "2026-07-07T00:00:00Z" }] })),
        );
    }
    add(
        activity,
        detail_with(json!({ "reviews": [{ "id": "r3", "body": "  ", "submittedAt": "2026-07-07T00:00:00Z" }] })),
    );
    add(
        activity,
        json!({
            "author": { "login": "ada-example" },
            "comments": [{ "id": "c2", "createdAt": "2026-07-01T00:00:00.5Z", "url": " https://x.example.test " }, { "id": "c1", "createdAt": "2026-07-01T00:00:00Z", "author": null }],
            "reviews": [{ "id": "r9", "state": "dismissed", "submittedAt": " 2026-07-01T00:00:00Z " }, { "id": "r10", "body": "x", "submittedAt": null }],
            "commits": [{ "oid": "o", "committedDate": "d", "authors": [{ "email": " e@example.test " }, {}, { "login": "  ", "name": "  ", "email": null }] }, { "oid": "p", "committedDate": "d" }],
        })
        .to_string(),
    );
    add(activity, json!({}).to_string());
    add(activity, json!({ "comments": [{ "id": "c1" }] }).to_string());

    // decodeWorkflowRunApprovalsJson
    let approvals = "decodeWorkflowRunApprovalsJson";
    add(
        approvals,
        json!([{ "databaseId": 10, "workflowName": "contributor tests", "url": "https://example.test/10" }, { "databaseId": 11, "workflowName": null, "url": null }, { "databaseId": 12, "workflowName": "  " }])
            .to_string(),
    );
    add(approvals, json!([{ "workflowName": "x" }]).to_string());

    // decodePullRequestHeadsJson
    let heads = "decodePullRequestHeadsJson";
    add(
        heads,
        json!([{ "number": 1, "headRefOid": "abc", "isCrossRepository": true, "headRepositoryOwner": { "login": " fork " } }, { "number": 2, "headRefOid": " def ", "headRepositoryOwner": null }]).to_string(),
    );
    add(heads, json!([{ "number": 1 }]).to_string());

    // decodeReviewThreadsJson
    let threads = "decodeReviewThreadsJson";
    add(
        threads,
        review_roster(
            vec![json!({ "login": "jules-example", "name": "Jules", "avatarUrl": "https://avatars.example.test/j.png" })],
            vec![json!({ "__typename": "Bot", "login": "lint-bot", "avatarUrl": "https://avatars.example.test/in/1.png" })],
        ),
    );
    add(
        threads,
        commits_page(
            json!([{ "commit": { "oid": "abc123", "additions": 18, "deletions": 7 } }, { "commit": { "oid": "def456", "additions": 3, "deletions": 0 } }]),
        ),
    );
    add(
        threads,
        commits_page(json!([{ "commit": { "oid": "merge123", "additions": 36_858, "deletions": 12_928, "parents": { "totalCount": 2 } } }])),
    );
    add(
        threads,
        commits_page(json!([
            { "commit": { "oid": "abc123", "messageHeadline": "Ship the timeline", "committedDate": "2026-07-05T00:00:00Z", "additions": 18, "deletions": 7, "authors": { "nodes": [{ "name": "Jules", "user": { "login": "jules-example" } }] } } },
            { "commit": { "oid": "def456", "messageHeadline": "Fix the flaky test", "committedDate": "2026-07-06T00:00:00Z" } },
        ])),
    );
    add(
        threads,
        review_roster(
            vec![json!({ "login": "jules-example", "avatarUrl": "https://avatars.example.test/j.png" })],
            vec![json!({ "login": "jules-example", "avatarUrl": "https://avatars.example.test/j.png" })],
        ),
    );
    add(threads, review_roster(vec![Value::Null], vec![]));
    add(
        threads,
        review_roster(
            vec![
                json!({}),
                json!({ "login": "jules-example", "avatarUrl": "https://avatars.example.test/j.png" }),
            ],
            vec![],
        ),
    );
    add(
        threads,
        threads_page_default(json!([
            { "id": "PRRT_a", "isResolved": false, "path": "apps/server/src/ws.ts", "comments": { "nodes": [{ "id": "t1", "body": "fix this", "createdAt": "2026-07-01T00:00:00Z" }] } },
            { "id": "PRRT_b", "isResolved": true, "path": "apps/web/src/main.tsx", "comments": { "nodes": [{ "id": "t2", "body": "done", "createdAt": "2026-07-01T00:00:00Z" }] } },
        ])),
    );
    add(
        threads,
        threads_page_default(
            json!([{ "id": "PRRT_c", "isResolved": false, "path": "apps/server/src/ws.ts", "comments": { "nodes": [
            { "id": "t1", "body": "fix this", "createdAt": "2026-07-01T00:00:00Z" },
            { "id": "t2", "body": "fixed", "createdAt": "2026-07-01T01:00:00Z" },
        ] } }]),
        ),
    );
    add(
        threads,
        threads_page(
            json!([{ "id": "PRRT_d", "path": "apps/server/src/ws.ts", "isResolved": false, "comments": { "nodes": [{ "id": "t1", "createdAt": "2026-07-01T00:00:00Z" }] } }]),
            80,
            json!({ "hasNextPage": true, "endCursor": "Y3Vyc29yOjE" }),
        ),
    );
    add(
        threads,
        threads_page_default(json!([{ "id": "PRRT_e", "path": "apps/server/src/ws.ts", "isResolved": false, "comments": {
            "totalCount": 140, "pageInfo": { "hasNextPage": true, "endCursor": "Y3Vyc29yOjI" }, "nodes": [{ "id": "t1", "createdAt": "2026-07-01T00:00:00Z" }],
        } }])),
    );
    add(threads, threads_with(json!([]), json!({ "viewerCanUpdate": false, "viewerDidAuthor": false })));
    add(threads, threads_with(json!([]), json!({})));
    add(
        threads,
        threads_with(
            json!([{ "id": "PRRT_1", "isResolved": false, "isOutdated": false, "path": "src/a.ts", "line": 42, "diffSide": "LEFT", "comments": { "totalCount": 2, "nodes": [thread_comment("c1", "first"), thread_comment("c2", "second")] } }]),
            json!({}),
        ),
    );
    add(
        threads,
        threads_with(
            json!([{ "id": "PRRT_2", "isResolved": true, "isOutdated": true, "path": "src/a.ts", "line": null, "diffSide": "RIGHT", "comments": { "totalCount": 1, "nodes": [thread_comment("c3", "stale")] } }]),
            json!({}),
        ),
    );
    add(
        threads,
        threads_with(
            json!([{ "id": "PRRT_3", "isResolved": true, "path": "src/a.ts", "line": 7, "diffSide": "RIGHT", "comments": { "totalCount": 1, "nodes": [thread_comment("c4", "done")] } }]),
            json!({}),
        ),
    );
    add(
        threads,
        threads_with(
            json!([]),
            json!({
                "reactionGroups": [{ "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 1, "nodes": [{ "login": "bea-example" }] } }],
                "comments": { "nodes": [{ "id": "c1", "reactionGroups": [{ "content": "THUMBS_UP", "reactors": { "totalCount": 1, "nodes": [{ "login": "jules-example" }] } }] }] },
                "reviews": { "nodes": [{ "id": "r1", "reactionGroups": [{ "content": "EYES", "reactors": { "totalCount": 1, "nodes": [{ "login": "helper-bot" }] } }] }] },
            }),
        ),
    );
    add(
        threads,
        json!({ "data": { "viewer": { "login": "Bea-Example" }, "repository": { "pullRequest": {
            "reviewThreads": { "totalCount": 0, "nodes": [] },
            "reactionGroups": [{ "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "bea-example" }, { "login": "jules-example" }] } }],
        } } } })
        .to_string(),
    );
    add(threads, full_threads());
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [{ "comments": {} }] } } } } }).to_string(),
    );
    add(
        threads,
        json!({ "data": { "viewer": null, "repository": { "pullRequest": { "reviewThreads": { "nodes": [] }, "reviewRequests": null, "commits": null } } } })
            .to_string(),
    );
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [] }, "commits": { "nodes": [{ "commit": { "oid": 1 } }] } } } } })
            .to_string(),
    );

    // decodeReviewThreadCommentsJson
    let thread_comments = "decodeReviewThreadCommentsJson";
    add(
        thread_comments,
        thread_comments_page(
            None,
            json!([{ "id": "t9", "body": "last", "createdAt": "2026-07-01T00:00:00Z" }]),
            json!({ "hasNextPage": false, "endCursor": "Y3Vyc29yOjk" }),
        ),
    );
    add(
        thread_comments,
        comment_with_groups(json!([
            { "content": "THUMBS_UP", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "jules-example" }, { "login": "bea-example" }] } },
            { "content": "PARTY_PARROT", "reactors": { "totalCount": 1, "nodes": [{ "login": "helper-bot" }] } },
            { "content": "HEART", "reactors": { "totalCount": 0, "nodes": [] } },
            { "content": "ROCKET", "reactors": { "totalCount": 140, "nodes": [{ "login": "a" }, { "login": "b" }, { "login": "c" }] } },
        ])),
    );
    add(
        thread_comments,
        thread_comments_page(
            Some("Bea-Example"),
            json!([{ "id": "t1", "body": "nice", "createdAt": "2026-07-01T00:00:00Z", "reactionGroups": [
                { "content": "HEART", "viewerHasReacted": true, "reactors": { "totalCount": 2, "nodes": [{ "login": "bea-example" }, { "login": "jules-example" }] } },
            ] }]),
            json!({ "hasNextPage": false, "endCursor": null }),
        ),
    );
    add(
        thread_comments,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "pullRequest": { "id": "PR_2" }, "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "n" }, "nodes": [] } } } }).to_string(),
    );
    add(thread_comments, json!({ "data": { "repository": null, "node": null } }).to_string());
    add(
        thread_comments,
        json!({ "data": { "repository": { "pullRequest": null }, "node": {} } }).to_string(),
    );
    add(
        thread_comments,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "pullRequest": null } } }).to_string(),
    );
    add(thread_comments, json!({ "data": { "node": null } }).to_string());

    // decodeViewerPermissionsJson
    let permissions = "decodeViewerPermissionsJson";
    for permission in ["ADMIN", "MAINTAIN", "WRITE", "TRIAGE", "READ", "NONE", " write "] {
        add(permissions, repository_access(Some(json!(permission))));
    }
    add(permissions, repository_access(None));
    add(permissions, repository_access(Some(Value::Null)));
    add(
        permissions,
        json!({ "data": { "repository": { "pullRequest": null, "mergeCommitAllowed": true } } }).to_string(),
    );
    add(
        permissions,
        viewer_permissions(json!({ "viewerPermission": "READ", "pullRequest": { "viewerCanUpdate": true, "viewerDidAuthor": true } })),
    );
    add(
        permissions,
        viewer_permissions(json!({ "viewerPermission": "READ", "pullRequest": { "viewerCanUpdate": false, "viewerDidAuthor": false } })),
    );
    add(permissions, viewer_permissions(json!({ "pullRequest": null })));
    add(
        permissions,
        viewer_permissions(json!({ "viewerPermission": "TRIAGE", "pullRequest": { "viewerCanUpdate": false, "viewerDidAuthor": false } })),
    );
    add(permissions, viewer_permissions(json!({})));

    // decodeLabelCandidatesJson
    let labels = "decodeLabelCandidatesJson";
    add(
        labels,
        label_candidates(
            json!([{ "name": "bug", "color": "d73a4a", "description": "Something is broken" }, { "name": "size:XL", "color": "e4572e", "description": null }]),
            &["size:XL"],
            false,
        ),
    );
    add(labels, label_candidates(json!([{ "name": "bug" }]), &["legacy"], false));
    add(labels, label_candidates(json!([]), &[], true));
    add(
        labels,
        label_candidates(
            json!([null, { "name": " bug " }, { "name": "bug", "color": "x" }, { "name": " " }]),
            &["legacy", " legacy ", "bug", "new"],
            false,
        ),
    );
    add(labels, json!({ "data": { "repository": { "labels": null, "pullRequest": null } } }).to_string());
    add(
        labels,
        json!({ "data": { "repository": { "pullRequest": { "labels": { "nodes": [null] } } } } }).to_string(),
    );
    add(labels, json!({ "data": { "repository": {} } }).to_string());

    // decodeReviewerCandidatesJson
    let reviewers = "decodeReviewerCandidatesJson";
    add(
        reviewers,
        reviewer_candidates(
            json!([{ "login": "bea-example" }, { "login": "ada-example", "name": "Ada Example" }]),
            vec![],
            Some("bea-example"),
            false,
        ),
    );
    add(
        reviewers,
        reviewer_candidates(
            json!([{ "login": "ada-example" }, { "login": "helper-bot" }]),
            vec![json!({ "login": "ada-example" })],
            None,
            false,
        ),
    );
    add(
        reviewers,
        reviewer_candidates(
            json!([{ "login": "ada-example" }]),
            vec![json!({ "slug": "reviewers", "name": "Reviewers" })],
            None,
            false,
        ),
    );
    add(reviewers, reviewer_candidates(json!([{ "login": "ada-example" }]), vec![], None, true));
    add(
        reviewers,
        reviewer_candidates(
            json!([null, { "login": " " }, { "login": "ada-example", "avatarUrl": " https://a.example.test " }, { "login": "ada-example", "name": "again" }]),
            vec![
                Value::Null,
                json!({}),
                json!({ "slug": " ", "login": "bea-example", "avatarUrl": "https://b.example.test" }),
                json!({ "login": "bea-example", "name": "Bea" }),
                json!({ "__typename": "Bot", "login": "deps-bot" }),
            ],
            Some(" ada-example "),
            false,
        ),
    );
    add(
        reviewers,
        json!({ "data": { "repository": { "assignableUsers": { "nodes": [] }, "pullRequest": null } } }).to_string(),
    );
    add(
        reviewers,
        json!({ "data": { "repository": { "assignableUsers": { "nodes": [] } } } }).to_string(),
    );

    // decodeBaseComparisonJson
    let base = "decodeBaseComparisonJson";
    add(
        base,
        comparison(json!({ "viewerCanUpdateBranch": true, "baseRef": { "compare": { "behindBy": 12 } } })),
    );
    add(
        base,
        comparison(json!({ "viewerCanUpdateBranch": false, "baseRef": { "compare": { "behindBy": 0 } } })),
    );
    add(base, comparison(json!({ "viewerCanUpdateBranch": true, "baseRef": null })));
    add(base, comparison(Value::Null));
    add(base, "{".into());
    add(base, comparison(json!({ "baseRef": { "compare": { "behindBy": -1 } } })));
    add(base, comparison(json!({ "baseRef": { "compare": { "behindBy": 2.5 } } })));
    add(base, comparison(json!({ "baseRef": { "compare": {} } })));
    add(base, json!({ "data": { "repository": null } }).to_string());

    // decodePullRequestFilesJson
    let files = "decodePullRequestFilesJson";
    add(
        files,
        json!([{ "filename": "src\\notes.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new" }]).to_string(),
    );
    add(
        files,
        json!([{ "previous_filename": " old\\name.ts ", "filename": " new\\name.ts ", "status": "renamed" }]).to_string(),
    );
    add(
        files,
        json!([{ "filename": "src/app.ts", "status": "modified", "patch": "@@ -1 +1 @@\n-old\n+new" }]).to_string(),
    );
    add(files, json!([{ "filename": "src/new.ts", "status": "added", "patch": "@@ -0,0 +1 @@\n+hello" }, { "filename": "src/gone.ts", "status": "removed", "patch": "@@ -1 +0,0 @@\n-bye" }]).to_string());
    add(
        files,
        json!([{ "filename": "src/new.ts", "status": "renamed", "previous_filename": "src/old.ts", "patch": "@@ -1 +1 @@\n-old\n+new" }]).to_string(),
    );
    add(
        files,
        json!([{ "filename": "logo.png", "status": "modified", "additions": 4, "deletions": 2 }, { "filename": "src/app.ts", "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-old\n+new" }])
            .to_string(),
    );
    add(
        files,
        json!([{ "filename": "src/new.ts", "previous_filename": "src/old.ts", "status": "renamed", "additions": 0, "deletions": 0 }]).to_string(),
    );
    add(
        files,
        json!([
            { "filename": "tab\tname\u{7}\u{1}\u{7f}é.ts", "status": " Added ", "patch": "@@ -0,0 +1 @@\n+x\n" },
            { "filename": "quote\".ts", "status": "renamed", "previous_filename": "", "patch": "@@ -1 +1 @@\n-a\n+b\n\n" },
            { "filename": "line\nbreak.ts", "status": null, "patch": null, "additions": null, "deletions": 5 },
            { "status": "modified" },
            null,
            { "filename": "copy.ts", "status": "copied", "patch": "" },
        ])
        .to_string(),
    );
    add(files, "{}".into());

    // decodePullRequestFilesViewedJson
    let viewed = "decodePullRequestFilesViewedJson";
    add(
        viewed,
        files_viewed_page(
            json!([{ "path": "src/a.ts", "viewerViewedState": "VIEWED" }, { "path": "src/b.ts", "viewerViewedState": "UNVIEWED" }, { "path": "src/c.ts", "viewerViewedState": "DISMISSED" }]),
            json!({ "hasNextPage": true, "endCursor": "cursor-2" }),
        ),
    );
    add(
        viewed,
        files_viewed_page(
            json!([{ "path": "src/a.ts", "viewerViewedState": "SOMETHING_NEW" }]),
            json!({ "hasNextPage": false, "endCursor": null }),
        ),
    );
    add(viewed, json!({ "data": { "repository": { "pullRequest": null } } }).to_string());
    add(viewed, json!({ "data": { "repository": null } }).to_string());
    add(
        viewed,
        files_viewed_page(
            json!([null, { "path": "", "viewerViewedState": "VIEWED" }, { "path": "x", "viewerViewedState": " viewed " }]),
            json!({ "hasNextPage": true, "endCursor": null }),
        ),
    );
    add(viewed, files_viewed_page(Value::Null, json!({ "hasNextPage": false, "endCursor": " c " })));
    add(viewed, files_viewed_page(json!([]), json!({ "hasNextPage": false })));

    // decodePullRequestStacksJson
    let stacks = "decodePullRequestStacksJson";
    add(stacks, stacks_json(stack(json!({}))));
    add(
        stacks,
        stacks_json(stack(
            json!({ "pull_requests": [{ "number": 11, "title": "Second layer", "draft": true, "head": { "ref": "feat/two", "sha": "abc123" }, "state": "open", "merged_at": null }] }),
        )),
    );
    add(stacks, stacks_json(stack(json!({ "base": "develop" }))));
    add(
        stacks,
        stacks_json(stack(json!({ "html_url": "https://github.example.test/acme/web/stacks/3" }))),
    );
    add(stacks, stacks_json(without(&stack(json!({})), "id")));
    add(stacks, stacks_json(stack(json!({ "id": null, "node_id": null }))));
    add(stacks, stacks_json(stack(json!({ "id": "stack-7" }))));
    add(stacks, stacks_json(stack(json!({ "id": 1.5 }))));
    add(stacks, stacks_json(stack(json!({ "html_url": " ", "node_id": " " , "id": null }))));
    add(stacks, stacks_json(stack(json!({ "base": { "sha": "x" } }))));
    add(stacks, "[]".into());
    add(stacks, stacks_json(without(&stack(json!({})), "number")));
    add(stacks, stacks_json(without(&stack(json!({})), "pull_requests")));
    add(stacks, "{".into());

    // decodePullRequestStackMembershipsJson
    let memberships = "decodePullRequestStackMembershipsJson";
    add(memberships, stack_memberships());
    add(memberships, "{\"errors\":[]}".into());
    add(memberships, json!({ "data": { "s01": { "pullRequest": { "stack": { "number": 1, "size": 2, "baseRefName": "b" }, "stackEntry": { "position": 2 } } }, "x": null, "s": null } }).to_string());
    add(
        memberships,
        json!({ "data": { "s0": { "pullRequest": { "stack": { "number": "1" } } } } }).to_string(),
    );

    // decodePullRequestSummariesJson
    let summaries_decoder = "decodePullRequestSummariesJson";
    add(summaries_decoder, summaries());
    add(summaries_decoder, json!({ "data": null }).to_string());
    add(summaries_decoder, json!({}).to_string());
    add(summaries_decoder, json!({ "data": { "s0": 3 } }).to_string());
    add(
        summaries_decoder,
        json!({ "data": { "s3": { "pullRequest": {
            "number": 3, "title": "t", "url": "u", "headRefName": "h", "baseRefName": "b", "updatedAt": "u", "createdAt": "c", "isDraft": true,
            "additions": null, "deletions": 4, "changedFiles": 2, "closedAt": " ", "mergeable": "conflicting", "reviewDecision": null,
            "latestReviews": { "nodes": [null, { "state": "APPROVED" }] }, "commits": { "nodes": [] },
        } } } })
        .to_string(),
    );

    // decodePullRequestStatsJson
    let stats = "decodePullRequestStatsJson";
    add(stats, json!({ "data": { "s0": { "pullRequest": { "additions": 5, "deletions": 1 } }, "s1": { "pullRequest": null }, "s2": null, "s3": { "pullRequest": {} }, "s4": {}, "other": null } }).to_string());
    add(stats, json!({ "data": null }).to_string());
    add(stats, json!({ "data": { "x": 1 } }).to_string());

    // decodePullRequestCoreJson
    let core = "decodePullRequestCoreJson";
    add(core, core_value().to_string());
    let mut closed = core_value();
    closed["data"]["repository"]["pullRequest"]["state"] = json!("CLOSED");
    closed["data"]["repository"]["pullRequest"]["autoMergeRequest"] = Value::Null;
    closed["data"]["repository"]["viewerPermission"] = json!("TRIAGE");
    add(core, closed.to_string());
    let mut no_rollup = core_value();
    no_rollup["data"]["repository"]["pullRequest"]["commits"] = json!({ "nodes": [{ "commit": { "statusCheckRollup": null } }] });
    no_rollup["data"]["repository"]["pullRequest"]["baseRef"] = json!({ "compare": null });
    add(core, no_rollup.to_string());
    let mut no_commits = core_value();
    no_commits["data"]["repository"]["pullRequest"]["commits"] = json!({ "nodes": [] });
    no_commits["data"]["repository"]["pullRequest"]["baseRef"] = Value::Null;
    add(core, no_commits.to_string());
    let mut missing = core_value();
    missing["data"]["repository"]["pullRequest"]
        .as_object_mut()
        .unwrap()
        .shift_remove("viewerCanUpdateBranch");
    add(core, missing.to_string());

    // decodePullRequestPreviewJson
    let preview = "decodePullRequestPreviewJson";
    add(
        preview,
        json!({ "data": { "repository": { "pullRequest": { "number": 3, "title": "t", "url": "u", "state": "MERGED", "isDraft": false, "createdAt": "c", "author": { "login": "ada-example", "avatarUrl": "a", "name": "Ada" } } } } }).to_string(),
    );
    add(preview, json!({ "data": { "repository": { "pullRequest": { "number": 3, "title": "t", "url": "u", "state": "OPEN", "isDraft": true, "createdAt": "c", "author": null } } } }).to_string());
    add(
        preview,
        json!({ "data": { "repository": { "pullRequest": { "number": 3, "title": "t", "url": "u", "state": "OPEN", "isDraft": true, "createdAt": "c" } } } })
            .to_string(),
    );

    // decodePullRequestNodeIdJson
    let node_id = "decodePullRequestNodeIdJson";
    add(
        node_id,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_kwDOA" } } } }).to_string(),
    );
    add(node_id, json!({ "data": { "repository": { "pullRequest": null } } }).to_string());

    // decodeReactionSubjectScopeJson
    let scope = "decodeReactionSubjectScopeJson";
    add(
        scope,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "id": "PR_1" } } }).to_string(),
    );
    add(
        scope,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "id": "IC_1", "pullRequest": { "id": "PR_1" } } } }).to_string(),
    );
    add(
        scope,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "id": "IC_1", "pullRequest": { "id": "PR_2" } } } }).to_string(),
    );
    add(scope, json!({ "data": { "repository": null, "node": { "id": "PR_1" } } }).to_string());
    add(
        scope,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": null } }).to_string(),
    );
    add(
        scope,
        json!({ "data": { "repository": { "pullRequest": { "id": "PR_1" } }, "node": { "id": "IC_1", "pullRequest": null } } }).to_string(),
    );

    // decodeReviewDismissalsJson
    let dismissals = "decodeReviewDismissalsJson";
    add(
        dismissals,
        json!({ "data": { "repository": { "pullRequest": { "timelineItems": { "pageInfo": { "hasNextPage": true, "endCursor": " c2 " }, "nodes": [
            { "dismissalMessage": "Outdated", "review": { "id": "PRR_1" } }, {}, { "review": null }, { "dismissalMessage": "x", "review": { "id": " " } },
        ] } } } } })
        .to_string(),
    );
    add(
        dismissals,
        json!({ "data": { "repository": { "pullRequest": { "timelineItems": { "nodes": [] } } } } }).to_string(),
    );
    add(
        dismissals,
        json!({ "data": { "repository": { "pullRequest": { "timelineItems": {} } } } }).to_string(),
    );

    // decodeActorAvatarsJson
    let avatars = "decodeActorAvatarsJson";
    add(
        avatars,
        json!({ "data": { "nodes": [{ "login": "ada-example", "avatarUrl": "https://a.example.test" }, null, {}, { "login": "bea-example", "avatarUrl": null }, { "login": " cy ", "avatarUrl": " c " }] } }).to_string(),
    );
    add(avatars, json!({ "data": { "nodes": null } }).to_string());

    // Which failure is reported where several fields are wrong, and how numbers and unions read.
    add(detail, detail_with(json!({ "number": "x", "reviewRequests": 5, "labels": [{}] })));
    add(detail, detail_with(json!({ "reviewRequests": [5], "labels": 5 })));
    add(
        detail,
        detail_with(json!({ "statusCheckRollup": [{ "name": 5 }], "latestReviews": [{ "state": 1 }] })),
    );
    add(detail, detail_with(json!({ "autoMergeRequest": 5, "closedAt": 1 })));
    add(detail, detail_with(json!({ "autoMergeRequest": { "mergeMethod": 5 } })));
    add(detail, detail_with(json!({ "author": [], "headRepositoryOwner": "x" })));
    add(detail, detail_with(json!({ "number": 1e2, "changedFiles": 2.0 })));
    add(
        detail,
        r#"{"number":9007199254740993,"title":"t","url":"u","headRefName":"h","baseRefName":"b","createdAt":"c","updatedAt":"u"}"#.into(),
    );
    add(
        detail,
        r#"{"number":-0,"title":"t","url":"u","headRefName":"h","baseRefName":"b","createdAt":"c","updatedAt":"u"}"#.into(),
    );
    add(
        detail,
        r#" {"number":1,"number":"x","title":"t","url":"u","headRefName":"h","baseRefName":"b","createdAt":"c","updatedAt":"u"} "#.into(),
    );
    add(detail, r#"{"number":1} trailing"#.into());
    add(detail, "[]".into());
    add(detail, "null".into());
    add(search, json!({ "data": { "search": [] } }).to_string());
    add(search, json!({ "data": { "search": { "pageInfo": {}, "nodes": [] } } }).to_string());
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [] }, "reactionGroups": 5 } } } }).to_string(),
    );
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [] }, "reactionGroups": [{ "content": 5 }] } } } }).to_string(),
    );
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [] }, "reviewDismissals": { "nodes": 5 }, "author": 5 } } } })
            .to_string(),
    );
    add(
        threads,
        json!({ "data": { "repository": { "pullRequest": { "reviewThreads": { "nodes": [], "pageInfo": null } } } } }).to_string(),
    );
    add(stacks, stacks_json(stack(json!({ "id": true }))));
    add(stacks, stacks_json(stack(json!({ "base": 5 }))));
    add(
        stacks,
        stacks_json(stack(json!({ "pull_requests": [{ "number": 1, "head": { "ref": "r", "sha": null } }] }))),
    );
    let mut core_errors = core_value();
    core_errors["data"]["repository"]["pullRequest"]["labels"] = json!({ "nodes": [{ "name": 5 }] });
    core_errors["data"]["repository"]["pullRequest"]["commits"] = json!({});
    add(core, core_errors.to_string());
    let mut core_requests = core_value();
    core_requests["data"]["repository"]["pullRequest"]["reviewRequests"] = json!({ "nodes": [{}] });
    core_requests["data"]["repository"]["pullRequest"]["latestReviews"] = json!(5);
    add(core, core_requests.to_string());

    corpus
}
