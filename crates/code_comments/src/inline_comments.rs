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
use multi_buffer::{Anchor, MultiBufferSnapshot};
use text::{Bias, Point};
use workspace::Workspace;

use crate::{
    AddComment, CommentAnchor, CommentKind, CommentStatus, CommentStore, CommentThread, ThreadId,
    comment_card::CommentCard, registry::comment_store,
};

/// Registers the `AddComment` workspace action and attaches comment rendering
/// to every editor as it is created.
pub fn init(cx: &mut App) {
    cx.observe_new::<Workspace>(|workspace, _window, _cx| {
        workspace.register_action(add_comment);
    })
    .detach();

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
    blocks: HashMap<ThreadId, ThreadBlock>,
    _subscriptions: Vec<Subscription>,
}

struct ThreadBlock {
    block_id: CustomBlockId,
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
    let Some(store) = resolve_store(editor, cx) else {
        return;
    };

    let mut subscriptions = Vec::new();
    subscriptions.push(cx.subscribe_in(
        &store,
        window,
        |editor, _store, _event, window, cx| {
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
    let Some(store) = resolve_store(editor, cx) else {
        return;
    };
    if editor.addon::<InlineCommentsAddon>().is_none() {
        return;
    }

    let multibuffer = editor.buffer().clone();
    let mb_snapshot = multibuffer.read(cx).snapshot(cx);
    let buffers = multibuffer.read(cx).all_buffers();

    let mut desired: HashMap<ThreadId, (CommentThread, Anchor)> = HashMap::default();
    for buffer in buffers {
        let buffer = buffer.read(cx);
        let Some(path) = buffer.file().map(|file| file.path().clone()) else {
            continue;
        };
        let threads = store.read(cx).threads_for_file(&path).to_vec();
        if threads.is_empty() {
            continue;
        }
        let buffer_snapshot = buffer.text_snapshot();
        let max_row = buffer_snapshot.max_point().row;
        for thread in threads {
            let row = thread.anchor.start_row.min(max_row);
            let text_anchor = buffer_snapshot.anchor_after(Point::new(row, 0));
            if let Some(anchor) = mb_snapshot.anchor_in_excerpt(text_anchor) {
                desired.insert(thread.id, (thread, anchor));
            }
        }
    }

    let mut blocks = editor
        .addon_mut::<InlineCommentsAddon>()
        .map(|addon| std::mem::take(&mut addon.blocks))
        .unwrap_or_default();

    // Remove blocks whose threads no longer exist.
    let stale: Vec<ThreadId> = blocks
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

    // Resize blocks whose comment count changed.
    let mut resizes = HashMap::default();
    for (id, block) in blocks.iter_mut() {
        if let Some((thread, _)) = desired.get(id) {
            let height = block_height(thread);
            if height != block.height {
                block.height = height;
                resizes.insert(block.block_id, height);
            }
        }
    }
    if !resizes.is_empty() {
        editor.resize_blocks(resizes, None, cx);
    }

    // Insert blocks for newly visible threads.
    let mut properties = Vec::new();
    let mut pending = Vec::new();
    for (id, (thread, anchor)) in &desired {
        if blocks.contains_key(id) {
            continue;
        }
        let height = block_height(thread);
        let card = cx.new(|cx| CommentCard::new(store.clone(), *id, window, cx));
        properties.push(BlockProperties {
            placement: BlockPlacement::Below(*anchor),
            height: Some(height),
            style: BlockStyle::Flex,
            render: Arc::new(move |_cx: &mut BlockContext| card.clone().into_any_element()),
            priority: 0,
        });
        pending.push((*id, height));
    }
    if !properties.is_empty() {
        let block_ids = editor.insert_blocks(properties, None, cx);
        for ((id, height), block_id) in pending.into_iter().zip(block_ids) {
            blocks.insert(id, ThreadBlock { block_id, height });
        }
    }

    if let Some(addon) = editor.addon_mut::<InlineCommentsAddon>() {
        addon.blocks = blocks;
    }
}

/// Estimated block height in display rows. Refined as the card content grows.
fn block_height(thread: &CommentThread) -> u32 {
    if thread.collapsed {
        return 4;
    }
    let nodes = thread.nodes.len().max(1) as u32;
    // Header + each comment (~3 rows) + the compose area.
    3 + nodes * 3 + 4
}

fn resolve_store(editor: &Editor, cx: &mut App) -> Option<Entity<CommentStore>> {
    let workspace = editor.workspace()?;
    workspace.update(cx, |workspace, cx| comment_store(workspace, cx))
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
        return;
    };
    let Some(store) = comment_store(workspace, cx) else {
        return;
    };
    let Some(thread) = editor.update(cx, |editor, cx| build_thread(editor, window, cx)) else {
        return;
    };
    store.update(cx, |store, cx| store.upsert_thread(thread, cx));
}

fn build_thread(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Option<CommentThread> {
    let path = editor.project_path(cx)?.path;
    let display_snapshot = editor.snapshot(window, cx);
    let selection = editor.selections.newest::<Point>(&display_snapshot);
    let start = selection.start;
    let end = selection.end;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    Some(CommentThread {
        id: ThreadId::new(),
        file: path,
        anchor: CommentAnchor {
            start_row: start.row,
            start_column: start.column,
            end_row: end.row,
            end_column: end.column,
            fingerprint: line_fingerprint(&snapshot, start.row),
        },
        kind: CommentKind::Comment,
        status: CommentStatus::Open,
        nodes: Vec::new(),
        collapsed: false,
    })
}

/// Trimmed text of the anchored line, stored for best-effort re-anchoring.
fn line_fingerprint(snapshot: &MultiBufferSnapshot, row: u32) -> String {
    let max_row = snapshot.max_point().row;
    let row = row.min(max_row);
    let start = Point::new(row, 0);
    let end = snapshot.clip_point(Point::new(row, u32::MAX), Bias::Left);
    snapshot
        .text_for_range(start..end)
        .collect::<String>()
        .trim()
        .to_string()
}
