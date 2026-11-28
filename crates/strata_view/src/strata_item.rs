use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, Font,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    PinchEvent, Pixels, Render, ScrollEvent, SharedString, Styled, Subscription, Task, WeakEntity,
    Window, canvas, div, point, prelude::*, px, size,
};
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use settings::Settings;
use strata_core::diff::{ElementChange, RelationshipChange, diff_into};
use strata_core::layout::{
    self, EdgeRouting, LayoutEngine, LayoutSpacing, Measure, Rect, StepLayouts,
};
use strata_core::timeline::{Placement, Timeline, step_timeline_placed};
use strata_spec::{Diagnostic, Element, Relationship, Spec, Step};
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use ui::{
    ButtonLike, ButtonSize, ButtonStyle, Divider, Icon, IconButton, IconName, IconSize, Label,
    LabelSize, Popover, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::item::{Item, ItemEvent};
use workspace::{Pane, Workspace};

use crate::diagram_theme::{self, DiagramColors};
use crate::painter::{
    FOCUS_EASING, Focus, Highlight, MAX_SCALE, MIN_SCALE, TextCache, ViewState, WindowText,
    card_icon, has_ancestor, paint_scene,
};
use crate::scene::{NodeItem, Phase, Scene};
use crate::{
    CloseDetails, FitToView, FrameChanges, NextStep, OpenPreview, OpenPreviewToTheSide,
    PreviousStep, ScrubBackward, ScrubForward, ShowAfter, ShowBefore, SlowDown, SpeedUp,
    ToggleFitEachStep, TogglePlayback, ZoomIn, ZoomOut,
};

const ZOOM_PER_PIXEL: f32 = 0.005;
const LINE_SCROLL_PIXELS: f32 = 20.0;
const FIT_MARGIN: f32 = 24.0;
const CLICK_SLOP: f32 = 4.0;
const DETAILS_WIDTH: f32 = 320.0;
const DETAILS_GAP: f32 = 8.0;
const DETAILS_MIN_HEIGHT: f32 = 360.0;
/// The step's story scrolls beyond this, so the canvas keeps its room.
const STORY_MAX_HEIGHT: f32 = 220.0;
/// Room kept free at the canvas's bottom for the zoom controls.
const ZOOM_CONTROLS_CLEARANCE: f32 = 48.0;

/// How long after the last edit the preview waits before parsing the buffer.
pub const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// Playback speeds the speed actions step through.
pub const SPEEDS: [f32; 4] = [0.25, 0.5, 1.0, 2.0];
const NORMAL_SPEED: usize = 2;
/// How far scrubbing with the keyboard moves, in seconds of step time.
const SCRUB_STEP: f32 = 0.05;

/// Below this scale element text gets too small to read, so the camera frames
/// the step's changes instead of the whole stack.
const READABLE_SCALE: f32 = 0.6;
/// Framing never zooms past actual size, so a single change keeps context.
const MAX_FRAME_SCALE: f32 = 1.0;
const ZOOM_STEP: f32 = 1.25;
/// Layout pixels kept around an element the camera glides to.
const SELECTION_FRAME_MARGIN: f32 = 160.0;
/// How long the camera takes to glide to a new framing.
const GLIDE_SECONDS: f32 = 0.6;
/// A glide zooms out on the way once it travels further than this share of
/// the viewport, by up to this factor.
const GLIDE_DIP_FROM: f32 = 0.35;
const GLIDE_MAX_DIP: f32 = 1.5;

/// The view that fits `region` (in layout coordinates) centered in `viewport`.
fn frame_view(region: Rect, viewport: gpui::Size<Pixels>) -> ViewState {
    let width = f32::from(viewport.width);
    let height = f32::from(viewport.height);
    let available_width = (width - 2.0 * FIT_MARGIN).max(1.0);
    let available_height = (height - 2.0 * FIT_MARGIN).max(1.0);
    let scale = if region.w > 0.0 && region.h > 0.0 {
        (available_width / region.w).min(available_height / region.h)
    } else {
        1.0
    }
    .min(MAX_FRAME_SCALE)
    .clamp(MIN_SCALE, MAX_SCALE);
    let center = region.center();
    ViewState {
        scale,
        offset: point(
            width / 2.0 - center.x * scale,
            height / 2.0 - center.y * scale,
        ),
    }
}

/// Between two views, moving the layout point at the viewport's center in a
/// straight line while the scale changes geometrically, so a glide that
/// zooms reads as one steady motion.
fn interpolate_view(
    from: ViewState,
    to: ViewState,
    progress: f32,
    viewport: gpui::Size<Pixels>,
) -> ViewState {
    let middle = point(
        f32::from(viewport.width) / 2.0,
        f32::from(viewport.height) / 2.0,
    );
    let center_of = |view: ViewState| {
        point(
            (middle.x - view.offset.x) / view.scale,
            (middle.y - view.offset.y) / view.scale,
        )
    };
    let (from_center, to_center) = (center_of(from), center_of(to));
    // A glide between places far apart zooms out on the way, as maps do when
    // they fly between places, so it never crosses only empty canvas.
    let distance =
        (to_center.x - from_center.x).hypot(to_center.y - from_center.y) * from.scale.min(to.scale);
    let span = f32::from(viewport.width)
        .max(f32::from(viewport.height))
        .max(1.0);
    let dip = (distance / span - GLIDE_DIP_FROM).clamp(0.0, GLIDE_MAX_DIP);
    let scale = from.scale * (to.scale / from.scale).powf(progress)
        / (1.0 + dip * (std::f32::consts::PI * progress).sin());
    let center = point(
        from_center.x + (to_center.x - from_center.x) * progress,
        from_center.y + (to_center.y - from_center.y) * progress,
    );
    ViewState {
        scale,
        offset: point(middle.x - center.x * scale, middle.y - center.y * scale),
    }
}

/// How fast every animation clock of the view runs: 1 normally. Recording
/// the view in a slow environment (software rendering draws only a few
/// frames a second) sets `STRATA_RECORDING_TIME_SCALE` below 1, so the
/// animation plays in slow motion and a recording sped back up by the same
/// factor shows every frame.
fn recording_time_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("STRATA_RECORDING_TIME_SCALE")
            .ok()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|scale| *scale > 0.0 && *scale <= 1.0)
            .unwrap_or(1.0)
    })
}

/// Seconds of animation time since `since`.
fn seconds_since(since: Instant) -> f32 {
    since.elapsed().as_secs_f32() * recording_time_scale()
}

/// The layouts at the two ends of `step_index`: the stable layout at both,
/// or with `fit_each_step`, the previous step's own layout and this one's.
fn placement(layouts: &StepLayouts, step_index: usize, fit_each_step: bool) -> Placement<'_> {
    if !fit_each_step {
        return Placement::stable(&layouts.union);
    }
    let after = layouts.steps.get(step_index).unwrap_or(&layouts.union);
    let before = step_index
        .checked_sub(1)
        .and_then(|index| layouts.steps.get(index))
        .unwrap_or(after);
    Placement { before, after }
}

fn union(first: Rect, second: Rect) -> Rect {
    let x = first.x.min(second.x);
    let y = first.y.min(second.y);
    Rect {
        x,
        y,
        w: first.right().max(second.right()) - x,
        h: first.bottom().max(second.bottom()) - y,
    }
}

