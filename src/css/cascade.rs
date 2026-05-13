//! Selector matching, specificity, and cascade resolution.
//!
//! For every DOM element we:
//!  1. find every rule in every stylesheet whose selector list matches
//!  2. sort by (origin, specificity, source order) — later wins
//!  3. apply declarations in that order onto a style inherited from the parent
//!  4. apply the element's inline `style=""` attribute last (highest priority)
//!
//! The result is a `StyleTree`: a `Vec<ComputedStyle>` indexed by `NodeId`.

use crate::css::parser::parse_inline_declarations;
use crate::css::types::{
    BorderStyle, BoxSides, Color, Combinator, ComputedStyle, Declaration, Dimension, Display,
    FontStyle, Rule, Selector, SimpleSelector, Stylesheet, TextAlign, Unit, Value, WhiteSpace,
};
use crate::dom::{Dom, NodeId, NodeKind};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub struct Specificity(pub u8, pub u8, pub u8);

pub fn compute_specificity(sel: &Selector) -> Specificity {
    let mut ids = 0u8;
    let mut classes = 0u8;
    let mut tags = 0u8;
    for compound in &sel.compounds {
        if compound.id.is_some() {
            ids = ids.saturating_add(1);
        }
        classes = classes.saturating_add(compound.classes.len() as u8);
        if compound.tag.is_some() {
            tags = tags.saturating_add(1);
        }
    }
    Specificity(ids, classes, tags)
}

pub fn selector_matches(sel: &Selector, dom: &Dom, node: NodeId) -> bool {
    if sel.compounds.is_empty() {
        return false;
    }
    let last = sel.compounds.len() - 1;
    if !matches_simple(&sel.compounds[last], dom, node) {
        return false;
    }
    let mut current = node;
    for i in (0..last).rev() {
        let combinator = sel.combinators[i];
        let target = &sel.compounds[i];
        let found = match combinator {
            Combinator::Descendant => walk_ancestors(dom, current, |id| matches_simple(target, dom, id)),
            Combinator::Child => dom.node(current).parent.and_then(|p| {
                if matches_simple(target, dom, p) {
                    Some(p)
                } else {
                    None
                }
            }),
            Combinator::AdjacentSibling => dom.node(current).prev_sibling.and_then(|p| {
                if matches_simple(target, dom, p) {
                    Some(p)
                } else {
                    None
                }
            }),
            Combinator::GeneralSibling => walk_prev_siblings(dom, current, |id| matches_simple(target, dom, id)),
        };
        match found {
            Some(id) => current = id,
            None => return false,
        }
    }
    true
}

fn walk_ancestors<F: Fn(NodeId) -> bool>(dom: &Dom, from: NodeId, pred: F) -> Option<NodeId> {
    let mut p = dom.node(from).parent;
    while let Some(id) = p {
        if pred(id) {
            return Some(id);
        }
        p = dom.node(id).parent;
    }
    None
}

fn walk_prev_siblings<F: Fn(NodeId) -> bool>(dom: &Dom, from: NodeId, pred: F) -> Option<NodeId> {
    let mut s = dom.node(from).prev_sibling;
    while let Some(id) = s {
        if pred(id) {
            return Some(id);
        }
        s = dom.node(id).prev_sibling;
    }
    None
}

