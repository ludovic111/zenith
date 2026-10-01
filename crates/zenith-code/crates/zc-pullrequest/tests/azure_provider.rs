//! Port of `AzureDevOpsPullRequestProvider.test.ts`: the provider over a mocked CLI.

#![allow(clippy::result_large_err)]

mod support_azure;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use regex::Regex;
use support_azure::MockCli;
use zc_contracts::{PullRequestMergeability, PullRequestState};
use zc_pullrequest::azure::cli::{AzureDevOpsIterationChanges, AzureDevOpsPullRequestCliError};
use zc_pullrequest::azure::diff::{byte_length, parse_azure_devops_diff_cursor, MAX_DIFF_SLICE_BYTES, MAX_DIFF_SLICE_FILES, MAX_FILE_DIFF_EDITS};
use zc_pullrequest::azure::json::{
    AzureDevOpsChangeEntry, AzureDevOpsChangeKind, AzureDevOpsItemContent, AzureDevOpsIteration, AzureDevOpsPullRequest, AzureDevOpsRepositoryLocation,
};
use zc_pullrequest::azure::provider::{LOCATION_CACHE_CAPACITY, MAX_DIFF_SPAWNS};
use zc_pullrequest::azure::AzureDevOpsPullRequestProvider;
use zc_pullrequest::provider::*;
use zc_sourcecontrol::errors::Cause;

fn iteration() -> AzureDevOpsIteration {
    AzureDevOpsIteration {
        id: 3,
        head_commit: "head".into(),
        merge_base_commit: "base".into(),
    }
}

fn pull_request(number: i64) -> AzureDevOpsPullRequest {
    AzureDevOpsPullRequest {
        number,
        title: format!("Pull request {number}"),
        url: format!("https://dev.azure.com/acme/web/_git/web/pullrequest/{number}"),
        author: None,
        head_branch: "feat/page".into(),
        base_branch: "main".into(),
        state: PullRequestState::Open,
        is_draft: false,
        mergeability: PullRequestMergeability::Mergeable,
        created_at: "2026-07-01T00:00:00Z".into(),
        updated_at: "2026-07-02T00:00:00Z".into(),
        closed_at: None,
        body: String::new(),
        review_request_logins: Vec::new(),
        reviewers: Vec::new(),
        location: Some(AzureDevOpsRepositoryLocation {
            project: "acme".into(),
            repository: "web".into(),
        }),
        auto_merge_enabled: false,
        auto_merge_method: None,
    }
}

fn change(path: &str, kind: AzureDevOpsChangeKind) -> AzureDevOpsChangeEntry {
    AzureDevOpsChangeEntry {
        path: path.into(),
        old_path: path.into(),
        change_kind: kind,
        object_id: Some("8f80".into()),
        original_object_id: Some("0ca4".into()),
    }
}

/// A side whose lines share nothing with the other side's: `lines` removals and additions of at
/// least `width` characters each.
fn side(prefix: &str, lines: usize, width: usize) -> String {
    let pad = "z".repeat(width);
    format!("{}\n", (0..lines).map(|line| format!("{prefix} {line} {pad}")).collect::<Vec<_>>().join("\n"))
}

fn change_request(number: i64) -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/w".into(),
        repository: "acme/web".into(),
        host: "dev.azure.com".into(),
        number,
    }
}

fn diff_input(number: i64, cursor: Option<&str>) -> GetDiffInput {
    GetDiffInput {
        change_request: change_request(number),
        cursor: cursor.map(Into::into),
        commit: None,
    }
}

fn base_mock(paths: Vec<String>, created: BTreeSet<String>) -> MockCli {
    MockCli {
        get_pull_request: Some(Box::new(|(_, number)| async move { Ok(pull_request(number)) }.boxed())),
        list_iterations: Some(Box::new(|_| async { Ok(vec![iteration()]) }.boxed())),
        list_iteration_changes: Some(Box::new(move |_| {
            let changes = paths
                .iter()
                .map(|path| {
                    change(
                        path,
                        if created.contains(path) {
                            AzureDevOpsChangeKind::New
                        } else {
                            AzureDevOpsChangeKind::Change
                        },
                    )
                })
                .collect();
            async move { Ok(AzureDevOpsIterationChanges { changes, truncated: false }) }.boxed()
        })),
        ..MockCli::default()
    }
}

