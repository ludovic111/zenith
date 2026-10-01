//! `PullRequestService.test.ts`, the listing half: which projects are read, the provider
//! switcher, continuation cursors, viewers, batched reads across a host, caching and
//! invalidation of listings, row filters, and the line counts.

mod support_service;

use std::sync::Arc;

use support_service::*;
use zc_contracts::*;
use zc_pullrequest::provider::*;

fn numbers(result: &PullRequestListResult) -> Vec<i64> {
    result.entries.iter().map(|entry| entry.number).collect()
}

/// A listing handler that answers `items` (truncated, continues) and logs the repositories.
fn listing(
    log: &Log<String>,
    items: Vec<ProviderChangeRequest>,
    truncated: bool,
    continues: bool,
) -> Handler<ListChangeRequestsInput, ProviderChangeRequestPage> {
    let log = log.clone();
    h(move |input: ListChangeRequestsInput| {
        log.push(input.repository.clone());
        let items = items.clone();
        async move { Ok(page(items, truncated, continues)) }
    })
}

#[tokio::test]
async fn refines_unknown_self_hosted_gitlab_projects_before_listing_merge_requests() {
    let calls = Counter::default();
    let self_hosted = gh("p1", "self-hosted", "/gitlab", "group/project")
        .provider("unknown")
        .host("code.example.test");
    let worktree = ProjectSpec {
        id: "p2".into(),
        workspace_root: "/gitlab-worktree".into(),
        ..self_hosted.clone()
    };
    let refine: Refine = {
        let calls = calls.clone();
        Arc::new(move |_, context| {
            calls.bump();
            assert_eq!(context.remote_url, "https://code.example.test/group/project.git");
            Some(SourceControlProviderInfo {
                kind: Kind::Gitlab,
                ..context.provider.clone()
            })
        })
    };
    let service = make_service_with(vec![self_hosted.build(), worktree.build()], vec![FakeProvider::new(Kind::Gitlab)], Some(refine));

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(calls.get(), 1);
    assert_eq!(result.providers[0].host, "code.example.test");
    assert_eq!(result.providers[0].kind, Kind::Gitlab);
}

#[tokio::test]
async fn derives_a_legacy_repository_host_after_refining_its_provider() {
    let mut legacy = gh("p1", "legacy self-hosted", "/gitlab", "group/project")
        .provider("unknown")
        .host("code.example.test")
        .build();
    // Persisted identities from before canonicalKey existed are still accepted at runtime.
    if let Some(Some(identity)) = legacy.repository_identity.as_mut() {
        identity.canonical_key = String::new();
    }
    let service = make_service_with(vec![legacy], vec![FakeProvider::new(Kind::Gitlab)], Some(refine_to(Kind::Gitlab)));

    let result = service
        .list(PullRequestListInput {
            host: Some("gitlab".into()),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(result.providers[0].host, "gitlab");
    assert_eq!(result.providers[0].kind, Kind::Gitlab);
}

#[tokio::test]
async fn tries_another_checkout_when_provider_refinement_remains_unknown() {
    let asked: Log<String> = Log::default();
    let self_hosted = gh("p1", "self-hosted", "/gone", "group/project").provider("unknown").host("code.example.test");
    let healthy = ProjectSpec {
        id: "p2".into(),
        workspace_root: "/healthy".into(),
        ..self_hosted.clone()
    };
    let refine: Refine = {
        let asked = asked.clone();
        Arc::new(move |cwd, context| {
            asked.push(cwd.to_owned());
            (cwd != "/gone").then(|| SourceControlProviderInfo {
                kind: Kind::Gitlab,
                ..context.provider.clone()
            })
        })
    };
    let service = make_service_with(vec![self_hosted.build(), healthy.build()], vec![FakeProvider::new(Kind::Gitlab)], Some(refine));

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(asked.all(), vec!["/gone", "/healthy"]);
    assert_eq!(result.providers[0].kind, Kind::Gitlab);
}

#[tokio::test]
async fn reads_nothing_from_a_host_with_no_implementation_but_reports_it() {
    let listed = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = listing(&listed, vec![change_request(1, "2026-07-02T00:00:00Z")], false, true);
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            project("p2", "notes", "/b", None).build(),
            gh("p3", "on gitlab", "/c", "group/project").provider("gitlab").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(listed.all(), vec!["pingdotgg/t3code"]);
    assert_eq!(result.entries[0].provider, Kind::Github);
    // The GitLab project is explained rather than quietly missing from the page.
    assert_eq!(
        result
            .providers
            .iter()
            .map(|summary| (summary.kind, summary.configured, summary.project_count))
            .collect::<Vec<_>>(),
        vec![(Kind::Github, true, 1), (Kind::Gitlab, false, 1)]
    );
}

#[tokio::test]
async fn asks_for_a_whole_page_of_a_host_and_for_the_readers_own_size_when_given_one() {
    let limits: Log<i64> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let limits = limits.clone();
        h(move |input: ListChangeRequestsInput| {
            limits.push(input.limit);
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    service.list(open_list()).await.unwrap();
    service
        .list(PullRequestListInput {
            limit: Some(10),
            ..open_list()
        })
        .await
        .unwrap();

    // Providers probe with one row over this, so 99 asks a host for 100.
    assert_eq!(limits.all(), vec![99, 10]);
}

#[tokio::test]
async fn says_where_each_repository_carries_on_and_from_nothing_it_has_run_out_of() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = h(|input: ListChangeRequestsInput| async move {
        Ok(page(
            vec![change_request(1, "2026-07-02T00:00:00Z")],
            input.repository == "pingdotgg/t3code",
            true,
        ))
    });
    let service = make_service(
        vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build(), gh("p2", "web", "/b", "acme/web").build()],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    // The instant of the oldest row, how many rows have gone, and the row already sent at that
    // instant. The repository that had nothing more is simply not in it.
    assert_eq!(
        result.next_cursors,
        cursors(&[("github.com pingdotgg/t3code", "2026-07-02T00:00:00Z|1|1")]).unwrap()
    );
}

#[tokio::test]
async fn offers_no_continuation_for_a_host_that_cannot_be_carried_on_from() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], true, false));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let result = service.list(open_list()).await.unwrap();

    // More rows exist and no cursor reaches them, which is what asking for a larger page is for.
    assert!(result.truncated);
    assert!(result.next_cursors.is_empty());
}

