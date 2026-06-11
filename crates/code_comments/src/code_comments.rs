//! Persistent, nested inline code comments for Zed.
//!
//! This crate owns the comment data model ([`Conversation`] / [`Comment`]),
//! its workspace-local sqlite persistence ([`CommentsDb`]), the per-editor
//! comment-card block rendering, agent integration, and a pluggable remote
//! sync layer.

mod agent_integration;
mod comment_card;
mod comment_settings;
mod comment_store;
mod comment_sync;
mod inline_comments;
mod persistence;
mod registry;

pub use comment_card::ConversationCard;
pub use comment_store::{
    ChangeId, Comment, CommentAnchor, CommentAuthor, CommentId, CommentKind, CommentStore,
    CommentStoreEvent, Conversation, ConversationId, ConversationStatus, RevisionId,
};
pub use comment_sync::{
    CUSTOM_PROVIDER_NAME, CodeReviewProvider, CodeReviewRegistry, CustomCommands, RepoContext,
};
pub use comment_settings::{CodeCommentsSettings, CommentSyncSettings};
pub use persistence::CommentsDb;

use std::sync::Arc;

use gpui::{App, actions};
use settings::{Settings as _, SettingsStore};

use crate::comment_sync::CustomCommandProvider;

actions!(
    code_comments,
    [
        /// Adds an inline comment on the selected line or range.
        AddComment,
        /// Sends every open task-kind comment to the active agent thread.
        SendTasksToAgent,
    ]
);

/// Initializes the code-comments feature: settings registration, the
/// per-editor comment-card rendering, and the built-in command-driven
/// code-review provider. Other concrete providers register themselves with
/// [`CodeReviewRegistry`].
pub fn init(cx: &mut App) {
    CodeCommentsSettings::register(cx);

    let provider = Arc::new(CustomCommandProvider::default());
    provider.set_commands(custom_commands_from_settings(cx));
    CodeReviewRegistry::register(cx, provider.clone());
    cx.observe_global::<SettingsStore>(move |cx| {
        provider.set_commands(custom_commands_from_settings(cx));
    })
    .detach();

    inline_comments::init(cx);
}

fn custom_commands_from_settings(cx: &App) -> CustomCommands {
    let settings = CodeCommentsSettings::get_global(cx);
    CustomCommands {
        detect: settings.sync.custom_detect_command.clone(),
        fetch: settings.sync.custom_fetch_command.clone(),
    }
}
