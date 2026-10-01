//! The Forgejo normalizers (no TS test file exists; these follow `forgejoPullRequestJson.ts`).

use serde_json::{json, Value};
use zc_contracts::{
    PullRequestCheckStatus, PullRequestCommentKind, PullRequestDiffSide, PullRequestMergeability, PullRequestReactionContent, PullRequestState,
};

use super::*;

fn pull(extra: Value) -> Value {
    let mut value = json!({
        "number": 7,
        "title": "Add the pull requests page",
        "body": null,
        "html_url": "https://forge.example.test/acme/web/pulls/7",
        "user": {"login": "maria", "full_name": "", "avatar_url": "https://forge.example.test/avatars/m"},
        "state": "open",
        "merged": false,
        "head": {"ref": "feat/page", "sha": "head7", "repo": {"full_name": "maria/web"}},
        "base": {"ref": "main", "sha": "base7", "repo": null},
        "created_at": "2026-07-01T12:00:00+02:00",
        "updated_at": "2026-07-02T00:00:00Z",
        "closed_at": null,
        "merged_at": null,
        "labels": null,
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

fn decode(value: Value) -> Decoded<ForgejoPullRequest> {
    decode_pull_request(value.as_object().unwrap())
}

#[test]
fn reads_a_pull_request_as_a_change_request() {
    let pr = decode(pull(json!({
        "labels": [{"id": 1, "name": "backend", "color": "00ff00"}, {"id": 2, "name": "ui"}],
        "requested_reviewers": [{"login": "kit"}],
        "additions": 3,
        "deletions": null,
        "mergeable": true,
    })))
    .unwrap();
    let change_request = forgejo_change_request(&pr);
    assert_eq!(
        change_request.author.as_ref().map(|actor| (actor.login.as_str(), actor.name.as_deref())),
        Some(("maria", None))
    );
    assert_eq!(change_request.head_repository_name_with_owner, Some(Some("maria/web".into())));
    assert_eq!(change_request.state, PullRequestState::Open);
    assert_eq!(change_request.mergeability, PullRequestMergeability::Mergeable);
    assert_eq!((change_request.additions, change_request.deletions), (3, 0));
    // Normalized to `toISOString`, the zone folded into UTC.
    assert_eq!(change_request.created_at, "2026-07-01T10:00:00.000Z");
    assert_eq!(change_request.closed_at, Some(None));
    assert_eq!(change_request.review_request_logins, vec!["kit".to_owned()]);
    assert_eq!(
        change_request
            .labels
            .iter()
            .map(|label| (label.name.as_str(), label.color.as_deref()))
            .collect::<Vec<_>>(),
        vec![("backend", Some("00ff00")), ("ui", None)]
    );
}

#[test]
fn reads_merged_before_closed_and_anything_else_as_open() {
    assert_eq!(
        forgejo_change_request(&decode(pull(json!({"state": "closed", "merged": true}))).unwrap()).state,
        PullRequestState::Merged
    );
    assert_eq!(
        forgejo_change_request(&decode(pull(json!({"state": "closed"}))).unwrap()).state,
        PullRequestState::Closed
    );
    assert_eq!(
        forgejo_change_request(&decode(pull(json!({"state": "weird"}))).unwrap()).state,
        PullRequestState::Open
    );
}

#[test]
fn reads_a_wip_title_as_a_draft_only_when_the_host_says_nothing() {
    let draft = |extra: Value| forgejo_change_request(&decode(pull(extra)).unwrap()).is_draft;
    assert!(draft(json!({"title": "[wip] page"})));
    assert!(draft(json!({"title": "WIP: page"})));
    assert!(!draft(json!({"title": "page WIP:"})));
    assert!(!draft(json!({"title": "WIP: page", "draft": false})));
    assert!(draft(json!({"draft": true})));
}

#[test]
fn leaves_mergeability_unknown_when_forgejo_has_not_checked() {
    assert_eq!(
        forgejo_change_request(&decode(pull(json!({}))).unwrap()).mergeability,
        PullRequestMergeability::Unknown
    );
    assert_eq!(
        forgejo_change_request(&decode(pull(json!({"mergeable": false}))).unwrap()).mergeability,
        PullRequestMergeability::Conflicting
    );
}

#[test]
fn keeps_a_timestamp_that_is_no_date_as_it_came() {
    assert_eq!(to_iso_utc("yesterday"), "yesterday");
    assert_eq!(to_iso_utc("2026-07-01T00:00:00.5Z"), "2026-07-01T00:00:00.500Z");
}

#[test]
fn rejects_what_the_schema_rejects() {
    // `body`, `closed_at`, `merged_at` and `labels` are required (nullable) keys.
    let mut missing_labels = pull(json!({}));
    missing_labels.as_object_mut().unwrap().remove("labels");
    assert!(decode(missing_labels).is_err());
    assert!(decode(pull(json!({"draft": null}))).is_err());
    assert!(decode(pull(json!({"head": {"ref": "x", "sha": "y"}}))).is_err());
    assert!(decode(pull(
        json!({"head": {"ref": "x", "sha": "y", "repo": {"full_name": "a/b", "permissions": {"push": true}}}})
    ))
    .is_err());
    assert!(decode(pull(json!({"additions": null, "changed_files": 1.5}))).is_err());
}

#[test]
fn spells_review_states_the_way_the_timeline_reads_them() {
    let review = |state: &str| {
        decode_review(&json!({"id": 3, "body": "ok", "user": null, "state": state, "submitted_at": "2026-07-01T00:00:00Z", "comments_count": 0}))
            .map(|review| forgejo_review(&review))
    };
    let request_changes = review("REQUEST_CHANGES").unwrap();
    assert_eq!(request_changes.id, "review:3");
    assert_eq!(request_changes.kind, PullRequestCommentKind::Review);
    assert_eq!(request_changes.review_state.as_deref(), Some("CHANGES_REQUESTED"));
    assert_eq!(review("COMMENT").unwrap().review_state.as_deref(), Some("COMMENTED"));
    assert_eq!(review("APPROVED").unwrap().review_state.as_deref(), Some("APPROVED"));
    assert_eq!(review("APPROVED").unwrap().url, None);
}

#[test]
fn places_an_inline_comment_on_the_old_side_when_only_its_original_position_remains() {
    let thread = |position: i64, original: i64| {
        let comment = decode_review_comment(&json!({
            "id": 11, "body": "here", "user": {"login": "kit"}, "created_at": "2026-07-01T00:00:00Z", "html_url": "",
            "path": "src/a.ts", "position": position, "original_position": original, "commit_id": "c", "original_commit_id": "o", "resolver": null,
        }))
        .unwrap();
        forgejo_review_thread(&comment)
    };
    let old = thread(0, 4);
    assert_eq!((old.side, old.line), (PullRequestDiffSide::Left, Some(4)));
    let new = thread(6, 4);
    assert_eq!((new.side, new.line), (PullRequestDiffSide::Right, Some(6)));
    let none = thread(0, 0);
    assert_eq!((none.side, none.line), (PullRequestDiffSide::Right, None));
    assert!(!none.is_resolved);
    assert_eq!(none.comments[0].url, None);
}

#[test]
fn reads_a_commit_headline_and_stats() {
    let commit = decode_commit(&json!({
        "sha": "abc", "author": null, "commit": {"message": "First line\n\nBody", "committer": {"date": "2026-07-01T00:00:00Z"}},
        "parents": [{"sha": "p"}], "stats": {"additions": 2, "deletions": 1},
    }))
    .unwrap();
    let commit = forgejo_commit(&commit);
    assert_eq!(commit.message_headline, "First line");
    assert_eq!(commit.committed_date, "2026-07-01T00:00:00.000Z");
    assert_eq!((commit.additions, commit.deletions), (Some(2), Some(1)));
    assert_eq!(commit.authors, Some(Vec::new()));
}

#[test]
fn keeps_the_newest_status_of_each_context() {
    let statuses: Vec<ForgejoStatus> = [
        json!({"context": "ci", "status": "pending", "description": "", "target_url": null, "updated_at": "2026-07-01T00:00:00Z"}),
        json!({"context": "ci", "status": "error", "description": "broke", "target_url": "https://ci.example.test/1", "updated_at": "2026-07-02T00:00:00Z"}),
        json!({"context": "", "status": "warning", "description": null, "target_url": "", "updated_at": "2026-07-01T00:00:00Z"}),
    ]
    .iter()
    .map(|status| decode_status(status).unwrap())
    .collect();
    let checks = forgejo_checks(&statuses);
    assert_eq!(
        checks.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
        vec![("ci", PullRequestCheckStatus::Failure), ("check", PullRequestCheckStatus::Pending)]
    );
    assert_eq!(checks[0].description.as_deref(), Some("broke"));
    assert_eq!(checks[1].url, None);
}

#[test]
fn groups_reactions_in_contract_order_counting_the_viewer_without_naming_them() {
    let reactions: Vec<ForgejoReaction> = [
        json!({"content": "heart", "user": {"login": "kit"}}),
        json!({"content": "+1", "user": {"login": "maria"}}),
        json!({"content": "+1", "user": null}),
        json!({"content": "+1", "user": {"login": "kit"}}),
        json!({"content": "party", "user": {"login": "kit"}}),
    ]
    .iter()
    .map(|reaction| decode_reaction(reaction).unwrap())
    .collect();
    let grouped = forgejo_reactions(&reactions, "maria");
    assert_eq!(grouped.len(), 2);
    assert_eq!(
        (grouped[0].content, grouped[0].count, grouped[0].actors.clone(), grouped[0].viewer_has_reacted),
        (PullRequestReactionContent::ThumbsUp, 3, vec!["kit".to_owned()], true)
    );
    assert_eq!((grouped[1].content, grouped[1].viewer_has_reacted), (PullRequestReactionContent::Heart, false));
    assert_eq!(forgejo_reaction_name(PullRequestReactionContent::ThumbsDown), "-1");
}
