use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Styled, Subscription, WeakEntity, Window, px,
};
use strata_core::diff::{ElementChange, StepDiff, diff, diff_into};
use theme::ActiveTheme;
use ui::{
    Button, ButtonCommon, ButtonStyle, Clickable, Color, Icon, IconName, IconSize, Label,
    LabelCommon, LabelSize, ListItem, ListItemSpacing, h_flex, v_flex,
};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::{OpenStack, StrataView, ToggleFocus};
use ui::prelude::*;

const PANEL_KEY: &str = "StrataPanel";
const DEFAULT_WIDTH: f32 = 260.0;

/// Lists the steps of the stack that the active stack view shows: the
/// baseline, then the pull requests nested under a collapsible stack entry.
pub struct StrataPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    view: Option<WeakEntity<StrataView>>,
    view_subscription: Option<Subscription>,
    _workspace_subscription: Subscription,
}

impl EventEmitter<PanelEvent> for StrataPanel {}

impl StrataPanel {
    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<StrataPanel>(window, cx);
        });
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let workspace_handle = workspace.weak_handle();
        let workspace_subscription = match workspace_handle.upgrade() {
            Some(entity) => cx.subscribe_in(
                &entity,
                window,
                |this, _workspace, event: &workspace::Event, _window, cx| match event {
                    workspace::Event::ActiveItemChanged => this.follow_active_item(cx),
                    // A preview opened to the side does not always become the
                    // active item, so a newly added stack is shown as well.
                    workspace::Event::ItemAdded { item } => {
                        this.show_view(item.downcast::<StrataView>(), cx)
                    }
                    workspace::Event::ItemRemoved { .. } => this.show_any_open_stack(cx),
                    _ => {}
                },
            ),
            None => cx.observe_self(|_, _| {}),
        };
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace_handle,
            position: DockPosition::Left,
            view: None,
            view_subscription: None,
            _workspace_subscription: workspace_subscription,
        };
        // The workspace is mid-update while it builds its panels, so the
        // active item comes from the reference rather than the entity.
        let active_view = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<StrataView>());
        this.show_view(active_view, cx);
        this
    }

    pub fn view(&self) -> Option<Entity<StrataView>> {
        self.view.as_ref().and_then(WeakEntity::upgrade)
    }

    /// Shows the stack of the active item when that is a stack view; any
    /// other item leaves the last stack on display.
    fn follow_active_item(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let active_view = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.downcast::<StrataView>());
        self.show_view(active_view, cx);
    }

    fn show_any_open_stack(&mut self, cx: &mut Context<Self>) {
        if self.view().is_some() {
            return;
        }
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open_view = workspace.read(cx).items_of_type::<StrataView>(cx).next();
        self.show_view(open_view, cx);
    }

    fn show_view(&mut self, view: Option<Entity<StrataView>>, cx: &mut Context<Self>) {
        let Some(view) = view else {
            return;
        };
        if self.view().as_ref() == Some(&view) {
            return;
        }
        self.view_subscription = Some(cx.observe(&view, |_, _, cx| cx.notify()));
        self.view = Some(view.downgrade());
        cx.notify();
    }
}

/// `+added ~changed -removed`, in the theme's version control colors.
pub fn change_counts(step_diff: &StepDiff) -> impl IntoElement {
    let mut added = 0;
    let mut changed = 0;
    let mut removed = 0;
    for change in step_diff.elements.values() {
        match change {
            ElementChange::Added => added += 1,
            ElementChange::Changed | ElementChange::Moved { .. } => changed += 1,
            ElementChange::Removed => removed += 1,
            ElementChange::Unchanged => {}
        }
    }
    let count = |prefix: &str, value: usize, color: Color| {
        (value > 0).then(|| {
            Label::new(format!("{prefix}{value}"))
                .size(LabelSize::XSmall)
                .color(color)
        })
    };
    h_flex()
        .flex_none()
        .gap_1()
        .children(count("+", added, Color::VersionControlAdded))
        .children(count("~", changed, Color::VersionControlModified))
        .children(count("\u{2212}", removed, Color::VersionControlDeleted))
}

impl Focusable for StrataPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StrataPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors().clone();
        let root = v_flex()
            .id("strata-panel")
            .key_context("StrataPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.panel_background);
        let Some(view) = self.view() else {
            return root
                .items_center()
                .justify_center()
                .gap_2()
                .p_4()
                .child(
                    Label::new("No stack open")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    Button::new("open-stack", "Open Stack")
                        .style(ButtonStyle::Outlined)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenStack), cx);
                        }),
                );
        };

        let state = view.read(cx);
        let spec = state.spec();
        let selected_index = state.step_index();
        let collapsed = state.stack_collapsed();
        let step_row =
            |index: usize, indent_level: usize| {
                let step = spec.steps.get(index)?;
                let selected = index == selected_index;
                let step_diff = diff_into(&spec.steps, index);
                let glyph = if index == 0 {
                    IconName::GitCommit
                } else {
                    IconName::PullRequest
                };
                let view = view.clone();
                Some(
                    ListItem::new(("strata-step", index))
                        .spacing(ListItemSpacing::Dense)
                        .inset(true)
                        .indent_level(indent_level)
                        .toggle_state(selected)
                        .start_slot(Icon::new(glyph).size(IconSize::Small).color(if selected {
                            Color::Default
                        } else {
                            Color::Muted
                        }))
                        .child(
                            h_flex()
                                .gap_1p5()
                                .min_w_0()
                                .child(
                                    Label::new(step.id.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(Label::new(step.title.clone()).truncate().color(
                                    if selected {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    },
                                )),
                        )
                        .end_slot(change_counts(&step_diff))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |view, cx| view.play_step(index, cx));
                        }),
                )
            };

        let pull_request_count = spec.steps.len().saturating_sub(1);
        let total = match (spec.steps.first(), spec.steps.last()) {
            (Some(first), Some(last)) if spec.steps.len() > 1 => diff(first, last),
            _ => StepDiff::default(),
        };
        let stack_name = spec.stack_name();
        let stack_section = (pull_request_count > 0).then(|| {
            let view = view.clone();
            v_flex()
                .child(
                    ListItem::new("strata-stack")
                        .spacing(ListItemSpacing::Dense)
                        .inset(true)
                        .toggle(Some(!collapsed))
                        .on_toggle({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |view, cx| view.toggle_stack_collapsed(cx))
                            }
                        })
                        .on_click(move |_, _, cx| {
                            view.update(cx, |view, cx| view.toggle_stack_collapsed(cx))
                        })
                        .child(Label::new(stack_name))
                        .end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Label::new(pull_request_count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(change_counts(&total)),
                        ),
                )
                .when(!collapsed, |this| {
                    this.children((1..spec.steps.len()).filter_map(|index| step_row(index, 1)))
                })
        });

        root.child(
            h_flex()
                .flex_none()
                .h(px(32.))
                .px_3()
                .border_b_1()
                .border_color(colors.border_variant)
                .child(
                    Label::new("Stack")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(
            v_flex()
                .id("strata-panel-steps")
                .flex_1()
                .overflow_y_scroll()
                .p_1()
                .children(step_row(0, 0))
                .children(stack_section),
        )
    }
}

impl Panel for StrataPanel {
    fn persistent_name() -> &'static str {
        "Stack Panel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(DEFAULT_WIDTH)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::GitBranch)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Stack Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}
