//! The diagram's look. Shapes, sizes and timings are LikeC4 1.61.1's (MIT,
//! `@likec4/diagram` styles); colors come from the active Zed theme.

use gpui::{App, Hsla};
use theme::ActiveTheme;

use crate::scene::Mark;

/// Every color comes from the active Zed theme, so the diagram reads like
/// the rest of the editor in light and dark themes alike. The shapes,
/// sizes and timings below keep LikeC4's look.
#[derive(Clone, Copy)]
pub struct DiagramColors {
    pub accent: Hsla,
    pub node_fill: Hsla,
    pub node_border: Hsla,
    /// The person silhouette and the cylinder cap.
    pub node_detail: Hsla,
    pub node_title: Hsla,
    pub node_secondary: Hsla,
    pub boundary_fill: Hsla,
    pub boundary_border: Hsla,
    pub boundary_title: Hsla,
    pub edge: Hsla,
    pub label_background: Hsla,
    pub label_text: Hsla,
    pub shadow: Hsla,
    pub created: Hsla,
    pub modified: Hsla,
    pub deleted: Hsla,
    pub selection_ring: Hsla,
}

impl DiagramColors {
    pub fn from_theme(cx: &App) -> Self {
        let theme = cx.theme();
        let colors = theme.colors();
        let status = theme.status();
        let accent = theme.players().local().cursor;
        let canvas = colors.editor_background;
        let node_fill = canvas.blend(accent.opacity(NODE_ACCENT_SHARE));
        Self {
            accent,
            node_fill,
            node_border: accent.opacity(NODE_BORDER_OPACITY),
            node_detail: node_fill.blend(accent.opacity(NODE_DETAIL_OPACITY)),
            node_title: colors.text,
            node_secondary: colors.text_muted,
            boundary_fill: accent.opacity(BOUNDARY_FILL_OPACITY),
            boundary_border: accent.opacity(BOUNDARY_BORDER_OPACITY),
            boundary_title: colors.text_accent,
            edge: colors.text_muted,
            label_background: canvas.opacity(LABEL_BACKGROUND_OPACITY),
            label_text: colors.text,
            shadow: gpui::black(),
            created: status.created,
            modified: status.modified,
            deleted: status.deleted,
            selection_ring: colors.border_focused,
        }
    }

    pub fn mark(&self, mark: Mark) -> Hsla {
        match mark {
            Mark::Added => self.created,
            Mark::Changed => self.modified,
            Mark::Removed => self.deleted,
        }
    }
}

/// How much of the accent tints an element over the canvas.
const NODE_ACCENT_SHARE: f32 = 0.22;
const NODE_BORDER_OPACITY: f32 = 0.6;
const NODE_DETAIL_OPACITY: f32 = 0.45;
/// Nested boundaries are translucent, so each level reads a little stronger.
const BOUNDARY_FILL_OPACITY: f32 = 0.05;
const BOUNDARY_BORDER_OPACITY: f32 = 0.4;
const LABEL_BACKGROUND_OPACITY: f32 = 0.85;
pub const NODE_BORDER_WIDTH: f32 = 1.0;

/// Compounds are drawn translucent: the default `opacity: 15` of the fill,
/// and a border of `50 + 50 * opacity` percent.
pub const COMPOUND_BORDER_WIDTH: f32 = 3.0;
pub const COMPOUND_RADIUS: f32 = 6.0;
pub const COMPOUND_TITLE_SIZE: f32 = 15.0;
pub const COMPOUND_TITLE_LEFT: f32 = 10.0;
pub const COMPOUND_TITLE_TOP: f32 = 2.0;
pub const COMPOUND_TITLE_MIN_HEIGHT: f32 = 28.0;

pub const NODE_RADIUS: f32 = 6.0;
/// Element technology text is drawn at this opacity.
pub const TECHNOLOGY_OPACITY: f32 = 0.92;
/// Element shape drop shadows (CSS `drop-shadow` stack), as
/// `(offset_y, blur, alpha, tinted with the accent)`.
pub const NODE_SHADOWS: [(f32, f32, f32, bool); 3] = [
    (2.0, 1.0, 0.212, false),
    (1.0, 1.0, 0.4, true),
    (5.0, 3.0, 0.102, false),
];
/// Cylinder cap tilt, from `cylinderSVGPath`.
pub const CYLINDER_TILT: f32 = 0.07;
pub const CYLINDER_STROKE_WIDTH: f32 = 2.0;

/// Person icon, 115 x 120 (`PersonIcon` in LikeC4's `ElementShape`).
pub const PERSON_ICON_WIDTH: f32 = 115.0;
pub const PERSON_ICON_HEIGHT: f32 = 120.0;
pub const PERSON_ICON_RIGHT_MARGIN: f32 = 6.0;

pub const EDGE_WIDTH: f32 = 3.0;
pub const EDGE_DASH: [f32; 2] = [8.0, 5.0];

/// Focus on a clicked element, as LikeC4 does (`focusNodesEdges`, and the
/// `data-likec4-dimmed` and `xyedgeAnimated` rules in `@likec4/diagram`'s
/// `styles.css`): everything not connected fades to 25% and 85% grayscale
/// over 0.4 s, and connected relationships widen to 4 px and flow, a
/// `8,10` dash moving 36 px every 0.8 s from source to target.
pub const FOCUS_DIM_OPACITY: f32 = 0.25;
pub const FOCUS_GRAYSCALE: f32 = 0.85;
pub const FOCUS_FADE_SECONDS: f32 = 0.4;
pub const ACTIVE_EDGE_WIDTH: f32 = 4.0;
pub const FLOW_DASH: [f32; 2] = [8.0, 10.0];
pub const FLOW_DISTANCE: f32 = 36.0;
pub const FLOW_SECONDS: f32 = 0.8;
pub const EDGE_LABEL_TECHNOLOGY_OPACITY: f32 = 0.8;
pub const EDGE_LABEL_RADIUS: f32 = 2.0;

pub const MARK_WIDTH: f32 = 3.0;
/// Marks stay visible when zoomed far out.
pub const MARK_MIN_SCREEN_WIDTH: f32 = 2.0;
/// Space between an element and its mark ring.
pub const MARK_GAP: f32 = 3.0;
/// Ghosts are tinted like Zed's `created.background` (10% of the color).
pub const GHOST_FILL_OPACITY: f32 = 0.1;
pub const SELECTION_WIDTH: f32 = 2.0;
