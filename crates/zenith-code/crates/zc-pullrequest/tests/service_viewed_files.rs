//! `PullRequestService.test.ts`, the viewed files: marks this environment keeps for a host that
//! keeps none (`pull_request_files_viewed` in a temp database), what the head has of the marked
//! files, the order of presses, and who the marks belong to.

#![allow(clippy::result_large_err, clippy::too_many_arguments)]

mod support_forgejo;
mod support_service;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use support_service::*;
use zc_contracts::*;
use zc_pullrequest::provider::*;
use zc_pullrequest::viewed_files::{FILE_REVISIONS_CACHE_CAPACITY, MAX_FILE_REVISION_PATHS};

/// What the head has: an insertion-ordered map of path to revision.
#[derive(Clone, Default)]
struct Head(Arc<Mutex<Vec<(String, String)>>>);

impl Head {
    fn of(entries: &[(&str, &str)]) -> Self {
        Self(Arc::new(Mutex::new(
            entries.iter().map(|(path, revision)| ((*path).to_owned(), (*revision).to_owned())).collect(),
        )))
    }

    fn set(&self, path: &str, revision: &str) {
        let mut entries = self.0.lock().unwrap();
        match entries.iter_mut().find(|(held, _)| held == path) {
            Some((_, held)) => *held = revision.to_owned(),
            None => entries.push((path.to_owned(), revision.to_owned())),
        }
    }

    fn get(&self, path: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(held, _)| held == path)
            .map(|(_, revision)| revision.clone())
    }

    fn all(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Default)]
struct Unreadable(Arc<Mutex<HashSet<String>>>);

impl Unreadable {
    fn of(paths: &[&str]) -> Self {
        Self(Arc::new(Mutex::new(paths.iter().map(|path| (*path).to_owned()).collect())))
    }
    fn add(&self, path: &str) {
        self.0.lock().unwrap().insert(path.to_owned());
    }
    fn remove(&self, path: &str) {
        self.0.lock().unwrap().remove(path);
    }
    fn contains(&self, path: &str) -> bool {
        self.0.lock().unwrap().contains(path)
    }
}

fn environment_capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        ..capabilities(&[PullRequestAction::Merge], &[PullRequestMergeMethod::Merge])
    }
}

/// `environmentViewedProvider(revisions, asked, unreadable)`: a GitLab-like host with no marks of
/// its own. A path the host looked at and did not find is at the empty version (a deleted file);
/// one it could not look at is left out entirely.
fn environment_viewed_provider(head: &Head, asked: &Log<Vec<String>>, unreadable: &Unreadable) -> FakeProvider {
    let mut gitlab = FakeProvider::new(Kind::Gitlab).with_capabilities(environment_capabilities());
    gitlab.get_files_viewed = Some(die("the host keeps no marks of its own"));
    gitlab.set_files_viewed = Some(die("the host keeps no marks of its own"));
    // A merge confirms itself against the host before it says the change request landed.
    gitlab.get_change_request_summary = Some(ok(summary_row(1, "2026-07-02T00:00:00Z")));
    gitlab.get_file_revisions = Some({
        let (head, asked, unreadable) = (head.clone(), asked.clone(), unreadable.clone());
        h(move |input: FileRevisionsInput| {
            asked.push(input.paths.clone());
            let revisions = input
                .paths
                .iter()
                .filter(|path| !unreadable.contains(path))
                .map(|path| (path.clone(), head.get(path).unwrap_or_default()))
                .collect();
            async move { Ok(ProviderFileRevisions { revisions, complete: None }) }
        })
    });
    gitlab
}

fn gitlab_project() -> OrchestrationProjectShell {
    gh("p1", "on gitlab", "/a", "group/project").provider("gitlab").build()
}

fn environment_viewed_service(head: &Head, asked: &Log<Vec<String>>, unreadable: &Unreadable) -> Harness {
    make_service(vec![gitlab_project()], vec![environment_viewed_provider(head, asked, unreadable)])
}

fn gitlab_reference() -> PullRequestRef {
    reference("p1", "group/project", 1)
}

fn viewed(path: &str) -> PullRequestFileViewed {
    PullRequestFileViewed {
        path: path.into(),
        state: PullRequestFileViewedState::Viewed,
    }
}

fn dismissed(path: &str) -> PullRequestFileViewed {
    PullRequestFileViewed {
        path: path.into(),
        state: PullRequestFileViewedState::Dismissed,
    }
}

