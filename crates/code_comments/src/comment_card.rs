//! The [`CommentCard`] view: a stack of independent per-comment cards rendered
//! as a block decoration below the anchored line, in the spirit of a GitHub PR
//! review thread.
//!
//! Each [`CommentNode`] gets its own bordered card with author, kind badge,
//! body, and per-comment actions (cycle kind, send to agent, reply). Resolve /
//! collapse / delete remain thread-wide and live on the root card.

use std::collections::{HashMap, HashSet};

use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Length, StyleRefinement,
    Subscription, TextStyle, TextStyleRefinement, UnderlineStyle, WeakEntity, Window, div,
};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use ui::prelude::*;
use workspace::Workspace;

use crate::{
    CommentAuthor, CommentId, CommentKind, CommentNode, CommentStatus, CommentStore, CommentThread,
    ThreadId, agent_integration,
};

const THREAD_MAX_WIDTH_PX: f32 = 680.0;
const DEPTH_INDENT_PX: f32 = 24.0;

/// Per-node state computed by the tree walk and consumed by `render_node`.
struct NodeRenderState {
    is_root: bool,
    depth: usize,
    thread_resolved: bool,
    thread_collapsed: bool,
    /// True when the user has collapsed this node's subtree. The node itself
    /// still renders; descendants are skipped.
    subtree_collapsed: bool,
    /// Total descendants under this node (used to show "N replies hidden"
    /// when `subtree_collapsed` is true; also used to decide whether to show
    /// the chevron toggle at all).
    descendant_count: usize,
}

/// What the (single, shared) input editor is currently composing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyTarget {
    /// No open input. Non-empty threads show a small "Reply" affordance at
    /// the bottom; empty threads still show the first-comment composer.
    None,
    /// User clicked the bottom "Reply" affordance; the composer expands
    /// inline at the bottom of the thread and replies to the most recent
    /// node.
    Bottom,
    /// User clicked Reply on a specific comment; the input is rendered inline
    /// under that node and submits as its child.
    Node(CommentId),
}

/// A rendered inline comment thread. One card exists per visible thread.
pub struct CommentCard {
    store: Entity<CommentStore>,
    thread_id: ThreadId,
    workspace: WeakEntity<Workspace>,
    /// Whether the comment is still anchored to matching code; `false` renders
    /// an "Outdated" badge on the root card.
    anchored: bool,
    input: Entity<Editor>,
    reply_target: ReplyTarget,
    /// Per-card transient override for resolved-thread auto-collapse. When the
    /// user clicks Expand on a resolved-and-collapsed thread, we inflate it
    /// without persisting; reopening the editor resets to the default
    /// (collapsed) state.
    force_expanded: bool,
    /// Nodes whose subtree the user has collapsed (the node itself stays
    /// visible; only its descendants are hidden). Transient — reopening the
    /// editor expands everything again.
    subtree_collapsed: HashSet<CommentId>,
    /// Per-node rendered-markdown entities. Keyed by node id; the cached body
    /// string lets us rebuild only when a node's body actually changes.
    markdown_cache: HashMap<CommentId, (String, Entity<Markdown>)>,
    _observe_store: Subscription,
}

