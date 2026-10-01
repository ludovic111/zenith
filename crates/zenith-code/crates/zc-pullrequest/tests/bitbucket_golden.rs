//! Golden comparison against the TypeScript Bitbucket pull request provider: the TS provider
//! (`golden/bitbucket_oracle.mjs`, run from source through node over the real `BitbucketApi` and
//! its fetch client) and the Rust one answer the same cases against the same local Bitbucket
//! stub. Every result (mapped to the TS object shape by the serializers below) and every error
//! (wire-encoded) must match, and so must the requests each case sent (method, path and query,
//! body), compared per case as sorted lists.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Nested error causes are compared by name
//! below the first level (see [`normalize`]). A case marked `racy` fails while concurrent reads
//! are in flight, so which of those reached the stub depends on timing: only its answer is
//! compared.

#![allow(clippy::result_large_err)]

mod support_bitbucket;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Map, Value};
use support_bitbucket::*;
use zc_contracts::PullRequestReviewCommentDraft;
use zc_core::vcs_process::VcsProcess;
use zc_pullrequest::bitbucket::BitbucketPullRequestProvider;
use zc_pullrequest::provider::*;
use zc_sourcecontrol::bitbucket::{BitbucketApi, BitbucketApiConfig, StaticBitbucketSettings};
use zc_sourcecontrol::util::system_clock;
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::GitVcsDriver;

const EMAIL: &str = "someone@example.test";
const API_TOKEN: &str = "test-token";
/// An HTTP date far enough ahead that both sides read it as the same `retryAt`.
const RETRY_AFTER: &str = "Wed, 01 Jan 2031 00:00:00 GMT";

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

fn run_oracle(cases: &[Value], base: &str) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/bitbucket_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("HOME", home.path())
        .env("T3CODE_BITBUCKET_API_BASE_URL", base)
        .env("T3CODE_BITBUCKET_EMAIL", EMAIL)
        .env("T3CODE_BITBUCKET_API_TOKEN", API_TOKEN)
        .env_remove("T3CODE_BITBUCKET_ACCESS_TOKEN")
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

/// Keeps the first-level cause's name and message; deeper causes are compared by name (a failed
/// TS decode keeps an Effect `Cause` there, which Rust reports as a `SchemaError`).
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

// ---------------------------------------------------------------------------------------------
// The Bitbucket stub
// ---------------------------------------------------------------------------------------------

fn merge(mut value: Value, extra: Value) -> Value {
    if let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) {
        base.extend(extra);
    }
    value
}

fn pull_request(repository: &str, id: i64, extra: Value) -> Value {
    merge(
        json!({
            "id": id,
            "title": format!("Pull request {id}"),
            "state": "OPEN",
            "created_on": "2026-06-16T05:04:32+00:00",
            "updated_on": "2026-06-16T05:04:33+00:00",
            "source": {"branch": {"name": "feat/page"}},
            "destination": {"branch": {"name": "main"}},
            "links": {"html": {"href": format!("https://bitbucket.example.test/{repository}/pull-requests/{id}")}},
        }),
        extra,
    )
}

fn rich_pull_request() -> Value {
    pull_request(
        "acme/web",
        1,
        json!({
            "title": "Add the widget pipeline",
            "description": "Adds **widgets**.\n\nSecond paragraph.",
            "draft": false,
            "created_on": "2026-06-16T05:04:32.258456+00:00",
            "updated_on": "2026-06-18T09:00:00.000001+02:00",
            "author": {"uuid": "{avery}", "nickname": "avery", "display_name": "Avery Stone", "links": {"avatar": {"href": "https://avatars.example.test/avery.png"}}},
            "source": {"branch": {"name": " feat/widgets "}, "repository": {"full_name": "fork-owner/web"}},
            "destination": {"branch": {"name": "main"}, "repository": {"full_name": "acme/web"}},
            "reviewers": [
                {"uuid": "{quinn}", "nickname": "quinn", "display_name": "Quinn Reyes"},
                {"uuid": " {release-bot} ", "display_name": "Release Bot"},
                {"uuid": null, "nickname": null, "display_name": null},
            ],
            "participants": [
                {"user": {"nickname": "quinn", "display_name": "Quinn Reyes"}, "role": "REVIEWER", "approved": true, "state": "approved", "participated_on": "2026-06-17T09:00:00+00:00"},
                {"user": {"nickname": "robin"}, "role": "PARTICIPANT", "approved": false, "state": "changes_requested", "participated_on": "2026-06-17T08:00:00.5+00:00"},
                {"user": {"display_name": "Release Bot"}, "role": "REVIEWER", "approved": true, "state": null, "participated_on": "2026-06-17T10:00:00+00:00"},
                {"user": {"nickname": "sam"}, "role": "REVIEWER", "approved": false, "state": null, "participated_on": null},
            ],
        }),
    )
}

