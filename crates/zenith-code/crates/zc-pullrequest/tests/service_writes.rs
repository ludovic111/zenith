//! `PullRequestService.test.ts`, the writes: capability and permission refusals, the strategies
//! handed to a host, stack actions, merges published once confirmed, and the refreshes every
//! write (and every explicit invalidation) sends to subscribed readers.

mod support_service;

use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::StreamExt;
use support_service::*;
use zc_contracts::*;
use zc_pullrequest::provider::*;

const UPDATED: &str = "2026-07-02T00:00:00Z";

fn web() -> OrchestrationProjectShell {
    gh("p1", "web", "/a", "acme/web").build()
}

fn web_ref() -> PullRequestRef {
    reference("p1", "acme/web", 1)
}

fn permissions(actions: &[PullRequestAction], resolve: bool, request_reviewers: bool) -> PullRequestViewerPermissions {
    PullRequestViewerPermissions {
        actions: actions.to_vec(),
        resolve,
        request_reviewers,
        ..all_permissions()
    }
}

/// A handler that records the action it was asked to run.
fn recording_action(log: &Log<RunActionInput>) -> Handler<RunActionInput, ()> {
    let log = log.clone();
    h(move |input: RunActionInput| {
        log.push(input);
        async { Ok(()) }
    })
}

fn actions_of(log: &Log<RunActionInput>) -> Vec<(PullRequestAction, Option<PullRequestMergeMethod>)> {
    log.all().into_iter().map(|input| (input.action, input.merge_method)).collect()
}

async fn next_within<T>(stream: &mut zc_ports::EventStream<T>) -> Option<T> {
    tokio::time::timeout(Duration::from_secs(5), stream.next()).await.ok().flatten()
}

