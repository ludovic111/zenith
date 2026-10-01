//! `pullRequestViewedFiles.ts`: the files a reader has ticked off, for a host that keeps no
//! marks of its own (`capabilities.viewedFiles == "environment"`, kept in zc-db's
//! `pull_request_files_viewed`), and the held record of what the head has of those files (the
//! host call behind the **Changed** badge alone).
//!
//! A file still at the revision it was cleared at reads as viewed; one the head has moved off
//! reads as dismissed. Revisions are asked for the marked paths alone, so a reader who has marked
//! nothing costs no host call.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use zc_contracts::{
    PullRequestFileViewed, PullRequestFileViewedState, PullRequestFilesViewedResult, PullRequestRef, PullRequestSetFilesViewedInput,
    PullRequestViewedFilesStore,
};
use zc_db::repos::pull_request_files_viewed::{self as store, FileViewedChange, FilesViewedScope};
use zc_db::DbError;
use zc_sourcecontrol::util::js_trim;

use crate::error::{Cause, PullRequestError};
use crate::provider::{FileRevisionsInput, ProviderFileRevisions, SetFilesViewedInput};
use crate::service::refs::RefInput;
use crate::service::{PullRequestService, SupportedProject};
use crate::util::{lower, OrderedMap};

/// How long the head's version of a file is believed.
pub const FILE_REVISIONS_CACHE_TTL_MS: i64 = 60_000;
/// How long a held answer stands while the next one is fetched.
pub const FILE_REVISIONS_STALE_WINDOW_MS: i64 = 10 * 60_000;
/// How many change requests' revisions are held.
pub const FILE_REVISIONS_CACHE_CAPACITY: usize = 64;
/// How many paths one change request's entry carries (a reader ticking one file after another
/// renews the same entry and would grow it without limit otherwise).
pub const MAX_FILE_REVISION_PATHS: usize = 1_000;

/// What the head has of the files a reader has marked. `asked` tracks what has been asked as
/// well as what was heard: a path missing from an answer keeps its last given version.
#[derive(Debug, Clone, Default)]
struct HeldFileRevisions {
    at: i64,
    asked: OrderedMap<String, ()>,
    revisions: OrderedMap<String, String>,
}

impl HeldFileRevisions {
    fn revisions(&self) -> HashMap<String, String> {
        self.revisions.iter().map(|(path, revision)| (path.clone(), revision.clone())).collect()
    }
}

/// A write gate per change request, and how many presses are queued on it.
type Gates = HashMap<String, (Arc<tokio::sync::Semaphore>, usize)>;

/// The viewed-files state of one service (`make(dependencies)`): only correct at one instance per
/// service, so it lives inside it.
#[derive(Default)]
pub struct ViewedFiles {
    held: Mutex<OrderedMap<String, HeldFileRevisions>>,
    refreshing: Mutex<HashSet<String>>,
    gates: Mutex<Gates>,
}

impl std::fmt::Debug for ViewedFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewedFiles").finish_non_exhaustive()
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `toFilesViewedStoreError(operation)`.
fn store_error(operation: &str, error: DbError) -> PullRequestError {
    let name = match &error {
        DbError::Sql { .. } => "PersistenceSqlError",
        DbError::Decode { .. } => "PersistenceDecodeError",
        _ => "Error",
    };
    PullRequestError::operation(operation, "This environment could not reach its record of which files you have seen.")
        .with_cause(Cause::new(zc_core::defect::Defect::error(name, error.to_string())))
}

/// `filesViewedScope`: which change request's marks, and whose. Provider and host lead because
/// the same repository can exist on more than one install; a host that names no reader is one
/// reader, not none.
fn files_viewed_scope(project: &SupportedProject, number: i64, viewer: Option<String>) -> FilesViewedScope {
    FilesViewedScope {
        provider: project.api.kind().as_str().to_owned(),
        host: project.host.clone(),
        repository: project.remote.clone(),
        number,
        viewer: viewer.unwrap_or_default(),
    }
}

/// Releases a press's place in its change request's queue, however the press ends.
struct GateTicket<'a> {
    gates: &'a Mutex<Gates>,
    key: String,
}

