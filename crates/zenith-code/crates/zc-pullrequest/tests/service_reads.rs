//! `PullRequestService.test.ts`, the reads: previews, summaries, stacks, details, diffs, routing
//! (hosted references, Forgejo authorities, Azure organisations) and routed credentials.

mod support_service;

use std::sync::Arc;

use support_service::*;
use zc_contracts::*;
use zc_pullrequest::provider::*;
use zc_pullrequest::PullRequestError;

const UPDATED: &str = "2026-07-02T00:00:00Z";

fn web() -> OrchestrationProjectShell {
    gh("p1", "web", "/a", "acme/web").build()
}

fn web_ref() -> PullRequestRef {
    reference("p1", "acme/web", 1)
}

fn invalidate_input(reference: PullRequestRef) -> PullRequestInvalidateInput {
    PullRequestInvalidateInput {
        reference: Some(reference),
        files_viewed_only: None,
    }
}

#[tokio::test]
async fn caches_narrow_previews_and_invalidates_them_after_refresh_or_mutation() {
    let reads = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_viewer = die("preview must not read the viewer");
    github.get_change_request_preview = Some({
        let reads = reads.clone();
        h(move |_| {
            reads.bump();
            async { Ok(preview_row(1, UPDATED)) }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let (first, second) = tokio::join!(service.preview(reference.clone()), service.preview(reference.clone()));
    second.unwrap();
    assert_eq!(
        first.unwrap(),
        PullRequestPreview {
            project_id: ProjectId::new("p1"),
            repository: "acme/web".into(),
            number: 1,
            title: "Change request 1".into(),
            url: "https://host/pull/1".into(),
            author: Some(actor("octocat")),
            state: PullRequestState::Open,
            is_draft: false,
            created_at: "2026-07-01T00:00:00Z".into(),
        }
    );
    assert_eq!(reads.get(), 1);
    service.invalidate(invalidate_input(reference.clone()), false).await;
    service.preview(reference.clone()).await.unwrap();
    assert_eq!(reads.get(), 2);
    service.run_action(action(&reference, PullRequestAction::Close)).await.unwrap();
    service.preview(reference.clone()).await.unwrap();
    assert_eq!(reads.get(), 3);
    service.refresh_after_turn(&ProjectId::new("p1")).await;
    service.preview(reference.clone()).await.unwrap();
    assert_eq!(reads.get(), 4);
    let error = service
        .preview(PullRequestRef {
            repository: "another/repo".into(),
            ..reference
        })
        .await
        .unwrap_err();
    assert_eq!(tag(&error), "PullRequestOperationError");
    assert_eq!(reads.get(), 4);
}

#[tokio::test]
async fn keeps_cached_previews_available_and_pauses_uncached_previews_until_quota_resets() {
    let reads = Counter::default();
    let retry_at = zc_core::time::parse_iso_millis("2099-08-13T14:00:00Z").unwrap();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_preview = Some({
        let reads = reads.clone();
        h(move |input: ChangeRequestRef| {
            let read = reads.bump();
            async move {
                if read == 2 {
                    return Err(rate_limited(
                        Kind::Github,
                        "getChangeRequestPreview",
                        "GitHub requests are paused until the rate limit resets.",
                        Some(retry_at),
                    ));
                }
                Ok(preview_row(input.number, UPDATED))
            }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();
    let numbered = |number| PullRequestRef { number, ..reference.clone() };

    service.preview(reference.clone()).await.unwrap();
    let limited = service.preview(numbered(2)).await.unwrap_err();
    assert_eq!(tag(&limited), "PullRequestOperationError");
    assert!(detail_text(&limited).contains("paused"));
    assert_eq!(service.preview(reference.clone()).await.unwrap().number, 1);
    for number in [2, 3, 4] {
        let paused = service.preview(numbered(number)).await.unwrap_err();
        assert_eq!(tag(&paused), "PullRequestOperationError");
        assert!(detail_text(&paused).contains("paused"));
    }
    assert_eq!(reads.get(), 2);
    service.set_time(retry_at).await;
    assert_eq!(service.preview(numbered(2)).await.unwrap().number, 2);
    assert_eq!(reads.get(), 3);
}

#[tokio::test]
async fn reuses_only_unexpired_detail_for_previews() {
    let preview_reads = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = ok(hosted("Description", 1));
    github.get_change_request_preview = Some({
        let preview_reads = preview_reads.clone();
        h(move |_| {
            preview_reads.bump();
            async {
                Ok(ProviderChangeRequestPreview {
                    title: "Updated title".into(),
                    state: PullRequestState::Closed,
                    ..preview_row(1, UPDATED)
                })
            }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.detail(reference.clone()).await.unwrap();
    assert_eq!(service.preview(reference.clone()).await.unwrap().title, "Change request 1");
    assert_eq!(preview_reads.get(), 0);
    service.adjust(16_000).await;
    let preview = service.preview(reference).await.unwrap();
    assert_eq!(preview.title, "Updated title");
    assert_eq!(preview.state, PullRequestState::Closed);
    assert_eq!(preview_reads.get(), 1);
}

#[tokio::test]
async fn does_not_wait_for_an_in_flight_detail_read_to_display_a_preview() {
    let detail_started = Gate::default();
    let release_detail = Gate::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let (detail_started, release_detail) = (detail_started.clone(), release_detail.clone());
        h(move |_| {
            let (detail_started, release_detail) = (detail_started.clone(), release_detail.clone());
            async move {
                detail_started.open();
                release_detail.wait().await;
                Ok(hosted("Description", 1))
            }
        })
    };
    github.get_change_request_preview = Some(ok(preview_row(1, UPDATED)));
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let detail = tokio::spawn({
        let service = service.service.clone();
        let reference = reference.clone();
        async move { service.detail(reference).await }
    });
    detail_started.wait().await;
    assert_eq!(service.preview(reference).await.unwrap().title, "Change request 1");
    release_detail.open();
    detail.await.unwrap().unwrap();
}

#[tokio::test]
async fn keeps_previews_warm_when_another_project_finishes_a_turn() {
    let reads: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_preview = Some({
        let reads = reads.clone();
        h(move |input: ChangeRequestRef| {
            reads.push(input.repository.clone());
            async { Ok(preview_row(1, UPDATED)) }
        })
    });
    let service = make_service(vec![web(), gh("p2", "docs", "/d", "acme/docs").build()], vec![github]);
    let web_reference = web_ref();
    let docs = reference("p2", "acme/docs", 1);

    service.preview(web_reference.clone()).await.unwrap();
    service.preview(docs.clone()).await.unwrap();
    service
        .preview(PullRequestRef {
            host: Some("github.com".into()),
            ..web_reference.clone()
        })
        .await
        .unwrap();
    assert_eq!(reads.all(), vec!["acme/web", "acme/docs"]);
    service.refresh_after_turn(&ProjectId::new("p1")).await;
    service.preview(web_reference).await.unwrap();
    service.preview(docs).await.unwrap();
    assert_eq!(reads.all(), vec!["acme/web", "acme/docs", "acme/web"]);
}

#[tokio::test]
async fn uses_full_detail_for_hosts_without_a_narrow_preview() {
    let reads = Counter::default();
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.get_change_request = {
        let reads = reads.clone();
        h(move |_| {
            reads.bump();
            async { Ok(hosted("Description", 1)) }
        })
    };
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").provider("gitlab").build()], vec![gitlab]);

    let preview = service.preview(web_ref()).await.unwrap();

    assert_eq!(preview.title, "Change request 1");
    assert_eq!(reads.get(), 1);
    assert!(serde_json::to_value(&preview).unwrap().get("body").is_none());
}

#[tokio::test]
async fn routing_verifies_the_current_account_on_the_requested_host_without_caching_it() {
    let viewer = Cell::new("first-account".to_owned());
    let mut github = FakeProvider::new(Kind::Github);
    github.get_routing_identity = Some({
        let viewer = viewer.clone();
        h(move |(cwd, host): (String, String)| {
            assert_eq!((cwd.as_str(), host.as_str()), ("/a", "github.example.test"));
            let viewer = viewer.get();
            async move {
                Ok(RoutingIdentity {
                    account_id: if viewer == "first-account" { "123".into() } else { "456".into() },
                    viewer,
                })
            }
        })
    });
    let service = make_service(vec![gh("p1", "web", "/a", "acme/web").host("github.example.test").build()], vec![github]);
    let reference = web_ref();

    assert_eq!(
        service.routing(reference.clone()).await.unwrap(),
        PullRequestRoutingResult {
            account_id: "123".into(),
            host: "github.example.test".into(),
            provider: Kind::Github,
            viewer: "first-account".into(),
            project_title: "web".into(),
            workspace_root: "/a".into(),
        }
    );
    viewer.set("second-account".into());
    assert_eq!(service.routing(reference.clone()).await.unwrap().viewer, "second-account");
    viewer.set(" ".into());
    let failure = service.routing(reference).await.unwrap_err();
    assert_eq!(tag(&failure), "PullRequestOperationError");
    assert_eq!(operation_of(&failure), Some("routeIdentity"));
}

#[tokio::test]
async fn refuses_a_repository_that_does_not_belong_to_the_requested_project() {
    let service = make_service(
        vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()],
        vec![FakeProvider::new(Kind::Github)],
    );

    let error = service.diff(diff_input(&reference("p1", "attacker/repo", 1))).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

fn stack(layers: Vec<ProviderChangeRequestStackLayer>) -> ProviderChangeRequestStack {
    ProviderChangeRequestStack {
        id: "9".into(),
        number: 3,
        url: "https://github.com/acme/web/stacks/3".into(),
        base: "main".into(),
        layers,
    }
}

fn layer(number: i64, head_branch: &str) -> ProviderChangeRequestStackLayer {
    ProviderChangeRequestStackLayer {
        title: None,
        is_draft: None,
        head_sha: None,
        number,
        head_branch: head_branch.into(),
        state: PullRequestState::Open,
    }
}

#[tokio::test]
async fn caches_stack_membership_separately_from_action_details() {
    let reads: Log<bool> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_stack = Some({
        let reads = reads.clone();
        h(move |input: GetChangeRequestStackInput| {
            let details = input.include_details == Some(true);
            reads.push(details);
            async move {
                let mut first = layer(7, "a");
                if details {
                    first.title = Some("First layer".into());
                    first.head_sha = Some("abc".into());
                }
                Ok(Some(stack(vec![first])))
            }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = reference("p1", "acme/web", 7);

    service.stack(reference.clone(), false).await.unwrap();
    service.stack(reference.clone(), false).await.unwrap();
    let detail = service.stack(reference.clone(), true).await.unwrap();
    service.stack(reference.clone(), true).await.unwrap();
    assert_eq!(reads.all(), vec![false, true]);
    assert_eq!(detail.unwrap().layers[0].head_sha.as_deref(), Some("abc"));
    service.invalidate(invalidate_input(reference.clone()), false).await;
    service.stack(reference.clone(), false).await.unwrap();
    service.stack(reference, true).await.unwrap();
    assert_eq!(reads.all(), vec![false, true, false, true]);
}

#[tokio::test]
async fn reads_a_host_native_stack_through_the_provider_and_null_where_it_has_none() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_stack = Some(ok(Some(stack(vec![layer(7, "a"), layer(8, "b")]))));
    let service = make_service(vec![web()], vec![github]);

    let found = service.stack(reference("p1", "acme/web", 7), true).await.unwrap().unwrap();
    assert_eq!(found.layers.iter().map(|layer| layer.number).collect::<Vec<_>>(), vec![7, 8]);

    let without_stacks = make_service(vec![web()], vec![FakeProvider::new(Kind::Github)]);
    assert_eq!(without_stacks.stack(reference("p1", "acme/web", 7), true).await.unwrap(), None);
}

#[tokio::test]
async fn routes_explicit_forgejo_http_authorities_through_ssh_checkouts_after_refinement() {
    for provider in ["forgejo", "unknown"] {
        let seen: Log<String> = Log::default();
        let viewers: Log<Option<String>> = Log::default();
        let mut forgejo = FakeProvider::new(Kind::Forgejo);
        forgejo.get_viewer = {
            let viewers = viewers.clone();
            h(move |input: ProviderHostRef| {
                viewers.push(input.host.clone());
                assert_eq!(input.host.as_deref(), Some("code.example:3000"));
                async { Ok("bilal".to_owned()) }
            })
        };
        forgejo.list_change_requests = h(|input: ListChangeRequestsInput| async move {
            assert_eq!(input.host, "code.example:3000");
            Ok(page(Vec::new(), false, true))
        });
        forgejo.get_change_request = h(|input: ChangeRequestRef| async move {
            assert_eq!(input.host, "code.example:3000");
            let mut detail = hosted("Forgejo detail", 1);
            detail.change_request.number = 42;
            Ok(detail)
        });
        forgejo.get_change_request_summary = Some({
            let seen = seen.clone();
            h(move |input: ChangeRequestRef| {
                seen.push(input.host.clone());
                async { Ok(summary_row(42, UPDATED)) }
            })
        });
        let refine: Refine = Arc::new(|_, context| {
            let requested = context.requested_host.as_deref()?;
            assert_eq!(requested, "code.example:3000");
            Some(info(Kind::Forgejo, "Forgejo", "http://code.example:3000"))
        });
        let service = make_service_with(
            vec![gh("ssh", "ssh", "/ssh", "team/repo")
                .provider(provider)
                .host("ssh.code.example")
                .remote_url("git@ssh.code.example:team/repo.git")
                .build()],
            vec![forgejo],
            Some(refine),
        );
        let reference = hosted_ref("ssh", "code.example:3000", "team/repo", 42);

        service.summary(reference.clone(), false).await.unwrap();
        assert_eq!(seen.all(), vec!["code.example:3000"]);
        let listed = service
            .list(PullRequestListInput {
                project_id: Some(ProjectId::new("ssh")),
                host: Some("code.example:3000".into()),
                ..open_list()
            })
            .await
            .unwrap();
        assert_eq!(listed.viewers.get("code.example:3000").map(String::as_str), Some("bilal"));
        assert_eq!(service.preview(reference.clone()).await.unwrap().number, 42);
        assert_eq!(service.detail(reference).await.unwrap().body, "Forgejo detail");
        assert_eq!(viewers.all(), vec![Some("code.example:3000".to_owned())]);
    }
}

#[tokio::test]
async fn rejects_a_different_forgejo_http_port_for_an_http_checkout() {
    let service = make_service(
        vec![gh("http", "http", "/http", "team/repo")
            .provider("forgejo")
            .host("code.example")
            .remote_url("http://code.example:4000/team/repo.git")
            .build()],
        vec![FakeProvider::new(Kind::Forgejo)],
    );

    let failure = service
        .summary(hosted_ref("http", "code.example:3000", "team/repo", 42), false)
        .await
        .unwrap_err();

    assert_eq!(tag(&failure), "PullRequestUnavailableError");
}

#[tokio::test]
async fn routes_a_hosted_reference_to_another_repository_through_a_project_on_that_host() {
    let seen: Log<(String, String, String)> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some({
        let seen = seen.clone();
        h(move |input: ChangeRequestRef| {
            seen.push((input.cwd.clone(), input.repository.clone(), input.host.clone()));
            async { Ok(summary_row(7, UPDATED)) }
        })
    });
    let service = make_service(vec![gh("frontend", "web", "/web", "acme/web").build()], vec![github]);

    let summary = service.summary(hosted_ref("frontend", "github.com", "acme/api", 7), false).await.unwrap();

    assert_eq!(summary.number, 7);
    assert_eq!(seen.all(), vec![("/web".to_owned(), "acme/api".to_owned(), "github.com".to_owned())]);
}

fn azure_reading(seen: &Log<String>) -> FakeProvider {
    let mut azure = FakeProvider::new(Kind::AzureDevops);
    azure.get_change_request_summary = Some({
        let seen = seen.clone();
        h(move |input: ChangeRequestRef| {
            seen.push(format!("read {} {}", input.cwd, input.repository));
            async { Ok(summary_row(7, UPDATED)) }
        })
    });
    azure.run_action = {
        let seen = seen.clone();
        h(move |input: RunActionInput| {
            seen.push(format!("write {} {}", input.change_request.cwd, input.change_request.repository));
            async { Ok(()) }
        })
    };
    azure
}

fn azure_project(id: &str, repository: &str) -> OrchestrationProjectShell {
    gh(id, id, &format!("/{id}"), repository).provider("azure-devops").host("dev.azure.com").build()
}

#[tokio::test]
async fn routes_azure_reads_and_writes_through_the_requested_organizations_checkout() {
    let seen = Log::default();
    let service = make_service(
        vec![
            azure_project("org-a", "org-a/project/_git/web"),
            azure_project("org-b", "org-b/project/_git/web"),
        ],
        vec![azure_reading(&seen)],
    );
    let reference = hosted_ref("org-a", "dev.azure.com", "org-b/project/_git/web", 7);

    service.summary(reference.clone(), false).await.unwrap();
    service.run_action(action(&reference, PullRequestAction::Merge)).await.unwrap();

    assert_eq!(seen.all(), vec!["read /org-b web", "write /org-b web", "read /org-b web"]);
}

#[tokio::test]
async fn routes_azure_url_reads_and_writes_through_ssh_legacy_and_visualstudio_checkouts() {
    for (host, repository, remote_url) in [
        ("ssh.dev.azure.com", "v3/org-b/project/web", "git@ssh.dev.azure.com:v3/org-b/project/web"),
        (
            "vs-ssh.visualstudio.com",
            "v3/org-b/project/web",
            "git@vs-ssh.visualstudio.com:v3/org-b/project/web",
        ),
        (
            "org-b.visualstudio.com",
            "DefaultCollection/project/_git/web",
            "https://org-b.visualstudio.com/DefaultCollection/project/_git/web",
        ),
    ] {
        let seen = Log::default();
        let target = project("target", "target", "/target", Some(repository))
            .provider("azure-devops")
            .host(host)
            .remote_url(remote_url)
            .build();
        let mut projects: Vec<OrchestrationProjectShell> = ["org-a/project/_git/web", "org-b/other-project/_git/web"]
            .iter()
            .map(|repository| {
                gh(repository, repository, &format!("/{repository}"), repository)
                    .provider("azure-devops")
                    .host("dev.azure.com")
                    .build()
            })
            .collect();
        projects.push(target);
        let service = make_service(projects, vec![azure_reading(&seen)]);
        let reference = hosted_ref("org-a/project/_git/web", "dev.azure.com", "org-b/project/_git/web", 7);

        service.summary(reference.clone(), false).await.unwrap();
        service.run_action(action(&reference, PullRequestAction::Merge)).await.unwrap();

        assert_eq!(
            seen.all(),
            vec!["read /target web", "write /target web", "read /target web"],
            "through a {host} checkout"
        );
    }
}

#[tokio::test]
async fn refuses_azure_cross_organization_reads_and_writes_without_its_checkout() {
    let mut azure = FakeProvider::new(Kind::AzureDevops);
    azure.get_change_request_summary = Some(die("must not read the wrong organization"));
    azure.run_action = die("must not modify the wrong organization");
    let service = make_service(vec![azure_project("org-a", "org-a/project/_git/web")], vec![azure]);
    let reference = hosted_ref("org-a", "dev.azure.com", "org-b/project/_git/web", 7);

    let read_error = service.summary(reference.clone(), false).await.unwrap_err();
    let write_error = service.run_action(action(&reference, PullRequestAction::Close)).await.unwrap_err();

    assert_eq!(tag(&read_error), "PullRequestUnavailableError");
    assert_eq!(tag(&write_error), "PullRequestUnavailableError");
}

#[tokio::test]
async fn refuses_a_hosted_reference_when_nothing_is_checked_out_from_that_host() {
    let service = make_service(vec![gh("frontend", "web", "/web", "acme/web").build()], vec![FakeProvider::new(Kind::Github)]);

    let error = service.summary(hosted_ref("frontend", "gitlab.com", "acme/api", 7), false).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestUnavailableError");
}

#[tokio::test]
async fn refuses_a_diff_on_a_host_that_cannot_produce_one() {
    let azure = FakeProvider {
        get_diff: die("must not be called"),
        ..FakeProvider::new(Kind::AzureDevops)
    }
    .with_capabilities(PullRequestCapabilities {
        diff: false,
        ..capabilities(&[PullRequestAction::Merge, PullRequestAction::Close], &[PullRequestMergeMethod::Merge])
    });
    let service = make_service(vec![gh("p1", "on azure", "/a", "org/project").provider("azure-devops").build()], vec![azure]);

    let error = service.diff(diff_input(&reference("p1", "org/project", 1))).await.unwrap_err();

    assert_eq!(tag(&error), "PullRequestOperationError");
}

#[tokio::test]
async fn keeps_unrelated_prs_warm_after_a_mutation_explicit_refresh_and_project_turn() {
    let calls: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let calls = calls.clone();
        h(move |input: ChangeRequestRef| {
            calls.push(format!("{}/{}", input.repository, input.number));
            async move {
                let mut detail = hosted("body", 1);
                detail.change_request.number = input.number;
                Ok(detail)
            }
        })
    };
    let service = make_service(vec![web(), gh("p2", "docs", "/b", "acme/docs").build()], vec![github]);
    let refs = [reference("p1", "acme/web", 1), reference("p1", "acme/web", 2), reference("p2", "acme/docs", 3)];
    let read_all = || async {
        for reference in &refs {
            service.summary(strict(reference), true).await.unwrap();
        }
    };

    read_all().await;
    service
        .invalidate(
            invalidate_input(PullRequestRef {
                host: Some("github.com".into()),
                ..refs[0].clone()
            }),
            false,
        )
        .await;
    read_all().await;
    assert_eq!(calls.all(), vec!["acme/web/1", "acme/web/2", "acme/docs/3", "acme/web/1"]);
    service.comment(comment_input(&refs[0], "hello")).await.unwrap();
    read_all().await;
    assert_eq!(calls.all()[4..], ["acme/web/1".to_owned()]);
    service.refresh_after_turn(&ProjectId::new("p1")).await;
    read_all().await;
    assert_eq!(calls.all()[5..], ["acme/web/1".to_owned(), "acme/web/2".to_owned()]);
}

#[tokio::test]
async fn keeps_matching_pr_numbers_on_different_hosts_separate_and_refreshes_the_serving_project() {
    let hosts: Log<String> = Log::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let hosts = hosts.clone();
        h(move |input: ChangeRequestRef| {
            hosts.push(input.host.clone());
            async { Ok(hosted("body", 1)) }
        })
    };
    let service = make_service(
        vec![
            gh("p1", "public", "/a", "acme/web").build(),
            gh("p2", "enterprise", "/b", "acme/web").host("enterprise.test").build(),
        ],
        vec![github],
    );
    let own = web_ref();
    let other = PullRequestRef {
        host: Some("enterprise.test".into()),
        ..own.clone()
    };
    let read_both = || async {
        service.summary(own.clone(), true).await.unwrap();
        service.summary(other.clone(), true).await.unwrap();
    };

    read_both().await;
    service
        .invalidate(
            invalidate_input(PullRequestRef {
                host: Some("github.com".into()),
                ..own.clone()
            }),
            false,
        )
        .await;
    read_both().await;
    assert_eq!(hosts.all(), vec!["github.com", "enterprise.test", "github.com"]);
    service.invalidate(invalidate_input(own.clone()), false).await;
    read_both().await;
    assert_eq!(hosts.all()[3..], ["github.com".to_owned()]);
    service.refresh_after_turn(&ProjectId::new("p2")).await;
    read_both().await;
    assert_eq!(hosts.all()[4..], ["enterprise.test".to_owned()]);
}

#[tokio::test]
async fn does_not_revive_old_summaries_when_project_epochs_are_evicted() {
    let title = Cell::new("old".to_owned());
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let title = title.clone();
        h(move |_| {
            let mut detail = hosted("body", 1);
            detail.change_request.title = title.get();
            async move { Ok(detail) }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    assert_eq!(service.summary(reference.clone(), true).await.unwrap().title, "old");
    title.set("new".into());
    service.refresh_after_turn(&ProjectId::new("p1")).await;
    assert_eq!(service.summary(reference.clone(), true).await.unwrap().title, "new");
    for index in 0..2048 {
        service.refresh_after_turn(&ProjectId::new(format!("project-{index}"))).await;
    }
    assert_eq!(service.summary(reference, true).await.unwrap().title, "new");
}

#[tokio::test]
async fn reads_the_fresh_diff_when_detail_or_summary_discovers_a_changed_revision() {
    let summary_started = Gate::default();
    let release_summary = Gate::default();
    let revision = Cell::new("2026-07-02T00:00:00Z".to_owned());
    let patch = Cell::new("old patch".to_owned());
    let diff_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let revision = revision.clone();
        h(move |_| {
            let mut detail = hosted("body", 1);
            detail.change_request.updated_at = revision.get();
            async move { Ok(detail) }
        })
    };
    github.get_change_request_summary = Some({
        let (revision, summary_started, release_summary) = (revision.clone(), summary_started.clone(), release_summary.clone());
        h(move |_| {
            let result = summary_row(1, &revision.get());
            let (summary_started, release_summary) = (summary_started.clone(), release_summary.clone());
            async move {
                summary_started.open();
                release_summary.wait().await;
                Ok(result)
            }
        })
    });
    github.get_diff = {
        let (patch, diff_calls) = (patch.clone(), diff_calls.clone());
        h(move |_| {
            diff_calls.bump();
            let patch = patch.get();
            async move { Ok(diff_slice(&patch, None)) }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let cold_summary = tokio::spawn({
        let service = service.service.clone();
        let reference = reference.clone();
        async move { service.summary(reference, true).await }
    });
    summary_started.wait().await;
    service.detail(reference.clone()).await.unwrap();
    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, "old patch");
    revision.set("2026-07-02T00:01:00Z".into());
    patch.set("new patch".into());
    service.adjust(16_000).await;
    service.detail(reference.clone()).await.unwrap();
    settle().await;
    assert_eq!(service.detail(reference.clone()).await.unwrap().updated_at, revision.get());
    release_summary.open();
    cold_summary.await.unwrap().unwrap();
    service.summary(reference.clone(), false).await.unwrap();
    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, "new patch");
    assert_eq!(diff_calls.get(), 2);

    revision.set("2026-07-02T00:02:00Z".into());
    patch.set("summary-discovered patch".into());
    service.adjust(61_000).await;
    service.summary(reference.clone(), false).await.unwrap();
    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, "summary-discovered patch");
    assert_eq!(diff_calls.get(), 3);
}

#[tokio::test]
async fn serves_core_detail_without_waiting_for_activity_and_shares_activity_between_clients() {
    let core_calls = Counter::default();
    let activity_calls = Counter::default();
    let stats_calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.list_change_request_stats = Some({
        let stats_calls = stats_calls.clone();
        h(move |_| {
            stats_calls.bump();
            async { Ok(Vec::new()) }
        })
    });
    github.get_change_request = {
        let core_calls = core_calls.clone();
        h(move |_| {
            core_calls.bump();
            async { Ok(detail_of(change_request(1, UPDATED), "Ready before the conversation")) }
        })
    };
    github.get_change_request_activity = {
        let activity_calls = activity_calls.clone();
        h(move |_| {
            activity_calls.bump();
            async {
                tokio::task::yield_now().await;
                Ok(empty_activity())
            }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let core = service.detail(reference.clone()).await.unwrap();
    assert_eq!(core.body, "Ready before the conversation");
    assert_eq!(core_calls.get(), 1);
    assert_eq!(activity_calls.get(), 0);

    let counts = service.list_stats(PullRequestListStatsInput { refs: vec![reference.clone()] }).await.unwrap();
    assert_eq!(stats_calls.get(), 0);
    assert_eq!(
        counts.stats,
        vec![PullRequestDiffStat {
            project_id: ProjectId::new("p1"),
            repository: "acme/web".into(),
            number: 1,
            additions: core.additions,
            deletions: core.deletions,
        }]
    );

    let (first, second) = tokio::join!(service.activity(reference.clone()), service.activity(reference.clone()));
    first.unwrap();
    second.unwrap();
    assert_eq!(activity_calls.get(), 1);

    service.invalidate(invalidate_input(reference.clone()), false).await;
    service.activity(reference).await.unwrap();
    assert_eq!(activity_calls.get(), 2);
}

#[tokio::test]
async fn shares_linked_summaries_and_reuses_them_for_display_without_asking_the_host_again() {
    let calls = Counter::default();
    let failing = Cell::new(false);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some({
        let (calls, failing) = (calls.clone(), failing.clone());
        h(move |_| {
            calls.bump();
            let should_fail = failing.get();
            async move {
                tokio::task::yield_now().await;
                if should_fail {
                    Err(failed(Kind::Github, "getChangeRequestSummary", "HTTP 504"))
                } else {
                    Ok(summary_row(1, UPDATED))
                }
            }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let (first, second) = tokio::join!(service.summary(reference.clone(), false), service.summary(reference.clone(), false));
    first.unwrap();
    second.unwrap();
    assert_eq!(calls.get(), 1);

    service.adjust(61_000).await;
    failing.set(true);
    let strict_error = service.summary(strict(&reference), true).await.unwrap_err();
    assert_eq!(tag(&strict_error), "PullRequestOperationError");

    let stale = service.summary(reference.clone(), true).await.unwrap();
    assert_eq!(stale.updated_at, UPDATED);
    // Display reads keep the last title and state rather than asking the host again.
    assert_eq!(calls.get(), 2);

    service.invalidate(invalidate_input(reference.clone()), false).await;
    let invalidated = service.summary(reference, true).await.unwrap_err();
    assert_eq!(tag(&invalidated), "PullRequestOperationError");
}

/// A provider whose summary, detail, diff, preview and host-kept viewed files all go through one
/// read that can be made to fail.
fn routed_reads(read: Arc<dyn Fn() -> Result<(), PullRequestProviderError> + Send + Sync>) -> FakeProvider {
    let mut github = FakeProvider::new(Kind::Github).with_capabilities(PullRequestCapabilities {
        viewed_files: Some(PullRequestViewedFilesStore::Host),
        ..full_capabilities()
    });
    github.get_files_viewed = Some({
        let read = read.clone();
        h(move |_| {
            let result = read();
            async move {
                result.map(|()| ProviderFilesViewed {
                    files: vec![PullRequestFileViewed {
                        path: "private.ts".into(),
                        state: PullRequestFileViewedState::Viewed,
                    }],
                    truncated: false,
                })
            }
        })
    });
    github.get_change_request_summary = Some({
        let read = read.clone();
        h(move |_| {
            let result = read();
            async move { result.map(|()| summary_row(1, UPDATED)) }
        })
    });
    github.get_change_request = {
        let read = read.clone();
        h(move |_| {
            let result = read();
            async move { result.map(|()| hosted("account A content", 1)) }
        })
    };
    github.get_diff = {
        let read = read.clone();
        h(move |_| {
            let result = read();
            async move { result.map(|()| diff_slice("private patch", None)) }
        })
    };
    github.get_change_request_preview = Some({
        let read = read.clone();
        h(move |_| {
            let result = read();
            async move { result.map(|()| preview_row(1, UPDATED)) }
        })
    });
    github
}

async fn read_operation(service: &zc_pullrequest::PullRequestService, operation: &str, input: PullRequestRef) -> Result<(), PullRequestError> {
    match operation {
        "summary" => service.summary(input, true).await.map(drop),
        "detail" => service.detail(input).await.map(drop),
        "diff" => service.diff(diff_input(&input)).await.map(drop),
        "preview" => service.preview(input).await.map(drop),
        "filesViewed" => service.files_viewed(input).await.map(drop),
        other => unreachable!("{other}"),
    }
}

#[tokio::test]
async fn keeps_routed_reads_separate_when_the_github_account_changes() {
    for operation in ["summary", "detail", "diff", "filesViewed"] {
        let failing = Cell::new(false);
        let calls = Counter::default();
        let read: Arc<dyn Fn() -> Result<(), PullRequestProviderError> + Send + Sync> = {
            let (failing, calls) = (failing.clone(), calls.clone());
            Arc::new(move || {
                calls.bump();
                if failing.get() {
                    Err(request_failed())
                } else {
                    Ok(())
                }
            })
        };
        let service = make_service(vec![web()], vec![routed_reads(read)]);
        let reference = web_ref();
        read_operation(
            &service,
            operation,
            PullRequestRef {
                expected_account_id: Some("101".into()),
                ..reference.clone()
            },
        )
        .await
        .unwrap();
        failing.set(true);

        for allow_stale in [false, true] {
            let error = read_operation(
                &service,
                operation,
                PullRequestRef {
                    expected_account_id: Some("202".into()),
                    allow_stale: Some(allow_stale),
                    ..reference.clone()
                },
            )
            .await
            .unwrap_err();
            assert_eq!(tag(&error), "PullRequestOperationError", "{operation}");
        }
        assert_eq!(calls.get(), 3, "{operation}");
    }
}

#[tokio::test]
async fn isolates_routed_caches_for_two_credentials_belonging_to_the_same_account() {
    for operation in ["summary", "detail", "diff", "preview", "filesViewed"] {
        let credential = Cell::new("broad".to_owned());
        let calls = Counter::default();
        let read: Arc<dyn Fn() -> Result<(), PullRequestProviderError> + Send + Sync> = {
            let (credential, calls) = (credential.clone(), calls.clone());
            Arc::new(move || {
                calls.bump();
                if credential.get() == "broad" {
                    Ok(())
                } else {
                    Err(request_failed())
                }
            })
        };
        let mut github = routed_reads(read);
        github.verified_credential = Some({
            let credential = credential.clone();
            h(move |_| {
                let fingerprint = credential.get();
                async move {
                    Ok(VerifiedCredential {
                        identity: VerifiedIdentity {
                            account_id: "101".into(),
                            viewer: "octocat".into(),
                            credential_fingerprint: fingerprint,
                        },
                        scope: Arc::new(PassthroughScope),
                    })
                }
            })
        });
        let service = make_service(vec![web()], vec![github]);
        let reference = PullRequestRef {
            host: Some("github.com".into()),
            expected_account_id: Some("101".into()),
            ..web_ref()
        };
        service
            .with_routing_credential(&reference, read_operation(&service, operation, reference.clone()))
            .await
            .unwrap();
        credential.set("restricted".into());
        for allow_stale in [false, true] {
            let input = PullRequestRef {
                allow_stale: Some(allow_stale),
                ..reference.clone()
            };
            let error = service
                .with_routing_credential(&reference, read_operation(&service, operation, input))
                .await
                .unwrap_err();
            assert_eq!(tag(&error), "PullRequestOperationError", "{operation}");
        }
        assert_eq!(calls.get(), 3, "{operation}");
    }
}

#[tokio::test]
async fn rejects_mismatched_routing_credentials_before_use_and_preserves_action_errors() {
    let operations = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_routing_identity = Some(ok(RoutingIdentity {
        account_id: "101".into(),
        viewer: "octocat".into(),
    }));
    github.verified_credential = Some(h(|_| async {
        Ok(VerifiedCredential {
            identity: VerifiedIdentity {
                account_id: "101".into(),
                viewer: "octocat".into(),
                credential_fingerprint: "credential-a".into(),
            },
            scope: Arc::new(PassthroughScope),
        })
    }));
    let service = make_service(vec![web()], vec![github]);
    let reference = PullRequestRef {
        host: Some("github.com".into()),
        expected_account_id: Some("202".into()),
        ..web_ref()
    };
    let operation = || {
        let operations = operations.clone();
        async move {
            operations.bump();
            Err::<(), _>(PullRequestError::operation("runAction", "ambiguous"))
        }
    };

    let rejected = service.with_routing_credential(&reference, operation()).await.unwrap_err();
    assert_eq!(tag(&rejected), "PullRequestOperationError");
    assert_eq!(operation_of(&rejected), Some("routeIdentity"));
    assert_eq!(operations.get(), 0);
    let action_error = service
        .with_routing_credential(
            &PullRequestRef {
                expected_account_id: Some("101".into()),
                ..reference.clone()
            },
            operation(),
        )
        .await
        .unwrap_err();
    assert_eq!(action_error.to_wire(), PullRequestError::operation("runAction", "ambiguous").to_wire());
    assert_eq!(operations.get(), 1);
    assert_eq!(
        service
            .routing_identity(PullRequestRoutingIdentityInput { host: "github.com".into() })
            .await
            .unwrap(),
        PullRequestRoutingIdentityResult {
            account_id: "101".into(),
            host: "github.com".into(),
            provider: LitGithub,
            viewer: "octocat".into(),
        }
    );
}

#[tokio::test]
async fn answers_a_known_pull_request_immediately_while_the_host_refreshes() {
    let gate = Gate::default();
    let calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let (gate, calls) = (gate.clone(), calls.clone());
        h(move |_| {
            let call = calls.bump();
            let gate = gate.clone();
            async move {
                if call > 1 {
                    gate.wait().await;
                }
                Ok(hosted("cached body", 4))
            }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    let first = service.detail(reference.clone()).await.unwrap();
    assert_eq!((first.body.as_str(), first.additions), ("cached body", 4));

    service.adjust(16_000).await;
    let second = service.detail(reference).await.unwrap();
    assert_eq!((second.body.as_str(), second.additions), ("cached body", 4));
    settle().await;
    assert_eq!(calls.get(), 2);
}

#[tokio::test]
async fn does_not_ask_the_host_again_for_a_linked_summary_it_already_holds() {
    let calls = Counter::default();
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some({
        let calls = calls.clone();
        h(move |_| {
            calls.bump();
            async { Ok(summary_row(1, UPDATED)) }
        })
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    assert_eq!(service.summary(reference.clone(), true).await.unwrap().title, "Change request 1");
    service.adjust(61_000).await;
    assert_eq!(service.summary(reference, true).await.unwrap().title, "Change request 1");
    assert_eq!(calls.get(), 1);
}

#[tokio::test]
async fn opening_detail_preserves_enriched_linked_summaries_and_updates_draft_and_diff_fields() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some(ok(ProviderChangeRequestSummary {
        is_draft: Some(true),
        review_decision: Some(Some(PullRequestReviewDecision::Approved)),
        checks_state: Some(Some(PullRequestChecksState::Passing)),
        ..summary_row(1, UPDATED)
    }));
    github.get_change_request = ok({
        let mut detail = hosted("body", 14);
        detail.change_request.deletions = 3;
        detail.changed_files = 5;
        detail.change_request.mergeability = PullRequestMergeability::Conflicting;
        detail
    });
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.summary(reference.clone(), true).await.unwrap();
    let detail = service.detail(reference.clone()).await.unwrap();
    let summary = service.summary(reference, true).await.unwrap();
    assert_eq!(summary.is_draft, Some(false));
    assert_eq!(summary.author, Some(detail.author));
    assert_eq!(summary.additions, Some(14));
    assert_eq!(summary.deletions, Some(3));
    assert_eq!(summary.changed_files, Some(5));
    assert_eq!(summary.mergeability, Some(PullRequestMergeability::Conflicting));
    assert_eq!(summary.review_decision, Some(Some(PullRequestReviewDecision::Approved)));
    assert_eq!(summary.checks_state, Some(Some(PullRequestChecksState::Passing)));
}

#[tokio::test]
async fn reuses_an_observed_merged_state_for_strict_settlement_reads() {
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = ok({
        let mut detail = hosted("merged body", 4);
        detail.change_request.state = PullRequestState::Merged;
        detail.change_request.updated_at = "2026-07-03T00:00:00Z".into();
        detail
    });
    github.get_change_request_summary = Some(die("strict merged state must not refresh"));
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.detail(reference.clone()).await.unwrap();

    let summary = service.summary(reference, false).await.unwrap();
    assert_eq!(summary.state, PullRequestState::Merged);
    assert_eq!(summary.updated_at, "2026-07-03T00:00:00Z");
}

fn summary_titled(title: &Cell<String>, state: &Cell<PullRequestState>) -> Handler<ChangeRequestRef, ProviderChangeRequestSummary> {
    let (title, state) = (title.clone(), state.clone());
    h(move |_| {
        let summary = ProviderChangeRequestSummary {
            title: title.get(),
            state: state.get(),
            ..summary_row(1, UPDATED)
        };
        async move { Ok(summary) }
    })
}

#[tokio::test]
async fn does_not_let_a_stale_detail_reopen_overwrite_a_fresher_linked_summary() {
    let gate = Gate::default();
    let detail_calls = Counter::default();
    let summary_title = Cell::new("old title".to_owned());
    let summary_state = Cell::new(PullRequestState::Open);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let (gate, detail_calls) = (gate.clone(), detail_calls.clone());
        h(move |_| {
            let call = detail_calls.bump();
            let gate = gate.clone();
            async move {
                if call > 1 {
                    gate.wait().await;
                }
                Ok(hosted("old body", 4))
            }
        })
    };
    github.get_change_request_summary = Some(summary_titled(&summary_title, &summary_state));
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    assert_eq!(service.detail(reference.clone()).await.unwrap().title, "Change request 1");

    summary_title.set("merged title".into());
    summary_state.set(PullRequestState::Merged);
    service.adjust(61_000).await;
    let settled = service.summary(reference.clone(), false).await.unwrap();
    assert_eq!((settled.title.as_str(), settled.state), ("merged title", PullRequestState::Merged));

    service.adjust(16_000).await;
    assert_eq!(service.detail(reference.clone()).await.unwrap().title, "Change request 1");
    settle().await;

    let display = service.summary(reference.clone(), true).await.unwrap();
    assert_eq!((display.title.as_str(), display.state), ("merged title", PullRequestState::Merged));
    assert_eq!(detail_calls.get(), 2);

    summary_title.set("updated after merge".into());
    service.adjust(61_000).await;
    assert_eq!(service.summary(reference.clone(), true).await.unwrap().title, "merged title");
    let refreshed = service.summary(strict(&reference), true).await.unwrap();
    assert_eq!((refreshed.title.as_str(), refreshed.state), ("updated after merge", PullRequestState::Merged));
}

#[tokio::test]
async fn does_not_let_a_still_cached_detail_overwrite_a_fresher_linked_summary() {
    let summary_title = Cell::new("old title".to_owned());
    let summary_state = Cell::new(PullRequestState::Open);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = ok(hosted("old body", 4));
    github.get_change_request_summary = Some(summary_titled(&summary_title, &summary_state));
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    assert_eq!(service.detail(reference.clone()).await.unwrap().title, "Change request 1");

    summary_title.set("merged title".into());
    summary_state.set(PullRequestState::Merged);
    assert_eq!(service.summary(reference.clone(), false).await.unwrap().state, PullRequestState::Merged);

    assert_eq!(service.detail(reference.clone()).await.unwrap().title, "Change request 1");
    settle().await;

    let display = service.summary(reference, true).await.unwrap();
    assert_eq!((display.title.as_str(), display.state), ("merged title", PullRequestState::Merged));
}

#[tokio::test]
async fn keeps_recent_detail_on_a_transient_refresh_failure_but_not_after_invalidation() {
    let failing = Cell::new(false);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request = {
        let failing = failing.clone();
        h(move |_| {
            let fail = failing.get();
            async move {
                if fail {
                    Err(failed(Kind::Github, "getChangeRequest", "spawn gh EAGAIN"))
                } else {
                    Ok(detail_of(change_request(1, UPDATED), "last good body"))
                }
            }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    service.detail(reference.clone()).await.unwrap();
    service.adjust(16_000).await;
    failing.set(true);
    let strict_error = service.detail(strict(&reference)).await.unwrap_err();
    assert_eq!(tag(&strict_error), "PullRequestOperationError");
    assert_eq!(service.detail(reference.clone()).await.unwrap().body, "last good body");

    service.invalidate(invalidate_input(reference.clone()), false).await;
    let invalidated = service.detail(reference).await.unwrap_err();
    assert_eq!(tag(&invalidated), "PullRequestOperationError");
}

#[tokio::test]
async fn carries_an_armed_auto_merge_through_to_the_detail_and_silence_as_silence() {
    async fn detail_with(auto_merge_enabled: Option<bool>) -> PullRequestDetail {
        let mut github = FakeProvider::new(Kind::Github);
        github.get_change_request = ok(ProviderChangeRequestDetail {
            auto_merge_enabled,
            changed_files: 0,
            ..detail_of(change_request(1, UPDATED), "")
        });
        let service = make_service(vec![web()], vec![github]);
        service.detail(web_ref()).await.unwrap()
    }

    assert_eq!(detail_with(Some(true)).await.auto_merge_enabled, Some(true));
    assert_eq!(detail_with(Some(false)).await.auto_merge_enabled, Some(false));
    // A host that says nothing leaves the field absent rather than claiming the merge is unarmed.
    assert_eq!(detail_with(None).await.auto_merge_enabled, None);
}

#[tokio::test]
async fn names_the_signed_in_account_in_the_detail_and_says_nothing_where_the_host_cannot() {
    let readable = || {
        let mut github = FakeProvider::new(Kind::Github);
        github.get_change_request = ok(ProviderChangeRequestDetail {
            changed_files: 0,
            ..detail_of(change_request(1, UPDATED), "")
        });
        github
    };
    let named = make_service(vec![web()], vec![readable()]).detail(web_ref()).await.unwrap();
    let unnamed = make_service(
        vec![web()],
        vec![FakeProvider {
            get_viewer: fail(unusable(Kind::Github, ProviderFailureReason::Unauthenticated)),
            ..readable()
        }],
    )
    .detail(web_ref())
    .await
    .unwrap();

    assert_eq!(named.viewer.as_deref(), Some("bilal"));
    assert_eq!(unnamed.viewer, None);
}

#[tokio::test]
async fn keeps_the_diff_cached_across_a_file_being_ticked_off() {
    let diff_reads = Counter::default();
    let viewed_reads = Counter::default();
    let state = Cell::new(PullRequestFileViewedState::Viewed);
    let mut github = FakeProvider::new(Kind::Github).with_capabilities(PullRequestCapabilities {
        viewed_files: Some(PullRequestViewedFilesStore::Host),
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    });
    github.get_diff = {
        let diff_reads = diff_reads.clone();
        h(move |_| {
            diff_reads.bump();
            async { Ok(diff_slice("@@", None)) }
        })
    };
    github.get_files_viewed = Some({
        let (viewed_reads, state) = (viewed_reads.clone(), state.clone());
        h(move |_| {
            viewed_reads.bump();
            let state = state.get();
            async move {
                Ok(ProviderFilesViewed {
                    files: vec![PullRequestFileViewed {
                        path: "src/a.ts".into(),
                        state,
                    }],
                    truncated: false,
                })
            }
        })
    });
    github.set_files_viewed = Some(ok(()));
    let service = make_service(vec![gh("p1", "t3code", "/a", "pingdotgg/t3code").build()], vec![github]);
    let reference = reference("p1", "pingdotgg/t3code", 1);

    service.diff(diff_input(&reference)).await.unwrap();
    service.files_viewed(reference.clone()).await.unwrap();
    service.set_files_viewed(set_files(&reference, &[("src/a.ts", false)])).await.unwrap();
    service.diff(diff_input(&reference)).await.unwrap();
    service.files_viewed(reference.clone()).await.unwrap();
    settle().await;

    // The press forgets only the reader's own ticks; cached diffs survive it.
    assert_eq!(diff_reads.get(), 1);
    assert_eq!(viewed_reads.get(), 2);

    state.set(PullRequestFileViewedState::Dismissed);
    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: Some(reference.clone()),
                files_viewed_only: Some(true),
            },
            false,
        )
        .await;
    service.diff(diff_input(&reference)).await.unwrap();
    assert_eq!(
        service.files_viewed(reference).await.unwrap().files,
        vec![PullRequestFileViewed {
            path: "src/a.ts".into(),
            state: PullRequestFileViewedState::Dismissed,
        }]
    );
    settle().await;
    assert_eq!(diff_reads.get(), 1);
    assert_eq!(viewed_reads.get(), 3);
}

#[tokio::test]
async fn returns_large_diff_slices_intact_without_retaining_them_in_either_cache() {
    let reads = Counter::default();
    let patch = "\u{1f4bb}".repeat(140_000);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_diff = {
        let (reads, patch) = (reads.clone(), patch.clone());
        h(move |_| {
            reads.bump();
            let patch = patch.clone();
            async move { Ok(diff_slice(&patch, Some("2"))) }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();
    let expected = PullRequestDiffResult {
        patch: patch.clone(),
        truncated: false,
        next_cursor: Some("2".into()),
        omitted_file_stats: None,
    };
    for input in [
        diff_input(&reference),
        PullRequestDiffInput {
            cursor: Some("2".into()),
            ..diff_input(&reference)
        },
        PullRequestDiffInput {
            commit: Some("a".repeat(40)),
            ..diff_input(&reference)
        },
    ] {
        let before = reads.get();
        assert_eq!(service.diff(input.clone()).await.unwrap(), expected);
        assert_eq!(service.diff(input).await.unwrap(), expected);
        assert_eq!(reads.get(), before + 2);
    }
}

#[tokio::test]
async fn caches_a_small_replacement_after_releasing_a_large_diff() {
    let reads = Counter::default();
    let large_patch = "x".repeat(300_000);
    let mut github = FakeProvider::new(Kind::Github);
    github.get_diff = {
        let (reads, large_patch) = (reads.clone(), large_patch.clone());
        h(move |_| {
            let read = reads.bump();
            let patch = if read == 1 { large_patch.clone() } else { "@@ small replacement".to_owned() };
            async move { Ok(diff_slice(&patch, None)) }
        })
    };
    let service = make_service(vec![web()], vec![github]);
    let reference = web_ref();

    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, large_patch);
    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, "@@ small replacement");
    assert_eq!(service.diff(diff_input(&reference)).await.unwrap().patch, "@@ small replacement");
    settle().await;
    assert_eq!(reads.get(), 2);
}

#[tokio::test]
async fn answers_the_reactor_port_from_the_same_reads_in_the_wire_shapes() {
    use zc_ports::PullRequests;
    let mut github = FakeProvider::new(Kind::Github);
    github.get_change_request_summary = Some(ok(summary_row(1, UPDATED)));
    let service = make_service(vec![web()], vec![github]);
    let port: &dyn PullRequests = &service.service;
    let wire = |repository: &str| zc_ports::contracts::PullRequestRef(serde_json::json!({"projectId": "p1", "repository": repository, "number": 1}));

    let summary = port.summary(wire("acme/web"), true).await.unwrap();
    assert_eq!(summary.0["title"], "Change request 1");
    assert_eq!(summary.0["closedAt"], serde_json::Value::Null);

    let error = port.summary(wire("acme/other"), true).await.unwrap_err();
    assert_eq!(error.tag, "PullRequestOperationError");
    assert_eq!(error.fields["operation"], "resolveRepository");
    assert_eq!(
        error.message,
        "Pull request operation resolveRepository failed: The change request does not belong to the selected project."
    );
}
