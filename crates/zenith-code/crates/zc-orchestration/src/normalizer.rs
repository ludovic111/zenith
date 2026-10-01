//! `orchestration/Normalizer.ts`: turns a client command into the engine's command before
//! dispatch — server timestamps, normalized workspace roots, and attachments persisted under
//! the attachments directory (inline images written, pending uploads claimed by copy).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::engine::{GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use zc_contracts::*;

use crate::attachments::{
    create_attachment_id, parse_base64_data_url, parse_thread_segment_from_attachment_id, plan_attachment_claim, resolve_attachment_path, AttachmentClaimPlan,
    PENDING_ATTACHMENT_THREAD_SEGMENT,
};
use crate::support::js_trim;

/// `PROVIDER_SEND_TURN_MAX_ATTACHMENTS`.
pub const PROVIDER_SEND_TURN_MAX_ATTACHMENTS: usize = 100;
/// `PROVIDER_SEND_TURN_MAX_IMAGE_BYTES`.
pub const PROVIDER_SEND_TURN_MAX_IMAGE_BYTES: i64 = 10 * 1024 * 1024;
const PROVIDER_SEND_TURN_MAX_TOTAL_IMAGE_BYTES: i64 = 80 * 1024 * 1024;
const PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES: &[&str] = &["image/gif", "image/jpeg", "image/png", "image/webp"];

/// `OrchestrationDispatchCommandError` with a message.
pub fn dispatch_command_error(message: impl Into<String>) -> OrchestrationDispatchCommandError {
    OrchestrationDispatchCommandError {
        tag: LitOrchestrationDispatchCommandError,
        message: message.into(),
        cause: None,
        bootstrap_thread_disposition: None,
    }
}

type NormalizeResult<T> = Result<T, OrchestrationDispatchCommandError>;

/// The facts of an attachment the limit check reads (`Pick<ChatAttachment, "type" | "mimeType" | "sizeBytes">`).
#[derive(Debug, Clone)]
struct AttachmentFacts {
    is_image_type: bool,
    mime_type: String,
    size_bytes: i64,
}

/// `getProviderAttachmentLimitError`.
fn provider_attachment_limit_error(attachments: &[AttachmentFacts]) -> Option<String> {
    if attachments.len() > PROVIDER_SEND_TURN_MAX_ATTACHMENTS {
        return Some(format!(
            "You can attach up to {PROVIDER_SEND_TURN_MAX_ATTACHMENTS} files per message or question response."
        ));
    }
    let image_bytes: i64 = attachments
        .iter()
        .filter(|attachment| attachment.is_image_type || PROVIDER_SEND_TURN_SUPPORTED_IMAGE_MIME_TYPES.contains(&attachment.mime_type.to_lowercase().as_str()))
        .map(|attachment| attachment.size_bytes)
        .sum();
    if image_bytes > PROVIDER_SEND_TURN_MAX_TOTAL_IMAGE_BYTES {
        return Some("Images can total up to 80 MiB per message or question response. Use smaller images or send fewer at once.".into());
    }
    None
}

/// An attachment as the client sent it: an inline image upload, or a reference to a stored one.
#[derive(Debug, Clone)]
enum ClientAttachment {
    Upload(ClientOrchestrationCommandThreadTurnStartMessageAttachmentsItemImage),
    Stored(ChatAttachment),
}

impl ClientAttachment {
    fn facts(&self) -> AttachmentFacts {
        match self {
            Self::Upload(upload) => AttachmentFacts {
                is_image_type: true,
                mime_type: upload.mime_type.clone(),
                size_bytes: upload.size_bytes,
            },
            Self::Stored(stored) => chat_attachment_facts(stored),
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Upload(upload) => &upload.name,
            Self::Stored(stored) => chat_attachment_parts(stored).0,
        }
    }

    fn client_id(&self) -> Option<&str> {
        match self {
            Self::Upload(upload) => upload.id.as_deref(),
            Self::Stored(stored) => Some(chat_attachment_parts(stored).1),
        }
    }
}

