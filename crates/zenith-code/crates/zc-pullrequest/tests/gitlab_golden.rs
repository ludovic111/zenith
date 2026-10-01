//! Golden comparison against the TypeScript GitLab pull request code: the same fake `glab`
//! (recorded outputs keyed by working directory, argv and piped body) answers both sides, the TS
//! provider runs over the real `VcsProcess` (`golden/gitlab_oracle.mjs`, through node and the real
//! effect/contracts packages), and every read method, a set of writes, the CLI's diff file
//! contents and the pure decoders must give the same JSON, errors included. The `glab` calls
//! each side made are compared too.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Nested error causes are compared by name
//! below the first level (see [`shape::normalize`]).

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_gitlab;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Map, Value};
use support_gitlab::fake::FakeClis;
use support_gitlab::shape::{self, to_json};
use zc_contracts::{PullRequestReviewCommentDraft, PullRequestReviewerKind};
use zc_pullrequest::gitlab::cli::{DiffFileContentsInput, MergeRequestTarget};
use zc_pullrequest::gitlab::json::{self as gitlab_json, AWARD_EMOJI_GRAPHQL_QUERY, REPOSITORY_BLOBS_GRAPHQL_QUERY};
use zc_pullrequest::gitlab::{GitLabPullRequestCli, GitLabPullRequestProvider};
use zc_pullrequest::provider::*;
use zc_sourcecontrol::gitlab::GitLabCli;

const JSON_BODY: &str = "--input - --header Content-Type: application/json";

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

fn run_oracle(cases: &[Value], bin: &Path) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/gitlab_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
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

// ---------------------------------------------------------------------------------------------
// The recorded host.

