use gpui::{
    Context, Entity, EventEmitter, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, Subscription, Window, canvas, div, px, relative,
};
use strata_core::timeline::Beat;
use theme::ActiveTheme;
use ui::{
    ButtonCommon, ButtonLike, Clickable, Color, Divider, Icon, IconButton, IconName, IconSize,
    Label, LabelCommon, LabelSize, ToggleButtonGroup, ToggleButtonGroupStyle, ToggleButtonSimple,
    Tooltip, h_flex, prelude::*,
};
use workspace::{ItemHandle, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView};

use crate::scene::Phase;
use crate::strata_item::{SPEEDS, StrataView};

const SCRUBBER_WIDTH: f32 = 160.0;
const SCRUBBER_MIN_WIDTH: f32 = 48.0;

/// The bar above a stack view: breadcrumbs and links on the left, playback
/// and the before/after toggle on the right.
pub struct StrataToolbar {
    view: Option<Entity<StrataView>>,
    _view_subscription: Option<Subscription>,
}

impl StrataToolbar {
    pub fn new() -> Self {
        Self {
            view: None,
            _view_subscription: None,
        }
    }
}

impl Default for StrataToolbar {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter<ToolbarItemEvent> for StrataToolbar {}

impl ToolbarItemView for StrataToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        match active_pane_item.and_then(|item| item.downcast::<StrataView>()) {
            Some(view) => {
                self._view_subscription = Some(cx.observe(&view, |_, _, cx| cx.notify()));
                self.view = Some(view);
                ToolbarItemLocation::Secondary
            }
            None => {
                self._view_subscription = None;
                self.view = None;
                ToolbarItemLocation::Hidden
            }
        }
    }
}

fn beat_color(beat: Beat, cx: &gpui::App) -> gpui::Hsla {
    let status = cx.theme().status();
    match beat {
        Beat::Remove => status.deleted,
        Beat::Move | Beat::Change => status.modified,
        Beat::Add | Beat::Draw => status.created,
    }
}

