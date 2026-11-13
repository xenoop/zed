use crate::{
    branch_picker,
    diff_multibuffer::DiffMultibuffer,
    git_panel::GitStatusEntry,
    project_diff::{
        self, CompareWithBranch, DeployBranchDiff, DiffSinceReviewed, ProjectDiff, ReviewDiff,
        render_send_review_to_agent_button,
    },
    review_comment_store,
    seen_files::{
        self, GoToNextChangedFile, GoToPreviousChangedFile, MarkAllFilesSeen, ToggleFileSeen,
    },
};
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, anyhow};
use editor::{
    Addon, Editor, EditorEvent, HiddenDiffHunkRenderer, SavedReviewComment, SplittableEditor,
    actions::{SendReviewToAgent, ToggleCommentMode, ToggleReviewCommentsHidden},
};
use git::{repository::DiffType, status::FileStatus};
use gpui::{
    Action, AnyElement, App, AppContext as _, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, MouseButton, Render, SharedString, Subscription, Task, WeakEntity,
};
use language::{BufferId, Capability};
use multi_buffer::{ExcerptBoundaryInfo, MultiBuffer};
use project::{
    Project, ProjectPath,
    git_store::{
        Repository,
        diff_buffer_list::{self, DiffBase},
    },
};
use settings::{GitDiffBaseSetting, Settings};
use std::{
    any::{Any, TypeId},
    sync::Arc,
};
use ui::{Checkbox, ContextMenu, DiffStat, Divider, PopoverMenu, Tooltip, prelude::*};
use workspace::{
    ItemHandle, ItemNavHistory, SerializableItem, ToolbarItemEvent, ToolbarItemLocation,
    ToolbarItemView, Workspace,
    item::{Item, ItemEvent, SaveOptions},
    notifications::NotifyTaskExt,
    searchable::SearchableItemHandle,
};
use zed_actions::agent::{OpenThreadChanges, ReviewBranchDiff};

/// The ref that `xen reviewed` moves to the commit a branch was last reviewed at.
fn reviewed_marker_ref(branch_name: &str) -> String {
    format!("refs/xen/reviewed/{branch_name}")
}

/// The workspace item for a branch (merge-base) diff: "Changes since {branch}".
/// It wraps a single [`DiffMultibuffer`] over [`DiffBase::Merge`] and delegates
/// the [`Item`] surface to it. The merge base can be changed in place via the
/// [`BranchDiffToolbar`]'s branch picker, which reloads without reconfiguring
/// the editor (the merge styling is identical for every base ref).
pub struct BranchDiff {
    diff: Entity<DiffMultibuffer>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    /// Saved review comments whose file is not (yet) in the view.
    unplaced_comments: Vec<SavedReviewComment>,
    comments_loaded: bool,
    _diff_event_subscription: Subscription,
    _seen_subscription: Subscription,
}

struct BranchDiffAddon {
    branch_diff: Entity<diff_buffer_list::DiffBufferList>,
    editor: WeakEntity<Editor>,
    multibuffer: WeakEntity<MultiBuffer>,
}

impl Addon for BranchDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        self.branch_diff
            .read(cx)
            .status_for_buffer_id(buffer_id, cx)
    }

    fn render_buffer_header_controls(
        &self,
        excerpt_info: &ExcerptBoundaryInfo,
        _buffer: &language::BufferSnapshot,
        _window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let multibuffer = self.multibuffer.upgrade()?;
        let buffer_id = excerpt_info.buffer_id();
        let is_seen = seen_files::is_seen(
            self.branch_diff.read(cx),
            multibuffer.read(cx),
            buffer_id,
            cx,
        );
        let branch_diff = self.branch_diff.clone();
        let editor = self.editor.clone();
        let checkbox = Checkbox::new(
            ElementId::NamedInteger("seen-file".into(), buffer_id.to_proto()),
            is_seen.into(),
        )
        .label("Seen")
        .label_size(LabelSize::Small)
        .label_color(Color::Muted)
        .tooltip(Tooltip::for_action_title(
            if is_seen {
                "Mark as Not Seen"
            } else {
                "Mark as Seen"
            },
            &ToggleFileSeen,
        ))
        .on_click(move |_, _, cx| {
            if let Some(editor) = editor.upgrade() {
                seen_files::set_seen(&branch_diff, &editor, &[buffer_id], !is_seen, cx);
            }
        });
        Some(
            h_flex()
                .id(ElementId::NamedInteger(
                    "seen-file-slot".into(),
                    buffer_id.to_proto(),
                ))
                .child(checkbox)
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    // Keep the click from reaching the header, which would toggle the fold.
                    cx.stop_propagation();
                })
                .into_any_element(),
        )
    }
}

