//! Accessors for [`OrchestrationCommand`] (the generated union has one struct per member) and
//! the client → server command conversion.

use zc_contracts::*;

macro_rules! commands {
    ($(($variant:ident, $tag:literal)),* $(,)?) => {
        /// The command `type`.
        pub fn command_type(command: &OrchestrationCommand) -> &'static str {
            match command {
                $(OrchestrationCommand::$variant(_) => $tag,)*
            }
        }

        /// The command's `commandId`.
        pub fn command_id(command: &OrchestrationCommand) -> &CommandId {
            match command {
                $(OrchestrationCommand::$variant(c) => &c.command_id,)*
            }
        }
    };
}

commands! {
    (ProjectCreateCommand, "project.create"),
    (ClientOrchestrationCommandProjectMetaUpdate, "project.meta.update"),
    (ClientOrchestrationCommandProjectDelete, "project.delete"),
    (ClientOrchestrationCommandThreadCreate, "thread.create"),
    (ClientOrchestrationCommandThreadDelete, "thread.delete"),
    (ClientOrchestrationCommandThreadArchive, "thread.archive"),
    (ClientOrchestrationCommandThreadUnarchive, "thread.unarchive"),
    (ClientOrchestrationCommandThreadSettle, "thread.settle"),
    (ClientOrchestrationCommandThreadUnsettle, "thread.unsettle"),
    (ClientOrchestrationCommandThreadSnooze, "thread.snooze"),
    (ClientOrchestrationCommandThreadUnsnooze, "thread.unsnooze"),
    (ClientOrchestrationCommandThreadPin, "thread.pin"),
    (ClientOrchestrationCommandThreadUnpin, "thread.unpin"),
    (ClientOrchestrationCommandThreadPinReorder, "thread.pin.reorder"),
    (ClientOrchestrationCommandThreadAutoSettleSet, "thread.auto-settle.set"),
    (ClientOrchestrationCommandThreadActiveReorder, "thread.active.reorder"),
    (ClientOrchestrationCommandThreadMetaUpdate, "thread.meta.update"),
    (ClientOrchestrationCommandThreadPullRequestLink, "thread.pull-request.link"),
    (ClientOrchestrationCommandThreadPullRequestUnlink, "thread.pull-request.unlink"),
    (ClientOrchestrationCommandThreadRuntimeModeSet, "thread.runtime-mode.set"),
    (ClientOrchestrationCommandThreadInteractionModeSet, "thread.interaction-mode.set"),
    (ThreadTurnStartCommand, "thread.turn.start"),
    (ClientOrchestrationCommandThreadTurnInterrupt, "thread.turn.interrupt"),
    (ClientOrchestrationCommandThreadApprovalRespond, "thread.approval.respond"),
    (ClientOrchestrationCommandThreadUserInputRespond, "thread.user-input.respond"),
    (ClientOrchestrationCommandThreadUserInputDismiss, "thread.user-input.dismiss"),
    (ClientOrchestrationCommandThreadCheckpointRevert, "thread.checkpoint.revert"),
    (ClientOrchestrationCommandThreadConversationRevert, "thread.conversation.revert"),
    (ClientOrchestrationCommandThreadSessionStop, "thread.session.stop"),
    (ThreadAutoSettle, "thread.auto-settle"),
    (ThreadPullRequestSync, "thread.pull-request.sync"),
    (ThreadPullRequestLinkSync, "thread.pull-request-link.sync"),
    (ThreadSessionSet, "thread.session.set"),
    (ThreadMessageAssistantDelta, "thread.message.assistant.delta"),
    (ThreadMessageAssistantComplete, "thread.message.assistant.complete"),
    (ThreadMessageReasoningDelta, "thread.message.reasoning.delta"),
    (ThreadMessageReasoningComplete, "thread.message.reasoning.complete"),
    (ThreadHistoryImport, "thread.history.import"),
    (ThreadMessageUserAppend, "thread.message.user.append"),
    (ThreadProposedPlanUpsert, "thread.proposed-plan.upsert"),
    (ThreadTurnDiffComplete, "thread.turn.diff.complete"),
    (ThreadActivityAppend, "thread.activity.append"),
    (ThreadRevertComplete, "thread.revert.complete"),
    (ThreadTitleRegenerationComplete, "thread.title.regeneration.complete"),
    (ThreadTitleGenerateComplete, "thread.title.generate.complete"),
    (ThreadTitleRefine, "thread.title.refine"),
    (ThreadPullRequestSync_, "thread.pull-request.sync"),
    (ThreadPullRequestLinkSync_, "thread.pull-request-link.sync"),
}

