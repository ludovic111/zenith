//! Golden comparison against the TypeScript Forgejo pull request provider: the same fake `tea`
//! (recorded `tea api --include` answers keyed by working directory, argv and piped body) serves
//! both sides, the TS provider runs over the real `ForgejoCli` and `VcsProcess`
//! (`golden/forgejo_oracle.mjs`), and every read method, a set of writes and the unsupported
//! ones must give the same JSON, errors included. The `tea` calls each side made are compared too.
//!
//! Needs `node` and `code/apps/server/node_modules`; without them the test prints why and passes
//! vacuously.

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_forgejo;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Map, Value};
use support_forgejo::fake::FakeClis;
use support_forgejo::shape::{self, to_json};
use zc_contracts::{PullRequestReviewCommentDraft, PullRequestReviewerKind};
use zc_pullrequest::forgejo::ForgejoPullRequestProvider;
use zc_pullrequest::provider::*;
use zc_pullrequest::PullRequestProviderApi;
use zc_sourcecontrol::forgejo::{ForgejoCli, ForgejoEnvironment};
use zc_sourcecontrol::util::system_clock;

const BASE: &str = "https://forge.example.test";

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

fn run_oracle(cases: &[Value], bin: &Path, home: &Path) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/forgejo_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("APPDATA")
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

fn user(login: &str) -> Value {
    json!({"login": login, "full_name": "", "avatar_url": format!("{BASE}/avatars/{login}")})
}