impl BranchDiff {
    pub(crate) fn register(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &DeployBranchDiff, window, cx| {
            Self::deploy_branch_diff(workspace, window, cx)
        });
        workspace.register_action(Self::compare_with_branch);
        workspace.register_action(|workspace, _: &DiffSinceReviewed, window, cx| {
            Self::deploy_diff_since_reviewed(workspace, window, cx)
        });
        workspace.register_action(|workspace, _: &OpenThreadChanges, window, cx| {
            Self::deploy_branch_diff(workspace, window, cx)
        });
        workspace::register_serializable_item::<Self>(cx);
    }

    pub(crate) fn deploy_branch_diff(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        telemetry::event!("Git Branch Diff Opened");
        let project = workspace.project().clone();
        let Some(intended_repo) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };

        let default_branch = intended_repo.update(cx, |repo, _| repo.default_branch(true));
        let workspace = cx.entity();
        let workspace_weak = workspace.downgrade();
        window
            .spawn(cx, async move |cx| {
                let base_ref = default_branch.await??;
                workspace.update_in(cx, |workspace, window, cx| {
                    let git_store = project.read(cx).git_store().clone();
                    let Some(base_ref) = base_ref else {
                        ProjectDiff::deploy_at(workspace, None, window, cx);
                        return;
                    };
                    let branch_diff =
                        if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                            Some(git_store.update(cx, |git_store, cx| {
                                git_store.ensure_display_diff(
                                    intended_repo.clone(),
                                    base_ref.clone(),
                                    cx,
                                )
                            }))
                        } else {
                            None
                        };
                    Self::deploy_branch_diff_with_base_ref(
                        workspace,
                        project,
                        intended_repo,
                        base_ref,
                        branch_diff,
                        None,
                        window,
                        cx,
                    );
                })?;

                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace_weak, window, cx);
    }

    fn deploy_diff_since_reviewed(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            Self::deploy_branch_diff(workspace, window, cx);
            return;
        };
        let Some(branch_name) = repository
            .read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.name().to_owned())
        else {
            Self::deploy_branch_diff(workspace, window, cx);
            return;
        };
        let marker: SharedString = reviewed_marker_ref(&branch_name).into();
        let marker_lookup =
            repository.update(cx, |repository, _| repository.show(marker.to_string()));
        let workspace_weak = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                // Over the remote protocol `show` is the only lookup, and it
                // cannot tell a missing ref from other git failures. A branch
                // without a marker is the normal first-review case, so any git
                // error falls back to the default-branch diff (and is logged);
                // a cancelled job is a real error.
                let has_marker = match marker_lookup.await? {
                    Ok(_) => true,
                    Err(error) => {
                        log::info!(
                            "no reviewed marker {marker}, showing changes since the default branch: {error:#}"
                        );
                        false
                    }
                };
                workspace_weak.update_in(cx, |workspace, window, cx| {
                    if has_marker {
                        Self::deploy_branch_diff_with_base_ref(
                            workspace, project, repository, marker, None, None, window, cx,
                        );
                    } else {
                        Self::deploy_branch_diff(workspace, window, cx);
                    }
                })?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(cx.entity().downgrade(), window, cx);
    }

    fn compare_with_branch(
        workspace: &mut Workspace,
        _: &CompareWithBranch,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let project = workspace.project().clone();
        let Some(repository) = project.read(cx).active_repository(cx) else {
            let workspace = cx.entity().downgrade();
            window
                .spawn(cx, async |_cx| {
                    let result: Result<()> = Err(anyhow!("No active repository"));
                    result
                })
                .detach_and_notify_err(workspace, window, cx);
            return;
        };
        let selected_branch = workspace.active_item_as::<Self>(cx).and_then(|item| {
            match item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => Some(base_ref.clone()),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged | DiffBase::Snapshot { .. } => {
                    None
                }
            }
        });
        let workspace_handle = workspace.weak_handle();
        let on_select = Arc::new({
            let repository = repository.clone();
            let workspace = workspace_handle.clone();
            move |branch: git::repository::Branch, window: &mut Window, cx: &mut App| {
                let base_ref: SharedString = branch.name().to_owned().into();
                workspace
                    .update(cx, |workspace, cx| {
                        Self::deploy_branch_diff_with_base_ref(
                            workspace,
                            project.clone(),
                            repository.clone(),
                            base_ref,
                            None,
                            None,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }
        });

        workspace.toggle_modal(window, cx, |window, cx| {
            branch_picker::select_modal(
                workspace_handle,
                Some(repository),
                selected_branch,
                on_select,
                window,
                cx,
            )
        });
    }

    pub(crate) fn deploy_branch_diff_with_base_ref(
        workspace: &mut Workspace,
        project: Entity<Project>,
        intended_repo: Entity<Repository>,
        base_ref: SharedString,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        entry: Option<GitStatusEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            matches!(
                item.diff_base(cx),
                DiffBase::Merge { base_ref: existing_base_ref } if existing_base_ref == &base_ref
            ) && item
                .repo(cx)
                .is_some_and(|repo| repo.read(cx).id == intended_repo.read(cx).id)
                && branch_diff.as_ref().is_none_or(|expected| {
                    item.diff.read(cx).branch_diff().entity_id() == expected.entity_id()
                })
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            if let Some(entry) = entry {
                existing.update(cx, |branch_diff, cx| {
                    branch_diff.move_to_entry(entry, window, cx);
                });
            }
            return;
        }

        let workspace_handle = cx.entity();
        let branch_diff = cx.new(|cx| {
            Self::new_with_base_ref(
                project,
                workspace_handle,
                base_ref,
                Some(intended_repo),
                branch_diff,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(branch_diff.clone()), None, true, window, cx);
        if let Some(entry) = entry {
            branch_diff.update(cx, |branch_diff, cx| {
                branch_diff.move_to_entry(entry, window, cx);
            });
        }
    }

    /// Opens (or focuses) the Changes view comparing the working tree with
    /// exactly `commit`: `HEAD` for "Last Commit", or the snapshot taken when
    /// the last agent turn started for "Last Turn".
    pub(crate) fn deploy_snapshot_diff(
        workspace: &mut Workspace,
        repository: Entity<Repository>,
        commit: SharedString,
        label: SharedString,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let diff_base = DiffBase::Snapshot { commit, label };
        let existing = workspace.items_of_type::<Self>(cx).find(|item| {
            let item = item.read(cx);
            item.diff_base(cx) == &diff_base
                && item
                    .repo(cx)
                    .is_some_and(|repo| repo.read(cx).id == repository.read(cx).id)
        });
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }
        let project = workspace.project().clone();
        let workspace_handle = cx.entity();
        let branch_diff = cx.new(|cx| {
            Self::new_with_diff_base(
                project,
                workspace_handle,
                diff_base,
                Some(repository),
                None,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(branch_diff), None, true, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn new_with_default_branch(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let Some(repo) = project.read(cx).git_store().read(cx).active_repository() else {
            return Task::ready(Err(anyhow!("No active repository")));
        };
        let main_branch = repo.update(cx, |repo, _| repo.default_branch(true));
        window.spawn(cx, async move |cx| {
            let base_ref = main_branch
                .await??
                .context("Could not determine default branch")?;
            cx.update(|window, cx| {
                let git_store = project.read(cx).git_store().clone();
                let branch_diff =
                    if git_store.read(cx).diff_base() == GitDiffBaseSetting::DefaultBranch {
                        Some(git_store.update(cx, |git_store, cx| {
                            git_store.ensure_display_diff(repo.clone(), base_ref.clone(), cx)
                        }))
                    } else {
                        None
                    };
                cx.new(|cx| {
                    Self::new_with_base_ref(
                        project,
                        workspace,
                        base_ref,
                        Some(repo),
                        branch_diff,
                        window,
                        cx,
                    )
                })
            })
        })
    }

    pub(crate) fn new_with_base_ref(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        base_ref: SharedString,
        repo: Option<Entity<Repository>>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_diff_base(
            project,
            workspace,
            DiffBase::Merge { base_ref },
            repo,
            branch_diff,
            window,
            cx,
        )
    }

    fn new_with_diff_base(
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        diff_base: DiffBase,
        repo: Option<Entity<Repository>>,
        branch_diff: Option<Entity<diff_buffer_list::DiffBufferList>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let branch_diff = branch_diff.unwrap_or_else(|| {
            let git_store = project.read(cx).git_store().clone();
            cx.new(|cx| diff_buffer_list::DiffBufferList::new(diff_base, git_store, repo, cx))
        });
        let branch_diff_for_addon = branch_diff.clone();
        let diff = cx.new(|cx| {
            DiffMultibuffer::new(
                branch_diff,
                Capability::ReadWrite,
                "No changes",
                move |editor, cx| {
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, move |rhs_editor, cx| {
                        rhs_editor.set_read_only(false);
                        rhs_editor.set_comment_mode(true, cx);
                        let multibuffer = rhs_editor.buffer().downgrade();
                        rhs_editor.register_addon(BranchDiffAddon {
                            branch_diff: branch_diff_for_addon,
                            editor: cx.weak_entity(),
                            multibuffer,
                        });
                    });
                },
                project.clone(),
                workspace.clone(),
                window,
                cx,
            )
        });
        Self::from_diff(diff, project, workspace, window, cx)
    }

    fn from_diff(
        diff: Entity<DiffMultibuffer>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let diff_event_subscription = cx.subscribe(&diff, |_, _, event: &EditorEvent, cx| {
            cx.emit(event.clone())
        });
        let multibuffer = diff.read(cx).multibuffer().clone();
        let rhs_editor = diff.read(cx).editor().read(cx).rhs_editor().clone();
        let comments_subscription =
            cx.subscribe(&rhs_editor, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::ReviewCommentsChanged { .. } = event {
                    this.save_comments(cx);
                }
            });
        let seen_subscription = cx.subscribe_in(
            &multibuffer,
            window,
            |this, _, event: &multi_buffer::Event, window, cx| match event {
                multi_buffer::Event::BuffersEdited { .. }
                | multi_buffer::Event::BufferDiffChanged
                | multi_buffer::Event::Reloaded => {
                    let list = this.diff.read(cx).branch_diff().clone();
                    seen_files::clear_stale_marks(&list, &this.rhs_editor(cx), cx);
                    cx.notify();
                }
                multi_buffer::Event::BufferRangesUpdated { buffer, .. } => {
                    // A file that was marked seen earlier (in a closed tab, say)
                    // starts folded.
                    let buffer_id = buffer.read(cx).remote_id();
                    let editor = this.rhs_editor(cx);
                    let list = this.diff.read(cx).branch_diff().clone();
                    seen_files::load(&list, cx);
                    let is_seen = seen_files::is_seen(
                        list.read(cx),
                        editor.read(cx).buffer().read(cx),
                        buffer_id,
                        cx,
                    );
                    if is_seen && !editor.read(cx).is_buffer_folded(buffer_id, cx) {
                        editor.update(cx, |editor, cx| editor.fold_buffer(buffer_id, cx));
                    }
                    // After the editor has laid out the new file.
                    cx.defer_in(window, |this, window, cx| this.restore_comments(window, cx));
                }
                _ => {}
            },
        );
        Self {
            diff,
            project,
            workspace: workspace.downgrade(),
            unplaced_comments: Vec::new(),
            comments_loaded: false,
            _diff_event_subscription: diff_event_subscription,
            _seen_subscription: Subscription::join(seen_subscription, comments_subscription),
        }
    }

    fn comments_key(&self, cx: &App) -> Option<String> {
        let repository = self.repo(cx)?;
        Some(review_comment_store::key_for_repository(&repository, cx))
    }

    fn load_comments(&mut self, cx: &mut Context<Self>) {
        if self.comments_loaded {
            return;
        }
        let Some(key) = self.comments_key(cx) else {
            return;
        };
        self.comments_loaded = true;
        self.unplaced_comments = review_comment_store::load(&key, cx);
    }

    /// Puts saved comments back on files that have appeared in the view.
    fn restore_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_comments(cx);
        if self.unplaced_comments.is_empty() {
            return;
        }
        let saved = std::mem::take(&mut self.unplaced_comments);
        let editor = self.rhs_editor(cx);
        let unplaced = editor.update(cx, |editor, cx| {
            editor.restore_review_comments(saved, window, cx)
        });
        // Restoring emits a change, which saves; keep what that save saw.
        self.unplaced_comments.extend(unplaced);
    }

    /// Saves the comments of the files this view shows; the comments of
    /// other files (made in file tabs, say) stay as they are.
    fn save_comments(&mut self, cx: &mut Context<Self>) {
        self.load_comments(cx);
        let Some(key) = self.comments_key(cx) else {
            return;
        };
        let editor = self.rhs_editor(cx);
        let comments = editor.read(cx).saved_review_comments(cx);
        let files: collections::HashSet<String> = editor
            .read(cx)
            .buffer()
            .read(cx)
            .all_buffers()
            .into_iter()
            .filter_map(|buffer| Some(buffer.read(cx).file()?.path().as_unix_str().to_string()))
            .collect();
        review_comment_store::save_for_files(key, &files, comments, cx);
    }

    fn rhs_editor(&self, cx: &App) -> Entity<Editor> {
        self.diff.read(cx).editor().read(cx).rhs_editor().clone()
    }

    fn toggle_file_seen(
        &mut self,
        _: &ToggleFileSeen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.rhs_editor(cx);
        let Some(buffer) = editor.read(cx).active_buffer(cx) else {
            return;
        };
        let buffer_id = buffer.read(cx).remote_id();
        let list = self.diff.read(cx).branch_diff().clone();
        let is_seen = seen_files::is_seen(
            list.read(cx),
            editor.read(cx).buffer().read(cx),
            buffer_id,
            cx,
        );
        seen_files::set_seen(&list, &editor, &[buffer_id], !is_seen, cx);
        if !is_seen {
            self.go_to_adjacent_file(true, window, cx);
        }
        cx.notify();
    }

    fn mark_all_files_seen(
        &mut self,
        _: &MarkAllFilesSeen,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = self.rhs_editor(cx);
        let list = self.diff.read(cx).branch_diff().clone();
        let buffer_ids = seen_files::ordered_buffer_ids(editor.read(cx).buffer().read(cx), cx);
        seen_files::set_seen(&list, &editor, &buffer_ids, true, cx);
        cx.notify();
    }

    fn go_to_adjacent_file(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.rhs_editor(cx);
        let Some(buffer_id) = seen_files::adjacent_buffer_id(editor.read(cx), forward, cx) else {
            return;
        };
        let Some(path_key) = editor
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx)
            .path_for_buffer(buffer_id)
            .cloned()
        else {
            return;
        };
        self.diff
            .update(cx, |diff, cx| diff.move_to_path(path_key, window, cx));
    }

    /// How many files are marked seen, out of how many.
    fn seen_progress(&self, cx: &App) -> (usize, usize) {
        let diff = self.diff.read(cx);
        seen_files::progress(diff.branch_diff().read(cx), diff.multibuffer().read(cx), cx)
    }

    pub(crate) fn diff_base<'a>(&'a self, cx: &'a App) -> &'a DiffBase {
        self.diff.read(cx).diff_base(cx)
    }

    pub(crate) fn repo(&self, cx: &App) -> Option<Entity<Repository>> {
        self.diff.read(cx).repo(cx)
    }

    pub(crate) fn branch_diff_list(&self, cx: &App) -> Entity<diff_buffer_list::DiffBufferList> {
        self.diff.read(cx).branch_diff().clone()
    }

    pub(crate) fn move_to_entry(
        &mut self,
        entry: GitStatusEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.move_to_entry(entry, window, cx));
    }

    pub(crate) fn autoscroll(&self, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| diff.autoscroll(cx));
    }

    fn set_merge_base(&mut self, base_ref: SharedString, cx: &mut Context<Self>) {
        self.diff.update(cx, |diff, cx| {
            diff.branch_diff().update(cx, |branch_diff, cx| {
                branch_diff.set_diff_base(DiffBase::Merge { base_ref }, cx);
            });
        });
    }

    fn review_diff(&mut self, _: &ReviewDiff, window: &mut Window, cx: &mut Context<Self>) {
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return;
        };
        let Some(repo) = self.repo(cx) else {
            return;
        };

        let diff_receiver = repo.update(cx, |repo, cx| {
            repo.diff(
                DiffType::MergeBase {
                    base_ref: base_ref.clone(),
                },
                cx,
            )
        });

        let workspace = self.workspace.clone();
        window
            .spawn(cx, {
                let workspace = workspace.clone();
                async move |cx| {
                    let diff_text = diff_receiver.await??;

                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update_in(cx, |_workspace, window, cx| {
                            window.dispatch_action(
                                ReviewBranchDiff {
                                    diff_text: diff_text.into(),
                                    base_ref,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        })?;
                    }

                    anyhow::Ok(())
                }
            })
            .detach_and_notify_err(workspace, window, cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn editor(&self, cx: &App) -> Entity<SplittableEditor> {
        self.diff.read(cx).editor().clone()
    }
}

impl EventEmitter<EditorEvent> for BranchDiff {}

impl Focusable for BranchDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff.read(cx).focus_handle(cx)
    }
}

