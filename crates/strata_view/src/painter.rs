use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    BorderStyle, Bounds, BoxShadow, Corners, Font, FontWeight, Hsla, PathBuilder, Pixels,
    SharedString, TextAlign, TextRun, TransformationMatrix, Window, WindowTextSystem, WrappedLine,
    px, size,
};
use strata_core::easing::CubicBezier;
use strata_core::layout::{self, EstimatedText, Rect, TextMeasure, TextRole};
use strata_core::timeline::partial_path;
use strata_spec::{Element, ElementKind, Step};
use ui::IconName;

use crate::diagram_theme::{self as theme, DiagramColors};
use crate::scene::{BoundaryItem, EdgeItem, LabelItem, Look, Mark, NodeItem, NodeText, Scene};

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

#[derive(Clone, Copy, PartialEq, Debug)]
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
        let font = self.font.clone()?;
        let shape = |text: &SharedString| {
            window
                .text_system()
                .shape_text(
                    text.clone(),
                    px(font_size),
                    &[run_for(text, &font, weight, tint)],
                    wrap_width.map(px),
                    None,
                )
                .map_err(|error| log::error!("strata: could not shape text: {error}"))
                .ok()
                .map(|lines| lines.into_iter().collect::<Vec<WrappedLine>>())
        };
        let visual_lines = |lines: &[WrappedLine]| {
            lines
                .iter()
                .map(|line| line.wrap_boundaries().len() + 1)
                .sum::<usize>()
        };
        let mut lines = shape(text)?;
        // GPUI's line clamp cuts a paragraph without wrapping what is left,
        // so text over its line limit is shortened here, with an ellipsis,
        // to the longest prefix that wraps within the limit.
        if let Some(limit) = max_lines
            && visual_lines(&lines) > limit
        {
            let characters: Vec<char> = text.chars().collect();
            let (mut fits, mut too_long) = (0, characters.len());
            let mut best = None;
            while too_long - fits > 1 {
                let middle = (fits + too_long) / 2;
                let prefix: String = characters.iter().take(middle).collect();
                let candidate = SharedString::from(format!("{}\u{2026}", prefix.trim_end()));
                match shape(&candidate) {
                    Some(shaped) if visual_lines(&shaped) <= limit => {
                        fits = middle;
                        best = Some(shaped);
                    }
                    _ => too_long = middle,
                }
            }
            lines = best.unwrap_or_default();
        }
        let block = Rc::new(ShapedBlock { lines });
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
    /// Whether the element being painted is under the mouse.
    hovered: bool,
    /// The scene's own opacity, applied on top of each item's.
    opacity: f32,
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
        self.alpha = quantize(alpha * self.opacity) / 64.0;
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
                (theme::COMPOUND_BORDER_WIDTH * scale).max(1.0),
                BorderStyle::Solid,
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
        // Titles stay legible when zoomed out, like labels on a map, but no
        // larger than the title strip the layout keeps for them.
        let size_px = theme::COMPOUND_TITLE_SIZE.max(
            (theme::COMPOUND_TITLE_MIN_SCREEN_SIZE / scale).min(theme::COMPOUND_TITLE_MAX_SIZE),
        );
        let title_origin_y = theme::COMPOUND_TITLE_TOP
            + ((theme::COMPOUND_TITLE_MIN_HEIGHT - size_px) / 2.0).max(0.0);
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
                f32::from(bounds.origin.x)
                    + (theme::COMPOUND_TITLE_LEFT * scale).max(theme::COMPOUND_TITLE_MIN_INSET),
                f32::from(bounds.origin.y)
                    + (title_origin_y * scale).max(theme::COMPOUND_TITLE_MIN_INSET),
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
        } * scale)
        .max(px(1.0));
        let edge_color = match (item.look.mark, self.flow) {
            (Some(mark), _) => self.tint_color(self.colors.mark(mark)),
            (None, Some(_)) => self.tint_color(self.colors.accent),
            (None, None) => self.tint_color(self.colors.edge),
        };
        let arrow = (item.drawn >= 1.0).then(|| arrow_corners(item)).flatten();
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
        // The layout leaves room for a larger arrowhead; the line runs on to
        // meet the smaller one.
        if let Some([_, base, _, _]) = arrow {
            builder.line_to(self.transform.point(base));
        }
        match builder.build() {
            Ok(path) => window.paint_path(path, edge_color),
            Err(error) => log::error!("strata: could not tessellate edge: {error}"),
        }

        // The arrowhead lands once the line has been drawn all the way.
        let Some([tip, _, left, right]) = arrow else {
            return;
        };
        let corners = [tip, left, right];
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
            // A knockout in the canvas color, so it hides lines crossing the
            // label however faded the label's own relationship is.
            self.colors
                .label_background
                .opacity(self.opacity.clamp(0.0, 1.0)),
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
        if let Some(mark) = mark.filter(|_| item.look.glow > 0.0) {
            window.paint_drop_shadows(
                bounds,
                radii,
                &[BoxShadow {
                    color: mark.opacity(theme::LANDING_GLOW_OPACITY * item.look.glow),
                    offset: gpui::point(px(0.0), px(0.0)),
                    blur_radius: px(theme::LANDING_GLOW_BLUR * scale.max(0.5)),
                    spread_radius: px(theme::LANDING_GLOW_SPREAD * scale.max(0.5)),
                    inset: false,
                }],
            );
        }

        match item.kind {
            ElementKind::Store => self.paint_cylinder(&item.rect, window),
            ElementKind::Person
            | ElementKind::System
            | ElementKind::Container
            | ElementKind::Component => {
                let border = if self.hovered {
                    self.colors.node_border_hover
                } else {
                    self.colors.node_border
                };
                window.paint_quad(gpui::quad(
                    bounds,
                    radii,
                    self.tint_color(self.colors.node_fill),
                    px(theme::NODE_BORDER_WIDTH),
                    self.tint_color(border),
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

    /// A Zed card: a header with the element kind's icon and name and, for
    /// a change, a status badge; then the title, technology and description,
    /// left-aligned. Without room for the header (a tall description), the
    /// text is drawn on its own.
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
            gap_before: f32,
        }
        let mut entries: Vec<Entry> = Vec::new();
        let shape = |this: &mut Self,
                     text: &SharedString,
                     role: TextRole,
                     max_lines: usize,
                     tint: Hsla,
                     gap_before: f32| {
            this.cache
                .shape(
                    window,
                    text,
                    role.font_size() * scale,
                    role.font_weight(),
                    tint,
                    Some(text_width * scale),
                    Some(max_lines),
                )
                .map(|block| Entry {
                    block,
                    role,
                    gap_before,
                })
        };
        entries.extend(shape(
            self,
            &text.title,
            TextRole::Title,
            TITLE_MAX_LINES,
            title_tint,
            0.0,
        ));
        if let Some(technology) = &text.technology {
            entries.extend(shape(
                self,
                technology,
                TextRole::Technology,
                TECHNOLOGY_MAX_LINES,
                technology_tint,
                theme::TECHNOLOGY_GAP,
            ));
        }
        let line_heights = |entries: &[Entry]| -> f32 {
            entries
                .iter()
                .map(|entry| {
                    Self::block_height(&entry.block, entry.role.line_height() * scale)
                        + entry.gap_before * scale
                })
                .sum()
        };
        let available = f32::from(bounds.size.height) - (top + bottom) * scale;
        let header_height = (theme::CARD_HEADER_HEIGHT + theme::CARD_HEADER_GAP) * scale;
        let tolerance = theme::CARD_FIT_TOLERANCE * scale;
        let with_header = line_heights(&entries) + header_height <= available + tolerance;
        // Shaped at a zoomed size, text can wrap onto a line more than the
        // layout measured; the description gives up lines so the card keeps
        // its header and nothing spills out of the box.
        if let Some(description) = &text.description {
            let room = available + tolerance
                - line_heights(&entries)
                - if with_header { header_height } else { 0.0 }
                - CONTENT_GAP * scale;
            let line_height = TextRole::Description.line_height() * scale;
            let lines = ((room / line_height).floor().max(0.0) as usize).min(DESCRIPTION_MAX_LINES);
            if lines > 0 {
                entries.extend(shape(
                    self,
                    description,
                    TextRole::Description,
                    lines,
                    secondary,
                    CONTENT_GAP,
                ));
            }
        }

        let heights: Vec<f32> = entries
            .iter()
            .map(|entry| Self::block_height(&entry.block, entry.role.line_height() * scale))
            .collect();
        let text_height = line_heights(&entries);
        let x = f32::from(bounds.origin.x) + left * scale;
        // Cards are sized for their longest text, so a shorter one centers
        // its content rather than leaving the space below it.
        let content_height = text_height + if with_header { header_height } else { 0.0 };
        let mut y = f32::from(bounds.origin.y)
            + top * scale
            + ((available - content_height) / 2.0).max(0.0);
        if with_header {
            self.paint_card_header(
                item,
                x,
                y,
                text_width * scale,
                secondary,
                ghost_color,
                window,
                cx,
            );
            y += header_height;
        }
        for (entry, height) in entries.iter().zip(heights) {
            y += entry.gap_before * scale;
            Self::paint_block(
                &entry.block,
                x,
                y,
                text_width * scale,
                entry.role.line_height() * scale,
                TextAlign::Left,
                window,
                cx,
            );
            y += height;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_card_header(
        &mut self,
        item: &NodeItem,
        x: f32,
        y: f32,
        width: f32,
        tint: Hsla,
        ghost_color: Option<Hsla>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let scale = self.scale();
        let height = theme::CARD_HEADER_HEIGHT * scale;
        let icon_size = theme::CARD_ICON_SIZE * scale;
        let icon_bounds = Bounds {
            origin: gpui::point(px(x), px(y + (height - icon_size) / 2.0)),
            size: size(px(icon_size), px(icon_size)),
        };
        if let Err(error) = window.paint_svg(
            icon_bounds,
            card_icon(item.kind).path().into(),
            None,
            TransformationMatrix::unit(),
            tint,
            cx,
        ) {
            log::error!("strata: could not paint an element icon: {error}");
        }
        let label_size = theme::CARD_LABEL_SIZE * scale;
        let label_x = x + icon_size + theme::CARD_ICON_GAP * scale;
        if let Some(block) = self.cache.shape(
            window,
            &SharedString::new_static(item.kind.display_name()),
            label_size,
            500.0,
            tint,
            None,
            Some(1),
        ) {
            Self::paint_block(
                &block,
                label_x,
                y + (height - label_size * 1.3) / 2.0,
                width,
                label_size * 1.3,
                TextAlign::Left,
                window,
                cx,
            );
        }

        let Some(mark) = item.look.mark.filter(|_| ghost_color.is_none()) else {
            return;
        };
        let color = self.tint_color(self.colors.mark(mark));
        let badge_size = theme::BADGE_TEXT_SIZE * scale;
        let Some(block) = self.cache.shape(
            window,
            &SharedString::new_static(mark_label(mark)),
            badge_size,
            600.0,
            color,
            None,
            Some(1),
        ) else {
            return;
        };
        let text_width = block
            .lines
            .first()
            .map_or(0.0, |line| f32::from(line.width()));
        let padding = theme::BADGE_PADDING_X * scale;
        let badge_height = theme::BADGE_HEIGHT * scale;
        let badge = Bounds {
            origin: gpui::point(
                px(x + width - text_width - 2.0 * padding),
                px(y + (height - badge_height) / 2.0),
            ),
            size: size(px(text_width + 2.0 * padding), px(badge_height)),
        };
        window.paint_quad(gpui::quad(
            badge,
            Corners::all(px(theme::BADGE_RADIUS * scale)),
            color.opacity(theme::BADGE_FILL_OPACITY),
            0.0,
            gpui::transparent_black(),
            BorderStyle::default(),
        ));
        Self::paint_block(
            &block,
            f32::from(badge.origin.x) + padding,
            f32::from(badge.origin.y) + (badge_height - badge_size * 1.3) / 2.0,
            text_width,
            badge_size * 1.3,
            TextAlign::Left,
            window,
            cx,
        );
    }

    /// A soft accent glow under the element whose details are open.
    fn paint_selection_glow(&mut self, rect: Rect, window: &mut Window) {
        let scale = self.scale();
        let bounds = self.transform.bounds(&rect);
        window.paint_drop_shadows(
            bounds,
            Corners::all(px(theme::NODE_RADIUS * scale)),
            &[BoxShadow {
                color: self.colors.accent.opacity(theme::SELECTION_GLOW_OPACITY),
                offset: gpui::point(px(0.0), px(0.0)),
                blur_radius: px(theme::SELECTION_GLOW_BLUR * scale.max(0.5)),
                spread_radius: px(theme::SELECTION_GLOW_SPREAD * scale.max(0.5)),
                inset: false,
            }],
        );
    }

    /// A dot grid, as on design canvases, so panning reads as movement.
    /// Dots thin out when zoomed out so they never turn into a texture.
    fn paint_grid(&mut self, window: &mut Window) {
        let scale = self.scale();
        let mut spacing = theme::GRID_SPACING * scale;
        while spacing < theme::GRID_MIN_SCREEN_SPACING {
            spacing *= 2.0;
        }
        let dot = theme::GRID_DOT_SIZE;
        let origin = self.transform.origin;
        let offset = self.transform.offset;
        let left = f32::from(self.viewport.origin.x);
        let top = f32::from(self.viewport.origin.y);
        let right = f32::from(self.viewport.right());
        let bottom = f32::from(self.viewport.bottom());
        let first_x = f32::from(origin.x) + offset.x.rem_euclid(spacing);
        let first_y = f32::from(origin.y) + offset.y.rem_euclid(spacing);
        let color = self.colors.grid;
        let mut y = first_y;
        while y < bottom {
            if y >= top {
                let mut x = first_x;
                while x < right {
                    if x >= left {
                        window.paint_quad(gpui::fill(
                            Bounds {
                                origin: gpui::point(px(x - dot / 2.0), px(y - dot / 2.0)),
                                size: size(px(dot), px(dot)),
                            },
                            color,
                        ));
                    }
                    x += spacing;
                }
            }
            y += spacing;
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
pub fn card_icon(kind: ElementKind) -> IconName {
    match kind {
        ElementKind::Person => IconName::Person,
        ElementKind::System => IconName::Server,
        ElementKind::Container => IconName::Box,
        ElementKind::Component => IconName::Blocks,
        ElementKind::Store => IconName::DatabaseZap,
    }
}

fn mark_label(mark: Mark) -> &'static str {
    match mark {
        Mark::Added => "Added",
        Mark::Changed => "Changed",
        Mark::Removed => "Removed",
    }
}

/// The tip, the base where the line meets the head, and the two back corners
/// of a relationship's arrowhead.
fn arrow_corners(item: &EdgeItem) -> Option<[layout::Point; 4]> {
    let tip = item.tip;
    let last = *item.points.last()?;
    let (dx, dy) = (tip.x - last.x, tip.y - last.y);
    let length = dx.hypot(dy);
    if length < 0.01 {
        return None;
    }
    let direction = (dx / length, dy / length);
    let arrow_length = theme::ARROW_LENGTH.min(length);
    let base = layout::Point {
        x: tip.x - direction.0 * arrow_length,
        y: tip.y - direction.1 * arrow_length,
    };
    let normal = (-direction.1, direction.0);
    let half_width = theme::ARROW_HALF_WIDTH * arrow_length / theme::ARROW_LENGTH;
    Some([
        tip,
        base,
        layout::Point {
            x: base.x + normal.0 * half_width,
            y: base.y + normal.1 * half_width,
        },
        layout::Point {
            x: base.x - normal.0 * half_width,
            y: base.y - normal.1 * half_width,
        },
    ])
}

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

/// Unchanged items step back to the timeline's dim opacity, which on Zed's
/// low-contrast cards reads as gone rather than as context; this curve keeps
/// them legible (0.3 shows at about 0.5) while marked items stay as they are.
fn present(look: &Look) -> Look {
    if look.mark.is_some() || look.ghost {
        return *look;
    }
    Look {
        alpha: look.alpha.clamp(0.0, 1.0).powf(theme::UNCHANGED_PRESENCE),
        ..*look
    }
}

/// What the click selected: its focus, and the rect to ring.
pub struct Highlight<'a> {
    pub focus: Option<&'a Focus>,
    pub selected: Option<Rect>,
    pub hovered: Option<&'a str>,
    /// The whole scene's opacity, for fading one step's picture into the next.
    pub opacity: f32,
    /// Whether to paint the dot grid under this scene.
    pub grid: bool,
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
        hovered: false,
        opacity: highlight.opacity,
    };
    let Highlight {
        focus,
        selected,
        hovered,
        grid,
        ..
    } = highlight;
    if grid {
        painter.paint_grid(window);
    }
    let element = |id: &str, look: &Look| {
        let look = present(look);
        match focus {
            Some(focus) => focus.element_look(id, &look),
            None => (look.alpha, 1.0),
        }
    };
    let relationship = |edge: &EdgeItem| -> ((f32, f32), Option<f32>) {
        let look = present(&edge.look);
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
            Some(focus) => (focus.dimmed(look.alpha), None),
            None => ((look.alpha, 1.0), None),
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
    // Only cards glow: under a translucent boundary a glow would fill it.
    if let Some(selected) =
        selected.filter(|rect| scene.nodes.iter().any(|node| node.rect == *rect))
    {
        painter.set_item(1.0, 1.0);
        painter.paint_selection_glow(selected, window);
    }
    for node in &scene.nodes {
        let (alpha, saturation) = element(&node.id, &node.look);
        painter.set_item(alpha, saturation);
        painter.hovered = hovered == Some(node.id.as_ref());
        painter.paint_node(node, window, cx);
    }
    painter.hovered = false;
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
