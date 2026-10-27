use std::sync::Arc;

use code_review::{
    CredentialAttempt, CredentialOutcome, RepositoryReviewInfo, ReviewState, ReviewStore,
    ReviewStoreEvent, review_target,
};
use collections::{HashMap, HashSet};
use editor::{
    Editor,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use git::{
    GitHostingProviderRegistry,
    code_review::{
        CommentAuthorRole, CommentId, DiffSide, DraftComment, ReviewComment, ReviewSubmission,
        ReviewThread, ReviewVerdict, ThreadId,
    },
};
use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    SharedString, Subscription, WeakEntity, Window,
};
use language::{LanguageRegistry, Point};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use menu::{Cancel, Confirm};
use multi_buffer::{Anchor, MultiBufferSnapshot};
use project::{
    Project,
    git_store::diff_buffer_list::{BranchDiffEvent, DiffBase},
};
use time::{OffsetDateTime, UtcOffset};
use ui::{Avatar, Chip, Disclosure, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt as _};
use zed_actions::git::SubmitReviewReply;

use crate::diff_multibuffer::DiffMultibuffer;

/// How deep nested replies are indented before they stop shifting right.
const MAX_REPLY_INDENT: usize = 2;

/// Shows a review's threads as cards between the lines of a branch diff.
pub(crate) struct ReviewThreads {
    store: Entity<ReviewStore>,
    diff: WeakEntity<DiffMultibuffer>,
    editor: WeakEntity<Editor>,
    languages: Arc<LanguageRegistry>,
    visible: bool,
    block_ids: Vec<CustomBlockId>,
    collapsed: HashSet<ThreadId>,
    expanded_resolved: HashSet<ThreadId>,
    markdown: HashMap<CommentId, (SharedString, Entity<Markdown>)>,
    reply_editors: HashMap<ThreadId, Entity<Editor>>,
    thread_errors: HashMap<ThreadId, SharedString>,
    threads_outside_diff: usize,
    _subscriptions: Vec<Subscription>,
}

