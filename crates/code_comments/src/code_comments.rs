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
pub use comment_sync::{CodeReviewProvider, CodeReviewRegistry, RepoContext};
pub use comment_settings::{CodeCommentsSettings, CommentSyncSettings};
pub use persistence::CommentsDb;

use gpui::{App, actions};
use settings::Settings as _;

actions!(
    code_comments,
    [
        /// Adds an inline comment on the selected line or range.
        AddComment,
        /// Sends every open task-kind comment to the active agent thread.
        SendTasksToAgent,
    ]
);

/// Initializes the code-comments feature: settings registration and the
/// per-editor comment-card rendering. Concrete code-review providers
/// register themselves with [`CodeReviewRegistry`].
pub fn init(cx: &mut App) {
    CodeCommentsSettings::register(cx);
    inline_comments::init(cx);
}