fn chat_attachment_facts(attachment: &ChatAttachment) -> AttachmentFacts {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => AttachmentFacts {
            is_image_type: true,
            mime_type: image.mime_type.clone(),
            size_bytes: image.size_bytes,
        },
        ChatAttachment::ChatFileAttachment(file) => AttachmentFacts {
            is_image_type: false,
            mime_type: file.mime_type.clone(),
            size_bytes: file.size_bytes,
        },
        ChatAttachment::ChatUnknownAttachment(unknown) => AttachmentFacts {
            is_image_type: unknown.r#type == "image",
            mime_type: unknown.mime_type.clone(),
            size_bytes: unknown.size_bytes,
        },
    }
}

/// `(name, id)` of a stored attachment.
fn chat_attachment_parts(attachment: &ChatAttachment) -> (&str, &str) {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => (&image.name, &image.id),
        ChatAttachment::ChatFileAttachment(file) => (&file.name, &file.id),
        ChatAttachment::ChatUnknownAttachment(unknown) => (&unknown.name, &unknown.id),
    }
}

fn with_id_and_mime(attachment: &ChatAttachment, id: &str) -> ChatAttachment {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => ChatAttachment::ChatImageAttachment(ChatImageAttachment {
            id: id.to_owned(),
            mime_type: image.mime_type.to_lowercase(),
            ..image.clone()
        }),
        ChatAttachment::ChatFileAttachment(file) => ChatAttachment::ChatFileAttachment(ChatFileAttachment {
            id: id.to_owned(),
            mime_type: file.mime_type.to_lowercase(),
            ..file.clone()
        }),
        ChatAttachment::ChatUnknownAttachment(unknown) => ChatAttachment::ChatUnknownAttachment(ChatUnknownAttachment {
            id: id.to_owned(),
            mime_type: unknown.mime_type.to_lowercase(),
            ..unknown.clone()
        }),
    }
}

fn to_question_attachment(attachment: ChatAttachment) -> Option<ChatImageAttachmentOrChatFileAttachment> {
    match attachment {
        ChatAttachment::ChatImageAttachment(image) => Some(ChatImageAttachmentOrChatFileAttachment::ChatImageAttachment(image)),
        ChatAttachment::ChatFileAttachment(file) => Some(ChatImageAttachmentOrChatFileAttachment::ChatFileAttachment(file)),
        ChatAttachment::ChatUnknownAttachment(_) => None,
    }
}

fn from_question_attachment(attachment: &ChatImageAttachmentOrChatFileAttachment) -> ChatAttachment {
    match attachment {
        ChatImageAttachmentOrChatFileAttachment::ChatImageAttachment(image) => ChatAttachment::ChatImageAttachment(image.clone()),
        ChatImageAttachmentOrChatFileAttachment::ChatFileAttachment(file) => ChatAttachment::ChatFileAttachment(file.clone()),
    }
}