impl Drop for GateTicket<'_> {
    fn drop(&mut self) {
        let mut gates = locked(self.gates);
        if let Some((_, pending)) = gates.get_mut(&self.key) {
            *pending -= 1;
            if *pending == 0 {
                gates.remove(&self.key);
            }
        }
    }
}

impl ViewedFiles {
    /// `fileRevisionsKey`: carries the reference's epoch (spelled from the project, since the
    /// epoch is bumped against the remote's spelling), so whatever moved the head strands what
    /// was held.
    fn file_revisions_key(service: &PullRequestService, project: &SupportedProject, reference: &PullRequestRef) -> String {
        let epoch = service.ref_epoch(&PullRequestRef {
            host: Some(project.host.clone()),
            repository: project.repository.clone(),
            ..reference.clone()
        });
        format!(
            "{epoch} {} {} {} {}",
            service.file_revisions_epoch(),
            reference.project_id.as_str(),
            lower(js_trim(&project.repository)),
            reference.number
        )
    }

    /// `recordFileRevisions`: `paths` are held as answered whether the host had a version for
    /// them or not; a `complete` answer adds every other path it carries (the host read the whole
    /// change to answer for one file).
    fn record(&self, key: &str, paths: &[String], answer: &ProviderFileRevisions, at: i64) -> HashMap<String, String> {
        let mut held = locked(&self.held);
        // Past the stale window the old entry is not worth merging into.
        let carried = held.get(key).filter(|held| at - held.at <= FILE_REVISIONS_STALE_WINDOW_MS).cloned();
        let mut entry = carried.clone().unwrap_or_default();
        let answered: HashMap<&str, &str> = answer.revisions.iter().map(|(path, revision)| (path.as_str(), revision.as_str())).collect();
        // The paths asked for go last, so a whole-change answer wider than the cap is trimmed down
        // to the reader's own files rather than over them.
        let mut learned: Vec<&str> = Vec::new();
        if answer.complete == Some(true) {
            learned.extend(answer.revisions.iter().map(|(path, _)| path.as_str()));
        }
        learned.extend(paths.iter().map(String::as_str));
        for path in learned {
            // Reinserted, so a full entry drops the path nobody has asked about the longest.
            entry.asked.insert_last(path.to_owned(), ());
            // A path left out of the answer keeps its last known version.
            if let Some(revision) = answered.get(path) {
                entry.revisions.insert_last(path.to_owned(), (*revision).to_owned());
            }
        }
        while entry.asked.len() > MAX_FILE_REVISION_PATHS {
            if let Some((path, ())) = entry.asked.pop_first() {
                entry.revisions.remove(&path);
            }
        }
        held.remove(key);
        if held.len() >= FILE_REVISIONS_CACHE_CAPACITY {
            held.pop_first();
        }
        // Only as fresh as its oldest revision: stamping a partial answer with `now` would let
        // an old revision ride past the point it should have been read again.
        entry.at = if entry.revisions.keys().all(|path| answered.contains_key(path.as_str())) {
            at
        } else {
            carried.map_or(at, |carried| carried.at)
        };
        let revisions = entry.revisions();
        held.insert_last(key.to_owned(), entry);
        revisions
    }

    /// `heldFileRevisionsFor`: a held entry covering every path asked for, still worth answering
    /// from. Put back at the end on every read, so the change request a reader is working
    /// through is not the one evicted.
    fn held_for(&self, key: &str, paths: &[String], now: i64) -> Option<(i64, HashMap<String, String>)> {
        let mut held = locked(&self.held);
        held.touch(key);
        let entry = held.get(key)?;
        if now - entry.at > FILE_REVISIONS_STALE_WINDOW_MS {
            return None;
        }
        paths.iter().all(|path| entry.asked.contains_key(path)).then(|| (entry.at, entry.revisions()))
    }

