//! Per-workspace state shared across editors:
//!
//! - [`CommentStore`] holds the sqlite-backed conversations.
//! - [`ChangeTracker`] holds the currently-active [`ChangeId`] resolved by
//!   the configured code-review provider; the editor uses it to filter
//!   which conversations render.

use collections::HashMap;
use gpui::{App, AppContext as _, BorrowAppContext as _, Context, Entity, EventEmitter, Global};
use workspace::{Workspace, WorkspaceId};

use crate::{ChangeId, CommentStore};

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

/// Per-workspace handle to the code-review change currently under review in
/// this worktree (as resolved by [`crate::CodeReviewProvider::detect`]).
/// Editors observe this entity and re-render when the active change changes
/// (e.g. on branch switch).
pub struct ChangeTracker {
    current_change_id: Option<ChangeId>,
}

/// Emitted whenever the active change changes. Observers re-read
/// `ChangeTracker::current_change_id` to get the new value.
#[derive(Clone, Debug)]
pub struct CurrentChangeChanged;

impl EventEmitter<CurrentChangeChanged> for ChangeTracker {}

impl ChangeTracker {
    fn new() -> Self {
        Self {
            current_change_id: None,
        }
    }

    pub fn current_change_id(&self) -> Option<&ChangeId> {
        self.current_change_id.as_ref()
    }

    /// Updates the tracked change id and emits [`CurrentChangeChanged`] when
    /// the value actually moves.
    pub fn set_current_change_id(
        &mut self,
        change_id: Option<ChangeId>,
        cx: &mut Context<Self>,
    ) {
        if self.current_change_id == change_id {
            return;
        }
        self.current_change_id = change_id;
        cx.emit(CurrentChangeChanged);
    }
}

#[derive(Default)]
struct GlobalChangeTrackers {
    trackers: HashMap<WorkspaceId, Entity<ChangeTracker>>,
}

impl Global for GlobalChangeTrackers {}

/// Returns the [`ChangeTracker`] for the workspace, creating and caching it
/// on first use. Returns `None` for workspaces with no database id, matching
/// the [`comment_store`] scoping.
pub fn change_tracker(workspace: &Workspace, cx: &mut App) -> Option<Entity<ChangeTracker>> {
    let workspace_id = workspace.database_id()?;

    if !cx.has_global::<GlobalChangeTrackers>() {
        cx.set_global(GlobalChangeTrackers::default());
    }

    if let Some(tracker) = cx
        .global::<GlobalChangeTrackers>()
        .trackers
        .get(&workspace_id)
        .cloned()
    {
        return Some(tracker);
    }

    let tracker = cx.new(|_| ChangeTracker::new());
    cx.update_global::<GlobalChangeTrackers, _>(|global, _| {
        global.trackers.insert(workspace_id, tracker.clone());
    });
    Some(tracker)
}
