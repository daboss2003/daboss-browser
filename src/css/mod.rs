//! CSS subsystem entry points.

mod cascade;
mod parser;
mod types;

pub use cascade::{selector_matches, InteractionState, StyleTree};
pub use parser::{parse, parse_selector_list_str};
#[allow(unused_imports)]
pub use types::{
    AlignContent, AlignItems, AnchorRef, AnchorSide, AnimationRule, BackgroundImage, BorderStyle,
    BoxShadow, BoxSides, BoxSizing, Color, ComputedStyle, Dimension, Direction, Display,
    FilterFunction, FlexDirection, FlexWrap, FontStyle, GridAutoFlow, GridLine, GridPlacement,
    GridTrack, JustifyContent, MediaCondition, MediaQuery, Overflow, Position, Selector,
    Stylesheet, TableLayout, TextAlign, TextDecoration, TextOverflow, TimingFunction,
    Transform2D, TransitionRule, Viewport, WhiteSpace,
};

use crate::dom::{Dom, NodeId, NodeKind};

/// A stylesheet either embedded inline or referenced by URL.
/// Phase 3 `main.rs` walks this list, fetching `External` refs through the
/// network client. Order in the list matches DOM source order so cascade
/// behaves correctly. External refs carry an optional `integrity` value
/// for Subresource Integrity verification.
pub enum StylesheetRef {
    Embedded(Stylesheet),
    External {
        href: String,
        integrity: Option<String>,
    },
}