/// Of the windows of `size` centered on each rect, the one holding the most
/// rects entirely, shrunk to those rects. Ties go to the larger total area.
fn densest_window(rects: &[Rect], size: layout::Size) -> Option<Rect> {
    let inside = |window: &Rect, rect: &Rect| {
        rect.x >= window.x
            && rect.y >= window.y
            && rect.right() <= window.right()
            && rect.bottom() <= window.bottom()
    };
    rects
        .iter()
        .map(|candidate| {
            let center = candidate.center();
            let window = Rect {
                x: center.x - size.w / 2.0,
                y: center.y - size.h / 2.0,
                w: size.w,
                h: size.h,
            };
            let held: Vec<Rect> = rects
                .iter()
                .filter(|rect| inside(&window, rect))
                .copied()
                .collect();
            let area: f32 = held.iter().map(|rect| rect.w * rect.h).sum();
            (held.len(), area, held)
        })
        .max_by(|first, second| first.0.cmp(&second.0).then(first.1.total_cmp(&second.1)))
        .and_then(|(_, _, held)| held.into_iter().reduce(union))
}

/// What the camera frames while the user has not moved it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framing {
    /// The whole stack when it is readable, otherwise what the step changes.
    Changes,
    Everything,
    /// The selected element and its surroundings.
    Selection,
}

struct Glide {
    from: ViewState,
    to: ViewState,
    started: Instant,
}

#[derive(Default)]
struct Camera {
    view: Option<ViewState>,
    /// The region and viewport the view was last framed for.
    framed: Option<(Rect, gpui::Size<Pixels>)>,
    user_adjusted: bool,
    glide: Option<Glide>,
}

impl Camera {
    /// The view for this frame, and whether it is still gliding.
    fn view_for(
        &mut self,
        region: Rect,
        viewport: gpui::Size<Pixels>,
        reduce_motion: bool,
    ) -> (ViewState, bool) {
        if let (Some(view), true) = (self.view, self.user_adjusted) {
            return (view, false);
        }
        if self.framed != Some((region, viewport)) {
            let target = frame_view(region, viewport);
            let resized = self
                .framed
                .is_some_and(|(_, framed_viewport)| framed_viewport != viewport);
            match self.view {
                Some(current) if !reduce_motion && !resized => {
                    self.glide = Some(Glide {
                        from: current,
                        to: target,
                        started: Instant::now(),
                    });
                }
                _ => {
                    self.glide = None;
                    self.view = Some(target);
                }
            }
            self.framed = Some((region, viewport));
        }
        let Some(glide) = &self.glide else {
            return (
                self.view.unwrap_or_else(|| frame_view(region, viewport)),
                false,
            );
        };
        let progress = seconds_since(glide.started) / GLIDE_SECONDS;
        if progress >= 1.0 {
            let target = glide.to;
            self.glide = None;
            self.view = Some(target);
            return (target, false);
        }
        let view = interpolate_view(glide.from, glide.to, FOCUS_EASING.ease(progress), viewport);
        self.view = Some(view);
        (view, true)
    }

    /// The user moved the view: it stays where they put it until the next
    /// step or a fit.
    fn adjust(&mut self) -> Option<&mut ViewState> {
        self.user_adjusted = true;
        self.glide = None;
        self.view.as_mut()
    }

    fn reframe(&mut self) {
        self.user_adjusted = false;
    }
}

enum State {
    /// The buffer has never held a valid spec.
    Waiting,
    Laying,
    Ready {
        layouts: Rc<StepLayouts>,
        timeline: Rc<Timeline>,
        scene: Rc<Scene>,
    },
    Failed(String),
}

/// The one clock: everything on screen is sampled at `t`.
pub struct Playback {
    pub t: f32,
    pub playing: bool,
    pub speed: usize,
    last_tick: Option<Instant>,
}

/// A press on the canvas: a drag pans, a press that barely moves is a click.
struct Press {
    start: gpui::Point<Pixels>,
    last: gpui::Point<Pixels>,
    dragged: bool,
}

pub struct StrataView {
    focus_handle: FocusHandle,
    buffer: Entity<Buffer>,
    spec: Arc<Spec>,
    /// Problems in the buffer as it is now. While there are any, the last
    /// valid spec stays on screen.
    problems: Vec<Diagnostic>,
    step_index: usize,
    playback: Playback,
    scrubbing: bool,
    scrubber_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    state: State,
    camera: Camera,
    press: Option<Press>,
    text_cache: Rc<RefCell<TextCache>>,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    selected: Option<String>,
    focus_since: Option<Instant>,
    framing: Framing,
    /// The element under the mouse, outlined and given a pointer cursor.
    hovered: Option<String>,
    /// Playback of a newly shown step waits while the camera glides to it.
    awaiting_camera: bool,
    /// Lay each step out on its own, fitting what it holds, instead of the
    /// stable layout of every step at once.
    fit_each_step: bool,
    /// The picture of the step shown before the current one, fading out
    /// since the moment the step changed.
    outgoing: Option<(Rc<Scene>, Instant)>,
    /// What the current step changes, before or after it.
    change_rects: Vec<Rect>,
    change_region_step: Option<usize>,
    stack_collapsed: bool,
    reload_task: Task<()>,
    layout_task: Task<()>,
    _buffer_subscription: Subscription,
}

pub enum StrataViewEvent {
    TitleChanged,
}

impl EventEmitter<StrataViewEvent> for StrataView {}