/// `canonicalizeClientCommandTimestamps`: the server's receipt time replaces every client
/// `createdAt` (including a bootstrap thread's).
pub fn canonicalize_client_command_timestamps(command: ClientOrchestrationCommand, received_at: &str) -> ClientOrchestrationCommand {
    use ClientOrchestrationCommand as C;
    let at = received_at.to_owned();
    match command {
        C::ProjectCreateCommand(c) => C::ProjectCreateCommand(ProjectCreateCommand { created_at: at, ..c }),
        C::ThreadCreate(c) => C::ThreadCreate(ClientOrchestrationCommandThreadCreate { created_at: at, ..c }),
        C::ThreadRuntimeModeSet(c) => C::ThreadRuntimeModeSet(ClientOrchestrationCommandThreadRuntimeModeSet { created_at: at, ..c }),
        C::ThreadInteractionModeSet(c) => C::ThreadInteractionModeSet(ClientOrchestrationCommandThreadInteractionModeSet { created_at: at, ..c }),
        C::ThreadTurnStart(mut c) => {
            c.created_at = at.clone();
            if let Some(create_thread) = c.bootstrap.as_mut().and_then(|bootstrap| bootstrap.create_thread.as_mut()) {
                create_thread.created_at = at;
            }
            C::ThreadTurnStart(c)
        }
        C::ThreadTurnInterrupt(c) => C::ThreadTurnInterrupt(ClientOrchestrationCommandThreadTurnInterrupt { created_at: at, ..c }),
        C::ThreadApprovalRespond(c) => C::ThreadApprovalRespond(ClientOrchestrationCommandThreadApprovalRespond { created_at: at, ..c }),
        C::ThreadUserInputRespond(c) => C::ThreadUserInputRespond(ClientOrchestrationCommandThreadUserInputRespond { created_at: at, ..c }),
        C::ThreadUserInputDismiss(c) => C::ThreadUserInputDismiss(ClientOrchestrationCommandThreadUserInputDismiss { created_at: at, ..c }),
        C::ThreadCheckpointRevert(c) => C::ThreadCheckpointRevert(ClientOrchestrationCommandThreadCheckpointRevert { created_at: at, ..c }),
        C::ThreadConversationRevert(c) => C::ThreadConversationRevert(ClientOrchestrationCommandThreadConversationRevert { created_at: at, ..c }),
        C::ThreadSessionStop(c) => C::ThreadSessionStop(ClientOrchestrationCommandThreadSessionStop { created_at: at, ..c }),
        other => other,
    }
}

/// `WorkspacePaths.normalizeWorkspaceRoot`: the absolute, home-expanded root, which must be an
/// existing directory (created first when asked).
pub fn normalize_workspace_root(workspace_root: &str, create_if_missing: bool) -> NormalizeResult<String> {
    let expanded = zc_core::paths::expand_home_path(js_trim(workspace_root));
    let normalized = zc_core::paths::resolve_path(&expanded);
    let shown = normalized.to_string_lossy().into_owned();
    let stat = |phase: &str| -> NormalizeResult<Option<std::fs::Metadata>> {
        match std::fs::metadata(&normalized) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(dispatch_command_error(format!("Failed to stat workspace root '{shown}' during '{phase}'."))),
        }
    };
    let mut metadata = stat("validate-existing")?;
    if metadata.is_none() && create_if_missing {
        std::fs::create_dir_all(&normalized).map_err(|_| dispatch_command_error(format!("Failed to create workspace root: {shown}")))?;
        metadata = stat("verify-created")?;
    }
    let Some(metadata) = metadata else {
        return Err(dispatch_command_error(format!("Workspace root does not exist: {shown}")));
    };
    if !metadata.is_dir() {
        return Err(dispatch_command_error(format!("Workspace root is not a directory: {shown}")));
    }
    Ok(shown)
}

fn remove_claimed_attachment_paths(paths: &[PathBuf]) {
    for path in paths {
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(attachment_path = %path.display(), %error, "Failed to remove an unclaimed attachment copy.");
            }
        }
    }
}

fn base64_engine() -> GeneralPurpose {
    GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    )
}

