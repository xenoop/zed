//! Editor integration: registers the `AddComment` action and keeps each
//! editor's inline comment-card blocks in sync with its workspace
//! [`CommentStore`].

use std::sync::Arc;

use collections::{HashMap, HashSet};
use editor::{
    Editor,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{App, AppContext as _, Context, Entity, IntoElement, Subscription, Window};
use multi_buffer::Anchor;
use text::{Bias, BufferSnapshot, Point};
use workspace::Workspace;

use crate::{
    AddComment, CodeCommentsSettings, CodeReviewRegistry, CommentAnchor, CommentKind,
    CommentStore, Conversation, ConversationId, ConversationStatus, RepoContext,
    SendTasksToAgent, agent_integration, comment_card::ConversationCard,
    registry::{ChangeTracker, change_tracker, comment_store},
};
use project::git_store::{GitStoreEvent, RepositoryEvent};
use settings::Settings as _;
use util::ResultExt as _;

/// Registers the `AddComment` workspace action and attaches comment rendering
/// to every editor as it is created.
pub fn init(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, window, cx| {
        workspace.register_action(add_comment);
        workspace.register_action(send_tasks);

        // `observe_new::<Editor>` fires before the editor's workspace handle
        // is wired up, so attaching from there silently fails. `ItemAdded`
        // fires after the editor is mounted into a pane (including for tabs
        // restored on workspace open), so it's the reliable hook.
        //
        // We resolve the store from the `&mut Workspace` the subscription
        // hands us — going through `attach_to_editor`'s `resolve_store` would
        // call `workspace.update(...)`, and `ItemAdded` can fire while the
        // workspace is already being updated (e.g. during deserialization),
        // double-leasing the entity and panicking.
        if let Some(window) = window {
            let workspace_entity = cx.entity();
            cx.subscribe_in(
                &workspace_entity,
                window,
                |workspace, _, event, window, cx| {
                    if let workspace::Event::ItemAdded { item } = event
                        && let Some(editor) = item.act_as::<Editor>(cx)
                        && let Some(store) = comment_store(workspace, cx)
                        && let Some(tracker) = change_tracker(workspace, cx)
                    {
                        editor.update(cx, |editor, cx| {
                            attach_with_state(editor, store, tracker, window, cx)
                        });
                    }
                },
            )
            .detach();

            setup_change_tracking(workspace, window, cx);
        }
    })
    .detach();

    // Keep the eager `observe_new::<Editor>` path too: for editors created
    // inside a workspace context (e.g. via AddComment's on-demand attach), it
    // can succeed and saves us a round-trip through the workspace event.
    cx.observe_new::<Editor>(|editor, window, cx| {
        if let Some(window) = window {
            attach_to_editor(editor, window, cx);
        }
    })
    .detach();
}

/// Per-editor state, stored as an editor addon so it is dropped together with
/// the editor (taking its comment-card entities and subscriptions with it).
struct InlineCommentsAddon {
    /// Cached store handle. We stash it here so `refresh` doesn't need to
    /// re-enter `workspace.update` (which would double-lease the workspace
    /// when refresh fires from inside the `AddComment` action handler).
    store: Entity<CommentStore>,
    /// Cached change-tracker handle. Same rationale as `store` — `refresh`
    /// reads the active change id from this instead of going through
    /// `workspace.update`.
    change_tracker: Entity<ChangeTracker>,
    blocks: HashMap<ConversationId, ThreadBlock>,
    _subscriptions: Vec<Subscription>,
}

struct ThreadBlock {
    block_id: CustomBlockId,
    card: Entity<ConversationCard>,
    height: u32,
}

impl editor::Addon for InlineCommentsAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn attach_to_editor(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    if editor.addon::<InlineCommentsAddon>().is_some() {
        return;
    }
    // Comments are workspace-local; editors with no workspace (input fields,
    // previews) get no store and are skipped.
    let Some((store, tracker)) = resolve_workspace_state(editor, cx) else {
        // Most editors that hit this path are input fields, prompts, popovers,
        // etc. that have no workspace and never want comments.
        log::debug!("code_comments: attach_to_editor skipped (no state at attach time)");
        return;
    };
    attach_with_state(editor, store, tracker, window, cx);
}