const PATCH: &str = concat!(
    "diff --git a/src/a.ts b/src/a.ts\n",
    "index 1111111..2222222 100644\n",
    "--- a/src/a.ts\n",
    "+++ b/src/a.ts\n",
    "@@ -1 +1 @@\n",
    "-a\n",
    "+b\n",
    "diff --git a/src/old name.ts b/src/new name.ts\n",
    "similarity index 90%\n",
    "rename from src/old name.ts\n",
    "rename to src/new name.ts\n",
    "index 3333333..4444444 100644\n",
    "--- a/src/old name.ts\t\n",
    "+++ b/src/new name.ts\t\n",
    "@@ -1 +1 @@\n",
    "-c\n",
    "+d\n",
    "diff --git a/gone.ts b/gone.ts\n",
    "deleted file mode 100644\n",
    "index 5555555..0000000\n",
    "--- a/gone.ts\n",
    "+++ /dev/null\n",
    "@@ -1 +0,0 @@\n",
    "-x\n",
    "diff --git \"a/caf\\303\\251.ts\" \"b/caf\\303\\251.ts\"\n",
    "index 6666666..7777777 100644\n",
    "@@ -1 +1 @@\n",
    "diff --git a/yarn.lock b/yarn.lock\n",
    "File excluded by pattern \"yarn.lock\"\n",
);

fn page(values: Value, next: Option<String>) -> String {
    let mut value = json!({"pagelen": 50, "values": values});
    if let Some(next) = next {
        value["next"] = json!(next);
    }
    value.to_string()
}

fn ok(body: impl Into<String>) -> Reply {
    reply(200, body)
}