/// Persists the attachments of a turn or question answer, in order. Returns the stored
/// attachments, and the final id of each attachment the client named.
fn persist_attachments(
    attachments_dir: &Path,
    thread_id: &str,
    attachments: &[ClientAttachment],
    claimed: &mut Vec<PathBuf>,
    final_ids: &mut HashMap<String, String>,
) -> NormalizeResult<Vec<ChatAttachment>> {
    let mut with_decoded_sizes: Vec<AttachmentFacts> = attachments.iter().map(ClientAttachment::facts).collect();
    let mut normalized = Vec::with_capacity(attachments.len());
    for (index, attachment) in attachments.iter().enumerate() {
        let name = attachment.name().to_owned();
        match attachment {
            ClientAttachment::Stored(stored) => {
                let (_, client_id) = chat_attachment_parts(stored);
                let (final_id, current_path, final_path) = match plan_attachment_claim(attachments_dir, thread_id, client_id) {
                    AttachmentClaimPlan::Ok {
                        final_id,
                        current_path,
                        final_path,
                    } => (final_id, current_path, final_path),
                    AttachmentClaimPlan::Rejected { reason } => {
                        return Err(dispatch_command_error(format!("Attachment '{name}' cannot be sent: {reason}.")));
                    }
                };
                let size = std::fs::metadata(&current_path)
                    .map_err(|_| dispatch_command_error(format!("Attachment '{name}' cannot be sent: attachment not found.")))?
                    .len();
                if size as i64 != chat_attachment_facts(stored).size_bytes {
                    return Err(dispatch_command_error(format!(
                        "Attachment '{name}' cannot be sent: stored size does not match."
                    )));
                }
                let normalized_attachment = with_id_and_mime(stored, &final_id);
                if resolve_attachment_path(attachments_dir, &normalized_attachment).as_ref() != Some(&final_path) {
                    return Err(dispatch_command_error(format!(
                        "Attachment '{name}' cannot be sent: attachment type does not match the upload."
                    )));
                }
                // Keep the pending copy until the turn succeeds (a failed bootstrap retries with
                // a fresh thread id). A copy, not a link: an agent editing the delivered file
                // must not change the retry source.
                std::fs::copy(&current_path, &final_path)
                    .map_err(|_| dispatch_command_error(format!("Failed to claim attachment '{name}' for this thread.")))?;
                claimed.push(final_path);
                final_ids.insert(client_id.to_owned(), final_id);
                normalized.push(normalized_attachment);
            }
            ClientAttachment::Upload(upload) => {
                let parsed = parse_base64_data_url(&upload.data_url).filter(|parsed| parsed.mime_type.starts_with("image/"));
                let Some(parsed) = parsed else {
                    return Err(dispatch_command_error(format!("Invalid image attachment payload for '{name}'.")));
                };
                let bytes = base64_engine()
                    .decode(parsed.base64.as_bytes())
                    .map_err(|_| dispatch_command_error(format!("Invalid image attachment payload for '{name}'.")))?;
                if bytes.is_empty() || bytes.len() as i64 > PROVIDER_SEND_TURN_MAX_IMAGE_BYTES {
                    return Err(dispatch_command_error(format!("Image attachment '{name}' is empty or too large.")));
                }
                let Some(attachment_id) = create_attachment_id(thread_id, None) else {
                    return Err(dispatch_command_error("Failed to create a safe attachment id."));
                };
                let persisted = ChatAttachment::ChatImageAttachment(ChatImageAttachment {
                    r#type: LitImage,
                    id: attachment_id.clone(),
                    name: upload.name.clone(),
                    mime_type: parsed.mime_type.to_lowercase(),
                    size_bytes: bytes.len() as i64,
                    source: upload.source.clone(),
                });
                with_decoded_sizes[index] = chat_attachment_facts(&persisted);
                if let Some(error) = provider_attachment_limit_error(&with_decoded_sizes) {
                    return Err(dispatch_command_error(error));
                }
                let Some(path) = resolve_attachment_path(attachments_dir, &persisted) else {
                    return Err(dispatch_command_error(format!("Failed to resolve persisted path for '{name}'.")));
                };
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|_| dispatch_command_error(format!("Failed to create attachment directory for '{name}'.")))?;
                }
                std::fs::write(&path, &bytes).map_err(|_| dispatch_command_error(format!("Failed to persist attachment '{name}'.")))?;
                claimed.push(path);
                if let Some(client_id) = &upload.id {
                    final_ids.insert(client_id.clone(), attachment_id);
                }
                normalized.push(persisted);
            }
        }
    }
    Ok(normalized)
}

