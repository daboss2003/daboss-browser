//! CSS types: parser AST plus the computed-style struct that layout/paint
//! will read from.

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Stylesheet {
    pub rules: Vec<Rule>,
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
    pub pseudo_classes: Vec<String>,
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
    None,
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
        // table_layout is not inherited (CSS spec)
        s.custom_properties = parent.custom_properties.clone();
        s
    }
}