#[tokio::test]
async fn uses_a_providers_raw_cursor_advance_when_it_consumed_malformed_rows() {
    let mut azure = FakeProvider::new(Kind::AzureDevops);
    azure.list_change_requests = ok(ProviderChangeRequestPage {
        cursor_advance: Some(4),
        ..page(vec![change_request(7, "2026-07-02T00:00:00Z")], true, true)
    });
    let service = make_service(
        vec![gh("p1", "web", "/a", "acme/web").provider("azure-devops").host("dev.azure.com").build()],
        vec![azure],
    );

    let result = service.list(open_list()).await.unwrap();

    // Keyed by the selector Azure is actually asked with, which is the repository's own name.
    assert_eq!(
        result.next_cursors,
        cursors(&[("dev.azure.com dev.azure.com/acme/web", "2026-07-02T00:00:00Z|4|7")]).unwrap()
    );
}

#[tokio::test]
async fn reads_only_the_repositories_it_was_asked_to_carry_on_with() {
    let listed: Log<String> = Log::default();
    let seen_cursors: Log<Option<ProviderListCursor>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let listed = listed.clone();
        let seen_cursors = seen_cursors.clone();
        h(move |input: ListChangeRequestsInput| {
            listed.push(input.repository.clone());
            seen_cursors.push(input.cursor.clone());
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let service = make_service(
        vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build(), gh("p2", "web", "/b", "acme/web").build()],
        vec![github],
    );

    let result = service
        .list(PullRequestListInput {
            cursors: cursors(&[("github.com acme/web", "2026-07-02T00:00:00Z|99|7")]),
            ..open_list()
        })
        .await
        .unwrap();

    // The other repository is already on the page; the host summaries stay over the workspace.
    assert_eq!(listed.all(), vec!["acme/web"]);
    assert_eq!(
        seen_cursors.all(),
        vec![Some(ProviderListCursor {
            updated_before: "2026-07-02T00:00:00Z".into(),
            delivered: 99,
        })]
    );
    assert_eq!(result.providers.len(), 1);
}

#[tokio::test]
async fn keeps_a_row_already_sent_at_the_boundary_instant_from_arriving_twice() {
    let mut github = FakeProvider::new(Kind::Github);
    // The boundary instant is asked for inclusively, so the host hands back the rows already sent
    // at it alongside the ones beside them.
    github.list_change_requests = ok(page(
        vec![
            change_request(7, "2026-07-02T00:00:00Z"),
            change_request(8, "2026-07-02T00:00:00Z"),
            change_request(9, "2026-07-01T00:00:00Z"),
        ],
        true,
        true,
    ));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let result = service
        .list(PullRequestListInput {
            cursors: cursors(&[("github.com pingdotgg/t3code", "2026-07-02T00:00:00Z|1|7")]),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(numbers(&result), vec![8, 9]);
    assert_eq!(
        result.next_cursors,
        cursors(&[("github.com pingdotgg/t3code", "2026-07-01T00:00:00Z|3|9")]).unwrap()
    );
}

#[tokio::test]
async fn keeps_the_earlier_exclusions_when_a_slice_ends_on_the_instant_it_began_on() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(
        vec![change_request(7, "2026-07-02T00:00:00Z"), change_request(8, "2026-07-02T00:00:00Z")],
        true,
        true,
    ));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let result = service
        .list(PullRequestListInput {
            cursors: cursors(&[("github.com pingdotgg/t3code", "2026-07-02T00:00:00Z|1|6")]),
            ..open_list()
        })
        .await
        .unwrap();

    // The next read has to keep excluding 6 as well as the two just sent.
    assert_eq!(numbers(&result), vec![7, 8]);
    assert_eq!(
        result.next_cursors,
        cursors(&[("github.com pingdotgg/t3code", "2026-07-02T00:00:00Z|3|6,7,8")]).unwrap()
    );
}

#[tokio::test]
async fn refuses_a_continuation_it_did_not_issue_before_asking_any_host_anything() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = die("should not be read");
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service
        .list(PullRequestListInput {
            cursors: cursors(&[("github.com pingdotgg/t3code", "yesterday")]),
            ..open_list()
        })
        .await
        .unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(
        error.message(),
        "Pull request operation list failed: The list could not be carried on from where it left off."
    );
}

#[tokio::test]
async fn calls_a_transient_viewer_failure_a_failed_operation_not_a_signed_out_cli() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = fail(failed(Kind::Github, "getViewer", "HTTP 500"));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service.list(open_list()).await.unwrap_err();

    // `cli-unauthenticated` would send the reader to `gh auth login` over a transient error.
    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn reports_an_unusable_host_over_a_merely_failing_one() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = fail(failed(Kind::Github, "getViewer", "HTTP 500"));
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.get_viewer = fail(unusable(Kind::Gitlab, ProviderFailureReason::MissingTool));
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "on gitlab", "/c", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let error = service.list(open_list()).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestUnavailableError");
    assert!(error.message().contains("glab"));
}

#[tokio::test]
async fn lists_every_host_that_has_an_implementation() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-01T00:00:00Z")], false, true));
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    // Nested groups need the full path, not the last two segments.
    gitlab.list_change_requests = h(|input: ListChangeRequestsInput| async move {
        assert_eq!(input.repository, "group/sub/project", "wrong repository identity");
        Ok(page(vec![change_request(2, "2026-07-05T00:00:00Z")], false, true))
    });
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "on gitlab", "/b", "group/sub/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(
        result.entries.iter().map(|entry| (entry.provider, entry.number)).collect::<Vec<_>>(),
        vec![(Kind::Gitlab, 2), (Kind::Github, 1)]
    );
}