/// `normalizeDispatchCommand`: the command the engine dispatches. `received_at` is the
/// server's receipt time (`DateTime.now`); `attachments_dir` is `<stateDir>/attachments`.
pub fn normalize_dispatch_command(command: ClientOrchestrationCommand, received_at: &str, attachments_dir: &Path) -> NormalizeResult<OrchestrationCommand> {
    use ClientOrchestrationCommand as C;
    let command = canonicalize_client_command_timestamps(command, received_at);
    match command {
        C::ProjectCreateCommand(c) => {
            let workspace_root = normalize_workspace_root(&c.workspace_root, c.create_workspace_root_if_missing == Some(true))?;
            Ok(OrchestrationCommand::ProjectCreateCommand(ProjectCreateCommand {
                workspace_root,
                create_workspace_root_if_missing: Some(c.create_workspace_root_if_missing == Some(true)),
                ..c
            }))
        }
        C::ProjectMetaUpdate(c) if c.workspace_root.is_some() => {
            let workspace_root = normalize_workspace_root(c.workspace_root.as_deref().unwrap_or_default(), false)?;
            Ok(OrchestrationCommand::ClientOrchestrationCommandProjectMetaUpdate(
                ClientOrchestrationCommandProjectMetaUpdate {
                    workspace_root: Some(workspace_root),
                    ..c
                },
            ))
        }
        C::ThreadUserInputRespond(c) => {
            let attachments: Vec<ClientAttachment> = c
                .attachments_by_question_id
                .iter()
                .flat_map(|by_question| by_question.values().flatten())
                .map(|attachment| ClientAttachment::Stored(from_question_attachment(attachment)))
                .collect();
            if let Some(error) = provider_attachment_limit_error(&attachments.iter().map(ClientAttachment::facts).collect::<Vec<_>>()) {
                return Err(dispatch_command_error(error));
            }
            let mut claimed = Vec::new();
            let mut final_ids = HashMap::new();
            let normalized = persist_attachments(attachments_dir, c.thread_id.as_str(), &attachments, &mut claimed, &mut final_ids)
                .inspect_err(|_| remove_claimed_attachment_paths(&claimed))?;
            if attachments.is_empty() {
                return Ok(OrchestrationCommand::ClientOrchestrationCommandThreadUserInputRespond(c));
            }
            let mut stored = normalized.into_iter();
            let by_question = c.attachments_by_question_id.as_ref().map(|by_question| {
                by_question
                    .iter()
                    .map(|(question_id, original)| {
                        let claimed: Vec<ChatImageAttachmentOrChatFileAttachment> =
                            stored.by_ref().take(original.len()).filter_map(to_question_attachment).collect();
                        (question_id.clone(), claimed)
                    })
                    .collect()
            });
            Ok(OrchestrationCommand::ClientOrchestrationCommandThreadUserInputRespond(
                ClientOrchestrationCommandThreadUserInputRespond {
                    attachments_by_question_id: by_question,
                    ..c
                },
            ))
        }
        C::ThreadTurnStart(c) => {
            let attachments: Vec<ClientAttachment> = c
                .message
                .attachments
                .iter()
                .map(|attachment| match attachment {
                    ClientOrchestrationCommandThreadTurnStartMessageAttachmentsItem::Image(upload) => ClientAttachment::Upload(upload.clone()),
                    ClientOrchestrationCommandThreadTurnStartMessageAttachmentsItem::ChatAttachment(stored) => ClientAttachment::Stored(stored.clone()),
                })
                .collect();
            if let Some(error) = provider_attachment_limit_error(&attachments.iter().map(ClientAttachment::facts).collect::<Vec<_>>()) {
                return Err(dispatch_command_error(error));
            }
            let mut seen = std::collections::HashSet::new();
            for attachment in &attachments {
                let Some(id) = attachment.client_id() else { continue };
                if !seen.insert(id.to_owned()) {
                    return Err(dispatch_command_error(format!(
                        "Attachment '{}' cannot be sent: duplicate attachment id.",
                        attachment.name()
                    )));
                }
            }
            let mut claimed = Vec::new();
            let mut final_ids = HashMap::new();
            let normalized = persist_attachments(attachments_dir, c.thread_id.as_str(), &attachments, &mut claimed, &mut final_ids)
                .inspect_err(|_| remove_claimed_attachment_paths(&claimed))?;
            // Context records bind to attachments by the id the client knew; they follow it.
            let context = c.message.context.map(|mut context| {
                for record in context.records.iter_mut() {
                    match record {
                        ComposerContextRecord::ImageContextRecord(image) => {
                            if let Some(final_id) = final_ids.get(&image.attachment_id) {
                                image.attachment_id = final_id.clone();
                            }
                        }
                        ComposerContextRecord::FileContextRecord(file) => {
                            if let Some(final_id) = final_ids.get(&file.attachment_id) {
                                file.attachment_id = final_id.clone();
                            }
                        }
                        _ => {}
                    }
                }
                context
            });
            Ok(OrchestrationCommand::ThreadTurnStartCommand(ThreadTurnStartCommand {
                r#type: LitThreadTurnStart,
                command_id: c.command_id,
                thread_id: c.thread_id,
                message: ThreadTurnStartCommandMessage {
                    message_id: c.message.message_id,
                    role: c.message.role,
                    text: c.message.text,
                    attachments: normalized,
                    context,
                },
                model_selection: c.model_selection,
                title_seed: c.title_seed,
                runtime_mode: c.runtime_mode,
                interaction_mode: c.interaction_mode,
                bootstrap: c.bootstrap,
                source_proposed_plan: c.source_proposed_plan,
                created_at: c.created_at,
            }))
        }
        other => Ok(crate::command::client_command_into_orchestration(other).unwrap_or_else(|_| unreachable!("turn start is handled above"))),
    }
}

