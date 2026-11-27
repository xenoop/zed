use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    BorderStyle, Bounds, BoxShadow, Corners, Font, FontWeight, Hsla, PathBuilder, Pixels,
    SharedString, TextAlign, TextRun, Window, WindowTextSystem, WrappedLine, px, size,
};
use strata_core::easing::CubicBezier;
use strata_core::layout::{self, EstimatedText, Rect, TextMeasure, TextRole};
use strata_core::timeline::partial_path;
use strata_spec::{Element, ElementKind, Step};

use crate::diagram_theme::{self as theme, DiagramColors};
use crate::scene::{BoundaryItem, EdgeItem, LabelItem, Look, NodeItem, NodeText, Scene};

pub const MIN_SCALE: f32 = 0.05;
pub const MAX_SCALE: f32 = 4.0;

pub fn font_with_weight(font: &Font, weight: f32) -> Font {
    Font {
        weight: FontWeight(weight),
        ..font.clone()
    }
}

pub fn run_for(text: &str, font: &Font, weight: f32, color: Hsla) -> TextRun {
    TextRun {
        len: text.len(),
        font: font_with_weight(font, weight),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Measures text with the same font the diagram is painted with.
pub struct WindowText {
    system: Rc<WindowTextSystem>,
    font: Font,
}

impl WindowText {
    pub fn new(system: Rc<WindowTextSystem>, font: Font) -> Self {
        Self { system, font }
    }
}

impl TextMeasure for WindowText {
    fn width(&self, text: &str, role: TextRole) -> f32 {
        text.lines()
            .map(|line| {
                let shaped = self.system.shape_line(
                    line.to_string().into(),
                    px(role.font_size()),
                    &[run_for(line, &self.font, role.font_weight(), gpui::black())],
                    None,
                );
                f32::from(shaped.width)
            })
            .fold(0.0, f32::max)
    }

    fn line_count(&self, text: &str, role: TextRole, wrap_width: f32) -> usize {
        let line_height = px(role.line_height());
        let shaped = self.system.shape_text(
            text.to_string().into(),
            px(role.font_size()),
            &[run_for(text, &self.font, role.font_weight(), gpui::black())],
            Some(px(wrap_width)),
            None,
        );
        match shaped {
            Ok(lines) => lines
                .iter()
                .map(|line| {
                    (f32::from(line.size(line_height).height) / role.line_height()).round() as usize
                })
                .sum::<usize>()
                .max(1),
            Err(error) => {
                log::error!("strata: text shaping failed, estimating: {error}");
                EstimatedText.line_count(text, role, wrap_width)
            }
        }
    }
}

#[derive(Clone, Copy)]
pub struct ViewState {
    pub offset: gpui::Point<f32>,
    pub scale: f32,
}

/// Shaped text, keyed by everything that shapes it: the string, font size
/// and weight, color (shaping bakes it in), wrap width and line limit.
/// Keys never refer to scene positions, which shift while a step animates.
#[derive(Default)]
pub struct TextCache {
    scale_bits: u32,
    font: Option<Font>,
    entries: HashMap<TextKey, Rc<ShapedBlock>>,
}

type TextKey = (SharedString, u32, u32, [u32; 4], Option<u32>, Option<usize>);

pub struct ShapedBlock {
    lines: Vec<WrappedLine>,
}

impl TextCache {
    /// Zooming changes every font size, so the old entries are dropped.
    fn prepare(&mut self, scale: f32, font: &Font) {
        if self.scale_bits != scale.to_bits() || self.font.as_ref() != Some(font) {
            self.scale_bits = scale.to_bits();
            self.font = Some(font.clone());
            self.entries.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn shape(
        &mut self,
        window: &Window,
        text: &SharedString,
        font_size: f32,
        weight: f32,
        tint: Hsla,
        wrap_width: Option<f32>,
        max_lines: Option<usize>,
    ) -> Option<Rc<ShapedBlock>> {
        let key = (
            text.clone(),
            font_size.to_bits(),
            weight.to_bits(),
            [
                tint.h.to_bits(),
                tint.s.to_bits(),
                tint.l.to_bits(),
                tint.a.to_bits(),
            ],
            wrap_width.map(f32::to_bits),
            max_lines,
        );
        if let Some(block) = self.entries.get(&key) {
            return Some(block.clone());
        }
        let font = self.font.as_ref()?;
        let shaped = window
            .text_system()
            .shape_text(
                text.clone(),
                px(font_size),
                &[run_for(text, font, weight, tint)],
                wrap_width.map(px),
                max_lines,
            )
            .map_err(|error| log::error!("strata: could not shape text: {error}"))
            .ok()?;
        let block = Rc::new(ShapedBlock {
            lines: shaped.into_iter().collect(),
        });
        self.entries.insert(key, block.clone());
        Some(block)
    }
}

/// Mark rings scale with zoom but never get thinner than a couple of pixels.
fn mark_width(scale: f32) -> f32 {
    (theme::MARK_WIDTH * scale).max(theme::MARK_MIN_SCREEN_WIDTH)
}

/// Maps layout coordinates to window pixels.
#[derive(Clone, Copy)]
struct Transform {
    origin: gpui::Point<Pixels>,
    offset: gpui::Point<f32>,
    scale: f32,
}

impl Transform {
    fn point(&self, world: layout::Point) -> gpui::Point<Pixels> {
        gpui::point(
            self.origin.x + px(world.x * self.scale + self.offset.x),
            self.origin.y + px(world.y * self.scale + self.offset.y),
        )
    }

    fn bounds(&self, rect: &Rect) -> Bounds<Pixels> {
        Bounds {
            origin: self.point(layout::Point {
                x: rect.x,
                y: rect.y,
            }),
            size: size(px(rect.w * self.scale), px(rect.h * self.scale)),
        }
    }

    fn visible(&self, rect: &Rect, viewport: &Bounds<Pixels>) -> bool {
        let screen = self.bounds(rect);
        screen.intersects(viewport)
    }
}

struct Painter<'a> {
    transform: Transform,
    viewport: Bounds<Pixels>,
    cache: &'a mut TextCache,
    colors: DiagramColors,
    /// Opacity of the item being painted.
    alpha: f32,
    /// Saturation multiplier of the item being painted (focus grays out).
    saturation: f32,
    /// For a relationship connected to the focused element: how far its
    /// dashes have flowed, in layout pixels.
    flow: Option<f32>,
}

impl Painter<'_> {
    /// A theme color faded and grayed for the current item.
    fn tint_color(&self, source: Hsla) -> Hsla {
        let mut tinted = source.opacity(self.alpha);
        tinted.s *= self.saturation;
        tinted
    }

    /// Opacity and saturation are rounded to 1/64 so that animation reuses
    /// shaped text instead of shaping it again on every frame.
    fn set_item(&mut self, alpha: f32, saturation: f32) {
        let quantize = |value: f32| (value.clamp(0.0, 1.0) * 64.0).round();
        self.alpha = quantize(alpha) / 64.0;
        self.saturation = quantize(saturation) / 64.0;
    }

    fn scale(&self) -> f32 {
        self.transform.scale
    }

    fn block_height(block: &ShapedBlock, line_height: f32) -> f32 {
        block
            .lines
            .iter()
            .map(|line| f32::from(line.size(px(line_height)).height))
            .sum()
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_block(
        block: &ShapedBlock,
        origin_x: f32,
        mut y: f32,
        width: f32,
        line_height: f32,
        align: TextAlign,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        for line in &block.lines {
            let origin = gpui::point(px(origin_x), px(y));
            let bounds = Bounds {
                origin,
                size: size(px(width), px(line_height)),
            };
            if let Err(error) = line.paint(origin, px(line_height), align, Some(bounds), window, cx)
            {
                log::error!("strata: could not paint text: {error}");
            }
            y += f32::from(line.size(px(line_height)).height);
        }
    }

    fn paint_boundary(&mut self, item: &BoundaryItem, window: &mut Window, cx: &mut gpui::App) {
        if !self.transform.visible(&item.rect, &self.viewport) {
            return;
        }
        let scale = self.scale();
        let bounds = self.transform.bounds(&item.rect);
        let fill = self.colors.boundary_fill;
        let mark = item.look.mark.map(|mark| self.colors.mark(mark));
        let (fill, border, border_width, border_style) = match (mark, item.look.ghost) {
            (Some(mark), true) => (
                self.tint_color(mark).opacity(theme::GHOST_FILL_OPACITY),
                self.tint_color(mark),
                mark_width(scale),
                BorderStyle::Dashed,
            ),
            (Some(mark), false) => (
                self.tint_color(fill),
                self.tint_color(mark),
                mark_width(scale),
                BorderStyle::Solid,
            ),
            (None, _) => (
                self.tint_color(fill),
                self.tint_color(self.colors.boundary_border),
                theme::COMPOUND_BORDER_WIDTH * scale,
                BorderStyle::Dashed,
            ),
        };
        window.paint_quad(gpui::quad(
            bounds,
            Corners::all(px(theme::COMPOUND_RADIUS * scale)),
            fill,
            px(border_width),
            border,
            border_style,
        ));
        let size_px = theme::COMPOUND_TITLE_SIZE;
        let title_origin_y =
            theme::COMPOUND_TITLE_TOP + (theme::COMPOUND_TITLE_MIN_HEIGHT - size_px) / 2.0;
        let title_color = match (mark, item.look.ghost) {
            (Some(mark), true) => mark,
            _ => self.colors.boundary_title,
        };
        let (alpha, saturation) = (self.alpha, self.saturation);
        let mut titles = Vec::with_capacity(2);
        if let Some(previous) = item
            .previous_title
            .as_ref()
            .filter(|_| item.content_mix < 1.0)
        {
            titles.push((previous, 1.0 - item.content_mix));
            titles.push((&item.title, item.content_mix));
        } else {
            titles.push((&item.title, 1.0));
        }
        for (title, share) in titles {
            if share <= 0.0 {
                continue;
            }
            self.set_item(alpha * share, saturation);
            let tint = self.tint_color(title_color);
            let Some(block) =
                self.cache
                    .shape(window, title, size_px * scale, 600.0, tint, None, Some(1))
            else {
                continue;
            };
            Self::paint_block(
                &block,
                f32::from(bounds.origin.x) + theme::COMPOUND_TITLE_LEFT * scale,
                f32::from(bounds.origin.y) + title_origin_y * scale,
                f32::from(bounds.size.width),
                size_px * scale,
                TextAlign::Left,
                window,
                cx,
            );
        }
        self.set_item(alpha, saturation);
    }

    fn paint_edge(&mut self, item: &EdgeItem, window: &mut Window) {
        if !self.transform.visible(&item.bounds, &self.viewport) {
            return;
        }
        let scale = self.scale();
        let width = px(match self.flow {
            Some(_) => theme::ACTIVE_EDGE_WIDTH,
            None => theme::EDGE_WIDTH,
        } * scale);
        let edge_color = match item.look.mark {
            Some(mark) => self.tint_color(self.colors.mark(mark)),
            None => self.tint_color(self.colors.edge),
        };
        let mut builder = PathBuilder::stroke(width);
        if let Some(flow) = self.flow {
            let pattern = flowing_dash(theme::FLOW_DASH[0], theme::FLOW_DASH[1], flow);
            let scaled: Vec<Pixels> = pattern.iter().map(|length| px(length * scale)).collect();
            builder = builder.dash_array(&scaled);
        } else if item.dashed {
            builder = builder.dash_array(&[
                px(theme::EDGE_DASH[0] * scale),
                px(theme::EDGE_DASH[1] * scale),
            ]);
        }
        let partial;
        let points: &[layout::Point] = if item.drawn < 1.0 {
            partial = partial_path(&item.points, item.drawn);
            &partial
        } else {
            &item.points
        };
        let Some((&start, rest)) = points.split_first() else {
            return;
        };
        builder.move_to(self.transform.point(start));
        for segment in rest.chunks_exact(3) {
            if let [control_a, control_b, end] = segment {
                builder.cubic_bezier_to(
                    self.transform.point(*end),
                    self.transform.point(*control_a),
                    self.transform.point(*control_b),
                );
            }
        }
        match builder.build() {
            Ok(path) => window.paint_path(path, edge_color),
            Err(error) => log::error!("strata: could not tessellate edge: {error}"),
        }

        // The arrowhead lands once the line has been drawn all the way.
        if item.drawn < 1.0 {
            return;
        }
        let Some(&base) = item.points.last() else {
            return;
        };
        let tip = item.tip;
        let (dx, dy) = (tip.x - base.x, tip.y - base.y);
        let length = dx.hypot(dy);
        if length < 0.01 {
            return;
        }
        let normal = (-dy / length, dx / length);
        let half_width = length * 0.42;
        let corners = [
            tip,
            layout::Point {
                x: base.x + normal.0 * half_width,
                y: base.y + normal.1 * half_width,
            },
            layout::Point {
                x: base.x - normal.0 * half_width,
                y: base.y - normal.1 * half_width,
            },
        ];
        let mut head = if item.hollow_head {
            PathBuilder::stroke(px(theme::EDGE_WIDTH * 0.66 * scale))
        } else {
            PathBuilder::fill()
        };
        for (index, corner) in corners.iter().enumerate() {
            let at = self.transform.point(*corner);
            if index == 0 {
                head.move_to(at);
            } else {
                head.line_to(at);
            }
        }
        head.close();
        match head.build() {
            Ok(path) => window.paint_path(path, edge_color),
            Err(error) => log::error!("strata: could not tessellate arrowhead: {error}"),
        }
    }

    fn paint_label(&mut self, item: &LabelItem, window: &mut Window, cx: &mut gpui::App) {
        if !self.transform.visible(&item.rect, &self.viewport) {
            return;
        }
        let scale = self.scale();
        let bounds = self.transform.bounds(&item.rect);
        window.paint_quad(gpui::quad(
            bounds,
            Corners::all(px(theme::EDGE_LABEL_RADIUS * scale)),
            self.tint_color(self.colors.label_background),
            0.0,
            gpui::transparent_black(),
            BorderStyle::default(),
        ));
        let metrics = layout::metrics::EDGE_LABEL_PADDING_INLINE;
        let inner_width = item.rect.w - 2.0 * metrics;
        let mut y = f32::from(bounds.origin.y) + layout::metrics::EDGE_LABEL_PADDING_BLOCK * scale;
        let x = f32::from(bounds.origin.x) + metrics * scale;
        if let Some(text) = &item.text {
            let role = TextRole::EdgeLabel;
            let tint = self.tint_color(self.colors.label_text);
            if let Some(block) = self.shape_colored(text, role, inner_width, tint, window) {
                Self::paint_block(
                    &block,
                    x,
                    y,
                    inner_width * scale,
                    role.line_height() * scale,
                    TextAlign::Left,
                    window,
                    cx,
                );
                y += Self::block_height(&block, role.line_height() * scale)
                    + layout::metrics::EDGE_LABEL_GAP * scale;
            }
        }
        if let Some(technology) = &item.technology {
            let role = TextRole::EdgeTechnology;
            let tint = self
                .tint_color(self.colors.label_text)
                .opacity(theme::EDGE_LABEL_TECHNOLOGY_OPACITY);
            if let Some(block) = self.shape_colored(technology, role, inner_width, tint, window) {
                Self::paint_block(
                    &block,
                    x,
                    y,
                    inner_width * scale,
                    role.line_height() * scale,
                    TextAlign::Left,
                    window,
                    cx,
                );
            }
        }
    }

    /// Shapes `text` with its final color baked into the run.
    fn shape_colored(
        &mut self,
        text: &SharedString,
        role: TextRole,
        wrap_width: f32,
        tint: Hsla,
        window: &Window,
    ) -> Option<Rc<ShapedBlock>> {
        let scale = self.scale();
        self.cache.shape(
            window,
            text,
            role.font_size() * scale,
            role.font_weight(),
            tint,
            Some(wrap_width * scale),
            None,
        )
    }

    fn paint_node(&mut self, item: &NodeItem, window: &mut Window, cx: &mut gpui::App) {
        if !self.transform.visible(&item.rect, &self.viewport) {
            return;
        }
        let scale = self.scale();
        let bounds = self.transform.bounds(&item.rect);
        let radii = Corners::all(px(theme::NODE_RADIUS * scale));
        let mark = item.look.mark.map(|mark| self.colors.mark(mark));
        if let (Some(mark), true) = (mark, item.look.ghost) {
            window.paint_quad(gpui::quad(
                bounds,
                radii,
                self.tint_color(mark).opacity(theme::GHOST_FILL_OPACITY),
                px(mark_width(scale)),
                self.tint_color(mark),
                BorderStyle::Dashed,
            ));
            self.paint_node_text(item, bounds, Some(mark), window, cx);
            return;
        }
        let shadows: Vec<BoxShadow> = theme::NODE_SHADOWS
            .iter()
            .map(|(offset_y, blur, alpha, accent)| BoxShadow {
                color: self
                    .tint_color(if *accent {
                        self.colors.accent
                    } else {
                        self.colors.shadow
                    })
                    .opacity(*alpha),
                offset: gpui::point(px(0.0), px(offset_y * scale)),
                blur_radius: px(blur * scale),
                spread_radius: px(0.0),
                inset: false,
            })
            .collect();
        window.paint_drop_shadows(bounds, radii, &shadows);

        match item.kind {
            ElementKind::Store => self.paint_cylinder(&item.rect, window),
            ElementKind::Person => {
                window.paint_quad(gpui::quad(
                    bounds,
                    radii,
                    self.tint_color(self.colors.node_fill),
                    px(theme::NODE_BORDER_WIDTH),
                    self.tint_color(self.colors.node_border),
                    BorderStyle::Solid,
                ));
                self.paint_person_icon(&item.rect, window);
            }
            ElementKind::System | ElementKind::Container | ElementKind::Component => {
                window.paint_quad(gpui::quad(
                    bounds,
                    radii,
                    self.tint_color(self.colors.node_fill),
                    px(theme::NODE_BORDER_WIDTH),
                    self.tint_color(self.colors.node_border),
                    BorderStyle::Solid,
                ));
            }
        }
        self.paint_node_text(item, bounds, None, window, cx);
        if let Some(mark) = mark {
            let gap = theme::MARK_GAP * scale + mark_width(scale) / 2.0;
            window.paint_quad(gpui::quad(
                bounds.dilate(px(gap)),
                Corners::all(px(theme::NODE_RADIUS * scale + gap)),
                gpui::transparent_black(),
                px(mark_width(scale)),
                self.tint_color(mark),
                BorderStyle::Solid,
            ));
        }
    }

    fn paint_node_text(
        &mut self,
        item: &NodeItem,
        bounds: Bounds<Pixels>,
        // Ghosts draw all their text in the mark color.
        ghost_color: Option<Hsla>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        // Changed content cross-fades: the old text fades out as the new one
        // fades in.
        let (alpha, saturation) = (self.alpha, self.saturation);
        let mut sets = Vec::with_capacity(2);
        if let Some(previous) = item
            .previous_text
            .as_ref()
            .filter(|_| item.content_mix < 1.0)
        {
            sets.push((previous, 1.0 - item.content_mix));
            sets.push((&item.text, item.content_mix));
        } else {
            sets.push((&item.text, 1.0));
        }
        for (text, share) in sets {
            if share <= 0.0 {
                continue;
            }
            self.set_item(alpha * share, saturation);
            self.paint_text_blocks(item, text, bounds, ghost_color, window, cx);
        }
        self.set_item(alpha, saturation);
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_text_blocks(
        &mut self,
        item: &NodeItem,
        text: &NodeText,
        bounds: Bounds<Pixels>,
        ghost_color: Option<Hsla>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        use layout::metrics::*;
        let scale = self.scale();
        let (top, right, bottom, left) = layout::node_paddings(item.kind);
        let text_width = item.rect.w - left - right;
        let title_tint = ghost_color.map_or_else(
            || self.tint_color(self.colors.node_title),
            |ghost| self.tint_color(ghost),
        );
        let secondary = ghost_color.map_or_else(
            || self.tint_color(self.colors.node_secondary),
            |ghost| self.tint_color(ghost),
        );
        let technology_tint = secondary.opacity(theme::TECHNOLOGY_OPACITY);

        struct Entry {
            block: Rc<ShapedBlock>,
            role: TextRole,
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut push =
            |this: &mut Self, text: &SharedString, role: TextRole, max_lines: usize, tint: Hsla| {
                let block = this.cache.shape(
                    window,
                    text,
                    role.font_size() * scale,
                    role.font_weight(),
                    tint,
                    Some(text_width * scale),
                    Some(max_lines),
                );
                if let Some(block) = block {
                    entries.push(Entry { block, role });
                }
            };
        push(
            self,
            &text.title,
            TextRole::Title,
            TITLE_MAX_LINES,
            title_tint,
        );
        if let Some(technology) = &text.technology {
            push(
                self,
                technology,
                TextRole::Technology,
                TECHNOLOGY_MAX_LINES,
                technology_tint,
            );
        }
        if let Some(description) = &text.description {
            push(
                self,
                description,
                TextRole::Description,
                DESCRIPTION_MAX_LINES,
                secondary,
            );
        }

        let gap = if entries.len() == 3 {
            CONTENT_GAP_DENSE
        } else {
            CONTENT_GAP
        } * scale;
        let heights: Vec<f32> = entries
            .iter()
            .map(|entry| Self::block_height(&entry.block, entry.role.line_height() * scale))
            .collect();
        let total = heights.iter().sum::<f32>() + gap * entries.len().saturating_sub(1) as f32;
        let available_top = f32::from(bounds.origin.y) + top * scale;
        let available = f32::from(bounds.size.height) - (top + bottom) * scale;
        let mut y = available_top + ((available - total) / 2.0).max(0.0);
        let x = f32::from(bounds.origin.x) + left * scale;
        for (entry, height) in entries.iter().zip(heights) {
            Self::paint_block(
                &entry.block,
                x,
                y,
                text_width * scale,
                entry.role.line_height() * scale,
                TextAlign::Center,
                window,
                cx,
            );
            y += height + gap;
        }
    }

    /// Zed's focus ring around the element whose details are open.
    fn paint_selection(&mut self, rect: Rect, window: &mut Window) {
        let scale = self.scale();
        let gap = theme::MARK_GAP * scale + mark_width(scale) + 2.0;
        window.paint_quad(gpui::quad(
            self.transform.bounds(&rect).dilate(px(gap)),
            Corners::all(px(theme::NODE_RADIUS * scale + gap)),
            gpui::transparent_black(),
            px(theme::SELECTION_WIDTH),
            self.tint_color(self.colors.selection_ring),
            BorderStyle::Solid,
        ));
    }

    fn paint_person_icon(&mut self, rect: &Rect, window: &mut Window) {
        let origin = layout::Point {
            x: rect.x + rect.w - theme::PERSON_ICON_WIDTH - theme::PERSON_ICON_RIGHT_MARGIN,
            y: rect.y + rect.h - theme::PERSON_ICON_HEIGHT,
        };
        let at = |x: f32, y: f32| {
            self.transform.point(layout::Point {
                x: origin.x + x,
                y: origin.y + y,
            })
        };
        let mut builder = PathBuilder::fill();
        builder.move_to(at(57.9197, 0.0));
        let curves: [[f32; 6]; 10] = [
            [10.9124, 0.0, 33.5766, 54.75, 33.5766, 54.75],
            [38.6131, 62.25, 45.3285, 60.75, 45.3285, 66.0],
            [45.3285, 70.5, 39.4526, 72.0, 33.5766, 72.75],
            [24.3431, 72.75, 15.9489, 71.25, 7.55474, 84.75],
            [2.51825, 93.0, 0.0, 120.0, 0.0, 120.0],
            [115.0, 120.0, 112.482, 93.0, 108.285, 84.75],
            [99.8905, 70.5, 91.4963, 72.75, 82.2628, 72.0],
            [76.3869, 71.25, 70.5109, 69.75, 70.5109, 65.25],
            [70.5109, 60.75, 77.2263, 62.25, 82.2628, 54.0],
            [82.2628, 54.75, 104.927, 0.0, 57.9197, 0.0],
        ];
        for (index, curve) in curves.iter().enumerate() {
            if index == 5 {
                builder.line_to(at(115.0, 120.0));
            }
            builder.cubic_bezier_to(
                at(curve[4], curve[5]),
                at(curve[0], curve[1]),
                at(curve[2], curve[3]),
            );
        }
        builder.close();
        let tint = self.tint_color(self.colors.node_detail);
        match builder.build() {
            Ok(path) => window.paint_path(path, tint),
            Err(error) => log::error!("strata: could not tessellate person icon: {error}"),
        }
    }

    fn paint_cylinder(&mut self, rect: &Rect, window: &mut Window) {
        let scale = self.scale();
        let radius_x = (rect.w / 2.0).round();
        let radius_y = (theme::CYLINDER_TILT * radius_x).round();
        let at = |x: f32, y: f32| {
            self.transform.point(layout::Point {
                x: rect.x + x,
                y: rect.y + y,
            })
        };
        let radii = gpui::point(px(radius_x * scale), px(radius_y * scale));
        let body_height = rect.h - 2.0 * radius_y;

        let build_body = |builder: &mut PathBuilder| {
            builder.move_to(at(rect.w, radius_y));
            builder.arc_to(radii, px(0.0), false, false, at(0.0, radius_y));
            builder.line_to(at(0.0, radius_y + body_height));
            builder.arc_to(
                radii,
                px(0.0),
                false,
                false,
                at(rect.w, radius_y + body_height),
            );
            builder.close();
        };
        let mut fill = PathBuilder::fill();
        build_body(&mut fill);
        match fill.build() {
            Ok(path) => window.paint_path(path, self.tint_color(self.colors.node_fill)),
            Err(error) => log::error!("strata: could not tessellate cylinder: {error}"),
        }
        let mut outline = PathBuilder::stroke(px(theme::CYLINDER_STROKE_WIDTH * scale));
        build_body(&mut outline);
        match outline.build() {
            Ok(path) => window.paint_path(path, self.tint_color(self.colors.node_border)),
            Err(error) => log::error!("strata: could not tessellate cylinder outline: {error}"),
        }

        let cap_radii = gpui::point(px((radius_x - 0.75) * scale), px(radius_y * scale));
        let build_cap = |builder: &mut PathBuilder| {
            builder.move_to(at(radius_x - (radius_x - 0.75), radius_y));
            builder.arc_to(
                cap_radii,
                px(0.0),
                false,
                true,
                at(radius_x + (radius_x - 0.75), radius_y),
            );
            builder.arc_to(
                cap_radii,
                px(0.0),
                false,
                true,
                at(radius_x - (radius_x - 0.75), radius_y),
            );
            builder.close();
        };
        let mut cap = PathBuilder::fill();
        build_cap(&mut cap);
        let tint = self.tint_color(self.colors.node_detail);
        match cap.build() {
            Ok(path) => window.paint_path(path, tint),
            Err(error) => log::error!("strata: could not tessellate cylinder cap: {error}"),
        }
        let mut cap_outline = PathBuilder::stroke(px(theme::CYLINDER_STROKE_WIDTH * scale));
        build_cap(&mut cap_outline);
        match cap_outline.build() {
            Ok(path) => window.paint_path(path, self.tint_color(self.colors.node_border)),
            Err(error) => log::error!("strata: could not tessellate cylinder cap outline: {error}"),
        }
    }
}

/// LikeC4's dim transition, `cubic-bezier(.5, 0, .2, 1)`.
pub const FOCUS_EASING: CubicBezier = CubicBezier::new(0.5, 0.0, 0.2, 1.0);

/// Whether `ancestor` is on `element`'s parent chain in `step`.
pub fn has_ancestor(step: &Step, element: &Element, ancestor: &str) -> bool {
    let mut current = element.parent.as_deref();
    let mut remaining = step.elements.len();
    while let Some(parent_id) = current {
        if parent_id == ancestor {
            return true;
        }
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        current = step
            .elements
            .iter()
            .find(|candidate| candidate.id == parent_id)
            .and_then(|candidate| candidate.parent.as_deref());
    }
    false
}

/// A `dash, gap` pattern shifted `offset` along the path. GPUI's dashes always
/// start at offset 0, so the shift is written into the pattern instead:
/// `[dash - o, gap, o, 0]` repeats as the original pattern moved by `o`.
fn flowing_dash(dash: f32, gap: f32, offset: f32) -> [f32; 4] {
    let offset = offset.rem_euclid(dash + gap);
    if offset < dash {
        [dash - offset, gap, offset, 0.0]
    } else {
        let into_gap = offset - dash;
        [0.0, gap - into_gap, dash, into_gap]
    }
}

/// What clicking an element lights up, as LikeC4's `focusNodesEdges` does.
pub struct Focus {
    /// The element, its descendants, and everything connected to them.
    pub lit: HashSet<String>,
    /// Relationships touching the element or its descendants, as `(from, to)`.
    pub edges: HashSet<(String, String)>,
    /// 0 when focus starts, 1 once everything else has faded.
    pub fade: f32,
    /// How far the connected relationships' dashes have flowed.
    pub flow: f32,
}

impl Focus {
    fn dimmed(&self, alpha: f32) -> (f32, f32) {
        (
            alpha * (1.0 - (1.0 - theme::FOCUS_DIM_OPACITY) * self.fade),
            1.0 - theme::FOCUS_GRAYSCALE * self.fade,
        )
    }

    /// A lit item shows at full strength, even if unchanged in this step.
    fn element_look(&self, id: &str, look: &Look) -> (f32, f32) {
        if self.lit.contains(id) {
            (if look.ghost { look.alpha } else { 1.0 }, 1.0)
        } else {
            self.dimmed(look.alpha)
        }
    }
}

/// What the click selected: its focus, and the rect to ring.
pub struct Highlight<'a> {
    pub focus: Option<&'a Focus>,
    pub selected: Option<Rect>,
}

pub fn paint_scene(
    scene: &Scene,
    highlight: Highlight,
    view: ViewState,
    bounds: Bounds<Pixels>,
    cache: &Rc<RefCell<TextCache>>,
    font: &Font,
    colors: DiagramColors,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let mut cache = cache.borrow_mut();
    cache.prepare(view.scale, font);
    let mut painter = Painter {
        transform: Transform {
            origin: bounds.origin,
            offset: view.offset,
            scale: view.scale,
        },
        viewport: bounds,
        cache: &mut cache,
        colors,
        alpha: 1.0,
        saturation: 1.0,
        flow: None,
    };
    let Highlight { focus, selected } = highlight;
    let element = |id: &str, look: &Look| match focus {
        Some(focus) => focus.element_look(id, look),
        None => (look.alpha, 1.0),
    };
    let relationship = |edge: &EdgeItem| -> ((f32, f32), Option<f32>) {
        match focus {
            Some(focus)
                if focus
                    .edges
                    .contains(&(edge.from.to_string(), edge.to.to_string())) =>
            {
                (
                    (
                        if edge.look.ghost {
                            edge.look.alpha
                        } else {
                            1.0
                        },
                        1.0,
                    ),
                    Some(focus.flow),
                )
            }
            Some(focus) => (focus.dimmed(edge.look.alpha), None),
            None => ((edge.look.alpha, 1.0), None),
        }
    };
    for boundary in &scene.boundaries {
        let (alpha, saturation) = element(&boundary.id, &boundary.look);
        painter.set_item(alpha, saturation);
        painter.paint_boundary(boundary, window, cx);
    }
    for edge in &scene.edges {
        let ((alpha, saturation), flow) = relationship(edge);
        painter.set_item(alpha, saturation);
        painter.flow = flow;
        painter.paint_edge(edge, window);
    }
    painter.flow = None;
    for edge in &scene.edges {
        if let Some(label) = edge.label.as_ref().filter(|_| edge.drawn >= 1.0) {
            let ((alpha, saturation), _) = relationship(edge);
            painter.set_item(alpha, saturation);
            painter.paint_label(label, window, cx);
        }
    }
    for node in &scene.nodes {
        let (alpha, saturation) = element(&node.id, &node.look);
        painter.set_item(alpha, saturation);
        painter.paint_node(node, window, cx);
    }
    if let Some(selected) = selected {
        painter.set_item(1.0, 1.0);
        painter.paint_selection(selected, window);
    }
}

#[cfg(test)]
mod tests {
    use super::flowing_dash;

    /// Whether position `at` along a path is inside a dash of `pattern`.
    fn on(pattern: &[f32], at: f32) -> bool {
        let period: f32 = pattern.iter().sum();
        let mut position = at.rem_euclid(period);
        for (index, length) in pattern.iter().enumerate() {
            if position < *length {
                return index % 2 == 0;
            }
            position -= length;
        }
        false
    }

    #[test]
    fn a_flowing_dash_is_the_pattern_moved_by_the_offset() {
        let (dash, gap) = (8.0, 10.0);
        for offset in [0.0, 3.0, 7.5, 8.0, 12.0, 17.9, 18.0, 30.0, -4.0] {
            let pattern = flowing_dash(dash, gap, offset);
            assert!(
                pattern.iter().all(|length| *length >= 0.0),
                "{offset}: {pattern:?}"
            );
            assert!((pattern.iter().sum::<f32>() - (dash + gap)).abs() < 1e-4);
            for step in 0..72 {
                let at = step as f32 * 0.5 + 0.25;
                assert_eq!(
                    on(&pattern, at),
                    on(&[dash, gap], at + offset),
                    "offset {offset} at {at}"
                );
            }
        }
    }
}
