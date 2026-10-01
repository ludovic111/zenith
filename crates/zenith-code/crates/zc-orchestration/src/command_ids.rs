//! Server-side command id conventions (plan §5.2). They are persisted in
//! `orchestration_events.command_id` and read back: the event store infers `actor_kind` from
//! the `server:` / `provider:` prefixes, and migration 046 matches
//! `server:auto-settle:%`. Every producer formats them through these helpers.

use zc_contracts::CommandId;
use zc_core::ids::uuid_v4;

/// `server:<tag>:<uuid>`: a command the server dispatches on its own behalf (`ws.ts`
/// `serverCommandId`, `CheckpointReactor`, `ProviderCommandReactor`).
pub fn server(tag: &str) -> CommandId {
    CommandId::new(format!("server:{tag}:{}", uuid_v4()))
}

/// `server:<tag>:<threadId>:<uuid>`: a server command about one thread (MCP toolkits).
pub fn server_for_thread(tag: &str, thread_id: &str) -> CommandId {
    CommandId::new(format!("server:{tag}:{thread_id}:{}", uuid_v4()))
}

/// `server:auto-settle:<threadId>:<uuid>` (`ThreadSettlementReactor`; migration 046 depends on it).
pub fn auto_settle(thread_id: &str) -> CommandId {
    server_for_thread("auto-settle", thread_id)
}

/// `server:pr-sync:<threadId>:<uuid>` (`PullRequestSyncReactor`).
pub fn pr_sync(thread_id: &str) -> CommandId {
    server_for_thread("pr-sync", thread_id)
}

/// `server:pr-stack-link:<threadId>:<uuid>` (`PullRequestSyncReactor`).
pub fn pr_stack_link(thread_id: &str) -> CommandId {
    server_for_thread("pr-stack-link", thread_id)
}

/// `server:thread-pull-request:<threadId>:<uuid>` (`ThreadPullRequestReactor`).
pub fn thread_pull_request(thread_id: &str) -> CommandId {
    server_for_thread("thread-pull-request", thread_id)
}

/// `provider:<runtimeEventId>:<tag>:<uuid>`: a command derived from a provider runtime event
/// (`ProviderRuntimeIngestion`).
pub fn provider(runtime_event_id: &str, tag: &str) -> CommandId {
    CommandId::new(format!("provider:{runtime_event_id}:{tag}:{}", uuid_v4()))
}

/// `provider:source-proposed-plan-implemented:<implementationThreadId>:<uuid>`
/// (`ProviderRuntimeIngestion`, marking a source plan implemented).
pub fn source_proposed_plan_implemented(implementation_thread_id: &str) -> CommandId {
    CommandId::new(format!("provider:source-proposed-plan-implemented:{implementation_thread_id}:{}", uuid_v4()))
}

/// `session-stop-for-archive:<archiveCommandId>` (`ws.ts`): deterministic, so a retried
/// archive replays the same stop receipt.
pub fn session_stop_for_archive(archive_command_id: &str) -> CommandId {
    CommandId::new(format!("session-stop-for-archive:{archive_command_id}"))
}

/// `session-stop-for-settle:<commandId ?? eventId>` (`ProviderCommandReactor`).
pub fn session_stop_for_settle(settle_command_or_event_id: &str) -> CommandId {
    CommandId::new(format!("session-stop-for-settle:{settle_command_or_event_id}"))
}

/// A plain client-style command id (a bare UUID), as the CLI and startup use.
pub fn plain() -> CommandId {
    CommandId::new(uuid_v4())
}

/// The actor that a command id implies (`inferActorKind`'s prefix rule).
pub fn is_server(command_id: &str) -> bool {
    command_id.starts_with("server:")
}

pub fn is_provider(command_id: &str) -> bool {
    command_id.starts_with("provider:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zc_core::ids::is_canonical_uuid;

    #[test]
    fn ids_follow_the_persisted_conventions() {
        let id = auto_settle("thread-1");
        let rest = id.as_str().strip_prefix("server:auto-settle:thread-1:").unwrap();
        assert!(is_canonical_uuid(rest));
        assert!(is_server(server("thread-turn-start").as_str()));
        assert!(provider("evt", "session-set").as_str().starts_with("provider:evt:session-set:"));
        assert!(is_provider(source_proposed_plan_implemented("t").as_str()));
        assert_eq!(session_stop_for_archive("cmd").as_str(), "session-stop-for-archive:cmd");
        assert_eq!(session_stop_for_settle("cmd").as_str(), "session-stop-for-settle:cmd");
        assert!(pr_sync("t").as_str().starts_with("server:pr-sync:t:"));
        assert!(pr_stack_link("t").as_str().starts_with("server:pr-stack-link:t:"));
        assert!(thread_pull_request("t").as_str().starts_with("server:thread-pull-request:t:"));
        assert!(is_canonical_uuid(plain().as_str()));
    }
}