fn pull(number: i64, extra: Value) -> Value {
    let mut value = json!({
        "number": number,
        "title": format!("Pull request {number}"),
        "body": "Ships it.",
        "html_url": format!("{BASE}/acme/web/pulls/{number}"),
        "user": {"login": "maria", "full_name": "Maria Example"},
        "state": "open",
        "merged": false,
        "mergeable": true,
        "is_locked": false,
        "head": {"ref": format!("feat/{number}"), "sha": format!("head{number}"), "repo": {"full_name": "maria/web", "permissions": {"push": true, "admin": false}}},
        "base": {"ref": "main", "sha": "base", "repo": {"full_name": "acme/web"}},
        "merge_base": "base",
        "created_at": "2026-07-01T10:00:00+02:00",
        "updated_at": "2026-07-02T00:00:00Z",
        "closed_at": null,
        "merged_at": null,
        "additions": 3,
        "deletions": null,
        "changed_files": 2,
        "comments": 1,
        "labels": [{"id": 1, "name": "backend", "color": "00ff00", "description": "Server"}],
        "requested_reviewers": [{"login": "kit"}],
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

/// Records `tea` answers for one workspace.
struct Tea<'a> {
    clis: &'a FakeClis,
    cwd: &'a str,
}

impl Tea<'_> {
    fn login(&self) {
        let logins = json!([{"name": "forge", "url": BASE, "ssh_host": "", "user": "maria", "default": "true"}]);
        self.clis.respond("tea", self.cwd, "login list --output json", None, logins.to_string(), "", 0);
    }

    /// `METHOD path` answered with `status`, `body` and an optional `link` header; `repo` names
    /// the `--repo` the call carries (none for `user`).
    fn api(&self, method: &str, path: &str, repo: Option<&str>, request: Option<&Value>, status: u16, body: &str, link: Option<&str>) {
        let repo = repo.map(|repo| format!(" --repo {repo}")).unwrap_or_default();
        let data = if request.is_some() { " --data @-" } else { "" };
        let args = format!("api --include --login forge{repo} --method {method}{data} {BASE}/api/v1/{path}");
        let headers = format!("HTTP/1.1 {status} X\n{}", link.map(|link| format!("Link: {link}\n")).unwrap_or_default());
        let stdin = request.map(Value::to_string);
        self.clis.respond("tea", self.cwd, &args, stdin.as_deref(), body, &headers, 0);
    }

    fn get(&self, path: &str, body: Value) {
        self.api("GET", path, Some("acme/web"), None, 200, &body.to_string(), None);
    }

    fn send(&self, method: &str, path: &str, request: Option<Value>) {
        self.api(method, path, Some("acme/web"), request.as_ref(), 200, "{}", None);
    }
}

fn record(clis: &FakeClis) {
    let next = Some(r#"<https://forge.example.test/api/v1/x?page=2>; rel="next""#);
    for cwd in ["ws", "ws-401", "ws-broken"] {
        Tea { clis, cwd }.login();
    }
    let tea = Tea { clis, cwd: "ws" };
    tea.api("GET", "user", None, None, 200, &user("kit").to_string(), None);
    Tea { clis, cwd: "ws-401" }.api("GET", "user", None, None, 401, r#"{"message":"token is required"}"#, None);

    // Listings.
    let rows = |numbers: &[i64]| {
        Value::Array(
            numbers
                .iter()
                .map(|number| pull(*number, json!({"updated_at": format!("2026-07-0{number}T00:00:00Z")})))
                .collect(),
        )
    };
    tea.api(
        "GET",
        "repos/acme/web/pulls?state=open&sort=recentupdate&limit=50&page=1",
        Some("acme/web"),
        None,
        200,
        &rows(&[1, 2, 3]).to_string(),
        next,
    );
    tea.api(
        "GET",
        "repos/acme/web/pulls?state=closed&sort=recentupdate&limit=50&page=1",
        Some("acme/web"),
        None,
        200,
        &json!([
            pull(1, json!({"state": "closed", "merged": true, "merged_at": "2026-07-03T00:00:00Z"})),
            pull(2, json!({"state": "closed", "title": "WIP: two"})),
            pull(3, json!({}))
        ])
        .to_string(),
        next,
    );
    tea.get(
        "repos/acme/web/pulls?state=closed&sort=recentupdate&limit=50&page=2",
        json!([
            pull(4, json!({"draft": true, "mergeable": false})),
            null,
            pull(
                6,
                json!({"user": null, "labels": null, "requested_reviewers": null, "head": {"ref": "x", "sha": "y", "repo": null}})
            )
        ]),
    );
    tea.get("repos/acme/web/pulls?state=closed&sort=recentupdate&limit=50&page=3", json!([]));
    Tea { clis, cwd: "ws-broken" }.api(
        "GET",
        "repos/acme/web/pulls?state=all&sort=recentupdate&limit=50&page=1",
        Some("acme/web"),
        None,
        200,
        "not json",
        None,
    );
    Tea { clis, cwd: "ws-broken" }.get("repos/acme/web/pulls/7", json!({"number": "7"}));

    // Detail, summary, permissions.
    tea.get("repos/acme/web/pulls/7", pull(7, json!({})));
    tea.api("GET", "repos/acme/web/pulls/8", Some("acme/web"), None, 404, "{}", None);
    tea.get(
        "repos/acme/web/pulls/9",
        pull(9, json!({"merge_base": "older", "user": {"login": "kit"}, "is_locked": true})),
    );
    tea.get(
        "repos/acme/web",
        json!({"full_name": "acme/web", "permissions": {"push": false, "admin": false}, "allow_squash_merge": false, "allow_rebase_update": true}),
    );
    tea.api(
        "GET",
        "repos/acme/web/statuses/head7?sort=recentupdate&limit=50&page=1",
        Some("acme/web"),
        None,
        200,
        &json!([
            {"context": "ci", "status": "success", "description": "", "target_url": "https://ci.example.test/2", "updated_at": "2026-07-02T00:00:00Z"},
            {"context": "ci", "status": "failure", "description": "old", "target_url": null, "updated_at": "2026-07-01T00:00:00Z"},
            {"context": "", "status": "warning", "description": null, "target_url": "", "updated_at": "2026-07-01T00:00:00Z"},
        ])
        .to_string(),
        next,
    );
    tea.get("repos/acme/web/statuses/head7?sort=recentupdate&limit=50&page=2", Value::Null);
    tea.get("repos/acme/web/statuses/head9?sort=recentupdate&limit=50&page=1", json!([]));

    // Activity.
    tea.get(
        "repos/acme/web/issues/7/comments",
        json!([
            {"id": 10, "body": "Later.", "user": user("kit"), "created_at": "2026-07-03T00:00:00Z", "html_url": format!("{BASE}/acme/web/pulls/7#issuecomment-10")},
            {"id": 13, "body": "Earlier.", "user": null, "created_at": "2026-07-01T09:00:00+02:00", "html_url": ""},
        ]),
    );
    tea.api(
        "GET",
        "repos/acme/web/pulls/7/reviews?limit=50&page=1",
        Some("acme/web"),
        None,
        200,
        &json!([
            {"id": 3, "body": "Looks good", "user": user("lee"), "state": "APPROVED", "submitted_at": "2026-07-02T00:00:00Z", "html_url": format!("{BASE}/acme/web/pulls/7#issuecomment-11"), "comments_count": 1},
            {"id": 4, "body": "", "user": user("lee"), "state": "PENDING", "submitted_at": "2026-07-02T00:00:00Z", "comments_count": 2},
            {"id": 5, "body": "", "user": user("maria"), "state": "REQUEST_REVIEW", "submitted_at": "2026-07-01T00:00:00Z", "comments_count": 0},
            {"id": 6, "body": "Fix it", "user": user("kit"), "state": "REQUEST_CHANGES", "submitted_at": "2026-07-02T12:00:00Z", "comments_count": 1},
        ])
        .to_string(),
        None,
    );
    tea.get("repos/acme/web/pulls/7/reviews?limit=50&page=2", json!([]));
    tea.get(
        "repos/acme/web/pulls/7/commits?limit=50&page=1",
        json!([
            {"sha": "c1", "author": user("maria"), "commit": {"message": "One\n\nBody", "committer": {"date": "2026-07-01T00:00:00Z"}}, "parents": [], "stats": {"additions": 2, "deletions": 1}},
            {"sha": "c2", "author": null, "commit": {"message": "Two", "committer": {"date": "not a date"}}, "parents": [{"sha": "c1"}], "stats": null},
        ]),
    );
    tea.get("repos/acme/web/pulls/7/commits?limit=50&page=2", json!([]));
    tea.get(
        "repos/acme/web/issues/7/reactions?limit=50&page=1",
        json!([{"content": "rocket", "user": user("kit")}, {"content": "+1", "user": user("kit")}]),
    );
    tea.get("repos/acme/web/issues/7/reactions?limit=50&page=2", Value::Null);
    tea.get(
        "repos/acme/web/pulls/7/reviews/3/comments",
        json!([{"id": 12, "body": "Here.", "user": user("lee"), "created_at": "2026-07-01T12:00:00Z", "path": "src/a.ts", "position": 0, "original_position": 3, "commit_id": "c", "original_commit_id": "o", "resolver": user("maria")}]),
    );
    tea.get(
        "repos/acme/web/pulls/7/reviews/6/comments",
        json!([{"id": 14, "body": "There.", "user": user("kit"), "created_at": "2026-07-02T13:00:00Z", "html_url": "x", "path": "src/b.ts", "position": 5, "original_position": 5, "commit_id": "c", "original_commit_id": "o", "resolver": null}]),
    );
    tea.get(
        "repos/acme/web/issues/comments/10/reactions",
        json!([{"content": "heart", "user": user("lee")}, {"content": "heart", "user": user("kit")}]),
    );
    tea.get("repos/acme/web/issues/comments/13/reactions", json!([]));
    tea.get("repos/acme/web/issues/comments/11/reactions", Value::Null);
    tea.get("repos/acme/web/issues/comments/12/reactions", json!([{"content": "eyes", "user": user("kit")}]));
    tea.get("repos/acme/web/issues/comments/14/reactions", json!([{"content": "laugh", "user": null}]));

    // Diffs, revisions, contents.
    let patch = "diff --git a/src/a.ts b/src/a.ts\nindex 1111111..2222222 100644\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/old.ts b/new.ts\nsimilarity index 100%\nrename from old.ts\nrename to new.ts\nindex 3333333..3333333 100644\n";
    tea.api("GET", "repos/acme/web/pulls/7.diff", Some("acme/web"), None, 200, patch, None);
    tea.api(
        "GET",
        "repos/acme/web/git/commits/abc%2F123.diff",
        Some("acme/web"),
        None,
        200,
        "diff --git a/x b/x\n",
        None,
    );
    tea.get(
        "repos/acme/web/git/commits/c2",
        json!({"sha": "c2", "author": null, "commit": {"message": "Two", "committer": {"date": "2026-07-02T00:00:00Z"}}, "parents": [{"sha": "c1"}]}),
    );
    tea.get(
        "repos/acme/web/contents/src/a%20b.ts?ref=base",
        json!({"content": "b2xkCg==", "encoding": "base64"}),
    );
    tea.get(
        "repos/acme/web/contents/src/a%20b.ts?ref=c1",
        json!({"content": "Y2Fm6Qo=", "encoding": "base64"}),
    );
    tea.api(
        "GET",
        "repos/maria/web/contents/src/a%20b.ts?ref=head7",
        Some("maria/web"),
        None,
        200,
        &json!({"content": "bmV3\nCg==", "encoding": "base64"}).to_string(),
        None,
    );
    tea.api(
        "GET",
        "repos/maria/web/contents/src/a%20b.ts?ref=c2",
        Some("maria/web"),
        None,
        200,
        &json!({"content": "bmV3Cg", "encoding": "utf-8"}).to_string(),
        None,
    );

    // Candidates.
    tea.get(
        "repos/acme/web/assignees?limit=50&page=1",
        json!([{"login": "maria"}, user("kit"), {"login": "", "full_name": "x"}, {"login": "lee", "full_name": "Lee Example"}]),
    );
    tea.get("repos/acme/web/assignees?limit=50&page=2", json!([]));
    tea.get(
        "repos/acme/web/labels?limit=50&page=1",
        json!([{"id": 1, "name": "backend", "color": "00ff00"}, {"id": 2, "name": "ui", "description": null}]),
    );
    tea.get("repos/acme/web/labels?limit=50&page=2", json!([]));

    // Writes.
    tea.send("POST", "repos/acme/web/pulls/7/merge", Some(json!({"Do": "squash"})));
    tea.send("PATCH", "repos/acme/web/pulls/7", Some(json!({"state": "closed"})));
    tea.send("POST", "repos/acme/web/pulls/7/update?style=rebase", None);
    tea.send("PATCH", "repos/acme/web/pulls/7", Some(json!({"title": "New title", "body": "New body"})));
    tea.send("POST", "repos/acme/web/issues/7/comments", Some(json!({"body": "true"})));
    tea.send("PATCH", "repos/acme/web/issues/comments/55", Some(json!({"body": "Reworded."})));
    tea.send(
        "POST",
        "repos/acme/web/pulls/7/reviews",
        Some(json!({"event": "APPROVED", "body": "Looks right.", "commit_id": "head7", "comments": [
            {"path": "src/a.ts", "body": "Add a test.", "old_position": 0, "new_position": 3},
            {"path": "src/old-b.ts", "body": "Gone?", "old_position": 4, "new_position": 0},
        ]})),
    );
    tea.send("POST", "repos/acme/web/pulls/7/requested_reviewers", Some(json!({"reviewers": ["kit", "lee"]})));
    tea.send("DELETE", "repos/acme/web/pulls/7/requested_reviewers", Some(json!({"reviewers": ["kit"]})));
    tea.send("POST", "repos/acme/web/issues/7/labels", Some(json!({"labels": [1, 2]})));
    tea.get(
        "repos/acme/web/pulls/7/reviews/3",
        json!({"id": 3, "body": "", "user": null, "state": "APPROVED", "submitted_at": "2026-07-01T00:00:00Z", "html_url": format!("{BASE}/acme/web/pulls/7#issuecomment-11"), "comments_count": 0}),
    );
    tea.send("POST", "repos/acme/web/issues/comments/11/reactions", Some(json!({"content": "+1"})));
    tea.send("DELETE", "repos/acme/web/issues/7/reactions", Some(json!({"content": "hooray"})));
    tea.api(
        "POST",
        "repos/acme/web/pulls/9/merge",
        Some("acme/web"),
        Some(&json!({"Do": "merge"})),
        405,
        r#"{"message":"not mergeable"}"#,
        None,
    );
}

// ---------------------------------------------------------------------------------------------
// The cases.

fn reference(cwd: &str, number: i64) -> Value {
    json!({"cwd": cwd, "repository": "acme/web", "host": "forge.example.test", "number": number})
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
    let case = |id: &str, method: &str, input: Value| json!({"id": id, "method": method, "input": input});
    let list = |cwd: &str, extra: Value| {
        with(
            json!({"cwd": cwd, "repository": "acme/web", "host": "forge.example.test", "state": "open", "involvement": "all", "viewer": "kit", "limit": 2, "query": "ignored"}),
            extra,
        )
    };
    let action = |id: &str, extra: Value| case(id, "runAction", with(reference(&main, 7), extra));
    vec![
        case("viewer", "getViewer", json!({"cwd": main, "host": "forge.example.test"})),
        case("viewer-401", "getViewer", json!({"cwd": ws("ws-401"), "host": "forge.example.test"})),
        case("list-open", "listChangeRequests", list(&main, json!({}))),
        case(
            "list-merged-offset",
            "listChangeRequests",
            list(
                &main,
                json!({"state": "merged", "limit": 3, "cursor": {"updatedBefore": "2026-07-02T00:00:00Z", "delivered": 4}}),
            ),
        ),
        case("list-merged-first", "listChangeRequests", list(&main, json!({"state": "merged", "limit": 10}))),
        case("list-broken", "listChangeRequests", list(&ws("ws-broken"), json!({"state": "all"}))),
        case("summary", "getChangeRequestSummary", reference(&main, 7)),
        case("summary-404", "getChangeRequestSummary", reference(&main, 8)),
        case("summary-invalid", "getChangeRequestSummary", reference(&ws("ws-broken"), 7)),
        case("detail", "getChangeRequest", reference(&main, 7)),
        case("detail-author-locked", "getChangeRequest", reference(&main, 9)),
        case("detail-404", "getChangeRequest", reference(&main, 8)),
        case("permissions", "getViewerPermissions", reference(&main, 7)),
        case("activity", "getChangeRequestActivity", reference(&main, 7)),
        case("diff", "getDiff", with(reference(&main, 7), json!({"cursor": "ignored"}))),
        case("diff-commit", "getDiff", with(reference(&main, 7), json!({"commit": "abc/123"}))),
        case(
            "revisions",
            "getFileRevisions",
            with(reference(&main, 7), json!({"paths": ["src/a.ts", "src/gone.ts", "new.ts"]})),
        ),
        case(
            "contents",
            "getDiffFileContents",
            with(
                reference(&main, 7),
                json!({"changeType": "change", "oldPath": "src/a b.ts", "newPath": "src/a b.ts"}),
            ),
        ),
        case(
            "contents-new",
            "getDiffFileContents",
            with(
                reference(&main, 7),
                json!({"changeType": "new", "oldPath": "src/a b.ts", "newPath": "src/a b.ts"}),
            ),
        ),
        case(
            "contents-commit",
            "getDiffFileContents",
            with(
                reference(&main, 7),
                json!({"commit": "c2", "changeType": "deleted", "oldPath": "src/a b.ts", "newPath": "src/a b.ts"}),
            ),
        ),
        case(
            "contents-bad-encoding",
            "getDiffFileContents",
            with(
                reference(&main, 7),
                json!({"commit": "c2", "changeType": "new", "oldPath": "src/a b.ts", "newPath": "src/a b.ts"}),
            ),
        ),
        case("candidates", "listReviewerCandidates", reference(&main, 7)),
        case("labels", "listLabelCandidates", reference(&main, 7)),
        action("action-merge", json!({"action": "merge", "mergeMethod": "squash"})),
        action("action-close", json!({"action": "close"})),
        action("action-update", json!({"action": "update-branch", "updateMethod": "rebase"})),
        action("action-ready", json!({"action": "ready"})),
        case("action-merge-refused", "runAction", with(reference(&main, 9), json!({"action": "merge"}))),
        case(
            "update",
            "updateChangeRequest",
            with(reference(&main, 7), json!({"title": "New title", "body": "New body"})),
        ),
        case("comment", "comment", with(reference(&main, 7), json!({"body": "true"}))),
        case(
            "update-comment",
            "updateComment",
            with(reference(&main, 7), json!({"commentId": "55", "kind": "issue-comment", "body": "Reworded."})),
        ),
        case(
            "review",
            "submitReview",
            with(
                reference(&main, 7),
                json!({"verdict": "approve", "body": "Looks right.", "comments": [
                    {"path": "src/a.ts", "position": {"kind": "added", "newLine": 3}, "body": "Add a test."},
                    {"path": "src/b.ts", "oldPath": "src/old-b.ts", "position": {"kind": "deleted", "oldLine": 4}, "body": "Gone?"},
                ]}),
            ),
        ),
        case(
            "reviewers-add",
            "setReviewerRequest",
            with(
                reference(&main, 7),
                json!({"reviewers": [{"id": "kit", "kind": "user"}, {"id": "lee", "kind": "user"}], "requested": true}),
            ),
        ),
        case(
            "reviewers-remove",
            "setReviewerRequest",
            with(reference(&main, 7), json!({"reviewers": [{"id": "kit", "kind": "user"}], "requested": false})),
        ),
        case(
            "labels-add",
            "setLabels",
            with(reference(&main, 7), json!({"labels": ["ui", "backend"], "applied": true})),
        ),
        case(
            "labels-missing",
            "setLabels",
            with(reference(&main, 7), json!({"labels": ["ui", "missing"], "applied": false})),
        ),
        case(
            "react-review",
            "setReaction",
            with(reference(&main, 7), json!({"subjectId": "review:3", "content": "thumbs-up", "reacted": true})),
        ),
        case(
            "react-pr",
            "setReaction",
            with(reference(&main, 7), json!({"content": "hooray", "reacted": false})),
        ),
        case(
            "react-bad-review",
            "setReaction",
            with(reference(&main, 7), json!({"subjectId": "review:x", "content": "heart", "reacted": true})),
        ),
        case(
            "react-bad-comment",
            "setReaction",
            with(reference(&main, 7), json!({"subjectId": "12a", "content": "heart", "reacted": true})),
        ),
        case("reply", "replyToThread", with(reference(&main, 7), json!({"threadId": "12", "body": "x"}))),
        case(
            "resolve",
            "setThreadResolution",
            with(reference(&main, 7), json!({"threadId": "12", "resolved": true})),
        ),
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

async fn run(provider: &ForgejoPullRequestProvider, method: &str, input: &Value) -> Value {
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
        "getChangeRequestSummary" => shape::outcome(provider.get_change_request_summary(change_request_ref(input)).await, shape::summary),
        "getChangeRequest" => shape::outcome(provider.get_change_request(change_request_ref(input)).await, shape::detail),
        "getViewerPermissions" => shape::outcome(
            provider
                .get_viewer_permissions(ViewerPermissionsInput {
                    change_request: change_request_ref(input),
                    include_update_branch: None,
                })
                .await,
            to_json,
        ),
        "getChangeRequestActivity" => shape::outcome(provider.get_change_request_activity(change_request_ref(input)).await, shape::activity),
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
        "getDiffFileContents" => shape::outcome(
            provider
                .get_diff_file_contents(DiffFileContentsInput {
                    change_request: change_request_ref(input),
                    commit: opt_str(input, "commit"),
                    change_type: from(&input["changeType"]),
                    old_path: opt_str(input, "oldPath").unwrap(),
                    new_path: opt_str(input, "newPath").unwrap(),
                })
                .await,
            shape::file_contents,
        ),
        "listReviewerCandidates" => shape::outcome(provider.list_reviewer_candidates(change_request_ref(input)).await, to_json),
        "listLabelCandidates" => shape::outcome(provider.list_label_candidates(change_request_ref(input)).await, to_json),
        "runAction" => shape::outcome(
            provider
                .run_action(RunActionInput {
                    change_request: change_request_ref(input),
                    action: from(&input["action"]),
                    stack_number: None,
                    expected_stack_heads: None,
                    merge_method: input.get("mergeMethod").map(from),
                    update_method: input.get("updateMethod").map(from),
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
        "setLabels" => shape::outcome(
            provider
                .set_labels(SetLabelsInput {
                    change_request: change_request_ref(input),
                    labels: from(&input["labels"]),
                    applied: input["applied"].as_bool().unwrap(),
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
async fn rust_matches_the_typescript_forgejo_provider() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let clis = FakeClis::new(&["tea"]);
    record(&clis);
    let root = tempfile::Builder::new().prefix("zc-forgejo-golden-").tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let home = root_path.join("home");
    for name in ["ws", "ws-401", "ws-broken", "home"] {
        std::fs::create_dir_all(root_path.join(name)).unwrap();
    }
    let cases = cases(&root_path);

    let expected = run_oracle(&cases, &clis.path, &home);
    let ts_calls = clis.take_log("tea");
    if std::env::var_os("ZC_GOLDEN_DUMP").is_some() {
        eprintln!("{}", serde_json::to_string_pretty(&expected).unwrap());
        eprintln!("{ts_calls:#?}");
    }

    let environment = ForgejoEnvironment {
        home: home.clone(),
        data_home: None,
        app_data: None,
        ..ForgejoEnvironment::from_process()
    };
    let provider = ForgejoPullRequestProvider::new(ForgejoCli::new(clis.process(), environment, system_clock()));
    let mut mismatches = Vec::new();
    for case in &cases {
        let id = case["id"].as_str().unwrap();
        let mut actual = run(&provider, case["method"].as_str().unwrap(), &case["input"]).await;
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
    let rust_calls = clis.take_log("tea");
    if ts_calls != rust_calls {
        let only_ts: Vec<_> = ts_calls.iter().filter(|call| !rust_calls.contains(call)).collect();
        let only_rust: Vec<_> = rust_calls.iter().filter(|call| !ts_calls.contains(call)).collect();
        mismatches.push(format!(
            "tea calls differ ({} TS, {} Rust):\n  only TS: {only_ts:#?}\n  only Rust: {only_rust:#?}",
            ts_calls.len(),
            rust_calls.len()
        ));
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    eprintln!("{} Forgejo cases and {} tea calls match the TS provider", cases.len(), ts_calls.len());
}