impl StrataView {
    pub fn new(buffer: Entity<Buffer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe_in(
            &buffer,
            window,
            |this, _buffer, event: &BufferEvent, window, cx| match event {
                BufferEvent::Edited { .. } | BufferEvent::Saved => {
                    this.schedule_reload(window, cx);
                }
                BufferEvent::FileHandleChanged | BufferEvent::Reloaded => {
                    cx.emit(StrataViewEvent::TitleChanged);
                }
                _ => {}
            },
        );
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            buffer,
            spec: Arc::new(empty_spec()),
            problems: Vec::new(),
            step_index: 0,
            playback: Playback {
                t: 0.0,
                playing: false,
                speed: NORMAL_SPEED,
                last_tick: None,
            },
            scrubbing: false,
            scrubber_bounds: Rc::new(Cell::new(None)),
            state: State::Waiting,
            camera: Camera::default(),
            press: None,
            text_cache: Rc::new(RefCell::new(TextCache::default())),
            canvas_bounds: Rc::new(Cell::new(None)),
            selected: None,
            focus_since: None,
            framing: Framing::Changes,
            hovered: None,
            awaiting_camera: false,
            fit_each_step: false,
            outgoing: None,
            change_rects: Vec::new(),
            change_region_step: None,
            stack_collapsed: false,
            reload_task: Task::ready(()),
            layout_task: Task::ready(()),
            _buffer_subscription: subscription,
        };
        this.reload_now(window, cx);
        this
    }

    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    pub fn problems(&self) -> &[Diagnostic] {
        &self.problems
    }

    pub fn step_index(&self) -> usize {
        self.step_index
    }

    pub fn playback(&self) -> &Playback {
        &self.playback
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready { .. })
    }

    pub fn step(&self) -> Option<&Step> {
        self.spec.steps.get(self.step_index)
    }

    pub fn stack_collapsed(&self) -> bool {
        self.stack_collapsed
    }

    pub fn toggle_stack_collapsed(&mut self, cx: &mut Context<Self>) {
        self.stack_collapsed = !self.stack_collapsed;
        cx.notify();
    }

    pub fn scrubber_bounds(&self) -> Rc<Cell<Option<Bounds<Pixels>>>> {
        self.scrubber_bounds.clone()
    }

    pub fn timeline(&self) -> Option<&Timeline> {
        match &self.state {
            State::Ready { timeline, .. } => Some(timeline),
            _ => None,
        }
    }

    pub fn duration(&self) -> f32 {
        self.timeline()
            .map(|timeline| timeline.duration)
            .unwrap_or(0.0)
    }

    pub fn file_name(&self, cx: &App) -> SharedString {
        self.buffer
            .read(cx)
            .file()
            .map(|file| SharedString::from(file.file_name(cx).to_string()))
            .unwrap_or_else(|| "untitled".into())
    }

    fn schedule_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(RELOAD_DEBOUNCE).await;
            this.update_in(cx, |this, window, cx| this.reload_now(window, cx))
                .log_err();
        });
    }

    /// Parses the buffer now: a valid spec is laid out and shown, an invalid
    /// one keeps the last valid spec on screen and lists its problems.
    pub fn reload_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.buffer.read(cx).text();
        match strata_spec::check(&text) {
            Ok(spec) => {
                self.problems.clear();
                let unchanged = *self.spec == spec && !matches!(self.state, State::Waiting);
                if !unchanged {
                    let keep = self.step().map(|step| step.id.clone());
                    self.relayout(Arc::new(spec), keep, window, cx);
                }
            }
            Err(problems) => self.problems = problems,
        }
        cx.notify();
    }

    /// Lays out `spec` in the background and shows it when done, at the step
    /// with id `keep` if it still exists, else at the last step. The previous
    /// spec stays on screen until then. A newer reload cancels this one.
    fn relayout(
        &mut self,
        spec: Arc<Spec>,
        keep: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.state, State::Waiting) {
            self.state = State::Laying;
        }
        let font = ThemeSettings::get_global(cx).ui_font.clone();
        // Text shaping is bound to this thread, so measure here and lay out in
        // the background from the measured sizes.
        let measure = Measure::with_sizing(
            &spec,
            &WindowText::new(window.text_system().clone(), font),
            &diagram_theme::card_sizing(),
        );
        self.layout_task = cx.spawn_in(window, async move |this, cx| {
            let layout_spec = spec.clone();
            let result = cx
                .background_spawn(async move {
                    layout::step_layouts(
                        &layout_spec,
                        &measure,
                        LayoutEngine::Dugong,
                        EdgeRouting::Orthogonal,
                        LayoutSpacing::COMPACT,
                    )
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(layouts) => this.install(spec, Rc::new(layouts), keep, window, cx),
                    Err(error) => {
                        log::error!("strata: layout failed: {error:#}");
                        this.state = State::Failed(format!("Layout failed: {error:#}"));
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn install(
        &mut self,
        spec: Arc<Spec>,
        layouts: Rc<StepLayouts>,
        keep: Option<String>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = spec.steps.len().saturating_sub(1);
        let step_index = keep
            .and_then(|id| spec.steps.iter().position(|step| step.id == id))
            .unwrap_or(last);
        self.spec = spec;
        self.step_index = step_index;
        self.state = State::Ready {
            timeline: Rc::new(step_timeline_placed(
                &self.spec,
                step_index,
                placement(&layouts, step_index, self.fit_each_step),
            )),
            scene: Rc::new(Scene::empty()),
            layouts,
        };
        let end = self.duration();
        self.seek(end);
        // Edits keep a view the user moved; otherwise the camera glides to
        // what the edited step now changes.
        self.update_change_region();
        if self
            .selected
            .as_deref()
            .is_some_and(|id| self.scene_rect(id).is_none())
        {
            self.select(None);
        }
        cx.emit(StrataViewEvent::TitleChanged);
    }

    /// Before at the start of the step, After at its end, none in between.
    pub fn phase(&self) -> Option<Phase> {
        let duration = self.duration();
        if duration <= 0.0 || self.playback.t >= duration {
            Some(Phase::After)
        } else if self.playback.t <= 0.0 {
            Some(Phase::Before)
        } else {
            None
        }
    }

    /// The step whose snapshot is on screen: the previous one before a change.
    fn shown_step_index(&self) -> usize {
        match self.phase() {
            Some(Phase::Before) => self.step_index.saturating_sub(1),
            _ => self.step_index,
        }
    }

    /// Samples the scene at the playback time. With the stable layout,
    /// unchanged elements stay put; with a layout per step, they settle from
    /// the previous step's layout into this step's.
    fn rebuild_scene(&mut self) {
        let State::Ready {
            layouts,
            timeline,
            scene,
        } = &mut self.state
        else {
            return;
        };
        match Scene::at_placed(
            &self.spec,
            self.step_index,
            timeline,
            self.playback.t,
            placement(layouts, self.step_index, self.fit_each_step),
        ) {
            Some(built) => *scene = Rc::new(built),
            None => self.state = State::Failed("The spec has no steps".into()),
        }
    }

    fn update_change_region(&mut self) {
        let State::Ready {
            layouts, timeline, ..
        } = &self.state
        else {
            return;
        };
        let placed = placement(layouts, self.step_index, self.fit_each_step);
        let ends = [0.0, timeline.duration];
        self.change_rects = ends
            .iter()
            .filter_map(|&t| Scene::at_placed(&self.spec, self.step_index, timeline, t, placed))
            .flat_map(|scene| {
                let nodes = scene
                    .nodes
                    .iter()
                    .filter(|node| node.look.mark.is_some())
                    .map(|node| node.rect)
                    .collect::<Vec<_>>();
                let boundaries = scene
                    .boundaries
                    .iter()
                    .filter(|boundary| boundary.look.mark.is_some())
                    .map(|boundary| boundary.rect)
                    .collect::<Vec<_>>();
                nodes.into_iter().chain(boundaries)
            })
            .collect();
        self.change_region_step = Some(self.step_index);
    }

    /// The layout region the camera frames when the user has not moved it.
    fn framing_region(&self, content: layout::Size, viewport: gpui::Size<Pixels>) -> Rect {
        let everything = Rect {
            x: 0.0,
            y: 0.0,
            w: content.w,
            h: content.h,
        };
        match self.framing {
            Framing::Everything => return everything,
            Framing::Selection => {
                if let Some(rect) = self.selected.as_deref().and_then(|id| self.scene_rect(id)) {
                    let margin = SELECTION_FRAME_MARGIN;
                    return Rect {
                        x: rect.x - margin,
                        y: rect.y - margin,
                        w: rect.w + 2.0 * margin,
                        h: rect.h + 2.0 * margin,
                    };
                }
            }
            Framing::Changes => {}
        }
        let readable = |region: Rect| frame_view(region, viewport).scale >= READABLE_SCALE;
        if readable(everything) {
            return everything;
        }
        let Some(changes) = self.change_rects.iter().copied().reduce(union) else {
            return everything;
        };
        if readable(changes) {
            return changes;
        }
        // The changes spread wider than one readable view: frame the window
        // at the smallest readable scale that holds the most of them.
        let window = layout::Size {
            w: (f32::from(viewport.width) - 2.0 * FIT_MARGIN).max(1.0) / READABLE_SCALE,
            h: (f32::from(viewport.height) - 2.0 * FIT_MARGIN).max(1.0) / READABLE_SCALE,
        };
        densest_window(&self.change_rects, window).unwrap_or(changes)
    }

    pub fn zoom_percent(&self) -> Option<u32> {
        self.camera
            .view
            .map(|view| (view.scale * 100.0).round() as u32)
    }

    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// Zooms by `factor` around `anchor`, a canvas point, or the center.
    fn zoom_by(&mut self, factor: f32, anchor: Option<gpui::Point<f32>>, cx: &mut Context<Self>) {
        let center = self.canvas_bounds.get().map(|bounds| {
            point(
                f32::from(bounds.size.width) / 2.0,
                f32::from(bounds.size.height) / 2.0,
            )
        });
        let Some(anchor) = anchor.or(center) else {
            return;
        };
        let Some(view) = self.camera.adjust() else {
            return;
        };
        let new_scale = (view.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let ratio = new_scale / view.scale;
        view.offset.x = anchor.x - (anchor.x - view.offset.x) * ratio;
        view.offset.y = anchor.y - (anchor.y - view.offset.y) * ratio;
        view.scale = new_scale;
        cx.notify();
    }

    pub fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.zoom_by(ZOOM_STEP, None, cx);
    }

    pub fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.zoom_by(1.0 / ZOOM_STEP, None, cx);
    }

    pub fn set_framing(&mut self, framing: Framing, cx: &mut Context<Self>) {
        self.framing = framing;
        self.camera.reframe();
        cx.notify();
    }

    /// Jumps to `t` in the current step and stops playing.
    pub fn seek(&mut self, t: f32) {
        self.playback.t = t.clamp(0.0, self.duration());
        self.playback.playing = false;
        self.rebuild_scene();
    }

    /// Shows a step at its start or end, without animating.
    pub fn go_to(&mut self, step_index: usize, phase: Phase, cx: &mut Context<Self>) {
        if step_index >= self.spec.steps.len() {
            return;
        }
        if step_index != self.step_index
            && !cx.reduce_motion()
            && let State::Ready { scene, .. } = &self.state
        {
            self.outgoing = Some((scene.clone(), Instant::now()));
        }
        self.step_index = step_index;
        if let State::Ready {
            timeline, layouts, ..
        } = &mut self.state
        {
            *timeline = Rc::new(step_timeline_placed(
                &self.spec,
                step_index,
                placement(layouts, step_index, self.fit_each_step),
            ));
        }
        self.seek(match phase {
            Phase::Before => 0.0,
            Phase::After => self.duration(),
        });
        let step_changed = self.change_region_step != Some(step_index);
        self.update_change_region();
        if step_changed {
            self.framing = Framing::Changes;
            self.camera.reframe();
            self.awaiting_camera = true;
        }
        if self
            .selected
            .as_deref()
            .is_some_and(|id| self.scene_rect(id).is_none())
        {
            self.select(None);
        }
        cx.emit(StrataViewEvent::TitleChanged);
        cx.notify();
    }

    /// Shows a step and plays its change from the start. With reduced
    /// motion, it jumps to the end state instead.
    pub fn play_step(&mut self, step_index: usize, cx: &mut Context<Self>) {
        self.go_to(step_index, Phase::Before, cx);
        self.play(cx);
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        let duration = self.duration();
        if cx.reduce_motion() || duration <= 0.0 {
            self.seek(duration);
        } else {
            if self.playback.t >= duration {
                self.playback.t = 0.0;
            }
            self.playback.playing = true;
            self.playback.last_tick = None;
        }
        cx.notify();
    }

    pub fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playback.playing {
            self.playback.playing = false;
            cx.notify();
        } else {
            self.play(cx);
        }
    }

    pub fn change_speed(&mut self, faster: bool, cx: &mut Context<Self>) {
        self.playback.speed = if faster {
            (self.playback.speed + 1).min(SPEEDS.len() - 1)
        } else {
            self.playback.speed.saturating_sub(1)
        };
        cx.notify();
    }

    pub fn fit_each_step(&self) -> bool {
        self.fit_each_step
    }

    /// Switches between the stable layout and one that fits each step. The
    /// step shows its after view in the new layout, faded in from the old
    /// picture while the camera frames it again.
    pub fn toggle_fit_each_step(&mut self, cx: &mut Context<Self>) {
        self.fit_each_step = !self.fit_each_step;
        if let State::Ready {
            timeline,
            layouts,
            scene,
        } = &mut self.state
        {
            if !cx.reduce_motion() {
                self.outgoing = Some((scene.clone(), Instant::now()));
            }
            *timeline = Rc::new(step_timeline_placed(
                &self.spec,
                self.step_index,
                placement(layouts, self.step_index, self.fit_each_step),
            ));
        }
        let end = self.duration();
        self.seek(end);
        self.update_change_region();
        self.framing = Framing::Changes;
        self.camera.reframe();
        cx.notify();
    }

    fn toggle_fit_each_step_action(
        &mut self,
        _: &ToggleFitEachStep,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_fit_each_step(cx);
    }

    pub fn cycle_speed(&mut self, cx: &mut Context<Self>) {
        self.playback.speed = (self.playback.speed + 1) % SPEEDS.len();
        cx.notify();
    }

    /// Moves the clock forward by `elapsed` seconds of real time, scaled by
    /// the playback speed.
    pub fn tick(&mut self, elapsed: f32) {
        if !self.playback.playing {
            return;
        }
        let speed = SPEEDS.get(self.playback.speed).copied().unwrap_or(1.0);
        let duration = self.duration();
        self.playback.t = (self.playback.t + elapsed * speed).min(duration);
        if self.playback.t >= duration {
            self.playback.playing = false;
        }
        self.rebuild_scene();
    }

    fn advance(&mut self, window: &Window) {
        if !self.playback.playing {
            self.playback.last_tick = None;
            return;
        }
        // The step's change starts once the camera has arrived at it, so the
        // beats play where the eye already is rather than during the move.
        if self.awaiting_camera {
            self.playback.last_tick = None;
            window.request_animation_frame();
            return;
        }
        let now = Instant::now();
        let elapsed = self
            .playback
            .last_tick
            .map(|last| now.duration_since(last).as_secs_f32() * recording_time_scale())
            .unwrap_or(0.0);
        self.playback.last_tick = Some(now);
        self.tick(elapsed);
        window.request_animation_frame();
    }

    pub fn scrub_to(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.scrubber_bounds.get() else {
            return;
        };
        let width = f32::from(bounds.size.width);
        if width <= 0.0 {
            return;
        }
        let fraction = (f32::from(position.x - bounds.origin.x) / width).clamp(0.0, 1.0);
        self.seek(fraction * self.duration());
        cx.notify();
    }

    pub fn start_scrub(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        self.scrubbing = true;
        self.scrub_to(position, cx);
    }

    pub fn move_scrub(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.scrubbing {
            if event.pressed_button == Some(MouseButton::Left) {
                self.scrub_to(event.position, cx);
            } else {
                self.scrubbing = false;
            }
        }
    }

    pub fn is_scrubbing(&self) -> bool {
        self.scrubbing
    }

    pub fn end_scrub(&mut self) {
        self.scrubbing = false;
    }

    pub fn select(&mut self, id: Option<String>) {
        self.focus_since = id.as_ref().map(|_| Instant::now());
        self.selected = id;
    }

    fn previous_step(&mut self, _: &PreviousStep, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.step_index.checked_sub(1) {
            self.play_step(index, cx);
        }
    }

    fn next_step(&mut self, _: &NextStep, _: &mut Window, cx: &mut Context<Self>) {
        if self.step_index + 1 < self.spec.steps.len() {
            self.play_step(self.step_index + 1, cx);
        }
    }

    fn toggle_playback(&mut self, _: &TogglePlayback, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_play(cx);
    }

    fn scrub_backward(&mut self, _: &ScrubBackward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek(self.playback.t - SCRUB_STEP);
        cx.notify();
    }

    fn scrub_forward(&mut self, _: &ScrubForward, _: &mut Window, cx: &mut Context<Self>) {
        self.seek(self.playback.t + SCRUB_STEP);
        cx.notify();
    }

    fn slow_down(&mut self, _: &SlowDown, _: &mut Window, cx: &mut Context<Self>) {
        self.change_speed(false, cx);
    }

    fn speed_up(&mut self, _: &SpeedUp, _: &mut Window, cx: &mut Context<Self>) {
        self.change_speed(true, cx);
    }

    fn show_before(&mut self, _: &ShowBefore, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to(self.step_index, Phase::Before, cx);
    }

    fn show_after(&mut self, _: &ShowAfter, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to(self.step_index, Phase::After, cx);
    }

    fn close_details(&mut self, _: &CloseDetails, _: &mut Window, cx: &mut Context<Self>) {
        self.select(None);
        cx.notify();
    }

    fn zoom_in_action(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_in(cx);
    }

    fn zoom_out_action(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom_out(cx);
    }

    fn fit_to_view(&mut self, _: &FitToView, _: &mut Window, cx: &mut Context<Self>) {
        self.set_framing(Framing::Everything, cx);
    }

    fn frame_changes(&mut self, _: &FrameChanges, _: &mut Window, cx: &mut Context<Self>) {
        self.set_framing(Framing::Changes, cx);
    }

    /// Floats over the canvas's bottom right corner, like a map's controls.
    fn render_zoom_controls(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let percent = self.zoom_percent()?;
        let framed = !self.camera.user_adjusted;
        let framing = self.framing;
        Some(
            h_flex()
                .absolute()
                .bottom_2()
                .right_2()
                .gap_0p5()
                .p_0p5()
                .elevation_2(cx)
                .child(
                    IconButton::new("strata-zoom-out", IconName::Dash)
                        .icon_size(IconSize::Small)
                        .tooltip(|_, cx| Tooltip::for_action("Zoom Out", &ZoomOut, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx))),
                )
                .child(
                    div().w(px(40.)).flex().justify_center().child(
                        Label::new(format!("{percent}%"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    IconButton::new("strata-zoom-in", IconName::Plus)
                        .icon_size(IconSize::Small)
                        .tooltip(|_, cx| Tooltip::for_action("Zoom In", &ZoomIn, cx))
                        .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx))),
                )
                .child(Divider::vertical())
                .child(
                    IconButton::new("strata-frame-changes", IconName::Crosshair)
                        .icon_size(IconSize::Small)
                        .toggle_state(framed && framing == Framing::Changes)
                        .tooltip(|_, cx| {
                            Tooltip::for_action("Frame This Step's Changes", &FrameChanges, cx)
                        })
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_framing(Framing::Changes, cx)),
                        ),
                )
                .child(
                    IconButton::new("strata-fit-to-view", IconName::Maximize)
                        .icon_size(IconSize::Small)
                        .toggle_state(framed && framing == Framing::Everything)
                        .tooltip(|_, cx| Tooltip::for_action("Fit Stack to View", &FitToView, cx))
                        .on_click(
                            cx.listener(|this, _, _, cx| this.set_framing(Framing::Everything, cx)),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The focus for the selected element, at the current time.
    fn focus(&self) -> Option<Focus> {
        let id = self.selected.as_deref()?;
        let since = self.focus_since?;
        let State::Ready { scene, .. } = &self.state else {
            return None;
        };
        let steps: Vec<&Step> = [
            self.spec.steps.get(self.shown_step_index()),
            self.step(),
            self.step_index
                .checked_sub(1)
                .and_then(|index| self.spec.steps.get(index)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut group: HashSet<String> = HashSet::from([id.to_string()]);
        for step in &steps {
            for element in &step.elements {
                if has_ancestor(step, element, id) {
                    group.insert(element.id.clone());
                }
            }
        }
        let mut lit = group.clone();
        let mut edges = HashSet::new();
        for edge in &scene.edges {
            if group.contains(edge.from.as_ref()) || group.contains(edge.to.as_ref()) {
                edges.insert((edge.from.to_string(), edge.to.to_string()));
                lit.insert(edge.from.to_string());
                lit.insert(edge.to.to_string());
            }
        }
        let elapsed = seconds_since(since);
        let cycle = (elapsed % diagram_theme::FLOW_SECONDS) / diagram_theme::FLOW_SECONDS;
        Some(Focus {
            lit,
            edges,
            fade: FOCUS_EASING.ease(elapsed / diagram_theme::FOCUS_FADE_SECONDS),
            // LikeC4 animates the dash offset from 36 px down to 0, which moves
            // the dashes from source to target.
            flow: diagram_theme::FLOW_DISTANCE * (1.0 - cycle),
        })
    }

    fn canvas_point(&self, position: gpui::Point<Pixels>) -> gpui::Point<f32> {
        let origin = self
            .canvas_bounds
            .get()
            .map(|bounds| bounds.origin)
            .unwrap_or_default();
        point(
            f32::from(position.x - origin.x),
            f32::from(position.y - origin.y),
        )
    }

    /// The rect of `id` in the current scene, preferring the element itself
    /// over a ghost of it.
    fn scene_rect(&self, id: &str) -> Option<Rect> {
        let State::Ready { scene, .. } = &self.state else {
            return None;
        };
        let nodes = scene
            .nodes
            .iter()
            .map(|node| (&node.id, node.rect, node.look.ghost));
        let boundaries = scene
            .boundaries
            .iter()
            .map(|boundary| (&boundary.id, boundary.rect, boundary.look.ghost));
        let mut matches: Vec<(Rect, bool)> = nodes
            .chain(boundaries)
            .filter(|(item_id, _, _)| item_id.as_ref() == id)
            .map(|(_, rect, ghost)| (rect, ghost))
            .collect();
        matches.sort_by_key(|(_, ghost)| *ghost);
        matches.first().map(|(rect, _)| *rect)
    }

    /// The element card under a canvas point, ignoring ghosts and boundaries.
    fn hit_node(&self, canvas_point: gpui::Point<f32>) -> Option<String> {
        let State::Ready { scene, .. } = &self.state else {
            return None;
        };
        let view = self.camera.view?;
        let world = layout::Point {
            x: (canvas_point.x - view.offset.x) / view.scale,
            y: (canvas_point.y - view.offset.y) / view.scale,
        };
        scene
            .nodes
            .iter()
            .filter(|node| !node.look.ghost)
            .find(|node| {
                world.x >= node.rect.x
                    && world.x <= node.rect.right()
                    && world.y >= node.rect.y
                    && world.y <= node.rect.bottom()
            })
            .map(|node| node.id.to_string())
    }

    /// The element under a canvas point: nodes over boundaries, and the
    /// innermost boundary when several contain it.
    fn hit(&self, canvas_point: gpui::Point<f32>) -> Option<String> {
        let State::Ready { scene, .. } = &self.state else {
            return None;
        };
        let view = self.camera.view?;
        let world = layout::Point {
            x: (canvas_point.x - view.offset.x) / view.scale,
            y: (canvas_point.y - view.offset.y) / view.scale,
        };
        let contains = |rect: &Rect| {
            world.x >= rect.x
                && world.x <= rect.right()
                && world.y >= rect.y
                && world.y <= rect.bottom()
        };
        let mut nodes: Vec<&NodeItem> = scene
            .nodes
            .iter()
            .filter(|node| contains(&node.rect))
            .collect();
        nodes.sort_by_key(|node| node.look.ghost);
        if let Some(node) = nodes.first() {
            return Some(node.id.to_string());
        }
        scene
            .boundaries
            .iter()
            .filter(|boundary| contains(&boundary.rect))
            .min_by(|first, second| {
                (first.rect.w * first.rect.h).total_cmp(&(second.rect.w * second.rect.h))
            })
            .map(|boundary| boundary.id.to_string())
    }

    fn window_rect(&self, id: &str) -> Option<Bounds<Pixels>> {
        let rect = self.scene_rect(id)?;
        let view = self.camera.view?;
        let origin = self.canvas_bounds.get()?.origin;
        Some(Bounds {
            origin: point(
                origin.x + px(rect.x * view.scale + view.offset.x),
                origin.y + px(rect.y * view.scale + view.offset.y),
            ),
            size: size(px(rect.w * view.scale), px(rect.h * view.scale)),
        })
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.press = Some(Press {
            start: event.position,
            last: event.position,
            dragged: false,
        });
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else {
            return;
        };
        if press.dragged {
            return;
        }
        let hit = self.hit(self.canvas_point(event.position));
        // Clicking the open element again, or empty canvas, closes the details.
        let next = match hit {
            Some(id) if self.selected.as_deref() != Some(id.as_str()) => Some(id),
            _ => None,
        };
        self.select(next);
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.press = None;
            let hovered = self.hit_node(self.canvas_point(event.position));
            if hovered != self.hovered {
                self.hovered = hovered;
                cx.notify();
            }
            return;
        }
        let Some(press) = self.press.as_mut() else {
            return;
        };
        let travelled = event.position - press.start;
        if !press.dragged && f32::from(travelled.x).hypot(f32::from(travelled.y)) < CLICK_SLOP {
            return;
        }
        press.dragged = true;
        let delta = event.position - press.last;
        press.last = event.position;
        self.pan_by(delta, cx);
    }

    fn pan_by(&mut self, delta: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(view) = self.camera.adjust() else {
            return;
        };
        view.offset.x += f32::from(delta.x);
        view.offset.y += f32::from(delta.y);
        cx.notify();
    }

    /// Scrolling pans and Cmd or Ctrl with scrolling zooms, as in Zed's
    /// image viewer.
    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(LINE_SCROLL_PIXELS));
        if event.modifiers.control || event.modifiers.platform {
            let cursor = self.canvas_point(event.position);
            self.zoom_by(
                (f32::from(delta.y) * ZOOM_PER_PIXEL).exp(),
                Some(cursor),
                cx,
            );
        } else {
            self.pan_by(delta, cx);
        }
    }

    fn on_pinch(&mut self, event: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.canvas_point(event.position);
        self.zoom_by(1.0 + event.delta, Some(cursor), cx);
    }

    fn render_canvas(
        &mut self,
        viewport: gpui::Size<Pixels>,
        font: Font,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        match &self.state {
            State::Waiting => placeholder("Waiting for a valid spec\u{2026}"),
            State::Laying => placeholder("Laying out\u{2026}"),
            State::Failed(message) => div()
                .p_3()
                .child(Label::new(message.clone()).color(Color::Error))
                .into_any_element(),
            State::Ready { scene, .. } => {
                let region = self.framing_region(scene.size, viewport);
                let (view, gliding) = self.camera.view_for(region, viewport, cx.reduce_motion());
                if gliding {
                    window.request_animation_frame();
                } else {
                    self.awaiting_camera = false;
                }
                let scene = scene.clone();
                let selected = self.selected.as_deref().and_then(|id| self.scene_rect(id));
                let focus = self.focus();
                if focus.is_some() {
                    // Connected relationships keep flowing while focused.
                    window.request_animation_frame();
                }
                let cache = self.text_cache.clone();
                let hovered = self.hovered.clone();
                let colors = DiagramColors::from_theme(cx);
                // The previous step's picture fades out while the new one
                // fades in, over the camera's glide.
                let outgoing = self.outgoing.take().and_then(|(previous, since)| {
                    let progress = seconds_since(since) / GLIDE_SECONDS;
                    (progress < 1.0).then(|| {
                        self.outgoing = Some((previous.clone(), since));
                        (previous, FOCUS_EASING.ease(progress))
                    })
                });
                if outgoing.is_some() {
                    window.request_animation_frame();
                }
                let incoming = outgoing.as_ref().map_or(1.0, |(_, progress)| *progress);
                canvas(
                    |_, _, _| {},
                    move |bounds: Bounds<Pixels>, _, window, cx| {
                        window.paint_layer(|window| {
                            if let Some((previous, progress)) = &outgoing {
                                paint_scene(
                                    previous,
                                    Highlight {
                                        focus: None,
                                        selected: None,
                                        hovered: None,
                                        opacity: 1.0 - progress,
                                        grid: true,
                                    },
                                    view,
                                    bounds,
                                    &cache,
                                    &font,
                                    colors,
                                    window,
                                    cx,
                                );
                            }
                            paint_scene(
                                &scene,
                                Highlight {
                                    focus: focus.as_ref(),
                                    selected,
                                    hovered: hovered.as_deref(),
                                    opacity: incoming,
                                    grid: outgoing.is_none(),
                                },
                                view,
                                bounds,
                                &cache,
                                &font,
                                colors,
                                window,
                                cx,
                            );
                        });
                    },
                )
                .absolute()
                .size_full()
                .into_any_element()
            }
        }
    }

    fn render_problems(&self, cx: &App) -> Option<AnyElement> {
        const SHOWN: usize = 4;
        if self.problems.is_empty() {
            return None;
        }
        let count = self.problems.len();
        let noun = if count == 1 { "problem" } else { "problems" };
        let shown = if self.spec.steps.is_empty() {
            "nothing to show until it is valid"
        } else {
            "showing the last valid version"
        };
        let status = cx.theme().status();
        let lines = self.problems.iter().take(SHOWN).map(|problem| {
            let location = match (problem.line, problem.column) {
                (Some(line), Some(column)) => format!(" (line {line}, column {column})"),
                _ => String::new(),
            };
            let scope = [problem.step.as_deref(), problem.id.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" / ");
            let scope = if scope.is_empty() {
                String::new()
            } else {
                format!("[{scope}] ")
            };
            Label::new(format!(
                "{} {scope}{}{location}",
                problem.kind, problem.message
            ))
            .size(LabelSize::Small)
            .color(Color::Muted)
        });
        Some(
            h_flex()
                .flex_none()
                .items_start()
                .gap_2()
                .px_3()
                .py_1p5()
                .bg(status.error_background)
                .border_b_1()
                .border_color(status.error_border)
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(Label::new(format!(
                            "{count} {noun} in {}: {shown}",
                            self.file_name(cx)
                        )))
                        .children(lines)
                        .when(count > SHOWN, |this| {
                            this.child(
                                Label::new(format!("and {} more", count - SHOWN))
                                    .size(LabelSize::Small)
                                    .color(Color::Placeholder),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// The step's summary, takeaway and notes in one slim strip.
    /// The step's story above the canvas: its summary, the takeaway as a
    /// callout, and per-element notes whose element chips select and frame
    /// that element.
    fn render_description(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let step = self.step()?;
        let shown = self.spec.steps.get(self.shown_step_index());
        let note_steps: Vec<&Step> = [Some(step), shown].into_iter().flatten().collect();
        let colors = cx.theme().colors();
        let accent = cx.theme().players().local().cursor;
        let notes: Vec<AnyElement> = step
            .notes
            .iter()
            .enumerate()
            .map(|(index, (id, note))| {
                let kind = element_in(&note_steps, id).map(|element| element.kind);
                let id = id.clone();
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        ButtonLike::new(("strata-note-element", index))
                            .style(ButtonStyle::Filled)
                            .size(ButtonSize::Compact)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .px_0p5()
                                    .children(kind.map(|kind| {
                                        Icon::new(card_icon(kind))
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted)
                                    }))
                                    .child(
                                        Label::new(label_of(&note_steps, &id))
                                            .size(LabelSize::Small),
                                    ),
                            )
                            .tooltip(Tooltip::text("Show this element"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select(Some(id.clone()));
                                this.set_framing(Framing::Selection, cx);
                            })),
                    )
                    .child(
                        div().flex_1().min_w_0().pt_0p5().child(
                            Label::new(note.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        if step.summary.is_none() && step.takeaway.is_none() && notes.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .id("strata-step-story")
                .flex_none()
                .max_h(px(STORY_MAX_HEIGHT))
                .overflow_y_scroll()
                .gap_2()
                .px_3()
                .py_2p5()
                .bg(colors.editor_background)
                .border_b_1()
                .border_color(colors.border_variant)
                .children(step.summary.clone().map(Label::new))
                .children(step.takeaway.clone().map(|takeaway| {
                    h_flex()
                        .items_start()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(accent.opacity(0.08))
                        .border_l_2()
                        .border_color(accent.opacity(0.6))
                        .child(
                            div().pt_0p5().child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(takeaway).size(LabelSize::Small)),
                        )
                }))
                .children(notes)
                .into_any_element(),
        )
    }

    /// The selected element's details, as a popover next to it.
    fn render_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.selected.as_deref()?;
        let target = self.window_rect(id)?;
        let current = self.step()?;
        let previous = self
            .step_index
            .checked_sub(1)
            .and_then(|index| self.spec.steps.get(index));
        let shown = self.spec.steps.get(self.shown_step_index())?;
        let lookup: Vec<&Step> = [Some(shown), Some(current), previous]
            .into_iter()
            .flatten()
            .collect();
        let element = element_in(&lookup, id)?;
        let owner = lookup
            .iter()
            .find(|step| step.elements.iter().any(|candidate| candidate.id == id))
            .copied()?;
        let diff = diff_into(&self.spec.steps, self.step_index);

        let (change_color, change_text) = match diff.element(id) {
            ElementChange::Unchanged => (Color::Placeholder, "Unchanged in this step".to_string()),
            ElementChange::Added => (Color::Created, format!("Added in {}", current.id)),
            ElementChange::Removed => (Color::Deleted, format!("Removed in {}", current.id)),
            ElementChange::Changed => {
                let fields = previous
                    .and_then(|previous| element_in(&[previous], id))
                    .and_then(|before| {
                        element_in(&[current], id).map(|after| changed_fields(before, after))
                    })
                    .unwrap_or_default();
                (Color::Modified, format!("Changed {}", fields.join(", ")))
            }
            ElementChange::Moved { changed, .. } => {
                let place = |labels: Vec<String>| {
                    if labels.is_empty() {
                        "top level".to_string()
                    } else {
                        labels.join(" \u{203a} ")
                    }
                };
                let from = previous
                    .and_then(|previous| {
                        element_in(&[previous], id).map(|before| path_labels(previous, before))
                    })
                    .unwrap_or_default();
                let to = element_in(&[current], id)
                    .map(|after| path_labels(current, after))
                    .unwrap_or_default();
                let also = if changed { ", and changed" } else { "" };
                (
                    Color::Modified,
                    format!("Moved from {} to {}{also}", place(from), place(to)),
                )
            }
        };

        let kind = element.kind.display_name().to_string();
        let kind_line = match &element.technology {
            Some(technology) => format!("{kind} \u{b7} {technology}"),
            None => kind,
        };
        let path = path_labels(owner, element);
        let relationship_row = |relationship: &Relationship, outgoing: bool| {
            let other = if outgoing {
                &relationship.to
            } else {
                &relationship.from
            };
            let arrow_color = match diff.relationship(&relationship.from, &relationship.to) {
                RelationshipChange::Added => Color::Created,
                RelationshipChange::Removed => Color::Deleted,
                RelationshipChange::Unchanged => Color::Muted,
            };
            let detail: Vec<String> = [
                relationship.display_label().map(String::from),
                relationship.technology.clone(),
            ]
            .into_iter()
            .flatten()
            .collect();
            h_flex()
                .gap_1p5()
                .h(px(22.))
                .child(
                    Label::new(if outgoing { "\u{2192}" } else { "\u{2190}" })
                        .size(LabelSize::Small)
                        .color(arrow_color),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(Label::new(label_of(&lookup, other)).size(LabelSize::Small)),
                )
                .when(!detail.is_empty(), |this| {
                    this.child(
                        Label::new(detail.join(" \u{b7} "))
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    )
                })
        };
        let outgoing: Vec<_> = owner
            .relationships
            .iter()
            .filter(|relationship| relationship.from == id)
            .map(|relationship| relationship_row(relationship, true))
            .collect();
        let incoming: Vec<_> = owner
            .relationships
            .iter()
            .filter(|relationship| relationship.to == id)
            .map(|relationship| relationship_row(relationship, false))
            .collect();
        let note = current.notes.get(id).cloned();
        let section = |title: &'static str| {
            Label::new(title)
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
        };

        let card = Popover::new().child(
            v_flex()
                .id("strata-details")
                .occlude()
                .w(px(DETAILS_WIDTH))
                .gap_1()
                .px_3()
                .py_2()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll(|_, _, cx| cx.stop_propagation())
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(Label::new(element.label.clone())),
                        )
                        .child(
                            IconButton::new("close-details", IconName::Close)
                                .icon_size(IconSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.select(None);
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    Label::new(kind_line)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .when(!path.is_empty(), |this| {
                    this.child(
                        Label::new(path.join(" \u{203a} "))
                            .size(LabelSize::XSmall)
                            .color(Color::Placeholder),
                    )
                })
                .children(
                    element
                        .description
                        .clone()
                        .map(|text| Label::new(text).size(LabelSize::Small).color(Color::Muted)),
                )
                .child(
                    Label::new(change_text)
                        .size(LabelSize::Small)
                        .color(change_color),
                )
                .when(!outgoing.is_empty() || !incoming.is_empty(), |this| {
                    this.child(Divider::horizontal())
                        .when(!outgoing.is_empty(), |this| {
                            this.child(section("Outgoing")).children(outgoing)
                        })
                        .when(!incoming.is_empty(), |this| {
                            this.child(section("Incoming")).children(incoming)
                        })
                })
                .children(note.map(|note| {
                    v_flex()
                        .gap_1()
                        .child(Divider::horizontal())
                        .child(section("Note"))
                        .child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
                })),
        );

        // Keep the popover inside the canvas: beside the element if it fits
        // on either side, else pushed in from the edge.
        let width = px(DETAILS_WIDTH);
        let gap = px(DETAILS_GAP);
        let right_side = target.right() + gap;
        let left_side = target.left() - gap - width;
        let left = match self.canvas_bounds.get() {
            None => right_side,
            Some(canvas) => {
                let (least, most) = (canvas.left() + gap, canvas.right() - gap - width);
                if right_side <= most {
                    right_side
                } else if left_side >= least && left_side <= most {
                    left_side
                } else {
                    right_side.min(most).max(least)
                }
            }
        };
        // Vertically, start level with the element but stay clear of the
        // zoom controls in the canvas's bottom corner, scrolling if needed.
        let (top, max_height) = match self.canvas_bounds.get() {
            None => (target.top(), None),
            Some(canvas) => {
                let floor = canvas.bottom() - px(ZOOM_CONTROLS_CLEARANCE);
                let ceiling = canvas.top() + gap;
                let top = target
                    .top()
                    .min(floor - px(DETAILS_MIN_HEIGHT))
                    .max(ceiling);
                (top, Some(floor - top))
            }
        };
        let card = div()
            .id("strata-details-scroll")
            .when_some(max_height, |this, height| this.max_h(height))
            .overflow_y_scroll()
            .child(card);
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(point(left, top))
                    .anchor(gpui::Anchor::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

fn empty_spec() -> Spec {
    Spec {
        version: strata_spec::SUPPORTED_VERSION,
        repo: String::new(),
        title: None,
        steps: Vec::new(),
    }
}

fn placeholder(text: &'static str) -> AnyElement {
    h_flex()
        .size_full()
        .justify_center()
        .child(Label::new(text).color(Color::Muted))
        .into_any_element()
}

fn element_in<'a>(steps: &[&'a Step], id: &str) -> Option<&'a Element> {
    steps
        .iter()
        .find_map(|step| step.elements.iter().find(|element| element.id == id))
}

fn label_of(steps: &[&Step], id: &str) -> String {
    element_in(steps, id)
        .map(|element| element.label.clone())
        .unwrap_or_else(|| id.to_string())
}

fn path_labels(step: &Step, element: &Element) -> Vec<String> {
    let mut labels = Vec::new();
    let mut current = element.parent.as_deref();
    while let Some(parent_id) = current {
        if labels.len() > step.elements.len() {
            break;
        }
        let Some(parent) = step
            .elements
            .iter()
            .find(|candidate| candidate.id == parent_id)
        else {
            break;
        };
        labels.push(parent.label.clone());
        current = parent.parent.as_deref();
    }
    labels.reverse();
    labels
}

fn changed_fields(before: &Element, after: &Element) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if before.label != after.label {
        fields.push("label");
    }
    if before.kind != after.kind {
        fields.push("kind");
    }
    if before.technology != after.technology {
        fields.push("technology");
    }
    if before.description != after.description {
        fields.push("description");
    }
    fields
}

impl Focusable for StrataView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StrataView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.advance(window);
        let font = ThemeSettings::get_global(cx).ui_font.clone();
        let viewport = window.viewport_size();
        let canvas_size = self
            .canvas_bounds
            .get()
            .map(|bounds| bounds.size)
            .unwrap_or_else(|| size(viewport.width.max(px(1.0)), viewport.height.max(px(1.0))));
        let bounds_cell = self.canvas_bounds.clone();
        let canvas_content = self.render_canvas(canvas_size, font.clone(), window, cx);
        let canvas_area = div()
            .id("strata-canvas")
            .relative()
            .flex_1()
            .w_full()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .child(
                canvas(
                    move |bounds, _, _| bounds_cell.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(|this, hovering: &bool, _, cx| {
                if !hovering && this.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .when(self.hovered.is_some(), |canvas| canvas.cursor_pointer())
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .child(canvas_content)
            .children(self.render_details(cx))
            .children(self.render_zoom_controls(cx));

        v_flex()
            .id("StrataView")
            .key_context("StrataView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .font(font)
            .on_action(cx.listener(Self::previous_step))
            .on_action(cx.listener(Self::next_step))
            .on_action(cx.listener(Self::toggle_playback))
            .on_action(cx.listener(Self::scrub_backward))
            .on_action(cx.listener(Self::scrub_forward))
            .on_action(cx.listener(Self::slow_down))
            .on_action(cx.listener(Self::speed_up))
            .on_action(cx.listener(Self::show_before))
            .on_action(cx.listener(Self::show_after))
            .on_action(cx.listener(Self::close_details))
            .on_action(cx.listener(Self::zoom_in_action))
            .on_action(cx.listener(Self::zoom_out_action))
            .on_action(cx.listener(Self::fit_to_view))
            .on_action(cx.listener(Self::frame_changes))
            .on_action(cx.listener(Self::toggle_fit_each_step_action))
            .children(self.render_problems(cx))
            .children(self.render_description(cx))
            .child(canvas_area)
    }
}

impl Item for StrataView {
    type Event = StrataViewEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        format!("Stack {}", self.file_name(cx)).into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("strata view: open")
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            StrataViewEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let buffer = self.buffer.clone();
        Task::ready(Some(cx.new(|cx| Self::new(buffer, window, cx))))
    }
}

impl StrataView {
    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(|workspace, _: &OpenPreview, window, cx| {
            let Some(item) = workspace.item_for_action(window, cx) else {
                return;
            };
            if let Some(buffer) = item
                .act_as::<MultiBuffer>(cx)
                .and_then(|multi_buffer| multi_buffer.read(cx).as_singleton())
                && let Some(pane) = workspace.pane_for_item_id(item.item_id())
            {
                Self::open_preview_in_pane(buffer, pane, true, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(buffer) = Self::resolve_active_buffer(workspace, cx) {
                let origin = workspace.active_pane().clone();
                let target = workspace.adjacent_pane_of(&origin, window, cx);
                Self::open_preview_in_pane(buffer, target, false, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &crate::OpenStack, window, cx| {
            Self::prompt_and_open_stack(workspace, window, cx);
        });
    }

    pub fn resolve_active_buffer(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<Buffer>> {
        workspace
            .active_item(cx)?
            .act_as::<MultiBuffer>(cx)?
            .read(cx)
            .as_singleton()
    }

    fn find_existing_preview(
        pane: &Pane,
        buffer: &Entity<Buffer>,
        cx: &App,
    ) -> Option<(Entity<StrataView>, usize)> {
        let view = pane
            .items_of_type::<StrataView>()
            .find(|view| view.read(cx).buffer == *buffer)?;
        let index = pane.index_for_item(&view)?;
        Some((view, index))
    }

    pub fn open_preview_in_pane(
        buffer: Entity<Buffer>,
        pane: Entity<Pane>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<StrataView> {
        let existing = Self::find_existing_preview(pane.read(cx), &buffer, cx);
        let view = match existing {
            Some((view, index)) => {
                pane.update(cx, |pane, cx| {
                    pane.activate_item(index, focus, focus, window, cx);
                });
                view
            }
            None => {
                let view = cx.new(|cx| Self::new(buffer, window, cx));
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(view.clone()), focus, focus, None, window, cx)
                });
                view
            }
        };
        cx.notify();
        view
    }

    fn prompt_and_open_stack(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let paths = workspace.prompt_for_open_path(
            gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some("Open a stack spec".into()),
            },
            project::DirectoryLister::Local(
                workspace.project().clone(),
                workspace.app_state().fs.clone(),
            ),
            workspace::OpenPathPromptMode::SelectPath,
            window,
            cx,
        );
        cx.spawn_in(window, async move |workspace, cx| {
            let Some(result) = paths.await.log_err().flatten() else {
                return Ok(());
            };
            for path in result.paths {
                Self::open_stack_file(workspace.clone(), path, cx).await?;
            }
            Result::<()>::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Opens `path` as a buffer in the project and previews it.
    pub async fn open_stack_file(
        workspace: WeakEntity<Workspace>,
        path: std::path::PathBuf,
        cx: &mut gpui::AsyncWindowContext,
    ) -> Result<()> {
        let open_task = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(path.clone(), workspace::OpenOptions::default(), window, cx)
        })?;
        let item = open_task.await?;
        workspace.update_in(cx, |workspace, window, cx| {
            if let Some(buffer) = item
                .act_as::<MultiBuffer>(cx)
                .and_then(|multi_buffer| multi_buffer.read(cx).as_singleton())
            {
                let pane = workspace.active_pane().clone();
                Self::open_preview_in_pane(buffer, pane, true, window, cx);
            }
        })?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }
}

#[cfg(test)]
mod camera_tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    fn viewport() -> gpui::Size<Pixels> {
        size(px(800.0), px(600.0))
    }

    #[test]
    fn framing_centers_the_region_and_never_passes_actual_size() {
        let view = frame_view(rect(100.0, 100.0, 50.0, 50.0), viewport());
        assert_eq!(view.scale, MAX_FRAME_SCALE);
        let center = point(
            125.0 * view.scale + view.offset.x,
            125.0 * view.scale + view.offset.y,
        );
        assert_eq!(center, point(400.0, 300.0));
    }

    #[test]
    fn a_glide_starts_and_ends_on_its_views() {
        let from = frame_view(rect(0.0, 0.0, 2000.0, 1000.0), viewport());
        let to = frame_view(rect(600.0, 300.0, 200.0, 100.0), viewport());
        assert_eq!(interpolate_view(from, to, 0.0, viewport()), from);
        let end = interpolate_view(from, to, 1.0, viewport());
        assert!((end.scale - to.scale).abs() < 1e-4);
        assert!((end.offset.x - to.offset.x).abs() < 1e-2);
        assert!((end.offset.y - to.offset.y).abs() < 1e-2);
    }

    #[test]
    fn the_densest_window_holds_the_largest_cluster() {
        let cluster = [
            rect(0.0, 0.0, 100.0, 60.0),
            rect(150.0, 0.0, 100.0, 60.0),
            rect(0.0, 100.0, 100.0, 60.0),
        ];
        let outlier = rect(5000.0, 5000.0, 100.0, 60.0);
        let mut rects = cluster.to_vec();
        rects.push(outlier);
        let framed = densest_window(&rects, layout::Size { w: 600.0, h: 400.0 });
        assert_eq!(framed, Some(rect(0.0, 0.0, 250.0, 160.0)));
    }
}
