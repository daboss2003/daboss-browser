//! CSS subsystem entry points. The public API is intentionally narrow:
//! parse stylesheets, compute styles for a DOM, look up a node's style.

mod cascade;
mod parser;
mod types;

pub use cascade::StyleTree;
pub use parser::parse;
// The cascade fills every field of ComputedStyle; layout/paint in later
// phases will be the actual consumers, hence the `allow(unused_imports)`.
#[allow(unused_imports)]
pub use types::{
    BorderStyle, BoxSides, Color, ComputedStyle, Dimension, Display, FontStyle, Stylesheet,
    TextAlign, WhiteSpace,
};

use crate::dom::{Dom, NodeId, NodeKind};

/// Compute a style for every node in the DOM.
///
/// `page_stylesheets` are the author-provided stylesheets in source order.
/// We always evaluate the user-agent stylesheet first so author rules win
/// on equal specificity.
pub fn style_dom(dom: &Dom, page_stylesheets: &[Stylesheet]) -> StyleTree {
    let ua = ua_stylesheet();
    let mut sheets: Vec<&Stylesheet> = Vec::with_capacity(1 + page_stylesheets.len());
    sheets.push(&ua);
    for s in page_stylesheets {
        sheets.push(s);
    }
    StyleTree::compute(dom, &sheets)
}

/// Walk the DOM and collect every author stylesheet expressed inline:
///  - the text content of `<style>` elements
///
/// External `<link rel="stylesheet">` resources are deferred (need fetching).
pub fn extract_embedded_stylesheets(dom: &Dom) -> Vec<Stylesheet> {
    let mut out = Vec::new();
    collect(dom, dom.document(), &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<Stylesheet>) {
    if let NodeKind::Element { tag, .. } = &dom.node(node).kind {
        if tag == "style" {
            let mut text = String::new();
            for child in dom.children(node) {
                if let NodeKind::Text(t) = &dom.node(child).kind {
                    text.push_str(t);
                }
            }
            out.push(parse(&text));
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for k in kids {
        collect(dom, k, out);
    }
}

/// Embedded user-agent stylesheet. Roughly matches what browsers ship as the
/// default for HTML5 elements. Kept tight on purpose; layout doesn't need
/// every property to look reasonable.
fn ua_stylesheet() -> Stylesheet {
    parse(UA_STYLESHEET)
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
    fn ua_makes_div_block_and_span_inline() {
        let dom = html::parse("<div></div><span></span>");
        let tree = style_dom(&dom, &[]);
        // Find the div and span
        fn find<'a>(dom: &'a Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    return Some(id);
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                if let Some(r) = find(dom, c, tag) {
                    return Some(r);
                }
            }
            None
        }
        let div = find(&dom, dom.document(), "div").unwrap();
        let span = find(&dom, dom.document(), "span").unwrap();
        assert_eq!(tree.get(div).display, Display::Block);
        assert_eq!(tree.get(span).display, Display::Inline);
    }

    #[test]
    fn embedded_style_block_is_picked_up() {
        let dom = html::parse(
            "<style>p { color: red; }</style><p>hi</p>",
        );
        let sheets = extract_embedded_stylesheets(&dom);
        assert_eq!(sheets.len(), 1);
        let tree = style_dom(&dom, &sheets);
        fn find(dom: &Dom, id: NodeId, tag: &str) -> Option<NodeId> {
            if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
                if t == tag {
                    return Some(id);
                }
            }
            for c in dom.children(id).collect::<Vec<_>>() {
                if let Some(r) = find(dom, c, tag) {
                    return Some(r);
                }
            }
            None
        }
        let p = find(&dom, dom.document(), "p").unwrap();
        assert_eq!(tree.get(p).color, Color::rgb(255, 0, 0));
    }
}