fn sorted(mut files: Vec<PullRequestFileViewed>) -> Vec<PullRequestFileViewed> {
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn sorted_paths(asked: &Log<Vec<String>>) -> Vec<Vec<String>> {
    asked
        .all()
        .into_iter()
        .map(|mut paths| {
            paths.sort();
            paths
        })
        .collect()
}

fn invalidate_reference(reference: PullRequestRef) -> PullRequestInvalidateInput {
    PullRequestInvalidateInput {
        reference: Some(reference),
        files_viewed_only: None,
    }
}

/// The diff `GET repos/reviewer/project/pulls/1.diff` answers: `alpha.ts` at `alpha`, and (unless
/// cut short) the other files, a quoted path, a deletion and a pure rename.
fn forgejo_diff(alpha: &str, truncated: bool) -> support_forgejo::Answer {
    if truncated {
        return support_forgejo::Answer {
            truncated: true,
            ..support_forgejo::status(200, &format!("diff --git a/alpha.ts b/alpha.ts\nindex eb2d7c5..{alpha} 100644\n"))
        };
    }
    let body = [
        "diff --git a/alpha.ts b/alpha.ts".to_owned(),
        format!("index eb2d7c5..{alpha} 100644"),
        "diff --git a/beta.ts b/beta.ts".to_owned(),
        "index b39e8b3..5851425 100644".to_owned(),
        r#"diff --git "a/caf\303\251 notes.txt" "b/caf\303\251 notes.txt""#.to_owned(),
        "index ffd99ce..6dd9855 100644".to_owned(),
        "diff --git a/deleted.txt b/deleted.txt".to_owned(),
        "deleted file mode 100644".to_owned(),
        "index 233f5c6..0000000".to_owned(),
        "diff --git a/old.txt b/renamed.txt".to_owned(),
        "similarity index 100%".to_owned(),
        "rename from old.txt".to_owned(),
        "rename to renamed.txt".to_owned(),
        String::new(),
    ]
    .join("\n");
    support_forgejo::status(200, &body)
}

#[tokio::test]
async fn tracks_forgejo_viewed_files_through_its_diff_and_refuses_truncated_baselines() {
    const DIFF: &str = "GET repos/reviewer/project/pulls/1.diff";
    let home = tempfile::tempdir().unwrap();
    let host = support_forgejo::Host::new();
    host.on("GET user", support_forgejo::ok(serde_json::json!({"login": "reviewer"})));
    host.on(DIFF, forgejo_diff("b485fe5", false));
    let provider: SharedProvider = Arc::new(host.provider(home.path()));
    let service = make_service_over(
        vec![gh("forgejo", "on forgejo", "/forgejo", "reviewer/project")
            .provider("forgejo")
            .host("forge.example.test")
            .remote_url("https://forge.example.test/reviewer/project.git")
            .build()],
        vec![provider],
        None,
    );
    let diff_reads = || host.requests().iter().filter(|request| request.as_str() == DIFF).count();
    let reference = hosted_ref("forgejo", "forge.example.test", "reviewer/project", 1);
    let states = |files: Vec<PullRequestFileViewed>| {
        files
            .into_iter()
            .map(|file| (file.path, file.state))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let paths = ["alpha.ts", "beta.ts", "café notes.txt", "deleted.txt", "renamed.txt"];

    service.set_files_viewed(set_files(&reference, &paths.map(|path| (path, true)))).await.unwrap();
    assert_eq!(
        states(service.files_viewed(reference.clone()).await.unwrap().files),
        paths.iter().map(|path| ((*path).to_owned(), PullRequestFileViewedState::Viewed)).collect()
    );
    assert_eq!(diff_reads(), 1);

    host.on(DIFF, forgejo_diff("aabbccd", false));
    service.invalidate(invalidate_reference(reference.clone()), false).await;
    assert_eq!(
        states(service.files_viewed(reference.clone()).await.unwrap().files),
        paths
            .iter()
            .map(|path| {
                let state = if *path == "alpha.ts" {
                    PullRequestFileViewedState::Dismissed
                } else {
                    PullRequestFileViewedState::Viewed
                };
                ((*path).to_owned(), state)
            })
            .collect()
    );
    service.set_files_viewed(set_files(&reference, &[("beta.ts", false)])).await.unwrap();
    assert!(!service
        .files_viewed(reference.clone())
        .await
        .unwrap()
        .files
        .iter()
        .any(|file| file.path == "beta.ts"));

    host.on(DIFF, forgejo_diff("aabbccd", true));
    service.invalidate(invalidate_reference(reference.clone()), false).await;
    service.set_files_viewed(set_files(&reference, &[("beta.ts", true)])).await.unwrap();
    host.on(DIFF, forgejo_diff("aabbccd", false));
    service.invalidate(invalidate_reference(reference.clone()), false).await;
    // A partial response must not stamp an empty revision and then dismiss the mark on recovery.
    assert_eq!(
        service
            .files_viewed(reference)
            .await
            .unwrap()
            .files
            .into_iter()
            .find(|file| file.path == "beta.ts"),
        Some(viewed("beta.ts"))
    );
}

#[tokio::test]
async fn keeps_hosted_forgejo_marks_with_their_repository_instead_of_the_serving_checkout() {
    let mut forgejo = environment_viewed_provider(&Head::of(&[("same.ts", "blob")]), &Log::default(), &Unreadable::default());
    forgejo.kind = Kind::Forgejo;
    let service = make_service_with(
        vec![gh("p1", "first repository", "/first", "reviewer/first")
            .provider("forgejo")
            .host("forge.example")
            .remote_url("https://forge.example:3000/reviewer/first.git")
            .build()],
        vec![forgejo],
        Some(std::sync::Arc::new(|_, _| Some(info(Kind::Forgejo, "Forgejo", "https://forge.example:3000")))),
    );
    let first = hosted_ref("p1", "forge.example:3000", "reviewer/first", 1);
    let second = PullRequestRef {
        repository: "reviewer/second".into(),
        ..first.clone()
    };
    service.set_files_viewed(set_files(&first, &[("same.ts", true)])).await.unwrap();
    assert!(service.files_viewed(second.clone()).await.unwrap().files.is_empty());
    service.set_files_viewed(set_files(&second, &[("same.ts", true)])).await.unwrap();
    service.set_files_viewed(set_files(&first, &[("same.ts", false)])).await.unwrap();
    assert_eq!(service.files_viewed(second.clone()).await.unwrap().files, vec![viewed("same.ts")]);

    service.push_project(
        gh("p2", "second repository", "/second", "reviewer/second")
            .provider("forgejo")
            .host("forge.example")
            .remote_url("https://forge.example:3000/reviewer/second.git"),
    );
    assert_eq!(
        service
            .files_viewed(PullRequestRef {
                project_id: ProjectId::new("p2"),
                ..second.clone()
            })
            .await
            .unwrap()
            .files,
        vec![viewed("same.ts")]
    );
    service.push_project(
        gh("ssh", "second repository over SSH", "/ssh", "reviewer/second")
            .provider("forgejo")
            .host("ssh.forge.example")
            .remote_url("git@ssh.forge.example:reviewer/second.git"),
    );
    assert_eq!(
        service
            .files_viewed(PullRequestRef {
                project_id: ProjectId::new("ssh"),
                ..second
            })
            .await
            .unwrap()
            .files,
        vec![viewed("same.ts")]
    );
}

#[tokio::test]
async fn keeps_viewed_files_itself_for_a_host_that_keeps_none_of_its_own() {
    let asked = Log::default();
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a"), ("src/b.ts", "blob-b")]), &asked, &Unreadable::default());

    // Nothing marked is nothing to ask the host about.
    let empty = service.files_viewed(gitlab_reference()).await.unwrap();
    assert_eq!(
        empty,
        PullRequestFilesViewedResult {
            files: Vec::new(),
            truncated: false
        }
    );
    assert!(asked.all().is_empty());

    service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true), ("src/b.ts", true)]))
        .await
        .unwrap();
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(marked.files), vec![viewed("src/a.ts"), viewed("src/b.ts")]);
    assert!(!marked.truncated);
    // The marked paths alone, and the read after the press is answered from what it heard.
    assert_eq!(sorted_paths(&asked), vec![vec!["src/a.ts".to_owned(), "src/b.ts".to_owned()]]);
}

