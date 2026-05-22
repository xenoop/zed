//! Persistent, nested inline code comments for Zed.
//!
//! This crate owns the comment data model ([`CommentThread`] / [`CommentNode`]),
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
mod sync_custom;
mod sync_github;

pub use comment_card::CommentCard;
pub use comment_store::{
    CommentAnchor, CommentAuthor, CommentId, CommentKind, CommentNode, CommentStatus, CommentStore,
    CommentStoreEvent, CommentThread, ThreadId,
};
pub use comment_sync::{
    CommentSyncProvider, CommentSyncRegistry, OutgoingComment, RemoteComment, RepoContext,
    ReviewUnit,
};
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
        /// Syncs comments with the configured remote review provider.
        SyncComments
    ]
);

/// Initializes the code-comments feature: settings, the built-in GitHub sync
/// provider, and the per-editor comment-card rendering and actions.
pub fn init(cx: &mut App) {
    CodeCommentsSettings::register(cx);
    CommentSyncRegistry::register(cx, std::sync::Arc::new(sync_github::GitHubSyncProvider));
    inline_comments::init(cx);
}