/// Attaches the addon using an already-resolved store and change-tracker. The
/// on-demand path used by `add_comment` calls this directly to avoid
/// re-entering `workspace.update` (which would double-lease the workspace
/// entity and panic).
fn attach_with_state(
    editor: &mut Editor,
    store: Entity<CommentStore>,
    change_tracker: Entity<ChangeTracker>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if editor.addon::<InlineCommentsAddon>().is_some() {
        return;
    }

    let mut subscriptions = Vec::new();
    subscriptions.push(cx.subscribe_in(
        &store,
        window,
        |editor, _store, _event, window, cx| {
            refresh(editor, window, cx);
        },
    ));
    // Active change can shift (branch switch, new PR detected): when it
    // does, re-render so the visible conversations match the new scope.
    subscriptions.push(cx.subscribe_in(
        &change_tracker,
        window,
        |editor, _tracker, _event, window, cx| {
            refresh(editor, window, cx);
        },
    ));
    // The Project Diff multibuffer adds and removes file excerpts over time;
    // refresh so comment cards follow their excerpts.
    let multibuffer = editor.buffer().clone();
    subscriptions.push(cx.subscribe_in(
        &multibuffer,
        window,
        |editor, _multibuffer, event, window, cx| {
            if matches!(
                event,
                multi_buffer::Event::BufferRangesUpdated { .. }
                    | multi_buffer::Event::BuffersRemoved { .. }
            ) {
                refresh(editor, window, cx);
            }
        },
    ));
    editor.register_addon(InlineCommentsAddon {
        store,
        change_tracker,
        blocks: HashMap::default(),
        _subscriptions: subscriptions,
    });
    refresh(editor, window, cx);
}

/// Reconciles the editor's comment-card blocks with the store: removes blocks
/// for deleted threads, inserts blocks for new ones, and resizes the rest.
///
/// Works buffer-by-buffer so it covers both singleton file editors and the
/// Project Diff multibuffer: each thread's stored buffer position is lifted to
/// a multibuffer anchor via `anchor_in_excerpt`.
fn refresh(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    // Read the store + tracker from the addon to avoid re-entering
    // `workspace.update` (which the resolver would do, and which panics when
    // refresh fires while the workspace is already being updated — e.g. from
    // within the `AddComment` action handler).
    let Some((store, current_change_id)) = editor.addon::<InlineCommentsAddon>().map(|addon| {
        (
            addon.store.clone(),
            addon.change_tracker.read(cx).current_change_id().cloned(),
        )
    }) else {
        log::warn!("code_comments: refresh skipped (editor has no addon)");
        return;
    };
    let Some(workspace) = editor.workspace() else {
        log::warn!("code_comments: refresh skipped (editor has no workspace)");
        return;
    };
    let workspace = workspace.downgrade();

    let multibuffer = editor.buffer().clone();
    let mb_snapshot = multibuffer.read(cx).snapshot(cx);
    let buffers = multibuffer.read(cx).all_buffers();

    let mut desired: HashMap<ConversationId, (Conversation, Anchor, bool)> = HashMap::default();
    for buffer in buffers {
        let buffer = buffer.read(cx);
        let Some(path) = buffer.file().map(|file| file.path().clone()) else {
            continue;
        };
        let threads: Vec<Conversation> = store
            .read(cx)
            .conversations_for_file(&path)
            .iter()
            // Conversations are scoped to the active change. A conversation
            // with no change_id (legacy local-only) is visible only when no
            // change is active; change-scoped ones are visible only when
            // they match the current change.
            .filter(|thread| thread.change_id == current_change_id)
            .cloned()
            .collect();
        if threads.is_empty() {
            continue;
        }
        let buffer_snapshot = buffer.text_snapshot();
        for thread in threads {
            let (row, anchored) = resolve_row(&buffer_snapshot, &thread.anchor);
            let text_anchor = buffer_snapshot.anchor_after(Point::new(row, 0));
            // `anchor_in_excerpt` returning None is normal in the Project Diff
            // multibuffer when a comment is on a line outside any visible
            // hunk, so we just skip it instead of logging.
            if let Some(anchor) = mb_snapshot.anchor_in_excerpt(text_anchor) {
                desired.insert(thread.id, (thread, anchor, anchored));
            }
        }
    }

    let mut blocks = editor
        .addon_mut::<InlineCommentsAddon>()
        .map(|addon| std::mem::take(&mut addon.blocks))
        .unwrap_or_default();

    // Remove blocks whose threads no longer exist.
    let stale: Vec<ConversationId> = blocks
        .keys()
        .copied()
        .filter(|id| !desired.contains_key(id))
        .collect();
    if !stale.is_empty() {
        let mut removed = HashSet::default();
        for id in stale {
            if let Some(block) = blocks.remove(&id) {
                removed.insert(block.block_id);
            }
        }
        editor.remove_blocks(removed, None, cx);
    }

    // Resize blocks whose comment count changed, and refresh the outdated
    // state of every card still on screen.
    let mut resizes = HashMap::default();
    for (id, block) in blocks.iter_mut() {
        if let Some((thread, _, anchored)) = desired.get(id) {
            let height = block_height(thread);
            if height != block.height {
                block.height = height;
                resizes.insert(block.block_id, height);
            }
            block
                .card
                .update(cx, |card, cx| card.set_anchored(*anchored, cx));
        }
    }
    if !resizes.is_empty() {
        editor.resize_blocks(resizes, None, cx);
    }

    // Insert blocks for newly visible threads.
    let mut properties = Vec::new();
    let mut pending = Vec::new();
    for (id, (thread, anchor, anchored)) in &desired {
        if blocks.contains_key(id) {
            continue;
        }
        let height = block_height(thread);
        let card = cx.new(|cx| {
            ConversationCard::new(store.clone(), *id, workspace.clone(), *anchored, window, cx)
        });
        let card_for_block = card.clone();
        properties.push(BlockProperties {
            placement: BlockPlacement::Below(*anchor),
            height: Some(height),
            style: BlockStyle::Flex,
            render: Arc::new(move |_cx: &mut BlockContext| {
                card_for_block.clone().into_any_element()
            }),
            priority: 0,
        });
        pending.push((*id, height, card));
    }
    if !properties.is_empty() {
        let block_ids = editor.insert_blocks(properties, None, cx);
        for ((id, height, card), block_id) in pending.into_iter().zip(block_ids) {
            blocks.insert(
                id,
                ThreadBlock {
                    block_id,
                    card,
                    height,
                },
            );
        }
    }

    if let Some(addon) = editor.addon_mut::<InlineCommentsAddon>() {
        addon.blocks = blocks;
    }
}