#[tokio::test]
async fn narrows_the_listing_to_one_host_when_asked() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = die("should not be read");
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = ok(page(vec![change_request(2, "2026-07-05T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "on gitlab", "/b", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let result = service
        .list(PullRequestListInput {
            host: Some("gitlab.com".into()),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(result.entries.iter().map(|entry| entry.provider).collect::<Vec<_>>(), vec![Kind::Gitlab]);
}

#[tokio::test]
async fn tells_two_hosts_of_one_kind_apart_in_the_switcher_and_the_filter() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = h(|input: ListChangeRequestsInput| async move {
        let items = if input.host == "ghe.example.com" {
            vec![change_request(2, "2026-07-05T00:00:00Z")]
        } else {
            Vec::new()
        };
        Ok(page(items, false, true))
    });
    let service = make_service(
        vec![
            gh("p1", "on github.com", "/a", "ping/one").build(),
            gh("p2", "on the enterprise install", "/b", "ping/two").host("ghe.example.com").build(),
        ],
        vec![github],
    );

    // Both hosts are GitHub, so a switcher keyed by provider kind would offer one pill for both.
    let all = service.list(open_list()).await.unwrap();
    assert_eq!(
        all.providers
            .iter()
            .map(|summary| (summary.host.clone(), summary.kind, summary.project_count))
            .collect::<Vec<_>>(),
        vec![("github.com".to_owned(), Kind::Github, 1), ("ghe.example.com".to_owned(), Kind::Github, 1)]
    );

    let scoped = service
        .list(PullRequestListInput {
            host: Some("ghe.example.com".into()),
            ..open_list()
        })
        .await
        .unwrap();
    assert_eq!(
        scoped.entries.iter().map(|entry| (entry.host.clone(), entry.number)).collect::<Vec<_>>(),
        vec![("ghe.example.com".to_owned(), 2)]
    );
}

#[tokio::test]
async fn keeps_one_host_listed_when_another_is_not_set_up() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-01T00:00:00Z")], false, true));
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.get_viewer = fail(unusable(Kind::Gitlab, ProviderFailureReason::MissingTool));
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "on gitlab", "/b", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(result.entries.iter().map(|entry| entry.provider).collect::<Vec<_>>(), vec![Kind::Github]);
    assert_eq!(
        result.providers.iter().map(|summary| (summary.kind, summary.configured)).collect::<Vec<_>>(),
        vec![(Kind::Github, true), (Kind::Gitlab, false)]
    );
}

#[tokio::test]
async fn fails_as_unavailable_only_when_no_host_can_be_read() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = fail(unusable(Kind::Github, ProviderFailureReason::MissingTool));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let error = service.list(open_list()).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestUnavailableError");
    assert_eq!(reason_of(&error), Some(PullRequestUnavailableReason::CliMissing));
}

#[tokio::test]
async fn reads_a_repository_once_when_several_worktrees_share_it() {
    let calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let calls = calls.clone();
        h(move |_| {
            calls.bump();
            async { Ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true)) }
        })
    };
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "t3code worktree", "/b", "PingDotGG/T3Code").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(calls.get(), 1);
    assert_eq!(result.entries.len(), 1);
}

#[tokio::test]
async fn keeps_healthy_repositories_when_one_of_them_cannot_be_read() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = h(|input: ListChangeRequestsInput| async move {
        if input.repository == "pingdotgg/broken" {
            Err(request_failed())
        } else {
            Ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true))
        }
    });
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "broken", "/b", "pingdotgg/broken").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.errors.iter().map(|error| error.project_title.clone()).collect::<Vec<_>>(),
        vec!["broken"]
    );
}

#[tokio::test]
async fn tries_another_workspace_on_the_same_host_for_the_viewer() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = h(|input: ProviderHostRef| async move {
        if input.cwd == "/healthy" {
            Ok("bilal".to_owned())
        } else {
            Err(unusable(Kind::Github, ProviderFailureReason::MissingTool))
        }
    });
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            gh("p1", "broken", "/broken", "acme/one").build(),
            gh("p2", "healthy", "/healthy", "acme/two").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.viewers.get("github.com").map(String::as_str), Some("bilal"));
}

#[tokio::test]
async fn keeps_two_hosts_of_one_provider_kind_as_two_accounts() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = h(|input: ProviderHostRef| async move {
        Ok(match input.cwd.as_str() {
            "/cloud" => "bilal",
            "/enterprise" => "b.hassan",
            _ => "unknown",
        }
        .to_owned())
    });
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            gh("p1", "cloud", "/cloud", "acme/web").build(),
            // The same path on a different host: neither the viewer nor the row may be shared.
            gh("p2", "enterprise", "/enterprise", "acme/web").host("github.acme.dev").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(result.entries.len(), 2);
    assert_eq!(
        result.viewers,
        [
            ("github.com".to_owned(), "bilal".to_owned()),
            ("github.acme.dev".to_owned(), "b.hassan".to_owned())
        ]
        .into_iter()
        .collect()
    );
    let mut hosts: Vec<String> = result.entries.iter().map(|entry| entry.host.clone()).collect();
    hosts.sort();
    assert_eq!(hosts, vec!["github.acme.dev", "github.com"]);
}

#[tokio::test]
async fn reports_repositories_on_a_host_that_could_not_be_read() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = h(|input: ProviderHostRef| async move {
        if input.cwd == "/cloud" {
            Ok("bilal".to_owned())
        } else {
            Err(unusable(Kind::Github, ProviderFailureReason::Unauthenticated))
        }
    });
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            gh("p1", "cloud", "/cloud", "acme/web").build(),
            gh("p2", "enterprise", "/enterprise", "acme/api").host("github.acme.dev").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    // The healthy host still lists, and the unreadable one is named rather than dropped.
    assert_eq!(result.entries.len(), 1);
    assert_eq!(
        result.errors.iter().map(|error| error.project_id.as_str().to_owned()).collect::<Vec<_>>(),
        vec!["p2"]
    );
}

#[tokio::test]
async fn stops_new_reads_after_a_rate_limit_while_leaving_manual_actions_available() {
    let list_calls = Counter::default();
    let action_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let list_calls = list_calls.clone();
        h(move |_| {
            list_calls.bump();
            async { Err(rate_limited(Kind::Github, "listChangeRequests", "GitHub API rate limit exceeded.", None)) }
        })
    };
    github.run_action = {
        let action_calls = action_calls.clone();
        h(move |_| {
            action_calls.bump();
            async { Ok(()) }
        })
    };
    let service = make_service(vec![gh("p1", "cloud", "/cloud", "acme/web").build()], vec![github]);

    let first = service
        .list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::All),
            ..open_list()
        })
        .await
        .unwrap();
    let paused = service
        .list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::Authored),
            ..open_list()
        })
        .await
        .unwrap();
    service
        .run_action(action(&reference("p1", "acme/web", 1), PullRequestAction::Close))
        .await
        .unwrap();

    assert_eq!(list_calls.get(), 1);
    assert_eq!(action_calls.get(), 1);
    assert_eq!(first.errors.len(), 1);
    assert_eq!(paused.errors.len(), 1);
}