#[tokio::test]
async fn holds_what_a_whole_change_answer_carried_so_the_next_tick_reads_nothing() {
    let asked: Log<Vec<String>> = Log::default();
    let head = Head::of(&[("src/a.ts", "blob-a"), ("src/b.ts", "blob-b")]);
    let mut provider = environment_viewed_provider(&head, &Log::default(), &Unreadable::default());
    // A host with no per-file version reads the whole change to answer for one file.
    provider.get_file_revisions = Some({
        let (head, asked) = (head.clone(), asked.clone());
        h(move |input: FileRevisionsInput| {
            asked.push(input.paths.clone());
            let revisions = head.all();
            async move {
                Ok(ProviderFileRevisions {
                    revisions,
                    complete: Some(true),
                })
            }
        })
    });
    let service = make_service(vec![gitlab_project()], vec![provider]);

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    // A path nothing has asked about before: its version came back with the first answer.
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();

    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()]]);
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();
    assert_eq!(sorted(marked.files), vec![viewed("src/a.ts"), viewed("src/b.ts")]);

    // Still only as fresh as the read it came from.
    service.adjust(2 * 60_000).await;
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();
    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()], vec!["src/b.ts".to_owned()]]);
}

#[tokio::test]
async fn reads_the_marks_without_asking_the_host_what_the_head_has_every_time() {
    let asked = Log::default();
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a")]), &asked, &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    // Past the marks' own cache, so this read reaches the point where the host would be asked.
    service.adjust(20_000).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(marked.files, vec![viewed("src/a.ts")]);
    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()]]);
}