impl ReviewThreads {
    pub(crate) fn new(
        diff: Entity<DiffMultibuffer>,
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store = cx.new(|cx| ReviewStore::new(cx));
        let editor = diff.read(cx).editor().read(cx).rhs_editor().clone();
        let multibuffer = diff.read(cx).multibuffer().clone();
        let diff_buffer_list = diff.read(cx).branch_diff().clone();
        let subscriptions = vec![
            cx.subscribe_in(&store, window, |this, _, event, window, cx| match event {
                ReviewStoreEvent::Changed => this.update_blocks(window, cx),
            }),
            cx.subscribe_in(
                &multibuffer,
                window,
                |this, _, event, window, cx| match event {
                    multi_buffer::Event::BufferRangesUpdated { .. }
                    | multi_buffer::Event::BuffersRemoved { .. } => this.update_blocks(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe(&diff_buffer_list, |this, _, event, cx| {
                if matches!(event, BranchDiffEvent::DiffBaseChanged) && this.visible {
                    this.refresh(cx);
                }
            }),
        ];
        Self {
            store,
            diff: diff.downgrade(),
            editor: editor.downgrade(),
            languages: project.read(cx).languages().clone(),
            visible: false,
            block_ids: Vec::new(),
            collapsed: HashSet::default(),
            expanded_resolved: HashSet::default(),
            markdown: HashMap::default(),
            reply_editors: HashMap::default(),
            thread_errors: HashMap::default(),
            threads_outside_diff: 0,
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn store(&self) -> &Entity<ReviewStore> {
        &self.store
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn threads_outside_diff(&self) -> usize {
        self.threads_outside_diff
    }

    pub(crate) fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        if self.visible {
            self.refresh(cx);
        }
        self.update_blocks(window, cx);
    }

    /// New comments are only allowed when the local files match the review's
    /// head, since their line numbers are sent as-is.
    pub(crate) fn can_comment(&self, cx: &App) -> bool {
        self.visible
            && self
                .store
                .read(cx)
                .loaded()
                .is_some_and(|loaded| loaded.local_head_matches)
    }

    /// The comments written in the diff's gutter, as review comments.
    pub(crate) fn drafts(&self, cx: &App) -> Vec<DraftComment> {
        let (Some(editor), Some(diff)) = (self.editor.upgrade(), self.diff.upgrade()) else {
            return Vec::new();
        };
        let diff = diff.read(cx);
        editor
            .read(cx)
            .review_comment_drafts(cx)
            .into_iter()
            .filter_map(|draft| {
                Some(DraftComment {
                    path: diff.repo_path_for_buffer(draft.buffer_id, cx)?,
                    side: DiffSide::New,
                    lines: draft.rows.start() + 1..=draft.rows.end() + 1,
                    body: draft.text,
                })
            })
            .collect()
    }

    pub(crate) fn clear_drafts(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.upgrade() {
            editor.update(cx, |editor, cx| editor.clear_review_comments(cx));
        }
    }

    fn submit_reply(&mut self, thread_id: ThreadId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.reply_editors.get(&thread_id).cloned() else {
            return;
        };
        let body = editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        self.thread_errors.remove(&thread_id);
        let task = self
            .store
            .update(cx, |store, cx| store.reply(thread_id.clone(), body, cx));
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(()) => editor.update(cx, |editor, cx| editor.clear(window, cx)),
                    Err(error) => {
                        this.thread_errors
                            .insert(thread_id, format!("Couldn't post: {error:#}").into());
                    }
                }
                this.update_blocks(window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn set_resolved(
        &mut self,
        thread_id: ThreadId,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.thread_errors.remove(&thread_id);
        let task = self.store.update(cx, |store, cx| {
            store.set_resolved(thread_id.clone(), resolved, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    this.thread_errors
                        .insert(thread_id, format!("Couldn't update: {error:#}").into());
                }
                this.update_blocks(window, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    /// Finds the review for the diff's repository and branch and reloads it.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(diff) = self.diff.upgrade() else {
            return;
        };
        let diff = diff.read(cx);
        let DiffBase::Merge { base_ref } = diff.diff_base(cx).clone() else {
            return;
        };
        let Some(repository) = diff.repo(cx) else {
            self.store.update(cx, |store, cx| {
                store.set_unavailable("This diff has no repository.".into(), cx)
            });
            return;
        };
        let snapshot = repository.read(cx).snapshot();
        let info = RepositoryReviewInfo {
            origin_url: snapshot.remote_origin_url.clone(),
            upstream_url: snapshot.remote_upstream_url.clone(),
            branch: snapshot
                .branch
                .as_ref()
                .map(|branch| branch.name().to_string().into()),
            remote_branch: snapshot
                .branch
                .as_ref()
                .and_then(|branch| branch.upstream.as_ref())
                .filter(|upstream| upstream.is_remote())
                .and_then(|upstream| upstream.branch_name())
                .map(|name| name.to_string().into()),
            head_sha: snapshot
                .head_commit
                .as_ref()
                .map(|commit| commit.sha.clone()),
        };
        let registry = GitHostingProviderRegistry::default_global(cx);
        match review_target(&info, &base_ref, registry) {
            Ok(target) => self.store.update(cx, |store, cx| store.refresh(target, cx)),
            Err(reason) => self
                .store
                .update(cx, |store, cx| store.set_unavailable(reason, cx)),
        }
    }

    #[cfg(test)]
    pub(crate) fn show_with_store(
        &mut self,
        store: Entity<ReviewStore>,
        target: code_review::ReviewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._subscriptions[0] =
            cx.subscribe_in(&store, window, |this, _, event, window, cx| match event {
                ReviewStoreEvent::Changed => this.update_blocks(window, cx),
            });
        self.store = store;
        self.visible = true;
        self.store.update(cx, |store, cx| store.refresh(target, cx));
    }

    #[cfg(test)]
    pub(crate) fn block_ids(&self) -> &[CustomBlockId] {
        &self.block_ids
    }

    #[cfg(test)]
    pub(crate) fn toggle_collapsed_for_test(
        &mut self,
        thread_id: &ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread = self.store.read(cx).loaded().and_then(|loaded| {
            loaded
                .threads
                .iter()
                .find(|thread| thread.id == *thread_id)
                .cloned()
        });
        if let Some(thread) = thread {
            self.toggle_collapsed(&thread, window, cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn reply_editor(&self, thread_id: &ThreadId) -> Option<Entity<Editor>> {
        self.reply_editors.get(thread_id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn submit_reply_for_test(
        &mut self,
        thread_id: ThreadId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.submit_reply(thread_id, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn set_resolved_for_test(
        &mut self,
        thread_id: ThreadId,
        resolved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_resolved(thread_id, resolved, window, cx);
    }

    fn toggle_collapsed(
        &mut self,
        thread: &ReviewThread,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if thread.is_resolved {
            if !self.expanded_resolved.remove(&thread.id) {
                self.expanded_resolved.insert(thread.id.clone());
            }
        } else if !self.collapsed.remove(&thread.id) {
            self.collapsed.insert(thread.id.clone());
        }
        self.update_blocks(window, cx);
    }

    fn is_expanded(&self, thread: &ReviewThread) -> bool {
        if thread.is_resolved {
            self.expanded_resolved.contains(&thread.id)
        } else {
            !self.collapsed.contains(&thread.id)
        }
    }

    fn markdown_for(
        &mut self,
        comment: &ReviewComment,
        cx: &mut Context<Self>,
    ) -> Entity<Markdown> {
        if let Some((body, markdown)) = self.markdown.get(&comment.id)
            && *body == comment.body
        {
            return markdown.clone();
        }
        let languages = self.languages.clone();
        let markdown = cx.new(|cx| Markdown::new(comment.body.clone(), Some(languages), None, cx));
        self.markdown
            .insert(comment.id.clone(), (comment.body.clone(), markdown.clone()));
        markdown
    }

    fn update_blocks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        let Some(editor) = self.editor.upgrade() else {
            return;
        };
        let can_comment = self.can_comment(cx);
        editor.update(cx, |editor, cx| {
            editor.set_diff_review_without_feature_flag(can_comment, cx)
        });
        let old_block_ids = std::mem::take(&mut self.block_ids);
        self.threads_outside_diff = 0;

        let threads = match self.store.read(cx).loaded() {
            Some(loaded) if self.visible => loaded.threads.clone(),
            _ => Vec::new(),
        };
        let mut placed = Vec::new();
        if let Some(diff) = self.diff.upgrade() {
            let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
            for thread in threads {
                match place_thread(&thread, &diff, &snapshot, cx) {
                    Some(placement) => placed.push((thread, placement)),
                    None => self.threads_outside_diff += 1,
                }
            }
        }

        let used_comment_ids = placed
            .iter()
            .flat_map(|(thread, _)| thread.comments.iter().map(|comment| comment.id.clone()))
            .collect::<HashSet<_>>();
        let placed_thread_ids = placed
            .iter()
            .map(|(thread, _)| thread.id.clone())
            .collect::<HashSet<_>>();
        self.reply_editors
            .retain(|thread_id, _| placed_thread_ids.contains(thread_id));
        for (thread, _) in &placed {
            if thread.can_reply && !self.reply_editors.contains_key(&thread.id) {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::auto_height(1, 10, window, cx);
                    editor.set_placeholder_text("Reply…", window, cx);
                    editor
                });
                self.reply_editors.insert(thread.id.clone(), editor);
            }
        }
        let busy_thread_ids = placed
            .iter()
            .filter(|(thread, _)| self.store.read(cx).is_thread_busy(&thread.id))
            .map(|(thread, _)| thread.id.clone())
            .collect::<HashSet<_>>();
        let view = cx.weak_entity();
        let blocks = placed
            .into_iter()
            .map(|(thread, placement)| {
                let comments = thread
                    .comments_with_depth()
                    .into_iter()
                    .map(|(comment, depth)| {
                        (comment.clone(), depth, self.markdown_for(comment, cx))
                    })
                    .collect::<Vec<_>>();
                let card = ThreadCard {
                    expanded: self.is_expanded(&thread),
                    reply_editor: self.reply_editors.get(&thread.id).cloned(),
                    busy: busy_thread_ids.contains(&thread.id),
                    error: self.thread_errors.get(&thread.id).cloned(),
                    thread,
                    comments,
                    line_visible: placement.line_visible,
                    view: view.clone(),
                };
                BlockProperties {
                    placement: if placement.above {
                        BlockPlacement::Above(placement.anchor)
                    } else {
                        BlockPlacement::Below(placement.anchor)
                    },
                    // The editor measures the card and resizes the block to fit.
                    height: Some(card.estimated_height()),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| card.render(cx)),
                    priority: 0,
                }
            })
            .collect::<Vec<_>>();

        self.markdown
            .retain(|comment_id, _| used_comment_ids.contains(comment_id));
        editor.update(cx, |editor, cx| {
            editor.remove_blocks(old_block_ids.into_iter().collect(), None, cx);
            self.block_ids = editor.insert_blocks(blocks, None, cx);
        });
    }
}

struct ThreadPlacement {
    anchor: Anchor,
    above: bool,
    /// False when the thread's line isn't part of the diff, so the card is
    /// shown at the top of its file instead.
    line_visible: bool,
}

fn place_thread(
    thread: &ReviewThread,
    diff: &Entity<DiffMultibuffer>,
    snapshot: &MultiBufferSnapshot,
    cx: &App,
) -> Option<ThreadPlacement> {
    let (buffer, buffer_diff) = diff.read(cx).buffer_and_diff_for_path(&thread.path)?;
    let buffer = buffer.read(cx).snapshot();
    let lines = thread
        .lines
        .clone()
        .or_else(|| thread.original_lines.clone());

    if let Some(lines) = lines {
        let last_row = lines.end().saturating_sub(1);
        match thread.side {
            DiffSide::New => {
                let row = last_row.min(buffer.max_point().row);
                let anchor = buffer.anchor_after(Point::new(row, buffer.line_len(row)));
                if let Some(anchor) = snapshot.anchor_in_excerpt(anchor) {
                    return Some(ThreadPlacement {
                        anchor,
                        above: false,
                        line_visible: true,
                    });
                }
            }
            DiffSide::Old => {
                let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
                let base_text = diff_snapshot.base_text();
                let base_row = last_row.min(base_text.max_point().row);
                let base_offset = base_text.point_to_offset(Point::new(base_row, 0));
                let hunk = diff_snapshot
                    .hunks_intersecting_range(
                        language::Anchor::min_max_range_for_buffer(buffer.remote_id()),
                        &buffer,
                    )
                    .find(|hunk| {
                        hunk.diff_base_byte_range.start <= base_offset
                            && base_offset <= hunk.diff_base_byte_range.end
                    });
                if let Some(anchor) =
                    hunk.and_then(|hunk| snapshot.anchor_in_excerpt(hunk.buffer_range.start))
                {
                    return Some(ThreadPlacement {
                        anchor,
                        above: true,
                        line_visible: true,
                    });
                }
            }
        }
    }

    let first_excerpt = snapshot.excerpts_for_buffer(buffer.remote_id()).next()?;
    Some(ThreadPlacement {
        anchor: snapshot.anchor_in_excerpt(first_excerpt.context.start)?,
        above: true,
        line_visible: false,
    })
}

struct ThreadCard {
    thread: ReviewThread,
    comments: Vec<(ReviewComment, usize, Entity<Markdown>)>,
    expanded: bool,
    line_visible: bool,
    reply_editor: Option<Entity<Editor>>,
    busy: bool,
    error: Option<SharedString>,
    view: WeakEntity<ReviewThreads>,
}

impl ThreadCard {
    fn estimated_height(&self) -> u32 {
        if self.expanded {
            4 + self.comments.len() as u32 * 3
        } else {
            2
        }
    }

    fn line_label(&self) -> SharedString {
        let lines = self
            .thread
            .lines
            .clone()
            .or_else(|| self.thread.original_lines.clone());
        let label = match lines {
            Some(lines) if lines.start() == lines.end() => format!("Line {}", lines.end()),
            Some(lines) => format!("Lines {}–{}", lines.start(), lines.end()),
            None => "File".to_string(),
        };
        let side = match self.thread.side {
            DiffSide::Old => " (removed)",
            DiffSide::New => "",
        };
        format!("{label}{side}").into()
    }

    fn render(&self, cx: &mut BlockContext) -> AnyElement {
        let is_open = !self.thread.is_resolved;
        let (accent_border, background, divider) = {
            let colors = cx.theme().colors();
            let status = cx.theme().status();
            (
                if is_open {
                    status.info_border
                } else {
                    colors.border_variant
                },
                if is_open {
                    colors.editor_background
                } else {
                    colors.surface_background
                },
                colors.border_variant,
            )
        };
        let left_padding = cx.margins.gutter.full_width() + cx.em_width * 2.;
        let id = SharedString::from(format!("review-thread-{}", self.thread.id.0));

        let header = self.render_header(cx.app);
        let mut card = v_flex()
            .id(id)
            .w_full()
            .max_w(rems(52.))
            .rounded_lg()
            .border_1()
            .border_color(accent_border)
            .bg(background)
            .overflow_hidden()
            .child(header);

        if self.expanded {
            let markdown_style = MarkdownStyle::themed(MarkdownFont::Agent, cx.window, cx.app);
            let now = OffsetDateTime::now_utc();
            let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
            let mut comments = Vec::with_capacity(self.comments.len());
            for (comment, depth, markdown) in &self.comments {
                comments.push(render_comment(
                    comment,
                    (*depth).min(MAX_REPLY_INDENT),
                    markdown,
                    markdown_style.clone(),
                    now,
                    local_offset,
                    divider,
                ));
            }
            card = card.child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_3()
                    .border_t_1()
                    .border_color(divider)
                    .children(comments),
            );
            card = card.child(self.render_footer(cx.app));
        }

        h_flex()
            .w_full()
            .pl(left_padding)
            .pr_4()
            .py_1()
            .child(card)
            .into_any_element()
    }

    fn render_footer(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let thread_id = self.thread.id.clone();
        let is_resolved = self.thread.is_resolved;
        let can_resolve = self.thread.can_resolve;
        let busy = self.busy;
        let id_suffix = thread_id.0.clone();

        v_flex()
            .px_3()
            .pb_2()
            .gap_1p5()
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when_some(self.reply_editor.clone(), |this, editor| {
                let view = self.view.clone();
                let thread_id = thread_id.clone();
                this.child(
                    div()
                        .key_context("ReviewThreadReply")
                        .on_action(move |_: &SubmitReviewReply, window, cx| {
                            view.update(cx, |view, cx| {
                                view.submit_reply(thread_id.clone(), window, cx)
                            })
                            .log_err();
                        })
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.editor_background)
                        .child(editor),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .when(can_resolve, |this| {
                        let view = self.view.clone();
                        let thread_id = thread_id.clone();
                        this.child(
                            Button::new(
                                SharedString::from(format!("review-thread-resolve-{id_suffix}")),
                                if is_resolved { "Unresolve" } else { "Resolve" },
                            )
                            .start_icon(
                                Icon::new(if is_resolved {
                                    IconName::Undo
                                } else {
                                    IconName::Check
                                })
                                .size(IconSize::Small)
                                .color(if is_resolved {
                                    Color::Muted
                                } else {
                                    Color::Success
                                }),
                            )
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |view, cx| {
                                    view.set_resolved(thread_id.clone(), !is_resolved, window, cx)
                                })
                                .log_err();
                            }),
                        )
                    })
                    .child(div().flex_1())
                    .when(self.reply_editor.is_some(), |this| {
                        let view = self.view.clone();
                        let thread_id = thread_id.clone();
                        this.child(
                            Button::new(
                                SharedString::from(format!("review-thread-reply-{id_suffix}")),
                                if busy { "Posting…" } else { "Reply" },
                            )
                            .style(ButtonStyle::Filled)
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                view.update(cx, |view, cx| {
                                    view.submit_reply(thread_id.clone(), window, cx)
                                })
                                .log_err();
                            }),
                        )
                    }),
            )
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status = cx.theme().status();
        let thread = self.thread.clone();
        let is_open = !thread.is_resolved;
        let view = self.view.clone();
        let comment_count = thread.comments.len();
        let first_comment = thread.first_comment().cloned();
        let url = first_comment
            .as_ref()
            .and_then(|comment| comment.url.clone());
        let disclosure_id = SharedString::from(format!("review-thread-toggle-{}", thread.id.0));
        let link_id = SharedString::from(format!("review-thread-link-{}", thread.id.0));

        let summary: SharedString = if is_open || self.expanded {
            format!(
                "{} comment{}",
                comment_count,
                if comment_count == 1 { "" } else { "s" }
            )
            .into()
        } else {
            let preview = first_comment
                .as_ref()
                .map(|comment| {
                    let author = comment
                        .author
                        .as_ref()
                        .map(|author| author.login.to_string())
                        .unwrap_or_else(|| "ghost".to_string());
                    let first_line = comment.body.lines().next().unwrap_or_default();
                    format!(" · {author}: {first_line}")
                })
                .unwrap_or_default();
            format!("{comment_count} comments{preview}").into()
        };

        h_flex()
            .w_full()
            .gap_2()
            .px_3()
            .py_1p5()
            .when(is_open, |this| this.bg(status.info_background))
            .child(if is_open {
                Icon::new(IconName::Chat)
                    .size(IconSize::Small)
                    .color(Color::Info)
            } else {
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success)
            })
            .when_some(
                thread.resolved_by.clone().filter(|_| !is_open),
                |this, resolved_by| {
                    this.child(
                        Label::new(format!("Resolved by {resolved_by}"))
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                },
            )
            .child(
                Label::new(self.line_label())
                    .size(LabelSize::Small)
                    .color(if is_open {
                        Color::Default
                    } else {
                        Color::Muted
                    }),
            )
            .when(thread.is_outdated, |this| {
                this.child(
                    Chip::new("Outdated")
                        .label_color(Color::Warning)
                        .bg_color(status.warning_background)
                        .border_color(status.warning_border),
                )
            })
            .when(!self.line_visible, |this| {
                this.child(
                    Label::new("not in the visible diff")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                div().flex_1().min_w_0().overflow_hidden().child(
                    Label::new(summary)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .when_some(url, |this, url| {
                this.child(
                    IconButton::new(link_id, IconName::ArrowUpRight)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Open on the web"))
                        .on_click(move |_, _, cx| cx.open_url(url.as_str())),
                )
            })
            .child(
                Disclosure::new(disclosure_id, self.expanded).on_click(move |_, window, cx| {
                    view.update(cx, |view, cx| view.toggle_collapsed(&thread, window, cx))
                        .log_err();
                }),
            )
            .border_b_1()
            .border_color(if self.expanded {
                colors.border_variant
            } else {
                gpui::transparent_black()
            })
    }
}

fn render_comment(
    comment: &ReviewComment,
    depth: usize,
    markdown: &Entity<Markdown>,
    markdown_style: MarkdownStyle,
    now: OffsetDateTime,
    local_offset: UtcOffset,
    reply_rule: gpui::Hsla,
) -> AnyElement {
    let login = comment
        .author
        .as_ref()
        .map(|author| author.login.clone())
        .unwrap_or_else(|| "ghost".into());
    let avatar_url = comment
        .author
        .as_ref()
        .and_then(|author| author.avatar_url.clone());
    let timestamp = time_format::format_localized_timestamp(
        comment.created_at,
        now,
        local_offset,
        time_format::TimestampFormat::Relative,
    );

    h_flex()
        .items_start()
        .gap_2()
        .when(depth > 0, |this| {
            this.ml(rems(depth as f32 * 1.5))
                .pl_2()
                .border_l_2()
                .border_color(reply_rule)
        })
        .child(match avatar_url {
            Some(url) => Avatar::new(url.to_string())
                .size(rems(1.25))
                .into_any_element(),
            None => Icon::new(IconName::Person)
                .size(IconSize::Medium)
                .color(Color::Muted)
                .into_any_element(),
        })
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(
                            Label::new(login)
                                .size(LabelSize::Small)
                                .weight(gpui::FontWeight::SEMIBOLD),
                        )
                        .when(comment.role == CommentAuthorRole::Author, |this| {
                            this.child(Chip::new("Author").label_color(Color::Accent))
                        })
                        .child(
                            Label::new(timestamp)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(MarkdownElement::new(markdown.clone(), markdown_style)),
        )
        .into_any_element()
}

/// Asks for an API token when none of the provider's sources had one.
pub(crate) struct ConnectReviewHostModal {
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    store: Entity<ReviewStore>,
    provider_name: SharedString,
    attempts: Vec<CredentialAttempt>,
}

impl ConnectReviewHostModal {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        store: Entity<ReviewStore>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let ReviewState::SignedOut {
            provider_name,
            attempts,
        } = store.read(cx).state().clone()
        else {
            return;
        };
        let workspace_handle = cx.weak_entity();
        workspace.toggle_modal(window, cx, |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Paste a personal access token", window, cx);
                editor.set_masked(true, cx);
                editor
            });
            Self {
                workspace: workspace_handle,
                editor,
                store,
                provider_name,
                attempts,
            }
        });
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.editor.read(cx).text(cx);
        if token.trim().is_empty() {
            return;
        }
        let task = self
            .store
            .update(cx, |store, cx| store.save_token(token, cx));
        task.detach_and_notify_err(self.workspace.clone(), window, cx);
        cx.emit(DismissEvent);
    }
}

impl EventEmitter<DismissEvent> for ConnectReviewHostModal {}
impl ModalView for ConnectReviewHostModal {}

impl Focusable for ConnectReviewHostModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for ConnectReviewHostModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let attempts = self.attempts.iter().map(|attempt| {
            let (icon, color, detail) = match &attempt.outcome {
                CredentialOutcome::Found => (IconName::Check, Color::Success, "found".into()),
                CredentialOutcome::NotFound => {
                    (IconName::Close, Color::Muted, SharedString::from("not set"))
                }
                CredentialOutcome::Failed(error) => (IconName::Close, Color::Error, error.clone()),
            };
            h_flex()
                .gap_2()
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                .child(
                    Label::new(attempt.source.label())
                        .size(LabelSize::Small)
                        .buffer_font(cx),
                )
                .child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
        });

        v_flex()
            .key_context("ConnectReviewHostModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .w(rems(34.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(Icon::new(IconName::Github).size(IconSize::Small))
                    .child(
                        Headline::new(format!("Connect {}", self.provider_name))
                            .size(HeadlineSize::XSmall),
                    ),
            )
            .child(
                Label::new("No token was found to read this branch's review threads:")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(v_flex().gap_1().children(attempts))
            .child(
                Label::new("Paste a token below to save it in your keychain.")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.editor.clone()),
            )
    }
}

/// Submits the gutter comments as one review, with a verdict.
pub(crate) struct FinishReviewModal {
    review_threads: WeakEntity<ReviewThreads>,
    store: Entity<ReviewStore>,
    summary: Entity<Editor>,
    verdict: ReviewVerdict,
    drafts: Vec<DraftComment>,
    title: SharedString,
    viewer_is_author: bool,
    supports_verdicts: bool,
    head_matches: bool,
    submitting: bool,
    error: Option<SharedString>,
}

impl FinishReviewModal {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        review_threads: Entity<ReviewThreads>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let store = review_threads.read(cx).store().clone();
        let Some(loaded) = store.read(cx).loaded().cloned() else {
            return;
        };
        let drafts = review_threads.read(cx).drafts(cx);
        workspace.toggle_modal(window, cx, |window, cx| {
            let summary = cx.new(|cx| {
                let mut editor = Editor::auto_height(2, 8, window, cx);
                editor.set_placeholder_text("Leave a comment (Markdown supported)", window, cx);
                editor
            });
            Self {
                review_threads: review_threads.downgrade(),
                store,
                summary,
                verdict: ReviewVerdict::Comment,
                drafts,
                title: format!("#{} {}", loaded.review.number, loaded.review.title).into(),
                viewer_is_author: loaded.review.viewer_is_author(),
                supports_verdicts: loaded.capabilities.verdicts,
                head_matches: loaded.local_head_matches,
                submitting: false,
                error: None,
            }
        });
    }

    fn verdict_allowed(&self, verdict: ReviewVerdict) -> bool {
        verdict == ReviewVerdict::Comment || (self.supports_verdicts && !self.viewer_is_author)
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.submitting {
            return;
        }
        if !self.head_matches && !self.drafts.is_empty() {
            self.error = Some(
                "Push your branch first: your comments are placed on local lines that differ from the pull request."
                    .into(),
            );
            cx.notify();
            return;
        }
        let body = self.summary.read(cx).text(cx);
        if body.trim().is_empty()
            && self.drafts.is_empty()
            && self.verdict == ReviewVerdict::Comment
        {
            self.error = Some("Write a summary or add comments to submit.".into());
            cx.notify();
            return;
        }
        let submission = ReviewSubmission {
            verdict: self.verdict,
            body,
            comments: self.drafts.clone(),
        };
        self.submitting = true;
        self.error = None;
        cx.notify();
        let task = self
            .store
            .update(cx, |store, cx| store.submit_review(submission, cx));
        let review_threads = self.review_threads.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    review_threads
                        .update(cx, |review_threads, cx| review_threads.clear_drafts(cx))
                        .log_err();
                    cx.emit(DismissEvent);
                }
                Err(error) => {
                    this.submitting = false;
                    this.error = Some(format!("Couldn't submit: {error:#}").into());
                    cx.notify();
                }
            })
        })
        .detach_and_log_err(cx);
    }
}