/// `cleanupFailedUploadedAttachments`: after a failed dispatch, removes the thread copies of
/// the pending uploads the normalizer claimed (inline images and non-pending ids are kept).
pub fn cleanup_failed_uploaded_attachments(original: &ClientOrchestrationCommand, normalized: &OrchestrationCommand, attachments_dir: &Path) {
    let originals: Vec<ClientAttachment> = match original {
        ClientOrchestrationCommand::ThreadTurnStart(c) => c
            .message
            .attachments
            .iter()
            .map(|attachment| match attachment {
                ClientOrchestrationCommandThreadTurnStartMessageAttachmentsItem::Image(upload) => ClientAttachment::Upload(upload.clone()),
                ClientOrchestrationCommandThreadTurnStartMessageAttachmentsItem::ChatAttachment(stored) => ClientAttachment::Stored(stored.clone()),
            })
            .collect(),
        ClientOrchestrationCommand::ThreadUserInputRespond(c) => c
            .attachments_by_question_id
            .iter()
            .flat_map(|by_question| by_question.values().flatten())
            .map(|attachment| ClientAttachment::Stored(from_question_attachment(attachment)))
            .collect(),
        _ => Vec::new(),
    };
    let normalized_attachments: Vec<ChatAttachment> = match normalized {
        OrchestrationCommand::ThreadTurnStartCommand(c) => c.message.attachments.clone(),
        OrchestrationCommand::ClientOrchestrationCommandThreadUserInputRespond(c) => c
            .attachments_by_question_id
            .iter()
            .flat_map(|by_question| by_question.values().flatten())
            .map(from_question_attachment)
            .collect(),
        _ => Vec::new(),
    };
    let mut claimed = Vec::new();
    for (index, attachment) in normalized_attachments.iter().enumerate() {
        let Some(ClientAttachment::Stored(original)) = originals.get(index) else {
            continue;
        };
        let original_id = chat_attachment_parts(original).1;
        if parse_thread_segment_from_attachment_id(original_id).as_deref() != Some(PENDING_ATTACHMENT_THREAD_SEGMENT) {
            continue;
        }
        if let Some(path) = resolve_attachment_path(attachments_dir, attachment) {
            claimed.push(path);
        }
    }
    remove_claimed_attachment_paths(&claimed);
}
