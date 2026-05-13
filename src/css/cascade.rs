//! Selector matching, specificity, and cascade resolution.
//!
//! Per-element pipeline:
//!  1. Inherit from parent (or initial values at the root).
//!  2. Collect every rule whose selector list matches; sort by
//!     (specificity, source order). Rules with a pseudo-element are
//!     filtered out — those don't apply to real DOM nodes (phase 4).
//!     Rules whose only-rightmost compound has unsupported pseudo-classes
//!     (`:hover`, `:focus`, etc.) are filtered out — they'll start matching
//!     once phase 6 wires up interaction state.
//!  3. Two-pass apply per element: first pass collects `--foo` custom
//!     properties into the element's map; second pass applies normal
//!     declarations with `var()` / `calc()` resolved against that map.
//!  4. Inline `style=""` is applied last with the same two passes.

use std::collections::HashMap;

use crate::css::parser::parse_inline_declarations;
use crate::css::types::{
    AlignContent, AlignItems, AttributeOp, AttributeSelector, BackgroundImage, BorderStyle,
    BoxShadow, BoxSides, BoxSizing, CalcExpr, Color, Combinator, ComputedStyle, Declaration,
    Dimension, Display, FlexDirection, FlexWrap, FontStyle, GridAutoFlow, GridLine, GridTrack,
    JustifyContent, Position, Rule, Selector, SimpleSelector, Stylesheet, TableLayout, TextAlign,
    TextDecoration, Unit, Value, WhiteSpace,
};
use crate::dom::{Dom, NodeId, NodeKind};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct Specificity(pub u16, pub u16, pub u16);

pub fn compute_specificity(sel: &Selector) -> Specificity {
    let mut ids = 0u16;
    let mut classes = 0u16;
    let mut tags = 0u16;
    for compound in &sel.compounds {
        if compound.id.is_some() {
            ids = ids.saturating_add(1);
        }
        classes = classes.saturating_add(compound.classes.len() as u16);
        classes = classes.saturating_add(compound.attributes.len() as u16);
        classes = classes.saturating_add(compound.pseudo_classes.len() as u16);
        if compound.tag.is_some() {
            tags = tags.saturating_add(1);
        }
    }
    if sel.pseudo_element.is_some() {
        tags = tags.saturating_add(1);
    }
    Specificity(ids, classes, tags)
}

#[allow(dead_code)] // backward-compatible helper used by selected tests
pub fn selector_matches(sel: &Selector, dom: &Dom, node: NodeId) -> bool {
    selector_matches_pseudo(sel, dom, node, None, &InteractionState::EMPTY)
}

/// `pseudo` selects which selectors match: `None` means "real-element" rules
/// (rejects any selector that targets a pseudo-element); `Some("before")` /
/// `Some("after")` matches only rules whose final compound carries that
/// pseudo-element. `hover_chain` lists the nodes currently in the `:hover`
/// state (the hovered node plus its ancestors).
pub fn selector_matches_pseudo(
    sel: &Selector,
    dom: &Dom,
    node: NodeId,
    pseudo: Option<&str>,
    interaction: &InteractionState,
) -> bool {
    if sel.pseudo_element.as_deref() != pseudo {
        return false;
    }
    if sel.compounds.is_empty() {
        return false;
    }
    let last = sel.compounds.len() - 1;
    if !matches_simple(&sel.compounds[last], dom, node, interaction) {
        return false;
    }
    let mut current = node;
    for i in (0..last).rev() {
        let combinator = sel.combinators[i];
        let target = &sel.compounds[i];
        let found = match combinator {
            Combinator::Descendant => walk_up(dom, current, |id| matches_simple(target, dom, id, interaction)),
            Combinator::Child => dom.node(current).parent.filter(|p| matches_simple(target, dom, *p, interaction)),
            Combinator::AdjacentSibling => dom
                .node(current)
                .prev_sibling
                .filter(|s| matches_simple(target, dom, *s, interaction)),
            Combinator::GeneralSibling => {
                walk_prev(dom, current, |id| matches_simple(target, dom, id, interaction))
            }
        };
        match found {
            Some(id) => current = id,
            None => return false,
        }
    }
    true
}

fn walk_up<F: Fn(NodeId) -> bool>(dom: &Dom, from: NodeId, pred: F) -> Option<NodeId> {
    let mut p = dom.node(from).parent;
    while let Some(id) = p {
        if pred(id) {
            return Some(id);
        }
        p = dom.node(id).parent;
    }
    None
}

fn walk_prev<F: Fn(NodeId) -> bool>(dom: &Dom, from: NodeId, pred: F) -> Option<NodeId> {
    let mut s = dom.node(from).prev_sibling;
    while let Some(id) = s {
        if pred(id) {
            return Some(id);
        }
        s = dom.node(id).prev_sibling;
    }
    None
}

fn matches_simple(
    sel: &SimpleSelector,
    dom: &Dom,
    node: NodeId,
    interaction: &InteractionState,
) -> bool {
    let (tag, attrs) = match &dom.node(node).kind {
        NodeKind::Element { tag, attrs } => (tag.as_str(), attrs.as_slice()),
        _ => return false,
    };

    if let Some(want) = &sel.tag {
        if tag != want {
            return false;
        }
    }
    if let Some(want) = &sel.id {
        let id_value = attrs.iter().find(|(k, _)| k == "id").map(|(_, v)| v.as_str());
        if id_value != Some(want.as_str()) {
            return false;
        }
    }
    for class in &sel.classes {
        let has = attrs
            .iter()
            .find(|(k, _)| k == "class")
            .map(|(_, v)| v.split_ascii_whitespace().any(|c| c == class))
            .unwrap_or(false);
        if !has {
            return false;
        }
    }
    for attr_sel in &sel.attributes {
        if !match_attribute(attr_sel, attrs) {
            return false;
        }
    }
    for pc in &sel.pseudo_classes {
        if !match_pseudo_class(pc, dom, node, interaction) {
            return false;
        }
    }
    true
}

fn match_attribute(sel: &AttributeSelector, attrs: &[(String, String)]) -> bool {
    let target = attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&sel.name))
        .map(|(_, v)| v.as_str());
    let val = match (sel.op, &sel.value, target) {
        (AttributeOp::Exists, _, Some(_)) => return true,
        (AttributeOp::Exists, _, None) => return false,
        (_, Some(v), Some(t)) => (v.as_str(), t),
        _ => return false,
    };
    let (needle, hay) = val;
    match sel.op {
        AttributeOp::Exists => true,
        AttributeOp::Equals => hay == needle,
        AttributeOp::Includes => hay.split_ascii_whitespace().any(|w| w == needle),
        AttributeOp::DashPrefix => hay == needle || hay.starts_with(&format!("{needle}-")),
        AttributeOp::Starts => hay.starts_with(needle),
        AttributeOp::Ends => hay.ends_with(needle),
        AttributeOp::Contains => !needle.is_empty() && hay.contains(needle),
    }
}