    async fn fetch_file_revisions(
        &self,
        service: &PullRequestService,
        project: &SupportedProject,
        reference: &PullRequestRef,
        paths: &[String],
        operation: &str,
    ) -> Result<HashMap<String, String>, PullRequestError> {
        let key = Self::file_revisions_key(service, project, reference);
        let answer = project
            .api
            .get_file_revisions(FileRevisionsInput {
                change_request: crate::service::reads_change_request(project, reference.number),
                paths: paths.to_vec(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider(operation, error))?;
        Ok(self.record(&key, paths, &answer, service.now()))
    }

    /// `fileRevisionsOf`: what the head has of these files, or `None` where the host cannot say
    /// (the marks just stop reporting staleness). `fresh` is for the press itself, which must not
    /// store a revision the head already moved off; otherwise a stale answer is served while the
    /// next one is fetched, one refresh at a time per change request.
    async fn file_revisions_of(
        &self,
        service: &PullRequestService,
        project: &SupportedProject,
        reference: &PullRequestRef,
        paths: &[String],
        operation: &'static str,
        fresh: bool,
    ) -> Result<Option<HashMap<String, String>>, PullRequestError> {
        if !project.api.optional_methods().get_file_revisions {
            return Ok(None);
        }
        let now = service.now();
        let key = Self::file_revisions_key(service, project, reference);
        let Some((at, revisions)) = self.held_for(&key, paths, now) else {
            return self.fetch_file_revisions(service, project, reference, paths, operation).await.map(Some);
        };
        if now - at <= FILE_REVISIONS_CACHE_TTL_MS {
            return Ok(Some(revisions));
        }
        if fresh {
            return self.fetch_file_revisions(service, project, reference, paths, operation).await.map(Some);
        }
        if !locked(&self.refreshing).insert(key.clone()) {
            return Ok(Some(revisions));
        }
        // Its own task: the caller has been answered and is gone before this lands.
        let service = service.clone();
        let project = project.clone();
        let reference = reference.clone();
        let paths = paths.to_vec();
        tokio::spawn(async move {
            let viewed = &service.inner.viewed_files;
            let _ = viewed.fetch_file_revisions(&service, &project, &reference, &paths, operation).await;
            locked(&viewed.refreshing).remove(&key);
        });
        Ok(Some(revisions))
    }

    /// The marks this environment keeps for a host that keeps none of its own.
    async fn environment_files_viewed(
        &self,
        service: &PullRequestService,
        project: &SupportedProject,
        reference: &PullRequestRef,
    ) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        let viewer = service.required_viewer_of(project, "filesViewed").await?;
        let scope = files_viewed_scope(project, reference.number, viewer);
        let held = service
            .inner
            .db
            .call(move |conn| store::list(conn, &scope))
            .await
            .map_err(|error| store_error("filesViewed", error))?;
        if held.files.is_empty() {
            return Ok(PullRequestFilesViewedResult {
                files: Vec::new(),
                truncated: held.truncated,
            });
        }
        let paths: Vec<String> = held.files.iter().map(|mark| mark.path.clone()).collect();
        // A host that will not say what its head has costs the marks their staleness, not the
        // reader every tick they made.
        let revisions = match self.file_revisions_of(service, project, reference, &paths, "filesViewed", false).await {
            Ok(revisions) => revisions,
            Err(error) => {
                tracing::warn!(
                    operation = "filesViewed",
                    reason = error.tag(),
                    "reporting viewed files without what the head has of them"
                );
                None
            }
        };
        let files = held
            .files
            .into_iter()
            .map(|mark| {
                // A mark stamped with no baseline holds until the reader presses it again; a path
                // the host had no answer for holds as cleared too.
                let state = match &mark.revision {
                    None => PullRequestFileViewedState::Viewed,
                    Some(stamped) => match revisions.as_ref().and_then(|revisions| revisions.get(&mark.path)) {
                        Some(revision) if revision != stamped => PullRequestFileViewedState::Dismissed,
                        _ => PullRequestFileViewedState::Viewed,
                    },
                };
                PullRequestFileViewed { path: mark.path, state }
            })
            .collect();
        Ok(PullRequestFilesViewedResult {
            files,
            truncated: held.truncated,
        })
    }

    async fn environment_set_files_viewed(
        &self,
        service: &PullRequestService,
        project: &SupportedProject,
        input: &PullRequestSetFilesViewedInput,
    ) -> Result<(), PullRequestError> {
        let viewer = service.required_viewer_of(project, "setFilesViewed").await?;
        // Only the files being cleared need a revision: an unticked one is about to lose its row.
        let cleared: Vec<String> = input.files.iter().filter(|file| file.viewed).map(|file| file.path.clone()).collect();
        let revisions = if cleared.is_empty() {
            None
        } else {
            match self
                .file_revisions_of(service, project, &input.reference(), &cleared, "setFilesViewed", true)
                .await
            {
                Ok(revisions) => revisions,
                // The press loses its baseline, not the press: the mark holds until pressed again.
                Err(error) => {
                    tracing::warn!(
                        operation = "setFilesViewed",
                        reason = error.tag(),
                        "recording viewed files without what the head has of them"
                    );
                    None
                }
            }
        };
        let viewed_at = zc_core::time::iso_from_millis(service.now());
        // A path left out of the answer stores with no baseline rather than the empty revision,
        // which is itself an answer (a deleted file).
        let files: Vec<FileViewedChange> = input
            .files
            .iter()
            .map(|file| FileViewedChange {
                path: file.path.clone(),
                revision: revisions.as_ref().and_then(|revisions| revisions.get(&file.path).cloned()),
                viewed: file.viewed,
            })
            .collect();
        let scope = files_viewed_scope(project, input.number, viewer);
        service
            .inner
            .db
            .call(move |conn| store::set(conn, &scope, &files, &viewed_at))
            .await
            .map_err(|error| store_error("setFilesViewed", error))
    }

    /// One environment-kept write at a time per change request: a tick's host round trip is
    /// slower than an untick's, so unordered presses could leave a stale tick over a later untick.
    async fn in_files_viewed_order(
        &self,
        service: &PullRequestService,
        project: &SupportedProject,
        input: &PullRequestSetFilesViewedInput,
    ) -> Result<(), PullRequestError> {
        let key = format!("{} {} {}", project.project.id.as_str(), project.remote, input.number);
        let gate = {
            let mut gates = locked(&self.gates);
            let entry = gates.entry(key.clone()).or_insert_with(|| (Arc::new(tokio::sync::Semaphore::new(1)), 0));
            entry.1 += 1;
            entry.0.clone()
        };
        let _ticket = GateTicket { gates: &self.gates, key };
        let _permit = gate.acquire().await.ok();
        self.environment_set_files_viewed(service, project, input).await
    }

    /// `filesViewed(input)`.
    pub(crate) async fn files_viewed(&self, service: &PullRequestService, input: &PullRequestRef) -> Result<PullRequestFilesViewedResult, PullRequestError> {
        let project = service.require_project(input).await?;
        let store_kind = project.api.capabilities().viewed_files;
        if store_kind == Some(PullRequestViewedFilesStore::Host) && project.api.optional_methods().get_files_viewed {
            let viewed = project
                .api
                .get_files_viewed(crate::service::reads_change_request(&project, input.number))
                .await
                .map_err(|error| PullRequestError::from_provider("filesViewed", error))?;
            return Ok(PullRequestFilesViewedResult {
                files: viewed.files,
                truncated: viewed.truncated,
            });
        }
        if store_kind == Some(PullRequestViewedFilesStore::Environment) {
            return self.environment_files_viewed(service, &project, input).await;
        }
        Err(PullRequestError::operation(
            "filesViewed",
            "This host does not track which files a reader has seen.",
        ))
    }

    /// `setFilesViewed(input)`.
    pub(crate) async fn set_files_viewed(&self, service: &PullRequestService, input: &PullRequestSetFilesViewedInput) -> Result<(), PullRequestError> {
        let project = service.require_project(&input.reference()).await?;
        let store_kind = project.api.capabilities().viewed_files;
        if store_kind == Some(PullRequestViewedFilesStore::Host) && project.api.optional_methods().set_files_viewed {
            return project
                .api
                .set_files_viewed(SetFilesViewedInput {
                    change_request: crate::service::reads_change_request(&project, input.number),
                    files: input.files.iter().map(|file| (file.path.clone(), file.viewed)).collect(),
                })
                .await
                .map_err(|error| PullRequestError::from_provider("setFilesViewed", error));
        }
        if store_kind == Some(PullRequestViewedFilesStore::Environment) {
            return self.in_files_viewed_order(service, &project, input).await;
        }
        Err(PullRequestError::operation(
            "setFilesViewed",
            "This host does not track which files a reader has seen.",
        ))
    }
}