#[tokio::test]
async fn answers_the_marks_from_what_it_last_heard_while_it_asks_the_host_again() {
    let asked = Log::default();
    let head = Head::of(&[("src/a.ts", "blob-a")]);
    let service = environment_viewed_service(&head, &asked, &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    head.set("src/a.ts", "blob-a-again");
    service.adjust(90_000).await;
    let held = service.files_viewed(gitlab_reference()).await.unwrap();
    settle().await;

    // The push is not in this answer, because waiting for the host is the thing being avoided.
    assert_eq!(held.files, vec![viewed("src/a.ts")]);
    assert_eq!(asked.len(), 2);

    service.adjust(20_000).await;
    let caught = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(caught.files, vec![dismissed("src/a.ts")]);
    // The refresh behind the previous answer is the one that heard about the push.
    assert_eq!(asked.len(), 2);
}

#[tokio::test]
async fn asks_the_host_about_a_file_it_has_not_been_asked_about_before() {
    let asked = Log::default();
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a"), ("src/b.ts", "blob-b")]), &asked, &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    service.adjust(20_000).await;
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();
    service.adjust(20_000).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(marked.files), vec![viewed("src/a.ts"), viewed("src/b.ts")]);
    // The second press paid for its own file; the read that follows was already covered.
    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()], vec!["src/b.ts".to_owned()]]);
}

#[tokio::test]
async fn does_not_let_a_press_about_one_file_keep_another_files_version_alive() {
    let asked = Log::default();
    let head = Head::of(&[("src/a.ts", "blob-a"), ("src/b.ts", "blob-b")]);
    let service = environment_viewed_service(&head, &asked, &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    service.adjust(40_000).await;
    // This press asks about its own file and carries the other one forward untouched.
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();
    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()], vec!["src/b.ts".to_owned()]]);

    head.set("src/a.ts", "blob-a-again");
    service.adjust(30_000).await;
    service.files_viewed(gitlab_reference()).await.unwrap();
    settle().await;
    assert_eq!(asked.len(), 3);

    service.adjust(20_000).await;
    let caught = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(caught.files), vec![dismissed("src/a.ts"), viewed("src/b.ts")]);
}

#[tokio::test]
async fn reports_a_file_pushed_to_since_it_was_cleared_as_changed() {
    let head = Head::of(&[("src/a.ts", "blob-a"), ("src/b.ts", "blob-b")]);
    let service = environment_viewed_service(&head, &Log::default(), &Unreadable::default());

    service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true), ("src/b.ts", true)]))
        .await
        .unwrap();
    head.set("src/a.ts", "blob-a-again");
    // A push is not something the marks can hear about, so the reader asks to be re-answered.
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(marked.files), vec![dismissed("src/a.ts"), viewed("src/b.ts")]);
}

#[tokio::test]
async fn clears_a_mark_again_when_the_file_is_put_back() {
    let asked = Log::default();
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a")]), &asked, &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", false)])).await.unwrap();
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert!(marked.files.is_empty());
    // Unticking asks the host nothing: the row is going away whatever the head has.
    assert_eq!(asked.all(), vec![vec!["src/a.ts".to_owned()]]);
}

#[tokio::test]
async fn keeps_a_deleted_file_cleared_which_the_head_has_no_version_of_at_all() {
    let service = environment_viewed_service(&Head::default(), &Log::default(), &Unreadable::default());

    service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/gone.ts", true)]))
        .await
        .unwrap();
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(marked.files, vec![viewed("src/gone.ts")]);
}

#[tokio::test]
async fn leaves_a_mark_alone_when_the_host_could_not_say_what_the_head_has_of_it() {
    // Reading the rest of a long change as deleted would clear every file past the cut.
    let head = Head::of(&[("src/a.ts", "blob-a"), ("src/past-the-cut.ts", "blob-b")]);
    let service = environment_viewed_service(&head, &Log::default(), &Unreadable::of(&["src/past-the-cut.ts"]));

    service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true), ("src/past-the-cut.ts", true)]))
        .await
        .unwrap();
    head.set("src/a.ts", "blob-a-again");
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(marked.files), vec![dismissed("src/a.ts"), viewed("src/past-the-cut.ts")]);
}

#[tokio::test]
async fn keeps_a_file_cleared_that_the_press_could_not_learn_a_version_for() {
    let head = Head::of(&[("src/past-the-cut.ts", "blob-b")]);
    let unreadable = Unreadable::of(&["src/past-the-cut.ts"]);
    let service = environment_viewed_service(&head, &Log::default(), &unreadable);

    service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/past-the-cut.ts", true)]))
        .await
        .unwrap();
    unreadable.remove("src/past-the-cut.ts");
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;

    assert_eq!(
        service.files_viewed(gitlab_reference()).await.unwrap().files,
        vec![viewed("src/past-the-cut.ts")]
    );
}