impl CommentCard {
    pub fn new(
        store: Entity<CommentStore>,
        thread_id: ThreadId,
        workspace: WeakEntity<Workspace>,
        anchored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 6, window, cx));
        let _observe_store = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            thread_id,
            workspace,
            anchored,
            input,
            reply_target: ReplyTarget::None,
            force_expanded: false,
            subtree_collapsed: HashSet::new(),
            markdown_cache: HashMap::new(),
            _observe_store,
        }
    }

    fn toggle_subtree_collapsed(&mut self, node_id: CommentId, cx: &mut Context<Self>) {
        if !self.subtree_collapsed.remove(&node_id) {
            self.subtree_collapsed.insert(node_id);
        }
        cx.notify();
    }

    fn set_force_expanded(&mut self, value: bool, cx: &mut Context<Self>) {
        if self.force_expanded != value {
            self.force_expanded = value;
            cx.notify();
        }
    }

    /// Refreshes the markdown-entity cache to match the current thread state:
    /// builds a new `Markdown` for nodes we haven't seen, replaces the entity
    /// whose body changed, and evicts nodes that no longer exist.
    fn sync_markdown_cache(&mut self, thread: &CommentThread, cx: &mut Context<Self>) {
        let live: std::collections::HashSet<CommentId> =
            thread.nodes.iter().map(|node| node.id).collect();
        self.markdown_cache.retain(|id, _| live.contains(id));
        for node in &thread.nodes {
            let needs_rebuild = self
                .markdown_cache
                .get(&node.id)
                .is_none_or(|(body, _)| body != &node.body);
            if needs_rebuild {
                let body: SharedString = node.body.clone().into();
                let entity = cx.new(|cx| Markdown::new(body, None, None, cx));
                self.markdown_cache
                    .insert(node.id, (node.body.clone(), entity));
            }
        }
    }

    fn markdown_style(&self, cx: &App) -> MarkdownStyle {
        let colors = cx.theme().colors();
        let mono_family: SharedString = "Zed Mono".into();
        MarkdownStyle {
            base_text_style: TextStyle {
                color: colors.text,
                ..Default::default()
            },
            code_block: StyleRefinement {
                text: TextStyleRefinement {
                    font_family: Some(mono_family.clone()),
                    background_color: Some(colors.editor_background),
                    ..Default::default()
                },
                margin: gpui::EdgesRefinement {
                    top: Some(Length::Definite(rems(0.5).into())),
                    bottom: Some(Length::Definite(rems(0.5).into())),
                    ..Default::default()
                },
                ..Default::default()
            },
            inline_code: TextStyleRefinement {
                font_family: Some(mono_family),
                background_color: Some(colors.editor_background),
                ..Default::default()
            },
            rule_color: Color::Muted.color(cx),
            block_quote_border_color: Color::Muted.color(cx),
            block_quote: TextStyleRefinement {
                color: Some(Color::Muted.color(cx)),
                ..Default::default()
            },
            link: TextStyleRefinement {
                color: Some(Color::Accent.color(cx)),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(Color::Accent.color(cx)),
                    wavy: false,
                }),
                ..Default::default()
            },
            syntax: cx.theme().syntax().clone(),
            selection_background_color: colors.element_selection_background,
            heading: Default::default(),
            ..Default::default()
        }
    }

    pub fn set_anchored(&mut self, anchored: bool, cx: &mut Context<Self>) {
        if self.anchored != anchored {
            self.anchored = anchored;
            cx.notify();
        }
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    fn cycle_node_kind(&mut self, node_id: CommentId, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            let current = store.thread(thread_id).and_then(|thread| {
                thread.nodes.iter().find(|node| node.id == node_id).map(|node| node.kind)
            });
            let next = match current {
                Some(CommentKind::Comment) => CommentKind::Question,
                Some(CommentKind::Question) => CommentKind::Task,
                _ => CommentKind::Comment,
            };
            store.set_node_kind(thread_id, node_id, next, cx);
        });
    }

    fn send_node_to_agent(&mut self, node_id: CommentId, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let thread_id = self.thread_id;
        workspace.update(cx, |workspace, cx| {
            agent_integration::send_node_to_agent(workspace, store, thread_id, node_id, cx);
        });
    }

    fn toggle_resolved(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            let next = match store.thread(thread_id).map(|thread| thread.status) {
                Some(CommentStatus::Resolved) => CommentStatus::Open,
                _ => CommentStatus::Resolved,
            };
            store.set_thread_status(thread_id, next, cx);
        });
    }

    fn toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            let collapsed = store
                .thread(thread_id)
                .map(|thread| thread.collapsed)
                .unwrap_or(false);
            store.set_thread_collapsed(thread_id, !collapsed, cx);
        });
    }

    fn delete_thread(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store
            .update(cx, |store, cx| store.remove_thread(thread_id, cx));
    }

    fn open_reply(&mut self, node_id: CommentId, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_target = ReplyTarget::Node(node_id);
        self.input.update(cx, |input, cx| input.clear(window, cx));
        self.input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn open_bottom_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_target = ReplyTarget::Bottom;
        self.input.update(cx, |input, cx| input.clear(window, cx));
        self.input.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn cancel_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reply_target = ReplyTarget::None;
        self.input.update(cx, |input, cx| input.clear(window, cx));
        cx.notify();
    }

    fn submit(&mut self, parent: Option<CommentId>, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.input.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            store.add_node(
                thread_id,
                CommentNode::new(CommentAuthor::User, body, parent, CommentKind::Comment),
                cx,
            );
        });
        self.input.update(cx, |input, cx| input.clear(window, cx));
        self.reply_target = ReplyTarget::None;
        cx.notify();
    }

    fn render_input(
        &self,
        parent: Option<CommentId>,
        submit_label: &'static str,
        key_suffix: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let cancel_key = SharedString::from(format!("cancel-{key_suffix}"));
        let submit_key = SharedString::from(format!("submit-{key_suffix}"));
        let show_cancel = matches!(
            self.reply_target,
            ReplyTarget::Node(_) | ReplyTarget::Bottom
        );
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border_variant)
                    .bg(colors.editor_background)
                    .p_1()
                    .child(self.input.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .justify_end()
                    .when(show_cancel, |this| {
                        this.child(
                            Button::new(cancel_key, "Cancel")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.cancel_reply(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(submit_key, submit_label)
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.submit(parent, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn kind_label(kind: CommentKind) -> &'static str {
        match kind {
            CommentKind::Comment => "Comment",
            CommentKind::Question => "Question",
            CommentKind::Task => "Task",
        }
    }

    fn kind_color(kind: CommentKind) -> Color {
        match kind {
            CommentKind::Comment => Color::Muted,
            CommentKind::Question => Color::Info,
            CommentKind::Task => Color::Warning,
        }
    }

    fn render_node(
        &self,
        node: &CommentNode,
        state: NodeRenderState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let NodeRenderState {
            is_root,
            depth,
            thread_resolved,
            thread_collapsed,
            subtree_collapsed,
            descendant_count,
        } = state;
        let colors = cx.theme().colors();
        let author = match &node.author {
            CommentAuthor::User => "You".to_string(),
            CommentAuthor::Agent => "Agent".to_string(),
            CommentAuthor::Remote { login } => login.clone(),
        };
        let node_id = node.id;
        let node_key = node_id.0.to_string();
        let group_name = SharedString::from(format!("card-{node_key}"));
        let is_replying_here = self.reply_target == ReplyTarget::Node(node_id);
        let kind = node.kind;
        let show_kind_chip = kind != CommentKind::Comment;

        // Header: optional chevron toggle (only when the node has children),
        // author, optional kind chip, root-level state badges; right side
        // hosts hover-revealed root actions.
        let header = h_flex()
            .w_full()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .when(descendant_count > 0, |this| {
                        let icon = if subtree_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        };
                        this.child(
                            IconButton::new(
                                SharedString::from(format!("subtree-{node_key}")),
                                icon,
                            )
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_subtree_collapsed(node_id, cx)
                            })),
                        )
                    })
                    .child(
                        Label::new(author)
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    )
                    .when(show_kind_chip, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("kind-{node_key}")),
                                Self::kind_label(kind),
                            )
                            .label_size(LabelSize::Small)
                            .color(Self::kind_color(kind))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.cycle_node_kind(node_id, cx)
                            })),
                        )
                    })
                    .when(is_root && thread_resolved, |this| {
                        this.child(
                            Label::new("Resolved")
                                .size(LabelSize::Small)
                                .color(Color::Success),
                        )
                    })
                    .when(is_root && !self.anchored, |this| {
                        this.child(
                            Label::new("Outdated")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .when(is_root, |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .visible_on_hover(group_name.clone())
                        .child(
                            Button::new(
                                SharedString::from(format!("collapse-{node_key}")),
                                if thread_collapsed { "Expand" } else { "Collapse" },
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_collapsed(cx))),
                        )
                        .child(
                            Button::new(
                                SharedString::from(format!("resolve-{node_key}")),
                                if thread_resolved { "Reopen" } else { "Resolve" },
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_resolved(cx))),
                        )
                        .child(
                            IconButton::new(
                                SharedString::from(format!("delete-{node_key}")),
                                IconName::Trash,
                            )
                            .icon_size(IconSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| this.delete_thread(cx))),
                        ),
                )
            });

        // Per-node action row. Agent-authored cards skip "Send to agent" since
        // bouncing the agent's own reply back to itself is never useful. The
        // whole row only paints on hover so the default state is the body.
        let show_send_to_agent = !matches!(node.author, CommentAuthor::Agent);
        let actions = h_flex()
            .gap_1()
            .justify_end()
            .visible_on_hover(group_name.clone())
            .when(!show_kind_chip, |this| {
                // Surface a quiet way to re-classify Comment-kind cards from
                // the hover row, since the header chip is hidden by default.
                this.child(
                    Button::new(
                        SharedString::from(format!("kind-{node_key}-hover")),
                        Self::kind_label(kind),
                    )
                    .label_size(LabelSize::Small)
                    .color(Self::kind_color(kind))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cycle_node_kind(node_id, cx)
                    })),
                )
            })
            .when(show_send_to_agent, |this| {
                this.child(
                    Button::new(
                        SharedString::from(format!("agent-{node_key}")),
                        "Send to agent",
                    )
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send_node_to_agent(node_id, cx)
                    })),
                )
            })
            .child(
                Button::new(SharedString::from(format!("reply-{node_key}")), "Reply")
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_reply(node_id, window, cx)
                    })),
            );

        // Tint agent-authored cards so they're visually distinct from user
        // comments at a glance, without being jarring.
        let (card_bg, border_color) = match &node.author {
            CommentAuthor::Agent => (colors.editor_subheader_background, colors.border),
            _ => (colors.elevated_surface_background, colors.border),
        };

        let body: AnyElement = match self.markdown_cache.get(&node_id) {
            Some((_, markdown)) => MarkdownElement::new(markdown.clone(), self.markdown_style(cx))
                .into_any_element(),
            None => div()
                .text_ui(cx)
                .child(SharedString::from(node.body.clone()))
                .into_any_element(),
        };

        let mut node_card = v_flex()
            .w_full()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(border_color)
            .bg(card_bg)
            .child(header)
            .child(body)
            .when(subtree_collapsed && descendant_count > 0, |this| {
                this.child(
                    Label::new(format!(
                        "{descendant_count} repl{} hidden",
                        if descendant_count == 1 { "y" } else { "ies" }
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .child(actions);

        if is_replying_here {
            node_card = node_card.child(self.render_input(
                Some(node_id),
                "Reply",
                node_key.clone(),
                cx,
            ));
        }

        div()
            .group(group_name)
            .pl(px(depth as f32 * DEPTH_INDENT_PX))
            .child(node_card)
            .into_any_element()
    }

    fn render_tree(&self, thread: &CommentThread, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // Depth-first walk: children render directly under their parent so the
        // visual order mirrors reply nesting. If a node's subtree is
        // collapsed, its descendants are skipped (but the node itself still
        // renders, with a "N replies hidden" indicator).
        fn walk(
            card: &CommentCard,
            thread: &CommentThread,
            node: &CommentNode,
            is_root: bool,
            depth: usize,
            resolved: bool,
            collapsed: bool,
            cx: &mut Context<CommentCard>,
            out: &mut Vec<AnyElement>,
        ) {
            let subtree_collapsed = card.subtree_collapsed.contains(&node.id);
            let descendant_count = count_descendants(thread, node.id);
            let state = NodeRenderState {
                is_root,
                depth,
                thread_resolved: resolved,
                thread_collapsed: collapsed,
                subtree_collapsed,
                descendant_count,
            };
            out.push(card.render_node(node, state, cx));
            if subtree_collapsed {
                return;
            }
            for child in thread.children(node.id) {
                walk(card, thread, child, false, depth + 1, resolved, collapsed, cx, out);
            }
        }

        let resolved = thread.status == CommentStatus::Resolved;
        let collapsed = thread.collapsed;
        let mut out = Vec::new();
        if let Some(root) = thread.root() {
            walk(self, thread, root, true, 0, resolved, collapsed, cx, &mut out);
        }
        out
    }
}

/// Total descendants under `root_id` in the thread tree.
fn count_descendants(thread: &CommentThread, root_id: CommentId) -> usize {
    let mut count = 0;
    let mut stack = vec![root_id];
    while let Some(id) = stack.pop() {
        for child in thread.children(id) {
            count += 1;
            stack.push(child.id);
        }
    }
    count
}

impl Focusable for CommentCard {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for CommentCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(thread) = self.store.read(cx).thread(self.thread_id).cloned() else {
            return div();
        };

        // Ensure every visible node has a (current) Markdown entity before we
        // walk the tree and render with `&self`.
        self.sync_markdown_cache(&thread, cx);

        let thread_key = thread.id.0.to_string();
        let resolved = thread.status == CommentStatus::Resolved;
        let user_collapsed = thread.collapsed;
        // Resolved threads auto-collapse to a compact strip so closed
        // conversations don't keep eating editor space. The user can inflate
        // them transiently via the strip's Expand control.
        let show_compact = thread.root().is_some()
            && (user_collapsed || (resolved && !self.force_expanded));

        let column = v_flex().w_full().gap_2().my_1();

        let column = if show_compact {
            column.child(self.render_compact_strip(&thread, resolved, &thread_key, cx))
        } else if thread.root().is_none() {
            // Empty thread (just created): the composer IS the only thing in
            // the thread.
            column.child(self.render_bottom_composer(&thread, &thread_key, cx))
        } else {
            // Tree of node cards + (idle) Reply affordance or (active) bottom
            // composer. The inline per-card reply takes precedence when open.
            let nodes = self.render_tree(&thread, cx);
            let column = column.children(nodes);
            match self.reply_target {
                ReplyTarget::None => {
                    column.child(self.render_reply_affordance(&thread_key, cx))
                }
                ReplyTarget::Bottom => {
                    column.child(self.render_bottom_composer(&thread, &thread_key, cx))
                }
                ReplyTarget::Node(_) => column,
            }
        };

        // Cap the width so the cards stay readable and don't span the editor.
        div().max_w(px(THREAD_MAX_WIDTH_PX)).child(column)
    }
}

impl CommentCard {
    /// One-line summary used when the thread is collapsed or resolved.
    /// Avoids the full bordered card chrome so closed threads recede into the
    /// background.
    fn render_compact_strip(
        &self,
        thread: &CommentThread,
        resolved: bool,
        thread_key: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let group_name = SharedString::from(format!("strip-{thread_key}"));
        let node_count = thread.nodes.len();

        // Distinct unique authors, in first-seen order, capped to keep the
        // strip from wrapping on busy threads.
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut authors: Vec<String> = Vec::new();
        for node in &thread.nodes {
            let name = match &node.author {
                CommentAuthor::User => "You".to_string(),
                CommentAuthor::Agent => "Agent".to_string(),
                CommentAuthor::Remote { login } => login.clone(),
            };
            if seen.insert(name.clone()) {
                authors.push(name);
            }
        }
        let author_summary = if authors.is_empty() {
            String::new()
        } else if authors.len() <= 3 {
            authors.join(", ")
        } else {
            format!("{}, +{}", authors[..2].join(", "), authors.len() - 2)
        };

        let (lead_text, lead_color) = if resolved {
            ("✓ Resolved".to_string(), Color::Success)
        } else {
            (format!("{node_count} comment(s) — collapsed"), Color::Muted)
        };
        let summary = if author_summary.is_empty() {
            lead_text
        } else if resolved {
            format!("{lead_text} · {node_count} · {author_summary}")
        } else {
            format!("{lead_text} · {author_summary}")
        };

        h_flex()
            .group(group_name.clone())
            .w_full()
            .justify_between()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.elevated_surface_background)
            .child(
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(lead_color),
            )
            .child(
                h_flex()
                    .gap_1()
                    .visible_on_hover(group_name)
                    .child(
                        Button::new(
                            SharedString::from(format!("expand-{thread_key}")),
                            "Expand",
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            // Set the per-card override for resolved threads;
                            // also clear the persisted collapsed flag so
                            // non-resolved threads stay expanded across opens.
                            if this.force_expanded {
                                // already inflated, this branch shouldn't fire
                            }
                            this.set_force_expanded(true, cx);
                            let thread_id = this.thread_id;
                            this.store.update(cx, |store, cx| {
                                if let Some(t) = store.thread(thread_id)
                                    && t.collapsed
                                {
                                    store.set_thread_collapsed(thread_id, false, cx);
                                }
                            });
                        })),
                    )
                    .when(resolved, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("reopen-{thread_key}")),
                                "Reopen",
                            )
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_force_expanded(true, cx);
                                this.toggle_resolved(cx);
                            })),
                        )
                    })
                    .child(
                        IconButton::new(
                            SharedString::from(format!("strip-delete-{thread_key}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.delete_thread(cx))),
                    ),
            )
            .into_any_element()
    }

    /// Idle-state reply affordance — a small right-aligned button shown at
    /// the bottom of a non-empty thread. Click expands the bottom composer
    /// (via `open_bottom_reply`) so we don't show an unfilled input by
    /// default.
    fn render_reply_affordance(&self, thread_key: &str, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .justify_end()
            .child(
                Button::new(
                    SharedString::from(format!("reply-affordance-{thread_key}")),
                    "Reply",
                )
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_bottom_reply(window, cx)
                })),
            )
            .into_any_element()
    }

    /// Single composer rendered at the bottom of every (non-compact, non-
    /// inline-reply) thread. Replies attach to the most recently added node
    /// so a linear conversation extends naturally; the first comment on an
    /// empty thread becomes the root.
    fn render_bottom_composer(
        &self,
        thread: &CommentThread,
        thread_key: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let (parent, label) = if thread.nodes.is_empty() {
            (None, "Comment")
        } else {
            let parent = thread
                .nodes
                .iter()
                .max_by_key(|node| node.created_at)
                .map(|node| node.id);
            (parent, "Reply")
        };
        v_flex()
            .w_full()
            .gap_1p5()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.elevated_surface_background)
            .child(self.render_input(
                parent,
                label,
                format!("bottom-{thread_key}"),
                cx,
            ))
            .into_any_element()
    }
}