fn matches_simple(sel: &SimpleSelector, dom: &Dom, node: NodeId) -> bool {
    let (tag, attrs) = match &dom.node(node).kind {
        NodeKind::Element { tag, attrs } => (tag, attrs),
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
    true
}

// ---------------- Style tree ----------------

pub struct StyleTree {
    pub styles: Vec<ComputedStyle>, // indexed by NodeId.index()
}

impl StyleTree {
    pub fn compute(dom: &Dom, sheets: &[&Stylesheet]) -> Self {
        // Pre-fill with initial values; only elements actually get computed,
        // but text/comment nodes can read the initial style harmlessly.
        let count = highest_node_id(dom).index() + 1;
        let mut styles = vec![ComputedStyle::initial(); count];
        compute_recursive(dom, dom.document(), sheets, None, &mut styles);
        Self { styles }
    }

    #[allow(dead_code)] // used by tests now, by layout in phase 4
    pub fn get(&self, id: NodeId) -> &ComputedStyle {
        &self.styles[id.index()]
    }
}

fn highest_node_id(dom: &Dom) -> NodeId {
    // The arena guarantees ids are contiguous; the highest id is the last one
    // allocated. We don't have a direct accessor, so walk from the document.
    // Tree walk is O(n) once, which is fine.
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

fn compute_recursive(
    dom: &Dom,
    node: NodeId,
    sheets: &[&Stylesheet],
    parent_style: Option<&ComputedStyle>,
    out: &mut [ComputedStyle],
) {
    let style = compute_one(dom, node, sheets, parent_style);
    out[node.index()] = style;
    let style_for_children = out[node.index()].clone();
    let kids: Vec<NodeId> = dom.children(node).collect();
    for child in kids {
        compute_recursive(dom, child, sheets, Some(&style_for_children), out);
    }
}

fn compute_one(
    dom: &Dom,
    node: NodeId,
    sheets: &[&Stylesheet],
    parent_style: Option<&ComputedStyle>,
) -> ComputedStyle {
    let mut style = match parent_style {
        Some(p) => ComputedStyle::inherit_from(p),
        None => ComputedStyle::initial(),
    };

    // Only elements are styled; text/comments inherit only.
    let attrs = match &dom.node(node).kind {
        NodeKind::Element { attrs, .. } => attrs.clone(),
        _ => return style,
    };

    // Collect matching rules across all sheets, with specificity + source order.
    // origin = sheet_index (0 = UA, higher = author); we already pass UA first
    // so simple sheet index ordering yields correct origin behavior.
    let mut matched: Vec<(Specificity, usize, &Rule)> = Vec::new();
    let mut order = 0usize;
    for sheet in sheets {
        for rule in &sheet.rules {
            order += 1;
            for sel in &rule.selectors {
                if selector_matches(sel, dom, node) {
                    matched.push((compute_specificity(sel), order, rule));
                    break;
                }
            }
        }
    }
    matched.sort_by_key(|(spec, ord, _)| (*spec, *ord));

    for (_, _, rule) in &matched {
        for decl in &rule.declarations {
            apply_declaration(&mut style, decl, parent_style);
        }
    }

    // Inline `style=""` wins over stylesheet rules (CSS rules say inline style
    // has specificity 1,0,0,0 which beats any author selector).
    for (k, v) in &attrs {
        if k == "style" {
            for decl in parse_inline_declarations(v) {
                apply_declaration(&mut style, &decl, parent_style);
            }
        }
    }

    style
}

// ---------------- apply_declaration ----------------

fn apply_declaration(style: &mut ComputedStyle, decl: &Declaration, parent: Option<&ComputedStyle>) {
    let property = decl.property.as_str();
    let value = &decl.value;

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
        "line-height" => {
            // Accept unitless number, length, or percentage.
            match value {
                Value::Number(n) => style.line_height = *n,
                Value::Percentage(p) => style.line_height = p / 100.0,
                Value::Length(_, _) => {
                    if let Some(px) = length_to_px(value, style.font_size, parent) {
                        style.line_height = px / style.font_size;
                    }
                }
                _ => {}
            }
        }
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
        "border" => {
            // Toy: parse a "1px solid #ccc"-style shorthand by scanning the list.
            apply_border_shorthand(value, style);
        }
        "width" => style.width = dimension_from(value, style.font_size, parent),
        "height" => style.height = dimension_from(value, style.font_size, parent),
        _ => {}
    }
}

fn display_from(v: &Value) -> Option<Display> {
    let Value::Keyword(k) = v else { return None };
    Some(match k.as_str() {
        "block" => Display::Block,
        "inline" => Display::Inline,
        "inline-block" => Display::InlineBlock,
        "list-item" => Display::ListItem,
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
        Value::List(items) => items.iter().find_map(color_from),
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
        Value::Length(n, u) => Some(length_n_unit_to_px(*n, *u, em_base, root_em)),
        Value::Number(0.0) => Some(0.0),
        Value::Percentage(_) => None, // percentages resolve at layout time
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
        Unit::Vw | Unit::Vh => 0.0,
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
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn class_overrides_tag() {
        let dom = html::parse(r#"<p class="hl">hi</p>"#);
        let sheet = parser::parse("p { color: red; } .hl { color: blue; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn id_beats_class() {
        let dom = html::parse(r#"<p id="x" class="hl">hi</p>"#);
        let sheet = parser::parse(".hl { color: blue; } #x { color: green; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.color, Color::rgb(0, 128, 0));
    }

    #[test]
    fn inline_beats_everything() {
        let dom = html::parse(r#"<p id="x" style="color: black">hi</p>"#);
        let sheet = parser::parse("#x { color: red; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.color, Color::BLACK);
    }

    #[test]
    fn color_inherits() {
        let dom = html::parse("<div><p>hi</p></div>");
        let sheet = parser::parse("div { color: red; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.color, Color::rgb(255, 0, 0));
    }

    #[test]
    fn descendant_combinator_matches() {
        let dom = html::parse("<div><span><b>hi</b></span></div>");
        let sheet = parser::parse("div b { color: blue; }");
        let s = style_for(&dom, &sheet, "b");
        assert_eq!(s.color, Color::rgb(0, 0, 255));
    }

    #[test]
    fn child_combinator_strict() {
        let dom = html::parse("<div><span><b>hi</b></span></div>");
        let sheet = parser::parse("div > b { color: blue; }");
        let s = style_for(&dom, &sheet, "b");
        // b is grandchild of div, not direct child, so should NOT match.
        assert_eq!(s.color, Color::BLACK);
    }

    #[test]
    fn em_resolves_to_px() {
        let dom = html::parse("<div><p>hi</p></div>");
        let sheet = parser::parse("div { font-size: 20px; } p { font-size: 1.5em; }");
        let s = style_for(&dom, &sheet, "p");
        assert!((s.font_size - 30.0).abs() < 0.001);
    }

    #[test]
    fn margin_shorthand_four_values() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { margin: 1px 2px 3px 4px; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.margin.top, 1.0);
        assert_eq!(s.margin.right, 2.0);
        assert_eq!(s.margin.bottom, 3.0);
        assert_eq!(s.margin.left, 4.0);
    }

    #[test]
    fn margin_shorthand_two_values() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { margin: 5px 10px; }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.margin.top, 5.0);
        assert_eq!(s.margin.right, 10.0);
        assert_eq!(s.margin.bottom, 5.0);
        assert_eq!(s.margin.left, 10.0);
    }
}