#[tokio::test]
async fn keeps_the_version_it_last_heard_when_a_later_read_of_the_head_stops_short() {
    let asked = Log::default();
    let head = Head::of(&[("src/a.ts", "blob-a")]);
    let unreadable = Unreadable::default();
    let service = environment_viewed_service(&head, &asked, &unreadable);

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    head.set("src/a.ts", "blob-a-again");
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;
    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![dismissed("src/a.ts")]);

    // The read behind the next answer has to stop before this file.
    unreadable.add("src/a.ts");
    service.adjust(90_000).await;
    service.files_viewed(gitlab_reference()).await.unwrap();
    service.adjust(20_000).await;

    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![dismissed("src/a.ts")]);
}

#[tokio::test]
async fn re_asks_what_the_head_has_of_a_marked_file_after_a_whole_workspace_refresh() {
    let head = Head::of(&[("src/a.ts", "blob-a")]);
    let service = environment_viewed_service(&head, &Log::default(), &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    // A push nobody told this environment about: the refresh is the reader asking for all of it.
    head.set("src/a.ts", "blob-a-again");
    service
        .invalidate(
            PullRequestInvalidateInput {
                reference: None,
                files_viewed_only: None,
            },
            false,
        )
        .await;

    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![dismissed("src/a.ts")]);
}

#[tokio::test]
async fn forgets_what_the_head_had_of_a_marked_file_once_a_mutation_moves_the_head() {
    let head = Head::of(&[("src/a.ts", "blob-a")]);
    let service = environment_viewed_service(&head, &Log::default(), &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    // Merging moves the head under the mark, and nobody asks for the refresh.
    head.set("src/a.ts", "blob-a-again");
    service.run_action(action(&gitlab_reference(), PullRequestAction::Merge)).await.unwrap();

    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![dismissed("src/a.ts")]);
}

#[tokio::test]
async fn still_reports_its_own_marks_when_the_host_will_not_say_what_the_head_has() {
    let answering = Cell::new(true);
    let mut gitlab = FakeProvider::new(Kind::Gitlab).with_capabilities(environment_capabilities());
    gitlab.get_files_viewed = Some(die("the host keeps no marks of its own"));
    gitlab.set_files_viewed = Some(die("the host keeps no marks of its own"));
    gitlab.get_file_revisions = Some({
        let answering = answering.clone();
        h(move |input: FileRevisionsInput| {
            let answer = if answering.get() {
                Ok(ProviderFileRevisions {
                    revisions: input.paths.iter().map(|path| (path.clone(), "blob-a".to_owned())).collect(),
                    complete: None,
                })
            } else {
                Err(request_failed())
            };
            async move { answer }
        })
    });
    let service = make_service(vec![gitlab_project()], vec![gitlab]);

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    answering.set(false);
    service.invalidate(invalidate_reference(gitlab_reference()), false).await;

    // A rate limit or a signed-out CLI costs the marks their staleness, not the record.
    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![viewed("src/a.ts")]);
}