#[tokio::test]
async fn refuses_an_action_the_host_never_claimed_it_could_run() {
    let ran = Log::default();
    let github = FakeProvider {
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    }
    // Bitbucket's shape: it can merge and close, but cannot reopen.
    .with_capabilities(capabilities(
        &[PullRequestAction::Merge, PullRequestAction::Close],
        &[PullRequestMergeMethod::Merge],
    ));
    let service = make_service(vec![web()], vec![github]);

    let error = service.run_action(action(&web_ref(), PullRequestAction::Reopen)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(ran.len(), 0);
}

#[tokio::test]
async fn publishes_a_merge_for_immediate_settlement_only_after_host_confirmation() {
    let merged_at = "2026-09-03T02:00:00.000Z";
    let state = Cell::new(PullRequestState::Open);
    let confirmation_fails = Cell::new(false);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some({
        let (state, confirmation_fails) = (state.clone(), confirmation_fails.clone());
        h(move |_| {
            let result = if confirmation_fails.get() {
                Err(failed(Kind::Github, "getChangeRequestSummary", "HTTP 504"))
            } else {
                Ok(ProviderChangeRequestSummary {
                    state: state.get(),
                    ..summary_row(1, merged_at)
                })
            };
            async move { result }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();
    let mut merges = service.subscribe_merges();

    // Queueing succeeds while the host still reports an open PR.
    service.run_action(action(&reference, PullRequestAction::Merge)).await.unwrap();
    let queued_refresh = service.refresh_revision().await;
    confirmation_fails.set(true);
    service.run_action(action(&reference, PullRequestAction::Merge)).await.unwrap();
    assert!(service.refresh_revision().await > queued_refresh);
    confirmation_fails.set(false);
    state.set(PullRequestState::Merged);
    service.set_time(zc_core::time::parse_iso_millis(merged_at).unwrap()).await;
    service
        .run_action(PullRequestActionInput {
            repository: " ACME/WEB ".into(),
            merge_method: Some(PullRequestMergeMethod::Merge),
            ..action(&reference, PullRequestAction::Merge)
        })
        .await
        .unwrap();

    let event = next_within(&mut merges).await.expect("a published merge");
    assert_eq!(event.reference.0, serde_json::json!({"projectId": "p1", "repository": "acme/web", "number": 1}));
    assert_eq!(event.merged_at, merged_at);
}

#[tokio::test]
async fn refreshes_every_reader_before_a_queued_merge_confirmation_finishes() {
    let confirmation_started = Gate::default();
    let confirm = Gate::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some({
        let (confirmation_started, confirm) = (confirmation_started.clone(), confirm.clone());
        h(move |_| {
            let (confirmation_started, confirm) = (confirmation_started.clone(), confirm.clone());
            async move {
                confirmation_started.open();
                confirm.wait().await;
                Ok(summary_row(1, "2026-09-16T00:00:00.000Z"))
            }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let mut merges = service.subscribe_merges();
    let mut readers = [service.subscribe_refreshes(), service.subscribe_refreshes()];
    let action_task = tokio::spawn({
        let service = service.service.clone();
        async move { service.run_action(action(&web_ref(), PullRequestAction::Merge)).await }
    });
    confirmation_started.wait().await;
    let first = next_within(&mut readers[0]).await.unwrap();
    let second = next_within(&mut readers[1]).await.unwrap();
    assert!(first > 0);
    assert_eq!(first, second);
    assert!(!action_task.is_finished());
    confirm.open();
    action_task.await.unwrap().unwrap();
    assert!(futures::FutureExt::now_or_never(merges.next()).is_none());
}

#[tokio::test]
async fn refuses_an_action_this_viewer_may_not_take_and_says_what_access_it_takes() {
    let ran = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    // The host merges; this account only reads it, and opened the change request.
    github.get_viewer_permissions = ok(permissions(
        &[
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
        ],
        true,
        false,
    ));
    github.run_action = recording_action(&ran);
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let error = service.run_action(action(&reference, PullRequestAction::Merge)).await.unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("You need write access on this repository to merge."));
    assert_eq!(ran.len(), 0);

    // What the author keeps whatever their access is still theirs to take.
    service.run_action(action(&reference, PullRequestAction::Close)).await.unwrap();
    assert_eq!(actions_of(&ran), vec![(PullRequestAction::Close, None)]);
}

#[tokio::test]
async fn gates_arming_a_merge_for_later_exactly_as_it_gates_merging_now() {
    let ran = Log::default();
    let github = FakeProvider {
        // This account may close the change request it opened, and nothing else here.
        get_viewer_permissions: ok(permissions(&[PullRequestAction::Close], true, false)),
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(capabilities(
        &[
            PullRequestAction::Merge,
            PullRequestAction::Close,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
        ],
        &[PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash],
    ));
    let service = make_service(vec![web()], vec![github]);
    let arm = |method| PullRequestActionInput {
        merge_method: Some(method),
        ..action(&web_ref(), PullRequestAction::EnableAutoMerge)
    };

    let refused = service.run_action(arm(PullRequestMergeMethod::Squash)).await.unwrap_err();
    assert_eq!(tag(&refused), "PullRequestOperationError");
    assert!(refused.message().contains("merged for you once it is ready"));
    assert_eq!(ran.len(), 0);

    // The strategy is checked against the host for an armed merge too.
    let wrong_strategy = service.run_action(arm(PullRequestMergeMethod::Rebase)).await.unwrap_err();
    assert_eq!(tag(&wrong_strategy), "PullRequestOperationError");
    assert_eq!(ran.len(), 0);
}

#[tokio::test]
async fn hands_the_host_the_strategy_an_armed_merge_was_asked_for() {
    let ran = Log::default();
    let github = FakeProvider {
        get_viewer_permissions: ok(permissions(
            &[
                PullRequestAction::Merge,
                PullRequestAction::EnableAutoMerge,
                PullRequestAction::DisableAutoMerge,
            ],
            true,
            true,
        )),
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(capabilities(
        &[
            PullRequestAction::Merge,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
        ],
        &[PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash],
    ));
    let service = make_service(vec![web()], vec![github]);

    service
        .run_action(PullRequestActionInput {
            merge_method: Some(PullRequestMergeMethod::Squash),
            ..action(&web_ref(), PullRequestAction::EnableAutoMerge)
        })
        .await
        .unwrap();
    assert_eq!(
        actions_of(&ran),
        vec![(PullRequestAction::EnableAutoMerge, Some(PullRequestMergeMethod::Squash))]
    );

    service.run_action(action(&web_ref(), PullRequestAction::DisableAutoMerge)).await.unwrap();
    assert_eq!(actions_of(&ran)[1], (PullRequestAction::DisableAutoMerge, None));
}

#[tokio::test]
async fn refuses_an_auto_merge_the_host_never_claimed_without_asking_it() {
    let ran = Log::default();
    // Bitbucket's shape: it merges, and has nothing that merges later on its own.
    let github = FakeProvider {
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.run_action(action(&web_ref(), PullRequestAction::EnableAutoMerge)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(ran.len(), 0);
}

fn thread_resolution(reference: &PullRequestRef, thread_id: &str, resolved: bool) -> PullRequestThreadResolutionInput {
    PullRequestThreadResolutionInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        thread_id: thread_id.into(),
        resolved,
    }
}

fn thread_reply(reference: &PullRequestRef, thread_id: &str, body: &str) -> PullRequestThreadReplyInput {
    PullRequestThreadReplyInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        thread_id: thread_id.into(),
        body: body.into(),
    }
}

#[tokio::test]
async fn refuses_to_resolve_a_conversation_this_viewer_may_not_without_asking_the_host() {
    let github = FakeProvider {
        get_viewer_permissions: ok(PullRequestViewerPermissions {
            resolve: false,
            ..all_permissions()
        }),
        set_thread_resolution: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.set_thread_resolution(thread_resolution(&web_ref(), "t1", true)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("to resolve a review conversation."));
}

#[tokio::test]
async fn asks_nobody_what_the_viewer_may_do_when_the_host_cannot_do_it_at_all() {
    let asked = Counter::default();
    let github = FakeProvider {
        get_viewer_permissions: {
            let asked = asked.clone();
            h(move |_| {
                asked.bump();
                async { panic!("must not be called") }
            })
        },
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(capabilities(
        &[PullRequestAction::Merge, PullRequestAction::Close],
        &[PullRequestMergeMethod::Merge],
    ));
    let service = make_service(vec![web()], vec![github]);

    service.run_action(action(&web_ref(), PullRequestAction::Reopen)).await.unwrap_err();

    // The capability check costs nothing; the permission read is a request, so it comes second.
    assert_eq!(asked.get(), 0);
}

#[tokio::test]
async fn refuses_a_comment_on_a_host_that_cannot_post_one() {
    let posted = Counter::default();
    let github = FakeProvider {
        comment: {
            let posted = posted.clone();
            h(move |_| {
                posted.bump();
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        diff: false,
        comment: false,
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![web()], vec![github]);

    let error = service.comment(comment_input(&web_ref(), "Looks good.")).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(posted.get(), 0);
}

#[tokio::test]
async fn rejects_an_empty_comment_before_reaching_the_host() {
    let github = FakeProvider {
        comment: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service
        .comment(comment_input(&reference("p1", "pingdotgg/t3code", 1), "   "))
        .await
        .unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

fn review(
    reference: &PullRequestRef,
    verdict: PullRequestReviewVerdict,
    body: &str,
    comments: Vec<PullRequestReviewCommentDraft>,
) -> PullRequestSubmitReviewInput {
    PullRequestSubmitReviewInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        verdict,
        body: body.into(),
        comments,
    }
}

#[tokio::test]
async fn refuses_a_verdict_the_host_never_claimed_without_asking_the_provider() {
    let submitted = Counter::default();
    let gitlab = FakeProvider {
        submit_review: {
            let submitted = submitted.clone();
            h(move |_| {
                submitted.bump();
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Gitlab)
    }
    .with_capabilities(PullRequestCapabilities {
        // GitLab's shape: it approves, and has nothing that rejects.
        review: PullRequestReviewCapabilities {
            verdicts: vec![PullRequestReviewVerdict::Comment, PullRequestReviewVerdict::Approve],
            ..full_review()
        },
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "on gitlab", "/a", "group/project").provider("gitlab").build()], vec![gitlab]);

    let error = service
        .submit_review(review(
            &reference("p1", "group/project", 1),
            PullRequestReviewVerdict::RequestChanges,
            "no",
            Vec::new(),
        ))
        .await
        .unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(submitted.get(), 0);
}

#[tokio::test]
async fn refuses_line_comments_on_a_host_that_takes_only_a_summary() {
    let github = FakeProvider {
        submit_review: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        review: PullRequestReviewCapabilities {
            inline_comment: false,
            reply: false,
            resolve: false,
            verdicts: vec![PullRequestReviewVerdict::Comment],
        },
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);
    let comment: PullRequestReviewCommentDraft = serde_json::from_value(serde_json::json!({
        "path": "src/a.ts",
        "position": {"kind": "added", "newLine": 1},
        "body": "nit",
    }))
    .unwrap();

    let error = service
        .submit_review(review(
            &reference("p1", "pingdotgg/t3code", 1),
            PullRequestReviewVerdict::Comment,
            "",
            vec![comment],
        ))
        .await
        .unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn refuses_a_review_with_neither_a_summary_nor_a_comment_but_lets_an_approval_through() {
    let approved = Counter::default();
    let github = FakeProvider {
        submit_review: {
            let approved = approved.clone();
            h(move |_| {
                approved.bump();
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);
    let reference = reference("p1", "pingdotgg/t3code", 1);

    let error = service
        .submit_review(review(&reference, PullRequestReviewVerdict::Comment, "   ", Vec::new()))
        .await
        .unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");

    // An approval is a verdict in itself, so it needs no words.
    service
        .submit_review(review(&reference, PullRequestReviewVerdict::Approve, "", Vec::new()))
        .await
        .unwrap();
    assert_eq!(approved.get(), 1);
}

#[tokio::test]
async fn refuses_to_resolve_a_conversation_on_a_host_that_cannot() {
    let github = FakeProvider {
        set_thread_resolution: die("must not be called"),
        reply_to_thread: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: false,
            resolve: false,
            verdicts: vec![PullRequestReviewVerdict::Comment],
        },
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);
    let reference = reference("p1", "pingdotgg/t3code", 1);

    let resolve_error = service.set_thread_resolution(thread_resolution(&reference, "t1", true)).await.unwrap_err();
    let reply_error = service.reply_to_thread(thread_reply(&reference, "t1", "hi")).await.unwrap_err();

    assert_eq!(tag(&resolve_error), "PullRequestOperationError");
    assert_eq!(tag(&reply_error), "PullRequestOperationError");
}

fn reaction(reference: &PullRequestRef, subject_id: Option<&str>) -> PullRequestReactionInput {
    PullRequestReactionInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        subject_id: subject_id.map(Into::into),
        content: PullRequestReactionContent::Heart,
        reacted: true,
    }
}

#[tokio::test]
async fn refuses_to_react_on_a_host_with_no_reactions() {
    let github = FakeProvider {
        set_reaction: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        reactions: Some(false),
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service.set_reaction(reaction(&reference("p1", "pingdotgg/t3code", 1), None)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn refuses_to_react_on_a_host_whose_capabilities_omit_reactions_entirely() {
    let github = FakeProvider {
        set_reaction: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        reactions: None,
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service.set_reaction(reaction(&reference("p1", "pingdotgg/t3code", 1), None)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn passes_a_reaction_through_with_its_subject_id_on_a_host_that_has_them() {
    let received: Log<(Option<String>, PullRequestReactionContent, bool)> = Log::default();
    let github = FakeProvider {
        set_reaction: {
            let received = received.clone();
            h(move |input: SetReactionInput| {
                received.push((input.subject_id.clone(), input.content, input.reacted));
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    service
        .set_reaction(reaction(&reference("p1", "pingdotgg/t3code", 1), Some("IC_1")))
        .await
        .unwrap();

    assert_eq!(received.all(), vec![(Some("IC_1".to_owned()), PullRequestReactionContent::Heart, true)]);
}

#[tokio::test]
async fn invalidates_the_cached_activity_after_reacting_like_the_other_mutations() {
    let activity_calls = Counter::default();
    let github = FakeProvider {
        get_change_request_activity: {
            let activity_calls = activity_calls.clone();
            h(move |_| {
                activity_calls.bump();
                async { Ok(empty_activity()) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.refresh_after_turn(&ProjectId::new("p1")).await;
    let previous_refresh = service.refresh_revision().await;
    service.activity(reference.clone()).await.unwrap();
    assert_eq!(activity_calls.get(), 1);

    service.set_reaction(reaction(&reference, None)).await.unwrap();
    assert!(service.refresh_revision().await > previous_refresh);
    service.activity(reference).await.unwrap();

    assert_eq!(activity_calls.get(), 2);
}

#[tokio::test]
async fn refuses_an_empty_reply_before_it_reaches_the_host() {
    let github = FakeProvider {
        reply_to_thread: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service
        .reply_to_thread(thread_reply(&reference("p1", "pingdotgg/t3code", 1), "t1", "   "))
        .await
        .unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn refuses_a_merge_strategy_the_host_does_not_offer() {
    let ran = Log::default();
    let github = FakeProvider {
        get_change_request_summary: Some(ok(summary_row(1, UPDATED))),
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    }
    // Azure DevOps's shape: it squashes as a completion option and has no rebase.
    .with_capabilities(capabilities(
        &[PullRequestAction::Merge],
        &[PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash],
    ));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);
    let merge = |method| PullRequestActionInput {
        merge_method: Some(method),
        ..action(&reference("p1", "pingdotgg/t3code", 1), PullRequestAction::Merge)
    };

    // Every provider maps an unrecognised strategy to its own default.
    let error = service.run_action(merge(PullRequestMergeMethod::Rebase)).await.unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(ran.len(), 0);

    service.run_action(merge(PullRequestMergeMethod::Squash)).await.unwrap();
    assert_eq!(actions_of(&ran), vec![(PullRequestAction::Merge, Some(PullRequestMergeMethod::Squash))]);
}

fn reviewer_request(reference: &PullRequestRef) -> PullRequestReviewerRequestInput {
    PullRequestReviewerRequestInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        reviewers: vec![PullRequestReviewerRequestInputReviewersItem {
            id: "octocat".into(),
            kind: PullRequestReviewerKind::User,
        }],
        requested: true,
    }
}

#[tokio::test]
async fn refuses_to_ask_for_a_review_on_a_host_that_cannot_before_any_call_is_made() {
    let asked = Counter::default();
    let github = FakeProvider {
        get_viewer_permissions: {
            let asked = asked.clone();
            h(move |_| {
                asked.bump();
                async { panic!("must not be called") }
            })
        },
        set_reviewer_request: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        reviewers: PullRequestReviewerCapabilities {
            request: false,
            list_candidates: false,
        },
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![web()], vec![github]);

    let error = service.request_reviewers(reviewer_request(&web_ref())).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("cannot ask somebody for a review."));
    assert_eq!(asked.get(), 0);
}

#[tokio::test]
async fn refuses_the_candidate_list_on_a_host_that_has_no_such_list_to_give() {
    let github = FakeProvider {
        list_reviewer_candidates: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        diff: false,
        comment: false,
        search: false,
        // Azure's shape: it takes a reviewer, and names nobody who could be one.
        reviewers: PullRequestReviewerCapabilities {
            request: true,
            list_candidates: false,
        },
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![web()], vec![github]);

    let error = service.reviewer_candidates(web_ref()).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("cannot say who may review a change request."));
}

#[tokio::test]
async fn refuses_a_review_request_this_viewer_may_not_make_and_says_what_access_it_takes() {
    let sent = Counter::default();
    let github = FakeProvider {
        // The host asks for reviews; this account only reads the repository.
        get_viewer_permissions: ok(permissions(
            &[
                PullRequestAction::Ready,
                PullRequestAction::Draft,
                PullRequestAction::Close,
                PullRequestAction::Reopen,
            ],
            true,
            false,
        )),
        set_reviewer_request: {
            let sent = sent.clone();
            h(move |_| {
                sent.bump();
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.request_reviewers(reviewer_request(&web_ref())).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("You need write access on this repository to ask for a review."));
    assert_eq!(sent.get(), 0);
}

#[tokio::test]
async fn keeps_the_menu_from_a_viewer_who_may_not_ask_which_is_all_it_is_for() {
    let github = FakeProvider {
        get_viewer_permissions: ok(permissions(&[], false, false)),
        list_reviewer_candidates: die("must not be called"),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.reviewer_candidates(web_ref()).await.unwrap_err();

    assert!(error.message().contains("You need write access on this repository to ask for a review."));
}

#[tokio::test]
async fn hands_the_hosts_own_candidate_list_back_and_asks_for_it_with_the_change_request() {
    let asked_for: Log<i64> = Log::default();
    let github = FakeProvider {
        list_reviewer_candidates: {
            let asked_for = asked_for.clone();
            h(move |input: ChangeRequestRef| {
                asked_for.push(input.number);
                async {
                    Ok(serde_json::from_value::<PullRequestReviewerCandidateList>(serde_json::json!({
                        "candidates": [{"id": "octocat", "kind": "user", "login": "octocat", "name": null, "avatarUrl": null, "isRequested": true}],
                        "truncated": false,
                    }))
                    .unwrap())
                }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let list = service.reviewer_candidates(reference("p1", "acme/web", 4)).await.unwrap();

    assert_eq!(asked_for.all(), vec![4]);
    assert_eq!(
        list.candidates.iter().map(|candidate| candidate.login.clone()).collect::<Vec<_>>(),
        vec!["octocat".to_owned()]
    );
}

fn label_change(reference: &PullRequestRef, applied: bool) -> PullRequestLabelChangeInput {
    PullRequestLabelChangeInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        labels: vec!["bug".into()],
        applied,
    }
}

#[tokio::test]
async fn refuses_a_label_change_on_a_host_that_has_not_said_it_takes_one() {
    let changed = Counter::default();
    let github = FakeProvider {
        // The method is there; the capability that would let it be called is not.
        set_labels: Some({
            let changed = changed.clone();
            h(move |_| {
                changed.bump();
                async { Ok(()) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.set_labels(label_change(&web_ref(), true)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("cannot change the labels"));
    assert_eq!(changed.get(), 0);
}

#[tokio::test]
async fn refuses_a_label_change_this_viewer_may_not_make_and_says_what_access_it_takes() {
    let changed = Counter::default();
    let github = FakeProvider {
        get_viewer_permissions: ok(PullRequestViewerPermissions {
            labels: Some(false),
            ..permissions(&[], false, false)
        }),
        list_label_candidates: Some(die("must not be called")),
        set_labels: Some({
            let changed = changed.clone();
            h(move |_| {
                changed.bump();
                async { Ok(()) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        labels: Some(true),
        ..full_capabilities()
    });
    let service = make_service(vec![web()], vec![github]);

    let list_error = service.label_candidates(web_ref()).await.unwrap_err();
    assert!(list_error.message().contains("You need triage access on this repository"));

    let error = service.set_labels(label_change(&web_ref(), true)).await.unwrap_err();
    assert!(error.message().contains("You need triage access on this repository"));
    assert_eq!(changed.get(), 0);
}

#[tokio::test]
async fn hands_a_label_change_to_the_host_and_reads_the_labels_back_for_the_menu() {
    let received: Log<(Vec<String>, bool)> = Log::default();
    let github = FakeProvider {
        list_label_candidates: Some(ok(PullRequestLabelCandidateList {
            candidates: vec![PullRequestLabelCandidate {
                name: "bug".into(),
                color: None,
                description: None,
                is_applied: false,
            }],
            truncated: false,
        })),
        set_labels: Some({
            let received = received.clone();
            h(move |input: SetLabelsInput| {
                received.push((input.labels.clone(), input.applied));
                async { Ok(()) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        labels: Some(true),
        ..full_capabilities()
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = reference("p1", "acme/web", 4);

    let list = service.label_candidates(reference.clone()).await.unwrap();
    assert_eq!(list.candidates.iter().map(|label| label.name.clone()).collect::<Vec<_>>(), vec!["bug"]);

    service.set_labels(label_change(&reference, false)).await.unwrap();
    assert_eq!(received.all(), vec![(vec!["bug".to_owned()], false)]);
}

#[tokio::test]
async fn close_and_reopen_notify_subscribed_readers_after_invalidating_their_cached_state() {
    let host_calls = Counter::default();
    let state = Cell::new(PullRequestState::Open);
    let github = FakeProvider {
        get_change_request_summary: Some({
            let state = state.clone();
            h(move |_| {
                let summary = ProviderChangeRequestSummary {
                    state: state.get(),
                    ..summary_row(1, "2026-09-16T00:00:00.000Z")
                };
                async move { Ok(summary) }
            })
        }),
        run_action: {
            let state = state.clone();
            h(move |input: RunActionInput| {
                state.set(if input.action == PullRequestAction::Close {
                    PullRequestState::Closed
                } else {
                    PullRequestState::Open
                });
                async { Ok(()) }
            })
        },
        list_change_requests: {
            let host_calls = host_calls.clone();
            h(move |_| {
                host_calls.bump();
                async { Ok(page(Vec::new(), false, false)) }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.refresh_after_turn(&ProjectId::new("p1")).await;
    service.list(open_list()).await.unwrap();
    assert_eq!(service.summary(reference.clone(), true).await.unwrap().state, PullRequestState::Open);
    for (verb, expected) in [
        (PullRequestAction::Close, PullRequestState::Closed),
        (PullRequestAction::Reopen, PullRequestState::Open),
    ] {
        let mut refreshes = service.subscribe_refreshes().skip(1);
        let refreshed = tokio::spawn({
            let service = service.service.clone();
            let reference = reference.clone();
            async move {
                refreshes.next().await;
                service.list(open_list()).await.unwrap();
                service.summary(reference, true).await.unwrap()
            }
        });
        service.run_action(action(&reference, verb)).await.unwrap();
        assert_eq!(refreshed.await.unwrap().state, expected);
    }
    assert_eq!(host_calls.get(), 3);
}

#[tokio::test]
async fn explicit_invalidation_refreshes_origin_readers_after_a_routed_host_mutation() {
    let state = Cell::new(PullRequestState::Open);
    let github = FakeProvider {
        get_change_request_summary: Some({
            let state = state.clone();
            h(move |_| {
                let summary = ProviderChangeRequestSummary {
                    state: state.get(),
                    ..summary_row(1, "2026-09-16T00:00:00.000Z")
                };
                async move { Ok(summary) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();
    let invalidation = || PullRequestInvalidateInput {
        reference: Some(reference.clone()),
        files_viewed_only: None,
    };

    service.refresh_after_turn(&ProjectId::new("p1")).await;
    let mut revision = service.refresh_revision().await;
    // The sync reactor invalidates before reading; it must not notify itself again.
    service.invalidate(invalidation(), false).await;
    assert_eq!(service.refresh_revision().await, revision);
    assert_eq!(service.summary(reference.clone(), true).await.unwrap().state, PullRequestState::Open);

    for next_state in [PullRequestState::Closed, PullRequestState::Open] {
        let mut refreshes = service.subscribe_refreshes().skip(1);
        let refreshed = tokio::spawn({
            let service = service.service.clone();
            let reference = reference.clone();
            async move {
                let next_revision = refreshes.next().await.unwrap();
                (next_revision, service.summary(reference, true).await.unwrap().state)
            }
        });
        state.set(next_state);
        service.invalidate(invalidation(), true).await;
        let (next_revision, observed) = refreshed.await.unwrap();
        assert_eq!(observed, next_state);
        assert!(next_revision > revision);
        revision = next_revision;
    }
}

#[tokio::test]
async fn authorizes_stack_rebases_and_refreshes_sibling_layers() {
    for cross_host in [false, true] {
        let taken = Counter::default();
        let summary_reads = Counter::default();
        let mutation_fails = Cell::new(false);
        let stack_rebase = Cell::new(true);
        let stack_capabilities = |stack_actions: bool| PullRequestCapabilities {
            update_methods: Some(vec![PullRequestUpdateMethod::Rebase]),
            stack_actions: Some(stack_actions),
            ..capabilities(&[PullRequestAction::UpdateBranch], &[PullRequestMergeMethod::Merge])
        };
        let github = FakeProvider {
            capability_sets: std::sync::Arc::new(vec![stack_capabilities(true), stack_capabilities(false)]),
            get_viewer_permissions: {
                let stack_rebase = stack_rebase.clone();
                h(move |_| {
                    let permissions = PullRequestViewerPermissions {
                        stack_rebase: Some(stack_rebase.get()),
                        verdicts: Vec::new(),
                        ..permissions(&[], false, false)
                    };
                    async move { Ok(permissions) }
                })
            },
            get_change_request_summary: Some({
                let summary_reads = summary_reads.clone();
                h(move |_| {
                    summary_reads.bump();
                    async { Ok(summary_row(8, "2026-07-01T00:00:00Z")) }
                })
            }),
            run_action: {
                let (taken, mutation_fails) = (taken.clone(), mutation_fails.clone());
                h(move |_| {
                    taken.bump();
                    let fails = mutation_fails.get();
                    async move {
                        if fails {
                            Err(request_failed())
                        } else {
                            Ok(())
                        }
                    }
                })
            },
            ..FakeProvider::new(Kind::Github)
        };
        let stack_actions = github.capability_set.clone();
        let service = make_service(
            vec![web(), gh("p2", "enterprise", "/b", "acme/web").host("enterprise.test").build()],
            vec![github],
        );
        let base = PullRequestRef {
            host: cross_host.then(|| "enterprise.test".to_owned()),
            ..reference("p1", "acme/web", 3)
        };
        let input = PullRequestActionInput {
            update_method: Some(PullRequestUpdateMethod::Rebase),
            stack_number: Some(50),
            expected_stack_heads: Some(vec![PullRequestStackHead {
                number: 3,
                head_sha: "ccc".into(),
            }]),
            ..action(&base, PullRequestAction::UpdateBranch)
        };
        let unrelated = PullRequestRef { number: 8, ..base.clone() };

        service.run_action(input.clone()).await.unwrap();
        assert_eq!(taken.get(), 1);
        service.summary(unrelated.clone(), true).await.unwrap();
        assert_eq!(summary_reads.get(), 1);
        stack_rebase.set(false);
        assert_eq!(tag(&service.run_action(input.clone()).await.unwrap_err()), "PullRequestOperationError");
        stack_rebase.set(true);
        stack_actions.store(1, Ordering::SeqCst);
        assert_eq!(tag(&service.run_action(input.clone()).await.unwrap_err()), "PullRequestOperationError");
        assert_eq!(taken.get(), 1);
        service.summary(unrelated.clone(), true).await.unwrap();
        assert_eq!(summary_reads.get(), 1);
        stack_actions.store(0, Ordering::SeqCst);
        mutation_fails.set(true);
        service.run_action(input).await.unwrap_err();
        assert_eq!(taken.get(), 2, "cross-host: {cross_host}");
        service.summary(unrelated, true).await.unwrap();
        assert_eq!(summary_reads.get(), 2, "cross-host: {cross_host}");
    }
}

#[tokio::test]
async fn refuses_a_way_of_updating_a_branch_that_the_host_or_the_viewer_does_not_allow() {
    let ran = Log::default();
    let github = FakeProvider {
        get_viewer_permissions: ok(PullRequestViewerPermissions {
            verdicts: vec![PullRequestReviewVerdict::Comment],
            update_methods: Some(vec![PullRequestUpdateMethod::Merge]),
            ..permissions(&[PullRequestAction::Close, PullRequestAction::UpdateBranch], true, false)
        }),
        run_action: recording_action(&ran),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(PullRequestCapabilities {
        // This host brings a stale branch up to date with a merge commit and nothing else.
        update_methods: Some(vec![PullRequestUpdateMethod::Merge]),
        ..capabilities(
            &[PullRequestAction::Merge, PullRequestAction::Close, PullRequestAction::UpdateBranch],
            &[PullRequestMergeMethod::Merge],
        )
    });
    let service = make_service(vec![web()], vec![github]);
    let update = |method| PullRequestActionInput {
        update_method: Some(method),
        ..action(&web_ref(), PullRequestAction::UpdateBranch)
    };

    // Asking for a rebase a host does not offer must fail rather than quietly merge instead.
    let error = service.run_action(update(PullRequestUpdateMethod::Rebase)).await.unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(ran.len(), 0);

    service.run_action(update(PullRequestUpdateMethod::Merge)).await.unwrap();
    assert_eq!(ran.all()[0].update_method, Some(PullRequestUpdateMethod::Merge));
}

#[tokio::test]
async fn refuses_to_merge_a_target_branch_into_a_source_branch_on_a_host_that_only_rebases() {
    let taken = Counter::default();
    let gitlab = FakeProvider {
        get_viewer_permissions: ok(PullRequestViewerPermissions {
            verdicts: vec![PullRequestReviewVerdict::Comment],
            update_methods: Some(vec![PullRequestUpdateMethod::Rebase]),
            ..permissions(&[PullRequestAction::Close, PullRequestAction::UpdateBranch], true, false)
        }),
        run_action: {
            let taken = taken.clone();
            h(move |_| {
                taken.bump();
                async { Ok(()) }
            })
        },
        ..FakeProvider::new(Kind::Gitlab)
    }
    .with_capabilities(PullRequestCapabilities {
        // What GitLab declares: it replays the branch, and has no update that merges the target in.
        update_methods: Some(vec![PullRequestUpdateMethod::Rebase]),
        ..capabilities(
            &[PullRequestAction::Merge, PullRequestAction::Close, PullRequestAction::UpdateBranch],
            &[PullRequestMergeMethod::Merge],
        )
    });
    let service = make_service(vec![gh("p1", "on gitlab", "/a", "group/project").provider("gitlab").build()], vec![gitlab]);
    let update = |method| PullRequestActionInput {
        update_method: Some(method),
        ..action(&reference("p1", "group/project", 1), PullRequestAction::UpdateBranch)
    };

    let error = service.run_action(update(PullRequestUpdateMethod::Merge)).await.unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(taken.get(), 0);

    service.run_action(update(PullRequestUpdateMethod::Rebase)).await.unwrap();
    assert_eq!(taken.get(), 1);
}

fn rewrite(reference: &PullRequestRef, title: Option<&str>, body: Option<&str>) -> PullRequestUpdateInput {
    PullRequestUpdateInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        title: title.map(Into::into),
        body: body.map(Into::into),
    }
}

fn remark(reference: &PullRequestRef, comment_id: &str, kind: PullRequestCommentUpdateInputKind, body: &str) -> PullRequestCommentUpdateInput {
    PullRequestCommentUpdateInput {
        project_id: reference.project_id.clone(),
        host: reference.host.clone(),
        expected_account_id: None,
        allow_stale: None,
        repository: reference.repository.clone(),
        number: reference.number,
        comment_id: comment_id.into(),
        kind,
        body: body.into(),
    }
}

#[tokio::test]
async fn sends_only_the_words_a_rewrite_carries() {
    let received: Log<(Option<String>, Option<String>)> = Log::default();
    let github = FakeProvider {
        update_change_request: Some({
            let received = received.clone();
            h(move |input: UpdateChangeRequestInput| {
                received.push((input.title.clone(), input.body.clone()));
                async { Ok(()) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.update(rewrite(&reference, Some("A better title"), None)).await.unwrap();
    service.update(rewrite(&reference, None, Some(""))).await.unwrap();
    service.update(rewrite(&reference, Some("Both"), Some("at once"))).await.unwrap();

    assert_eq!(
        received.all(),
        vec![
            (Some("A better title".to_owned()), None),
            (None, Some(String::new())),
            (Some("Both".to_owned()), Some("at once".to_owned())),
        ]
    );
}

#[tokio::test]
async fn refuses_a_rewrite_that_changes_nothing_before_any_call_is_made() {
    let github = FakeProvider {
        update_change_request: Some(die("must not be called")),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service.update(rewrite(&web_ref(), None, None)).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert!(error.message().contains("Nothing was changed."));
}

#[tokio::test]
async fn refuses_to_rewrite_anything_on_a_host_that_never_claimed_it() {
    let github = FakeProvider {
        update_change_request: Some(die("must not be called")),
        update_comment: Some(die("must not be called")),
        ..FakeProvider::new(Kind::Github)
    }
    .with_capabilities(capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge]));
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let rewrite_refused = service.update(rewrite(&reference, Some("New"), None)).await.unwrap_err();
    let comment_refused = service
        .update_comment(remark(&reference, "IC_1", PullRequestCommentUpdateInputKind::IssueComment, "New"))
        .await
        .unwrap_err();

    assert!(rewrite_refused.message().contains("cannot rewrite a change request."));
    assert!(comment_refused.message().contains("cannot rewrite a comment."));
}

#[tokio::test]
async fn passes_a_rewritten_remark_through_with_the_id_and_kind_it_arrived_under() {
    let received: Log<(String, PullRequestCommentUpdateInputKind, String)> = Log::default();
    let github = FakeProvider {
        update_comment: Some({
            let received = received.clone();
            h(move |input: UpdateCommentInput| {
                received.push((input.comment_id.clone(), input.kind, input.body.clone()));
                async { Ok(()) }
            })
        }),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    service
        .update_comment(remark(
            &web_ref(),
            "PRRC_1",
            PullRequestCommentUpdateInputKind::ReviewComment,
            "Second thoughts",
        ))
        .await
        .unwrap();

    assert_eq!(
        received.all(),
        vec![(
            "PRRC_1".to_owned(),
            PullRequestCommentUpdateInputKind::ReviewComment,
            "Second thoughts".to_owned()
        )]
    );
}

#[tokio::test]
async fn refuses_a_remark_rewritten_into_nothing_but_whitespace() {
    let github = FakeProvider {
        update_comment: Some(die("must not be called")),
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);

    let error = service
        .update_comment(remark(&web_ref(), "IC_1", PullRequestCommentUpdateInputKind::IssueComment, "   \n  "))
        .await
        .unwrap_err();

    assert!(error.message().contains("A comment cannot be empty."));
}

#[tokio::test]
async fn forgets_the_cached_detail_after_a_rewrite_or_terminal_turn() {
    let core_calls = Counter::default();
    let github = FakeProvider {
        get_change_request: {
            let core_calls = core_calls.clone();
            h(move |_| {
                core_calls.bump();
                async {
                    Ok(ProviderChangeRequestDetail {
                        changed_files: 0,
                        ..detail_of(change_request(1, UPDATED), "")
                    })
                }
            })
        },
        ..FakeProvider::new(Kind::Github)
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.detail(reference.clone()).await.unwrap();
    service.update(rewrite(&reference, Some("Renamed"), None)).await.unwrap();
    service.detail(reference.clone()).await.unwrap();
    assert_eq!(core_calls.get(), 2);

    service.refresh_after_turn(&ProjectId::new("p1")).await;
    service.detail(reference).await.unwrap();
    assert_eq!(core_calls.get(), 3);
}
