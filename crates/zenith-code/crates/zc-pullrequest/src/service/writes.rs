//! The writes. Each is refused first for what the host cannot do (free), then for what this
//! account may not do, asked of the host itself (a request), and only then handed to the
//! provider: the page hides what a viewer may not do, and a request that arrived without passing
//! through the page must not be taken on the client's word.

use serde_json::json;
use zc_contracts::{
    PullRequestAction, PullRequestActionInput, PullRequestCommentInput, PullRequestCommentUpdateInput, PullRequestLabelCandidateList,
    PullRequestLabelChangeInput, PullRequestReactionInput, PullRequestRef, PullRequestReviewVerdict, PullRequestReviewerCandidateList,
    PullRequestReviewerRequestInput, PullRequestState, PullRequestSubmitReviewInput, PullRequestThreadReplyInput, PullRequestThreadResolutionInput,
    PullRequestUpdateInput, PullRequestUpdateMethod, PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_sourcecontrol::util::js_trim;

use super::epochs::InvalidateOnDrop;
use super::projects::SupportedProject;
use super::reads::change_request_of;
use super::refs::{ref_scope, RefInput};
use super::PullRequestService;
use crate::error::PullRequestError;
use crate::provider::{
    CommentInput, ReplyToThreadInput, ReviewerRef, RunActionInput, SetLabelsInput, SetReactionInput, SetReviewerRequestInput, SetThreadResolutionInput,
    SubmitReviewInput, UpdateChangeRequestInput, UpdateCommentInput, ViewerPermissionsInput,
};

/// What a verdict is called when refusing it, so the sentence reads as an action.
fn verdict_label(verdict: PullRequestReviewVerdict) -> &'static str {
    match verdict.as_str() {
        "approve" => "approve",
        "request-changes" => "request changes on",
        _ => "review",
    }
}

/// Why an action is refused to this viewer, said as the access it would take. Merging needs
/// write and nothing else; the other four are also the author's to take.
fn action_access_refusal(action: PullRequestAction) -> &'static str {
    match action.as_str() {
        "merge" => "You need write access on this repository to merge.",
        "ready" => "You need write access on this repository, or to have opened this change request, to mark it ready for review.",
        "draft" => "You need write access on this repository, or to have opened this change request, to return it to a draft.",
        "close" => "You need write access on this repository, or to have opened this change request, to close it.",
        "update-branch" => "You need write access on this repository, or to have opened this change request, to update its branch.",
        "reopen" => "You need write access on this repository, or to have opened this change request, to reopen it.",
        "enable-auto-merge" => "You need write access on this repository to have it merged for you once it is ready.",
        "disable-auto-merge" => "You need write access on this repository to stop it being merged for you once it is ready.",
        "revert" => "You need write access on this repository to open a revert pull request.",
        _ => "You need write access on this repository to approve workflows from a fork pull request.",
    }
}

/// Why asking for a review (and the menu behind it) is refused.
const REVIEWER_REQUEST_REFUSAL: &str = "You need write access on this repository to ask for a review.";
const LABEL_CHANGE_REFUSAL: &str = "You need triage access on this repository to change its labels.";

fn refuse(operation: &str, detail: impl Into<String>) -> PullRequestError {
    PullRequestError::operation(operation, detail)
}

impl PullRequestService {
    /// `viewerPermissionsOf`: what the signed-in account may do with this change request, read
    /// freshly from the host for every write.
    async fn viewer_permissions_of(
        &self,
        project: &SupportedProject,
        number: i64,
        operation: &str,
        include_update_branch: bool,
    ) -> Result<PullRequestViewerPermissions, PullRequestError> {
        project
            .api
            .get_viewer_permissions(ViewerPermissionsInput {
                change_request: change_request_of(project, number),
                include_update_branch: Some(include_update_branch),
            })
            .await
            .map_err(|error| PullRequestError::from_provider(operation, error))
    }