#[tokio::test]
async fn uses_a_manual_rate_limit_to_pause_later_reads() {
    let list_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let list_calls = list_calls.clone();
        h(move |_| {
            list_calls.bump();
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    github.run_action = fail(rate_limited(Kind::Github, "runAction", "GitHub API rate limit exceeded.", None));
    let service = make_service(vec![gh("p1", "cloud", "/cloud", "acme/web").build()], vec![github]);

    service
        .run_action(action(&reference("p1", "acme/web", 1), PullRequestAction::Close))
        .await
        .unwrap_err();
    let error = service
        .list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::All),
            ..open_list()
        })
        .await
        .unwrap_err();

    assert_eq!(list_calls.get(), 0);
    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn flags_a_review_request_for_the_viewer_but_not_on_their_own_change_request() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(
        vec![
            ProviderChangeRequest {
                review_request_logins: vec!["Bilal".into()],
                ..change_request(1, "2026-07-02T00:00:00Z")
            },
            ProviderChangeRequest {
                author: Some(actor("bilal")),
                review_request_logins: vec!["bilal".into()],
                ..change_request(2, "2026-07-02T00:00:00Z")
            },
        ],
        false,
        true,
    ));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(
        result.entries.iter().map(|entry| entry.viewer_review_requested).collect::<Vec<_>>(),
        vec![true, false]
    );
}