fn match_pseudo_class(
    name: &str,
    dom: &Dom,
    node: NodeId,
    interaction: &InteractionState,
) -> bool {
    match name {
        "root" => dom.node(node).parent == Some(dom.document()),
        "first-child" => dom.node(node).prev_sibling.is_none(),
        "last-child" => dom.node(node).next_sibling.is_none(),
        "hover" => interaction.hover_chain.contains(&node),
        "focus" | "focus-visible" | "focus-within" => {
            interaction.focus_chain.contains(&node)
        }
        // :active, :checked, :visited, :link, :not(...), :nth-*, etc.
        // Still not modelled.
        _ => false,
    }
}

/// Per-frame interaction state used by the cascade for stateful
/// pseudo-classes. Both chains are "the matched node + its ancestors".
pub struct InteractionState<'a> {
    pub hover_chain: &'a [NodeId],
    pub focus_chain: &'a [NodeId],
}

impl InteractionState<'_> {
    pub const EMPTY: InteractionState<'static> = InteractionState {
        hover_chain: &[],
        focus_chain: &[],
    };
}

// ---------------- Style tree ----------------

pub struct StyleTree {
    pub styles: Vec<ComputedStyle>,
    /// `before[i] = Some(style)` iff element node `i` has a non-empty `::before`
    /// pseudo. The style inherits from the element's own style and is the
    /// computed style the pseudo-element box will use during layout/paint.
    pub before: Vec<Option<ComputedStyle>>,
    pub after: Vec<Option<ComputedStyle>>,
}

impl StyleTree {
    #[allow(dead_code)] // backward-compatible wrapper kept for tests
    pub fn compute(dom: &Dom, sheets: &[&Stylesheet]) -> Self {
        Self::compute_with(dom, sheets, &InteractionState::EMPTY)
    }

    /// Cascade against the given stylesheets, applying `:hover` / `:focus`
    /// to nodes listed in `interaction.hover_chain` / `interaction.focus_chain`
    /// respectively. Pass `&InteractionState::EMPTY` for the static case.
    pub fn compute_with(
        dom: &Dom,
        sheets: &[&Stylesheet],
        interaction: &InteractionState,
    ) -> Self {
        let count = highest_node_id(dom).index() + 1;
        let mut styles = vec![ComputedStyle::initial(); count];
        let mut before = vec![None; count];
        let mut after = vec![None; count];
        compute_recursive(
            dom,
            dom.document(),
            sheets,
            None,
            &mut styles,
            &mut before,
            &mut after,
            interaction,
        );
        Self {
            styles,
            before,
            after,
        }
    }

    pub fn get(&self, id: NodeId) -> &ComputedStyle {
        &self.styles[id.index()]
    }

    pub fn before_style(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.before.get(id.index()).and_then(|s| s.as_ref())
    }

    pub fn after_style(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.after.get(id.index()).and_then(|s| s.as_ref())
    }
}

fn highest_node_id(dom: &Dom) -> NodeId {
    let mut max = dom.document();
    walk_max(dom, dom.document(), &mut max);
    max
}

