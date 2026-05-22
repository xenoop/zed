//! The [`CommentCard`] view: the block widget rendered below a commented line.
//! It draws the thread's nested comment tree and the controls for replying,
//! resolving, collapsing, and deleting.

use editor::Editor;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Subscription, WeakEntity, Window, div,
};
use ui::prelude::*;
use workspace::Workspace;

use crate::{
    CommentAuthor, CommentKind, CommentNode, CommentStatus, CommentStore, CommentThread, ThreadId,
    agent_integration,
};

/// A rendered inline comment thread. One card exists per visible thread.
pub struct CommentCard {
    store: Entity<CommentStore>,
    thread_id: ThreadId,
    workspace: WeakEntity<Workspace>,
    /// Input for composing the root comment or a reply.
    input: Entity<Editor>,
    _observe_store: Subscription,
}

impl CommentCard {
    pub fn new(
        store: Entity<CommentStore>,
        thread_id: ThreadId,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| Editor::auto_height(1, 6, window, cx));
        let _observe_store = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            store,
            thread_id,
            workspace,
            input,
            _observe_store,
        }
    }

    /// Cycles the thread's kind: Comment → Question → Task → Comment.
    fn cycle_kind(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            let next = match store.thread(thread_id).map(|thread| thread.kind) {
                Some(CommentKind::Comment) => CommentKind::Question,
                Some(CommentKind::Question) => CommentKind::Task,
                _ => CommentKind::Comment,
            };
            store.set_thread_kind(thread_id, next, cx);
        });
    }

    /// Sends this thread to the active agent thread; the agent's reply is
    /// appended to the comment tree.
    fn send_to_agent(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let store = self.store.clone();
        let thread_id = self.thread_id;
        workspace.update(cx, |workspace, cx| {
            agent_integration::send_thread_to_agent(workspace, store, thread_id, cx);
        });
    }

    pub fn thread_id(&self) -> ThreadId {
        self.thread_id
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.input.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            return;
        }
        let thread_id = self.thread_id;
        self.store.update(cx, |store, cx| {
            let parent = store
                .thread(thread_id)
                .and_then(|thread| thread.root().map(|root| root.id));
            store.add_node(
                thread_id,
                CommentNode::new(CommentAuthor::User, body, parent),
                cx,
            );
        });
        self.input.update(cx, |input, cx| input.clear(window, cx));
        cx.notify();
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

    fn delete(&mut self, cx: &mut Context<Self>) {
        let thread_id = self.thread_id;
        self.store
            .update(cx, |store, cx| store.remove_thread(thread_id, cx));
    }

    fn render_node(&self, node: &CommentNode, depth: usize, cx: &App) -> AnyElement {
        let author = match &node.author {
            CommentAuthor::User => "You".to_string(),
            CommentAuthor::Agent => "Agent".to_string(),
            CommentAuthor::Remote { login } => login.clone(),
        };
        v_flex()
            .pl(px(depth as f32 * 14.0))
            .gap_0p5()
            .child(
                Label::new(author)
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .child(
                div()
                    .text_ui(cx)
                    .child(SharedString::from(node.body.clone())),
            )
            .into_any_element()
    }

    fn render_tree(&self, thread: &CommentThread, cx: &App) -> Vec<AnyElement> {
        // Depth-first walk starting from the root; children are ordered after
        // their parent so indentation reflects the reply hierarchy.
        fn walk(
            card: &CommentCard,
            thread: &CommentThread,
            node: &CommentNode,
            depth: usize,
            cx: &App,
            out: &mut Vec<AnyElement>,
        ) {
            out.push(card.render_node(node, depth, cx));
            for child in thread.children(node.id) {
                walk(card, thread, child, depth + 1, cx, out);
            }
        }

        let mut out = Vec::new();
        if let Some(root) = thread.root() {
            walk(self, thread, root, 0, cx, &mut out);
        }
        out
    }
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

        let resolved = thread.status == CommentStatus::Resolved;
        let collapsed = thread.collapsed;
        let node_count = thread.nodes.len();
        let thread_key = thread.id.0.to_string();
        let kind_label = match thread.kind {
            CommentKind::Comment => "Comment",
            CommentKind::Question => "Question",
            CommentKind::Task => "Task",
        };

        let header = h_flex()
            .w_full()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(
                            SharedString::from(format!("kind-{thread_key}")),
                            kind_label,
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.cycle_kind(cx))),
                    )
                    .when(resolved, |this| {
                        this.child(
                            Label::new("Resolved")
                                .size(LabelSize::Small)
                                .color(Color::Success),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(
                            SharedString::from(format!("agent-{thread_key}")),
                            "Send to agent",
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.send_to_agent(cx))),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("collapse-{thread_key}")),
                            if collapsed { "Expand" } else { "Collapse" },
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_collapsed(cx))),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!("resolve-{thread_key}")),
                            if resolved { "Reopen" } else { "Resolve" },
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_resolved(cx))),
                    )
                    .child(
                        IconButton::new(
                            SharedString::from(format!("delete-{thread_key}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                    ),
            );

        let mut card = v_flex()
            .w_full()
            .gap_1p5()
            .p_2()
            .my_1()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().elevated_surface_background)
            .child(header);

        if collapsed {
            card = card.child(
                Label::new(format!("{node_count} comment(s) — expand to view"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
            return card;
        }

        card = card.children(self.render_tree(&thread, cx));

        let submit_label = if thread.root().is_some() {
            "Reply"
        } else {
            "Comment"
        };
        card.child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .rounded_sm()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .p_1()
                        .child(self.input.clone()),
                )
                .child(
                    h_flex().justify_end().child(
                        Button::new(
                            SharedString::from(format!("submit-{thread_key}")),
                            submit_label,
                        )
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
                ),
        )
    }
}