    /// `runAction` (before invalidation): the repository the action was taken on.
    async fn run_action_inner(&self, input: &PullRequestActionInput) -> Result<String, PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        let capabilities = project.api.capabilities();
        let action = input.action;
        if input.stack_number.is_some()
            && (capabilities.stack_actions != Some(true)
                || !matches!(action.as_str(), "merge" | "update-branch")
                || input.expected_stack_heads.is_none()
                || (action.as_str() == "update-branch" && input.update_method.map(PullRequestUpdateMethod::as_str) != Some("rebase")))
        {
            return Err(refuse("runAction", "This stack action is not supported or has no expected head revision."));
        }
        // The surface hides what a host cannot do, and this refuses it as well.
        if !capabilities.actions.contains(&action) {
            return Err(refuse("runAction", format!("This host cannot {} a change request.", action.as_str())));
        }
        // A strategy the host does not offer is refused rather than passed on: every provider
        // maps an unrecognised one to its own default.
        if let Some(method) = input.merge_method {
            if !capabilities.merge_methods.contains(&method) {
                return Err(refuse("runAction", format!("This host cannot merge with the {} strategy.", method.as_str())));
            }
        }
        if let Some(method) = input.update_method {
            if !capabilities.update_methods.as_deref().unwrap_or_default().contains(&method) {
                return Err(refuse("runAction", format!("This host cannot update a branch by {}.", method.as_str())));
            }
        }
        // What the host can do and what this account may ask of it both have to say yes; the
        // second costs a request, so it is asked last.
        let viewer = self
            .viewer_permissions_of(&project, input.number, "runAction", action.as_str() == "update-branch")
            .await?;
        let stack_rebase = input.stack_number.is_some() && action.as_str() == "update-branch";
        let allowed = if stack_rebase {
            viewer.stack_rebase == Some(true)
        } else {
            viewer.actions.contains(&action)
        };
        if !allowed {
            return Err(refuse("runAction", action_access_refusal(action)));
        }
        if !stack_rebase {
            if let Some(method) = input.update_method {
                if !viewer.update_methods.as_deref().unwrap_or_default().contains(&method) {
                    return Err(refuse("runAction", action_access_refusal(PullRequestAction::UpdateBranch)));
                }
            }
        }
        let result = project
            .api
            .run_action(RunActionInput {
                change_request: change_request_of(&project, input.number),
                action,
                stack_number: input.stack_number,
                expected_stack_heads: input.expected_stack_heads.clone(),
                merge_method: input.merge_method,
                update_method: input.update_method,
            })
            .await;
        // Once the authorised action starts, a failure may leave partial remote updates; the
        // refusals above changed nothing.
        if input.stack_number.is_some() {
            self.refresh_after_turn_impl(&project.project.id).await;
        }
        result.map_err(|error| PullRequestError::from_provider("runAction", error))?;
        Ok(if project.api.kind() == SourceControlProviderKind::AzureDevops {
            js_trim(&input.repository).to_owned()
        } else {
            project.repository.clone()
        })
    }

    /// `runActionAndInvalidate`: the action, then every reader is refreshed; a merge is published
    /// only once the host confirms it (a merge action can merely enqueue it or arm auto-merge).
    pub(crate) async fn run_action_and_invalidate(&self, input: PullRequestActionInput) -> Result<(), PullRequestError> {
        let canonical = self.canonical_ref(&input.reference()).await?;
        let scope = ref_scope(&canonical);
        self.inner.read_cache.invalidate(&scope).await;
        let guard = InvalidateOnDrop::new(self, scope.clone());
        let result = self.run_action_inner(&input).await;
        guard.disarm();
        self.inner.read_cache.invalidate(&scope).await;
        let repository = result?;
        let listings = self.with_epochs(|epochs| {
            epochs.bump_ref(&PullRequestRef {
                repository: repository.clone(),
                ..canonical.clone()
            });
            epochs.listings = epochs.next();
            epochs.listings
        });
        self.notify_readers(listings);
        if input.action != PullRequestAction::Merge {
            return Ok(());
        }
        let confirmed = match self
            .summary_uncached(&PullRequestRef {
                repository: repository.clone(),
                ..input.reference()
            })
            .await
        {
            Ok(summary) => Some(summary),
            Err(error) => {
                tracing::warn!(error = %error, "failed to confirm pull request merge");
                None
            }
        };
        if confirmed.map(|summary| summary.state) != Some(PullRequestState::Merged) {
            return Ok(());
        }
        let event = zc_ports::pull_requests::PullRequestMergeEvent {
            reference: zc_ports::contracts::PullRequestRef(json!({
                "projectId": input.project_id.as_str(),
                "repository": repository,
                "number": input.number,
            })),
            merged_at: zc_core::time::iso_from_millis(self.now()),
        };
        let _ = self.inner.merges.send(event);
        Ok(())
    }

    /// `comment`. The contract keeps the body verbatim (markdown), so "did the reader write
    /// anything" is checked here.
    pub(crate) async fn comment_impl(&self, input: &PullRequestCommentInput) -> Result<(), PullRequestError> {
        if js_trim(&input.body).is_empty() {
            return Err(refuse("comment", "A comment cannot be empty."));
        }
        let project = self.require_project(&input.reference()).await?;
        if !project.api.capabilities().comment {
            return Err(refuse("comment", "This host cannot post a comment on a change request."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "comment", false).await?;
        if !viewer.comment {
            return Err(refuse("comment", "You need write access on this repository to comment on a change request."));
        }
        project
            .api
            .comment(CommentInput {
                change_request: change_request_of(&project, input.number),
                body: input.body.clone(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider("comment", error))
    }

    /// `update`: rewriting the change request's own words is left to the host to allow or refuse
    /// (every host lets the author rewrite whatever their access, and none reports that as a
    /// permission).
    pub(crate) async fn update_impl(&self, input: &PullRequestUpdateInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        let can_rewrite =
            project.api.capabilities().edit.as_ref().is_some_and(|edit| edit.change_request) && project.api.optional_methods().update_change_request;
        if !can_rewrite {
            return Err(refuse("update", "This host cannot rewrite a change request."));
        }
        if input.title.is_none() && input.body.is_none() {
            return Err(refuse("update", "Nothing was changed."));
        }
        project
            .api
            .update_change_request(UpdateChangeRequestInput {
                change_request: change_request_of(&project, input.number),
                title: input.title.clone(),
                body: input.body.clone(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider("update", error))
    }

    /// `updateComment`: like `update`, left to the host.
    pub(crate) async fn update_comment_impl(&self, input: &PullRequestCommentUpdateInput) -> Result<(), PullRequestError> {
        if js_trim(&input.body).is_empty() {
            return Err(refuse("updateComment", "A comment cannot be empty."));
        }
        let project = self.require_project(&input.reference()).await?;
        let can_rewrite = project.api.capabilities().edit.as_ref().is_some_and(|edit| edit.comment) && project.api.optional_methods().update_comment;
        if !can_rewrite {
            return Err(refuse("updateComment", "This host cannot rewrite a comment."));
        }
        project
            .api
            .update_comment(UpdateCommentInput {
                change_request: change_request_of(&project, input.number),
                comment_id: input.comment_id.clone(),
                kind: input.kind,
                body: input.body.clone(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider("updateComment", error))
    }

    pub(crate) async fn submit_review_impl(&self, input: &PullRequestSubmitReviewInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        let review = &project.api.capabilities().review;
        if !review.verdicts.contains(&input.verdict) {
            return Err(refuse(
                "submitReview",
                format!("This host cannot {} a change request.", verdict_label(input.verdict)),
            ));
        }
        if !input.comments.is_empty() && !review.inline_comment {
            return Err(refuse("submitReview", "This host cannot comment on a line of a change request."));
        }
        // A verdict with nothing attached is a request every host rejects; this says which half
        // is missing rather than reporting the host's refusal.
        if input.verdict != PullRequestReviewVerdict::Approve && js_trim(&input.body).is_empty() && input.comments.is_empty() {
            return Err(refuse("submitReview", "A review needs a summary or at least one comment."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "submitReview", false).await?;
        if !viewer.verdicts.contains(&input.verdict) {
            return Err(refuse(
                "submitReview",
                format!("You need write access on this repository to {} a change request.", verdict_label(input.verdict)),
            ));
        }
        if !input.comments.is_empty() && !viewer.comment {
            return Err(refuse(
                "submitReview",
                "You need write access on this repository to comment on a line of a change request.",
            ));
        }
        project
            .api
            .submit_review(SubmitReviewInput {
                change_request: change_request_of(&project, input.number),
                verdict: input.verdict,
                body: input.body.clone(),
                comments: input.comments.clone(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider("submitReview", error))
    }

    pub(crate) async fn reply_to_thread_impl(&self, input: &PullRequestThreadReplyInput) -> Result<(), PullRequestError> {
        if js_trim(&input.body).is_empty() {
            return Err(refuse("replyToThread", "A reply cannot be empty."));
        }
        let project = self.require_project(&input.reference()).await?;
        if !project.api.capabilities().review.reply {
            return Err(refuse("replyToThread", "This host cannot reply to a review conversation."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "replyToThread", false).await?;
        if !viewer.comment {
            return Err(refuse(
                "replyToThread",
                "You need write access on this repository to reply to a review conversation.",
            ));
        }
        project
            .api
            .reply_to_thread(ReplyToThreadInput {
                change_request: change_request_of(&project, input.number),
                thread_id: input.thread_id.clone(),
                body: input.body.clone(),
            })
            .await
            .map_err(|error| PullRequestError::from_provider("replyToThread", error))
    }

    pub(crate) async fn set_thread_resolution_impl(&self, input: &PullRequestThreadResolutionInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if !project.api.capabilities().review.resolve {
            return Err(refuse("setThreadResolution", "This host cannot resolve a review conversation."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "setThreadResolution", false).await?;
        if !viewer.resolve {
            return Err(refuse(
                "setThreadResolution",
                "You need write access on this repository, or to have opened this change request, to resolve a review conversation.",
            ));
        }
        project
            .api
            .set_thread_resolution(SetThreadResolutionInput {
                change_request: change_request_of(&project, input.number),
                thread_id: input.thread_id.clone(),
                resolved: input.resolved,
            })
            .await
            .map_err(|error| PullRequestError::from_provider("setThreadResolution", error))
    }

    /// `setReaction`: gated on the host alone (every host with reactions takes one from whoever
    /// can read the change request).
    pub(crate) async fn set_reaction_impl(&self, input: &PullRequestReactionInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if project.api.capabilities().reactions != Some(true) {
            return Err(refuse("setReaction", "This host has no reactions."));
        }
        project
            .api
            .set_reaction(SetReactionInput {
                change_request: change_request_of(&project, input.number),
                subject_id: input.subject_id.clone(),
                content: input.content,
                reacted: input.reacted,
            })
            .await
            .map_err(|error| PullRequestError::from_provider("setReaction", error))
    }

    /// `reviewerCandidates`: wanted only by somebody about to ask, so the same permission guards
    /// the menu and the request.
    pub(crate) async fn reviewer_candidates_impl(&self, input: &PullRequestRef) -> Result<PullRequestReviewerCandidateList, PullRequestError> {
        let project = self.require_project(input).await?;
        if !project.api.capabilities().reviewers.list_candidates {
            return Err(refuse("reviewerCandidates", "This host cannot say who may review a change request."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "reviewerCandidates", false).await?;
        if !viewer.request_reviewers {
            return Err(refuse("reviewerCandidates", REVIEWER_REQUEST_REFUSAL));
        }
        project
            .api
            .list_reviewer_candidates(change_request_of(&project, input.number))
            .await
            .map_err(|error| PullRequestError::from_provider("reviewerCandidates", error))
    }

    pub(crate) async fn request_reviewers_impl(&self, input: &PullRequestReviewerRequestInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if !project.api.capabilities().reviewers.request {
            return Err(refuse("requestReviewers", "This host cannot ask somebody for a review."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "requestReviewers", false).await?;
        if !viewer.request_reviewers {
            return Err(refuse("requestReviewers", REVIEWER_REQUEST_REFUSAL));
        }
        project
            .api
            .set_reviewer_request(SetReviewerRequestInput {
                change_request: change_request_of(&project, input.number),
                reviewers: input
                    .reviewers
                    .iter()
                    .map(|reviewer| ReviewerRef {
                        id: reviewer.id.clone(),
                        kind: reviewer.kind,
                    })
                    .collect(),
                requested: input.requested,
            })
            .await
            .map_err(|error| PullRequestError::from_provider("requestReviewers", error))
    }

    /// `labelCandidates`: like the reviewer candidates, guarded by the permission of the change.
    pub(crate) async fn label_candidates_impl(&self, input: &PullRequestRef) -> Result<PullRequestLabelCandidateList, PullRequestError> {
        let project = self.require_project(input).await?;
        if project.api.capabilities().labels != Some(true) || !project.api.optional_methods().list_label_candidates {
            return Err(refuse("labelCandidates", "This host cannot change the labels on a change request."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "labelCandidates", false).await?;
        if viewer.labels == Some(false) {
            return Err(refuse("labelCandidates", LABEL_CHANGE_REFUSAL));
        }
        project
            .api
            .list_label_candidates(change_request_of(&project, input.number))
            .await
            .map_err(|error| PullRequestError::from_provider("labelCandidates", error))
    }

    pub(crate) async fn set_labels_impl(&self, input: &PullRequestLabelChangeInput) -> Result<(), PullRequestError> {
        let project = self.require_project(&input.reference()).await?;
        if project.api.capabilities().labels != Some(true) || !project.api.optional_methods().set_labels {
            return Err(refuse("setLabels", "This host cannot change the labels on a change request."));
        }
        let viewer = self.viewer_permissions_of(&project, input.number, "setLabels", false).await?;
        if viewer.labels == Some(false) {
            return Err(refuse("setLabels", LABEL_CHANGE_REFUSAL));
        }
        project
            .api
            .set_labels(SetLabelsInput {
                change_request: change_request_of(&project, input.number),
                labels: input.labels.clone(),
                applied: input.applied,
            })
            .await
            .map_err(|error| PullRequestError::from_provider("setLabels", error))
    }
}
