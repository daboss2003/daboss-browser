//! CSS types: parser AST plus the computed-style struct that layout/paint
//! will read from.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TransitionRule {
    /// Property name (lowercased). `"all"` matches every animatable
    /// property; visual interpolation today only honours `opacity`.
    pub property: String,
    /// Duration in seconds.
    pub duration_s: f32,
    /// Delay in seconds.
    pub delay_s: f32,
    /// Timing function — currently only `linear` and `ease` are
    /// distinguishable; everything else falls back to `linear`.
    pub timing: TimingFunction,
}

#[derive(Debug, Clone)]
pub struct AnimationRule {
    pub name: String,
    pub duration_s: f32,
    pub delay_s: f32,
    pub iteration_count: f32, // `f32::INFINITY` for `infinite`
    pub timing: TimingFunction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingFunction {
    Linear,
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
}

/// One entry in a `filter:` declaration. Numeric arguments are
/// percent-normalised (0.0..=1.0 for `100%`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)] // fields read by paint once visual application lands
pub enum FilterFunction {
    Blur(f32),
    Brightness(f32),
    Contrast(f32),
    Grayscale(f32),
    HueRotate(f32),
    Invert(f32),
    Opacity(f32),
    Saturate(f32),
    Sepia(f32),
}

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
    /// Shadow-DOM scope. `None` means a page-level (or UA)
    /// stylesheet; `Some(host)` means the rules were emitted from a
    /// `<style>` inside the shadow root attached to `host`, and
    /// should only match descendants of that shadow root.
    pub scope: Option<crate::dom::NodeId>,
    /// `true` for the User Agent stylesheet. UA rules bypass the
    /// shadow-scope filter so default block/inline/etc. still
    /// apply inside shadow trees.
    pub is_ua: bool,
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
    /// `:has(<selectors>)` — matches if ANY descendant (or the
    /// element itself for the leading-`:scope` variant) of the
    /// element matches the inner selector list.
    Has(Vec<Selector>),
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

/// `anchor(<name>? <side>)` reference parsed off the value of `top`,
/// `right`, `bottom`, or `left`. `name` is `None` when the call omits
/// the dashed-ident — the element's `position-anchor` is used at
/// layout time.
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorRef {
    pub name: Option<String>,
    pub side: AnchorSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorSide {
    Top,
    Right,
    Bottom,
    Left,
    Center,
    /// Logical "start" / "end" — for the toy we treat as Top/Left for
    /// block-axis lookups and Left/Right for inline-axis (i.e. an LTR
    /// approximation).
    Start,
    End,
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

#[derive(Debug, Clone)]
pub enum GridTrack {
    /// A fixed pixel width (also used for em-resolved lengths).
    Px(f32),
    /// A flexible track sized in fractional units.
    Fr(f32),
    /// `auto` — sized to fit its content.
    Auto,
    /// `<percentage>` of the container.
    Percent(f32),
    /// `minmax(min, max)` — clamps the resolved size between two
    /// other track sizes. Box+heap because the inner tracks would
    /// otherwise make the enum recursive.
    MinMax(Box<GridTrack>, Box<GridTrack>),
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
    /// Direction-aware: maps to Left for LTR, Right for RTL.
    Start,
    /// Direction-aware: maps to Right for LTR, Left for RTL.
    End,
}

impl TextAlign {
    /// Resolve `start`/`end` against the writing direction.
    /// Other values pass through unchanged.
    pub fn resolved(self, dir: Direction) -> TextAlign {
        match (self, dir) {
            (TextAlign::Start, Direction::Ltr) => TextAlign::Left,
            (TextAlign::Start, Direction::Rtl) => TextAlign::Right,
            (TextAlign::End, Direction::Ltr) => TextAlign::Right,
            (TextAlign::End, Direction::Rtl) => TextAlign::Left,
            (other, _) => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStyle {
    None,
    Solid,
    Dashed,
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Rtl currently only affects text-align default
pub enum Direction {
    Ltr,
    Rtl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // hidden/scroll/auto parsed; clipping is layout-time work
pub enum Overflow {
    Visible,
    Hidden,
    Scroll,
    Auto,
    Clip,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // ellipsis/clip parsed; visual truncation is layout work
pub enum TextOverflow {
    Clip,
    Ellipsis,
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhiteSpace {
    Normal,
    Pre,
    NoWrap,
    /// Like `pre` but lines wrap at the box edge.
    PreWrap,
    /// Like `normal` but preserves runs of whitespace.
    PreLine,
    /// Like `pre-wrap` but wrappable on whitespace inside the wrap.
    BreakSpaces,
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

/// How `mask-image` is interpreted per-pixel. `MatchSource` is the
/// CSS default: alpha for `mask-image: url(...)` SVG sources,
/// luminance for raster bitmaps. The toy collapses match-source to
/// alpha (matches the most common author usage — alpha-channel
/// PNG masks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskMode {
    Alpha,
    Luminance,
    MatchSource,
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
    /// `mask-image: url(...)` or `mask-image: linear-gradient(...)`.
    /// Applied per-pixel at paint time: the element subtree is
    /// composited to an offscreen pixmap, the mask is drawn at the
    /// element's box size, and `mask_mode` selects whether the
    /// mask's alpha or luminance multiplies into the subtree alpha.
    pub mask_image: Option<BackgroundImage>,
    pub mask_mode: MaskMode,
    /// Single uniform corner radius in pixels (no per-corner support).
    /// Not inherited.
    pub border_radius: f32,
    /// 0..=1. Multiplies the alpha of every painted pixel in this element's
    /// subtree. Not inherited.
    pub opacity: f32,
    /// `filter:` chain. Only the `opacity(<n>)` function affects paint
    /// today (folded into [`ComputedStyle::opacity`] equivalence at
    /// apply time). Other functions parse but don't render yet.
    pub filter: Vec<FilterFunction>,
    /// Writing direction. Inherited. Affects the default value of
    /// `text-align` (`Ltr` → `Left`, `Rtl` → `Right`); other RTL
    /// effects (inline-direction reordering, bidi-aware caret) are
    /// future work.
    pub direction: Direction,
    /// `overflow-x` / `overflow-y`. Parsed and stored but layout
    /// doesn't actually clip yet — see the deferred work in
    /// `text-overflow: ellipsis` below.
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    /// `text-overflow`. Currently parse-only; visual truncation
    /// requires width-aware glyph clipping in the inline formatting
    /// context.
    pub text_overflow: TextOverflow,
    /// `line-clamp` / `-webkit-line-clamp`. When `Some(n)`, inline
    /// text inside this element is limited to `n` visible lines,
    /// with an ellipsis stamped onto the last line if more text
    /// would have wrapped.
    pub line_clamp: Option<u32>,
    /// `scroll-snap-type` — e.g. `x mandatory`, `y proximity`,
    /// `both mandatory`. Lower-cased, parsed as-is. The scroll
    /// handler reads this to decide whether to snap on scroll-end.
    pub scroll_snap_type: Option<String>,
    /// `scroll-snap-align` — `none`, `start`, `end`, `center`, or a
    /// pair. Stored as the raw lower-cased string for inspection.
    pub scroll_snap_align: Option<String>,
    /// `font-feature-settings` — raw `"liga" 1, "smcp"` style
    /// payload. Passed verbatim; cosmic-text picks up the standard
    /// OpenType features it knows about.
    pub font_feature_settings: Option<String>,
    /// `hyphens` — `none` / `manual` / `auto`. Without a
    /// hyphenation dictionary `auto` falls back to `manual` (i.e.
    /// only break at U+00AD soft-hyphen).
    pub hyphens: Option<String>,
    /// `container-type` — `normal` / `inline-size` / `size`. Marks
    /// this element as a query container so descendant
    /// `@container` rules can target its box size.
    pub container_type: Option<String>,
    /// `container-name` — author-supplied identifier for filtered
    /// `@container <name> (...)` queries.
    pub container_name: Option<String>,
    /// `will-change` — comma-separated, lower-cased token list. Used
    /// as a hint to promote this element into its own composited
    /// layer; the painter checks for `transform` / `opacity` /
    /// `filter` and caches the layer's pixmap by content hash so a
    /// CSS animation that only mutates those properties skips the
    /// subtree repaint cost.
    pub will_change: Option<String>,
    /// `aspect-ratio: <w> / <h>`. When set, layout derives the
    /// other dimension from a known one (e.g. width from height
    /// for an `<img>` whose intrinsic ratio we want to override).
    /// Stored as `width / height`. `None` means "auto".
    pub aspect_ratio: Option<f32>,
    /// `transition: <prop> <duration> [<timing>] [<delay>]` entries.
    /// When a tracked property changes between cascades, the browser
    /// shell starts a running animation that interpolates the old →
    /// new value over `duration`.
    pub transitions: Vec<TransitionRule>,
    /// `animation: <name> <duration> ...` entries pointing at
    /// `@keyframes` blocks. Browser shell instantiates each on the
    /// next animation tick.
    pub animations: Vec<AnimationRule>,
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
    /// `grid-template-columns: subgrid` — inherit column tracks
    /// from the nearest ancestor grid container. The layout slices
    /// the parent's tracks to match this item's column span.
    pub subgrid_columns: bool,
    /// `grid-template-rows: subgrid` — same, for rows.
    pub subgrid_rows: bool,
    /// `grid-template-areas` parsed as a row-of-rows: outer Vec is rows,
    /// inner Vec is the named cells across that row (or `.` for empty).
    pub grid_template_areas: Vec<Vec<String>>,
    pub grid_auto_flow: GridAutoFlow,
    /// Track size used when items spill past the explicit columns
    /// (e.g. `grid-column: 5` inside a 3-column template).
    pub grid_auto_columns: GridTrack,
    pub grid_auto_rows: GridTrack,
    /// Default item alignment along the inline axis (overridable per
    /// item via `justify-self`).
    pub justify_items: AlignItems,

    // ----- Grid item properties -----
    pub grid_placement: GridPlacement,
    /// Per-item override of `align-items` (cross axis in flex, block
    /// axis in grid). `None` inherits the container default.
    pub align_self: Option<AlignItems>,
    /// Per-item override of `justify-items` (inline axis in grid).
    pub justify_self: Option<AlignItems>,

    // ----- Positioning -----
    pub position: Position,
    pub top: Option<f32>,
    pub right: Option<f32>,
    pub bottom: Option<f32>,
    pub left: Option<f32>,
    /// `z-index` painting order. `None` means "auto" (default DOM order).
    pub z_index: Option<i32>,

    // ----- Anchor positioning -----
    /// `anchor-name: --foo` — this element registers as an anchor under
    /// the given dashed-ident. Multiple anchor-name values join with a
    /// comma in the spec; we keep only the first for the toy.
    pub anchor_name: Option<String>,
    /// `position-anchor: --foo` — default anchor for un-named
    /// `anchor()` calls in this element's inset properties.
    pub position_anchor: Option<String>,
    /// `top: anchor(--foo bottom)` etc. When present, supersedes the
    /// plain length in the matching side during the post-layout
    /// anchor-positioning pass. Name=None means "use position-anchor".
    pub anchor_top: Option<AnchorRef>,
    pub anchor_right: Option<AnchorRef>,
    pub anchor_bottom: Option<AnchorRef>,
    pub anchor_left: Option<AnchorRef>,

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
            mask_image: None,
            mask_mode: MaskMode::MatchSource,
            border_radius: 0.0,
            opacity: 1.0,
            filter: Vec::new(),
            direction: Direction::Ltr,
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            text_overflow: TextOverflow::Clip,
            line_clamp: None,
            scroll_snap_type: None,
            scroll_snap_align: None,
            font_feature_settings: None,
            hyphens: None,
            container_type: None,
            container_name: None,
            will_change: None,
            aspect_ratio: None,
            transitions: Vec::new(),
            animations: Vec::new(),
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
            subgrid_columns: false,
            subgrid_rows: false,
            grid_template_areas: Vec::new(),
            grid_auto_flow: GridAutoFlow::Row,
            grid_auto_columns: GridTrack::Auto,
            grid_auto_rows: GridTrack::Auto,
            justify_items: AlignItems::Stretch,
            grid_placement: GridPlacement::default(),
            align_self: None,
            justify_self: None,
            position: Position::Static,
            anchor_name: None,
            position_anchor: None,
            anchor_top: None,
            anchor_right: None,
            anchor_bottom: None,
            anchor_left: None,
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
        s.direction = parent.direction; // inherited
        // Default text-align follows direction.
        if matches!(parent.direction, Direction::Rtl)
            && matches!(parent.text_align, TextAlign::Left)
        {
            s.text_align = TextAlign::Right;
        }
        // table_layout, background_image, border_radius, opacity, box_shadow,
        // and transform are NOT inherited per CSS spec.
        s.custom_properties = parent.custom_properties.clone();
        s
    }
}