struct SliceRead {
    slice: ProviderDiffSlice,
    reads: Vec<String>,
    peak_in_flight: usize,
}

#[derive(Default)]
struct SliceOptions<'a> {
    refused: &'a [&'a str],
    created: &'a [&'a str],
    cursor: Option<&'a str>,
}

async fn read_slice(paths: &[String], lines: usize, width: usize, options: SliceOptions<'_>) -> SliceRead {
    let reads = Arc::new(Mutex::new(Vec::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let refused: BTreeSet<String> = options.refused.iter().map(|p| (*p).to_owned()).collect();
    let created: BTreeSet<String> = options.created.iter().map(|p| (*p).to_owned()).collect();
    let mut mock = base_mock(paths.to_vec(), created.clone());
    let order = paths.to_vec();
    let (reads_in, in_flight_in, peak_in) = (reads.clone(), in_flight.clone(), peak.clone());
    mock.read_item_content = Some(Box::new(move |(path, commit)| {
        let (reads, in_flight, peak) = (reads_in.clone(), in_flight_in.clone(), peak_in.clone());
        // The later a file is listed the sooner it answers, so the patch has nothing but the
        // change list to take its order from.
        let answers_after = order.len() - order.iter().position(|p| *p == path).unwrap();
        let refused = refused.contains(&path);
        let created = created.contains(&path);
        async move {
            reads.lock().unwrap().push(path.clone());
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            for _ in 0..answers_after {
                tokio::task::yield_now().await;
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
            if refused {
                return Err(AzureDevOpsPullRequestCliError::Read {
                    cwd: "/w".into(),
                    operation: "readItemContent",
                    cause: Cause::message("refused"),
                });
            }
            let old_side = commit != "head";
            if old_side && created {
                return Ok(AzureDevOpsItemContent::default());
            }
            Ok(AzureDevOpsItemContent {
                contents: side(if old_side { "old" } else { "new" }, lines, width),
                is_binary: false,
            })
        }
        .boxed()
    }));
    let provider = AzureDevOpsPullRequestProvider::with_cli(Arc::new(mock));
    let slice = provider.get_diff(diff_input(7, options.cursor)).await.unwrap();
    let reads = reads.lock().unwrap().clone();
    SliceRead {
        slice,
        reads,
        peak_in_flight: peak.load(Ordering::SeqCst),
    }
}

/// Which files the patch carries a section for, in order.
fn patched_paths(patch: &str) -> Vec<String> {
    Regex::new(r"(?m)^diff --git a/(\S+) b/")
        .unwrap()
        .captures_iter(patch)
        .map(|c| c[1].to_owned())
        .collect()
}

fn names(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

#[tokio::test]
async fn summary_costs_the_one_pull_request_read() {
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = reads.clone();
    // listIterations and listIterationChanges are left unimplemented: a summary reaching for
    // either would panic.
    let mock = MockCli {
        get_pull_request: Some(Box::new(move |(_, number)| {
            counted.fetch_add(1, Ordering::SeqCst);
            async move { Ok(pull_request(number)) }.boxed()
        })),
        ..MockCli::default()
    };
    let provider = AzureDevOpsPullRequestProvider::with_cli(Arc::new(mock));
    assert!(provider.optional_methods().get_change_request_summary);
    let summary = provider.get_change_request_summary(change_request(7)).await.unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(summary.title, "Pull request 7");
    assert_eq!(summary.changed_files, None);
}

#[tokio::test]
async fn detail_still_reports_the_file_count() {
    let mock = base_mock(names(&["a.ts", "b.ts"]), BTreeSet::new());
    let provider = AzureDevOpsPullRequestProvider::with_cli(Arc::new(mock));
    let detail = provider.get_change_request(change_request(7)).await.unwrap();
    assert_eq!(detail.changed_files, 2);
}

#[tokio::test]
async fn holds_every_reader_together_to_one_requests_worth_of_processes() {
    let paths = names(&["a.ts", "b.ts", "c.ts", "d.ts", "e.ts", "f.ts"]);
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut mock = base_mock(paths, BTreeSet::new());
    let (in_flight_in, peak_in) = (in_flight.clone(), peak.clone());
    mock.read_item_content = Some(Box::new(move |_| {
        let (in_flight, peak) = (in_flight_in.clone(), peak_in.clone());
        async move {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(AzureDevOpsItemContent {
                contents: side("new", 2, 4),
                is_binary: false,
            })
        }
        .boxed()
    }));
    let provider = AzureDevOpsPullRequestProvider::with_cli(Arc::new(mock));
    let (first, second) = tokio::join!(provider.get_diff(diff_input(7, None)), provider.get_diff(diff_input(8, None)));
    first.unwrap();
    second.unwrap();
    assert!(peak.load(Ordering::SeqCst) <= MAX_DIFF_SPAWNS);
    assert!(peak.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn leaves_a_run_of_files_it_could_not_diff_at_all_for_the_next_slice() {
    let paths: Vec<String> = (0..MAX_DIFF_SLICE_FILES + 20).map(|at| format!("gen/a{at}.bin")).collect();
    let refused: Vec<&str> = paths.iter().map(String::as_str).collect();
    let read = read_slice(
        &paths,
        2,
        4,
        SliceOptions {
            refused: &refused,
            ..SliceOptions::default()
        },
    )
    .await;
    assert_eq!(patched_paths(&read.slice.patch).len(), MAX_DIFF_SLICE_FILES);
    // Well inside the byte budget, so the file count stopped it.
    assert!(byte_length(&read.slice.patch) < MAX_DIFF_SLICE_BYTES);
    assert_eq!(
        parse_azure_devops_diff_cursor(read.slice.next_cursor.as_deref()).unwrap().file_index,
        MAX_DIFF_SLICE_FILES as i64
    );
}

#[tokio::test]
async fn asks_for_both_sides_of_several_files_at_once() {
    let read = read_slice(&names(&["a.ts", "b.ts", "c.ts", "d.ts"]), 2, 4, SliceOptions::default()).await;
    assert!(read.peak_in_flight > 2);
}

#[tokio::test]
async fn holds_the_number_of_files_it_reads_at_once_down() {
    let paths: Vec<String> = (0..24).map(|file| format!("file-{file}.ts")).collect();
    let read = read_slice(&paths, 2, 4, SliceOptions::default()).await;
    assert_eq!(read.reads.len(), paths.len() * 2);
    assert!(read.peak_in_flight <= 8);
}

#[tokio::test]
async fn keeps_the_patch_in_the_order_the_change_was_listed() {
    let paths = names(&["a.ts", "b.ts", "c.ts", "d.ts", "e.ts"]);
    let read = read_slice(&paths, 2, 4, SliceOptions::default()).await;
    assert_eq!(patched_paths(&read.slice.patch), paths);
    assert_eq!(read.slice.next_cursor, None);
}

#[tokio::test]
async fn leaves_a_file_the_host_refused_listed_without_its_hunks_in_its_place() {
    let paths = names(&["a.ts", "b.ts", "c.ts"]);
    let read = read_slice(
        &paths,
        2,
        4,
        SliceOptions {
            refused: &["b.ts"],
            ..SliceOptions::default()
        },
    )
    .await;
    assert_eq!(patched_paths(&read.slice.patch), paths);
    assert!(read.slice.truncated);
    assert!(read.slice.patch.contains("+++ b/b.ts\ndiff --git a/c.ts"));
    assert_eq!(Regex::new(r"(?m)^@@ ").unwrap().find_iter(&read.slice.patch).count(), 2);
}

#[tokio::test]
async fn stops_on_the_byte_ceiling_without_carrying_what_it_read_past_it() {
    let paths = names(&["a.ts", "b.ts", "c.ts", "d.ts", "e.ts", "f.ts"]);
    let read = read_slice(&paths, 100, 900, SliceOptions::default()).await;
    assert!(read.slice.patch.len() > MAX_DIFF_SLICE_BYTES);
    assert_eq!(patched_paths(&read.slice.patch), names(&["a.ts", "b.ts"]));
    assert_eq!(read.slice.next_cursor.as_deref(), Some("3:2"));
    assert_eq!(
        read.reads.into_iter().collect::<BTreeSet<_>>(),
        names(&["a.ts", "b.ts", "c.ts", "d.ts"]).into_iter().collect()
    );
}

#[tokio::test]
async fn narrows_what_it_reads_at_once_as_the_slice_fills() {
    let paths = names(&["a.ts", "b.ts", "c.ts", "d.ts", "e.ts", "f.ts", "g.ts", "h.ts"]);
    let read = read_slice(&paths, 50, 450, SliceOptions::default()).await;
    assert!(read.slice.next_cursor.is_some());
    assert_eq!(
        read.reads.into_iter().collect::<BTreeSet<_>>(),
        patched_paths(&read.slice.patch).into_iter().collect()
    );
}

#[tokio::test]
async fn stops_once_the_diff_work_one_request_may_do_is_spent() {
    let paths: Vec<String> = (0..12).map(|file| format!("file-{file}.ts")).collect();
    let read = read_slice(&paths, MAX_FILE_DIFF_EDITS / 4, 1, SliceOptions::default()).await;
    assert!(read.slice.patch.len() < MAX_DIFF_SLICE_BYTES);
    assert_eq!(patched_paths(&read.slice.patch), paths[..5].to_vec());
    assert_eq!(read.slice.next_cursor.as_deref(), Some("3:5"));
}

#[tokio::test]
async fn carries_a_whole_new_file_and_stops_the_slice_on_what_it_weighed() {
    let paths = names(&["new.ts", "b.ts", "c.ts", "d.ts"]);
    let read = read_slice(
        &paths,
        8_000,
        30,
        SliceOptions {
            created: &["new.ts"],
            ..SliceOptions::default()
        },
    )
    .await;
    assert_eq!(patched_paths(&read.slice.patch), names(&["new.ts"]));
    assert!(read.slice.patch.contains("--- /dev/null"));
    assert!(read.slice.patch.contains("@@ -0,0 +1,8000 @@"));
    assert!(read.slice.patch.len() > MAX_DIFF_SLICE_BYTES);
    assert_eq!(read.slice.next_cursor.as_deref(), Some("3:1"));
}

#[tokio::test]
async fn carries_on_from_where_the_last_slice_stopped() {
    let paths = names(&["a.ts", "b.ts", "c.ts", "d.ts", "e.ts", "f.ts"]);
    let read = read_slice(
        &paths,
        100,
        900,
        SliceOptions {
            cursor: Some("3:2"),
            ..SliceOptions::default()
        },
    )
    .await;
    assert_eq!(patched_paths(&read.slice.patch), names(&["c.ts", "d.ts"]));
    assert_eq!(read.slice.next_cursor.as_deref(), Some("3:4"));
}

#[tokio::test]
async fn keeps_the_pull_request_being_read_not_the_one_looked_up_first() {
    const HOT: i64 = 7;
    let reads_of: Arc<Mutex<HashMap<i64, usize>>> = Arc::default();
    let counted = reads_of.clone();
    let mut mock = base_mock(names(&["a.ts"]), BTreeSet::new());
    mock.get_pull_request = Some(Box::new(move |(_, number)| {
        *counted.lock().unwrap().entry(number).or_default() += 1;
        async move { Ok(pull_request(number)) }.boxed()
    }));
    mock.read_item_content = Some(Box::new(|_| {
        async {
            Ok(AzureDevOpsItemContent {
                contents: side("new", 2, 4),
                is_binary: false,
            })
        }
        .boxed()
    }));
    let provider = AzureDevOpsPullRequestProvider::with_cli(Arc::new(mock));
    provider.get_diff(diff_input(HOT, None)).await.unwrap();
    // A cache's worth of cold pull requests, with the open one read in between each of them.
    for filled in 0..LOCATION_CACHE_CAPACITY as i64 {
        provider.get_diff(diff_input(HOT + 1 + filled, None)).await.unwrap();
        provider.get_diff(diff_input(HOT, None)).await.unwrap();
    }
    assert_eq!(reads_of.lock().unwrap().get(&HOT), Some(&1));
}