/// Resolves a thread's display row against the current buffer using its stored
/// fingerprint. Returns the row to anchor the card to and whether the comment
/// is still anchored — `false` marks it outdated.
///
/// If the fingerprinted line moved, it is found by scanning a window around the
/// stored row; if it cannot be found at all, the comment is flagged outdated
/// and left at its stored row.
fn resolve_row(snapshot: &BufferSnapshot, anchor: &CommentAnchor) -> (u32, bool) {
    let max_row = snapshot.max_point().row;
    let stored = anchor.start_row.min(max_row);
    if anchor.fingerprint.is_empty() {
        return (stored, true);
    }
    if line_text(snapshot, stored) == anchor.fingerprint {
        return (stored, true);
    }
    const WINDOW: u32 = 80;
    let lo = anchor.start_row.saturating_sub(WINDOW);
    let hi = (anchor.start_row + WINDOW).min(max_row);
    for row in lo..=hi {
        if line_text(snapshot, row) == anchor.fingerprint {
            return (row, true);
        }
    }
    (stored, false)
}

fn line_text(snapshot: &BufferSnapshot, row: u32) -> String {
    let start = Point::new(row, 0);
    let end = snapshot.clip_point(Point::new(row, u32::MAX), Bias::Left);
    snapshot
        .text_for_range(start..end)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Estimated block height in display rows. Refined as the card content grows.
///
/// Reserves enough rows for the thread header, each per-node card (header +
/// body + reply button + padding ~7 rows), and one potentially-open reply
/// input (~5 rows). Bigger than strictly needed but avoids clipping; the card
/// is left-aligned and capped in width so the extra space is not visually
/// noisy.
fn block_height(thread: &Conversation) -> u32 {
    if thread.collapsed {
        return 4;
    }
    let nodes = thread.nodes.len().max(1) as u32;
    3 + nodes * 7 + 6
}

fn resolve_workspace_state(
    editor: &Editor,
    cx: &mut App,
) -> Option<(Entity<CommentStore>, Entity<ChangeTracker>)> {
    let workspace = editor.workspace()?;
    workspace.update(cx, |workspace, cx| {
        let store = comment_store(workspace, cx)?;
        let tracker = change_tracker(workspace, cx)?;
        Some((store, tracker))
    })
}

/// `AddComment` handler: creates an open, empty comment thread anchored to the
/// active editor's newest selection. The card's input is used to write the
/// first comment.
fn add_comment(
    workspace: &mut Workspace,
    _: &AddComment,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(editor) = workspace
        .active_item(cx)
        .and_then(|item| item.act_as::<Editor>(cx))
    else {
        log::warn!("code_comments: no active editor for AddComment");
        return;
    };
    let Some(store) = comment_store(workspace, cx) else {
        log::warn!(
            "code_comments: workspace has no database_id, comment store unavailable (open a saved project)"
        );
        return;
    };
    let Some(tracker) = change_tracker(workspace, cx) else {
        return;
    };
    let current_change_id = tracker.read(cx).current_change_id().cloned();
    let Some(mut thread) = editor.update(cx, |editor, cx| build_thread(editor, window, cx)) else {
        log::warn!(
            "code_comments: build_thread returned None (file likely has no project_path; save it under a worktree)"
        );
        return;
    };
    // Tag the new conversation with whatever change is currently active so
    // it shows up in the right scope (and not in other branches).
    thread.change_id = current_change_id;

    // The `observe_new::<Editor>` callback fires during workspace startup,
    // before the editor's workspace handle is ready, so most editors never get
    // the addon attached. Attach now, on demand, with the store and tracker
    // we already resolved — going through `attach_to_editor` would re-enter
    // `workspace.update` and double-lease the workspace entity.
    let store_for_attach = store.clone();
    let tracker_for_attach = tracker.clone();
    editor.update(cx, |editor, cx| {
        attach_with_state(editor, store_for_attach, tracker_for_attach, window, cx)
    });
    store.update(cx, |store, cx| store.upsert_conversation(thread, cx));
}

/// `SendTasksToAgent` handler: delivers every open task-kind comment to the
/// active agent thread.
fn send_tasks(
    workspace: &mut Workspace,
    _: &SendTasksToAgent,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = comment_store(workspace, cx) else {
        return;
    };
    agent_integration::send_tasks_to_agent(workspace, store, cx);
}

fn build_thread(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<Conversation> {
    let display_snapshot = editor.snapshot(window, cx);
    let selection = editor.selections.newest::<Point>(&display_snapshot);
    let multi_buffer = editor.buffer().clone();
    let multi_snapshot = multi_buffer.read(cx).snapshot(cx);

    // Resolve the underlying buffer + buffer-local point from the primary
    // selection. Works for both singleton-file editors and the Project Diff
    // multibuffer (where there's no editor-wide `project_path`).
    let (buffer_snapshot, buffer_point) = multi_snapshot.point_to_buffer_point(selection.start)?;
    let buffer_id = buffer_snapshot.remote_id();
    let buffer_entity = multi_buffer.read(cx).buffer(buffer_id)?;
    let buffer = buffer_entity.read(cx);
    let path = buffer.file().map(|file| file.path().clone())?;

    let text_snapshot = buffer.text_snapshot();
    Some(Conversation {
        id: ConversationId::new(),
        change_id: None,
        file: path,
        anchor: CommentAnchor {
            start_row: buffer_point.row,
            start_column: buffer_point.column,
            end_row: buffer_point.row,
            end_column: buffer_point.column,
            fingerprint: line_text(&text_snapshot, buffer_point.row),
            revision: None,
        },
        kind: CommentKind::Comment,
        status: ConversationStatus::Open,
        nodes: Vec::new(),
        collapsed: false,
    })
}

/// Subscribes the workspace to git-store events so the per-workspace
/// [`ChangeTracker`] keeps the active change id in sync with the worktree's
/// HEAD, and kicks off an initial detect so the value is populated as soon
/// as the workspace opens.
fn setup_change_tracking(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tracker) = change_tracker(workspace, cx) else {
        return;
    };

    spawn_detect_current_change(workspace, tracker.clone(), cx);

    let git_store = workspace.project().read(cx).git_store().clone();
    cx.subscribe_in(
        &git_store,
        window,
        move |workspace, _git_store, event, _window, cx| {
            if matches!(
                event,
                GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, _)
            ) {
                spawn_detect_current_change(workspace, tracker.clone(), cx);
            }
        },
    )
    .detach();
}

/// Resolves the configured provider, calls `detect`, and stores the result on
/// the per-workspace [`ChangeTracker`]. When no provider is registered (or
/// detect returns `None`), clears the tracker so the editor falls back to
/// rendering only legacy local-only conversations.
fn spawn_detect_current_change(
    workspace: &Workspace,
    tracker: Entity<ChangeTracker>,
    cx: &mut Context<Workspace>,
) {
    let settings = CodeCommentsSettings::get_global(cx).clone();
    let Some(provider) = CodeReviewRegistry::get(cx, &settings.sync.provider) else {
        tracker.update(cx, |tracker, cx| tracker.set_current_change_id(None, cx));
        return;
    };
    let Some(worktree_root) = workspace
        .project()
        .read(cx)
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        return;
    };
    let ctx = RepoContext { worktree_root };
    let tracker = tracker.downgrade();
    cx.spawn(async move |_workspace, cx| {
        let detected = provider.detect(&ctx).await.log_err().flatten();
        tracker
            .update(cx, |tracker, cx| {
                tracker.set_current_change_id(detected, cx);
            })
            .log_err();
    })
    .detach();
}