fn walk_max(dom: &Dom, node: NodeId, max: &mut NodeId) {
    if node.index() > max.index() {
        *max = node;
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for k in kids {
        walk_max(dom, k, max);
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_recursive(
    dom: &Dom,
    node: NodeId,
    sheets: &[&Stylesheet],
    parent_style: Option<&ComputedStyle>,
    out: &mut [ComputedStyle],
    before: &mut [Option<ComputedStyle>],
    after: &mut [Option<ComputedStyle>],
    interaction: &InteractionState,
) {
    let style = compute_one(dom, node, sheets, parent_style, interaction);
    if matches!(&dom.node(node).kind, NodeKind::Element { .. }) {
        before[node.index()] =
            compute_pseudo_style(dom, node, sheets, &style, "before", interaction);
        after[node.index()] = compute_pseudo_style(dom, node, sheets, &style, "after", interaction);
    }
    out[node.index()] = style;
    let style_for_children = out[node.index()].clone();
    let kids: Vec<NodeId> = dom.children(node).collect();
    for child in kids {
        compute_recursive(
            dom,
            child,
            sheets,
            Some(&style_for_children),
            out,
            before,
            after,
            interaction,
        );
    }
}

fn compute_pseudo_style(
    dom: &Dom,
    node: NodeId,
    sheets: &[&Stylesheet],
    element_style: &ComputedStyle,
    pseudo_name: &str,
    interaction: &InteractionState,
) -> Option<ComputedStyle> {
    // Pseudo-elements inherit non-resetting properties from their host.
    let mut style = ComputedStyle::inherit_from(element_style);
    style.content = None; // reset; rules will set it

    let mut matched: Vec<(Specificity, usize, &Rule)> = Vec::new();
    let mut order = 0usize;
    for sheet in sheets {
        for rule in &sheet.rules {
            order += 1;
            for sel in &rule.selectors {
                if selector_matches_pseudo(sel, dom, node, Some(pseudo_name), interaction) {
                    matched.push((compute_specificity(sel), order, rule));
                    break;
                }
            }
        }
    }
    if matched.is_empty() {
        return None;
    }
    matched.sort_by_key(|(spec, ord, _)| (*spec, *ord));

    // Two-pass apply (like the main cascade): custom properties first, then
    // normal declarations resolving against the local var map.
    use std::collections::HashMap;
    let mut props: HashMap<String, Value> = style.custom_properties.clone();
    for (_, _, rule) in &matched {
        for decl in &rule.declarations {
            if decl.property.starts_with("--") {
                let resolved = resolve_value(&decl.value, &props, 0);
                props.insert(decl.property.clone(), resolved);
            }
        }
    }
    style.custom_properties = props;
    for (_, _, rule) in &matched {
        for decl in &rule.declarations {
            if decl.property.starts_with("--") {
                continue;
            }
            apply(decl, &mut style, Some(element_style));
        }
    }

    // No `content` → no generated content → no pseudo box at all.
    if style.content.is_none() {
        return None;
    }
    Some(style)
}

fn compute_one(
    dom: &Dom,
    node: NodeId,
    sheets: &[&Stylesheet],
    parent_style: Option<&ComputedStyle>,
    interaction: &InteractionState,
) -> ComputedStyle {
    let mut style = match parent_style {
        Some(p) => ComputedStyle::inherit_from(p),
        None => ComputedStyle::initial(),
    };

    let attrs = match &dom.node(node).kind {
        NodeKind::Element { attrs, .. } => attrs.clone(),
        _ => return style,
    };

    // Collect matches with specificity + source order.
    let mut matched: Vec<(Specificity, usize, &Rule)> = Vec::new();
    let mut order = 0usize;
    for sheet in sheets {
        for rule in &sheet.rules {
            order += 1;
            for sel in &rule.selectors {
                if selector_matches_pseudo(sel, dom, node, None, interaction) {
                    matched.push((compute_specificity(sel), order, rule));
                    break;
                }
            }
        }
    }
    matched.sort_by_key(|(spec, ord, _)| (*spec, *ord));

    // Inline style is parsed once; treated as the highest-priority "ruleset".
    let inline_decls: Vec<Declaration> = attrs
        .iter()
        .filter(|(k, _)| k == "style")
        .flat_map(|(_, v)| parse_inline_declarations(v))
        .collect();

    // -------- Pass 1: collect custom properties into the element's map --------
    let mut props: HashMap<String, Value> = style.custom_properties.clone();
    for (_, _, rule) in &matched {
        for decl in &rule.declarations {
            if decl.property.starts_with("--") {
                let resolved = resolve_value(&decl.value, &props, 0);
                props.insert(decl.property.clone(), resolved);
            }
        }
    }
    for decl in &inline_decls {
        if decl.property.starts_with("--") {
            let resolved = resolve_value(&decl.value, &props, 0);
            props.insert(decl.property.clone(), resolved);
        }
    }
    style.custom_properties = props;

    // -------- Pass 2: apply normal declarations --------
    for (_, _, rule) in &matched {
        for decl in &rule.declarations {
            if decl.property.starts_with("--") {
                continue;
            }
            apply(decl, &mut style, parent_style);
        }
    }
    for decl in &inline_decls {
        if decl.property.starts_with("--") {
            continue;
        }
        apply(decl, &mut style, parent_style);
    }

    style
}

// ---------------- Value resolution (var, calc) ----------------

const MAX_VAR_DEPTH: u32 = 12;

fn resolve_value(value: &Value, props: &HashMap<String, Value>, depth: u32) -> Value {
    if depth >= MAX_VAR_DEPTH {
        return Value::Keyword(String::new());
    }
    match value {
        Value::Var { name, fallback } => match props.get(name) {
            Some(v) => resolve_value(v, props, depth + 1),
            None => match fallback {
                Some(fb) => resolve_value(fb, props, depth + 1),
                None => Value::Keyword(String::new()),
            },
        },
        Value::Calc(expr) => evaluate_calc(expr, props, depth + 1).unwrap_or(Value::Keyword(String::new())),
        Value::List(items) => Value::List(items.iter().map(|v| resolve_value(v, props, depth)).collect()),
        other => other.clone(),
    }
}

fn evaluate_calc(
    expr: &CalcExpr,
    props: &HashMap<String, Value>,
    depth: u32,
) -> Option<Value> {
    // We can only fully resolve if there are no percentages / vw / vh,
    // since those need layout context. Returns Length(px), Number, or None.
    let n = calc_to_number(expr, props, depth)?;
    Some(Value::Length(n, Unit::Px))
}

/// Reduce a calc expression to a single px (or unit-less number). Returns
/// `None` if anything requires a layout-time context.
fn calc_to_number(expr: &CalcExpr, props: &HashMap<String, Value>, depth: u32) -> Option<f32> {
    if depth >= MAX_VAR_DEPTH {
        return None;
    }
    match expr {
        CalcExpr::Length(n, u) => length_n_unit_to_px_maybe(*n, *u),
        CalcExpr::Number(n) => Some(*n),
        CalcExpr::Percentage(_) => None,
        CalcExpr::Var(name, fallback) => {
            let value = match props.get(name) {
                Some(v) => v.clone(),
                None => match fallback {
                    Some(fb) => fb.as_ref().clone(),
                    None => return None,
                },
            };
            value_to_number(&value, props, depth + 1)
        }
        CalcExpr::Add(a, b) => Some(calc_to_number(a, props, depth + 1)? + calc_to_number(b, props, depth + 1)?),
        CalcExpr::Sub(a, b) => Some(calc_to_number(a, props, depth + 1)? - calc_to_number(b, props, depth + 1)?),
        CalcExpr::Mul(a, b) => Some(calc_to_number(a, props, depth + 1)? * calc_to_number(b, props, depth + 1)?),
        CalcExpr::Div(a, b) => {
            let den = calc_to_number(b, props, depth + 1)?;
            if den == 0.0 {
                return None;
            }
            Some(calc_to_number(a, props, depth + 1)? / den)
        }
    }
}

fn value_to_number(v: &Value, props: &HashMap<String, Value>, depth: u32) -> Option<f32> {
    match v {
        Value::Length(n, u) => length_n_unit_to_px_maybe(*n, *u),
        Value::Number(n) => Some(*n),
        Value::Percentage(_) => None,
        Value::Var { name, fallback } => {
            let v = match props.get(name) {
                Some(v) => v.clone(),
                None => fallback.as_ref().map(|b| b.as_ref().clone())?,
            };
            value_to_number(&v, props, depth + 1)
        }
        Value::Calc(expr) => calc_to_number(expr, props, depth + 1),
        _ => None,
    }
}

fn length_n_unit_to_px_maybe(n: f32, u: Unit) -> Option<f32> {
    match u {
        Unit::Px => Some(n),
        Unit::Pt => Some(n * 4.0 / 3.0),
        Unit::Pc => Some(n * 16.0),
        Unit::Cm => Some(n * 96.0 / 2.54),
        Unit::Mm => Some(n * 96.0 / 25.4),
        Unit::In => Some(n * 96.0),
        // em/rem need a base font size; we don't have it inside calc evaluation
        // without threading it through. Toy: treat as initial root font size.
        Unit::Em | Unit::Rem => Some(n * ComputedStyle::ROOT_FONT_SIZE),
        Unit::Vw | Unit::Vh | Unit::Fr => None,
    }
}

// ---------------- apply_declaration ----------------

fn apply(decl: &Declaration, style: &mut ComputedStyle, parent: Option<&ComputedStyle>) {
    let resolved = resolve_value(&decl.value, &style.custom_properties, 0);
    apply_declaration(style, &decl.property, &resolved, parent);
}

fn apply_declaration(
    style: &mut ComputedStyle,
    property: &str,
    value: &Value,
    parent: Option<&ComputedStyle>,
) {
    match property {
        "display" => {
            if let Some(d) = display_from(value) {
                style.display = d;
            }
        }
        "color" => {
            if let Some(c) = color_from(value) {
                style.color = c;
            }
        }
        "background-color" => {
            if let Some(c) = color_from(value) {
                style.background_color = c;
            }
        }
        "background" => {
            // The shorthand carries multiple sub-values (color, image, repeat, etc.).
            // We extract the color portion; the rest gets stored implicitly
            // (we don't currently model background images / position / repeat).
            if let Some(c) = color_from_any(value) {
                style.background_color = c;
            }
        }
        "font-size" => {
            let em_base = parent.map(|p| p.font_size).unwrap_or(ComputedStyle::ROOT_FONT_SIZE);
            if let Some(px) = font_size_from(value, em_base) {
                style.font_size = px;
            }
        }
        "font-weight" => {
            if let Some(w) = font_weight_from(value) {
                style.font_weight = w;
            }
        }
        "font-style" => {
            if let Value::Keyword(k) = value {
                style.font_style = match k.as_str() {
                    "italic" | "oblique" => FontStyle::Italic,
                    _ => FontStyle::Normal,
                };
            }
        }
        "font-family" => {
            let families = font_family_from(value);
            if !families.is_empty() {
                style.font_family = families;
            }
        }
        "text-align" => {
            if let Value::Keyword(k) = value {
                style.text_align = match k.as_str() {
                    "right" => TextAlign::Right,
                    "center" => TextAlign::Center,
                    "justify" => TextAlign::Justify,
                    _ => TextAlign::Left,
                };
            }
        }
        "line-height" => match value {
            Value::Number(n) => style.line_height = *n,
            Value::Percentage(p) => style.line_height = p / 100.0,
            Value::Length(_, _) => {
                if let Some(px) = length_to_px(value, style.font_size, parent) {
                    style.line_height = px / style.font_size;
                }
            }
            _ => {}
        },
        "white-space" => {
            if let Value::Keyword(k) = value {
                style.white_space = match k.as_str() {
                    "pre" => WhiteSpace::Pre,
                    "nowrap" => WhiteSpace::NoWrap,
                    _ => WhiteSpace::Normal,
                };
            }
        }
        "margin" => apply_box_shorthand(value, &mut style.margin, style.font_size, parent),
        "margin-top" => apply_side(value, &mut style.margin.top, style.font_size, parent),
        "margin-right" => apply_side(value, &mut style.margin.right, style.font_size, parent),
        "margin-bottom" => apply_side(value, &mut style.margin.bottom, style.font_size, parent),
        "margin-left" => apply_side(value, &mut style.margin.left, style.font_size, parent),
        "padding" => apply_box_shorthand(value, &mut style.padding, style.font_size, parent),
        "padding-top" => apply_side(value, &mut style.padding.top, style.font_size, parent),
        "padding-right" => apply_side(value, &mut style.padding.right, style.font_size, parent),
        "padding-bottom" => apply_side(value, &mut style.padding.bottom, style.font_size, parent),
        "padding-left" => apply_side(value, &mut style.padding.left, style.font_size, parent),
        "border-width" => apply_box_shorthand(value, &mut style.border_width, style.font_size, parent),
        "border-color" => {
            if let Some(c) = color_from(value) {
                style.border_color = c;
            }
        }
        "border-style" => {
            if let Value::Keyword(k) = value {
                style.border_style = match k.as_str() {
                    "solid" => BorderStyle::Solid,
                    "dashed" => BorderStyle::Dashed,
                    "dotted" => BorderStyle::Dotted,
                    _ => BorderStyle::None,
                };
            }
        }
        "border" => apply_border_shorthand(value, style),
        "width" => style.width = dimension_from(value, style.font_size, parent),
        "height" => style.height = dimension_from(value, style.font_size, parent),
        "border-spacing" => {
            // 1 length: same horizontal + vertical. 2 lengths: h, v.
            let em = style.font_size;
            let resolved: Vec<f32> = match value {
                Value::List(items) => items
                    .iter()
                    .filter_map(|v| length_to_px(v, em, parent))
                    .collect(),
                _ => length_to_px(value, em, parent).into_iter().collect(),
            };
            match resolved.as_slice() {
                [h] => style.border_spacing = (*h, *h),
                [h, v, ..] => style.border_spacing = (*h, *v),
                _ => {}
            }
        }
        "table-layout" => {
            if let Value::Keyword(k) = value {
                style.table_layout = match k.as_str() {
                    "fixed" => TableLayout::Fixed,
                    _ => TableLayout::Auto,
                };
            }
        }
        // `border-collapse` is parsed and silently ignored; the toy table
        // engine doesn't implement collapse mode and rendering is identical
        // either way until phase 5 paints borders.
        "border-collapse" => {}
        "content" => {
            style.content = content_from(value);
        }
        "flex-direction" => {
            if let Value::Keyword(k) = value {
                style.flex_direction = match k.as_str() {
                    "row-reverse" => FlexDirection::RowReverse,
                    "column" => FlexDirection::Column,
                    "column-reverse" => FlexDirection::ColumnReverse,
                    _ => FlexDirection::Row,
                };
            }
        }
        "flex-wrap" => {
            if let Value::Keyword(k) = value {
                style.flex_wrap = match k.as_str() {
                    "wrap" => FlexWrap::Wrap,
                    "wrap-reverse" => FlexWrap::WrapReverse,
                    _ => FlexWrap::NoWrap,
                };
            }
        }
        "justify-content" => {
            if let Value::Keyword(k) = value {
                style.justify_content = match k.as_str() {
                    "flex-end" | "end" | "right" => JustifyContent::FlexEnd,
                    "center" => JustifyContent::Center,
                    "space-between" => JustifyContent::SpaceBetween,
                    "space-around" => JustifyContent::SpaceAround,
                    "space-evenly" => JustifyContent::SpaceEvenly,
                    _ => JustifyContent::FlexStart,
                };
            }
        }
        "align-items" => {
            if let Value::Keyword(k) = value {
                style.align_items = match k.as_str() {
                    "flex-end" | "end" => AlignItems::FlexEnd,
                    "center" => AlignItems::Center,
                    "baseline" => AlignItems::Baseline,
                    "stretch" => AlignItems::Stretch,
                    _ => AlignItems::FlexStart,
                };
            }
        }
        "flex-grow" => {
            if let Value::Number(n) = value {
                style.flex_grow = n.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Value::Number(n) = value {
                style.flex_shrink = n.max(0.0);
            }
        }
        "flex-basis" => {
            style.flex_basis = dimension_from(value, style.font_size, parent);
        }
        "flex" => {
            // Shorthand: `flex: <grow> <shrink> <basis>` or just `flex: <grow>`
            // or `flex: <basis>`. Toy implementation honours the common forms.
            apply_flex_shorthand(value, style, parent);
        }
        "gap" | "grid-gap" => {
            // `gap: <length>` sets both row and column gap. With two values:
            // `gap: row column`.
            let em = style.font_size;
            match value {
                Value::List(items) => {
                    let a = items
                        .first()
                        .and_then(|v| length_to_px(v, em, parent))
                        .unwrap_or(0.0);
                    let b = items
                        .get(1)
                        .and_then(|v| length_to_px(v, em, parent))
                        .unwrap_or(a);
                    style.gap = (a, b);
                }
                _ => {
                    if let Some(px) = length_to_px(value, em, parent) {
                        style.gap = (px, px);
                    }
                }
            }
        }
        "row-gap" => {
            if let Some(px) = length_to_px(value, style.font_size, parent) {
                style.gap.0 = px;
            }
        }
        "column-gap" => {
            if let Some(px) = length_to_px(value, style.font_size, parent) {
                style.gap.1 = px;
            }
        }
        "grid-template-columns" => {
            style.grid_template_columns = grid_tracks_from(value, style.font_size, parent);
        }
        "grid-template-rows" => {
            style.grid_template_rows = grid_tracks_from(value, style.font_size, parent);
        }
        "grid-template-areas" => {
            style.grid_template_areas = grid_template_areas_from(value);
        }
        "grid-auto-flow" => {
            if let Value::Keyword(k) = value {
                style.grid_auto_flow = match k.as_str() {
                    "column" => GridAutoFlow::Column,
                    "row dense" | "dense row" | "dense" => GridAutoFlow::RowDense,
                    "column dense" | "dense column" => GridAutoFlow::ColumnDense,
                    _ => GridAutoFlow::Row,
                };
            } else if let Value::List(items) = value {
                let mut dense = false;
                let mut column = false;
                for it in items {
                    if let Value::Keyword(k) = it {
                        match k.as_str() {
                            "dense" => dense = true,
                            "column" => column = true,
                            _ => {}
                        }
                    }
                }
                style.grid_auto_flow = match (column, dense) {
                    (true, true) => GridAutoFlow::ColumnDense,
                    (true, false) => GridAutoFlow::Column,
                    (false, true) => GridAutoFlow::RowDense,
                    (false, false) => GridAutoFlow::Row,
                };
            }
        }
        "grid-area" => {
            // `grid-area: name` or shorthand `<row-start> / <col-start> / <row-end> / <col-end>`
            apply_grid_area(value, style);
        }
        "grid-column" => apply_grid_axis(value, style, true),
        "grid-row" => apply_grid_axis(value, style, false),
        "grid-column-start" => style.grid_placement.column_start = grid_line_from(value),
        "grid-column-end" => style.grid_placement.column_end = grid_line_from(value),
        "grid-row-start" => style.grid_placement.row_start = grid_line_from(value),
        "grid-row-end" => style.grid_placement.row_end = grid_line_from(value),
        "align-content" => {
            if let Value::Keyword(k) = value {
                style.align_content = match k.as_str() {
                    "flex-start" | "start" => AlignContent::FlexStart,
                    "flex-end" | "end" => AlignContent::FlexEnd,
                    "center" => AlignContent::Center,
                    "space-between" => AlignContent::SpaceBetween,
                    "space-around" => AlignContent::SpaceAround,
                    "space-evenly" => AlignContent::SpaceEvenly,
                    _ => AlignContent::Stretch,
                };
            }
        }
        "order" => {
            if let Value::Number(n) = value {
                style.order = *n as i32;
            }
        }
        "box-sizing" => {
            if let Value::Keyword(k) = value {
                style.box_sizing = match k.as_str() {
                    "border-box" => BoxSizing::BorderBox,
                    _ => BoxSizing::ContentBox,
                };
            }
        }
        "min-width" => style.min_width = max_min_from(value, style.font_size, parent),
        "max-width" => style.max_width = max_min_from(value, style.font_size, parent),
        "min-height" => style.min_height = max_min_from(value, style.font_size, parent),
        "max-height" => style.max_height = max_min_from(value, style.font_size, parent),
        "position" => {
            if let Value::Keyword(k) = value {
                style.position = match k.as_str() {
                    "relative" => Position::Relative,
                    "absolute" => Position::Absolute,
                    "fixed" => Position::Fixed,
                    "sticky" => Position::Sticky,
                    _ => Position::Static,
                };
            }
        }
        "top" => style.top = offset_from(value, style.font_size, parent),
        "right" => style.right = offset_from(value, style.font_size, parent),
        "bottom" => style.bottom = offset_from(value, style.font_size, parent),
        "left" => style.left = offset_from(value, style.font_size, parent),
        "z-index" => {
            if let Value::Number(n) = value {
                style.z_index = Some(*n as i32);
            } else if matches!(value, Value::Keyword(k) if k == "auto") {
                style.z_index = None;
            }
        }
        "text-decoration" | "text-decoration-line" => {
            if let Value::Keyword(k) = value {
                style.text_decoration = match k.as_str() {
                    "underline" => TextDecoration::Underline,
                    "line-through" => TextDecoration::LineThrough,
                    "overline" => TextDecoration::Overline,
                    "none" => TextDecoration::None,
                    _ => style.text_decoration,
                };
            }
        }
        "background-image" => {
            style.background_image = background_image_from(value);
        }
        "border-radius" => {
            if let Some(px) = length_to_px(value, style.font_size, parent) {
                style.border_radius = px.max(0.0);
            }
        }
        "opacity" => {
            if let Value::Number(n) = value {
                style.opacity = n.clamp(0.0, 1.0);
            } else if let Value::Percentage(p) = value {
                style.opacity = (p / 100.0).clamp(0.0, 1.0);
            }
        }
        "box-shadow" => {
            style.box_shadow = box_shadow_from(value, style.font_size, parent);
        }
        "transform" => {
            style.transform_translate = transform_translate_from(value, style.font_size, parent);
        }
        _ => {
            // Background shorthand can carry an image (URL or gradient) too;
            // we already grabbed the color above. Walk the value list once
            // more here so authors using `background: <color> url(...)` see
            // both applied.
            if property == "background" {
                if let Some(img) = background_image_from(value) {
                    style.background_image = Some(img);
                }
            }
        }
    }
}

fn background_image_from(v: &Value) -> Option<BackgroundImage> {
    match v {
        Value::Url(u) => Some(BackgroundImage::Url(u.clone())),
        Value::LinearGradient { angle_deg, stops } => Some(BackgroundImage::LinearGradient {
            angle_deg: *angle_deg,
            stops: stops.clone(),
        }),
        Value::List(items) => items.iter().find_map(background_image_from),
        Value::Keyword(k) if k == "none" => None,
        _ => None,
    }
}

fn box_shadow_from(
    value: &Value,
    em_base: f32,
    parent: Option<&ComputedStyle>,
) -> Option<BoxShadow> {
    let items = match value {
        Value::List(v) => v.clone(),
        Value::Keyword(k) if k == "none" => return None,
        other => vec![other.clone()],
    };
    let mut lengths = Vec::new();
    let mut color = Color::rgb(0, 0, 0);
    for it in items {
        match it {
            Value::Length(_, _) | Value::Number(0.0) => {
                if let Some(px) = length_to_px(&it, em_base, parent) {
                    lengths.push(px);
                }
            }
            Value::Color(c) => color = c,
            _ => {}
        }
    }
    let offset_x = lengths.first().copied().unwrap_or(0.0);
    let offset_y = lengths.get(1).copied().unwrap_or(0.0);
    let blur = lengths.get(2).copied().unwrap_or(0.0);
    Some(BoxShadow {
        offset_x,
        offset_y,
        blur,
        color,
    })
}

fn transform_translate_from(
    value: &Value,
    em_base: f32,
    parent: Option<&ComputedStyle>,
) -> Option<(f32, f32)> {
    // Look for the first `translate*(...)` function in the value (a value list
    // can mix multiple transforms; the toy honors the first translate).
    let candidates: Vec<&Value> = match value {
        Value::List(items) => items.iter().collect(),
        single => vec![single],
    };
    for v in candidates {
        if let Value::Function { name, args } = v {
            let n = name.as_str();
            if n == "translate" || n == "translatex" || n == "translatey" {
                let resolve = |v: &Value| length_to_px(v, em_base, parent);
                let a = args.first().and_then(resolve).unwrap_or(0.0);
                let b = args.get(1).and_then(resolve).unwrap_or(0.0);
                return Some(match n {
                    "translatex" => (a, 0.0),
                    "translatey" => (0.0, a),
                    _ => (a, b),
                });
            }
        }
    }
    None
}

fn offset_from(v: &Value, em: f32, parent: Option<&ComputedStyle>) -> Option<f32> {
    match v {
        Value::Keyword(k) if k == "auto" => None,
        _ => length_to_px(v, em, parent),
    }
}

fn apply_flex_shorthand(value: &Value, style: &mut ComputedStyle, parent: Option<&ComputedStyle>) {
    let em = style.font_size;
    let items: Vec<&Value> = match value {
        Value::List(v) => v.iter().collect(),
        single => vec![single],
    };
    if items.is_empty() {
        return;
    }
    // CSS spec: a single number → flex-grow; a single length/percent → flex-basis.
    // Two values: <grow> <shrink>, <grow> <basis>, or <grow> <basis>.
    // Three values: <grow> <shrink> <basis>.
    let mut grow = 0.0_f32;
    let mut shrink = 1.0_f32;
    let mut basis = Dimension::Auto;
    let mut grow_set = false;
    let mut shrink_set = false;
    let mut basis_set = false;
    for it in items {
        match it {
            Value::Number(n) => {
                if !grow_set {
                    grow = (*n).max(0.0);
                    grow_set = true;
                } else if !shrink_set {
                    shrink = (*n).max(0.0);
                    shrink_set = true;
                }
            }
            Value::Length(_, _) | Value::Percentage(_) => {
                basis = dimension_from(it, em, parent);
                basis_set = true;
            }
            Value::Keyword(k) if k == "auto" => {
                basis = Dimension::Auto;
                basis_set = true;
            }
            Value::Keyword(k) if k == "none" => {
                grow = 0.0;
                shrink = 0.0;
                basis = Dimension::Auto;
                grow_set = true;
                shrink_set = true;
                basis_set = true;
            }
            _ => {}
        }
    }
    style.flex_grow = grow;
    if shrink_set {
        style.flex_shrink = shrink;
    }
    let _ = basis_set;
    style.flex_basis = basis;
}

fn grid_tracks_from(value: &Value, em: f32, parent: Option<&ComputedStyle>) -> Vec<GridTrack> {
    let items: Vec<&Value> = match value {
        Value::List(v) => v.iter().collect(),
        Value::Keyword(k) if k == "none" => return Vec::new(),
        single => vec![single],
    };
    let mut tracks = Vec::with_capacity(items.len());
    for it in items {
        match it {
            Value::Function { name, args } if name == "repeat" => {
                // repeat(<count>, <track-list>) → expand by replicating the
                // pattern `count` times.
                let count = args
                    .first()
                    .and_then(|v| match v {
                        Value::Number(n) => Some((*n as i32).max(0) as usize),
                        Value::Keyword(k) if k == "auto-fit" || k == "auto-fill" => {
                            // We don't have a viewport context to honour
                            // auto-fit; expand to a single instance.
                            Some(1)
                        }
                        _ => None,
                    })
                    .unwrap_or(0);
                let mut pattern: Vec<GridTrack> = Vec::new();
                for a in args.iter().skip(1) {
                    let sub = grid_tracks_from(a, em, parent);
                    pattern.extend(sub);
                }
                if pattern.is_empty() {
                    continue;
                }
                for _ in 0..count {
                    tracks.extend(pattern.iter().cloned());
                }
            }
            Value::Keyword(k) if k == "auto" => tracks.push(GridTrack::Auto),
            Value::Percentage(p) => tracks.push(GridTrack::Percent(*p)),
            Value::Number(n) => tracks.push(GridTrack::Px(*n)),
            Value::Length(n, Unit::Fr) => tracks.push(GridTrack::Fr(*n)),
            Value::Length(_, _) => {
                if let Some(px) = length_to_px(it, em, parent) {
                    tracks.push(GridTrack::Px(px));
                } else {
                    tracks.push(GridTrack::Auto);
                }
            }
            _ => tracks.push(GridTrack::Auto),
        }
    }
    tracks
}

fn grid_template_areas_from(value: &Value) -> Vec<Vec<String>> {
    let items: Vec<&Value> = match value {
        Value::List(v) => v.iter().collect(),
        single => vec![single],
    };
    let mut rows = Vec::new();
    for it in items {
        if let Value::String(s) = it {
            let row: Vec<String> = s
                .split_ascii_whitespace()
                .map(|w| w.to_string())
                .collect();
            if !row.is_empty() {
                rows.push(row);
            }
        }
    }
    rows
}

fn grid_line_from(value: &Value) -> Option<GridLine> {
    match value {
        Value::Keyword(k) if k == "auto" => Some(GridLine::Auto),
        Value::Number(n) => Some(GridLine::Index(*n as i32)),
        Value::Keyword(k) => Some(GridLine::Name(k.clone())),
        Value::List(items) => {
            // `span 2` or `span name` etc.
            let mut span: Option<i32> = None;
            let mut saw_span = false;
            let mut name: Option<String> = None;
            for it in items {
                match it {
                    Value::Keyword(k) if k == "span" => saw_span = true,
                    Value::Number(n) => span = Some(*n as i32),
                    Value::Keyword(k) => name = Some(k.clone()),
                    _ => {}
                }
            }
            if saw_span {
                Some(GridLine::Span(span.unwrap_or(1)))
            } else if let Some(n) = name {
                Some(GridLine::Name(n))
            } else if let Some(n) = span {
                Some(GridLine::Index(n))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn apply_grid_axis(value: &Value, style: &mut ComputedStyle, is_column: bool) {
    // `grid-column: start / end` or just `start`.
    let (start, end) = match value {
        Value::List(items) => {
            // Look for a "/" separator. The parser may emit two list items
            // separated by Keyword("/") or just two values; we handle both.
            let mut slash_pos: Option<usize> = None;
            for (i, v) in items.iter().enumerate() {
                if matches!(v, Value::Keyword(k) if k == "/") {
                    slash_pos = Some(i);
                    break;
                }
            }
            if let Some(p) = slash_pos {
                let left: Value =
                    if p == 1 { items[0].clone() } else { Value::List(items[..p].to_vec()) };
                let right: Value = if items.len() == p + 2 {
                    items[p + 1].clone()
                } else {
                    Value::List(items[p + 1..].to_vec())
                };
                (grid_line_from(&left), grid_line_from(&right))
            } else if items.len() == 2 {
                (grid_line_from(&items[0]), grid_line_from(&items[1]))
            } else {
                (grid_line_from(value), None)
            }
        }
        single => (grid_line_from(single), None),
    };
    if is_column {
        style.grid_placement.column_start = start;
        style.grid_placement.column_end = end;
    } else {
        style.grid_placement.row_start = start;
        style.grid_placement.row_end = end;
    }
}

fn apply_grid_area(value: &Value, style: &mut ComputedStyle) {
    match value {
        Value::Keyword(k) => {
            // `grid-area: name`
            style.grid_placement.area = Some(k.clone());
        }
        Value::List(_) => {
            // Shorthand: <row-start> / <col-start> / <row-end> / <col-end>
            // The parser produces these as a flat list with "/" keyword
            // separators interspersed.
            let parts = split_on_slash(value);
            let lines: Vec<Option<GridLine>> =
                parts.iter().map(|p| grid_line_from(p)).collect();
            style.grid_placement.row_start = lines.first().cloned().unwrap_or(None);
            style.grid_placement.column_start = lines.get(1).cloned().unwrap_or(None);
            style.grid_placement.row_end = lines.get(2).cloned().unwrap_or(None);
            style.grid_placement.column_end = lines.get(3).cloned().unwrap_or(None);
        }
        _ => {}
    }
}

fn split_on_slash(value: &Value) -> Vec<Value> {
    let items: Vec<&Value> = match value {
        Value::List(v) => v.iter().collect(),
        single => return vec![single.clone()],
    };
    let mut out = Vec::new();
    let mut group: Vec<Value> = Vec::new();
    for v in items {
        if matches!(v, Value::Keyword(k) if k == "/") {
            if group.len() == 1 {
                out.push(group.pop().unwrap());
            } else if !group.is_empty() {
                out.push(Value::List(std::mem::take(&mut group)));
            }
        } else {
            group.push(v.clone());
        }
    }
    if group.len() == 1 {
        out.push(group.pop().unwrap());
    } else if !group.is_empty() {
        out.push(Value::List(group));
    }
    out
}

fn max_min_from(value: &Value, em: f32, parent: Option<&ComputedStyle>) -> Option<f32> {
    match value {
        Value::Keyword(k) if k == "none" || k == "auto" => None,
        _ => length_to_px(value, em, parent),
    }
}

fn content_from(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Keyword(k) if k == "none" || k == "normal" => None,
        Value::List(items) => {
            let mut acc = String::new();
            for it in items {
                if let Value::String(s) = it {
                    acc.push_str(s);
                }
            }
            if acc.is_empty() {
                None
            } else {
                Some(acc)
            }
        }
        _ => None,
    }
}

fn display_from(v: &Value) -> Option<Display> {
    let Value::Keyword(k) = v else { return None };
    Some(match k.as_str() {
        "block" => Display::Block,
        "inline" => Display::Inline,
        "inline-block" => Display::InlineBlock,
        "list-item" => Display::ListItem,
        "flex" => Display::Flex,
        "inline-flex" => Display::InlineFlex,
        "grid" => Display::Grid,
        "inline-grid" => Display::InlineGrid,
        "none" => Display::None,
        _ => return None,
    })
}

fn color_from(v: &Value) -> Option<Color> {
    match v {
        Value::Color(c) => Some(*c),
        _ => None,
    }
}

fn color_from_any(v: &Value) -> Option<Color> {
    match v {
        Value::Color(c) => Some(*c),
        Value::List(items) => items.iter().find_map(color_from_any),
        _ => None,
    }
}

fn font_weight_from(v: &Value) -> Option<u16> {
    match v {
        Value::Number(n) => Some((*n as u16).clamp(100, 900)),
        Value::Keyword(k) => Some(match k.as_str() {
            "normal" => 400,
            "bold" => 700,
            "lighter" => 300,
            "bolder" => 700,
            _ => k.parse::<u16>().ok()?.clamp(100, 900),
        }),
        _ => None,
    }
}

fn font_family_from(v: &Value) -> Vec<String> {
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::Keyword(k) => Some(k.clone()),
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }
    match v {
        Value::List(items) => items.iter().filter_map(one).collect(),
        _ => one(v).into_iter().collect(),
    }
}

fn font_size_from(v: &Value, em_base: f32) -> Option<f32> {
    match v {
        Value::Length(n, u) => Some(length_n_unit_to_px(*n, *u, em_base, em_base)),
        Value::Percentage(p) => Some(em_base * p / 100.0),
        Value::Number(0.0) => Some(0.0),
        Value::Keyword(k) => Some(match k.as_str() {
            "xx-small" => 9.0,
            "x-small" => 10.0,
            "small" => 13.0,
            "medium" => 16.0,
            "large" => 18.0,
            "x-large" => 24.0,
            "xx-large" => 32.0,
            "smaller" => em_base * 0.83,
            "larger" => em_base * 1.2,
            _ => return None,
        }),
        _ => None,
    }
}

fn length_to_px(v: &Value, em_base: f32, parent: Option<&ComputedStyle>) -> Option<f32> {
    let root_em = parent.map(|p| p.font_size).unwrap_or(ComputedStyle::ROOT_FONT_SIZE);
    match v {
        // vw/vh need viewport size, which we don't have at cascade time — defer
        // to layout. `fr` is a grid-track sizer, not a real length.
        Value::Length(_, Unit::Vw | Unit::Vh | Unit::Fr) => None,
        Value::Length(n, u) => Some(length_n_unit_to_px(*n, *u, em_base, root_em)),
        Value::Number(0.0) => Some(0.0),
        Value::Percentage(_) => None,
        _ => None,
    }
}

fn length_n_unit_to_px(n: f32, u: Unit, em_base: f32, root_em: f32) -> f32 {
    match u {
        Unit::Px => n,
        Unit::Em => n * em_base,
        Unit::Rem => n * root_em,
        Unit::Pt => n * 4.0 / 3.0,
        Unit::Pc => n * 16.0,
        Unit::Cm => n * 96.0 / 2.54,
        Unit::Mm => n * 96.0 / 25.4,
        Unit::In => n * 96.0,
        Unit::Vw | Unit::Vh | Unit::Fr => 0.0,
    }
}

fn dimension_from(v: &Value, em_base: f32, parent: Option<&ComputedStyle>) -> Dimension {
    match v {
        Value::Keyword(k) if k == "auto" => Dimension::Auto,
        Value::Percentage(p) => Dimension::Percent(*p),
        _ => match length_to_px(v, em_base, parent) {
            Some(px) => Dimension::Length(px),
            None => Dimension::Auto,
        },
    }
}

fn apply_side(value: &Value, slot: &mut f32, em_base: f32, parent: Option<&ComputedStyle>) {
    if let Some(px) = length_to_px(value, em_base, parent) {
        *slot = px;
    } else if matches!(value, Value::Keyword(k) if k == "auto") {
        *slot = 0.0;
    }
}

fn apply_box_shorthand(
    value: &Value,
    sides: &mut BoxSides,
    em_base: f32,
    parent: Option<&ComputedStyle>,
) {
    let list = match value {
        Value::List(v) => v.clone(),
        other => vec![other.clone()],
    };
    let px: Vec<f32> = list
        .iter()
        .map(|v| length_to_px(v, em_base, parent).unwrap_or(0.0))
        .collect();
    let (t, r, b, l) = match px.as_slice() {
        [a] => (*a, *a, *a, *a),
        [a, b] => (*a, *b, *a, *b),
        [a, b, c] => (*a, *b, *c, *b),
        [a, b, c, d, ..] => (*a, *b, *c, *d),
        _ => return,
    };
    *sides = BoxSides { top: t, right: r, bottom: b, left: l };
}

fn apply_border_shorthand(value: &Value, style: &mut ComputedStyle) {
    let items = match value {
        Value::List(v) => v.clone(),
        other => vec![other.clone()],
    };
    for item in items {
        match item {
            Value::Length(n, u) => {
                let px = length_n_unit_to_px(n, u, style.font_size, style.font_size);
                style.border_width = BoxSides::uniform(px);
            }
            Value::Number(0.0) => {
                style.border_width = BoxSides::uniform(0.0);
            }
            Value::Color(c) => style.border_color = c,
            Value::Keyword(k) => {
                style.border_style = match k.as_str() {
                    "solid" => BorderStyle::Solid,
                    "dashed" => BorderStyle::Dashed,
                    "dotted" => BorderStyle::Dotted,
                    "none" => BorderStyle::None,
                    _ => style.border_style,
                };
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::parser;
    use crate::html;

    fn style_for(dom: &crate::dom::Dom, sheet: &Stylesheet, tag: &str) -> ComputedStyle {
        let tree = StyleTree::compute(dom, &[sheet]);
        find_styled(dom, tag, &tree).expect("element not found")
    }

    fn find_styled(dom: &crate::dom::Dom, tag: &str, tree: &StyleTree) -> Option<ComputedStyle> {
        fn walk(
            dom: &crate::dom::Dom,
            id: NodeId,
            tag: &str,
            tree: &StyleTree,
        ) -> Option<ComputedStyle> {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    return Some(tree.get(id).clone());
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                if let Some(s) = walk(dom, c, tag, tree) {
                    return Some(s);
                }
            }
            None
        }
        walk(dom, dom.document(), tag, tree)
    }

    #[test]
    fn tag_selector_applies() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { color: red; }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn id_beats_class() {
        let dom = html::parse(r#"<p id="x" class="hl">hi</p>"#);
        let sheet = parser::parse(".hl { color: blue; } #x { color: green; }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(0, 128, 0));
    }

    #[test]
    fn inline_beats_everything() {
        let dom = html::parse(r#"<p id="x" style="color: black">hi</p>"#);
        let sheet = parser::parse("#x { color: red; }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::BLACK);
    }

    #[test]
    fn color_inherits() {
        let dom = html::parse("<div><p>hi</p></div>");
        let sheet = parser::parse("div { color: red; }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn descendant_combinator() {
        let dom = html::parse("<div><span><b>hi</b></span></div>");
        let sheet = parser::parse("div b { color: blue; }");
        assert_eq!(style_for(&dom, &sheet, "b").color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn child_combinator_strict() {
        let dom = html::parse("<div><span><b>hi</b></span></div>");
        let sheet = parser::parse("div > b { color: blue; }");
        assert_eq!(style_for(&dom, &sheet, "b").color, Color::BLACK);
    }

    #[test]
    fn em_resolves_to_px() {
        let dom = html::parse("<div><p>hi</p></div>");
        let sheet = parser::parse("div { font-size: 20px; } p { font-size: 1.5em; }");
        assert!((style_for(&dom, &sheet, "p").font_size - 30.0).abs() < 0.001);
    }

    #[test]
    fn margin_shorthand() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { margin: 1px 2px 3px 4px; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.margin.top, 1.0);
        assert_eq!(s.margin.right, 2.0);
        assert_eq!(s.margin.bottom, 3.0);
        assert_eq!(s.margin.left, 4.0);
    }

    #[test]
    fn attribute_exists_selector() {
        let dom = html::parse(r#"<input type="text"><input>"#);
        let sheet = parser::parse("input[type] { color: red; }");
        let tree = StyleTree::compute(&dom, &[&sheet]);
        // Find the two input elements
        fn collect(dom: &Dom, id: NodeId, tag: &str, out: &mut Vec<NodeId>) {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    out.push(id);
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                collect(dom, c, tag, out);
            }
        }
        let mut inputs = Vec::new();
        collect(&dom, dom.document(), "input", &mut inputs);
        assert_eq!(inputs.len(), 2);
        // First has type, gets red
        assert_eq!(tree.get(inputs[0]).color, Color::rgb(255, 0, 0));
        // Second has no type, stays black
        assert_eq!(tree.get(inputs[1]).color, Color::BLACK);
    }

    #[test]
    fn attribute_equals_selector() {
        let dom = html::parse(r#"<input type="text"><input type="number">"#);
        let sheet = parser::parse(r#"input[type="text"] { color: red; }"#);
        let tree = StyleTree::compute(&dom, &[&sheet]);
        fn collect(dom: &Dom, id: NodeId, tag: &str, out: &mut Vec<NodeId>) {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    out.push(id);
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                collect(dom, c, tag, out);
            }
        }
        let mut inputs = Vec::new();
        collect(&dom, dom.document(), "input", &mut inputs);
        assert_eq!(tree.get(inputs[0]).color, Color::rgb(255, 0, 0));
        assert_eq!(tree.get(inputs[1]).color, Color::BLACK);
    }

    #[test]
    fn attribute_starts_with() {
        let dom = html::parse(r#"<a href="https://x"></a><a href="http://x"></a>"#);
        let sheet = parser::parse(r#"a[href^="https"] { color: green; }"#);
        let tree = StyleTree::compute(&dom, &[&sheet]);
        fn collect(dom: &Dom, id: NodeId, tag: &str, out: &mut Vec<NodeId>) {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    out.push(id);
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                collect(dom, c, tag, out);
            }
        }
        let mut links = Vec::new();
        collect(&dom, dom.document(), "a", &mut links);
        assert_eq!(tree.get(links[0]).color, Color::rgb(0, 128, 0));
        // No UA stylesheet here; the second <a> falls through to initial (black).
        assert_eq!(tree.get(links[1]).color, Color::BLACK);
    }

    #[test]
    fn hover_pseudo_class_does_not_match_yet() {
        let dom = html::parse("<a>hi</a>");
        let sheet = parser::parse("a:hover { color: red; }");
        let s = style_for(&dom, &sheet, "a");
        // :hover never matches in phase 3, so color stays at initial (black).
        assert_eq!(s.color, Color::BLACK);
    }

    #[test]
    fn pseudo_element_rule_does_not_apply_to_real_node() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p::before { color: red; }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::BLACK);
    }

    #[test]
    fn css_variable_simple() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse(":root, p { --main: #ff0000; } p { color: var(--main); }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn css_variable_inherits() {
        let dom = html::parse("<div><p>hi</p></div>");
        let sheet = parser::parse("div { --main: #00ff00; } p { color: var(--main); }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(0, 255, 0));
    }

    #[test]
    fn css_variable_fallback() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { color: var(--missing, blue); }");
        assert_eq!(style_for(&dom, &sheet, "p").color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn calc_arithmetic() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { font-size: calc(10px + 6px); }");
        assert!((style_for(&dom, &sheet, "p").font_size - 16.0).abs() < 0.001);
    }

    #[test]
    fn calc_with_var() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { --gap: 4px; padding: calc(var(--gap) * 2); }");
        let s = style_for(&dom, &sheet, "p");
        assert!((s.padding.top - 8.0).abs() < 0.001);
    }
}