#[tokio::test]
async fn finishes_two_presses_on_one_file_in_the_order_they_were_made() {
    // A tick asks the host before it stores anything and an untick asks nothing, so the second
    // press would otherwise land first and be overwritten by the first finishing behind it.
    let held = Gate::default();
    let mut gitlab = FakeProvider::new(Kind::Gitlab).with_capabilities(environment_capabilities());
    gitlab.get_files_viewed = Some(die("the host keeps no marks of its own"));
    gitlab.set_files_viewed = Some(die("the host keeps no marks of its own"));
    gitlab.get_file_revisions = Some({
        let held = held.clone();
        h(move |input: FileRevisionsInput| {
            let held = held.clone();
            async move {
                held.wait().await;
                Ok(ProviderFileRevisions {
                    revisions: input.paths.iter().map(|path| (path.clone(), "blob-a".to_owned())).collect(),
                    complete: None,
                })
            }
        })
    });
    let service = make_service(vec![gitlab_project()], vec![gitlab]);

    let tick = tokio::spawn({
        let service = service.service.clone();
        async move { service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await }
    });
    // Far enough for the tick to be waiting on the host rather than still on its way there.
    service.adjust(1_000).await;
    let untick = tokio::spawn({
        let service = service.service.clone();
        async move { service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", false)])).await }
    });
    service.adjust(1_000).await;
    held.open();
    tick.await.unwrap().unwrap();
    untick.await.unwrap().unwrap();

    // The untick came second and stands: the file is open again.
    assert!(service.files_viewed(gitlab_reference()).await.unwrap().files.is_empty());
}

/// Azure, whose selector is a bare repository name, backed by this environment's own marks.
fn azure_viewed_service() -> Harness {
    let mut azure = FakeProvider::new(Kind::AzureDevops).with_capabilities(environment_capabilities());
    azure.get_files_viewed = Some(die("the host keeps no marks of this environment's own"));
    azure.set_files_viewed = Some(die("the host keeps no marks of this environment's own"));
    azure.get_file_revisions = Some(h(|input: FileRevisionsInput| async move {
        Ok(ProviderFileRevisions {
            revisions: input.paths.iter().map(|path| (path.clone(), "blob-a".to_owned())).collect(),
            complete: None,
        })
    }));
    make_service(
        vec![
            gh("p1", "platform web", "/a", "acme/platform/_git/web")
                .provider("azure-devops")
                .host("dev.azure.com")
                .build(),
            gh("p2", "other web", "/b", "acme/other/_git/web")
                .provider("azure-devops")
                .host("dev.azure.com")
                .build(),
        ],
        vec![azure],
    )
}

#[tokio::test]
async fn keeps_the_marks_of_two_azure_repositories_of_the_same_name_apart() {
    // Azure addresses a repository by its bare name, unique inside one of its projects only.
    let service = azure_viewed_service();
    let platform = reference("p1", "web", 1);
    let other = reference("p2", "web", 1);

    service.set_files_viewed(set_files(&platform, &[("src/a.ts", true)])).await.unwrap();

    assert_eq!(service.files_viewed(platform).await.unwrap().files, vec![viewed("src/a.ts")]);
    assert!(service.files_viewed(other).await.unwrap().files.is_empty());
}

#[tokio::test]
async fn keeps_environment_marks_apart_from_another_change_requests() {
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a")]), &Log::default(), &Unreadable::default());

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    let other = service
        .files_viewed(PullRequestRef {
            number: 2,
            ..gitlab_reference()
        })
        .await
        .unwrap();

    assert!(other.files.is_empty());
}

#[tokio::test]
async fn bounds_the_paths_one_change_requests_held_revisions_carry() {
    // A press carries at most half the cap, so going one past it takes three.
    let asked = Log::default();
    let service = environment_viewed_service(&Head::default(), &asked, &Unreadable::default());
    let press = |prefix: &str, count: usize| {
        let paths: Vec<String> = (0..count).map(|index| format!("{prefix}/{index:04}.ts")).collect();
        let files: Vec<(&str, bool)> = paths.iter().map(|path| (path.as_str(), true)).collect();
        set_files(&gitlab_reference(), &files)
    };

    service.set_files_viewed(press("a", MAX_FILE_REVISION_PATHS / 2)).await.unwrap();
    service.set_files_viewed(press("b", MAX_FILE_REVISION_PATHS / 2)).await.unwrap();
    service.set_files_viewed(press("c", 1)).await.unwrap();
    let pressed = asked.len();

    // The marks a read carries come first by path, so this one covers the earliest batch, which
    // is where the paths asked about longest ago are.
    service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(asked.len(), pressed + 1);
    assert!(asked.all().last().unwrap().contains(&"a/0000.ts".to_owned()));
}

#[tokio::test]
async fn keeps_the_marked_paths_when_a_whole_change_answer_is_wider_than_the_cap() {
    // What the trim reaches has to be the paths the answer threw in rather than the ticked ones.
    let head = Head(Arc::new(Mutex::new(
        (0..MAX_FILE_REVISION_PATHS + 178)
            .map(|index| (format!("src/f{index:04}.ts"), format!("blob-{index}")))
            .collect(),
    )));
    let mut provider = environment_viewed_provider(&head, &Log::default(), &Unreadable::default());
    provider.get_file_revisions = Some({
        let head = head.clone();
        h(move |_| {
            let revisions = head.all();
            async move {
                Ok(ProviderFileRevisions {
                    revisions,
                    complete: Some(true),
                })
            }
        })
    });
    let service = make_service(vec![gitlab_project()], vec![provider]);
    let ticked = ["src/f0000.ts", "src/f0500.ts"];

    service
        .set_files_viewed(set_files(&gitlab_reference(), &ticked.map(|path| (path, true))))
        .await
        .unwrap();
    for path in ticked {
        head.set(path, "blob-moved");
    }
    // Past the stale window, so the read is answered by the host.
    service.adjust(11 * 60_000).await;
    let marked = service.files_viewed(gitlab_reference()).await.unwrap();

    assert_eq!(sorted(marked.files), vec![dismissed("src/f0000.ts"), dismissed("src/f0500.ts")]);
}

#[tokio::test]
async fn keeps_the_change_request_being_ticked_through_not_the_one_pressed_first() {
    let asked = Log::default();
    let service = environment_viewed_service(&Head::of(&[("src/a.ts", "blob-a")]), &asked, &Unreadable::default());
    let press = |number: i64| set_files(&PullRequestRef { number, ..gitlab_reference() }, &[("src/a.ts", true)]);

    service.set_files_viewed(press(1)).await.unwrap();
    // A cache's worth of other change requests, with the open one pressed in between each.
    for filled in 0..FILE_REVISIONS_CACHE_CAPACITY as i64 {
        service.set_files_viewed(press(2 + filled)).await.unwrap();
        service.set_files_viewed(press(1)).await.unwrap();
    }

    assert_eq!(asked.len(), 1 + FILE_REVISIONS_CACHE_CAPACITY);
}

fn environment_viewed_service_with_viewer(head: &Head, get_viewer: Handler<ProviderHostRef, String>) -> Harness {
    let provider = FakeProvider {
        get_viewer,
        ..environment_viewed_provider(head, &Log::default(), &Unreadable::default())
    };
    make_service(vec![gitlab_project()], vec![provider])
}

#[tokio::test]
async fn keeps_one_readers_marks_on_a_host_that_names_nobody() {
    let service = environment_viewed_service_with_viewer(&Head::of(&[("src/a.ts", "blob-a")]), ok(String::new()));

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();

    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![viewed("src/a.ts")]);
}