impl Render for StrataToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view_entity) = self.view.clone() else {
            return div().into_any_element();
        };
        let colors = cx.theme().colors().clone();
        let view = view_entity.read(cx);
        let Some(step) = view.step() else {
            return div().into_any_element();
        };
        let spec = view.spec();
        let repository = spec.repo.trim_end_matches('/').to_string();
        let pull_request_url = step.pr.map(|number| format!("{repository}/pull/{number}"));
        let diff_url = step
            .base
            .as_ref()
            .zip(step.head.as_ref())
            .map(|(base, head)| format!("{repository}/compare/{base}...{head}"));

        let playback = view.playback();
        let duration = view.duration();
        let fraction = if duration > 0.0 {
            (playback.t / duration).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let beats: Vec<_> = view
            .timeline()
            .filter(|_| duration > 0.0)
            .map(|timeline| {
                timeline
                    .beats
                    .iter()
                    .map(|span| {
                        div()
                            .absolute()
                            .top(px(8.))
                            .h(px(4.))
                            .left(relative(span.start / duration))
                            .w(relative(span.duration / duration))
                            .rounded(px(2.))
                            .bg(beat_color(span.beat, cx).opacity(0.55))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let playing = playback.playing;
        let time_label = format!("{:.2} / {:.2}s", playback.t, duration);
        let speed = SPEEDS.get(playback.speed).copied().unwrap_or(1.0);
        let has_before = view.step_index() > 0;
        let selected_phase = match view.phase() {
            Some(Phase::Before) => 0,
            Some(Phase::After) => 1,
            None => usize::MAX,
        };
        let scrubber_bounds = view.scrubber_bounds();
        let stack_name = spec.stack_name();
        let step_id = step.id.clone();
        let step_title = step.title.clone();
        let step_status = step.status.clone();

        let separator = || {
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(Color::Disabled)
        };
        let open_link =
            |id: &'static str, icon: IconName, tooltip: &'static str, url: Option<String>| {
                url.map(|url| {
                    IconButton::new(id, icon)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                })
            };

        h_flex()
            .id("strata-toolbar")
            .w_full()
            .h(px(32.))
            .px_2()
            .gap_1p5()
            .bg(colors.toolbar_background)
            .border_b_1()
            .border_color(colors.border_variant)
            .on_mouse_move({
                let view = view_entity.clone();
                move |event, _, cx| {
                    view.update(cx, |view, cx| view.move_scrub(event, cx));
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let view = view_entity.clone();
                move |_, _, cx| view.update(cx, |view, _| view.end_scrub())
            })
            .on_mouse_up_out(MouseButton::Left, {
                let view = view_entity.clone();
                move |_, _, cx| view.update(cx, |view, _| view.end_scrub())
            })
            .child(
                div().min_w_0().truncate().child(
                    Label::new(stack_name)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(separator())
            .child(
                Label::new(step_id)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(separator())
            .child(div().min_w_0().truncate().child(Label::new(step_title)))
            .children(step_status.map(|status| {
                Label::new(status)
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder)
            }))
            .child(div().flex_1())
            .children(open_link(
                "strata-open-pull-request",
                IconName::PullRequest,
                "Open pull request",
                pull_request_url,
            ))
            .children(open_link(
                "strata-exact-diff",
                IconName::FileDiff,
                "Open exact diff",
                diff_url,
            ))
            .child(Divider::vertical())
            .child(
                IconButton::new(
                    "strata-play",
                    if playing {
                        IconName::DebugPause
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(if playing { "Pause" } else { "Play" }))
                .on_click({
                    let view = view_entity.clone();
                    move |_, _, cx| view.update(cx, |view, cx| view.toggle_play(cx))
                }),
            )
            .child(
                div()
                    .id("strata-scrubber")
                    .relative()
                    .w(px(SCRUBBER_WIDTH))
                    .min_w(px(SCRUBBER_MIN_WIDTH))
                    .h(px(20.))
                    .cursor_pointer()
                    .child(
                        canvas(
                            move |bounds, _, _| scrubber_bounds.set(Some(bounds)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(9.))
                            .h(px(2.))
                            .w_full()
                            .rounded(px(1.))
                            .bg(colors.element_selected),
                    )
                    .children(beats)
                    .child(
                        div()
                            .absolute()
                            .top(px(3.))
                            .h(px(14.))
                            .w(px(2.))
                            .left(relative(fraction))
                            .ml(px(-1.))
                            .rounded(px(1.))
                            .bg(colors.text),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        let view = view_entity.clone();
                        move |event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            view.update(cx, |view, cx| view.start_scrub(event.position, cx));
                        }
                    }),
            )
            .child(
                div().w(px(78.)).flex_none().child(
                    Label::new(time_label)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                ButtonLike::new("strata-speed")
                    .tooltip(Tooltip::text("Playback speed"))
                    .child(
                        Label::new(format!("{speed}\u{d7}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click({
                        let view = view_entity.clone();
                        move |_, _, cx| view.update(cx, |view, cx| view.cycle_speed(cx))
                    }),
            )
            .child(
                div().flex_none().child(
                    ToggleButtonGroup::single_row(
                        "strata-phase",
                        [
                            ToggleButtonSimple::new("Before", {
                                let view = view_entity.clone();
                                move |_, _, cx| {
                                    if has_before {
                                        view.update(cx, |view, cx| {
                                            view.go_to(view.step_index(), Phase::Before, cx)
                                        });
                                    }
                                }
                            }),
                            ToggleButtonSimple::new("After", {
                                let view = view_entity.clone();
                                move |_, _, cx| {
                                    view.update(cx, |view, cx| {
                                        view.go_to(view.step_index(), Phase::After, cx)
                                    });
                                }
                            }),
                        ],
                    )
                    .style(ToggleButtonGroupStyle::Outlined)
                    .auto_width()
                    .selected_index(selected_phase),
                ),
            )
            .into_any_element()
    }
}
