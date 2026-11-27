//! The LikeC4 dark theme. Values are from LikeC4 1.61.1 (MIT): colors and
//! relationship colors from `@likec4/core` (`styles` chunk), compound colors
//! from `LikeC4Styles.colorsForCompounds(primary, 5)`, canvas and layout
//! constants from `@likec4/diagram` styles. See `docs/likec4-parity.md`.

pub const CANVAS: u32 = 0x242424;

pub struct Palette {
    pub fill: u32,
    pub stroke: u32,
    pub hi_contrast: u32,
    pub lo_contrast: u32,
}

/// LikeC4's default element color (`primary`, a.k.a. `blue`). Every element
/// kind uses it, as in LikeC4's own defaults.
pub const PRIMARY: Palette = Palette {
    fill: 0x3b82f6,
    stroke: 0x2563eb,
    hi_contrast: 0xeff6ff,
    lo_contrast: 0xbfdbfe,
};

pub fn palette_for(_kind: strata_spec::ElementKind) -> &'static Palette {
    &PRIMARY
}

/// `colorsForCompounds(primary, 5)`: fill and stroke by compound depth 1..5.
pub const COMPOUND_COLORS: [(u32, u32); 5] = [
    (0x1a71e1, 0x0e53d5),
    (0x206ad5, 0x164dca),
    (0x2362ca, 0x1b46c0),
    (0x265bbe, 0x1e40b6),
    (0x2753b3, 0x203aac),
];

/// Compounds are drawn translucent: the default `opacity: 15` of the fill,
/// and a border of `50 + 50 * opacity` percent.
pub const COMPOUND_OPACITY: f32 = 0.15;
pub const COMPOUND_BORDER_OPACITY: f32 = 0.575;
pub const COMPOUND_BORDER_WIDTH: f32 = 3.0;
pub const COMPOUND_RADIUS: f32 = 6.0;
pub const COMPOUND_TITLE_COLOR: u32 = 0xa8d9ff;
pub const COMPOUND_TITLE_SIZE: f32 = 15.0;
pub const COMPOUND_TITLE_LEFT: f32 = 10.0;
pub const COMPOUND_TITLE_TOP: f32 = 2.0;
pub const COMPOUND_TITLE_MIN_HEIGHT: f32 = 28.0;

pub const NODE_RADIUS: f32 = 6.0;
/// Element technology text is drawn at this opacity.
pub const TECHNOLOGY_OPACITY: f32 = 0.92;
/// Element shape drop shadows (CSS `drop-shadow` stack), as
/// `(offset_y, blur, color, alpha)`; the second is tinted with the stroke.
pub const NODE_SHADOWS: [(f32, f32, u32, f32); 3] = [
    (2.0, 1.0, 0x000000, 0.212),
    (1.0, 1.0, 0x2563eb, 0.4),
    (5.0, 3.0, 0x000000, 0.102),
];

/// The person silhouette and cylinder cap use `color-mix(stroke 80%, fill)`.
pub const SHAPE_MIX_STROKE: f32 = 0.8;
/// Cylinder cap tilt, from `cylinderSVGPath`.
pub const CYLINDER_TILT: f32 = 0.07;
pub const CYLINDER_STROKE_WIDTH: f32 = 2.0;

/// Person icon, 115 x 120 (`PersonIcon` in LikeC4's `ElementShape`).
pub const PERSON_ICON_WIDTH: f32 = 115.0;
pub const PERSON_ICON_HEIGHT: f32 = 120.0;
pub const PERSON_ICON_RIGHT_MARGIN: f32 = 6.0;

pub const EDGE_COLOR: u32 = 0x949494;
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
pub const EDGE_LABEL_BG: u32 = 0x18191b;
pub const EDGE_LABEL_BG_OPACITY: f32 = 0.65;
pub const EDGE_LABEL_TEXT: u32 = 0xe8e8e8;
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