pub fn discover_stylesheets(dom: &Dom) -> Vec<StylesheetRef> {
    let mut out = Vec::new();
    collect(dom, dom.document(), &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<StylesheetRef>) {
    collect_scoped(dom, node, None, out);
}

/// Same as `collect` but tracks the current shadow-root scope.
/// When the walker enters a `__shadow_root__` element, every
/// `<style>` it emits below it gets tagged with the host shadow
/// root's NodeId so the cascade can scope-gate it.
fn collect_scoped(
    dom: &Dom,
    node: NodeId,
    scope: Option<NodeId>,
    out: &mut Vec<StylesheetRef>,
) {
    let mut child_scope = scope;
    if let NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        if tag == "__shadow_root__" {
            // Descend with this node as the active shadow scope.
            child_scope = Some(node);
        }
        match tag.as_str() {
            "style" => {
                let mut text = String::new();
                for child in dom.children(node) {
                    if let NodeKind::Text(t) = &dom.node(child).kind {
                        text.push_str(t);
                    }
                }
                let mut sheet = parse(&text);
                sheet.scope = scope;
                out.push(StylesheetRef::Embedded(sheet));
            }
            "link" if scope.is_none() => {
                // Shadow-internal `<link rel="stylesheet">` would need
                // a similar scope tag; for the toy we only support
                // shadow-internal `<style>` blocks (most actual usage).
                let rel = attrs.iter().find(|(k, _)| k == "rel").map(|(_, v)| v.as_str());
                let is_stylesheet = rel
                    .map(|r| r.split_ascii_whitespace().any(|w| w.eq_ignore_ascii_case("stylesheet")))
                    .unwrap_or(false);
                if is_stylesheet {
                    if let Some((_, href)) = attrs.iter().find(|(k, _)| k == "href") {
                        if !href.is_empty() {
                            let integrity = attrs
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("integrity"))
                                .map(|(_, v)| v.clone());
                            out.push(StylesheetRef::External {
                                href: href.clone(),
                                integrity,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for k in kids {
        collect_scoped(dom, k, child_scope, out);
    }
}

/// Compute a style for every node in the DOM. The UA stylesheet is always
/// evaluated first so author rules win on equal specificity.
pub fn style_dom(dom: &Dom, page_stylesheets: &[Stylesheet]) -> StyleTree {
    style_dom_with(dom, page_stylesheets, &InteractionState::EMPTY)
}

/// Same as `style_dom` but with explicit `:hover` / `:focus` chains and a
/// default desktop viewport. Used by tests and the static PNG path.
pub fn style_dom_with(
    dom: &Dom,
    page_stylesheets: &[Stylesheet],
    interaction: &InteractionState,
) -> StyleTree {
    style_dom_with_viewport(dom, page_stylesheets, interaction, &Viewport::DEFAULT)
}

/// Cascade evaluation with full context: interaction state for
/// `:hover` / `:focus`, plus the current viewport for `@media` queries.
/// Each input stylesheet is flattened: rules from `@media` blocks whose
/// query matches the viewport get merged into the rule list at their
/// original position, preserving cascade order.
pub fn style_dom_with_viewport(
    dom: &Dom,
    page_stylesheets: &[Stylesheet],
    interaction: &InteractionState,
    viewport: &Viewport,
) -> StyleTree {
    let ua = ua_stylesheet();
    let flattened: Vec<Stylesheet> = std::iter::once(&ua)
        .chain(page_stylesheets.iter())
        .map(|s| flatten_for_viewport(s, viewport))
        .collect();
    let sheets: Vec<&Stylesheet> = flattened.iter().collect();
    StyleTree::compute_with(dom, &sheets, interaction)
}

fn flatten_for_viewport(sheet: &Stylesheet, vp: &Viewport) -> Stylesheet {
    let mut flat = Stylesheet {
        rules: sheet.rules.clone(),
        // Carry forward the scope + UA flag so cascade-time
        // gating still works after media-block flattening.
        scope: sheet.scope,
        is_ua: sheet.is_ua,
        ..Stylesheet::default()
    };
    for mb in &sheet.media_blocks {
        if media_query_matches(&mb.query, vp) {
            flat.rules.extend(mb.rules.iter().cloned());
        }
    }
    flat
}

/// `true` if **any** alternative in `q` matches `vp`. An empty query
/// (e.g. `@media {}`) is treated as always matching.
pub fn media_query_matches(q: &MediaQuery, vp: &Viewport) -> bool {
    if q.alternatives.is_empty() {
        return true;
    }
    q.alternatives
        .iter()
        .any(|alt| alt.iter().all(|c| condition_matches(c, vp)))
}

fn condition_matches(c: &MediaCondition, vp: &Viewport) -> bool {
    match c {
        MediaCondition::MediaType(t) => {
            // We render to a screen and never to a printer in toy land.
            matches!(t.as_str(), "screen" | "all")
        }
        MediaCondition::MinWidth(px) => vp.width >= *px,
        MediaCondition::MaxWidth(px) => vp.width <= *px,
        MediaCondition::MinHeight(px) => vp.height >= *px,
        MediaCondition::MaxHeight(px) => vp.height <= *px,
        MediaCondition::ExactWidth(px) => (vp.width - *px).abs() < 0.5,
        MediaCondition::Orientation(which) => {
            let landscape = vp.width >= vp.height;
            (which == "landscape" && landscape) || (which == "portrait" && !landscape)
        }
        MediaCondition::PrefersColorScheme(scheme) => scheme == vp.color_scheme,
        // Unknown features fail the alternative they're in.
        MediaCondition::Unsupported(_) => false,
    }
}

fn ua_stylesheet() -> Stylesheet {
    let mut s = parse(UA_STYLESHEET);
    s.is_ua = true;
    s
}

const UA_STYLESHEET: &str = r#"
html, address, blockquote, body, dd, div, dl, dt, fieldset, form,
frame, frameset, h1, h2, h3, h4, h5, h6, noframes, ol, p, ul,
center, dir, hr, menu, pre, article, aside, footer, header,
hgroup, main, nav, section, figure, figcaption {
    display: block;
}
li { display: list-item; }
table { display: block; }
tr { display: block; }
td, th { display: inline-block; }

head { display: none; }
script { display: none; }
style { display: none; }
title { display: none; }
meta { display: none; }
link { display: none; }
base { display: none; }
template { display: none; }
noscript { display: none; }

body {
    margin: 8px;
    font-family: serif;
    font-size: 16px;
    color: black;
    line-height: 1.2;
}

h1 { font-size: 2em; font-weight: bold; margin-top: 0.67em; margin-bottom: 0.67em; }
h2 { font-size: 1.5em; font-weight: bold; margin-top: 0.83em; margin-bottom: 0.83em; }
h3 { font-size: 1.17em; font-weight: bold; margin-top: 1em; margin-bottom: 1em; }
h4 { font-weight: bold; margin-top: 1.33em; margin-bottom: 1.33em; }
h5 { font-size: 0.83em; font-weight: bold; margin-top: 1.67em; margin-bottom: 1.67em; }
h6 { font-size: 0.67em; font-weight: bold; margin-top: 2.33em; margin-bottom: 2.33em; }

p { margin-top: 1em; margin-bottom: 1em; }
ul, ol { margin-top: 1em; margin-bottom: 1em; padding-left: 40px; }

strong, b { font-weight: bold; }
em, i { font-style: italic; }
code, kbd, samp, pre, tt { font-family: monospace; }
pre { white-space: pre; margin-top: 1em; margin-bottom: 1em; }

a { color: blue; }

hr { display: block; margin-top: 0.5em; margin-bottom: 0.5em; border-style: solid; border-width: 1px; border-color: black; }

img { display: inline-block; }
iframe { display: inline-block; width: 300px; height: 150px; border-width: 2px; border-style: solid; border-color: gray; }

input, button, select, textarea { display: inline-block; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn discovers_external_link_and_inline_style() {
        let dom = html::parse(
            r#"<link rel="stylesheet" href="a.css">
               <style>p { color: red; }</style>
               <link rel="stylesheet" href="b.css">"#,
        );
        let refs = discover_stylesheets(&dom);
        assert_eq!(refs.len(), 3);
        assert!(matches!(refs[0], StylesheetRef::External { ref href, .. } if href == "a.css"));
        assert!(matches!(refs[1], StylesheetRef::Embedded(_)));
        assert!(matches!(refs[2], StylesheetRef::External { ref href, .. } if href == "b.css"));
    }

    #[test]
    fn ignores_non_stylesheet_links() {
        let dom = html::parse(r#"<link rel="icon" href="favicon.ico">"#);
        let refs = discover_stylesheets(&dom);
        assert_eq!(refs.len(), 0);
    }

    #[test]
    fn media_query_matches_min_max_width() {
        let s = parse("@media (max-width: 600px) { p { color: red; } }");
        let narrow = Viewport::from_size(360.0, 800.0);
        let wide = Viewport::from_size(1200.0, 800.0);
        assert!(media_query_matches(&s.media_blocks[0].query, &narrow));
        assert!(!media_query_matches(&s.media_blocks[0].query, &wide));
    }

    #[test]
    fn media_query_evaluation_drives_cascade_decisions() {
        // The narrow-viewport rule turns <p> red; the page rule turns it blue.
        let dom = html::parse("<html><body><p>X</p></body></html>");
        let sheet = parse(
            "@media (max-width: 600px) { p { color: red; } } p { color: blue; }",
        );
        let interaction = InteractionState::EMPTY;
        let narrow = Viewport::from_size(360.0, 800.0);
        let wide = Viewport::from_size(1200.0, 800.0);

        let narrow_tree = style_dom_with_viewport(&dom, &[sheet.clone()], &interaction, &narrow);
        let wide_tree = style_dom_with_viewport(&dom, &[sheet], &interaction, &wide);

        // Find the <p> node.
        let p = find_p(&dom).expect("p");
        let narrow_color = narrow_tree.get(p).color;
        let wide_color = wide_tree.get(p).color;
        // Narrow: red wins because @media matches and comes after the
        // unscoped rule in cascade order. Wide: only the page rule
        // applies, so blue.
        assert_eq!(narrow_color.r, 255);
        assert_eq!(narrow_color.g, 0);
        assert_eq!(wide_color.b, 255);
    }

    fn find_p(dom: &Dom) -> Option<NodeId> {
        fn walk(dom: &Dom, n: NodeId) -> Option<NodeId> {
            if let NodeKind::Element { tag, .. } = &dom.node(n).kind {
                if tag == "p" {
                    return Some(n);
                }
            }
            for c in dom.children(n).collect::<Vec<_>>() {
                if let Some(found) = walk(dom, c) {
                    return Some(found);
                }
            }
            None
        }
        walk(dom, dom.document())
    }
}