fn counting_viewer(lookups: &Counter) -> Handler<ProviderHostRef, String> {
    let lookups = lookups.clone();
    h(move |_| {
        lookups.bump();
        async { Ok("bilal".to_owned()) }
    })
}

#[tokio::test]
async fn puts_a_listing_and_a_press_for_one_host_on_a_single_viewer_lookup() {
    let lookups = Counter::default();
    let provider = FakeProvider {
        get_viewer: {
            let lookups = lookups.clone();
            h(move |_| {
                lookups.bump();
                async {
                    // Suspends before answering, as a subprocess would.
                    tokio::task::yield_now().await;
                    Ok("bilal".to_owned())
                }
            })
        },
        ..environment_viewed_provider(&Head::of(&[("src/a.ts", "blob-a")]), &Log::default(), &Unreadable::default())
    };
    let service = make_service(vec![gitlab_project()], vec![provider]);

    // A cold page load reads the listing and the reader's own marks at the same time.
    let (listed, marks) = tokio::join!(service.list(open_list()), service.files_viewed(gitlab_reference()));
    listed.unwrap();
    marks.unwrap();

    assert_eq!(lookups.get(), 1);
}

#[tokio::test]
async fn carries_a_bounded_number_of_its_own_marks_and_says_it_held_more() {
    let rows = zc_db::repos::pull_request_files_viewed::MAX_FILES_VIEWED_ROWS as usize;
    let paths: Vec<String> = (0..rows + 40).map(|at| format!("src/f{at:04}.ts")).collect();
    let head = Head(Arc::new(Mutex::new(paths.iter().map(|path| (path.clone(), "blob".to_owned())).collect())));
    let service = environment_viewed_service(&head, &Log::default(), &Unreadable::default());

    let files: Vec<(&str, bool)> = paths.iter().map(|path| (path.as_str(), true)).collect();
    service.set_files_viewed(set_files(&gitlab_reference(), &files)).await.unwrap();
    let read = service.files_viewed(gitlab_reference()).await.unwrap();

    // The reader is told the count is short rather than shown a quietly clipped list.
    assert_eq!(read.files.len(), rows);
    assert!(read.truncated);
}

fn backing_off() -> Handler<FileRevisionsInput, ProviderFileRevisions> {
    // Backing off for the hour, so the pause outlives the ten minutes who is signed in is held.
    fail(rate_limited(
        Kind::Gitlab,
        "getFileRevisions",
        "API rate limit exceeded.",
        Some(60 * 60 * 1_000),
    ))
}

#[tokio::test]
async fn records_a_press_while_the_host_is_backing_off() {
    let lookups = Counter::default();
    let provider = FakeProvider {
        get_viewer: counting_viewer(&lookups),
        get_file_revisions: Some(backing_off()),
        ..environment_viewed_provider(&Head::of(&[("src/a.ts", "blob-a")]), &Log::default(), &Unreadable::default())
    };
    let service = make_service(vec![gitlab_project()], vec![provider]);

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    service.adjust(11 * 60_000).await;
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();

    // These rows keep no baseline, because none was read, so they hold until pressed again.
    assert_eq!(
        sorted(service.files_viewed(gitlab_reference()).await.unwrap().files),
        vec![viewed("src/a.ts"), viewed("src/b.ts")]
    );
    assert_eq!(lookups.get(), 2);
}