fn mr(number: i64, extra: Value) -> Value {
    let mut value = json!({
        "iid": number,
        "title": format!("Merge request {number}"),
        "web_url": format!("https://gitlab.example.test/acme/web/-/merge_requests/{number}"),
        "source_branch": format!("feat/{number}"),
        "target_branch": "main",
        "created_at": "2026-07-01T00:00:00Z",
        "updated_at": format!("2026-07-{:02}T00:00:00Z", 1 + number % 27),
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn mr_path(number: i64) -> String {
    format!("projects/acme%2Fweb/merge_requests/{number}")
}

fn detail_7() -> Value {
    mr(
        7,
        json!({
            "title": "Add the merge requests page",
            "description": "Ships the page.",
            "author": {"id": 1, "username": "bilal", "name": "Bilal Example", "avatar_url": "https://avatars.example.test/b.png"},
            "state": "opened",
            "draft": false,
            "merge_status": "cannot_be_merged",
            "has_conflicts": false,
            "merged_at": null,
            "closed_at": null,
            "reviewers": [{"id": 5, "username": "octo", "name": "Octo Example"}, {"id": 9, "username": "hubot", "avatar_url": " "}],
            "labels": ["backend", " ui ", ""],
            "changes_count": "1000+",
            "head_pipeline": {"status": "manual", "web_url": "https://gitlab.example.test/acme/web/-/pipelines/9", "source": "push"},
            "user": {"can_merge": false},
            "merge_when_pipeline_succeeds": false,
            "auto_merge_enabled": true,
            "squash_on_merge": true,
            "squash": true,
            "diverged_commits_count": 2,
            "diff_refs": {"base_sha": "base7", "head_sha": "head7", "start_sha": "start7"},
        }),
    )
}

fn notes_page_1() -> Value {
    let mut notes = vec![
        json!({"id": 1000, "body": "assigned to @bilal", "system": true, "created_at": "2026-07-01T00:00:00Z"}),
        json!({"id": 1001, "type": "DiffNote", "body": "Rename this.", "author": {"username": "julius"}, "created_at": "2026-07-01T01:00:00Z", "position": {"new_path": null, "old_path": "src/old.ts"}}),
        json!({"id": 1002, "body": "   ", "created_at": "2026-07-01T02:00:00Z"}),
        json!({"id": "malformed", "body": "x", "created_at": "2026-07-01T02:00:00Z"}),
    ];
    notes.extend((4..100).map(|index| json!({"id": 1000 + index, "body": format!("note {index}"), "author": {"username": "julius", "name": " Julius "}, "created_at": "2026-07-02T00:00:00Z", "type": null})));
    Value::Array(notes)
}

fn award_body(cursor: Option<&str>) -> String {
    json!({"query": AWARD_EMOJI_GRAPHQL_QUERY, "variables": {"fullPath": "acme/web", "iid": "7", "cursor": cursor}}).to_string()
}

fn blobs_body(reference: &str, paths: &[String]) -> String {
    json!({"query": REPOSITORY_BLOBS_GRAPHQL_QUERY, "variables": {"fullPath": "acme/web", "ref": reference, "paths": paths}}).to_string()
}

fn diff_files_page_1() -> Value {
    let mut files = vec![
        json!({"old_path": r" old\name.ts ", "new_path": r" new\name.ts ", "renamed_file": true, "diff": ""}),
        json!({"old_path": "bin/run", "new_path": "bin/run", "new_file": true, "b_mode": "100755", "diff": "@@ -0,0 +1 @@\n+run"}),
        json!({"old_path": "src/gone.ts", "new_path": "src/gone.ts", "deleted_file": true, "a_mode": null, "diff": "@@ -1 +0,0 @@\n-bye\n"}),
        json!({"old_path": "big.bin", "new_path": "big.bin", "diff": "", "too_large": true}),
        json!({"old_path": "tab\there.txt", "new_path": "tab\there.txt", "diff": null, "collapsed": false}),
        json!({"new_path": "missing-old-path.ts", "diff": "@@ -1 +1 @@\n-a\n+b\n"}),
    ];
    files.extend((6..100).map(|index| json!({"old_path": format!("src/{index}.ts"), "new_path": format!("src/{index}.ts"), "diff": "@@ -1 +1 @@\n-a\n+b\n"})));
    Value::Array(files)
}

fn record(clis: &FakeClis) {
    let glab = |cwd: &str, args: &str, stdin: Option<&str>, stdout: &str| clis.respond("glab", cwd, args, stdin, stdout, "", 0);
    let fail = |cwd: &str, args: &str, stderr: &str| clis.respond("glab", cwd, args, None, "", stderr, 1);

    // Viewer.
    glab("ws", "api user", None, r#"{"username":"bilal","id":1}"#);
    glab("ws-noname", "api user", None, r#"{"username":"  "}"#);
    fail("ws-401", "api user", "glab: 401 Unauthorized");
    glab("ws-broken", "api user", None, r#"{"username":42}"#);

    // Listings.
    let rows =
        json!([mr(1, json!({"author": {"username": "bilal"}, "labels": ["a"]})), {"iid": "bad"}, mr(2, json!({"work_in_progress": true, "state": "closed"}))]);
    glab(
        "ws",
        "api projects/acme%2Fweb/merge_requests?state=opened&author_username=bilal&order_by=updated_at&sort=desc&per_page=3&page=1",
        None,
        &rows.to_string(),
    );
    glab(
        "ws",
        "api projects/acme%2Fweb/merge_requests?state=merged&reviewer_username=bilal&search=a%26b%20%22c%22&order_by=updated_at&sort=desc&per_page=3&page=2",
        None,
        &json!([mr(4, json!({"merged_at": "2026-07-09T00:00:00Z", "state": "opened"}))]).to_string(),
    );
    let page = |first: i64, count: i64| Value::Array((first..first + count).map(|number| mr(number, json!({}))).collect()).to_string();
    glab(
        "ws",
        "api projects/acme%2Fplatform%2Fweb/merge_requests?state=all&order_by=updated_at&sort=desc&per_page=100&page=1",
        None,
        &page(1, 100),
    );
    glab(
        "ws",
        "api projects/acme%2Fplatform%2Fweb/merge_requests?state=all&order_by=updated_at&sort=desc&per_page=100&page=2",
        None,
        &page(101, 10),
    );
    glab(
        "ws",
        "api projects/acme%2Fweb/merge_requests?state=closed&order_by=updated_at&sort=desc&per_page=11&page=1",
        None,
        "  \n",
    );
    glab(
        "ws-broken",
        "api projects/acme%2Fweb/merge_requests?state=opened&order_by=updated_at&sort=desc&per_page=11&page=1",
        None,
        "not json",
    );

    // Detail, permissions, candidates.
    glab(
        "ws",
        &format!("api {}?include_diverged_commits_count=true", mr_path(7)),
        None,
        &detail_7().to_string(),
    );
    glab(
        "ws",
        "api projects/acme%2Fweb?license=false",
        None,
        r#"{"merge_method":"rebase_merge","squash_option":"default_off"}"#,
    );
    fail("ws", &format!("api {}?include_diverged_commits_count=true", mr_path(8)), "404 Not Found");
    glab("ws", &format!("api {}", mr_path(7)), None, &detail_7().to_string());
    glab(
        "ws",
        "api projects/acme%2Fweb/users?per_page=100",
        None,
        &json!([
            {"id": 1, "username": "bilal"},
            {"id": 5, "username": "octo"},
            {"id": 9, "username": "hubot", "name": null},
            {"username": "noid"},
            {"id": 12, "username": " "},
            {"id": 13, "username": "kit", "name": " Kit ", "avatar_url": "https://avatars.example.test/k.png"},
            {"id": "14", "username": "stringid"},
        ])
        .to_string(),
    );

    // Activity.
    glab(
        "ws",
        &format!("api {}/notes?per_page=100&page=1&order_by=created_at&sort=asc", mr_path(7)),
        None,
        &notes_page_1().to_string(),
    );
    glab(
        "ws",
        &format!("api {}/notes?per_page=100&page=2&order_by=created_at&sort=asc", mr_path(7)),
        None,
        &json!([
            {"id": 2001, "body": "Second page.", "author": {"username": "kit", "avatar_url": "https://avatars.example.test/k.png"}, "created_at": "2026-07-03T00:00:00Z"},
            {"id": 2002, "type": " DiffNote ", "body": "On a line.", "created_at": "2026-07-03T01:00:00Z", "position": {"new_path": " src/a.ts ", "old_path": "src/a.ts"}},
        ])
        .to_string(),
    );
    glab(
        "ws",
        &format!("api {}/commits?per_page=100&with_stats=true", mr_path(7)),
        None,
        &json!([
            {"id": " c2 ", "title": "second", "committed_date": "2026-07-02T00:00:00Z", "author_name": "Ada Example", "stats": {"additions": 3, "deletions": -1}},
            {"id": "c1", "created_at": "2026-07-01T00:00:00Z", "author_email": "ada@example.test", "parent_ids": ["c0"]},
            {"id": "  ", "committed_date": "2026-07-01T00:00:00Z"},
            {"id": "c0", "parent_ids": null, "committed_date": "2026-07-01T00:00:00Z"},
            {"id": "cx", "committed_date": " ", "created_at": null},
        ])
        .to_string(),
    );
    glab(
        "ws",
        &format!("api {}/discussions?per_page=100&page=1", mr_path(7)),
        None,
        &json!([
            {"id": "d-right", "notes": [
                {"id": 1001, "body": "Rename this.", "author": {"username": "julius"}, "created_at": "2026-07-01T01:00:00Z", "resolvable": true, "resolved": true,
                 "position": {"position_type": "text", "new_path": "src/a.ts", "old_path": "src/a.ts", "new_line": 12, "old_line": null}},
                {"id": 2001, "body": null, "created_at": "2026-07-01T02:00:00Z", "system": false},
            ]},
            {"id": "d-left", "notes": [
                {"id": 3001, "body": "Why?", "created_at": "2026-07-01T03:00:00Z", "resolved": null,
                 "position": {"position_type": "text", "new_path": "src/b.ts", "old_path": " src/old-b.ts ", "old_line": 0}},
            ]},
            {"id": "d-image", "notes": [{"id": 3002, "body": "img", "created_at": "2026-07-01T03:00:00Z", "position": {"position_type": "image", "new_path": "a.png"}}]},
            {"id": "d-plain", "notes": [{"id": 3003, "body": "ship it", "created_at": "2026-07-01Z"}]},
            {"id": "d-system-first", "notes": [
                {"id": 3004, "body": "changed the line", "system": true, "created_at": "2026-07-01T04:00:00Z"},
                {"id": 3005, "body": "After the system note.", "created_at": "2026-07-01T05:00:00Z", "position": {"position_type": "text", "new_path": "src/c.ts", "new_line": 3}},
            ]},
            {"id": 7, "notes": []},
            {"id": "d-bad-note", "notes": [{"id": 3006, "created_at": "2026-07-01T05:00:00Z", "resolvable": null}]},
        ])
        .to_string(),
    );
    glab(
        "ws",
        &format!("api graphql --method POST {JSON_BODY}"),
        Some(&award_body(None)),
        &json!({"data": {"currentUser": {"username": "Bilal"}, "project": {"mergeRequest": {
            "awardEmoji": {"nodes": [
                {"name": "thumbsup", "user": {"username": "bilal"}},
                {"name": " ThumbsUp ", "user": {"username": "julius"}},
                {"name": "partyparrot", "user": {"username": "kit"}},
                {"name": "rocket", "user": null},
                null,
            ]},
            "notes": {"pageInfo": {"hasNextPage": true, "endCursor": " c1 "}, "nodes": [
                {"id": "gid://gitlab/DiffNote/1001", "awardEmoji": {"nodes": [{"name": "heart", "user": {"username": "julius"}}, {"name": "tada", "user": {"username": "bilal"}}]}},
                {"id": "gid://gitlab/Note/1004", "awardEmoji": {"nodes": [{"name": "partyparrot", "user": {"username": "bilal"}}]}},
                {"id": "not-a-gid", "awardEmoji": {"nodes": [{"name": "eyes", "user": {"username": "kit"}}]}},
                null,
            ]},
        }}}})
        .to_string(),
    );
    glab(
        "ws",
        &format!("api graphql --method POST {JSON_BODY}"),
        Some(&award_body(Some("c1"))),
        &json!({"data": {"currentUser": {"username": "bilal"}, "project": {"mergeRequest": {
            "awardEmoji": {"nodes": [{"name": "eyes", "user": {"username": "kit"}}]},
            "notes": {"pageInfo": {"hasNextPage": false, "endCursor": "c2"}, "nodes": [
                {"id": "gid://gitlab/Note/2001", "awardEmoji": {"nodes": [{"name": "confused", "user": {"username": "kit"}}]}},
                {"id": "gid://gitlab/DiffNote/1001", "awardEmoji": {"nodes": [{"name": "rocket", "user": {"username": "kit"}}]}},
            ]},
        }}}})
        .to_string(),
    );

    // Diffs.
    glab(
        "ws",
        &format!("api {}/diffs?per_page=100&page=1", mr_path(7)),
        None,
        &diff_files_page_1().to_string(),
    );
    glab(
        "ws",
        &format!("api {}/diffs?per_page=100&page=2", mr_path(7)),
        None,
        &json!([{"old_path": "z.ts", "new_path": "z.ts", "diff": "@@ -1 +1 @@\n-y\n+z"}, {"old_path": "z2.ts", "new_path": "z2.ts", "diff": "", "collapsed": true}]).to_string(),
    );
    glab(
        "ws",
        "api projects/acme%2Fweb/repository/commits/a1b2c3d/diff?per_page=100&page=1",
        None,
        &json!([{"old_path": "only.ts", "new_path": "only.ts", "diff": "@@ -1 +1 @@\n-o\n+n\n"}]).to_string(),
    );
    glab(
        "ws-broken",
        &format!("api {}/diffs?per_page=100&page=1", mr_path(7)),
        None,
        r#"{"message":"404 Not Found"}"#,
    );

    // File revisions.
    glab(
        "ws",
        &format!("api graphql --method POST {JSON_BODY}"),
        Some(&blobs_body("head7", &["src/a.ts".into(), "src/gone.ts".into(), " spaced.ts".into()])),
        r#"{"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"src/a.ts","oid":"aaa"},{"path":" spaced.ts","oid":" bbb "},{"path":"src/gone.ts","oid":null}]}}}}}"#,
    );
    glab(
        "ws",
        &format!("api {}", mr_path(8)),
        None,
        r#"{"diff_refs":{"base_sha":"base8","head_sha":"head8","start_sha":"start8"}}"#,
    );
    glab(
        "ws",
        &format!("api graphql --method POST {JSON_BODY}"),
        Some(&blobs_body("head8", &["src/a.ts".into()])),
        r#"{"data":{"project":null}}"#,
    );
    let many: Vec<String> = (0..150).map(|index| format!("src/{index}.ts")).collect();
    glab(
        "ws-paths",
        &format!("api {}", mr_path(7)),
        None,
        r#"{"diff_refs":{"base_sha":"b","head_sha":"h","start_sha":"s"}}"#,
    );
    for batch in many.chunks(100) {
        let nodes: Vec<Value> = batch
            .iter()
            .filter(|path| !path.ends_with("7.ts"))
            .map(|path| json!({"path": path, "oid": format!("oid-{path}")}))
            .collect();
        glab(
            "ws-paths",
            &format!("api graphql --method POST {JSON_BODY}"),
            Some(&blobs_body("h", batch)),
            &json!({"data": {"project": {"repository": {"blobs": {"nodes": nodes}}}}}).to_string(),
        );
    }

    // Diff file contents (the CLI's read).
    let raw = |path: &str, reference: &str| format!("api projects/acme%2Fweb/repository/files/{path}/raw?ref={reference}");
    glab("ws", &raw("src%2Fa.ts", "base7"), None, "old a\n");
    glab("ws", &raw("src%2Fa.ts", "head7"), None, "new a\n");
    glab(
        "ws",
        "api projects/acme%2Fweb/repository/commits/abcdef1",
        None,
        r#"{"id":"abcdef1","parent_ids":[]}"#,
    );
    glab("ws", &raw("src%2Ffirst.ts", "abcdef1"), None, "first\n");
    glab(
        "ws",
        "api projects/acme%2Fweb/repository/commits/abcdef2",
        None,
        r#"{"id":"abcdef2","parent_ids":[" "]}"#,
    );
    glab(
        "ws",
        "api projects/acme%2Fweb/repository/commits/abcdef3",
        None,
        r#"{"id":"abcdef3","parent_ids":["abcdef2"]}"#,
    );
    glab("ws", &raw("src%2Fr.ts", "abcdef2"), None, "before\u{FFFD}after\n");
    glab("ws", &raw("src%2Fr2.ts", "abcdef3"), None, "after\n");
    clis.respond("glab", "ws", &raw("assets%2Flogo.png", "base7"), None, b"\x89PNG\x00\x01", "", 0);
    clis.respond("glab", "ws", &raw("assets%2Flogo.png", "head7"), None, b"\x89PNG\x00\x02", "", 0);
    clis.respond("glab", "ws", &raw("docs%2Flatin1.txt", "base7"), None, b"caf\xe9\n", "", 0);
    clis.respond("glab", "ws", &raw("big.txt", "base7"), None, vec![b'a'; 1024 * 1024 + 10], "", 0);

    // Writes.
    glab("ws", "mr merge 7 --repo acme/web --auto-merge=false --yes --rebase", None, "");
    glab(
        "ws",
        &format!("api {}/cancel_merge_when_pipeline_succeeds --method POST", mr_path(7)),
        None,
        "{}",
    );
    glab("ws", "mr update 7 --repo acme/web --ready", None, "");
    fail("ws", "mr close 9 --repo acme/web", "boom");
    glab(
        "ws",
        &format!("api {} --method PUT {JSON_BODY}", mr_path(7)),
        Some(r#"{"title":"New title","description":"New body"}"#),
        "{}",
    );
    glab(
        "ws",
        &format!("api {}/notes --method POST {JSON_BODY}", mr_path(7)),
        Some(r#"{"body":"true"}"#),
        "{}",
    );
    glab(
        "ws",
        &format!("api {}/notes/42 --method PUT {JSON_BODY}", mr_path(7)),
        Some(r#"{"body":"Reworded."}"#),
        "{}",
    );
    let review_comment = |body: Value| {
        glab(
            "ws",
            &format!("api {}/discussions --method POST {JSON_BODY}", mr_path(7)),
            Some(&body.to_string()),
            "{}",
        )
    };
    review_comment(
        json!({"body": "Add a test.", "position": {"base_sha": "base7", "head_sha": "head7", "start_sha": "start7", "position_type": "text", "old_path": "src/a.ts", "new_path": "src/a.ts", "new_line": 3}}),
    );
    review_comment(
        json!({"body": "Moved?", "position": {"base_sha": "base7", "head_sha": "head7", "start_sha": "start7", "position_type": "text", "old_path": "src/old-b.ts", "new_path": "src/b.ts", "old_line": 4, "new_line": 5}}),
    );
    glab(
        "ws",
        &format!("api {}/notes --method POST {JSON_BODY}", mr_path(7)),
        Some(r#"{"body":" Looks right. "}"#),
        "{}",
    );
    glab("ws", &format!("api {}/approve --method POST", mr_path(7)), None, "{}");
    glab("ws", &format!("api {}", mr_path(10)), None, r#"{"diff_refs":null}"#);
    glab(
        "ws",
        &format!("api {} --method PUT {JSON_BODY}", mr_path(7)),
        Some(r#"{"reviewer_ids":[5,9,12]}"#),
        "{}",
    );
    glab(
        "ws",
        &format!("api {} --method PUT {JSON_BODY}", mr_path(7)),
        Some(r#"{"reviewer_ids":[9]}"#),
        "{}",
    );
    glab(
        "ws",
        &format!("api {}/discussions/abc%20123%2Fx/notes --method POST {JSON_BODY}", mr_path(7)),
        Some(r#"{"body":"Agreed."}"#),
        "{}",
    );
    glab("ws", &format!("api {}/notes/42/award_emoji?name=eyes --method POST", mr_path(7)), None, "{}");
    glab(
        "ws",
        &format!("api {}/award_emoji", mr_path(7)),
        None,
        r#"[{"id":30,"name":"thumbsup","user":{"username":"julius"}},{"id":"x","name":"thumbsup"},{"id":31,"name":" ThumbsUp ","user":{"username":" bilal "}}]"#,
    );
    glab("ws", &format!("api {}/award_emoji/31 --method DELETE", mr_path(7)), None, "");
    glab(
        "ws",
        &format!("api {}/discussions/abc123 --method PUT {JSON_BODY}", mr_path(7)),
        Some(r#"{"resolved":false}"#),
        "{}",
    );
}

// ---------------------------------------------------------------------------------------------
// The cases.

fn reference(cwd: &str, number: i64) -> Value {
    json!({"cwd": cwd, "repository": "acme/web", "host": "gitlab.example.test", "number": number})
}

fn with(mut value: Value, extra: Value) -> Value {
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn cases(root: &Path) -> Vec<Value> {
    let ws = |name: &str| root.join(name).to_string_lossy().into_owned();
    let main = ws("ws");
    let provider = |id: &str, method: &str, input: Value| json!({"id": id, "op": "provider", "method": method, "input": input});
    let list = |cwd: &str, extra: Value| {
        with(
            json!({"cwd": cwd, "repository": "acme/web", "host": "gitlab.example.test", "state": "open", "involvement": "all", "viewer": "bilal", "limit": 10}),
            extra,
        )
    };
    let many: Vec<String> = (0..150).map(|index| format!("src/{index}.ts")).collect();
    let contents = |id: &str, extra: Value| json!({"id": id, "op": "cli", "method": "getMergeRequestDiffFileContents", "input": with(json!({"cwd": main, "repository": "acme/web", "number": 7}), extra)});
    let mut cases = vec![
        provider("viewer", "getViewer", json!({"cwd": main})),
        provider("viewer-noname", "getViewer", json!({"cwd": ws("ws-noname"), "host": "gitlab.example.test"})),
        provider("viewer-401", "getViewer", json!({"cwd": ws("ws-401")})),
        provider("viewer-broken", "getViewer", json!({"cwd": ws("ws-broken")})),
        provider("viewer-unscripted", "getViewer", json!({"cwd": ws("ws-fail")})),
        provider("list-first", "listChangeRequests", list(&main, json!({"involvement": "authored", "limit": 2}))),
        provider(
            "list-cursor",
            "listChangeRequests",
            list(
                &main,
                json!({"state": "merged", "involvement": "reviewing", "limit": 2, "query": " a&b \"c\" ", "cursor": {"updatedBefore": "2026-07-02T00:00:00Z", "delivered": 3}}),
            ),
        ),
        provider(
            "list-nested",
            "listChangeRequests",
            list(&main, json!({"repository": "acme/platform/web", "state": "all", "limit": 150})),
        ),
        provider("list-empty", "listChangeRequests", list(&main, json!({"state": "closed", "query": "  "}))),
        provider("list-broken", "listChangeRequests", list(&ws("ws-broken"), json!({}))),
        provider("detail", "getChangeRequest", reference(&main, 7)),
        provider("detail-404", "getChangeRequest", reference(&main, 8)),
        provider("activity", "getChangeRequestActivity", reference(&main, 7)),
        provider("activity-fail", "getChangeRequestActivity", reference(&ws("ws-fail"), 7)),
        provider("permissions", "getViewerPermissions", reference(&main, 7)),
        provider("permissions-404", "getViewerPermissions", reference(&main, 8)),
        provider("diff-first", "getDiff", reference(&main, 7)),
        provider("diff-cursor", "getDiff", with(reference(&main, 7), json!({"cursor": "2"}))),
        provider("diff-commit", "getDiff", with(reference(&main, 7), json!({"commit": "a1b2c3d"}))),
        provider("diff-bad-cursor", "getDiff", with(reference(&main, 7), json!({"cursor": "0"}))),
        provider("diff-bad-commit", "getDiff", with(reference(&main, 7), json!({"commit": "zz"}))),
        provider("diff-broken", "getDiff", reference(&ws("ws-broken"), 7)),
        provider(
            "revisions",
            "getFileRevisions",
            with(reference(&main, 7), json!({"paths": ["src/a.ts", "src/gone.ts", " spaced.ts"]})),
        ),
        provider(
            "revisions-unanswered",
            "getFileRevisions",
            with(reference(&main, 8), json!({"paths": ["src/a.ts"]})),
        ),
        provider(
            "revisions-many",
            "getFileRevisions",
            with(reference(&ws("ws-paths"), 7), json!({"paths": many})),
        ),
        provider("revisions-none", "getFileRevisions", with(reference(&ws("ws-fail"), 7), json!({"paths": []}))),
        provider("candidates", "listReviewerCandidates", reference(&main, 7)),
        provider("candidates-404", "listReviewerCandidates", reference(&main, 8)),
        contents("contents-change", json!({"changeType": "change", "oldPath": "src/a.ts", "newPath": "src/a.ts"})),
        contents(
            "contents-new-root",
            json!({"commit": "abcdef1", "changeType": "new", "oldPath": "src/first.ts", "newPath": "src/first.ts"}),
        ),
        contents(
            "contents-no-parent",
            json!({"commit": "abcdef2", "changeType": "change", "oldPath": "src/r.ts", "newPath": "src/r.ts"}),
        ),
        contents(
            "contents-commit",
            json!({"commit": "abcdef3", "changeType": "rename-changed", "oldPath": "src/r.ts", "newPath": "src/r2.ts"}),
        ),
        contents(
            "contents-binary",
            json!({"changeType": "change", "oldPath": "assets/logo.png", "newPath": "assets/logo.png"}),
        ),
        contents(
            "contents-invalid-utf8",
            json!({"changeType": "deleted", "oldPath": "docs/latin1.txt", "newPath": "docs/latin1.txt"}),
        ),
        contents(
            "contents-oversized",
            json!({"changeType": "deleted", "oldPath": "big.txt", "newPath": "big.txt"}),
        ),
        contents(
            "contents-bad-commit",
            json!({"commit": "nope", "changeType": "new", "oldPath": "a", "newPath": "a"}),
        ),
        provider(
            "action-merge",
            "runAction",
            with(reference(&main, 7), json!({"action": "merge", "mergeMethod": "rebase"})),
        ),
        provider(
            "action-disable",
            "runAction",
            with(reference(&main, 7), json!({"action": "disable-auto-merge"})),
        ),
        provider(
            "action-ready",
            "runAction",
            with(reference(&main, 7), json!({"action": "ready", "mergeMethod": "squash"})),
        ),
        provider("action-close-fails", "runAction", with(reference(&main, 9), json!({"action": "close"}))),
        provider(
            "update",
            "updateChangeRequest",
            with(reference(&main, 7), json!({"title": "New title", "body": "New body"})),
        ),
        provider("comment", "comment", with(reference(&main, 7), json!({"body": "true"}))),
        provider(
            "update-comment",
            "updateComment",
            with(reference(&main, 7), json!({"commentId": "42", "kind": "issue-comment", "body": "Reworded."})),
        ),
        provider(
            "review",
            "submitReview",
            with(
                reference(&main, 7),
                json!({"verdict": "approve", "body": " Looks right. ", "comments": [
                    {"path": "src/a.ts", "position": {"kind": "added", "newLine": 3}, "body": "Add a test."},
                    {"path": "src/b.ts", "oldPath": "src/old-b.ts", "position": {"kind": "context", "oldLine": 4, "newLine": 5, "side": "right"}, "body": "Moved?"},
                ]}),
            ),
        ),
        provider(
            "review-no-refs",
            "submitReview",
            with(
                reference(&main, 10),
                json!({"verdict": "comment", "body": "", "comments": [{"path": "a.ts", "position": {"kind": "deleted", "oldLine": 1}, "body": "x"}]}),
            ),
        ),
        provider(
            "reviewers-add",
            "setReviewerRequest",
            with(
                reference(&main, 7),
                json!({"reviewers": [{"id": "9", "kind": "user"}, {"id": "octo", "kind": "user"}, {"id": " 12 ", "kind": "user"}, {"id": "0", "kind": "user"}], "requested": true}),
            ),
        ),
        provider(
            "reviewers-remove",
            "setReviewerRequest",
            with(reference(&main, 7), json!({"reviewers": [{"id": "5", "kind": "user"}], "requested": false})),
        ),
        provider(
            "reply",
            "replyToThread",
            with(reference(&main, 7), json!({"threadId": "abc 123/x", "body": "Agreed."})),
        ),
        provider(
            "react-add",
            "setReaction",
            with(reference(&main, 7), json!({"subjectId": "42", "content": "eyes", "reacted": true})),
        ),
        provider(
            "react-remove",
            "setReaction",
            with(reference(&main, 7), json!({"content": "thumbs-up", "reacted": false})),
        ),
        provider(
            "resolve",
            "setThreadResolution",
            with(reference(&main, 7), json!({"threadId": "abc123", "resolved": false})),
        ),
    ];
    cases.extend(json_cases());
    cases
}

/// Edge payloads for the pure decoders.
fn json_cases() -> Vec<Value> {
    let decode = |id: &str, method: &str, raw: String| json!({"id": id, "op": "json", "method": method, "input": {"raw": raw}});
    let full = mr(
        3,
        json!({"draft": true, "has_conflicts": true, "merge_status": "can_be_merged", "user": null, "merge_when_pipeline_succeeds": null, "auto_merge_enabled": null}),
    );
    vec![
        decode("json-list", "list", json!([full, {"iid": 4.0, "title": "t", "web_url": "u", "source_branch": "s", "target_branch": "t", "created_at": "c", "updated_at": "u"}, {"iid": 1.5}, [], null, mr(5, json!({"draft": null}))]).to_string()),
        decode("json-list-not-array", "list", r#"{"message":"x"}"#.into()),
        decode("json-detail", "detail", full.to_string()),
        decode("json-detail-bad-label", "detail", mr(3, json!({"labels": [1]})).to_string()),
        decode("json-detail-numeric-changes", "detail", mr(3, json!({"changes_count": 3})).to_string()),
        decode("json-detail-changes", "detail", mr(3, json!({"changes_count": " -2 ", "auto_merge_enabled": false, "squash_on_merge": true, "user": {}})).to_string()),
        decode("json-detail-user-null-can-merge", "detail", mr(3, json!({"user": {"can_merge": null}})).to_string()),
        decode("json-detail-array", "detail", "[]".into()),
        decode("json-viewer-missing", "viewer", "{}".into()),
        decode("json-viewer-trimmed", "viewer", r#"{"username":" octo "}"#.into()),
        decode("json-users", "users", r#"[{"id":1,"username":"a","name":"","avatar_url":null},{"id":null,"username":"b"},{"id":2.5,"username":"c"}]"#.into()),
        decode("json-caps-semi", "mergeCapabilities", r#"{"merge_method":" Rebase_Merge ","squash_option":"ALWAYS"}"#.into()),
        decode("json-caps-unknown", "mergeCapabilities", r#"{"merge_method":"other","squash_option":null}"#.into()),
        decode("json-caps-bad", "mergeCapabilities", r#"{"merge_method":1}"#.into()),
        decode("json-diff-refs-missing", "diffRefs", r#"{"message":"404"}"#.into()),
        decode("json-diff-refs-partial", "diffRefs", r#"{"diff_refs":{"base_sha":"a"}}"#.into()),
        decode("json-commit-refs", "commitDiffRefs", r#"{"id":" abc ","parent_ids":[" p1 ","p2"]}"#.into()),
        decode("json-commit-refs-bad", "commitDiffRefs", r#"{"id":"abc","parent_ids":null}"#.into()),
        decode("json-notes-bad", "notes", "nope".into()),
        decode("json-diffs-empty", "diffs", "[]".into()),
        decode("json-diffs-control", "diffs", json!([{"old_path": "a\u{1}b\"c", "new_path": "a\u{7f}é", "renamed_file": true, "diff": "@@\n\n"}]).to_string()),
        decode("json-awards-no-project", "awards", r#"{"data":{"project":null}}"#.into()),
        decode("json-awards-missing-project", "awards", r#"{"data":{}}"#.into()),
        decode("json-awards-null-page-info", "awards", r#"{"data":{"project":{"mergeRequest":{"notes":{"pageInfo":null,"nodes":[]}}}}}"#.into()),
        decode("json-awards-no-nodes", "awards", r#"{"data":{"project":{"mergeRequest":{"notes":{"pageInfo":{"hasNextPage":true,"endCursor":"  "}}}}}}"#.into()),
        json!({"id": "json-own-award", "op": "json", "method": "ownAward", "input": {"raw": r#"[{"id":1,"name":"heart","user":{"username":"bilal"}},{"id":2,"name":"HEART","user":{"username":"bilal"}}]"#, "args": {"content": "heart", "viewer": "bilal"}}}),
        decode("json-blobs-no-nodes", "blobs", r#"{"data":{"project":{"repository":{"blobs":{"nodes":null}}}}}"#.into()),
        decode("json-blobs-missing-project", "blobs", r#"{"data":{}}"#.into()),
        decode("json-blobs-duplicate", "blobs", r#"{"data":{"project":{"repository":{"blobs":{"nodes":[{"path":"a","oid":"1"},{"path":"b","oid":"2"},{"path":"a","oid":"3"}]}}}}}"#.into()),
    ]
}

// ---------------------------------------------------------------------------------------------
// The Rust side.

fn change_request_ref(input: &Value) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: input["cwd"].as_str().unwrap().into(),
        repository: input["repository"].as_str().unwrap().into(),
        host: input["host"].as_str().unwrap_or_default().into(),
        number: input["number"].as_i64().unwrap(),
    }
}

fn opt_str(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn from<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

fn unit(_: &()) -> Value {
    Value::Null
}

fn list_item(item: &gitlab_json::GitLabMergeRequestListItem) -> Value {
    json!({
        "number": item.number, "title": item.title, "url": item.url, "author": item.author, "headBranch": item.head_branch,
        "baseBranch": item.base_branch, "state": item.state, "isDraft": item.is_draft, "mergeability": item.mergeability,
        "additions": item.additions, "deletions": item.deletions, "createdAt": item.created_at, "updatedAt": item.updated_at,
        "reviewRequestLogins": item.review_request_logins, "labels": item.labels,
    })
}

fn decoded_detail(detail: &gitlab_json::GitLabMergeRequestDetail) -> Value {
    let mut value = list_item(&detail.item);
    let extra = shape::Obj::new()
        .set("body", &detail.body)
        .set("changedFiles", detail.changed_files)
        .set("mergedAt", &detail.merged_at)
        .set("closedAt", &detail.closed_at)
        .set("reviewers", &detail.reviewers)
        .set("checks", &detail.checks)
        .set("viewerCanMerge", detail.viewer_can_merge)
        .set("reviewerIds", &detail.reviewer_ids)
        .opt("autoMergeEnabled", detail.auto_merge_enabled)
        .opt("autoMergeMethod", detail.auto_merge_method)
        .opt("divergedCommits", detail.diverged_commits)
        .done();
    value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    value
}

fn refs(refs: &Option<gitlab_json::GitLabDiffRefs>) -> Value {
    refs.as_ref().map_or(
        Value::Null,
        |refs| json!({"baseSha": refs.base_sha, "headSha": refs.head_sha, "startSha": refs.start_sha}),
    )
}

fn decoded<T>(result: Result<T, String>, shape: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({"ok": shape(value)}),
        Err(_) => json!({"error": "decode"}),
    }
}

fn run_json(method: &str, input: &Value) -> Value {
    let raw = input["raw"].as_str().unwrap();
    match method {
        "list" => decoded(
            gitlab_json::decode_merge_request_list_json(raw),
            |batch| json!({"items": batch.items.iter().map(list_item).collect::<Vec<_>>(), "rawIndexes": batch.raw_indexes, "rawCount": batch.raw_count}),
        ),
        "detail" => decoded(gitlab_json::decode_merge_request_detail_json(raw), |detail| decoded_detail(&detail)),
        "viewer" => decoded(gitlab_json::decode_viewer_json(raw), |viewer| json!(viewer)),
        "users" => decoded(
            gitlab_json::decode_project_users_json(raw),
            |users| json!({"candidates": users.candidates, "rawCount": users.raw_count}),
        ),
        "mergeCapabilities" => decoded(gitlab_json::decode_project_merge_capabilities_json(raw), |capabilities| to_json(&capabilities)),
        "discussions" => decoded(
            gitlab_json::decode_discussions_json(raw),
            |discussions| json!({"threads": discussions.threads, "rawCount": discussions.raw_count}),
        ),
        "diffRefs" => decoded(gitlab_json::decode_diff_refs_json(raw), |value| refs(&value)),
        "notes" => decoded(
            gitlab_json::decode_notes_json(raw),
            |notes| json!({"comments": notes.comments, "rawCount": notes.raw_count}),
        ),
        "commits" => decoded(gitlab_json::decode_commits_json(raw), |commits| to_json(&commits)),
        "commitDiffRefs" => decoded(gitlab_json::decode_commit_diff_refs_json(raw), |value| refs(&value)),
        "diffs" => decoded(
            gitlab_json::decode_merge_request_diffs_json(raw),
            |patch| json!({"patch": patch.patch, "truncated": patch.truncated, "rawCount": patch.raw_count}),
        ),
        "awards" => decoded(gitlab_json::decode_award_emoji_json(raw), |page| {
            json!({
                "reactions": page.reactions,
                "reactionsByNoteId": page.reactions_by_note_id.into_entries().into_iter().map(|(id, reactions)| json!([id, reactions])).collect::<Vec<_>>(),
                "nextCursor": page.next_cursor,
            })
        }),
        "ownAward" => decoded(
            gitlab_json::decode_own_award_id_json(raw, from(&input["args"]["content"]), input["args"]["viewer"].as_str().unwrap()),
            |id| json!(id),
        ),
        "blobs" => decoded(gitlab_json::decode_repository_blobs_json(raw), |blobs| {
            blobs.map_or(Value::Null, |blobs| {
                Value::Array(blobs.into_entries().into_iter().map(|(path, oid)| json!([path, oid])).collect())
            })
        }),
        other => panic!("unknown decoder {other}"),
    }
}

async fn run_cli(cli: &GitLabPullRequestCli, method: &str, input: &Value) -> Value {
    assert_eq!(method, "getMergeRequestDiffFileContents");
    let commit = opt_str(input, "commit");
    let result = cli
        .get_merge_request_diff_file_contents(DiffFileContentsInput {
            target: MergeRequestTarget {
                cwd: input["cwd"].as_str().unwrap(),
                repository: input["repository"].as_str().unwrap(),
                number: input["number"].as_i64().unwrap(),
            },
            commit: commit.as_deref(),
            change_type: from(&input["changeType"]),
            old_path: input["oldPath"].as_str().unwrap(),
            new_path: input["newPath"].as_str().unwrap(),
        })
        .await;
    match result {
        Ok((old, new)) => json!({"ok": {"oldContents": old, "newContents": new}}),
        Err(error) => json!({"error": {"tag": error.tag(), "message": error.message()}}),
    }
}

async fn run_provider(provider: &GitLabPullRequestProvider, method: &str, input: &Value) -> Value {
    match method {
        "getViewer" => shape::outcome(
            provider
                .get_viewer(ProviderHostRef {
                    cwd: input["cwd"].as_str().unwrap().into(),
                    host: opt_str(input, "host"),
                })
                .await,
            |viewer| json!(viewer),
        ),
        "listChangeRequests" => shape::outcome(
            provider
                .list_change_requests(ListChangeRequestsInput {
                    cwd: input["cwd"].as_str().unwrap().into(),
                    repository: input["repository"].as_str().unwrap().into(),
                    host: input["host"].as_str().unwrap().into(),
                    state: from(&input["state"]),
                    involvement: from(&input["involvement"]),
                    viewer: input["viewer"].as_str().unwrap().into(),
                    limit: input["limit"].as_i64().unwrap(),
                    query: opt_str(input, "query"),
                    cursor: input.get("cursor").map(|cursor| ProviderListCursor {
                        updated_before: cursor["updatedBefore"].as_str().unwrap().into(),
                        delivered: cursor["delivered"].as_i64().unwrap(),
                    }),
                    filters: None,
                })
                .await,
            shape::page,
        ),
        "getChangeRequest" => shape::outcome(provider.get_change_request(change_request_ref(input)).await, shape::detail),
        "getChangeRequestActivity" => shape::outcome(provider.get_change_request_activity(change_request_ref(input)).await, shape::activity),
        "getViewerPermissions" => shape::outcome(
            provider
                .get_viewer_permissions(ViewerPermissionsInput {
                    change_request: change_request_ref(input),
                    include_update_branch: None,
                })
                .await,
            to_json,
        ),
        "getDiff" => shape::outcome(
            provider
                .get_diff(GetDiffInput {
                    change_request: change_request_ref(input),
                    cursor: opt_str(input, "cursor"),
                    commit: opt_str(input, "commit"),
                })
                .await,
            shape::diff,
        ),
        "getFileRevisions" => shape::outcome(
            provider
                .get_file_revisions(FileRevisionsInput {
                    change_request: change_request_ref(input),
                    paths: from(&input["paths"]),
                })
                .await,
            shape::file_revisions,
        ),
        "listReviewerCandidates" => shape::outcome(provider.list_reviewer_candidates(change_request_ref(input)).await, to_json),
        "runAction" => shape::outcome(
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
        "updateChangeRequest" => shape::outcome(
            provider
                .update_change_request(UpdateChangeRequestInput {
                    change_request: change_request_ref(input),
                    title: opt_str(input, "title"),
                    body: opt_str(input, "body"),
                })
                .await,
            unit,
        ),
        "comment" => shape::outcome(
            provider
                .comment(CommentInput {
                    change_request: change_request_ref(input),
                    body: opt_str(input, "body").unwrap(),
                })
                .await,
            unit,
        ),
        "updateComment" => shape::outcome(
            provider
                .update_comment(UpdateCommentInput {
                    change_request: change_request_ref(input),
                    comment_id: opt_str(input, "commentId").unwrap(),
                    kind: from(&input["kind"]),
                    body: opt_str(input, "body").unwrap(),
                })
                .await,
            unit,
        ),
        "submitReview" => shape::outcome(
            provider
                .submit_review(SubmitReviewInput {
                    change_request: change_request_ref(input),
                    verdict: from(&input["verdict"]),
                    body: opt_str(input, "body").unwrap(),
                    comments: from::<Vec<PullRequestReviewCommentDraft>>(&input["comments"]),
                })
                .await,
            unit,
        ),
        "setReviewerRequest" => shape::outcome(
            provider
                .set_reviewer_request(SetReviewerRequestInput {
                    change_request: change_request_ref(input),
                    reviewers: input["reviewers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|reviewer| ReviewerRef {
                            id: reviewer["id"].as_str().unwrap().into(),
                            kind: from::<PullRequestReviewerKind>(&reviewer["kind"]),
                        })
                        .collect(),
                    requested: input["requested"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        "replyToThread" => shape::outcome(
            provider
                .reply_to_thread(ReplyToThreadInput {
                    change_request: change_request_ref(input),
                    thread_id: opt_str(input, "threadId").unwrap(),
                    body: opt_str(input, "body").unwrap(),
                })
                .await,
            unit,
        ),
        "setReaction" => shape::outcome(
            provider
                .set_reaction(SetReactionInput {
                    change_request: change_request_ref(input),
                    subject_id: opt_str(input, "subjectId"),
                    content: from(&input["content"]),
                    reacted: input["reacted"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        "setThreadResolution" => shape::outcome(
            provider
                .set_thread_resolution(SetThreadResolutionInput {
                    change_request: change_request_ref(input),
                    thread_id: opt_str(input, "threadId").unwrap(),
                    resolved: input["resolved"].as_bool().unwrap(),
                })
                .await,
            unit,
        ),
        other => panic!("unknown provider method {other}"),
    }
}

#[tokio::test]
async fn rust_matches_the_typescript_gitlab_provider() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let clis = FakeClis::new(&["glab"]);
    record(&clis);
    let root = tempfile::Builder::new().prefix("zc-gitlab-golden-").tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    for name in ["ws", "ws-noname", "ws-401", "ws-broken", "ws-fail", "ws-paths"] {
        std::fs::create_dir_all(root_path.join(name)).unwrap();
    }
    let cases = cases(&root_path);

    let expected = run_oracle(&cases, &clis.path);
    let ts_calls = clis.take_log("glab");
    if std::env::var_os("ZC_GOLDEN_DUMP").is_some() {
        eprintln!("{}", serde_json::to_string_pretty(&expected).unwrap());
        eprintln!("{ts_calls:#?}");
    }

    let provider = GitLabPullRequestProvider::new(GitLabCli::new(clis.process()));
    let cli = GitLabPullRequestCli::new(GitLabCli::new(clis.process()));
    let mut mismatches = Vec::new();
    for case in &cases {
        let id = case["id"].as_str().unwrap();
        let (method, input) = (case["method"].as_str().unwrap(), &case["input"]);
        let mut actual = match case["op"].as_str().unwrap() {
            "provider" => run_provider(&provider, method, input).await,
            "cli" => run_cli(&cli, method, input).await,
            _ => run_json(method, input),
        };
        let mut wanted = expected.get(id).cloned().unwrap_or(Value::Null);
        shape::normalize(&mut actual, 0);
        shape::normalize(&mut wanted, 0);
        if actual != wanted {
            mismatches.push(format!(
                "{id}:\n  ts:   {}\n  rust: {}",
                serde_json::to_string(&wanted).unwrap(),
                serde_json::to_string(&actual).unwrap()
            ));
        }
    }
    let rust_calls = clis.take_log("glab");
    if ts_calls != rust_calls {
        let only_ts: Vec<_> = ts_calls.iter().filter(|call| !rust_calls.contains(call)).collect();
        let only_rust: Vec<_> = rust_calls.iter().filter(|call| !ts_calls.contains(call)).collect();
        mismatches.push(format!(
            "glab calls differ ({} TS, {} Rust):\n  only TS: {only_ts:#?}\n  only Rust: {only_rust:#?}",
            ts_calls.len(),
            rust_calls.len()
        ));
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    eprintln!("{} GitLab cases and {} glab calls match the TS provider", cases.len(), ts_calls.len());
}
