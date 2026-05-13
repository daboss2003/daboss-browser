//! CSS subsystem entry points.

mod cascade;
mod parser;
mod types;

pub use cascade::{InteractionState, StyleTree};
pub use parser::parse;
#[allow(unused_imports)]
pub use types::{
    BackgroundImage, BorderStyle, BoxShadow, BoxSides, Color, ComputedStyle, Dimension, Display,
    FontStyle, Stylesheet, TableLayout, TextAlign, TextDecoration, WhiteSpace,
};

use crate::dom::{Dom, NodeId, NodeKind};

/// A stylesheet either embedded inline or referenced by URL.
/// Phase 3 `main.rs` walks this list, fetching `External` refs through the
/// network client. Order in the list matches DOM source order so cascade
/// behaves correctly.
pub enum StylesheetRef {
    Embedded(Stylesheet),
    External(String),
}

pub fn discover_stylesheets(dom: &Dom) -> Vec<StylesheetRef> {
    let mut out = Vec::new();
    collect(dom, dom.document(), &mut out);
    out
}

fn collect(dom: &Dom, node: NodeId, out: &mut Vec<StylesheetRef>) {
    if let NodeKind::Element { tag, attrs } = &dom.node(node).kind {
        match tag.as_str() {
            "style" => {
                let mut text = String::new();
                for child in dom.children(node) {
                    if let NodeKind::Text(t) = &dom.node(child).kind {
                        text.push_str(t);
                    }
                }
                out.push(StylesheetRef::Embedded(parse(&text)));
            }
            "link" => {
                let rel = attrs.iter().find(|(k, _)| k == "rel").map(|(_, v)| v.as_str());
                let is_stylesheet = rel
                    .map(|r| r.split_ascii_whitespace().any(|w| w.eq_ignore_ascii_case("stylesheet")))
                    .unwrap_or(false);
                if is_stylesheet {
                    if let Some((_, href)) = attrs.iter().find(|(k, _)| k == "href") {
                        if !href.is_empty() {
                            out.push(StylesheetRef::External(href.clone()));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for k in kids {
        collect(dom, k, out);
    }
}

/// Compute a style for every node in the DOM. The UA stylesheet is always
/// evaluated first so author rules win on equal specificity.
pub fn style_dom(dom: &Dom, page_stylesheets: &[Stylesheet]) -> StyleTree {
    style_dom_with(dom, page_stylesheets, &InteractionState::EMPTY)
}

/// Same as `style_dom` but with explicit `:hover` / `:focus` chains.
pub fn style_dom_with(
    dom: &Dom,
    page_stylesheets: &[Stylesheet],
    interaction: &InteractionState,
) -> StyleTree {
    let ua = ua_stylesheet();
    let mut sheets: Vec<&Stylesheet> = Vec::with_capacity(1 + page_stylesheets.len());
    sheets.push(&ua);
    for s in page_stylesheets {
        sheets.push(s);
    }
    StyleTree::compute_with(dom, &sheets, interaction)
}

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
    fn discovers_external_link_and_inline_style() {
        let dom = html::parse(
            r#"<link rel="stylesheet" href="a.css">
               <style>p { color: red; }</style>
               <link rel="stylesheet" href="b.css">"#,
        );
        let refs = discover_stylesheets(&dom);
        assert_eq!(refs.len(), 3);
        assert!(matches!(refs[0], StylesheetRef::External(ref s) if s == "a.css"));
        assert!(matches!(refs[1], StylesheetRef::Embedded(_)));
        assert!(matches!(refs[2], StylesheetRef::External(ref s) if s == "b.css"));
    }

    #[test]
    fn ignores_non_stylesheet_links() {
        let dom = html::parse(r#"<link rel="icon" href="favicon.ico">"#);
        let refs = discover_stylesheets(&dom);
        assert_eq!(refs.len(), 0);
    }
}