#[tokio::test]
async fn asks_who_is_reading_through_a_pause_only_for_the_press_that_is_waiting_on_it() {
    let lookups = Counter::default();
    let provider = FakeProvider {
        get_viewer: counting_viewer(&lookups),
        list_change_requests: ok(page(Vec::new(), false, true)),
        get_file_revisions: Some(backing_off()),
        ..environment_viewed_provider(&Head::of(&[("src/a.ts", "blob-a")]), &Log::default(), &Unreadable::default())
    };
    let service = make_service(vec![gitlab_project()], vec![provider]);

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    assert_eq!(lookups.get(), 1);
    service.adjust(11 * 60_000).await;

    // A listing is not the reader waiting on this lookup.
    let listed = service
        .list(PullRequestListInput {
            involvement: Some(PullRequestInvolvement::All),
            ..open_list()
        })
        .await
        .unwrap_err();
    assert_eq!(tag(&listed), "PullRequestOperationError");
    assert_eq!(lookups.get(), 1);

    // The press is bounded by what the reader does, so it is asked rather than refused.
    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)])).await.unwrap();
    assert_eq!(lookups.get(), 2);
}

#[tokio::test]
async fn does_not_ask_a_paused_host_who_is_reading_again_after_the_ask_failed() {
    let lookups = Counter::default();
    let provider = FakeProvider {
        get_viewer: {
            let lookups = lookups.clone();
            h(move |_| {
                lookups.bump();
                async { Err(failed(Kind::Gitlab, "getViewer", "glab exited with status 1")) }
            })
        },
        list_change_requests: ok(page(Vec::new(), false, true)),
        run_action: fail(rate_limited(Kind::Gitlab, "runAction", "API rate limit exceeded.", Some(60 * 60 * 1_000))),
        ..environment_viewed_provider(&Head::of(&[("src/a.ts", "blob-a")]), &Log::default(), &Unreadable::default())
    };
    let service = make_service(vec![gitlab_project()], vec![provider]);
    let all = || PullRequestListInput {
        involvement: Some(PullRequestInvolvement::All),
        ..open_list()
    };

    service.files_viewed(gitlab_reference()).await.unwrap_err();
    assert_eq!(lookups.get(), 1);
    service.run_action(action(&gitlab_reference(), PullRequestAction::Merge)).await.unwrap_err();

    // A failed lookup is held nowhere, so a background read let through the pause would spawn the
    // host's CLI on every refresh.
    service.list(all()).await.unwrap_err();
    service.adjust(11 * 60_000).await;
    service.list(all()).await.unwrap_err();
    assert_eq!(lookups.get(), 1);
}

#[tokio::test]
async fn refuses_the_marks_when_the_host_could_not_be_asked_who_is_reading() {
    let answering = Cell::new(true);
    let service = environment_viewed_service_with_viewer(&Head::of(&[("src/a.ts", "blob-a")]), {
        let answering = answering.clone();
        h(move |_| {
            let answer = if answering.get() {
                Ok("bilal".to_owned())
            } else {
                Err(failed(Kind::Gitlab, "getViewer", "glab exited with status 1"))
            };
            async move { answer }
        })
    });

    service.set_files_viewed(set_files(&gitlab_reference(), &[("src/a.ts", true)])).await.unwrap();
    // Who is signed in is held for ten minutes, so the lookup has to come round again first.
    answering.set(false);
    service.adjust(11 * 60_000).await;

    let read = service.files_viewed(gitlab_reference()).await.unwrap_err();
    let write = service
        .set_files_viewed(set_files(&gitlab_reference(), &[("src/b.ts", true)]))
        .await
        .unwrap_err();
    assert_eq!(tag(&read), "PullRequestOperationError");
    assert_eq!(tag(&write), "PullRequestOperationError");

    answering.set(true);
    assert_eq!(service.files_viewed(gitlab_reference()).await.unwrap().files, vec![viewed("src/a.ts")]);
}

#[tokio::test]
async fn refuses_to_track_viewed_files_on_a_host_that_does_not() {
    let mut gitlab = FakeProvider::new(Kind::Gitlab);
    gitlab.get_files_viewed = Some(die("must not be called"));
    gitlab.set_files_viewed = Some(die("must not be called"));
    let service = make_service(vec![gitlab_project()], vec![gitlab]);

    let read = service.files_viewed(gitlab_reference()).await.unwrap_err();
    let write = service.set_files_viewed(set_files(&gitlab_reference(), &[("a.ts", true)])).await.unwrap_err();

    assert_eq!(tag(&read), "PullRequestOperationError");
    assert_eq!(tag(&write), "PullRequestOperationError");
}