fn rate_limited() -> Reply {
    let mut answer = reply(429, r#"{"error":{"message":"slow down"}}"#);
    answer.headers.push(("retry-after".into(), RETRY_AFTER.into()));
    answer
}

fn route(seen: &Seen, base: &str) -> Reply {
    let (path, query) = seen.target.split_once('?').unwrap_or((seen.target.as_str(), ""));
    let next = |rest: &str| Some(format!("{base}{rest}"));
    let path = path.strip_prefix("/2.0").unwrap_or(path);
    if path.starts_with("/__case/") {
        return ok("{}");
    }
    if seen.method != "GET" {
        return if path == "/repositories/acme/web/pullrequests/4/merge" {
            reply(409, r#"{"error":{"message":"conflict"}}"#)
        } else {
            ok("{}")
        };
    }
    let second_page = query.contains("page=2");
    match path {
        "/user" => ok(json!({"nickname": " avery ", "display_name": "Avery Stone"}).to_string()),
        "/user/permissions/repositories" => {
            let filter = url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default();
            if filter.contains("acme/api") {
                reply(410, "{}")
            } else if filter.contains("acme/web") {
                ok(page(json!([{"type": "repository_permission", "permission": " Read "}]), None))
            } else {
                ok(page(json!([]), None))
            }
        }
        "/repositories/acme/web/pullrequests" if second_page => ok(page(
            json!([
                pull_request("acme/web", 4, json!({"state": "MERGED"})),
                pull_request("acme/web", 5, json!({"draft": true}))
            ]),
            None,
        )),
        "/repositories/acme/web/pullrequests" => ok(page(
            json!([
                rich_pull_request(),
                {"id": "not a number"},
                pull_request("acme/web", 2, json!({"state": "DECLINED", "source": {"branch": {"name": "x"}, "repository": null}})),
                pull_request("acme/web", 3, json!({"state": "superseded", "draft": null})),
                pull_request("acme/web", 6, json!({"state": "SUPERSEDED", "reviewers": [{"nickname": "quinn"}]})),
            ]),
            next("/repositories/acme/web/pullrequests?page=2"),
        )),
        "/repositories/acme/far/pullrequests" => ok(page(
            json!([]),
            Some("https://elsewhere.example.test/2.0/repositories/acme/far/pullrequests?page=2".into()),
        )),
        "/repositories/acme/web/pullrequests/1" => ok(rich_pull_request().to_string()),
        "/repositories/acme/web/pullrequests/1/diffstat" if second_page => ok(page(json!([{"lines_added": 4, "lines_removed": 7}]), None)),
        "/repositories/acme/web/pullrequests/1/diffstat" => ok(page(
            json!([{"lines_added": 9, "lines_removed": 2}, {"lines_added": null}, "junk"]),
            next("/repositories/acme/web/pullrequests/1/diffstat?pagelen=50&page=2"),
        )),
        "/repositories/acme/web/pullrequests/1/conflicts" => ok(page(json!([]), None)),
        "/repositories/acme/web/pullrequests/1/statuses" if second_page => ok(page(json!([{"key": "x", "name": "Coverage", "state": "something"}]), None)),
        "/repositories/acme/web/pullrequests/1/statuses" => ok(page(
            json!([
                {"key": "build", "name": "Pipeline", "state": "INPROGRESS"},
                {"key": "build", "name": "Pipeline", "state": "SUCCESSFUL", "url": " https://ci.example.test/1 "},
                {"key": "deploy", "name": "Pipeline", "state": "FAILED", "description": " Deploy failed "},
                {"key": "lint"},
                {"state": "STOPPED"},
            ]),
            next("/repositories/acme/web/pullrequests/1/statuses?pagelen=50&page=2"),
        )),
        "/repositories/acme/web/pullrequests/1/comments" if second_page => ok(page(
            json!([
                {"id": 16, "content": {"raw": "Done."}, "user": {"nickname": "robin"}, "created_on": "2026-06-16T08:00:00+00:00", "parent": {"id": 10}},
                {"id": 17, "content": {"raw": "Thanks"}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T07:30:00+00:00", "parent": {"id": 16}},
                {"id": 18, "content": {"raw": "Orphan reply"}, "created_on": "2026-06-16T09:00:00+00:00", "parent": {"id": 999}},
                {"id": 19, "content": {"raw": "Line zero"}, "created_on": "2026-06-16T09:30:00+00:00", "inline": {"path": "src/zero.ts", "to": 0}},
            ]),
            None,
        )),
        "/repositories/acme/web/pullrequests/1/comments" => ok(page(
            json!([
                {"id": 10, "content": {"raw": "Rename this."}, "user": {"nickname": "avery"}, "created_on": "2026-06-16T06:00:00+00:00",
                 "inline": {"path": "src/a.ts", "to": 12, "from": null}, "links": {"html": {"href": "https://bitbucket.example.test/acme/web/pull-requests/1/_/diff#comment-10"}}},
                {"id": 11, "content": {"raw": "Gone"}, "created_on": "2026-06-16T06:10:00+00:00", "deleted": true},
                {"id": 12, "content": {"raw": "Draft"}, "created_on": "2026-06-16T06:20:00+00:00", "pending": true},
                {"id": 13, "content": {"raw": "  "}, "created_on": "2026-06-16T06:30:00+00:00"},
                {"id": 14, "content": {"raw": "General remark"}, "user": {"display_name": "Release Bot"}, "created_on": "2026-06-16T07:00:00.123+00:00"},
                {"id": 15, "content": {"raw": "Why remove this?"}, "user": {"nickname": "quinn"}, "created_on": "2026-06-16T05:30:00+00:00",
                 "inline": {"path": "src/old.ts", "from": 4, "outdated": true}, "resolution": {"type": "pullrequest_comment_resolution"}},
                {"id": "bad"},
            ]),
            next("/repositories/acme/web/pullrequests/1/comments?pagelen=50&page=2"),
        )),
        "/repositories/acme/web/pullrequests/1/commits" if second_page => ok(page(
            json!([
                {"hash": " bbb2222 ", "message": null, "date": "2026-06-14T04:00:00Z"},
                {"hash": "aaa1111", "message": "First", "date": "2026-06-13T04:00:00+00:00", "author": null},
            ]),
            None,
        )),
        "/repositories/acme/web/pullrequests/1/commits" => ok(page(
            json!([
                {"hash": "ddd4444", "message": "Fourth\n\nbody", "date": "2026-06-16T04:00:00+00:00",
                 "author": {"raw": "Avery Stone <avery@example.test>", "user": {"nickname": "avery", "display_name": "Avery Stone"}}},
                {"hash": "ccc3333", "message": "Third", "date": "2026-06-15T04:00:00+00:00", "author": {"raw": " Robin Example <robin@example.test> "}},
                {"hash": "eee5555", "date": null},
            ]),
            next("/repositories/acme/web/pullrequests/1/commits?pagelen=50&page=2"),
        )),
        "/repositories/acme/web/pullrequests/1/diff" => Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: PATCH.into(),
        },
        "/repositories/acme/web/diff/a1b2c3d4e5f6" => Reply {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: "diff --git a/one.ts b/one.ts\nindex 1234567..89abcde 100644\n@@ -1 +1 @@\n".into(),
        },
        "/workspaces/acme/members" if second_page => ok(page(json!([]), None)),
        "/workspaces/acme/members" => ok(page(
            json!([
                {"user": {"uuid": "{avery}", "nickname": "avery", "display_name": "Avery Stone"}},
                {"user": {"uuid": "{quinn}", "nickname": "quinn", "display_name": "Quinn Reyes"}},
                {"user": {"uuid": "{release-bot}", "display_name": "Release Bot"}},
                {"user": {"nickname": "no-uuid"}},
                {"user": null},
                "junk",
                {"user": {"uuid": "{robin}", "nickname": "robin", "links": {"avatar": {"href": "https://avatars.example.test/robin.png"}}}},
            ]),
            next("/workspaces/acme/members?page=2"),
        )),
        // A merged draft on another repository, whose optional reads fail.
        "/repositories/acme/api/pullrequests/2" => ok(pull_request("acme/api", 2, json!({"state": "MERGED", "draft": true})).to_string()),
        "/repositories/acme/api/pullrequests/2/diffstat" => ok(page(json!([]), None)),
        "/repositories/acme/api/pullrequests/2/conflicts" => ok(page(json!([{"path": "src/a.ts"}]), None)),
        "/repositories/acme/api/pullrequests/2/statuses" => reply(500, "{}"),
        "/repositories/acme/api/pullrequests/2/comments" => reply(403, "{}"),
        "/repositories/acme/api/pullrequests/2/commits" => rate_limited(),
        // A pull request Bitbucket answers with something unreadable.
        "/repositories/acme/web/pullrequests/3" => ok(r#"{"id":3}"#),
        // Credentials Bitbucket refuses on one read.
        "/repositories/acme/web/pullrequests/4" => ok(pull_request("acme/web", 4, json!({})).to_string()),
        "/repositories/acme/web/pullrequests/4/diffstat" => reply(401, "{}"),
        // A conversation whose next page points away from the configured Bitbucket.
        "/repositories/acme/web/pullrequests/6" => ok(pull_request("acme/web", 6, json!({})).to_string()),
        "/repositories/acme/web/pullrequests/6/comments" => ok(page(json!([]), Some("https://elsewhere.example.test/2.0/comments?page=2".into()))),
        "/repositories/acme/web/pullrequests/6/commits" => ok(page(json!([]), None)),
        // Timestamps in every shape Bitbucket might send, and a conversation page that is not one.
        "/repositories/acme/web/pullrequests/7" => ok(pull_request(
            "acme/web",
            7,
            json!({
                "created_on": "2026-06-16",
                "updated_on": "2026-06-16T05:04:32",
                "participants": [
                    {"user": {"nickname": "quinn"}, "state": "approved", "participated_on": "not a date"},
                    {"user": {"nickname": "robin"}, "approved": true, "participated_on": "2026-06-16T05:04:32.1234567-05:30"},
                ],
            }),
        )
        .to_string()),
        "/repositories/acme/web/pullrequests/7/comments" => ok(json!({"values": [], "size": 1.5}).to_string()),
        "/repositories/acme/web/pullrequests/7/commits" => {
            ok(json!({"values": [{"hash": "abc1234", "message": "Only", "date": "2026-06-16T05:04:32Z"}], "next": "  "}).to_string())
        }
        // Threads that lean on the edges of reassembly: a comment numbered -1, which every
        // parentless comment is read as answering, and a parent cycle.
        "/repositories/acme/web/pullrequests/8" => ok(pull_request("acme/web", 8, json!({})).to_string()),
        "/repositories/acme/web/pullrequests/8/comments" => ok(page(
            json!([
                {"id": -1, "content": {"raw": "Minus one"}, "created_on": "2026-06-16T05:00:00+00:00", "inline": {"path": "src/minus.ts", "to": 2}},
                {"id": 30, "content": {"raw": "No parent key"}, "created_on": "2026-06-16T04:00:00+00:00"},
                {"id": 31, "content": {"raw": "Null parent"}, "created_on": "2026-06-16T03:00:00+00:00", "parent": null, "inline": {"path": " src/null.ts ", "from": 3, "to": null}},
                {"id": 20, "content": {"raw": "Cycle a"}, "created_on": "2026-06-16T06:00:00+00:00", "parent": {"id": 21}, "inline": {"path": "src/cycle.ts", "to": 1}},
                {"id": 21, "content": {"raw": "Cycle b"}, "created_on": "2026-06-16T06:30:00+00:00", "parent": {"id": 20}, "inline": {"path": "src/cycle.ts", "to": 1}},
                {"id": 32, "content": {"raw": "Duplicate first"}, "created_on": "2026-06-16T07:00:00+00:00", "parent": null, "inline": {"path": "src/dup.ts", "to": 5}},
                {"id": 32, "content": {"raw": "Duplicate second"}, "created_on": "2026-06-16T07:10:00+00:00", "parent": null, "inline": {"path": "src/dup2.ts", "to": 6}},
                {"id": 33, "content": {"raw": "Bad inline"}, "created_on": "2026-06-16T07:20:00+00:00", "inline": {"path": "src/x.ts", "to": "seven"}},
            ]),
            None,
        )),
        "/repositories/acme/web/pullrequests/8/commits" => ok(page(json!([]), None)),
        "/repositories/acme/odd/pullrequests" => ok(json!({"values": [], "size": "many"}).to_string()),
        _ => reply(404, r#"{"type":"error","error":{"message":"Not found"}}"#),
    }
}

// ---------------------------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------------------------

fn reference(repository: &str, number: i64) -> Value {
    json!({"cwd": "/work/web", "repository": repository, "host": "bitbucket.org", "number": number})
}

fn case(id: &str, method: &str, input: Value) -> Value {
    json!({"id": id, "method": method, "input": input})
}

fn racy(id: &str, method: &str, input: Value) -> Value {
    json!({"id": id, "method": method, "input": input, "racy": true})
}

fn with(repository: &str, number: i64, extra: Value) -> Value {
    merge(reference(repository, number), extra)
}

fn list_input(repository: &str, extra: Value) -> Value {
    merge(
        json!({"cwd": "/work/web", "repository": repository, "host": "bitbucket.org", "state": "open", "involvement": "all", "viewer": "avery", "limit": 50}),
        extra,
    )
}

fn cases() -> Vec<Value> {
    let review_comments = json!([
        {"path": "src/a.ts", "position": {"kind": "added", "newLine": 3}, "body": "Nice."},
        {"path": "src/old.ts", "position": {"kind": "deleted", "oldLine": 4}, "body": "Why remove?"},
        {"path": "src/a.ts", "position": {"kind": "context", "oldLine": 5, "newLine": 6, "side": "left"}, "body": "Left."},
        {"path": "src/a.ts", "position": {"kind": "context", "oldLine": 5, "newLine": 6, "side": "right"}, "body": "Right \"quoted\"."},
    ]);
    vec![
        case("viewer", "getViewer", json!({"cwd": "/work/web", "host": "bitbucket.org"})),
        case("list-open", "listChangeRequests", list_input("acme/web", json!({}))),
        case("list-short", "listChangeRequests", list_input("acme/web", json!({"limit": 2}))),
        case("list-zero", "listChangeRequests", list_input("acme/web", json!({"limit": 0}))),
        case(
            "list-all-search",
            "listChangeRequests",
            list_input(
                "acme/web",
                json!({"state": "all", "query": " a \"quoted\" \\ term ", "cursor": {"updatedBefore": "2026-07-02T00:00:00.123Z", "delivered": 50}}),
            ),
        ),
        case(
            "list-closed",
            "listChangeRequests",
            list_input("acme/web", json!({"state": "closed", "query": "   "})),
        ),
        case(
            "list-merged",
            "listChangeRequests",
            list_input("acme/web", json!({"state": "merged", "limit": 3})),
        ),
        case("list-unsupported", "listChangeRequests", list_input("acme/team/web", json!({}))),
        case("list-untrusted", "listChangeRequests", list_input("acme/far", json!({}))),
        case("detail", "getChangeRequest", reference("acme/web", 1)),
        case("detail-recovered", "getChangeRequest", reference("acme/api", 2)),
        racy("detail-unreadable", "getChangeRequest", reference("acme/web", 3)),
        racy("detail-unauthenticated", "getChangeRequest", reference("acme/web", 4)),
        racy("detail-missing", "getChangeRequest", reference("acme/web", 5)),
        case("detail-unsupported", "getChangeRequest", reference("acme", 1)),
        case("activity", "getChangeRequestActivity", reference("acme/web", 1)),
        racy("activity-rate-limited", "getChangeRequestActivity", reference("acme/api", 2)),
        case("activity-untrusted-page", "getChangeRequestActivity", reference("acme/web", 6)),
        case("activity-odd-dates", "getChangeRequestActivity", reference("acme/web", 7)),
        case("activity-odd-threads", "getChangeRequestActivity", reference("acme/web", 8)),
        case("list-odd-page", "listChangeRequests", list_input("acme/odd", json!({}))),
        case("permissions-read", "getViewerPermissions", reference("acme/web", 1)),
        case("permissions-removed", "getViewerPermissions", reference("acme/api", 2)),
        case("permissions-unsupported", "getViewerPermissions", reference("acme/x/y", 2)),
        case("diff", "getDiff", reference("acme/web", 1)),
        case("diff-commit", "getDiff", with("acme/web", 1, json!({"commit": "a1b2c3d4e5f6"}))),
        case("diff-not-a-sha", "getDiff", with("acme/web", 1, json!({"commit": "not-a-sha"}))),
        case("diff-missing", "getDiff", reference("acme/web", 5)),
        case(
            "revisions",
            "getFileRevisions",
            with("acme/web", 1, json!({"paths": ["src/a.ts", "missing.ts", "gone.ts"]})),
        ),
        case("revisions-cached", "getFileRevisions", with("acme/web", 1, json!({"paths": ["src/b.ts"]}))),
        case("revisions-none", "getFileRevisions", with("acme/web", 1, json!({"paths": []}))),
        case("revisions-missing", "getFileRevisions", with("acme/web", 5, json!({"paths": ["src/a.ts"]}))),
        case("reviewer-candidates", "listReviewerCandidates", reference("acme/web", 1)),
        case(
            "merge-rebase",
            "runAction",
            with("acme/web", 1, json!({"action": "merge", "mergeMethod": "rebase"})),
        ),
        case("merge-default", "runAction", with("acme/web", 1, json!({"action": "merge"}))),
        case(
            "merge-squash",
            "runAction",
            with("acme/web", 1, json!({"action": "merge", "mergeMethod": "squash"})),
        ),
        case(
            "merge-conflict",
            "runAction",
            with("acme/web", 4, json!({"action": "merge", "mergeMethod": "merge"})),
        ),
        case("close", "runAction", with("acme/web", 1, json!({"action": "close"}))),
        case("edit-title", "updateChangeRequest", with("acme/web", 1, json!({"title": "A new title"}))),
        case(
            "edit-body",
            "updateChangeRequest",
            with("acme/web", 1, json!({"body": "New body — with \"quotes\".\n"})),
        ),
        case("edit-both", "updateChangeRequest", with("acme/web", 1, json!({"title": "Both", "body": ""}))),
        case("comment", "comment", with("acme/web", 1, json!({"body": "Looks \"good\" — ship it\n\ttabbed"}))),
        case(
            "edit-comment",
            "updateComment",
            with("acme/web", 1, json!({"commentId": "10", "kind": "review-comment", "body": "Edited."})),
        ),
        case(
            "review-approve",
            "submitReview",
            with("acme/web", 1, json!({"verdict": "approve", "body": "  ", "comments": review_comments})),
        ),
        case(
            "review-comment",
            "submitReview",
            with("acme/web", 1, json!({"verdict": "comment", "body": "Summary.", "comments": []})),
        ),
        case(
            "review-changes",
            "submitReview",
            with("acme/web", 1, json!({"verdict": "request-changes", "body": "Two things.", "comments": []})),
        ),
        case(
            "request-reviewer",
            "setReviewerRequest",
            with("acme/web", 1, json!({"reviewers": [{"id": "{robin}", "kind": "user"}], "requested": true})),
        ),
        case(
            "unrequest-reviewer",
            "setReviewerRequest",
            with(
                "acme/web",
                1,
                json!({"reviewers": [{"id": "{quinn}", "kind": "user"}, {"id": "{nobody}", "kind": "user"}], "requested": false}),
            ),
        ),
        case("reply", "replyToThread", with("acme/web", 1, json!({"threadId": "10", "body": "Fixed."}))),
        case(
            "reply-odd-id",
            "replyToThread",
            with("acme/web", 1, json!({"threadId": "abc", "body": "Fixed."})),
        ),
        case(
            "resolve",
            "setThreadResolution",
            with("acme/web", 1, json!({"threadId": "10", "resolved": true})),
        ),
        case(
            "unresolve",
            "setThreadResolution",
            with("acme/web", 1, json!({"threadId": "10", "resolved": false})),
        ),
        case(
            "react",
            "setReaction",
            with("acme/web", 1, json!({"subjectId": "10", "content": "heart", "reacted": true})),
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// The Rust side, and its results in the TS object shape
// ---------------------------------------------------------------------------------------------

fn change_request_ref(input: &Value) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: input["cwd"].as_str().unwrap().into(),
        repository: input["repository"].as_str().unwrap().into(),
        host: input["host"].as_str().unwrap().into(),
        number: input["number"].as_i64().unwrap(),
    }
}

fn str_of(input: &Value, key: &str) -> String {
    input[key].as_str().unwrap().to_owned()
}

fn optional_str(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn from<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

/// Inserts `value` under `key`, leaving the key out when the TS object would not carry it.
fn put<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<T>) {
    if let Some(value) = value {
        map.insert(key.into(), serde_json::to_value(value).unwrap());
    }
}

fn change_request_json(change_request: &ProviderChangeRequest) -> Map<String, Value> {
    let mut map = Map::new();
    put(&mut map, "stack", change_request.stack.as_ref());
    map.insert("number".into(), json!(change_request.number));
    map.insert("title".into(), json!(change_request.title));
    map.insert("url".into(), json!(change_request.url));
    map.insert("author".into(), json!(change_request.author));
    map.insert("headBranch".into(), json!(change_request.head_branch));
    put(&mut map, "headRepositoryNameWithOwner", change_request.head_repository_name_with_owner.as_ref());
    map.insert("baseBranch".into(), json!(change_request.base_branch));
    map.insert("state".into(), json!(change_request.state));
    map.insert("isDraft".into(), json!(change_request.is_draft));
    map.insert("mergeability".into(), json!(change_request.mergeability));
    map.insert("additions".into(), json!(change_request.additions));
    map.insert("deletions".into(), json!(change_request.deletions));
    map.insert("createdAt".into(), json!(change_request.created_at));
    put(&mut map, "closedAt", change_request.closed_at.as_ref());
    put(&mut map, "mergedAt", change_request.merged_at.as_ref());
    map.insert("updatedAt".into(), json!(change_request.updated_at));
    map.insert("reviewRequestLogins".into(), json!(change_request.review_request_logins));
    map.insert("labels".into(), json!(change_request.labels));
    put(&mut map, "reviewDecision", change_request.review_decision.as_ref());
    put(&mut map, "checksState", change_request.checks_state.as_ref());
    map
}

fn detail_json(detail: &ProviderChangeRequestDetail) -> Value {
    let mut map = change_request_json(&detail.change_request);
    map.insert("changedFiles".into(), json!(detail.changed_files));
    map.insert("body".into(), json!(detail.body));
    map.insert("mergedAt".into(), json!(detail.merged_at));
    map.insert("closedAt".into(), json!(detail.closed_at));
    map.insert("reviewers".into(), json!(detail.reviewers));
    map.insert("checks".into(), json!(detail.checks));
    map.insert("mergeCapabilities".into(), json!(detail.merge_capabilities));
    map.insert("viewerPermissions".into(), json!(detail.viewer_permissions));
    put(&mut map, "baseComparison", detail.base_comparison.as_ref());
    put(&mut map, "behindBy", detail.behind_by);
    put(&mut map, "autoMergeEnabled", detail.auto_merge_enabled);
    put(&mut map, "autoMergeMethod", detail.auto_merge_method.as_ref());
    put(&mut map, "workflowApprovalsRequired", detail.workflow_approvals_required);
    Value::Object(map)
}

fn activity_json(activity: &ProviderChangeRequestActivity) -> Value {
    let mut map = Map::new();
    put(&mut map, "author", activity.author.as_ref());
    put(&mut map, "reviewers", activity.reviewers.as_ref());
    map.insert("comments".into(), json!(activity.comments));
    map.insert("commentCount".into(), json!(activity.comment_count));
    map.insert("commentsTruncated".into(), json!(activity.comments_truncated));
    map.insert("reviewThreads".into(), json!(activity.review_threads));
    map.insert("commits".into(), json!(activity.commits));
    put(&mut map, "reactions", activity.reactions.as_ref());
    Value::Object(map)
}

fn outcome<T>(result: ProviderResult<T>, to_json: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({"ok": to_json(value)}),
        Err(error) => json!({"error": error.to_wire()}),
    }
}

async fn run(provider: &BitbucketPullRequestProvider, case: &Value) -> Value {
    let input = &case["input"];
    match case["method"].as_str().unwrap() {
        "getViewer" => outcome(
            provider
                .get_viewer(ProviderHostRef {
                    cwd: str_of(input, "cwd"),
                    host: optional_str(input, "host"),
                })
                .await,
            |viewer| json!(viewer),
        ),
        "listChangeRequests" => {
            let request = ListChangeRequestsInput {
                cwd: str_of(input, "cwd"),
                repository: str_of(input, "repository"),
                host: str_of(input, "host"),
                state: from(&input["state"]),
                involvement: from(&input["involvement"]),
                viewer: str_of(input, "viewer"),
                limit: input["limit"].as_i64().unwrap(),
                query: optional_str(input, "query"),
                cursor: input.get("cursor").map(|cursor| ProviderListCursor {
                    updated_before: str_of(cursor, "updatedBefore"),
                    delivered: cursor["delivered"].as_i64().unwrap(),
                }),
                filters: None,
            };
            outcome(provider.list_change_requests(request).await, |page| {
                let mut map = Map::new();
                map.insert(
                    "items".into(),
                    Value::Array(page.items.iter().map(|item| Value::Object(change_request_json(item))).collect()),
                );
                map.insert("truncated".into(), json!(page.truncated));
                put(&mut map, "cursorAdvance", page.cursor_advance);
                map.insert("continues".into(), json!(page.continues));
                Value::Object(map)
            })
        }
        "getChangeRequest" => outcome(provider.get_change_request(change_request_ref(input)).await, |detail| detail_json(&detail)),
        "getChangeRequestActivity" => outcome(provider.get_change_request_activity(change_request_ref(input)).await, |activity| {
            activity_json(&activity)
        }),
        "getViewerPermissions" => outcome(
            provider
                .get_viewer_permissions(ViewerPermissionsInput {
                    change_request: change_request_ref(input),
                    include_update_branch: None,
                })
                .await,
            |permissions| json!(permissions),
        ),
        "getDiff" => outcome(
            provider
                .get_diff(GetDiffInput {
                    change_request: change_request_ref(input),
                    cursor: None,
                    commit: optional_str(input, "commit"),
                })
                .await,
            |slice| {
                let mut map = Map::new();
                map.insert("patch".into(), json!(slice.patch));
                map.insert("truncated".into(), json!(slice.truncated));
                map.insert("nextCursor".into(), json!(slice.next_cursor));
                put(&mut map, "omittedFileStats", slice.omitted_file_stats);
                Value::Object(map)
            },
        ),
        "getFileRevisions" => outcome(
            provider
                .get_file_revisions(FileRevisionsInput {
                    change_request: change_request_ref(input),
                    paths: from(&input["paths"]),
                })
                .await,
            |revisions| {
                let mut map = Map::new();
                map.insert("revisions".into(), json!(revisions.revisions));
                put(&mut map, "complete", revisions.complete);
                Value::Object(map)
            },
        ),
        "listReviewerCandidates" => outcome(provider.list_reviewer_candidates(change_request_ref(input)).await, |list| json!(list)),
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
            |()| Value::Null,
        ),
        "updateChangeRequest" => outcome(
            provider
                .update_change_request(UpdateChangeRequestInput {
                    change_request: change_request_ref(input),
                    title: optional_str(input, "title"),
                    body: optional_str(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "comment" => outcome(
            provider
                .comment(CommentInput {
                    change_request: change_request_ref(input),
                    body: str_of(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "updateComment" => outcome(
            provider
                .update_comment(UpdateCommentInput {
                    change_request: change_request_ref(input),
                    comment_id: str_of(input, "commentId"),
                    kind: from(&input["kind"]),
                    body: str_of(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "submitReview" => outcome(
            provider
                .submit_review(SubmitReviewInput {
                    change_request: change_request_ref(input),
                    verdict: from(&input["verdict"]),
                    body: str_of(input, "body"),
                    comments: from::<Vec<PullRequestReviewCommentDraft>>(&input["comments"]),
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
                            id: str_of(reviewer, "id"),
                            kind: from(&reviewer["kind"]),
                        })
                        .collect(),
                    requested: input["requested"].as_bool().unwrap(),
                })
                .await,
            |()| Value::Null,
        ),
        "replyToThread" => outcome(
            provider
                .reply_to_thread(ReplyToThreadInput {
                    change_request: change_request_ref(input),
                    thread_id: str_of(input, "threadId"),
                    body: str_of(input, "body"),
                })
                .await,
            |()| Value::Null,
        ),
        "setThreadResolution" => outcome(
            provider
                .set_thread_resolution(SetThreadResolutionInput {
                    change_request: change_request_ref(input),
                    thread_id: str_of(input, "threadId"),
                    resolved: input["resolved"].as_bool().unwrap(),
                })
                .await,
            |()| Value::Null,
        ),
        "setReaction" => outcome(
            provider
                .set_reaction(SetReactionInput {
                    change_request: change_request_ref(input),
                    subject_id: optional_str(input, "subjectId"),
                    content: from(&input["content"]),
                    reacted: input["reacted"].as_bool().unwrap(),
                })
                .await,
            |()| Value::Null,
        ),
        other => panic!("unknown method {other}"),
    }
}

/// The requests of each case, split at the `__case/<id>` markers both sides send after a case.
fn requests_by_case(seen: &[Seen]) -> Vec<(String, Vec<String>)> {
    let mut cases = Vec::new();
    let mut current = Vec::new();
    for request in seen {
        match request.target.strip_prefix("/2.0/__case/") {
            Some(id) => {
                let id = url::form_urlencoded::parse(format!("id={id}").as_bytes())
                    .next()
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                current.sort();
                cases.push((id, std::mem::take(&mut current)));
            }
            None => current.push(format!("{} {} {}", request.method, request.target, request.body)),
        }
    }
    cases
}

#[tokio::test]
async fn rust_matches_the_typescript_bitbucket_provider() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let base: Arc<OnceLock<String>> = Arc::default();
    let handler_base = base.clone();
    let server = MockServer::start(move |seen| route(seen, handler_base.get().map(String::as_str).unwrap_or_default())).await;
    base.set(server.base.clone()).unwrap();
    let cases = cases();

    let (oracle_cases, oracle_base) = (cases.clone(), server.base.clone());
    let ts = tokio::task::spawn_blocking(move || run_oracle(&oracle_cases, &oracle_base)).await.unwrap();
    let ts_seen = server.seen();

    let worktrees = tempfile::tempdir().unwrap();
    let bitbucket = BitbucketApi::new(
        BitbucketApiConfig {
            base_url: server.base.clone(),
            access_token: None,
            email: Some(EMAIL.into()),
            api_token: Some(API_TOKEN.into()),
        },
        Arc::new(StaticBitbucketSettings(None)),
        system_clock(),
        GitVcsDriver::new(worktrees.path()),
        VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default()))),
    );
    let provider = BitbucketPullRequestProvider::new(bitbucket, system_clock());
    let marker = reqwest::Client::new();
    let mut mismatches = Vec::new();
    for case in &cases {
        let id = case["id"].as_str().unwrap();
        let mut expected = ts.get(id).cloned().unwrap_or(Value::Null);
        let mut actual = run(&provider, case).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        marker.get(format!("{}/__case/{id}", server.base)).send().await.unwrap();
        for value in [&mut expected, &mut actual] {
            if let Some(error) = value.get_mut("error") {
                normalize(error, 0);
            }
        }
        if expected != actual {
            mismatches.push(format!("{id}:\n  ts:   {expected}\n  rust: {actual}"));
        }
    }

    let all = server.seen();
    let ts_requests = requests_by_case(&ts_seen);
    let rust_requests = requests_by_case(&all[ts_seen.len()..]);
    assert_eq!(ts_requests.len(), cases.len(), "the TS side marked every case");
    assert_eq!(rust_requests.len(), cases.len(), "the Rust side marked every case");
    for ((case, (ts_id, ts)), (rust_id, rust)) in cases.iter().zip(&ts_requests).zip(&rust_requests) {
        assert_eq!((ts_id.as_str(), rust_id.as_str()), (case["id"].as_str().unwrap(), case["id"].as_str().unwrap()));
        if case.get("racy").is_none() && ts != rust {
            mismatches.push(format!("{ts_id} requests:\n  ts:   {ts:#?}\n  rust: {rust:#?}"));
        }
    }
    assert!(mismatches.is_empty(), "{} differences:\n{}", mismatches.len(), mismatches.join("\n"));
    eprintln!("{} cases identical", cases.len());
}
