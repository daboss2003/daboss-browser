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
    AlignContent, AlignItems, AnimationRule, AttributeOp, AttributeSelector, BackgroundImage,
    BorderStyle, BoxShadow, BoxSides, BoxSizing, CalcExpr, Color, Combinator, ComputedStyle,
    Declaration, Dimension, Direction, Display, FilterFunction, FlexDirection, FlexWrap,
    FontStyle, GridAutoFlow, GridLine, GridTrack, JustifyContent, Overflow, Position,
    PseudoClass, Rule, Selector, SimpleSelector, Stylesheet, TableLayout, TextAlign,
    TextDecoration, TextOverflow, TimingFunction, Transform2D, TransitionRule, Unit, Value,
    WhiteSpace,
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
        for pc in &compound.pseudo_classes {
            match pc {
                // `:where(...)` contributes zero specificity per spec.
                PseudoClass::Where(_) | PseudoClass::Has(_) => {}
                // `:is(...)` / `:not(...)` take the max specificity of
                // their inner selectors. We approximate with the highest
                // (classes, tags) seen and roll it into our triple.
                PseudoClass::Is(inner) | PseudoClass::Not(inner) => {
                    let inner_spec = inner
                        .iter()
                        .map(compute_specificity)
                        .max()
                        .unwrap_or(Specificity(0, 0, 0));
                    ids = ids.saturating_add(inner_spec.0);
                    classes = classes.saturating_add(inner_spec.1);
                    tags = tags.saturating_add(inner_spec.2);
                }
                _ => {
                    classes = classes.saturating_add(1);
                }
            }
        }
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
        if !match_pseudo_class_v(pc, dom, node, interaction) {
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

fn match_pseudo_class_v(
    pc: &PseudoClass,
    dom: &Dom,
    node: NodeId,
    interaction: &InteractionState,
) -> bool {
    match pc {
        PseudoClass::Name(name) => match name.as_str() {
            "root" => dom.node(node).parent == Some(dom.document()),
            "first-child" => is_first_element_child(dom, node),
            "last-child" => is_last_element_child(dom, node),
            "only-child" => {
                is_first_element_child(dom, node) && is_last_element_child(dom, node)
            }
            "first-of-type" => type_index_from_start(dom, node) == 1,
            "last-of-type" => type_index_from_end(dom, node) == 1,
            "only-of-type" => {
                type_index_from_start(dom, node) == 1 && type_index_from_end(dom, node) == 1
            }
            "empty" => dom
                .children(node)
                .all(|c| matches!(dom.node(c).kind, NodeKind::Comment(_))),
            "hover" => interaction.hover_chain.contains(&node),
            "focus" | "focus-visible" | "focus-within" => {
                interaction.focus_chain.contains(&node)
            }
            _ => false,
        },
        PseudoClass::Not(inner) => !inner.iter().any(|s| {
            selector_matches_pseudo(s, dom, node, None, interaction)
        }),
        PseudoClass::Is(inner) | PseudoClass::Where(inner) => inner.iter().any(|s| {
            selector_matches_pseudo(s, dom, node, None, interaction)
        }),
        PseudoClass::NthChild(nth) => {
            nth.matches(element_index_from_start(dom, node))
        }
        PseudoClass::NthLastChild(nth) => {
            nth.matches(element_index_from_end(dom, node))
        }
        PseudoClass::NthOfType(nth) => nth.matches(type_index_from_start(dom, node)),
        PseudoClass::NthLastOfType(nth) => nth.matches(type_index_from_end(dom, node)),
        PseudoClass::Has(inner) => {
            // `:has(...)` matches when ANY descendant matches the
            // inner selector list. The toy walks the whole subtree;
            // a real engine indexes selectors for fast rejection.
            // Limit to direct descendants for the simple case (most
            // common: `:has(> .child)`); we just walk the full
            // descendant tree.
            has_descendant_match(dom, node, inner, interaction)
        }
    }
}

fn has_descendant_match(
    dom: &Dom,
    root: NodeId,
    selectors: &[Selector],
    interaction: &InteractionState,
) -> bool {
    for child in dom.children(root).collect::<Vec<_>>() {
        if selectors.iter().any(|s| {
            selector_matches_pseudo(s, dom, child, None, interaction)
        }) {
            return true;
        }
        if has_descendant_match(dom, child, selectors, interaction) {
            return true;
        }
    }
    false
}

fn is_first_element_child(dom: &Dom, node: NodeId) -> bool {
    let mut prev = dom.node(node).prev_sibling;
    while let Some(p) = prev {
        if matches!(dom.node(p).kind, NodeKind::Element { .. }) {
            return false;
        }
        prev = dom.node(p).prev_sibling;
    }
    true
}

fn is_last_element_child(dom: &Dom, node: NodeId) -> bool {
    let mut next = dom.node(node).next_sibling;
    while let Some(n) = next {
        if matches!(dom.node(n).kind, NodeKind::Element { .. }) {
            return false;
        }
        next = dom.node(n).next_sibling;
    }
    true
}

fn element_index_from_start(dom: &Dom, node: NodeId) -> i32 {
    let parent = match dom.node(node).parent {
        Some(p) => p,
        None => return 0,
    };
    let mut idx = 0;
    for sib in dom.children(parent) {
        if matches!(dom.node(sib).kind, NodeKind::Element { .. }) {
            idx += 1;
            if sib == node {
                return idx;
            }
        }
    }
    0
}

fn element_index_from_end(dom: &Dom, node: NodeId) -> i32 {
    let parent = match dom.node(node).parent {
        Some(p) => p,
        None => return 0,
    };
    let kids: Vec<NodeId> = dom
        .children(parent)
        .filter(|c| matches!(dom.node(*c).kind, NodeKind::Element { .. }))
        .collect();
    let position = kids.iter().position(|&c| c == node);
    match position {
        Some(p) => (kids.len() - p) as i32,
        None => 0,
    }
}

fn type_index_from_start(dom: &Dom, node: NodeId) -> i32 {
    let parent = match dom.node(node).parent {
        Some(p) => p,
        None => return 0,
    };
    let want_tag = match &dom.node(node).kind {
        NodeKind::Element { tag, .. } => tag,
        _ => return 0,
    };
    let mut idx = 0;
    for sib in dom.children(parent) {
        if let NodeKind::Element { tag, .. } = &dom.node(sib).kind {
            if tag == want_tag {
                idx += 1;
                if sib == node {
                    return idx;
                }
            }
        }
    }
    0
}

fn type_index_from_end(dom: &Dom, node: NodeId) -> i32 {
    let parent = match dom.node(node).parent {
        Some(p) => p,
        None => return 0,
    };
    let want_tag = match &dom.node(node).kind {
        NodeKind::Element { tag, .. } => tag,
        _ => return 0,
    };
    let mut same_type: Vec<NodeId> = Vec::new();
    for sib in dom.children(parent) {
        if let NodeKind::Element { tag, .. } = &dom.node(sib).kind {
            if tag == want_tag {
                same_type.push(sib);
            }
        }
    }
    let pos = same_type.iter().position(|&c| c == node);
    match pos {
        Some(p) => (same_type.len() - p) as i32,
        None => 0,
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

    /// Mutable access for the animation engine to write interpolated
    /// values back per frame. Auto-grows the underlying vec when
    /// `id` is beyond the current length (e.g. nodes inserted by JS
    /// after the initial cascade).
    pub fn get_mut(&mut self, id: NodeId) -> &mut ComputedStyle {
        let idx = id.index();
        if idx >= self.styles.len() {
            self.styles
                .resize_with(idx + 1, ComputedStyle::initial);
        }
        &mut self.styles[idx]
    }

    pub fn before_style(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.before.get(id.index()).and_then(|s| s.as_ref())
    }

    pub fn after_style(&self, id: NodeId) -> Option<&ComputedStyle> {
        self.after.get(id.index()).and_then(|s| s.as_ref())
    }
}

/// Synthetic tag used by `element.attachShadow()` to inject a
/// child that owns the shadow DOM subtree. Kept in sync with
/// `js::shadow_dom::SHADOW_TAG`.
const SHADOW_ROOT_TAG: &str = "__shadow_root__";

/// Walk `node`'s ancestor chain and return the nearest shadow root
/// (the `__shadow_root__` synthetic element). Returns `None` when
/// `node` is part of the regular light tree.
fn shadow_root_of(dom: &Dom, node: NodeId) -> Option<NodeId> {
    let mut cursor = dom.node(node).parent;
    while let Some(p) = cursor {
        if let NodeKind::Element { tag, .. } = &dom.node(p).kind {
            if tag == SHADOW_ROOT_TAG {
                return Some(p);
            }
        }
        cursor = dom.node(p).parent;
    }
    // The node could *itself* be the shadow root.
    if let NodeKind::Element { tag, .. } = &dom.node(node).kind {
        if tag == SHADOW_ROOT_TAG {
            return Some(node);
        }
    }
    None
}

/// Decide whether a stylesheet's rules are allowed to match
/// against `node`, given the shadow-root context.
///
/// * UA rules always match.
/// * Page-level rules (scope=None) match only when `node` is
///   NOT inside any shadow tree.
/// * Shadow-scoped rules (scope=Some(N)) match only when `node`
///   is inside the shadow tree rooted at N.
fn sheet_scope_allows(
    sheet: &Stylesheet,
    _dom: &Dom,
    _node: NodeId,
    node_shadow_root: Option<NodeId>,
) -> bool {
    if sheet.is_ua {
        return true;
    }
    match sheet.scope {
        None => node_shadow_root.is_none(),
        Some(scope_root) => node_shadow_root == Some(scope_root),
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

    let node_shadow_root = shadow_root_of(dom, node);
    let mut matched: Vec<(Specificity, usize, &Rule)> = Vec::new();
    let mut order = 0usize;
    for sheet in sheets {
        if !sheet_scope_allows(sheet, dom, node, node_shadow_root) {
            order += sheet.rules.len();
            continue;
        }
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

    // Honour the HTML `dir` attribute: `dir="rtl"` flips the
    // computed direction before any CSS rules apply (CSS author
    // rules can still override with `direction: ...`).
    if let Some((_, v)) = attrs.iter().find(|(k, _)| k.eq_ignore_ascii_case("dir")) {
        match v.to_ascii_lowercase().as_str() {
            "rtl" => {
                style.direction = crate::css::types::Direction::Rtl;
                // Default text-align should follow direction.
                if matches!(style.text_align, TextAlign::Left) {
                    style.text_align = TextAlign::Right;
                }
            }
            "ltr" => {
                style.direction = crate::css::types::Direction::Ltr;
            }
            _ => {} // "auto" — leave as inherited
        }
    }

    // Collect matches with specificity + source order. Shadow DOM
    // scoping: a stylesheet's `scope` field decides whether its
    // rules can match `node`. UA rules ignore scope (they're the
    // default block/inline/etc.).
    let node_shadow_root = shadow_root_of(dom, node);
    let mut matched: Vec<(Specificity, usize, &Rule)> = Vec::new();
    let mut order = 0usize;
    for sheet in sheets {
        if !sheet_scope_allows(sheet, dom, node, node_shadow_root) {
            order += sheet.rules.len();
            continue;
        }
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
                    "start" => TextAlign::Start,
                    "end" => TextAlign::End,
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
                    "pre-wrap" => WhiteSpace::PreWrap,
                    "pre-line" => WhiteSpace::PreLine,
                    "break-spaces" => WhiteSpace::BreakSpaces,
                    _ => WhiteSpace::Normal,
                };
            }
        }
        "margin" => apply_box_shorthand(value, &mut style.margin, style.font_size, parent),
        "margin-top" => apply_side(value, &mut style.margin.top, style.font_size, parent),
        "margin-right" => apply_side(value, &mut style.margin.right, style.font_size, parent),
        "margin-bottom" => apply_side(value, &mut style.margin.bottom, style.font_size, parent),
        "margin-left" => apply_side(value, &mut style.margin.left, style.font_size, parent),
        // Logical properties — under our LTR horizontal-tb assumption,
        // `inline-start` = left, `inline-end` = right, `block-start` =
        // top, `block-end` = bottom. Real browsers swap these based on
        // `writing-mode` and `direction`; we don't read those yet.
        "margin-inline-start" => {
            apply_side(value, &mut style.margin.left, style.font_size, parent)
        }
        "margin-inline-end" => {
            apply_side(value, &mut style.margin.right, style.font_size, parent)
        }
        "margin-block-start" => {
            apply_side(value, &mut style.margin.top, style.font_size, parent)
        }
        "margin-block-end" => {
            apply_side(value, &mut style.margin.bottom, style.font_size, parent)
        }
        "margin-inline" => {
            apply_side(value, &mut style.margin.left, style.font_size, parent);
            apply_side(value, &mut style.margin.right, style.font_size, parent);
        }
        "margin-block" => {
            apply_side(value, &mut style.margin.top, style.font_size, parent);
            apply_side(value, &mut style.margin.bottom, style.font_size, parent);
        }
        "padding" => apply_box_shorthand(value, &mut style.padding, style.font_size, parent),
        "padding-top" => apply_side(value, &mut style.padding.top, style.font_size, parent),
        "padding-right" => apply_side(value, &mut style.padding.right, style.font_size, parent),
        "padding-bottom" => apply_side(value, &mut style.padding.bottom, style.font_size, parent),
        "padding-left" => apply_side(value, &mut style.padding.left, style.font_size, parent),
        "padding-inline-start" => {
            apply_side(value, &mut style.padding.left, style.font_size, parent)
        }
        "padding-inline-end" => {
            apply_side(value, &mut style.padding.right, style.font_size, parent)
        }
        "padding-block-start" => {
            apply_side(value, &mut style.padding.top, style.font_size, parent)
        }
        "padding-block-end" => {
            apply_side(value, &mut style.padding.bottom, style.font_size, parent)
        }
        "padding-inline" => {
            apply_side(value, &mut style.padding.left, style.font_size, parent);
            apply_side(value, &mut style.padding.right, style.font_size, parent);
        }
        "padding-block" => {
            apply_side(value, &mut style.padding.top, style.font_size, parent);
            apply_side(value, &mut style.padding.bottom, style.font_size, parent);
        }
        "inset-inline-start" => style.left = offset_from(value, style.font_size, parent),
        "inset-inline-end" => style.right = offset_from(value, style.font_size, parent),
        "inset-block-start" => style.top = offset_from(value, style.font_size, parent),
        "inset-block-end" => style.bottom = offset_from(value, style.font_size, parent),
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
            if matches!(value, Value::Keyword(k) if k == "subgrid") {
                style.subgrid_columns = true;
                style.grid_template_columns = Vec::new();
            } else {
                style.subgrid_columns = false;
                style.grid_template_columns = grid_tracks_from(value, style.font_size, parent);
            }
        }
        "grid-template-rows" => {
            if matches!(value, Value::Keyword(k) if k == "subgrid") {
                style.subgrid_rows = true;
                style.grid_template_rows = Vec::new();
            } else {
                style.subgrid_rows = false;
                style.grid_template_rows = grid_tracks_from(value, style.font_size, parent);
            }
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
        "grid-auto-columns" => {
            let mut tracks = grid_tracks_from(value, style.font_size, parent);
            if let Some(t) = tracks.pop() {
                style.grid_auto_columns = t;
            }
        }
        "grid-auto-rows" => {
            let mut tracks = grid_tracks_from(value, style.font_size, parent);
            if let Some(t) = tracks.pop() {
                style.grid_auto_rows = t;
            }
        }
        "justify-items" => {
            if let Value::Keyword(k) = value {
                style.justify_items = align_keyword(k);
            }
        }
        "justify-self" => {
            if let Value::Keyword(k) = value {
                style.justify_self = Some(align_keyword(k));
            }
        }
        "align-self" => {
            if let Value::Keyword(k) = value {
                style.align_self = Some(align_keyword(k));
            }
        }
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
        "top" => {
            style.anchor_top = anchor_ref_from(value);
            style.top = offset_from(value, style.font_size, parent);
        }
        "right" => {
            style.anchor_right = anchor_ref_from(value);
            style.right = offset_from(value, style.font_size, parent);
        }
        "bottom" => {
            style.anchor_bottom = anchor_ref_from(value);
            style.bottom = offset_from(value, style.font_size, parent);
        }
        "left" => {
            style.anchor_left = anchor_ref_from(value);
            style.left = offset_from(value, style.font_size, parent);
        }
        "anchor-name" => {
            style.anchor_name = match value {
                Value::Keyword(k) if k == "none" => None,
                Value::Keyword(k) => Some(k.clone()),
                Value::List(items) => items.iter().find_map(|v| {
                    if let Value::Keyword(k) = v {
                        if k != "," {
                            return Some(k.clone());
                        }
                    }
                    None
                }),
                _ => None,
            };
        }
        "position-anchor" => {
            style.position_anchor = match value {
                Value::Keyword(k) if k == "auto" || k == "none" => None,
                Value::Keyword(k) => Some(k.clone()),
                _ => None,
            };
        }
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
        "mask-image" | "-webkit-mask-image" => {
            style.mask_image = background_image_from(value);
        }
        "mask-mode" => {
            if let Value::Keyword(k) = value {
                style.mask_mode = match k.as_str() {
                    "alpha" => crate::css::MaskMode::Alpha,
                    "luminance" => crate::css::MaskMode::Luminance,
                    _ => crate::css::MaskMode::MatchSource,
                };
            }
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
        "direction" => {
            if let Value::Keyword(k) = value {
                style.direction = match k.as_str() {
                    "rtl" => Direction::Rtl,
                    _ => Direction::Ltr,
                };
            }
        }
        "overflow" => {
            let v = overflow_from(value);
            style.overflow_x = v;
            style.overflow_y = v;
        }
        "overflow-x" => style.overflow_x = overflow_from(value),
        "overflow-y" => style.overflow_y = overflow_from(value),
        "text-overflow" => {
            style.text_overflow = match value {
                Value::Keyword(k) if k == "ellipsis" => TextOverflow::Ellipsis,
                Value::Keyword(k) if k == "clip" => TextOverflow::Clip,
                Value::String(s) => TextOverflow::String(s.clone()),
                _ => TextOverflow::Clip,
            };
        }
        "line-clamp" | "-webkit-line-clamp" => {
            style.line_clamp = match value {
                Value::Number(n) if *n > 0.0 => Some(*n as u32),
                Value::Keyword(k) if k == "none" => None,
                _ => style.line_clamp,
            };
        }
        "scroll-snap-type" => {
            style.scroll_snap_type = stringify_value(value);
        }
        "scroll-snap-align" => {
            style.scroll_snap_align = stringify_value(value);
        }
        "font-feature-settings" => {
            style.font_feature_settings = stringify_value(value);
        }
        "hyphens" => {
            style.hyphens = match value {
                Value::Keyword(k) => Some(k.to_ascii_lowercase()),
                _ => None,
            };
        }
        "container-type" => {
            style.container_type = match value {
                Value::Keyword(k) => Some(k.to_ascii_lowercase()),
                _ => None,
            };
        }
        "container-name" => {
            style.container_name = match value {
                Value::Keyword(k) => Some(k.clone()),
                Value::String(s) => Some(s.clone()),
                _ => None,
            };
        }
        "aspect-ratio" => {
            style.aspect_ratio = match value {
                Value::Keyword(k) if k == "auto" => None,
                Value::Number(n) if *n > 0.0 => Some(*n),
                Value::List(parts) if parts.len() == 2 => {
                    let w = parts
                        .first()
                        .and_then(|v| match v {
                            Value::Number(n) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(1.0);
                    let h = parts
                        .get(1)
                        .and_then(|v| match v {
                            Value::Number(n) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(1.0);
                    if h > 0.0 {
                        Some(w / h)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }
        "will-change" => {
            style.will_change = match value {
                Value::Keyword(k) if k == "auto" => None,
                Value::Keyword(k) => Some(k.to_ascii_lowercase()),
                _ => stringify_value(value).map(|s| s.to_ascii_lowercase()),
            };
        }
        "container" => {
            // Shorthand: `container: <name> [/ <type>]`. Best-effort
            // parse — split on `/`, name first, type second.
            let text = render_value(value);
            let mut parts = text.split('/');
            if let Some(name) = parts.next().map(str::trim) {
                if !name.is_empty() {
                    style.container_name = Some(name.to_string());
                }
            }
            if let Some(ty) = parts.next().map(str::trim) {
                if !ty.is_empty() {
                    style.container_type = Some(ty.to_ascii_lowercase());
                }
            }
        }
        "transition" => {
            style.transitions = transitions_from(value);
        }
        "transition-property" => {
            for t in style.transitions.iter_mut() {
                if let Value::Keyword(k) = value {
                    t.property = k.clone();
                }
            }
        }
        "transition-duration" => {
            if let Some(d) = duration_seconds(value) {
                for t in style.transitions.iter_mut() {
                    t.duration_s = d;
                }
            }
        }
        "animation" => {
            style.animations = animations_from(value);
        }
        "animation-name" => {
            if let Value::Keyword(k) = value {
                for a in style.animations.iter_mut() {
                    a.name = k.clone();
                }
                if style.animations.is_empty() {
                    style.animations.push(AnimationRule {
                        name: k.clone(),
                        duration_s: 0.0,
                        delay_s: 0.0,
                        iteration_count: 1.0,
                        timing: TimingFunction::Linear,
                    });
                }
            }
        }
        "animation-duration" => {
            if let Some(d) = duration_seconds(value) {
                for a in style.animations.iter_mut() {
                    a.duration_s = d;
                }
            }
        }
        "animation-iteration-count" => {
            if let Some(n) = iteration_count(value) {
                for a in style.animations.iter_mut() {
                    a.iteration_count = n;
                }
            }
        }
        "filter" => {
            style.filter = filter_chain_from(value);
            // Fold `opacity(<n>)` into the regular opacity for paint —
            // it's the only filter function we can render correctly
            // without offscreen rendering.
            for f in &style.filter {
                if let FilterFunction::Opacity(o) = f {
                    style.opacity *= o.clamp(0.0, 1.0);
                }
            }
        }
        "box-shadow" => {
            style.box_shadow = box_shadow_from(value, style.font_size, parent);
        }
        "transform" => {
            let matrix = transform_matrix_from(value, style.font_size, parent);
            if let Some(t) = matrix {
                // Always populate the fast translate path so existing
                // paint code paths keep working.
                style.transform_translate = Some((t.tx, t.ty));
                style.transform = if t.is_pure_translate() { None } else { Some(t) };
            } else {
                style.transform_translate = None;
                style.transform = None;
            }
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

/// Parse `transform: ...` into a composed 2D matrix. Supports translate,
/// translateX/Y, scale, scaleX/Y, rotate (+ rotateZ), skewX/Y, and
/// matrix(). Unknown / 3D variants are skipped silently. Returns `None`
/// only when the value is literally `none` or contains nothing usable.
fn duration_seconds(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Length(n, Unit::Px) => Some(*n / 1000.0), // ambiguous; treat as ms
        Value::Length(n, _) => Some(*n), // assume seconds
        Value::String(s) | Value::Keyword(s) => {
            if let Some(ms) = s.strip_suffix("ms") {
                ms.parse::<f32>().ok().map(|v| v / 1000.0)
            } else if let Some(sec) = s.strip_suffix("s") {
                sec.parse::<f32>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

fn iteration_count(v: &Value) -> Option<f32> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Keyword(k) if k == "infinite" => Some(f32::INFINITY),
        _ => None,
    }
}

fn timing_from(v: &Value) -> TimingFunction {
    match v {
        Value::Keyword(k) => match k.as_str() {
            "linear" => TimingFunction::Linear,
            "ease" => TimingFunction::Ease,
            "ease-in" => TimingFunction::EaseIn,
            "ease-out" => TimingFunction::EaseOut,
            "ease-in-out" => TimingFunction::EaseInOut,
            _ => TimingFunction::Linear,
        },
        _ => TimingFunction::Linear,
    }
}

/// Parse `transition: <prop> <dur> [<timing>] [<delay>], ...` into
/// rules. We accept any whitespace-delimited token order.
fn transitions_from(value: &Value) -> Vec<TransitionRule> {
    let mut out = Vec::new();
    let groups: Vec<Vec<Value>> = match value {
        Value::List(xs) => {
            // Split on commas isn't directly available — our parser
            // currently flattens commas, so treat the entire list as
            // a single transition for the toy.
            vec![xs.clone()]
        }
        single => vec![vec![single.clone()]],
    };
    for group in groups {
        let mut rule = TransitionRule {
            property: "all".into(),
            duration_s: 0.0,
            delay_s: 0.0,
            timing: TimingFunction::Linear,
        };
        let mut saw_duration = false;
        for v in &group {
            if let Some(d) = duration_seconds(v) {
                if !saw_duration {
                    rule.duration_s = d;
                    saw_duration = true;
                } else {
                    rule.delay_s = d;
                }
                continue;
            }
            if let Value::Keyword(k) = v {
                match k.as_str() {
                    "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" => {
                        rule.timing = timing_from(v);
                    }
                    _ => {
                        rule.property = k.to_ascii_lowercase();
                    }
                }
            }
        }
        if rule.duration_s > 0.0 || !rule.property.is_empty() {
            out.push(rule);
        }
    }
    out
}

fn animations_from(value: &Value) -> Vec<AnimationRule> {
    let mut rule = AnimationRule {
        name: String::new(),
        duration_s: 0.0,
        delay_s: 0.0,
        iteration_count: 1.0,
        timing: TimingFunction::Linear,
    };
    let items: Vec<&Value> = match value {
        Value::List(xs) => xs.iter().collect(),
        single => vec![single],
    };
    let mut saw_duration = false;
    for v in items {
        if let Some(d) = duration_seconds(v) {
            if !saw_duration {
                rule.duration_s = d;
                saw_duration = true;
            } else {
                rule.delay_s = d;
            }
            continue;
        }
        if let Some(n) = iteration_count(v) {
            rule.iteration_count = n;
            continue;
        }
        if let Value::Keyword(k) = v {
            match k.as_str() {
                "linear" | "ease" | "ease-in" | "ease-out" | "ease-in-out" => {
                    rule.timing = timing_from(v);
                }
                "infinite" => {
                    rule.iteration_count = f32::INFINITY;
                }
                _ => {
                    if rule.name.is_empty() {
                        rule.name = k.clone();
                    }
                }
            }
        }
    }
    if rule.name.is_empty() && rule.duration_s == 0.0 {
        return Vec::new();
    }
    vec![rule]
}

fn overflow_from(value: &Value) -> Overflow {
    match value {
        Value::Keyword(k) => match k.as_str() {
            "hidden" => Overflow::Hidden,
            "scroll" => Overflow::Scroll,
            "auto" => Overflow::Auto,
            "clip" => Overflow::Clip,
            _ => Overflow::Visible,
        },
        _ => Overflow::Visible,
    }
}

/// Parse the right-hand side of a `filter:` declaration into a list of
/// [`FilterFunction`]s. Numeric arguments without units are taken as
/// fractions (`0`..`1`); percentages divide by 100; `<length>` for
/// `blur()` is normalised to pixels.
fn filter_chain_from(value: &Value) -> Vec<FilterFunction> {
    let mut out = Vec::new();
    let items: Vec<&Value> = match value {
        Value::Keyword(k) if k == "none" => return out,
        Value::List(xs) => xs.iter().collect(),
        single => vec![single],
    };
    for v in items {
        let Value::Function { name, args } = v else {
            continue;
        };
        let amount = |default: f32| -> f32 {
            match args.first() {
                Some(Value::Number(n)) => *n,
                Some(Value::Percentage(p)) => *p / 100.0,
                Some(Value::Length(n, _)) => *n,
                _ => default,
            }
        };
        let entry = match name.to_ascii_lowercase().as_str() {
            "blur" => {
                // blur() takes a length; use 0 default
                let n = match args.first() {
                    Some(Value::Length(n, _)) => *n,
                    Some(Value::Number(n)) => *n,
                    _ => 0.0,
                };
                FilterFunction::Blur(n.max(0.0))
            }
            "brightness" => FilterFunction::Brightness(amount(1.0).max(0.0)),
            "contrast" => FilterFunction::Contrast(amount(1.0).max(0.0)),
            "grayscale" => FilterFunction::Grayscale(amount(1.0).clamp(0.0, 1.0)),
            "hue-rotate" => FilterFunction::HueRotate(amount(0.0)),
            "invert" => FilterFunction::Invert(amount(1.0).clamp(0.0, 1.0)),
            "opacity" => FilterFunction::Opacity(amount(1.0).clamp(0.0, 1.0)),
            "saturate" => FilterFunction::Saturate(amount(1.0).max(0.0)),
            "sepia" => FilterFunction::Sepia(amount(1.0).clamp(0.0, 1.0)),
            _ => continue,
        };
        out.push(entry);
    }
    out
}

/// Best-effort textual rendering of a CSS `Value` for properties we
/// don't fully model (scroll-snap-type, font-feature-settings, etc.).
/// Returns `None` if the value is empty or unrecognised.
fn stringify_value(value: &Value) -> Option<String> {
    let s = render_value(value);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Keyword(k) => k.clone(),
        Value::String(s) => format!("\"{s}\""),
        Value::Number(n) => format!("{n}"),
        Value::Percentage(p) => format!("{p}%"),
        Value::Length(n, unit) => format!("{n}{unit:?}").to_lowercase(),
        Value::List(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(" "),
        Value::Function { name, args } => {
            let inner = args
                .iter()
                .map(render_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}({inner})")
        }
        _ => String::new(),
    }
}

fn transform_matrix_from(
    value: &Value,
    em_base: f32,
    parent: Option<&ComputedStyle>,
) -> Option<Transform2D> {
    let items: Vec<&Value> = match value {
        Value::List(items) => items.iter().collect(),
        Value::Keyword(k) if k == "none" => return None,
        single => vec![single],
    };

    let mut composed = Transform2D::IDENTITY;
    let mut any = false;

    for v in items {
        let Value::Function { name, args } = v else {
            continue;
        };
        let n = name.to_ascii_lowercase();
        let resolve_len =
            |v: &Value| length_to_px(v, em_base, parent).unwrap_or(0.0);
        let as_number = |v: &Value| match v {
            Value::Number(n) => Some(*n),
            Value::Percentage(p) => Some(p / 100.0),
            _ => None,
        };
        let as_angle_rad = |v: &Value| match v {
            Value::Number(n) => Some(*n),
            Value::Length(n, Unit::Px) => Some(*n),
            Value::Length(n, _) => Some(*n),
            // We don't parse units like `deg` / `rad` / `turn` deeply, so
            // sniff via the Function name + bare Number. Most stylesheets
            // pass plain numbers in rotate(<angle>) using the deg suffix
            // captured as Length already. Treat unit-less as degrees.
            _ => None,
        };
        let step = match n.as_str() {
            "translate" => Transform2D::translate(
                args.first().map(resolve_len).unwrap_or(0.0),
                args.get(1).map(resolve_len).unwrap_or(0.0),
            ),
            "translatex" => Transform2D::translate(
                args.first().map(resolve_len).unwrap_or(0.0),
                0.0,
            ),
            "translatey" => Transform2D::translate(
                0.0,
                args.first().map(resolve_len).unwrap_or(0.0),
            ),
            "scale" => {
                let a = args.first().and_then(as_number).unwrap_or(1.0);
                let b = args.get(1).and_then(as_number).unwrap_or(a);
                Transform2D::scale(a, b)
            }
            "scalex" => Transform2D::scale(
                args.first().and_then(as_number).unwrap_or(1.0),
                1.0,
            ),
            "scaley" => Transform2D::scale(
                1.0,
                args.first().and_then(as_number).unwrap_or(1.0),
            ),
            "rotate" | "rotatez" => {
                let raw = args.first().and_then(as_angle_rad).unwrap_or(0.0);
                Transform2D::rotate(angle_to_radians(raw, args.first()))
            }
            "skewx" => {
                let raw = args.first().and_then(as_angle_rad).unwrap_or(0.0);
                Transform2D::skew(angle_to_radians(raw, args.first()), 0.0)
            }
            "skewy" => {
                let raw = args.first().and_then(as_angle_rad).unwrap_or(0.0);
                Transform2D::skew(0.0, angle_to_radians(raw, args.first()))
            }
            "matrix" => {
                if args.len() >= 6 {
                    let nums: Vec<f32> = args
                        .iter()
                        .filter_map(as_number)
                        .collect();
                    if nums.len() >= 6 {
                        Transform2D {
                            sx: nums[0],
                            kx: nums[1],
                            ky: nums[2],
                            sy: nums[3],
                            tx: nums[4],
                            ty: nums[5],
                        }
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            // 3D variants: approximated against the 2D plane (no
            // perspective projection). `rotateZ` is a true 2D rotate;
            // `rotateX`/`rotateY` flatten to scale-along-axis using
            // `cos(angle)`; `rotate3d(x, y, z, a)` picks one of those
            // depending on which axis dominates.
            "rotatex" => {
                let raw = args.first().and_then(as_angle_rad).unwrap_or(0.0);
                let a = angle_to_radians(raw, args.first());
                Transform2D::scale(1.0, a.cos())
            }
            "rotatey" => {
                let raw = args.first().and_then(as_angle_rad).unwrap_or(0.0);
                let a = angle_to_radians(raw, args.first());
                Transform2D::scale(a.cos(), 1.0)
            }
            "rotate3d" => {
                let x = args.first().and_then(as_number).unwrap_or(0.0);
                let y = args.get(1).and_then(as_number).unwrap_or(0.0);
                let z = args.get(2).and_then(as_number).unwrap_or(0.0);
                let raw = args.get(3).and_then(as_angle_rad).unwrap_or(0.0);
                let a = angle_to_radians(raw, args.get(3));
                let ax = x.abs();
                let ay = y.abs();
                let az = z.abs();
                if az >= ax && az >= ay {
                    Transform2D::rotate(a * z.signum())
                } else if ax >= ay {
                    Transform2D::scale(1.0, a.cos())
                } else {
                    Transform2D::scale(a.cos(), 1.0)
                }
            }
            "translate3d" => {
                // Drop the z component; xy translate is well-defined.
                Transform2D::translate(
                    args.first().map(resolve_len).unwrap_or(0.0),
                    args.get(1).map(resolve_len).unwrap_or(0.0),
                )
            }
            "scale3d" => {
                // Drop the z scale; xy is well-defined.
                let a = args.first().and_then(as_number).unwrap_or(1.0);
                let b = args.get(1).and_then(as_number).unwrap_or(1.0);
                Transform2D::scale(a, b)
            }
            "matrix3d" => {
                // 4x4 matrix in column-major order. Project onto 2D
                // by taking m11/m12/m21/m22 (the upper-left of the
                // rotation/scale block) plus m41/m42 (translation).
                if args.len() >= 16 {
                    let nums: Vec<f32> = args.iter().filter_map(as_number).collect();
                    if nums.len() >= 16 {
                        Transform2D {
                            sx: nums[0],
                            kx: nums[1],
                            ky: nums[4],
                            sy: nums[5],
                            tx: nums[12],
                            ty: nums[13],
                        }
                    } else {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            "perspective" => {
                // Pure perspective is a viewing transform; with no 3D
                // pipeline it's identity. Skip silently so a
                // `perspective(800px) rotateY(30deg)` chain still
                // contributes its rotateY component.
                continue;
            }
            _ => continue,
        };
        composed = composed.then(&step);
        any = true;
    }

    if any {
        Some(composed)
    } else {
        None
    }
}

/// CSS angles can be `deg`, `rad`, `turn`, or `grad`. The raw value we
/// see is already the numeric part (the CSS parser stripped the unit
/// into a `Length` with `Unit::Px` because there's no Angle unit on
/// `Value`). Detect the original suffix from the source argument when
/// available; otherwise assume degrees.
fn angle_to_radians(raw: f32, source: Option<&Value>) -> f32 {
    use std::f32::consts::PI;
    let unit_hint = match source {
        Some(Value::Length(_, Unit::Px)) => "deg",
        // Boa-style angle units land in Keyword or String when our
        // parser doesn't recognise them.
        Some(Value::Keyword(k)) | Some(Value::String(k)) => {
            let trimmed = k.trim();
            // Match the suffix in case the value got stringified.
            if trimmed.ends_with("rad") {
                "rad"
            } else if trimmed.ends_with("turn") {
                "turn"
            } else if trimmed.ends_with("grad") {
                "grad"
            } else {
                "deg"
            }
        }
        _ => "deg",
    };
    match unit_hint {
        "rad" => raw,
        "turn" => raw * 2.0 * PI,
        "grad" => raw * PI / 200.0,
        _ => raw * PI / 180.0,
    }
}

fn offset_from(v: &Value, em: f32, parent: Option<&ComputedStyle>) -> Option<f32> {
    match v {
        Value::Keyword(k) if k == "auto" => None,
        Value::Function { name, .. } if name == "anchor" => None,
        _ => length_to_px(v, em, parent),
    }
}

/// Parse `anchor(<name>? <side>)` off the value of an inset property.
/// `name` is a dashed-ident (`--foo`); `side` is one of top/right/
/// bottom/left/center/start/end. Anything else returns `None`.
fn anchor_ref_from(v: &Value) -> Option<crate::css::AnchorRef> {
    let Value::Function { name, args } = v else {
        return None;
    };
    if name != "anchor" {
        return None;
    }
    // Args are a flat list of tokens; commas appear as Keyword(",").
    // Take everything before the first comma (the side argument).
    let mut anchor_name: Option<String> = None;
    let mut side: Option<crate::css::AnchorSide> = None;
    for a in args {
        if matches!(a, Value::Keyword(k) if k == ",") {
            break;
        }
        if let Value::Keyword(k) = a {
            if k.starts_with("--") {
                anchor_name = Some(k.clone());
                continue;
            }
            let parsed = match k.as_str() {
                "top" => Some(crate::css::AnchorSide::Top),
                "right" => Some(crate::css::AnchorSide::Right),
                "bottom" => Some(crate::css::AnchorSide::Bottom),
                "left" => Some(crate::css::AnchorSide::Left),
                "center" => Some(crate::css::AnchorSide::Center),
                "start" => Some(crate::css::AnchorSide::Start),
                "end" => Some(crate::css::AnchorSide::End),
                _ => None,
            };
            if parsed.is_some() {
                side = parsed;
            }
        }
    }
    let side = side?;
    Some(crate::css::AnchorRef {
        name: anchor_name,
        side,
    })
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
            Value::Function { name, args } if name == "minmax" => {
                let min_v = args
                    .first()
                    .map(|v| single_track(v, em, parent))
                    .unwrap_or(GridTrack::Auto);
                let max_v = args
                    .get(1)
                    .map(|v| single_track(v, em, parent))
                    .unwrap_or(GridTrack::Auto);
                tracks.push(GridTrack::MinMax(Box::new(min_v), Box::new(max_v)));
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

/// Resolve a single CSS value as one `GridTrack` (used by the
/// `minmax(min, max)` parser; each side is itself a track-list-of-one).
fn single_track(value: &Value, em: f32, parent: Option<&ComputedStyle>) -> GridTrack {
    match value {
        Value::Keyword(k) if k == "auto" => GridTrack::Auto,
        Value::Percentage(p) => GridTrack::Percent(*p),
        Value::Number(n) => GridTrack::Px(*n),
        Value::Length(n, Unit::Fr) => GridTrack::Fr(*n),
        Value::Length(_, _) => {
            length_to_px(value, em, parent)
                .map(GridTrack::Px)
                .unwrap_or(GridTrack::Auto)
        }
        _ => GridTrack::Auto,
    }
}

fn align_keyword(k: &str) -> AlignItems {
    match k {
        "flex-end" | "end" | "right" => AlignItems::FlexEnd,
        "center" => AlignItems::Center,
        "baseline" => AlignItems::Baseline,
        "stretch" => AlignItems::Stretch,
        _ => AlignItems::FlexStart,
    }
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
            } else if items
                .iter()
                .any(|v| matches!(v, Value::Keyword(k) if k == "span"))
            {
                // `grid-column: span N` (or `grid-column: span name`) is a
                // single line value, not two — keep the list intact.
                (grid_line_from(value), None)
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
    fn transform_translate_populates_fast_path() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { transform: translate(10px, 20px); }");
        let s = style_for(&dom, &sheet, "p");
        assert_eq!(s.transform_translate, Some((10.0, 20.0)));
        // Pure translate stays on the fast path; no full matrix.
        assert!(s.transform.is_none());
    }

    #[test]
    fn transform_rotate_builds_matrix() {
        let dom = html::parse("<p>hi</p>");
        let sheet = parser::parse("p { transform: rotate(90deg); }");
        let s = style_for(&dom, &sheet, "p");
        let t = s.transform.expect("transform matrix");
        // rotate(90deg) = [[0,-1],[1,0]] (column-major sx,kx,ky,sy).
        assert!((t.sx - 0.0).abs() < 1e-5);
        assert!((t.kx - 1.0).abs() < 1e-5);
        assert!((t.ky - -1.0).abs() < 1e-5);
        assert!((t.sy - 0.0).abs() < 1e-5);
    }

    #[test]
    fn transform_scale_translate_composes() {
        let dom = html::parse("<p>hi</p>");
        // Order: scale first, then translate (right-to-left in CSS).
        let sheet =
            parser::parse("p { transform: translate(5px, 0px) scale(2); }");
        let s = style_for(&dom, &sheet, "p");
        let t = s.transform.expect("matrix");
        // After applying scale then translate: x' = sx*x + tx, sy = 2.
        assert!((t.sx - 2.0).abs() < 1e-5);
        assert!((t.sy - 2.0).abs() < 1e-5);
        assert!((t.tx - 5.0).abs() < 1e-5);
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

    #[test]
    fn dir_rtl_attribute_flips_direction_and_default_align() {
        let mut dom = crate::dom::Dom::default();
        let doc = dom.document();
        let p = dom.create_element(
            "p".to_string(),
            vec![("dir".to_string(), "rtl".to_string())],
        );
        dom.append_child(doc, p);
        let sheet = parser::parse("p { color: black; }");
        let tree = StyleTree::compute(&dom, &[&sheet]);
        let s = tree.get(p);
        assert!(matches!(s.direction, crate::css::Direction::Rtl));
        // Default text-align of Left got auto-flipped to Right
        // for the RTL paragraph.
        assert!(matches!(s.text_align, TextAlign::Right));
    }

    #[test]
    fn text_align_start_resolves_via_direction() {
        let dom = html::parse("<p>hello</p>");
        let sheet = parser::parse("p { text-align: start; }");
        let tree = StyleTree::compute(&dom, &[&sheet]);
        let id = find_node(&dom, &dom.document(), "p").unwrap();
        let resolved = tree.get(id).text_align.resolved(tree.get(id).direction);
        assert!(
            matches!(resolved, TextAlign::Left),
            "LTR + start should resolve to Left, got {:?}",
            resolved
        );
    }

    fn find_node(
        dom: &crate::dom::Dom,
        from: &NodeId,
        tag: &str,
    ) -> Option<NodeId> {
        if let NodeKind::Element { tag: t, .. } = &dom.node(*from).kind {
            if t == tag {
                return Some(*from);
            }
        }
        for c in dom.children(*from).collect::<Vec<_>>() {
            if let Some(found) = find_node(dom, &c, tag) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn page_css_does_not_cross_into_shadow_tree() {
        // The outer <p> is in the light DOM and should pick up the
        // page rule. The inner <p> sits inside a __shadow_root__
        // and must NOT pick it up (scope isolation).
        //
        // We build the DOM directly rather than going through
        // html::parse — the HTML parser would treat
        // `<__shadow_root__>` as an unknown tag and serialize
        // through any tagsoup normalisation. attachShadow() emits
        // the synthetic root by name; mirror that here.
        let mut dom = crate::dom::Dom::default();
        let doc = dom.document();
        let outer_p = dom.create_element("p".to_string(), Vec::new());
        dom.append_child(doc, outer_p);
        let host = dom.create_element("div".to_string(), Vec::new());
        dom.append_child(doc, host);
        let shadow = dom.create_element("__shadow_root__".to_string(), Vec::new());
        dom.append_child(host, shadow);
        let inner_p = dom.create_element("p".to_string(), Vec::new());
        dom.append_child(shadow, inner_p);

        let page = parser::parse("p { color: red; }");
        let tree = StyleTree::compute(&dom, &[&page]);
        assert_eq!(tree.get(outer_p).color, Color::rgb(255, 0, 0));
        assert_ne!(
            tree.get(inner_p).color,
            Color::rgb(255, 0, 0),
            "inner inside shadow must not inherit page CSS"
        );
    }

    #[test]
    fn shadow_internal_style_only_matches_shadow_descendants() {
        // Shadow-internal <style> turns its descendants blue;
        // light-tree siblings of the same tag stay default.
        let mut dom = crate::dom::Dom::default();
        let doc = dom.document();
        let outer_span = dom.create_element("span".to_string(), Vec::new());
        dom.append_child(doc, outer_span);
        let host = dom.create_element("div".to_string(), Vec::new());
        dom.append_child(doc, host);
        let shadow = dom.create_element("__shadow_root__".to_string(), Vec::new());
        dom.append_child(host, shadow);
        let style_el = dom.create_element("style".to_string(), Vec::new());
        dom.append_child(shadow, style_el);
        let style_text = dom.create_text("span{color:rgb(0,0,255);}".to_string());
        dom.append_child(style_el, style_text);
        let inner_span = dom.create_element("span".to_string(), Vec::new());
        dom.append_child(shadow, inner_span);

        let refs = crate::css::discover_stylesheets(&dom);
        let sheets: Vec<crate::css::Stylesheet> = refs
            .into_iter()
            .filter_map(|r| match r {
                crate::css::StylesheetRef::Embedded(s) => Some(s),
                _ => None,
            })
            .collect();
        let tree = crate::css::style_dom(&dom, &sheets);
        assert_ne!(
            tree.get(outer_span).color,
            Color::rgb(0, 0, 255),
            "shadow-internal style must not affect light tree"
        );
        assert_eq!(
            tree.get(inner_span).color,
            Color::rgb(0, 0, 255),
            "shadow-internal style should affect its own subtree"
        );
    }

    fn find_two(
        dom: &crate::dom::Dom,
        tree: &StyleTree,
        tag: &str,
    ) -> (ComputedStyle, ComputedStyle) {
        let mut hits = Vec::new();
        walk_all(dom, dom.document(), tag, tree, &mut hits);
        assert!(hits.len() >= 2, "expected at least 2 matches for <{tag}>");
        (hits[0].clone(), hits[1].clone())
    }

    fn walk_all(
        dom: &crate::dom::Dom,
        id: NodeId,
        tag: &str,
        tree: &StyleTree,
        out: &mut Vec<ComputedStyle>,
    ) {
        if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
            if t == tag {
                out.push(tree.get(id).clone());
            }
        }
        for c in dom.children(id).collect::<Vec<_>>() {
            walk_all(dom, c, tag, tree, out);
        }
    }
}