impl EventEmitter<DismissEvent> for FinishReviewModal {}
impl ModalView for FinishReviewModal {}

impl Focusable for FinishReviewModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.summary.focus_handle(cx)
    }
}

impl Render for FinishReviewModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let verdicts = [
            (
                ReviewVerdict::Comment,
                "Comment",
                "Post your comments without approving or blocking.",
                Color::Default,
            ),
            (
                ReviewVerdict::Approve,
                "Approve",
                "Approve these changes for merging.",
                Color::Success,
            ),
            (
                ReviewVerdict::RequestChanges,
                "Request Changes",
                "Ask for changes before this can merge.",
                Color::Error,
            ),
        ];
        let verdict_rows = verdicts
            .into_iter()
            .map(|(verdict, title, description, color)| {
                let allowed = self.verdict_allowed(verdict);
                let selected = self.verdict == verdict;
                h_flex()
                    .id(SharedString::from(format!("finish-review-{title}")))
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if selected {
                        colors.border_focused
                    } else {
                        colors.border_variant
                    })
                    .when(selected, |this| this.bg(colors.element_selected))
                    .when(allowed, |this| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.verdict = verdict;
                                cx.notify();
                            }))
                    })
                    .when(!allowed, |this| this.opacity(0.5))
                    .child(
                        Icon::new(if selected {
                            IconName::Check
                        } else {
                            IconName::Circle
                        })
                        .size(IconSize::Small)
                        .color(if selected {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                    )
                    .child(
                        v_flex()
                            .child(Label::new(title).color(if allowed {
                                color
                            } else {
                                Color::Muted
                            }))
                            .child(
                                Label::new(description)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
            });

        let pending = self.drafts.iter().map(|draft| {
            let lines = if draft.lines.start() == draft.lines.end() {
                format!("{}:{}", draft.path.as_unix_str(), draft.lines.end())
            } else {
                format!(
                    "{}:{}–{}",
                    draft.path.as_unix_str(),
                    draft.lines.start(),
                    draft.lines.end()
                )
            };
            h_flex()
                .gap_2()
                .child(
                    Label::new(lines)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(draft.body.lines().next().unwrap_or_default().to_string())
                            .size(LabelSize::Small)
                            .truncate(),
                    ),
                )
        });
        let pending_label = match self.drafts.len() {
            0 => "No pending comments".to_string(),
            1 => "1 pending comment will be posted".to_string(),
            count => format!("{count} pending comments will be posted"),
        };
        let submit_label = match self.verdict {
            _ if self.submitting => "Submitting…",
            ReviewVerdict::Comment => "Submit Comments",
            ReviewVerdict::Approve => "Approve",
            ReviewVerdict::RequestChanges => "Request Changes",
        };

        v_flex()
            .key_context("FinishReviewModal")
            .on_action(cx.listener(Self::cancel))
            .elevation_2(cx)
            .w(rems(36.))
            .p_3()
            .gap_3()
            .child(
                v_flex()
                    .child(Headline::new("Finish Review").size(HeadlineSize::Small))
                    .child(
                        Label::new(self.title.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .child(self.summary.clone()),
            )
            .child(v_flex().gap_1().children(verdict_rows))
            .when(self.viewer_is_author, |this| {
                this.child(
                    Label::new("This is your pull request, so you can only comment.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new(pending_label)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(pending),
            )
            .when(!self.head_matches, |this| {
                this.child(
                    Label::new("Your checkout isn't at the pull request's latest commit.")
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("finish-review-cancel", "Cancel")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("finish-review-submit", submit_label)
                            .style(ButtonStyle::Filled)
                            .disabled(self.submitting)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