#[tokio::test]
async fn hands_the_provider_the_host_its_repository_lives_on() {
    let hosts: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let hosts = hosts.clone();
        h(move |input: ListChangeRequestsInput| {
            hosts.push(input.host.clone());
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let service = make_service(vec![gh("p1", "enterprise", "/a", "acme/web").host("github.acme.dev").build()], vec![github]);

    service.list(open_list()).await.unwrap();

    // The host travels separately, or a GitHub Enterprise repository is read off github.com.
    assert_eq!(hosts.all(), vec!["github.acme.dev"]);
}

#[tokio::test]
async fn asks_every_host_the_readers_search_rather_than_filtering_what_came_back() {
    let asked: Log<Option<String>> = Log::default();
    let listing = {
        let asked = asked.clone();
        h(move |input: ListChangeRequestsInput| {
            asked.push(input.query.clone());
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = listing.clone();
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = listing;
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "on gitlab", "/b", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    service
        .list(PullRequestListInput {
            query: Some("pull requests page".into()),
            ..open_list()
        })
        .await
        .unwrap();

    // A search that stopped at the service could only find what was already loaded.
    assert_eq!(asked.all(), vec![Some("pull requests page".to_owned()), Some("pull requests page".to_owned())]);
}

#[tokio::test]
async fn asks_for_no_search_when_the_reader_has_typed_nothing() {
    let asked: Log<Option<String>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let asked = asked.clone();
        h(move |input: ListChangeRequestsInput| {
            asked.push(input.query.clone());
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);

    service.list(open_list()).await.unwrap();

    assert_eq!(asked.all(), vec![None]);
}

#[tokio::test]
async fn asks_another_checkout_who_is_signed_in_when_the_first_one_cannot_answer() {
    let asked: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = {
        let asked = asked.clone();
        h(move |input: ProviderHostRef| {
            asked.push(input.cwd.clone());
            async move {
                if input.cwd == "/gone" {
                    Err(failed(Kind::Github, "getViewer", "not a git repository"))
                } else {
                    Ok("bilal".to_owned())
                }
            }
        })
    };
    github.list_change_requests = ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            // One repository, checked out twice: the listing reads it once; the viewer lookup has
            // two places to ask.
            gh("p1", "t3code (stale worktree)", "/gone", "pingdotgg/t3code").build(),
            gh("p2", "t3code", "/healthy", "pingdotgg/t3code").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(asked.all(), vec!["/gone", "/healthy"]);
    assert_eq!(result.entries.len(), 1);
    assert!(result.providers[0].configured);
}

#[tokio::test]
async fn answers_a_repeated_listing_from_cache_and_concurrent_readers_share_one_request() {
    let host_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let host_calls = host_calls.clone();
        h(move |_| {
            host_calls.bump();
            async {
                tokio::task::yield_now().await;
                Ok(page(vec![change_request(1, "2026-07-02T00:00:00Z")], false, false))
            }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let (first, second) = tokio::join!(service.list(open_list()), service.list(open_list()));
    first.unwrap();
    second.unwrap();
    service.list(open_list()).await.unwrap();
    assert_eq!(host_calls.get(), 1);

    // A different filter is a different answer, not a cache hit.
    service.list(list_input(PullRequestListState::All)).await.unwrap();
    assert_eq!(host_calls.get(), 2);
}

#[tokio::test]
async fn shares_one_cold_viewer_lookup_across_distinct_concurrent_lists() {
    let viewer_calls = Counter::default();
    let list_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = {
        let viewer_calls = viewer_calls.clone();
        h(move |_| {
            viewer_calls.bump();
            async {
                tokio::task::yield_now().await;
                Ok("bilal".to_owned())
            }
        })
    };
    github.list_change_requests = {
        let list_calls = list_calls.clone();
        h(move |_| {
            list_calls.bump();
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let lists = [PullRequestInvolvement::All, PullRequestInvolvement::Authored, PullRequestInvolvement::Reviewing].map(|involvement| {
        service.list(PullRequestListInput {
            involvement: Some(involvement),
            ..open_list()
        })
    });
    for result in futures::future::join_all(lists).await {
        result.unwrap();
    }

    assert_eq!(viewer_calls.get(), 1);
    assert_eq!(list_calls.get(), 3);
}

#[tokio::test]
async fn uses_five_host_reads_for_the_normal_indexed_repository_page_workflow() {
    let viewer_calls = Counter::default();
    let search_calls = Counter::default();
    let fallback_calls = Counter::default();
    let stats_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = {
        let viewer_calls = viewer_calls.clone();
        h(move |_| {
            viewer_calls.bump();
            async { Ok("bilal".to_owned()) }
        })
    };
    github.list_change_requests_across = Some({
        let search_calls = search_calls.clone();
        h(move |input: ListChangeRequestsAcrossInput| {
            search_calls.bump();
            async move {
                let items = if input.involvement == PullRequestInvolvement::All {
                    vec![batched(1, "acme/web", "2026-07-02T00:00:00Z")]
                } else {
                    Vec::new()
                };
                Ok(ProviderBatchedChangeRequestPage { items, truncated: false })
            }
        })
    });
    github.list_change_requests = {
        let fallback_calls = fallback_calls.clone();
        h(move |_| {
            fallback_calls.bump();
            async { Ok(page(Vec::new(), false, true)) }
        })
    };
    github.list_change_request_stats = Some({
        let stats_calls = stats_calls.clone();
        h(move |_| {
            stats_calls.bump();
            async {
                Ok(vec![ProviderChangeRequestStat {
                    repository: "acme/web".into(),
                    number: 1,
                    additions: 3,
                    deletions: 1,
                }])
            }
        })
    });
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let baseline = service
        .list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::All),
            ..open_list()
        })
        .await
        .unwrap();
    let (authored, reviewing) = tokio::join!(
        service.list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::Authored),
            ..open_list()
        }),
        service.list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::Reviewing),
            ..open_list()
        })
    );
    authored.unwrap();
    reviewing.unwrap();
    service
        .list_stats(PullRequestListStatsInput {
            refs: baseline
                .entries
                .iter()
                .map(|entry| reference(entry.project_id.as_str(), &entry.repository, entry.number))
                .collect(),
        })
        .await
        .unwrap();

    assert_eq!((viewer_calls.get(), search_calls.get(), fallback_calls.get(), stats_calls.get()), (1, 3, 0, 1));
}

#[tokio::test]
async fn returns_the_refreshed_listing_on_the_first_read_after_its_cache_expires() {
    let host_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let host_calls = host_calls.clone();
        h(move |_| {
            let number = host_calls.bump() as i64;
            async move { Ok(page(vec![change_request(number, "2026-07-02T00:00:00Z")], false, false)) }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let first = service.list(open_list()).await.unwrap();
    assert_eq!(numbers(&first), vec![1]);

    service.adjust(31_000).await;
    let refreshed = service.list(open_list()).await.unwrap();

    assert_eq!(host_calls.get(), 2);
    assert_eq!(numbers(&refreshed), vec![2]);
}

#[tokio::test]
async fn a_listing_narrowed_to_some_projects_is_its_own_cache_entry() {
    let asked: Log<Vec<String>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests_across = Some({
        let asked = asked.clone();
        h(move |input: ListChangeRequestsAcrossInput| {
            asked.push(input.repositories.clone());
            async move {
                Ok(ProviderBatchedChangeRequestPage {
                    items: input
                        .repositories
                        .iter()
                        .enumerate()
                        .map(|(index, repository)| batched(index as i64 + 1, repository, "2026-07-02T00:00:00Z"))
                        .collect(),
                    truncated: false,
                })
            }
        })
    });
    let service = make_service(
        vec![gh("p1", "web", "/a", "acme/web").build(), gh("p2", "docs", "/b", "acme/docs").build()],
        vec![github],
    );

    service.list(open_list()).await.unwrap();
    let narrowed = service
        .list(PullRequestListInput {
            project_ids: Some(vec![ProjectId::new("p2")]),
            ..open_list()
        })
        .await
        .unwrap();

    // The narrowing is part of the key, so it reads its own scope instead of the wider answer.
    assert_eq!(
        asked.all(),
        vec![vec!["acme/web".to_owned(), "acme/docs".to_owned()], vec!["acme/docs".to_owned()]]
    );
    assert_eq!(
        narrowed.entries.iter().map(|entry| entry.repository.clone()).collect::<Vec<_>>(),
        vec!["acme/docs"]
    );

    service
        .list(PullRequestListInput {
            project_ids: Some(vec![ProjectId::new("p2")]),
            ..open_list()
        })
        .await
        .unwrap();
    assert_eq!(asked.len(), 2);
}

#[tokio::test]
async fn keeps_listing_freshness_tied_to_read_start_when_filtered_reads_finish_out_of_order() {
    let older_started = Gate::default();
    let release_older = Gate::default();
    let reads = Counter::default();
    let updated_at = "2026-07-02T00:00:00Z";
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let (older_started, release_older, reads) = (older_started.clone(), release_older.clone(), reads.clone());
        h(move |input: ListChangeRequestsInput| {
            reads.bump();
            let older = input.filters.as_ref().and_then(|filters| filters.checks) == Some(PullRequestListFiltersChecks::Failing);
            let (older_started, release_older) = (older_started.clone(), release_older.clone());
            async move {
                if older {
                    older_started.open();
                    release_older.wait().await;
                }
                Ok(page(
                    vec![ProviderChangeRequest {
                        checks_state: Some(Some(if older {
                            PullRequestChecksState::Failing
                        } else {
                            PullRequestChecksState::Passing
                        })),
                        mergeability: if older {
                            PullRequestMergeability::Mergeable
                        } else {
                            PullRequestMergeability::Conflicting
                        },
                        ..change_request(1, updated_at)
                    }],
                    false,
                    false,
                ))
            }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);
    let input = |checks| PullRequestListInput {
        filters: Some(PullRequestListFilters {
            checks: Some(checks),
            ..no_filters()
        }),
        ..open_list()
    };

    let older_read = tokio::spawn({
        let service = service.service.clone();
        let input = input(PullRequestListFiltersChecks::Failing);
        async move { service.list(input).await }
    });
    older_started.wait().await;
    service.adjust(1_000).await;
    let newer = service.list(input(PullRequestListFiltersChecks::Passing)).await.unwrap();
    release_older.open();
    let older = older_read.await.unwrap().unwrap();

    assert_eq!(older.entries[0].checks_state, Some(PullRequestChecksState::Failing));
    assert_eq!(older.entries[0].mergeability, PullRequestMergeability::Mergeable);
    assert_eq!(newer.entries[0].checks_state, Some(PullRequestChecksState::Passing));
    assert_eq!(newer.entries[0].mergeability, PullRequestMergeability::Conflicting);
    let (older_at, newer_at) = (older.entries[0].observed_at.unwrap().0, newer.entries[0].observed_at.unwrap().0);
    assert!(older_at < newer_at);

    let cached_older = service.list(input(PullRequestListFiltersChecks::Failing)).await.unwrap();
    assert_eq!(cached_older.entries[0].observed_at.unwrap().0, older_at);
    assert_eq!(reads.get(), 2);
}

#[tokio::test]
async fn explicit_and_turn_invalidations_make_the_next_listing_ask_the_host_again() {
    let host_calls = Counter::default();
    let viewer_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = {
        let viewer_calls = viewer_calls.clone();
        h(move |_| {
            viewer_calls.bump();
            async { Ok("bilal".to_owned()) }
        })
    };
    github.list_change_requests = {
        let host_calls = host_calls.clone();
        h(move |_| {
            host_calls.bump();
            async { Ok(page(Vec::new(), false, false)) }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);
    let reference = reference("p1", "acme/web", 1);

    service.list(open_list()).await.unwrap();
    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: None,
                files_viewed_only: None,
            },
            false,
        )
        .await;
    service.list(open_list()).await.unwrap();
    assert_eq!(host_calls.get(), 2);
    assert_eq!(viewer_calls.get(), 2);

    // Forgetting one change request leaves the listings shared.
    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: Some(reference),
                files_viewed_only: None,
            },
            false,
        )
        .await;
    service.list(open_list()).await.unwrap();
    assert_eq!(host_calls.get(), 2);
    service.refresh_after_turn(&ProjectId::new("p1")).await;
    let refresh = service.refresh_revision().await;
    service.list(open_list()).await.unwrap();
    assert!(refresh > 0);
    assert_eq!(host_calls.get(), 3);
}

#[tokio::test]
async fn does_not_cache_a_failed_listing() {
    let host_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    // The viewer lookup is what fails the whole listing rather than one repository.
    github.get_viewer = {
        let host_calls = host_calls.clone();
        h(move |_| {
            let call = host_calls.bump();
            async move {
                if call == 1 {
                    Err(request_failed())
                } else {
                    Ok("bilal".to_owned())
                }
            }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let error = service.list(open_list()).await.unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    let second = service.list(open_list()).await.unwrap();
    assert_eq!(host_calls.get(), 2);
    assert!(second.providers[0].configured);
}

#[tokio::test]
async fn reads_a_hosts_repositories_in_one_search_and_files_the_rows_back_under_each() {
    let asked: Log<Vec<String>> = Log::default();
    let separately: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = listing(&separately, Vec::new(), false, true);
    github.list_change_requests_across = Some({
        let asked = asked.clone();
        h(move |input: ListChangeRequestsAcrossInput| {
            asked.push(input.repositories.clone());
            async {
                Ok(ProviderBatchedChangeRequestPage {
                    items: vec![
                        batched(1, "acme/web", "2026-07-03T00:00:00Z"),
                        batched(2, "pingdotgg/t3code", "2026-07-02T00:00:00Z"),
                    ],
                    truncated: false,
                })
            }
        })
    });
    // A host with no search across repositories keeps being asked one at a time.
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = listing(&separately, vec![change_request(3, "2026-07-01T00:00:00Z")], false, true);
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "web", "/b", "acme/web").build(),
            gh("p3", "on gitlab", "/c", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(asked.all(), vec![vec!["pingdotgg/t3code".to_owned(), "acme/web".to_owned()]]);
    assert_eq!(separately.all(), vec!["group/project"]);
    // Ordered by update across every host, and each row under its repository's project.
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| (entry.project_id.as_str().to_owned(), entry.number))
            .collect::<Vec<_>>(),
        vec![("p2".to_owned(), 1), ("p1".to_owned(), 2), ("p3".to_owned(), 3)]
    );
}

#[tokio::test]
async fn carries_every_repository_of_a_slice_on_from_the_oldest_row_in_it() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests_across = Some(ok(ProviderBatchedChangeRequestPage {
        items: vec![
            batched(1, "acme/web", "2026-07-03T00:00:00Z"),
            batched(2, "pingdotgg/t3code", "2026-07-02T00:00:00Z"),
            batched(3, "acme/web", "2026-07-02T00:00:00Z"),
        ],
        truncated: true,
    }));
    let service = make_service(
        vec![
            gh("p1", "t3code", "/a", "pingdotgg/t3code").build(),
            gh("p2", "web", "/b", "acme/web").build(),
            gh("p3", "docs", "/c", "acme/docs").build(),
        ],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    // The boundary is the oldest row of the whole slice; `acme/docs`, which the slice holds
    // nothing of, is read on its own.
    assert!(result.truncated);
    assert_eq!(
        result.next_cursors,
        cursors(&[
            ("github.com pingdotgg/t3code", "2026-07-02T00:00:00Z|1|2"),
            ("github.com acme/web", "2026-07-02T00:00:00Z|2|3")
        ])
        .unwrap()
    );
}

#[tokio::test]
async fn carries_a_slice_on_without_sending_the_rows_it_already_sent() {
    let seen_cursors: Log<Option<ProviderListCursor>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests_across = Some({
        let seen_cursors = seen_cursors.clone();
        h(move |input: ListChangeRequestsAcrossInput| {
            seen_cursors.push(input.cursor.clone());
            async {
                Ok(ProviderBatchedChangeRequestPage {
                    items: vec![batched(3, "acme/web", "2026-07-02T00:00:00Z"), batched(4, "acme/web", "2026-07-02T00:00:00Z")],
                    truncated: true,
                })
            }
        })
    });
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let result = service
        .list(PullRequestListInput {
            cursors: cursors(&[("github.com acme/web", "2026-07-02T00:00:00Z|1|3")]),
            ..open_list()
        })
        .await
        .unwrap();

    // The row already sent at the boundary comes back, is dropped, and stays named in the next
    // cursor, which has not moved off that instant.
    assert_eq!(
        seen_cursors.all(),
        vec![Some(ProviderListCursor {
            updated_before: "2026-07-02T00:00:00Z".into(),
            delivered: 1,
        })]
    );
    assert_eq!(numbers(&result), vec![4]);
    assert_eq!(
        result.next_cursors,
        cursors(&[("github.com acme/web", "2026-07-02T00:00:00Z|2|3,3,4")]).unwrap()
    );
}

#[tokio::test]
async fn reads_a_workspace_larger_than_one_search_in_chunks_and_merges_them() {
    let asked: Log<usize> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests_across = Some({
        let asked = asked.clone();
        h(move |input: ListChangeRequestsAcrossInput| {
            asked.push(input.repositories.len());
            async move {
                Ok(ProviderBatchedChangeRequestPage {
                    items: input
                        .repositories
                        .iter()
                        .enumerate()
                        .map(|(index, repository)| batched(index as i64 + 1, repository, "2026-07-02T00:00:00Z"))
                        .collect(),
                    truncated: false,
                })
            }
        })
    });
    let projects = (0..101)
        .map(|index| {
            gh(
                &format!("p{index}"),
                &format!("repo {index}"),
                &format!("/w{index}"),
                &format!("acme/repo{index}"),
            )
            .build()
        })
        .collect();
    let service = make_service(projects, vec![github]);

    let result = service.list(open_list()).await.unwrap();

    assert_eq!(asked.all(), vec![100, 1]);
    assert_eq!(result.entries.len(), 101);
}

#[tokio::test]
async fn asks_on_its_own_for_a_repository_a_search_answered_nothing_for() {
    let separately: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = {
        let separately = separately.clone();
        h(move |input: ListChangeRequestsInput| {
            separately.push(input.repository.clone());
            async move {
                if input.repository == "acme/docs" {
                    Err(request_failed())
                } else {
                    Ok(page(Vec::new(), false, true))
                }
            }
        })
    };
    github.list_change_requests_across = Some(ok(ProviderBatchedChangeRequestPage {
        items: vec![batched(1, "acme/web", "2026-07-03T00:00:00Z")],
        truncated: false,
    }));
    let service = make_service(
        vec![gh("p1", "web", "/a", "acme/web").build(), gh("p2", "docs", "/b", "acme/docs").build()],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    // What a repository GitHub will not search looks like: read the old way, and its failure is
    // reported against its own project.
    assert_eq!(separately.all(), vec!["acme/docs"]);
    assert_eq!(
        result.errors,
        vec![PullRequestListProjectError {
            project_id: ProjectId::new("p2"),
            project_title: "docs".into(),
            message: "acme/docs could not be read.".into(),
        }]
    );
    assert_eq!(numbers(&result), vec![1]);
}

#[tokio::test]
async fn reads_the_repositories_one_at_a_time_when_the_search_itself_fails() {
    let separately = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = listing(&separately, vec![change_request(1, "2026-07-02T00:00:00Z")], false, true);
    github.list_change_requests_across = Some(fail(request_failed()));
    let service = make_service(
        vec![gh("p1", "web", "/a", "acme/web").build(), gh("p2", "docs", "/b", "acme/docs").build()],
        vec![github],
    );

    let result = service.list(open_list()).await.unwrap();

    // One failed question about two repositories is not two unreadable repositories.
    let mut asked = separately.all();
    asked.sort();
    assert_eq!(asked, vec!["acme/docs", "acme/web"]);
    assert!(result.errors.is_empty());
    assert_eq!(result.entries.len(), 2);
}

#[tokio::test]
async fn fills_in_the_line_counts_for_the_rows_it_is_given() {
    let asked: Log<Vec<(String, i64)>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_request_stats = Some({
        let asked = asked.clone();
        h(move |input: ListChangeRequestStatsInput| {
            asked.push(input.change_requests.clone());
            async {
                Ok(vec![ProviderChangeRequestStat {
                    repository: "acme/web".into(),
                    number: 1,
                    additions: 12,
                    deletions: 3,
                }])
            }
        })
    });
    // Its listing carries the counts already, so it has nothing to be asked.
    let gitlab = FakeProvider::new(Kind::Gitlab);
    let service = make_service(
        vec![
            gh("p1", "web", "/a", "acme/web").build(),
            gh("p2", "on gitlab", "/b", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );

    let result = service
        .list_stats(PullRequestListStatsInput {
            refs: vec![
                reference("p1", "acme/web", 1),
                reference("p1", "acme/web", 2),
                reference("p2", "group/project", 3),
                // Not the repository this project's remote points at, so it is dropped.
                reference("p1", "evil/repo", 4),
            ],
        })
        .await
        .unwrap();

    assert_eq!(asked.all(), vec![vec![("acme/web".to_owned(), 1), ("acme/web".to_owned(), 2)]]);
    // Only the rows the host answered for.
    assert_eq!(
        result.stats,
        vec![PullRequestDiffStat {
            project_id: ProjectId::new("p1"),
            repository: "acme/web".into(),
            number: 1,
            additions: 12,
            deletions: 3,
        }]
    );
}

#[tokio::test]
async fn reuses_counts_across_overlapping_pages_until_expiry_explicit_invalidation_or_a_reference_changes() {
    let asked: Log<Vec<i64>> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_request_stats = Some({
        let asked = asked.clone();
        h(move |input: ListChangeRequestStatsInput| {
            asked.push(input.change_requests.iter().map(|(_, number)| *number).collect());
            async move {
                Ok(input
                    .change_requests
                    .iter()
                    .map(|(repository, number)| ProviderChangeRequestStat {
                        repository: repository.clone(),
                        number: *number,
                        additions: 12,
                        deletions: 3,
                    })
                    .collect())
            }
        })
    });
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);
    let refs = |numbers: &[i64]| PullRequestListStatsInput {
        refs: numbers.iter().map(|number| reference("p1", "acme/web", *number)).collect(),
    };

    service.list_stats(refs(&[1, 2])).await.unwrap();
    let overlapping = service.list_stats(refs(&[2, 3])).await.unwrap();
    assert_eq!(overlapping.stats.iter().map(|stat| stat.number).collect::<Vec<_>>(), vec![2, 3]);
    service.list_stats(refs(&[1, 2, 3])).await.unwrap();
    assert_eq!(asked.all(), vec![vec![1, 2], vec![3]]);

    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: Some(reference("p1", "acme/web", 2)),
                files_viewed_only: None,
            },
            false,
        )
        .await;
    service.list_stats(refs(&[1, 2, 3])).await.unwrap();
    assert_eq!(asked.all(), vec![vec![1, 2], vec![3], vec![2]]);

    service.adjust(61_000).await;
    service.list_stats(refs(&[1, 2, 3])).await.unwrap();
    assert_eq!(asked.all(), vec![vec![1, 2], vec![3], vec![2], vec![1, 2, 3]]);

    service.refresh_after_turn(&ProjectId::new("p1")).await;
    service.list_stats(refs(&[1])).await.unwrap();
    assert_eq!(asked.all(), vec![vec![1, 2], vec![3], vec![2], vec![1, 2, 3], vec![1]]);

    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: None,
                files_viewed_only: None,
            },
            false,
        )
        .await;
    service.list_stats(refs(&[1])).await.unwrap();
    assert_eq!(asked.all(), vec![vec![1, 2], vec![3], vec![2], vec![1, 2, 3], vec![1], vec![1]]);
}

#[tokio::test]
async fn keeps_the_rows_when_the_line_counts_cannot_be_read() {
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_request_stats = Some(fail(request_failed()));
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").build()], vec![github]);

    let result = service
        .list_stats(PullRequestListStatsInput {
            refs: vec![reference("p1", "acme/web", 1)],
        })
        .await
        .unwrap();

    assert!(result.stats.is_empty());
}

#[tokio::test]
async fn narrows_the_rows_of_a_host_that_ignored_the_filters_it_was_handed() {
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    // Only GitHub narrows a listing for itself; every other host answers unnarrowed.
    gitlab.list_change_requests = ok(page(
        vec![
            ProviderChangeRequest {
                is_draft: true,
                ..change_request(1, "2026-07-02T00:00:00Z")
            },
            change_request(2, "2026-07-01T00:00:00Z"),
        ],
        false,
        false,
    ));
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").provider("gitlab").build()], vec![gitlab]);

    let result = service
        .list(PullRequestListInput {
            filters: Some(PullRequestListFilters {
                draft: Some(PullRequestListFiltersDraft::Hide),
                ..no_filters()
            }),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(numbers(&result), vec![2]);
}

#[tokio::test]
async fn keeps_a_row_of_a_host_that_ignored_the_filters_if_any_name_of_a_label_group_holds() {
    let sized = |number: i64, updated_at: &str, names: &[&str]| ProviderChangeRequest {
        labels: names
            .iter()
            .map(|name| PullRequestLabel {
                name: (*name).into(),
                color: None,
            })
            .collect(),
        ..change_request(number, updated_at)
    };
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = ok(page(
        vec![
            sized(1, "2026-07-04T00:00:00Z", &["size:S", "bug"]),
            sized(2, "2026-07-03T00:00:00Z", &["size:XS", "bug"]),
            sized(3, "2026-07-02T00:00:00Z", &["size:L", "bug"]),
            sized(4, "2026-07-01T00:00:00Z", &["size:S"]),
        ],
        false,
        false,
    ));
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").provider("gitlab").build()], vec![gitlab]);

    // Either size satisfies the first group; the second group is its own question.
    let result = service
        .list(PullRequestListInput {
            filters: Some(PullRequestListFilters {
                labels: Some(vec![vec!["size:S".into(), "size:XS".into()], vec!["bug".into()]]),
                ..no_filters()
            }),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(numbers(&result), vec![1, 2]);
}

#[tokio::test]
async fn resolves_an_author_filter_of_me_to_the_viewer_before_narrowing_a_hosts_rows() {
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = ok(page(
        vec![
            change_request(1, "2026-07-02T00:00:00Z"),
            ProviderChangeRequest {
                author: Some(actor("bilal")),
                ..change_request(2, "2026-07-01T00:00:00Z")
            },
        ],
        false,
        false,
    ));
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").provider("gitlab").build()], vec![gitlab]);

    let result = service
        .list(PullRequestListInput {
            filters: Some(PullRequestListFilters {
                author: Some("me".into()),
                ..no_filters()
            }),
            ..open_list()
        })
        .await
        .unwrap();

    assert_eq!(numbers(&result), vec![2]);
}

#[tokio::test]
async fn judges_the_review_filter_only_on_a_host_that_summarises_its_reviews() {
    // GitHub answers with the field on every row: null is "nobody has decided yet".
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_requests = ok(page(
        vec![
            ProviderChangeRequest {
                review_decision: Some(None),
                ..change_request(1, "2026-07-02T00:00:00Z")
            },
            ProviderChangeRequest {
                review_decision: Some(Some(PullRequestReviewDecision::Approved)),
                ..change_request(2, "2026-07-02T00:00:00Z")
            },
        ],
        false,
        true,
    ));
    // GitLab never supplies the field, so its rows are not the filter's to judge.
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.list_change_requests = ok(page(vec![change_request(3, "2026-07-02T00:00:00Z")], false, true));
    let service = make_service(
        vec![
            gh("p1", "web", "/a", "acme/web").build(),
            gh("p2", "on gitlab", "/b", "group/project").provider("gitlab").build(),
        ],
        vec![github, gitlab],
    );
    let with_review = |review| PullRequestListInput {
        filters: Some(PullRequestListFilters {
            review: Some(review),
            ..no_filters()
        }),
        ..open_list()
    };

    let mut none = numbers(&service.list(with_review(PullRequestListFiltersReview::None)).await.unwrap());
    none.sort();
    assert_eq!(none, vec![1, 3]);

    let mut approved = numbers(&service.list(with_review(PullRequestListFiltersReview::Approved)).await.unwrap());
    approved.sort();
    assert_eq!(approved, vec![2, 3]);
}

#[tokio::test]
async fn keeps_azure_continuation_cursors_separate_for_repositories_with_the_same_name() {
    let seen = Log::default();
    let mut azure = FakeProvider::new(Kind::AzureDevops);
    azure.list_change_requests = {
        let seen = seen.clone();
        h(move |input: ListChangeRequestsInput| {
            seen.push(input.cwd.clone());
            async { Ok(page(vec![change_request(7, "2026-07-02T00:00:00Z")], true, true)) }
        })
    };
    let projects = ["org-a", "org-b"]
        .iter()
        .map(|organization| {
            gh(
                organization,
                organization,
                &format!("/{organization}"),
                &format!("{organization}/project/_git/web"),
            )
            .provider("azure-devops")
            .host("dev.azure.com")
            .build()
        })
        .collect();
    let service = make_service(projects, vec![azure]);

    let first = service.list(open_list()).await.unwrap();
    assert_eq!(first.next_cursors.len(), 2);
    let key = first.next_cursors.keys().find(|key| key.contains("org-b")).unwrap().clone();
    seen.clear();
    service
        .list(PullRequestListInput {
            cursors: cursors(&[(key.as_str(), first.next_cursors[&key].as_str())]),
            ..open_list()
        })
        .await
        .unwrap();
    assert_eq!(seen.all(), vec!["/org-b"]);
}
