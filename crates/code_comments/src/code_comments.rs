//! Persistent, nested inline code comments for Zed.
//!
//! This crate owns the comment data model ([`CommentThread`] / [`CommentNode`]),
//! its workspace-local sqlite persistence ([`CommentsDb`]), and—​in later
//! phases—​the editor block rendering, agent integration, and remote sync.

mod comment_store;
mod persistence;

pub use comment_store::{
    CommentAnchor, CommentAuthor, CommentId, CommentKind, CommentNode, CommentStatus, CommentStore,
    CommentStoreEvent, CommentThread, ThreadId,
};
pub use persistence::CommentsDb;

use gpui::App;

/// Initializes the code-comments feature. Editor wiring, actions, settings,
/// and remote sync are registered here as later phases land.
pub fn init(_cx: &mut App) {}
