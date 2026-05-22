//! Persistent, nested inline code comments for Zed.
//!
//! This crate owns the comment data model ([`CommentThread`] / [`CommentNode`]),
//! its workspace-local sqlite persistence ([`CommentsDb`]), the per-editor
//! comment-card block rendering, and—​in later phases—​agent integration and
//! remote sync.

mod agent_integration;
mod comment_card;
mod comment_store;
mod inline_comments;
mod persistence;
mod registry;

pub use comment_card::CommentCard;
pub use comment_store::{
    CommentAnchor, CommentAuthor, CommentId, CommentKind, CommentNode, CommentStatus, CommentStore,
    CommentStoreEvent, CommentThread, ThreadId,
};
pub use persistence::CommentsDb;

use gpui::{App, actions};

actions!(
    code_comments,
    [
        /// Adds an inline comment on the selected line or range.
        AddComment,
        /// Sends every open task-kind comment to the active agent thread.
        SendTasksToAgent
    ]
);

/// Initializes the code-comments feature: the `AddComment` action and the
/// per-editor comment-card rendering. Settings and remote sync are registered
/// here as later phases land.
pub fn init(cx: &mut App) {
    inline_comments::init(cx);
}