impl Item for BranchDiff {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch).color(Color::Muted))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff
            .update(cx, |diff, cx| diff.deactivated(window, cx));
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.diff
            .update(cx, |diff, cx| diff.navigate(data, window, cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(self.tab_content_text(0, cx))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        match self.diff_base(cx) {
            DiffBase::Merge { base_ref } => format!("Changes since {}", base_ref).into(),
            DiffBase::Snapshot { label, .. } => format!("Changes since {label}").into(),
            DiffBase::Head | DiffBase::Index | DiffBase::Staged => "Changes".into(),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Branch Diff Opened")
    }

    fn as_searchable(&self, _: &Entity<Self>, cx: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.diff.read(cx).editor().clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.diff.read(cx).for_each_project_item(cx, f);
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.diff.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff
            .update(cx, |diff, cx| diff.set_nav_history(nav_history, cx));
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let diff_base = self.diff_base(cx).clone();
        let repo = self.repo(cx);
        let project = self.project.clone();
        let branch_diff = self.diff.read(cx).branch_diff().clone();
        Task::ready(Some(cx.new(|cx| {
            Self::new_with_diff_base(
                project,
                workspace,
                diff_base,
                repo,
                Some(branch_diff),
                window,
                cx,
            )
        })))
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.diff.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.diff.read(cx).has_conflict(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.save(options, project, window, cx))
    }

    fn save_as(
        &mut self,
        _: Entity<Project>,
        _: ProjectPath,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Task<Result<()>> {
        unreachable!()
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.diff
            .update(cx, |diff, cx| diff.reload(project, window, cx))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<DiffMultibuffer>() {
            Some(self.diff.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(
                self.diff
                    .read(cx)
                    .editor()
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .into(),
            )
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff.read(cx).editor().clone().into())
        } else if type_id == TypeId::of::<diff_buffer_list::DiffBufferList>() {
            Some(self.diff.read(cx).branch_diff().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff.update(cx, |diff, cx| {
            diff.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for BranchDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("ChangesView")
            .on_action(cx.listener(Self::review_diff))
            // The editor toggles the mode; this only redraws the toolbar's toggle.
            .capture_action(cx.listener(|_, _: &ToggleCommentMode, _, cx| cx.notify()))
            .capture_action(cx.listener(|_, _: &ToggleReviewCommentsHidden, _, cx| cx.notify()))
            .on_action(cx.listener(Self::toggle_file_seen))
            .on_action(cx.listener(Self::mark_all_files_seen))
            .on_action(cx.listener(|this, _: &GoToNextChangedFile, window, cx| {
                this.go_to_adjacent_file(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &GoToPreviousChangedFile, window, cx| {
                    this.go_to_adjacent_file(false, window, cx)
                }),
            )
            .child(self.diff.clone())
    }
}

impl SerializableItem for BranchDiff {
    fn serialized_item_kind() -> &'static str {
        "BranchDiff"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        window.spawn(cx, async move |cx| {
            let diff_base = db.get_project_diff_base(item_id, workspace_id)?;
            let DiffBase::Merge { base_ref } = diff_base else {
                anyhow::bail!("expected a merge base for a branch diff");
            };
            let workspace = workspace.upgrade().context("workspace gone")?;
            cx.update(|window, cx| {
                cx.new(|cx| {
                    Self::new_with_base_ref(project, workspace, base_ref, None, None, window, cx)
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let DiffBase::Merge { base_ref } = self.diff_base(cx).clone() else {
            return None;
        };
        let diff_base = DiffBase::Merge { base_ref };
        let db = project_diff::persistence::ProjectDiffDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_project_diff_base(item_id, workspace_id, diff_base)
                .await
        }))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}

pub struct BranchDiffToolbar {
    branch_diff: Option<WeakEntity<BranchDiff>>,
    _branch_diff_observation: Option<Subscription>,
    _seen_observation: Subscription,
}

impl BranchDiffToolbar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            branch_diff: None,
            _branch_diff_observation: None,
            _seen_observation: cx.observe_global::<seen_files::SeenFiles>(|_, cx| cx.notify()),
        }
    }

    fn branch_diff(&self, _: &App) -> Option<Entity<BranchDiff>> {
        self.branch_diff.as_ref()?.upgrade()
    }

    fn dispatch_action(&self, action: &dyn Action, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(branch_diff) = self.branch_diff(cx) {
            branch_diff.focus_handle(cx).focus(window, cx);
        }
        let action = action.boxed_clone();
        cx.defer(move |cx| {
            cx.dispatch_action(action.as_ref());
        })
    }
}

impl EventEmitter<ToolbarItemEvent> for BranchDiffToolbar {}

impl ToolbarItemView for BranchDiffToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        let branch_diff = active_pane_item.and_then(|item| item.act_as::<BranchDiff>(cx));
        self._branch_diff_observation = branch_diff
            .as_ref()
            .map(|branch_diff| cx.observe(branch_diff, |_, _, cx| cx.notify()));
        self.branch_diff = branch_diff.map(|entity| entity.downgrade());
        if self.branch_diff.is_some() {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

impl Render for BranchDiffToolbar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(branch_diff) = self.branch_diff(cx) else {
            return div();
        };
        let focus_handle = branch_diff.focus_handle(cx);
        let review_count = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .total_review_comment_count();
        let (additions, deletions) = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .calculate_changed_lines(cx);
        let diff_base = branch_diff.read(cx).diff_base(cx).clone();
        let compare_menu = render_compare_menu(&branch_diff, &diff_base, cx);
        let DiffBase::Merge { base_ref } = diff_base else {
            // Snapshot comparisons ("Last Commit", "Last Turn") have no base
            // branch to pick and cannot be sent for review as a branch diff.
            return h_flex()
                .my_neg_1()
                .py_1()
                .gap_1p5()
                .when(
                    !branch_diff
                        .read(cx)
                        .diff
                        .read(cx)
                        .multibuffer()
                        .read(cx)
                        .is_empty(),
                    |this| {
                        this.child(DiffStat::new(
                            "branch-diff-stat",
                            additions as usize,
                            deletions as usize,
                        ))
                    },
                )
                .children(render_seen_progress(&branch_diff, cx))
                .child(Divider::vertical().ml_1())
                .child(compare_menu)
                .child(render_comment_mode_toggle(&branch_diff, cx))
                .child(render_comments_hidden_toggle(&branch_diff, cx))
                .when(review_count > 0, |this| {
                    this.child(Divider::vertical()).child(
                        render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.dispatch_action(&SendReviewToAgent, window, cx)
                            }),
                        ),
                    )
                });
        };
        let selected_base_ref = base_ref.clone();
        let base_ref_label = format!("Base: {base_ref}");
        let repository = branch_diff.read(cx).repo(cx);
        let workspace = branch_diff.read(cx).workspace.clone();
        let project = branch_diff.read(cx).project.clone();
        let buffers = branch_diff.read(cx).diff.read(cx).branch_diff().clone();
        let uses_display_diff = repository.as_ref().is_some_and(|repository| {
            project
                .read(cx)
                .git_store()
                .read(cx)
                .display_diff_for_repo(repository.read(cx).id)
                .is_some_and(|display_diff| display_diff.entity_id() == buffers.entity_id())
        });
        let view_for_picker = branch_diff.downgrade();

        let is_multibuffer_empty = branch_diff
            .read(cx)
            .diff
            .read(cx)
            .multibuffer()
            .read(cx)
            .is_empty();
        let is_ai_enabled = AgentSettings::get_global(cx).enabled(cx);

        let show_review_button = !is_multibuffer_empty && is_ai_enabled;

        h_flex()
            .my_neg_1()
            .py_1()
            .gap_1p5()
            .flex_wrap()
            .justify_between()
            .when(!is_multibuffer_empty, |this| {
                this.child(DiffStat::new(
                    "branch-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
            })
            .children(render_seen_progress(&branch_diff, cx))
            .child(Divider::vertical().ml_1())
            .child(compare_menu)
            .child(render_comment_mode_toggle(&branch_diff, cx))
            .child(render_comments_hidden_toggle(&branch_diff, cx))
            .child(
                PopoverMenu::new("branch-diff-base-branch-picker")
                    .menu(move |window, cx| {
                        let view_for_picker = view_for_picker.clone();
                        let workspace_for_select = workspace.clone();
                        let repository_for_select = repository.clone();
                        let project = project.clone();
                        let on_select = Arc::new(
                            move |branch: git::repository::Branch,
                                  window: &mut Window,
                                  cx: &mut App| {
                                let base_ref: SharedString = branch.name().to_owned().into();
                                if uses_display_diff {
                                    let Some(repository) = repository_for_select.clone() else {
                                        return;
                                    };
                                    workspace_for_select
                                        .update(cx, |workspace, cx| {
                                            BranchDiff::deploy_branch_diff_with_base_ref(
                                                workspace,
                                                project.clone(),
                                                repository,
                                                base_ref,
                                                None,
                                                None,
                                                window,
                                                cx,
                                            );
                                        })
                                        .ok();
                                } else {
                                    view_for_picker
                                        .update(cx, |branch_diff, cx| {
                                            branch_diff.set_merge_base(base_ref, cx);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            },
                        );

                        Some(branch_picker::select_popover(
                            workspace.clone(),
                            repository.clone(),
                            Some(selected_base_ref.clone()),
                            on_select,
                            window,
                            cx,
                        ))
                    })
                    .trigger_with_tooltip(
                        Button::new("branch-diff-base-branch", base_ref_label).end_icon(
                            Icon::new(IconName::ChevronDown)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        ),
                        Tooltip::text("Select Base Branch"),
                    ),
            )
            .when(show_review_button, |this| {
                let focus_handle = focus_handle.clone();
                this.child(Divider::vertical()).child(
                    Button::new("review-diff", "Review Diff")
                        .start_icon(
                            Icon::new(IconName::ZedAssistant)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .tooltip(move |_, cx| {
                            Tooltip::with_meta_in(
                                "Review Diff",
                                Some(&ReviewDiff),
                                "Send this diff for your last agent to review.",
                                &focus_handle,
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&ReviewDiff, window, cx);
                        })),
                )
            })
            .when(review_count > 0, |this| {
                this.child(Divider::vertical()).child(
                    render_send_review_to_agent_button(review_count, &focus_handle).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.dispatch_action(&SendReviewToAgent, window, cx)
                        }),
                    ),
                )
            })
    }
}

/// The comparison picker of the Changes view: against the branch base, the
/// last commit, or the working tree when the last agent turn started.
fn render_comment_mode_toggle(branch_diff: &Entity<BranchDiff>, cx: &App) -> impl IntoElement {
    let editor = branch_diff.read(cx).rhs_editor(cx);
    let comment_mode = editor.read(cx).comment_mode();
    IconButton::new("comment-mode", IconName::Chat)
        .icon_size(IconSize::Small)
        .toggle_state(comment_mode)
        .tooltip(Tooltip::for_action_title(
            if comment_mode {
                "Comment Mode: typing starts a comment"
            } else {
                "Edit Mode: typing edits the file"
            },
            &ToggleCommentMode,
        ))
        .on_click(move |_, _, cx| {
            editor.update(cx, |editor, cx| {
                let comment_mode = editor.comment_mode();
                editor.set_comment_mode(!comment_mode, cx);
            });
        })
}

fn render_comments_hidden_toggle(branch_diff: &Entity<BranchDiff>, cx: &App) -> impl IntoElement {
    let editor = branch_diff.read(cx).rhs_editor(cx);
    let hidden = editor.read(cx).review_comments_hidden();
    IconButton::new(
        "comments-hidden",
        if hidden {
            IconName::EyeOff
        } else {
            IconName::Eye
        },
    )
    .icon_size(IconSize::Small)
    .tooltip(Tooltip::for_action_title(
        if hidden {
            "Show Comments"
        } else {
            "Hide Comments"
        },
        &ToggleReviewCommentsHidden,
    ))
    .on_click(move |_, window, cx| {
        editor.update(cx, |editor, cx| {
            let hidden = editor.review_comments_hidden();
            editor.set_review_comments_hidden(!hidden, window, cx);
        });
    })
}

fn render_seen_progress(branch_diff: &Entity<BranchDiff>, cx: &App) -> Option<AnyElement> {
    let (seen, total) = branch_diff.read(cx).seen_progress(cx);
    if total == 0 {
        return None;
    }
    Some(
        Label::new(format!("Seen {seen}/{total}"))
            .size(LabelSize::Small)
            .color(if seen == total {
                Color::Success
            } else {
                Color::Muted
            })
            .into_any_element(),
    )
}

fn render_compare_menu(
    branch_diff: &Entity<BranchDiff>,
    diff_base: &DiffBase,
    cx: &App,
) -> impl IntoElement + use<> {
    let label: SharedString = match diff_base {
        DiffBase::Merge { .. } => "Compare: Branch".into(),
        DiffBase::Snapshot { label, .. } => format!("Compare: {label}").into(),
        DiffBase::Head | DiffBase::Index | DiffBase::Staged => "Compare".into(),
    };
    let current = diff_base.clone();
    let workspace = branch_diff.read(cx).workspace.clone();
    let repository = branch_diff.read(cx).repo(cx);
    let last_turn = repository.as_ref().and_then(|repository| {
        branch_diff
            .read(cx)
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .last_turn_snapshot(repository.read(cx).id)
    });
    PopoverMenu::new("branch-diff-compare")
        .menu(move |window, cx| {
            let workspace = workspace.clone();
            let repository = repository.clone();
            let last_turn = last_turn.clone();
            let current = current.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                let deploy_snapshot = |commit: SharedString, label: &'static str| {
                    let workspace = workspace.clone();
                    let repository = repository.clone();
                    move |window: &mut Window, cx: &mut App| {
                        let Some(repository) = repository.clone() else {
                            return;
                        };
                        workspace
                            .update(cx, |workspace, cx| {
                                BranchDiff::deploy_snapshot_diff(
                                    workspace,
                                    repository,
                                    commit.clone(),
                                    label.into(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    }
                };
                let is_snapshot = |wanted: &str| {
                    matches!(&current, DiffBase::Snapshot { label, .. } if label.as_ref() == wanted)
                };
                menu.toggleable_entry(
                    "Branch base",
                    matches!(current, DiffBase::Merge { .. }),
                    IconPosition::Start,
                    None,
                    {
                        let workspace = workspace.clone();
                        move |window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    BranchDiff::deploy_branch_diff(workspace, window, cx);
                                })
                                .ok();
                        }
                    },
                )
                .toggleable_entry(
                    LAST_COMMIT_LABEL,
                    is_snapshot(LAST_COMMIT_LABEL),
                    IconPosition::Start,
                    None,
                    deploy_snapshot("HEAD".into(), LAST_COMMIT_LABEL),
                )
                .toggleable_entry_disabled_when(
                    LAST_TURN_LABEL,
                    is_snapshot(LAST_TURN_LABEL),
                    last_turn.is_none(),
                    IconPosition::Start,
                    None,
                    deploy_snapshot(last_turn.clone().unwrap_or_default(), LAST_TURN_LABEL),
                )
            }))
        })
        .trigger_with_tooltip(
            Button::new("branch-diff-compare-button", label).end_icon(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            ),
            Tooltip::text("Compare with the branch base, the last commit, or the start of the last agent turn"),
        )
}

const LAST_COMMIT_LABEL: &str = "Last Commit";
const LAST_TURN_LABEL: &str = "Last Turn";

#[cfg(test)]
mod tests {
    use collections::HashMap;
    use editor::test::editor_test_context::assert_state_with_diff;
    use git::status::{FileStatus, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::Path;
    use std::sync::Arc;
    use std::{cell::Cell, rc::Rc};
    use unindent::Unindent as _;
    use util::{
        path,
        rel_path::{RelPath, rel_path},
    };
    use workspace::MultiWorkspace;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // Seen marks are saved to the database: keep each test's own.
            cx.set_global(db::AppDatabase::test_new());
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test(iterations = 50)]
    async fn test_split_diff_conflict_path_transition_with_dirty_buffer_invalid_anchor_panics(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Split);
                });
            });
        });

        let build_conflict_text: fn(usize) -> String = |tag: usize| {
            let mut lines = (0..80)
                .map(|line_index| format!("line {line_index}"))
                .collect::<Vec<_>>();
            for offset in [5usize, 20, 37, 61] {
                lines[offset] = format!("base-{tag}-line-{offset}");
            }
            format!("{}\n", lines.join("\n"))
        };
        let initial_conflict_text = build_conflict_text(0);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "helper.txt": "same\n",
                "conflict.txt": initial_conflict_text,
            }),
        )
        .await;
        fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
            state
                .refs
                .insert("MERGE_HEAD".into(), "conflict-head".into());
        })
        .unwrap();
        fs.set_status_for_repo(
            path!("/project/.git").as_ref(),
            &[(
                "conflict.txt",
                FileStatus::Unmerged(UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }),
            )],
        );
        fs.set_merge_base_content_for_repo(
            path!("/project/.git").as_ref(),
            &[
                ("conflict.txt", build_conflict_text(1)),
                ("helper.txt", "same\n".to_string()),
            ],
        );

        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let _branch_diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/project/conflict.txt"), cx)
            })
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "dirty\n")], None, cx));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        cx.run_until_parked();

        cx.update(|window, cx| {
            let fs = fs.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    fs.with_git_state(path!("/project/.git").as_ref(), true, |state| {
                        state.refs.insert("HEAD".into(), "head-1".into());
                        state.refs.remove("MERGE_HEAD");
                    })
                    .unwrap();
                    fs.set_status_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            (
                                "conflict.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                            (
                                "helper.txt",
                                FileStatus::Tracked(TrackedStatus {
                                    index_status: git::status::StatusCode::Modified,
                                    worktree_status: git::status::StatusCode::Modified,
                                }),
                            ),
                        ],
                    );
                    // FakeFs assigns deterministic OIDs by entry position; flipping order churns
                    // conflict diff identity without reaching into view internals.
                    fs.set_merge_base_content_for_repo(
                        path!("/project/.git").as_ref(),
                        &[
                            ("helper.txt", "helper-base\n".to_string()),
                            ("conflict.txt", build_conflict_text(2)),
                        ],
                    );
                })
                .detach();
        });

        cx.update(|window, cx| {
            let buffer = buffer.clone();
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().simulate_random_delay().await;
                    for edit_index in 0..10 {
                        if edit_index > 0 {
                            cx.background_executor().simulate_random_delay().await;
                        }
                        buffer.update(cx, |buffer, cx| {
                            let len = buffer.len();
                            if edit_index % 2 == 0 {
                                buffer.edit(
                                    [(0..0, format!("status-burst-head-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            } else {
                                buffer.edit(
                                    [(len..len, format!("status-burst-tail-{edit_index}\n"))],
                                    None,
                                    cx,
                                );
                            }
                        });
                    }
                })
                .detach();
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_branch_diff(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        cx.run_until_parked();

        let editor = diff.read_with(cx, |diff, cx| diff.editor(cx).read(cx).rhs_editor().clone());

        let expected_diff = "
                - A
                + C
                + ˇnew
                + created-in-head"
            .unindent();
        assert_state_with_diff(&editor, cx, &expected_diff);

        cx.update(|window, cx| {
            editor.focus_handle(cx).focus(window, cx);
            window.dispatch_action(git::Restore.boxed_clone(), cx);
        });
        cx.run_until_parked();
        assert_state_with_diff(&editor, cx, &expected_diff);

        let statuses: HashMap<Arc<RelPath>, Option<FileStatus>> =
            editor.update(cx, |editor, cx| {
                editor
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .iter()
                    .map(|buffer| {
                        (
                            buffer.read(cx).file().unwrap().path().clone(),
                            editor.status_for_buffer_id(buffer.read(cx).remote_id(), cx),
                        )
                    })
                    .collect()
            });

        assert_eq!(
            statuses,
            HashMap::from_iter([
                (
                    rel_path("a.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Modified,
                        worktree_status: git::status::StatusCode::Modified
                    }))
                ),
                (rel_path("b.txt").into_arc(), Some(FileStatus::Untracked)),
                (
                    rel_path("d.txt").into_arc(),
                    Some(FileStatus::Tracked(TrackedStatus {
                        index_status: git::status::StatusCode::Added,
                        worktree_status: git::status::StatusCode::Added
                    }))
                )
            ])
        );
    }

    #[gpui::test]
    async fn test_branch_diff_action_matches_existing_item_by_base_ref(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let target_branch_diff = cx.update(|window, cx| {
            let repository = project.read(cx).active_repository(cx);
            cx.new(|cx| {
                BranchDiff::new_with_base_ref(
                    project.clone(),
                    workspace.clone(),
                    "topic".into(),
                    repository,
                    None,
                    window,
                    cx,
                )
            })
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(
                Box::new(target_branch_diff.clone()),
                None,
                true,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.update(|window, cx| {
            window.dispatch_action(DeployBranchDiff.boxed_clone(), cx);
        });
        cx.run_until_parked();

        let (active_base_ref, mut base_refs) = workspace.update(cx, |workspace, cx| {
            let active_item = workspace.active_item_as::<BranchDiff>(cx).unwrap();
            let active_base_ref = match active_item.read(cx).diff_base(cx) {
                DiffBase::Merge { base_ref } => base_ref.to_string(),
                DiffBase::Head | DiffBase::Index | DiffBase::Staged | DiffBase::Snapshot { .. } => {
                    panic!("expected active item to be a branch diff")
                }
            };
            let base_refs = workspace
                .items_of_type::<BranchDiff>(cx)
                .filter_map(|item| match item.read(cx).diff_base(cx) {
                    DiffBase::Merge { base_ref } => Some(base_ref.to_string()),
                    DiffBase::Head
                    | DiffBase::Index
                    | DiffBase::Staged
                    | DiffBase::Snapshot { .. } => None,
                })
                .collect::<Vec<_>>();
            (active_base_ref, base_refs)
        });
        base_refs.sort();

        assert_eq!(active_base_ref, "origin/main");
        assert_eq!(base_refs, vec!["origin/main", "topic"]);
    }

    #[gpui::test]
    async fn test_snapshot_diff_reuses_item_per_snapshot(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "changed",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();

        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .expect("test project has a repository");
        project.update(cx, |project, cx| {
            let repo_id = repository.read(cx).id;
            project.git_store().update(cx, |git_store, _| {
                git_store.set_last_turn_snapshot(repo_id, "turn-sha".into());
            });
            assert_eq!(
                project.git_store().read(cx).last_turn_snapshot(repo_id),
                Some("turn-sha".into())
            );
        });

        for (commit, label) in [
            ("HEAD", LAST_COMMIT_LABEL),
            ("turn-sha", LAST_TURN_LABEL),
            ("HEAD", LAST_COMMIT_LABEL),
        ] {
            workspace.update_in(cx, |workspace, window, cx| {
                BranchDiff::deploy_snapshot_diff(
                    workspace,
                    repository.clone(),
                    commit.into(),
                    label.into(),
                    window,
                    cx,
                );
            });
            cx.run_until_parked();
        }

        workspace.update(cx, |workspace, cx| {
            let active = workspace.active_item_as::<BranchDiff>(cx).unwrap();
            assert_eq!(
                active.read(cx).tab_content_text(0, cx),
                SharedString::from("Changes since Last Commit")
            );
            let mut commits = workspace
                .items_of_type::<BranchDiff>(cx)
                .filter_map(|item| match item.read(cx).diff_base(cx) {
                    DiffBase::Snapshot { commit, .. } => Some(commit.to_string()),
                    DiffBase::Head
                    | DiffBase::Index
                    | DiffBase::Staged
                    | DiffBase::Merge { .. } => None,
                })
                .collect::<Vec<_>>();
            commits.sort();
            assert_eq!(commits, vec!["HEAD", "turn-sha"]);
        });
    }

    #[gpui::test]
    async fn test_seen_marks_fold_files_and_clear_on_edit(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let diff = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        cx.run_until_parked();

        let (editor, list) = diff.read_with(cx, |diff, cx| {
            (
                diff.rhs_editor(cx),
                diff.diff.read(cx).branch_diff().clone(),
            )
        });
        let progress =
            |cx: &mut VisualTestContext| diff.read_with(cx, |diff, cx| diff.seen_progress(cx));
        assert_eq!(progress(cx), (0, 3));

        let first = editor.read_with(cx, |editor, cx| {
            seen_files::ordered_buffer_ids(editor.buffer().read(cx), cx)
                .first()
                .copied()
                .unwrap()
        });
        cx.update(|_, cx| seen_files::set_seen(&list, &editor, &[first], true, cx));
        cx.run_until_parked();
        assert_eq!(progress(cx), (1, 3));
        assert!(editor.read_with(cx, |editor, cx| editor.is_buffer_folded(first, cx)));

        let buffer = editor.read_with(cx, |editor, cx| {
            editor.buffer().read(cx).buffer(first).unwrap()
        });
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "edited ")], None, cx));
        cx.run_until_parked();
        assert_eq!(progress(cx), (0, 3));
        assert!(!editor.read_with(cx, |editor, cx| editor.is_buffer_folded(first, cx)));

        diff.update_in(cx, |diff, window, cx| {
            diff.mark_all_files_seen(&MarkAllFilesSeen, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(progress(cx), (3, 3));

        // After a restart the marks come back from the database, and the
        // files they cover start folded.
        cx.update(|_, cx| cx.set_global(seen_files::SeenFiles::default()));
        assert_eq!(progress(cx), (0, 3));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let reopened = cx
            .update(|window, cx| {
                BranchDiff::new_with_default_branch(project.clone(), workspace, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            reopened.read_with(cx, |diff, cx| diff.seen_progress(cx)),
            (3, 3)
        );
        let reopened_editor = reopened.read_with(cx, |diff, cx| diff.rhs_editor(cx));
        assert!(reopened_editor.read_with(cx, |editor, cx| editor.is_buffer_folded(first, cx)));
    }

    #[allow(clippy::disallowed_methods)]
    fn run_git(work_directory: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(work_directory)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The files shown in a Changes view, with each file's base text.
    fn shown_files(
        diff: &Entity<BranchDiff>,
        cx: &mut VisualTestContext,
    ) -> Vec<(String, Option<String>)> {
        diff.read_with(cx, |diff, cx| {
            let multibuffer = diff.diff.read(cx).multibuffer().read(cx);
            let mut files = multibuffer
                .all_buffers()
                .into_iter()
                .map(|buffer| {
                    let buffer = buffer.read(cx);
                    let path = buffer.file().unwrap().path().as_unix_str().to_string();
                    let base_text = multibuffer
                        .diff_for(buffer.remote_id())
                        .and_then(|diff| diff.read(cx).base_text_string(cx));
                    (path, base_text)
                })
                .collect::<Vec<_>>();
            files.sort();
            files
        })
    }

    /// Real git and a real file system notify asynchronously, so poll rather
    /// than rely on `run_until_parked` alone.
    #[allow(clippy::disallowed_methods)]
    fn wait_for<T: PartialEq + std::fmt::Debug>(
        cx: &mut VisualTestContext,
        expected: T,
        mut actual: impl FnMut(&mut VisualTestContext) -> T,
    ) {
        for _ in 0..300 {
            cx.run_until_parked();
            if actual(cx) == expected {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(actual(cx), expected);
    }

    #[gpui::test]
    async fn test_last_turn_and_last_commit_against_real_git(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        let directory = util::test::TempTree::new(json!({
            "project": {
                "a.txt": "one\n",
                "b.txt": "two\n",
            },
        }));
        let root = directory.path().join("project");
        run_git(&root, &["init", "-b", "main"]);
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-m", "initial"]);
        // An uncommitted edit made before the turn starts: part of "Last
        // Commit", but not of "Last Turn".
        std::fs::write(root.join("a.txt"), "one edited before the turn\n").unwrap();

        let project =
            Project::test(fs::RealFs::new(None, cx.executor()), [root.as_path()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        wait_for(cx, true, |cx| {
            project.read_with(cx, |project, cx| project.active_repository(cx).is_some())
        });
        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .unwrap();

        let (_staged, turn_snapshot) = repository
            .update(cx, |repository, _| repository.create_archive_checkpoint())
            .await
            .unwrap()
            .unwrap();

        std::fs::write(root.join("b.txt"), "two changed in the turn\n").unwrap();
        std::fs::write(root.join("new.txt"), "created in the turn\n").unwrap();
        let worktree = project.read_with(cx, |project, cx| project.worktrees(cx).next().unwrap());
        worktree::WorktreeModelHandle::flush_fs_events(&worktree, cx).await;

        let open = |commit: SharedString, label: &'static str, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                BranchDiff::deploy_snapshot_diff(
                    workspace,
                    repository.clone(),
                    commit,
                    label.into(),
                    window,
                    cx,
                );
                workspace.active_item_as::<BranchDiff>(cx).unwrap()
            })
        };

        let last_turn = open(turn_snapshot.into(), LAST_TURN_LABEL, cx);
        wait_for(
            cx,
            vec![
                ("b.txt".to_string(), Some("two\n".to_string())),
                ("new.txt".to_string(), None),
            ],
            |cx| shown_files(&last_turn, cx),
        );

        let last_commit = open("HEAD".into(), LAST_COMMIT_LABEL, cx);
        wait_for(
            cx,
            vec![
                ("a.txt".to_string(), Some("one\n".to_string())),
                ("b.txt".to_string(), Some("two\n".to_string())),
                ("new.txt".to_string(), None),
            ],
            |cx| shown_files(&last_commit, cx),
        );
    }

    fn bind_default_keymap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let asset = if cfg!(target_os = "macos") {
                "keymaps/default-macos.json"
            } else if cfg!(target_os = "windows") {
                "keymaps/default-windows.json"
            } else {
                "keymaps/default-linux.json"
            };
            let bindings =
                settings::KeymapFile::load_asset_allow_partial_failure(asset, cx).unwrap();
            cx.bind_keys(bindings);
        });
    }

    #[gpui::test]
    async fn test_changes_view_default_keybindings(cx: &mut TestAppContext) {
        init_test(cx);
        bind_default_keymap(cx);
        let (toggle_seen, mark_all_seen) = if cfg!(target_os = "macos") {
            ("cmd-alt-y", "cmd-shift-y")
        } else {
            ("ctrl-alt-y", "ctrl-shift-y")
        };

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "d.txt": "created-in-head",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "A".into())],
        );
        cx.run_until_parked();

        // The agent panel's shortcut opens the Changes view from anywhere in the workspace.
        cx.focus(&workspace);
        cx.simulate_keystrokes("alt-shift-d");
        cx.run_until_parked();
        let diff = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<BranchDiff>(cx)
            })
            .expect("alt-shift-d should open the Changes view");
        let editor = diff.read_with(cx, |diff, cx| diff.rhs_editor(cx));
        let files = editor.read_with(cx, |editor, cx| {
            seen_files::ordered_buffer_ids(editor.buffer().read(cx), cx)
        });
        assert_eq!(files.len(), 3);
        let current_file = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, cx| {
                editor
                    .active_buffer(cx)
                    .map(|buffer| buffer.read(cx).remote_id())
            })
        };
        let progress =
            |cx: &mut VisualTestContext| diff.read_with(cx, |diff, cx| diff.seen_progress(cx));

        diff.update_in(cx, |diff, window, cx| {
            diff.focus_handle(cx).focus(window, cx);
            diff.diff
                .update(cx, |diff, cx| diff.move_to_beginning(window, cx));
        });
        cx.run_until_parked();
        assert_eq!(current_file(cx), Some(files[0]));

        cx.simulate_keystrokes("alt-]");
        cx.run_until_parked();
        assert_eq!(current_file(cx), Some(files[1]));
        cx.simulate_keystrokes("alt-[");
        cx.run_until_parked();
        assert_eq!(current_file(cx), Some(files[0]));

        // Marking the file seen folds it and moves on to the next one.
        cx.simulate_keystrokes(toggle_seen);
        cx.run_until_parked();
        assert_eq!(progress(cx), (1, 3));
        assert!(editor.read_with(cx, |editor, cx| editor.is_buffer_folded(files[0], cx)));
        assert_eq!(current_file(cx), Some(files[1]));

        cx.simulate_keystrokes(mark_all_seen);
        cx.run_until_parked();
        assert_eq!(progress(cx), (3, 3));

        // The toolbar draws for the branch comparison and for a snapshot one.
        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .unwrap();
        let last_commit = workspace.update_in(cx, |workspace, window, cx| {
            BranchDiff::deploy_snapshot_diff(
                workspace,
                repository,
                "HEAD".into(),
                LAST_COMMIT_LABEL.into(),
                window,
                cx,
            );
            workspace.active_item_as::<BranchDiff>(cx).unwrap()
        });
        cx.run_until_parked();
        let toolbar = cx.new(|cx| BranchDiffToolbar::new(cx));
        for item in [diff, last_commit] {
            toolbar.update_in(cx, |toolbar, window, cx| {
                let location = toolbar.set_active_pane_item(Some(&item), window, cx);
                assert_eq!(location, ToolbarItemLocation::PrimaryRight);
            });
            cx.draw(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(1200.), gpui::px(40.)),
                |_, _| toolbar.clone().into_any_element(),
            );
        }
    }

    #[gpui::test]
    async fn test_comment_mode_in_changes_view(cx: &mut TestAppContext) {
        init_test(cx);
        bind_default_keymap(cx);
        let (send_now, comment_on_selection, toggle_comment_mode) = if cfg!(target_os = "macos") {
            ("cmd-enter", "cmd-alt-m", "cmd-alt-c")
        } else {
            ("ctrl-enter", "ctrl-alt-m", "ctrl-alt-c")
        };

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "one\ntwo\nthree\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\n".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\n".into())],
        );
        cx.run_until_parked();

        let sent_reviews = Rc::new(Cell::new(0));
        workspace.update(cx, |workspace, _| {
            let sent_reviews = sent_reviews.clone();
            workspace.register_action(move |_, _: &SendReviewToAgent, _, _| {
                sent_reviews.set(sent_reviews.get() + 1);
            });
        });

        cx.focus(&workspace);
        cx.simulate_keystrokes("alt-shift-d");
        cx.run_until_parked();
        let diff = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<BranchDiff>(cx)
            })
            .unwrap();
        let editor = diff.read_with(cx, |diff, cx| diff.rhs_editor(cx));
        diff.update_in(cx, |diff, window, cx| {
            diff.focus_handle(cx).focus(window, cx);
            diff.diff
                .update(cx, |diff, cx| diff.move_to_beginning(window, cx));
        });
        cx.run_until_parked();
        let file_text =
            |cx: &mut VisualTestContext| editor.read_with(cx, |editor, cx| editor.text(cx));
        let original_text = file_text(cx);
        assert!(editor.read_with(cx, |editor, _| editor.comment_mode()));

        // Typing on code starts a comment and leaves the file alone.
        cx.simulate_input("why?");
        cx.run_until_parked();
        assert_eq!(file_text(cx), original_text);
        let prompt = editor
            .read_with(cx, |editor, _| editor.diff_review_prompt_editor().cloned())
            .expect("typing in comment mode should open a comment");
        assert_eq!(prompt.read_with(cx, |prompt, cx| prompt.text(cx)), "why?");

        // Sending stores the comment and hands it to the agent straight away.
        cx.simulate_keystrokes(send_now);
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.total_review_comment_count()),
            1
        );
        assert_eq!(sent_reviews.get(), 1);
        assert_eq!(file_text(cx), original_text);

        // Out of comment mode, typing edits the file again.
        editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.simulate_keystrokes(toggle_comment_mode);
        cx.run_until_parked();
        assert!(!editor.read_with(cx, |editor, _| editor.comment_mode()));
        cx.simulate_input("x");
        cx.run_until_parked();
        assert_ne!(file_text(cx), original_text);

        // The shortcut comments on the selection in either mode.
        cx.simulate_keystrokes(comment_on_selection);
        cx.run_until_parked();
        // The edit above moved the lines, so this may be a new comment box.
        let focused_contexts = cx.update(|window, _| window.context_stack());
        assert!(
            focused_contexts
                .iter()
                .any(|context| context.contains("ReviewComment")),
            "{focused_contexts:?}"
        );
    }

    #[gpui::test]
    async fn test_review_comments_survive_a_restart(cx: &mut TestAppContext) {
        init_test(cx);
        bind_default_keymap(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "one\ntwo\nthree\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\n".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\n".into())],
        );
        cx.run_until_parked();

        cx.focus(&workspace);
        cx.simulate_keystrokes("alt-shift-d");
        cx.run_until_parked();
        let diff = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<BranchDiff>(cx)
            })
            .unwrap();
        let editor = diff.read_with(cx, |diff, cx| diff.rhs_editor(cx));
        diff.update_in(cx, |diff, window, cx| {
            diff.focus_handle(cx).focus(window, cx);
            diff.diff
                .update(cx, |diff, cx| diff.move_to_beginning(window, cx));
        });
        cx.run_until_parked();

        cx.simulate_input("why?");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            let sent = editor.send_review_comments_to_agent(window, cx);
            assert_eq!(sent.len(), 1);
            editor.set_review_comment_replies(
                vec![(sent[0].id, editor::AgentReply::Answered("Because.".into()))],
                window,
                cx,
            );
        });
        cx.run_until_parked();
        let saved = editor.read_with(cx, |editor, cx| editor.saved_review_comments(cx));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].file_path, "a.txt");
        assert_eq!(saved[0].comment, "why?");
        assert_eq!(saved[0].answer.as_deref(), Some("Because."));
        assert!(!saved[0].outdated);

        // Hiding comments closes the boxes but keeps the comments, with a
        // marker on the commented lines; showing brings the boxes back.
        let toggle_hidden = if cfg!(target_os = "macos") {
            "cmd-shift-m"
        } else {
            "ctrl-shift-m"
        };
        let box_count_and_markers = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, cx| {
                (
                    editor.diff_review_prompt_editor().is_some(),
                    editor.total_review_comment_count(),
                    editor.review_comments_hidden(),
                    editor.hidden_review_comment_marker_count(cx),
                )
            })
        };
        assert_eq!(box_count_and_markers(cx), (true, 1, false, 0));
        cx.simulate_keystrokes(toggle_hidden);
        cx.run_until_parked();
        assert_eq!(box_count_and_markers(cx), (false, 1, true, 1));
        editor.update_in(cx, |editor, window, cx| {
            editor.focus_handle(cx).focus(window, cx);
        });
        cx.simulate_keystrokes(toggle_hidden);
        cx.run_until_parked();
        assert_eq!(box_count_and_markers(cx), (true, 1, false, 0));

        // A new view, as after a restart, shows the comment with its answer.
        let reopen = |cx: &mut VisualTestContext| {
            let workspace = workspace.clone();
            let project = project.clone();
            cx.update(|window, cx| {
                BranchDiff::new_with_default_branch(project, workspace, window, cx)
            })
        };
        let reopened = reopen(cx).await.unwrap();
        cx.run_until_parked();
        let reopened_editor = reopened.read_with(cx, |diff, cx| diff.rhs_editor(cx));
        assert_eq!(
            reopened_editor.read_with(cx, |editor, cx| editor.saved_review_comments(cx)),
            saved
        );
        assert_eq!(
            reopened_editor.read_with(cx, |editor, _| editor.unsent_review_comment_count()),
            0
        );

        // Once the commented line has changed, the comment is kept but marked
        // outdated.
        let commented_row = saved[0].start_row;
        let buffer = editor.read_with(cx, |editor, cx| {
            editor
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .next()
                .unwrap()
        });
        buffer.update(cx, |buffer, cx| {
            let start = buffer.point_to_offset(language::Point::new(commented_row, 0));
            buffer.edit([(start..start + 1, "X")], None, cx);
        });
        cx.run_until_parked();
        let reopened = reopen(cx).await.unwrap();
        cx.run_until_parked();
        let outdated = reopened.read_with(cx, |diff, cx| {
            diff.rhs_editor(cx).read(cx).saved_review_comments(cx)
        });
        assert_eq!(outdated.len(), 1);
        assert!(outdated[0].outdated);
        assert_eq!(outdated[0].comment, "why?");
    }

    #[gpui::test]
    async fn test_comment_mode_in_a_file_tab(cx: &mut TestAppContext) {
        init_test(cx);
        bind_default_keymap(cx);
        let toggle_comment_mode = if cfg!(target_os = "macos") {
            "cmd-alt-c"
        } else {
            "ctrl-alt-c"
        };

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "one\ntwo\nthree\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        cx.run_until_parked();
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let open_file = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path("a.txt")), None, true, window, cx)
            })
        };

        let editor = open_file(cx).await.unwrap().downcast::<Editor>().unwrap();
        cx.run_until_parked();
        let original_text = editor.read_with(cx, |editor, cx| editor.text(cx));
        // A file tab edits as usual until comment mode is switched on.
        assert!(!editor.read_with(cx, |editor, _| editor.comment_mode()));
        cx.simulate_keystrokes(toggle_comment_mode);
        cx.run_until_parked();
        assert!(editor.read_with(cx, |editor, _| editor.comment_mode()));

        cx.simulate_input("why?");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, cx| editor.text(cx)),
            original_text
        );
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.total_review_comment_count()),
            1
        );

        // The comment is saved with the repository's others, so the Changes
        // view and a reopened tab both show it.
        let repository = project
            .read_with(cx, |project, cx| project.active_repository(cx))
            .unwrap();
        let saved = cx.update(|_, cx| {
            let key = review_comment_store::key_for_repository(&repository, cx);
            review_comment_store::load(&key, cx)
        });
        assert_eq!(saved.len(), 1);
        assert_eq!(
            (saved[0].file_path.as_str(), saved[0].comment.as_str()),
            ("a.txt", "why?")
        );

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_all_items(&Default::default(), window, cx)
                })
            })
            .await
            .unwrap();
        drop(editor);
        cx.run_until_parked();
        let reopened = open_file(cx).await.unwrap().downcast::<Editor>().unwrap();
        cx.run_until_parked();
        assert_eq!(
            reopened.read_with(cx, |editor, cx| editor.saved_review_comments(cx)),
            saved
        );
    }
}
