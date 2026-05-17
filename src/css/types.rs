//! CSS types: parser AST plus the computed-style struct that layout/paint
//! will read from.

use std::collections::HashMap;

/// Column-major 2D affine `(sx, kx, ky, sy, tx, ty)`. Maps the point
/// `(x, y)` to `(sx·x + ky·y + tx, kx·x + sy·y + ty)`. Mirrors the layout
/// of `tiny_skia::Transform` so converting at paint time is trivial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    pub sx: f32,
    pub kx: f32,
    pub ky: f32,
    pub sy: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Transform2D {
    pub const IDENTITY: Transform2D = Transform2D {
        sx: 1.0,
        kx: 0.0,
        ky: 0.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn translate(tx: f32, ty: f32) -> Self {
        Self {
            sx: 1.0,
            kx: 0.0,
            ky: 0.0,
            sy: 1.0,
            tx,
            ty,
        }
    }

    pub fn scale(sx: f32, sy: f32) -> Self {
        Self {
            sx,
            kx: 0.0,
            ky: 0.0,
            sy,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// `angle` is in radians.
    pub fn rotate(angle: f32) -> Self {
        let (s, c) = angle.sin_cos();
        Self {
            sx: c,
            kx: s,
            ky: -s,
            sy: c,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// Skew in radians (CSS uses `skewX(angle)` / `skewY(angle)`).
    pub fn skew(ax: f32, ay: f32) -> Self {
        Self {
            sx: 1.0,
            kx: ay.tan(),
            ky: ax.tan(),
            sy: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// `self ∘ other` — apply `other` first, then `self`.
    pub fn then(&self, other: &Self) -> Self {
        Self {
            sx: self.sx * other.sx + self.ky * other.kx,
            kx: self.kx * other.sx + self.sy * other.kx,
            ky: self.sx * other.ky + self.ky * other.sy,
            sy: self.kx * other.ky + self.sy * other.sy,
            tx: self.sx * other.tx + self.ky * other.ty + self.tx,
            ty: self.kx * other.tx + self.sy * other.ty + self.ty,
        }
    }

    /// True if the transform is a pure translation (no rotation, scale,
    /// or skew). Lets paint take the fast offset-only path.
    pub fn is_pure_translate(&self) -> bool {
        (self.sx - 1.0).abs() < 1e-5
            && (self.sy - 1.0).abs() < 1e-5
            && self.kx.abs() < 1e-5
            && self.ky.abs() < 1e-5
    }
}

impl Default for Transform2D {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Snapshot of the environment that `@media` queries are evaluated
/// against. Built from the current page viewport at cascade time.
#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    /// `prefers-color-scheme`. Always `"light"` until we add a settings
    /// surface that lets the user pick.
    pub color_scheme: &'static str,
}

impl Viewport {
    pub const DEFAULT: Viewport = Viewport {
        width: 1024.0,
        height: 768.0,
        color_scheme: "light",
    };

    pub fn from_size(width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            color_scheme: "light",
        }
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
    /// `@media` blocks. Cascade evaluates each block's query against the
    /// viewport / device-state at compute time; matching blocks
    /// contribute their `rules` to the normal cascade.
    pub media_blocks: Vec<MediaBlock>,
    /// `@font-face` declarations. The browser shell walks these and
    /// downloads their `src:` URLs through the SSRF-guarded client,
    /// then registers them with the text shaping system.
    pub font_faces: Vec<FontFace>,
    /// `@keyframes` rules. Stored verbatim; animation playback is a
    /// later phase but we keep them out of "silently dropped" territory.
    pub keyframes: Vec<KeyframesAnim>,
}

#[derive(Debug, Clone)]
pub struct MediaBlock {
    pub query: MediaQuery,
    pub rules: Vec<Rule>,
}

/// A parsed `@media` query. We support comma-separated alternatives
/// (any one matching is enough) and `and`-joined conditions inside each
/// alternative.
#[derive(Debug, Clone, Default)]
pub struct MediaQuery {
    pub alternatives: Vec<Vec<MediaCondition>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields are read by `evaluate_media_query` in the next commit
pub enum MediaCondition {
    /// `screen`, `print`, `all` — anything else is treated as non-matching.
    MediaType(String),
    MinWidth(f32),
    MaxWidth(f32),
    MinHeight(f32),
    MaxHeight(f32),
    Orientation(String),
    PrefersColorScheme(String),
    /// Bare `(width: 360px)` exact match — rare in practice but valid.
    ExactWidth(f32),
    /// Anything we don't recognise (`(any-pointer: fine)`, calc(), etc.).
    /// Treated as non-matching so we don't accidentally apply mobile
    /// styles desktop-wide. Carries the raw text for debug logs.
    Unsupported(String),
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // weight/style consumed once @font-face wiring is finished
pub struct FontFace {
    pub family: String,
    pub sources: Vec<FontSource>,
    pub weight: Option<u16>,
    pub style: Option<FontStyle>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // format hint consumed when we pick from src: alternates
pub enum FontSource {
    Url(String, Option<String>),
    Local(String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // steps consumed once animations land
pub struct KeyframesAnim {
    pub name: String,
    pub steps: Vec<KeyframeStep>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct KeyframeStep {
    /// `0.0` for `from`, `1.0` for `to`, `n%` → `n / 100`.
    pub offset: f32,
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub declarations: Vec<Declaration>,
}

/// `compounds` left-to-right (ancestor → target); `combinators[i]` joins
/// `compounds[i]` to `compounds[i+1]`. `pseudo_element` (if any) applies to
/// the rightmost compound and only matches generated content in layout.
#[derive(Debug, Clone)]
pub struct Selector {
    pub compounds: Vec<SimpleSelector>,
    pub combinators: Vec<Combinator>,
    pub pseudo_element: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SimpleSelector {
    pub tag: Option<String>,
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attributes: Vec<AttributeSelector>,
    pub pseudo_classes: Vec<PseudoClass>,
}

#[derive(Debug, Clone)]
pub enum PseudoClass {
    /// Bare pseudo-classes: `:hover`, `:focus`, `:first-child`, etc.
    /// Lowercased name.
    Name(String),
    /// `:not(...)` — element matches when *no* inner selector does.
    Not(Vec<Selector>),
    /// `:is(...)` / `:matches(...)` — matches if any inner does.
    Is(Vec<Selector>),
    /// `:where(...)` — same matching as `:is()` but contributes zero
    /// specificity. The cascade reads this variant to do that.
    Where(Vec<Selector>),
    /// `:nth-child(an+b)`. The element's 1-based index among its
    /// element siblings must satisfy `idx = a·n + b` for some `n >= 0`.
    NthChild(Nth),
    /// `:nth-of-type(an+b)` — same, but indexes within siblings sharing
    /// the same tag name.
    NthOfType(Nth),
    /// `:nth-last-child(an+b)` — index counted from the end.
    NthLastChild(Nth),
    /// `:nth-last-of-type(an+b)`.
    NthLastOfType(Nth),
}

#[derive(Debug, Clone, Copy)]
pub struct Nth {
    pub a: i32,
    pub b: i32,
}

impl Nth {
    /// `:nth-child(odd)` ≡ `2n+1`, `:nth-child(even)` ≡ `2n`.
    pub const ODD: Nth = Nth { a: 2, b: 1 };
    pub const EVEN: Nth = Nth { a: 2, b: 0 };

    /// True if `index` (1-based) satisfies `a·n + b = index` for some
    /// non-negative integer `n`.
    pub fn matches(&self, index: i32) -> bool {
        if index < 1 {
            return false;
        }
        match self.a {
            0 => index == self.b,
            a => {
                let n = (index - self.b) as f32 / a as f32;
                n.fract() == 0.0 && n >= 0.0
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AttributeSelector {
    pub name: String,
    pub op: AttributeOp,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeOp {
    Exists,     // [name]
    Equals,     // [name=val]
    Includes,   // [name~=val]    whitespace-separated word match
    DashPrefix, // [name|=val]    val or val-...
    Starts,     // [name^=val]
    Ends,       // [name$=val]
    Contains,   // [name*=val]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    Descendant,
    Child,
    AdjacentSibling,
    GeneralSibling,
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub property: String, // includes leading `--` for custom properties
    pub value: Value,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Url field consumed by background-image in phase 5/paint
pub enum Value {
    Keyword(String),
    Length(f32, Unit),
    Percentage(f32),
    Color(Color),
    Number(f32),
    String(String),
    Url(String),
    List(Vec<Value>),
    /// `var(--name)` or `var(--name, fallback)` — substituted at apply time
    /// against the element's resolved custom properties.
    Var {
        name: String,
        fallback: Option<Box<Value>>,
    },
    /// `calc(...)` — evaluated at apply time. Falls back to `Keyword("")`
    /// (no effect) if it can't be resolved at cascade time (e.g. contains
    /// percentages or vw/vh that need layout context).
    Calc(Box<CalcExpr>),
    /// `linear-gradient(<angle>, <color>, ...)` pre-parsed into uniformly
    /// spaced color stops + an angle in degrees (180 = top → bottom).
    LinearGradient {
        angle_deg: f32,
        stops: Vec<(f32, Color)>,
    },
    /// Unrecognised function call kept structured so consumers like `transform`
    /// can still pattern-match on it.
    Function {
        name: String,
        args: Vec<Value>,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Percentage resolved at layout time in phase 4
pub enum CalcExpr {
    Length(f32, Unit),
    Percentage(f32),
    Number(f32),
    Var(String, Option<Box<Value>>),
    Add(Box<CalcExpr>, Box<CalcExpr>),
    Sub(Box<CalcExpr>, Box<CalcExpr>),
    Mul(Box<CalcExpr>, Box<CalcExpr>),
    Div(Box<CalcExpr>, Box<CalcExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Px,
    Em,
    Rem,
    Pt,
    Pc,
    Cm,
    Mm,
    In,
    Vw,
    Vh,
    /// CSS `fr` — only meaningful as a grid track sizer.
    Fr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

// ---------------- Computed style ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Block,
    Inline,
    InlineBlock,
    ListItem,
    Flex,
    InlineFlex,
    Grid,
    InlineGrid,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexWrap {
    NoWrap,
    Wrap,
    WrapReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JustifyContent {
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignItems {
    FlexStart,
    FlexEnd,
    Center,
    Stretch,
    Baseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
    Sticky,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSizing {
    ContentBox,
    BorderBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignContent {
    Stretch,
    FlexStart,
    FlexEnd,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridAutoFlow {
    Row,
    Column,
    RowDense,
    ColumnDense,
}

/// A single endpoint of a `grid-column` / `grid-row` placement.
#[derive(Debug, Clone)]
pub enum GridLine {
    Auto,
    /// 1-based line number (negatives count from the end).
    Index(i32),
    /// Named line / area (`grid-column: header`).
    Name(String),
    /// Span N tracks.
    Span(i32),
}

/// Resolved placement for a grid item.
#[derive(Debug, Clone, Default)]
pub struct GridPlacement {
    pub column_start: Option<GridLine>,
    pub column_end: Option<GridLine>,
    pub row_start: Option<GridLine>,
    pub row_end: Option<GridLine>,
    /// `grid-area: name` references a named region in
    /// `grid-template-areas`.
    pub area: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum GridTrack {
    /// A fixed pixel width (also used for em-resolved lengths).
    Px(f32),
    /// A flexible track sized in fractional units.
    Fr(f32),
    /// `auto` — sized to fit its content.
    Auto,
    /// `<percentage>` of the container.
    Percent(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontStyle {
    Normal,
    Italic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
    None,
    Solid,
    Dashed,
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Pre,
    NoWrap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableLayout {
    Auto,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDecoration {
    None,
    Underline,
    LineThrough,
    Overline,
}

#[derive(Debug, Clone)]
pub enum BackgroundImage {
    Url(String),
    LinearGradient {
        angle_deg: f32,
        stops: Vec<(f32, Color)>, // (position 0..1, color)
    },
}

#[derive(Debug, Clone, Copy)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub color: Color,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BoxSides {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl BoxSides {
    pub fn uniform(v: f32) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // Length / Percent consumed by layout in phase 4
pub enum Dimension {
    Auto,
    Length(f32),
    Percent(f32),
}

#[derive(Debug, Clone)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background_color: Color,
    pub font_family: Vec<String>,
    pub font_size: f32, // px
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub text_align: TextAlign,
    pub margin: BoxSides,
    pub padding: BoxSides,
    pub border_width: BoxSides,
    pub border_color: Color,
    pub border_style: BorderStyle,
    pub width: Dimension,
    pub height: Dimension,
    pub line_height: f32, // multiplier of font_size
    pub white_space: WhiteSpace,
    /// Horizontal and vertical gap between adjacent table cells (the CSS
    /// `border-spacing` property). Inherited.
    pub border_spacing: (f32, f32),
    /// `auto` measures intrinsic content widths per column; `fixed` uses
    /// `<col>` widths and first-row widths. Not inherited.
    pub table_layout: TableLayout,
    /// CSS `content` property. Only meaningful on `::before` / `::after`
    /// pseudo-element styles — for real elements it stays `None`. A
    /// non-`None` value triggers generation of a pseudo-element box during
    /// layout. Not inherited.
    pub content: Option<String>,
    /// Underline / line-through / overline. Inherited.
    pub text_decoration: TextDecoration,
    /// `background-image` value: an image URL to fetch, or a
    /// `linear-gradient(...)` to render. Not inherited.
    pub background_image: Option<BackgroundImage>,
    /// Single uniform corner radius in pixels (no per-corner support).
    /// Not inherited.
    pub border_radius: f32,
    /// 0..=1. Multiplies the alpha of every painted pixel in this element's
    /// subtree. Not inherited.
    pub opacity: f32,
    /// One drop shadow. Not inherited.
    pub box_shadow: Option<BoxShadow>,
    /// `transform: translate(...)` only — `(dx_px, dy_px)`. Not inherited
    /// in the CSS sense but propagates to descendants via the paint
    /// translate stack.
    pub transform_translate: Option<(f32, f32)>,
    /// Composed 2D transform (rotate / scale / skew / matrix / mixed).
    /// When `Some`, paint uses this matrix for rects, borders, and
    /// background-image draws; text glyphs continue painting at the
    /// matrix's translation component only (no per-glyph rotation yet).
    pub transform: Option<Transform2D>,

    // ----- Flexbox container properties -----
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub justify_content: JustifyContent,
    pub align_items: AlignItems,
    /// Spacing between adjacent flex/grid items (CSS `gap`). Tuple is
    /// `(row_gap, column_gap)`.
    pub gap: (f32, f32),

    // ----- Flex item properties -----
    /// `flex-grow` factor (default 0).
    pub flex_grow: f32,
    /// `flex-shrink` factor (default 1).
    pub flex_shrink: f32,
    /// `flex-basis` (default `auto` → use natural content size).
    pub flex_basis: Dimension,

    /// Multi-line flex cross-axis distribution.
    pub align_content: AlignContent,
    /// Flex item reordering. Items are laid out in ascending order, ties
    /// broken by DOM order. Default 0.
    pub order: i32,

    // ----- Grid container properties -----
    pub grid_template_columns: Vec<GridTrack>,
    pub grid_template_rows: Vec<GridTrack>,
    /// `grid-template-areas` parsed as a row-of-rows: outer Vec is rows,
    /// inner Vec is the named cells across that row (or `.` for empty).
    pub grid_template_areas: Vec<Vec<String>>,
    pub grid_auto_flow: GridAutoFlow,

    // ----- Grid item properties -----
    pub grid_placement: GridPlacement,

    // ----- Positioning -----
    pub position: Position,
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
    /// `z-index` painting order. `None` means "auto" (default DOM order).
    pub z_index: Option<i32>,

    // ----- Sizing constraints -----
    pub box_sizing: BoxSizing,
    pub min_width: Option<f32>,
    pub max_width: Option<f32>,
    pub min_height: Option<f32>,
    pub max_height: Option<f32>,

    /// Resolved custom properties (CSS variables). Inherited like color.
    pub custom_properties: HashMap<String, Value>,
}

impl ComputedStyle {
    pub const ROOT_FONT_SIZE: f32 = 16.0;

    pub fn initial() -> Self {
        Self {
            display: Display::Inline,
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            font_family: vec!["serif".into()],
            font_size: Self::ROOT_FONT_SIZE,
            font_weight: 400,
            font_style: FontStyle::Normal,
            text_align: TextAlign::Left,
            margin: BoxSides::default(),
            padding: BoxSides::default(),
            border_width: BoxSides::default(),
            border_color: Color::BLACK,
            border_style: BorderStyle::None,
            width: Dimension::Auto,
            height: Dimension::Auto,
            line_height: 1.2,
            white_space: WhiteSpace::Normal,
            border_spacing: (2.0, 2.0), // CSS initial value
            table_layout: TableLayout::Auto,
            content: None,
            text_decoration: TextDecoration::None,
            background_image: None,
            border_radius: 0.0,
            opacity: 1.0,
            box_shadow: None,
            transform_translate: None,
            transform: None,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::NoWrap,
            justify_content: JustifyContent::FlexStart,
            align_items: AlignItems::Stretch,
            gap: (0.0, 0.0),
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: Dimension::Auto,
            align_content: AlignContent::Stretch,
            order: 0,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_template_areas: Vec::new(),
            grid_auto_flow: GridAutoFlow::Row,
            grid_placement: GridPlacement::default(),
            position: Position::Static,
            top: None,
            right: None,
            bottom: None,
            left: None,
            z_index: None,
            box_sizing: BoxSizing::ContentBox,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
            custom_properties: HashMap::new(),
        }
    }

    pub fn inherit_from(parent: &Self) -> Self {
        let mut s = Self::initial();
        s.color = parent.color;
        s.font_family = parent.font_family.clone();
        s.font_size = parent.font_size;
        s.font_weight = parent.font_weight;
        s.font_style = parent.font_style;
        s.text_align = parent.text_align;
        s.line_height = parent.line_height;
        s.white_space = parent.white_space;
        s.border_spacing = parent.border_spacing; // inherited
        s.text_decoration = parent.text_decoration; // inherited
        // table_layout, background_image, border_radius, opacity, box_shadow,
        // and transform are NOT inherited per CSS spec.
        s.custom_properties = parent.custom_properties.clone();
        s
    }
}
