//! Tracks the per-workspace [`CommentStore`] instances so editors in the same
//! workspace share one store (and one sqlite-backed comment set).

use collections::HashMap;
use gpui::{App, AppContext as _, BorrowAppContext as _, Entity, Global};
use workspace::{Workspace, WorkspaceId};

use crate::CommentStore;

#[derive(Default)]
struct GlobalCommentStores {
    stores: HashMap<WorkspaceId, Entity<CommentStore>>,
}

impl Global for GlobalCommentStores {}

/// Returns the [`CommentStore`] for the given workspace, creating and caching
/// it on first use. Returns `None` for workspaces with no database id (e.g.
/// ones that were never persisted), since comments are workspace-local.
pub fn comment_store(workspace: &Workspace, cx: &mut App) -> Option<Entity<CommentStore>> {
    let workspace_id = workspace.database_id()?;

    if !cx.has_global::<GlobalCommentStores>() {
        cx.set_global(GlobalCommentStores::default());
    }

    if let Some(store) = cx
        .global::<GlobalCommentStores>()
        .stores
        .get(&workspace_id)
        .cloned()
    {
        return Some(store);
    }

    let store = cx.new(|cx| CommentStore::new(workspace_id, cx));
    cx.update_global::<GlobalCommentStores, _>(|global, _| {
        global.stores.insert(workspace_id, store.clone());
    });
    Some(store)
}