/// `commandToAggregateRef`: project commands target their project, every other command its
/// thread.
pub fn aggregate_ref(command: &OrchestrationCommand) -> (OrchestrationAggregateKind, String) {
    match command {
        OrchestrationCommand::ProjectCreateCommand(c) => (OrchestrationAggregateKind::Project, c.project_id.0.clone()),
        OrchestrationCommand::ClientOrchestrationCommandProjectMetaUpdate(c) => (OrchestrationAggregateKind::Project, c.project_id.0.clone()),
        OrchestrationCommand::ClientOrchestrationCommandProjectDelete(c) => (OrchestrationAggregateKind::Project, c.project_id.0.clone()),
        other => (OrchestrationAggregateKind::Thread, thread_id(other).map(|id| id.0.clone()).unwrap_or_default()),
    }
}

/// The `threadId` of a thread command.
pub fn thread_id(command: &OrchestrationCommand) -> Option<&ThreadId> {
    use OrchestrationCommand as C;
    Some(match command {
        C::ProjectCreateCommand(_) | C::ClientOrchestrationCommandProjectMetaUpdate(_) | C::ClientOrchestrationCommandProjectDelete(_) => return None,
        C::ClientOrchestrationCommandThreadCreate(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadDelete(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadArchive(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUnarchive(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadSettle(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUnsettle(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadSnooze(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUnsnooze(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadPin(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUnpin(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadPinReorder(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadAutoSettleSet(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadActiveReorder(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadMetaUpdate(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadPullRequestLink(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadPullRequestUnlink(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadRuntimeModeSet(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadInteractionModeSet(c) => &c.thread_id,
        C::ThreadTurnStartCommand(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadTurnInterrupt(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadApprovalRespond(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUserInputRespond(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadUserInputDismiss(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadCheckpointRevert(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadConversationRevert(c) => &c.thread_id,
        C::ClientOrchestrationCommandThreadSessionStop(c) => &c.thread_id,
        C::ThreadAutoSettle(c) => &c.thread_id,
        C::ThreadPullRequestSync(c) | C::ThreadPullRequestSync_(c) => &c.thread_id,
        C::ThreadPullRequestLinkSync(c) | C::ThreadPullRequestLinkSync_(c) => &c.thread_id,
        C::ThreadSessionSet(c) => &c.thread_id,
        C::ThreadMessageAssistantDelta(c) => &c.thread_id,
        C::ThreadMessageAssistantComplete(c) => &c.thread_id,
        C::ThreadMessageReasoningDelta(c) => &c.thread_id,
        C::ThreadMessageReasoningComplete(c) => &c.thread_id,
        C::ThreadHistoryImport(c) => &c.thread_id,
        C::ThreadMessageUserAppend(c) => &c.thread_id,
        C::ThreadProposedPlanUpsert(c) => &c.thread_id,
        C::ThreadTurnDiffComplete(c) => &c.thread_id,
        C::ThreadActivityAppend(c) => &c.thread_id,
        C::ThreadRevertComplete(c) => &c.thread_id,
        C::ThreadTitleRegenerationComplete(c) => &c.thread_id,
        C::ThreadTitleGenerateComplete(c) => &c.thread_id,
        C::ThreadTitleRefine(c) => &c.thread_id,
    })
}

/// A client command as the engine's union, for members that need no normalization (every
/// one except `thread.turn.start`, whose upload attachments the normalizer persists first).
/// Returns the command back for `thread.turn.start`.
pub fn client_command_into_orchestration(command: ClientOrchestrationCommand) -> Result<OrchestrationCommand, Box<ClientOrchestrationCommandThreadTurnStart>> {
    use ClientOrchestrationCommand as Client;
    use OrchestrationCommand as C;
    Ok(match command {
        Client::ProjectCreateCommand(c) => C::ProjectCreateCommand(c),
        Client::ProjectMetaUpdate(c) => C::ClientOrchestrationCommandProjectMetaUpdate(c),
        Client::ProjectDelete(c) => C::ClientOrchestrationCommandProjectDelete(c),
        Client::ThreadCreate(c) => C::ClientOrchestrationCommandThreadCreate(c),
        Client::ThreadDelete(c) => C::ClientOrchestrationCommandThreadDelete(c),
        Client::ThreadArchive(c) => C::ClientOrchestrationCommandThreadArchive(c),
        Client::ThreadUnarchive(c) => C::ClientOrchestrationCommandThreadUnarchive(c),
        Client::ThreadSettle(c) => C::ClientOrchestrationCommandThreadSettle(c),
        Client::ThreadUnsettle(c) => C::ClientOrchestrationCommandThreadUnsettle(c),
        Client::ThreadSnooze(c) => C::ClientOrchestrationCommandThreadSnooze(c),
        Client::ThreadUnsnooze(c) => C::ClientOrchestrationCommandThreadUnsnooze(c),
        Client::ThreadPin(c) => C::ClientOrchestrationCommandThreadPin(c),
        Client::ThreadUnpin(c) => C::ClientOrchestrationCommandThreadUnpin(c),
        Client::ThreadPinReorder(c) => C::ClientOrchestrationCommandThreadPinReorder(c),
        Client::ThreadAutoSettleSet(c) => C::ClientOrchestrationCommandThreadAutoSettleSet(c),
        Client::ThreadActiveReorder(c) => C::ClientOrchestrationCommandThreadActiveReorder(c),
        Client::ThreadMetaUpdate(c) => C::ClientOrchestrationCommandThreadMetaUpdate(c),
        Client::ThreadPullRequestLink(c) => C::ClientOrchestrationCommandThreadPullRequestLink(c),
        Client::ThreadPullRequestUnlink(c) => C::ClientOrchestrationCommandThreadPullRequestUnlink(c),
        Client::ThreadRuntimeModeSet(c) => C::ClientOrchestrationCommandThreadRuntimeModeSet(c),
        Client::ThreadInteractionModeSet(c) => C::ClientOrchestrationCommandThreadInteractionModeSet(c),
        Client::ThreadTurnStart(c) => return Err(Box::new(c)),
        Client::ThreadTurnInterrupt(c) => C::ClientOrchestrationCommandThreadTurnInterrupt(c),
        Client::ThreadApprovalRespond(c) => C::ClientOrchestrationCommandThreadApprovalRespond(c),
        Client::ThreadUserInputRespond(c) => C::ClientOrchestrationCommandThreadUserInputRespond(c),
        Client::ThreadUserInputDismiss(c) => C::ClientOrchestrationCommandThreadUserInputDismiss(c),
        Client::ThreadCheckpointRevert(c) => C::ClientOrchestrationCommandThreadCheckpointRevert(c),
        Client::ThreadConversationRevert(c) => C::ClientOrchestrationCommandThreadConversationRevert(c),
        Client::ThreadSessionStop(c) => C::ClientOrchestrationCommandThreadSessionStop(c),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_accessors_cover_project_and_thread_commands() {
        let project: OrchestrationCommand = serde_json::from_value(json!({
            "type": "project.delete",
            "commandId": "cmd-1",
            "projectId": "project-1"
        }))
        .unwrap();
        assert_eq!(command_type(&project), "project.delete");
        assert_eq!(command_id(&project).as_str(), "cmd-1");
        assert_eq!(aggregate_ref(&project), (OrchestrationAggregateKind::Project, "project-1".to_owned()));
        let thread: OrchestrationCommand = serde_json::from_value(json!({
            "type": "thread.auto-settle",
            "commandId": "cmd-2",
            "threadId": "thread-1",
            "snapshotSequence": 3,
            "settledAt": "2026-01-01T00:00:00.000Z"
        }))
        .unwrap();
        assert_eq!(command_type(&thread), "thread.auto-settle");
        assert_eq!(aggregate_ref(&thread), (OrchestrationAggregateKind::Thread, "thread-1".to_owned()));
    }
}
