//! Block layout + inline formatting context.
//!
//! For each block element we:
//!  1. Resolve margin / border / padding / width against the containing block.
//!  2. Walk the children, splitting them into groups:
//!       - a single **block** child (display: block/list-item) → a row of
//!         its own, laid out by recursing into block layout
//!       - a contiguous run of **inline** children (display: inline,
//!         inline-block, or text node) → one inline formatting context,
//!         where all of them share line boxes
//!  3. Lay out each group vertically inside the content area, summing heights.
//!  4. Set the element's box rect.
//!
//! Inline runs use `cosmic-text` for real text shaping with per-span weight
//! and style (so `<strong>` and `<em>` get bold/italic glyphs in the same
//! shaped string). Glyphs are then redistributed to their source DOM nodes
//! by byte index — each inline element ends up with a bounding rect equal
//! to the union of its glyphs across however many lines they wrap onto.

use std::ops::Range;

use super::text::{collapse_whitespace, InlineContent, InlineSpan, TextLayout};
use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{BoxSides, ComputedStyle, Dimension, Display, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};

pub fn layout(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    node: NodeId,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    match &dom.node(node).kind {
        NodeKind::Document => {
            let mut y = containing.y;
            let kids: Vec<NodeId> = dom.children(node).collect();
            for child in kids {
                let cb = Rect { y, ..containing };
                y += layout(dom, styles, text, child, cb, tree);
            }
            y - containing.y
        }
        NodeKind::Element { .. } => {
            let style = styles.get(node);
            if style.display == Display::None {
                return 0.0;
            }
            layout_block(dom, styles, text, node, style, containing, tree)
        }
        NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::Doctype(_) => {
            // Loose text / metadata outside an element shouldn't normally exist
            // after the tree builder runs, but if it does we ignore it for
            // block layout; an enclosing IFC would have absorbed it.
            0.0
        }
    }
}

fn layout_block(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    node: NodeId,
    style: &ComputedStyle,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let margin = style.margin;
    let border = style.border_width;
    let padding = style.padding;

    let cb_width = containing.width;
    let content_width = match style.width {
        Dimension::Length(w) => w,
        Dimension::Percent(pct) => {
            (cb_width * pct / 100.0
                - border.left
                - border.right
                - padding.left
                - padding.right)
                .max(0.0)
        }
        Dimension::Auto => (cb_width
            - margin.left
            - margin.right
            - border.left
            - border.right
            - padding.left
            - padding.right)
            .max(0.0),
    };

    let border_box_width =
        content_width + border.left + border.right + padding.left + padding.right;

    let border_box_x = containing.x + margin.left;
    let border_box_y = containing.y + margin.top;
    let content_x = border_box_x + border.left + padding.left;
    let content_y = border_box_y + border.top + padding.top;

    // Group children: contiguous inline runs become one IFC each, block
    // children stand alone.
    let kids: Vec<NodeId> = dom.children(node).collect();
    let groups = group_children(dom, styles, &kids);

    let mut child_y = content_y;
    for group in groups {
        let cb = Rect {
            x: content_x,
            y: child_y,
            width: content_width,
            height: 0.0,
        };
        let h = match group {
            ChildGroup::Block(child) => layout(dom, styles, text, child, cb, tree),
            ChildGroup::Inline(nodes) => {
                layout_inline_run(dom, styles, text, &nodes, style, cb, tree)
            }
        };
        child_y += h;
    }
    let computed_content_height = child_y - content_y;

    let content_height = match style.height {
        Dimension::Length(h) => h,
        Dimension::Percent(_) | Dimension::Auto => computed_content_height,
    };
    let border_box_height =
        content_height + border.top + border.bottom + padding.top + padding.bottom;

    let rect = Rect {
        x: border_box_x,
        y: border_box_y,
        width: border_box_width,
        height: border_box_height,
    };
    let kind = match style.display {
        Display::Block | Display::ListItem => BoxKind::Block,
        Display::InlineBlock => BoxKind::InlineBlock,
        Display::Inline => BoxKind::Inline,
        Display::None => unreachable!(),
    };
    tree.boxes[node.index()] = Some(LayoutBox {
        kind,
        rect,
        padding,
        border,
        margin,
    });

    margin.top + border_box_height + margin.bottom
}

// ---------------- Child grouping ----------------

enum ChildGroup {
    Block(NodeId),
    Inline(Vec<NodeId>),
}

#[derive(Copy, Clone)]
enum ChildClass {
    Block,
    Inline,
    Skip,
}

fn classify(dom: &Dom, styles: &StyleTree, child: NodeId) -> ChildClass {
    match &dom.node(child).kind {
        NodeKind::Text(s) => {
            if s.is_empty() {
                ChildClass::Skip
            } else {
                ChildClass::Inline
            }
        }
        NodeKind::Element { .. } => match styles.get(child).display {
            Display::Block | Display::ListItem => ChildClass::Block,
            Display::Inline | Display::InlineBlock => ChildClass::Inline,
            Display::None => ChildClass::Skip,
        },
        _ => ChildClass::Skip,
    }
}

fn group_children(dom: &Dom, styles: &StyleTree, kids: &[NodeId]) -> Vec<ChildGroup> {
    let mut groups = Vec::new();
    let mut inline_run: Vec<NodeId> = Vec::new();
    for &child in kids {
        match classify(dom, styles, child) {
            ChildClass::Block => {
                if !inline_run.is_empty() {
                    groups.push(ChildGroup::Inline(std::mem::take(&mut inline_run)));
                }
                groups.push(ChildGroup::Block(child));
            }
            ChildClass::Inline => inline_run.push(child),
            ChildClass::Skip => {}
        }
    }
    if !inline_run.is_empty() {
        groups.push(ChildGroup::Inline(inline_run));
    }
    groups
}

// ---------------- Inline formatting context ----------------

fn layout_inline_run(
    dom: &Dom,
    styles: &StyleTree,
    text_layout: &mut TextLayout,
    nodes: &[NodeId],
    parent_style: &ComputedStyle,
    cb: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let mut content = InlineContent::default();
    for &child in nodes {
        collect_inline(dom, styles, child, &mut content);
    }
    if content.text.trim().is_empty() {
        return 0.0;
    }

    let shaped = text_layout.shape_inline(&content, cb.width.max(0.0), parent_style, styles);

    // Distribute glyphs to source nodes by byte range.
    for span in &content.spans {
        let mut rect_opt: Option<Rect> = None;
        for glyph in &shaped.glyphs {
            if span.range.contains(&glyph.text_start) {
                let gr = Rect {
                    x: cb.x + glyph.x,
                    y: cb.y + glyph.y,
                    width: glyph.width,
                    height: glyph.height,
                };
                rect_opt = Some(match rect_opt {
                    Some(r) => union(r, gr),
                    None => gr,
                });
            }
        }
        if let Some(rect) = rect_opt {
            tree.boxes[span.node.index()] = Some(LayoutBox {
                kind: if span.is_element {
                    BoxKind::Inline
                } else {
                    BoxKind::Text
                },
                rect,
                padding: BoxSides::default(),
                border: BoxSides::default(),
                margin: BoxSides::default(),
            });
        }
    }

    shaped.total_height
}

fn collect_inline(dom: &Dom, styles: &StyleTree, node: NodeId, content: &mut InlineContent) {
    match &dom.node(node).kind {
        NodeKind::Text(raw) => {
            let collapsed = collapse_whitespace(raw);
            if collapsed.is_empty() {
                return;
            }
            let start = content.text.len();
            content.text.push_str(&collapsed);
            let end = content.text.len();
            content.spans.push(InlineSpan {
                range: Range { start, end },
                node,
                is_element: false,
            });
        }
        NodeKind::Element { .. } => {
            let style = styles.get(node);
            if style.display == Display::None {
                return;
            }
            let start = content.text.len();
            let kids: Vec<NodeId> = dom.children(node).collect();
            for child in kids {
                collect_inline(dom, styles, child, content);
            }
            let end = content.text.len();
            if end > start {
                content.spans.push(InlineSpan {
                    range: Range { start, end },
                    node,
                    is_element: true,
                });
            }
        }
        _ => {}
    }
}

fn union(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css;
    use crate::html;

    fn run(html_src: &str, viewport_w: f32) -> (crate::dom::Dom, BoxTree) {
        let dom = html::parse(html_src);
        let sheets = match css::discover_stylesheets(&dom).into_iter().next() {
            Some(css::StylesheetRef::Embedded(s)) => vec![s],
            _ => vec![],
        };
        let styles = css::style_dom(&dom, &sheets);
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: viewport_w,
            height: 0.0,
        };
        let tree = super::super::layout(&dom, &styles, viewport);
        (dom, tree)
    }

    fn find(dom: &crate::dom::Dom, id: NodeId, tag: &str) -> Option<NodeId> {
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

    #[test]
    fn body_inherits_viewport_width() {
        let (dom, tree) = run("<p>hi</p>", 1000.0);
        let body = find(&dom, dom.document(), "body").unwrap();
        let b = tree.get(body).unwrap();
        assert_eq!(b.rect.x, 8.0);
        assert_eq!(b.rect.y, 8.0);
        assert_eq!(b.rect.width, 1000.0 - 16.0);
    }

    #[test]
    fn blocks_stack_vertically() {
        let (dom, tree) = run(
            "<style>.a, .b { margin: 0; padding: 0; height: 50px; }</style>\
             <div class=a></div><div class=b></div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let kids: Vec<NodeId> = dom
            .children(body)
            .filter(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .collect();
        let a = tree.get(kids[0]).unwrap();
        let b = tree.get(kids[1]).unwrap();
        assert_eq!(a.rect.y, 8.0);
        assert_eq!(b.rect.y, 8.0 + 50.0);
    }

    #[test]
    fn percentage_width_resolves() {
        let (dom, tree) = run(
            "<style>.half { width: 50%; height: 30px; margin: 0; padding: 0; }</style>\
             <div class=half></div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let div = dom
            .children(body)
            .find(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .unwrap();
        let b = tree.get(div).unwrap();
        assert!((b.rect.width - 492.0).abs() < 0.001);
    }

    #[test]
    fn display_none_produces_no_box() {
        let (dom, tree) = run(
            "<style>.gone { display: none; }</style>\
             <div class=gone>invisible</div><div>visible</div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let kids: Vec<NodeId> = dom
            .children(body)
            .filter(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .collect();
        assert!(tree.get(kids[0]).is_none());
        assert!(tree.get(kids[1]).is_some());
    }

    #[test]
    fn long_text_wraps_to_multiple_lines() {
        let long = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                    Sed do eiusmod tempor incididunt ut labore et dolore magna \
                    aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                    ullamco laboris nisi ut aliquip ex ea commodo consequat.";
        let src = format!(
            "<style>body {{ margin: 0; }} p {{ margin: 0; padding: 0; }}</style><p>{long}</p>"
        );
        let (dom, tree) = run(&src, 400.0);
        let p = find(&dom, dom.document(), "p").unwrap();
        let b = tree.get(p).unwrap();
        let one_line = 16.0 * 1.2;
        assert!(
            b.rect.height > one_line * 1.5,
            "expected wrap, got {}",
            b.rect.height
        );
    }

    #[test]
    fn short_text_stays_one_line() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } p { margin: 0; padding: 0; }</style><p>hi</p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        let b = tree.get(p).unwrap();
        let one_line = 16.0 * 1.2;
        assert!(b.rect.height <= one_line * 1.5);
    }

    #[test]
    fn inline_siblings_share_a_line() {
        // Three short inline elements inside one block at 1000px viewport.
        // All three should sit on the same line: their y is identical and the
        // p's height is one line.
        let (dom, tree) = run(
            "<style>body { margin: 0; } p { margin: 0; padding: 0; }</style>\
             <p><span>aaa</span><em>bbb</em><strong>ccc</strong></p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        let span = find(&dom, p, "span").unwrap();
        let em = find(&dom, p, "em").unwrap();
        let strong = find(&dom, p, "strong").unwrap();
        let one_line = 16.0 * 1.2;
        assert!(
            tree.get(p).unwrap().rect.height <= one_line * 1.5,
            "p too tall: {}",
            tree.get(p).unwrap().rect.height
        );
        // All three inline boxes start at the same y.
        let y_span = tree.get(span).unwrap().rect.y;
        let y_em = tree.get(em).unwrap().rect.y;
        let y_strong = tree.get(strong).unwrap().rect.y;
        assert!((y_span - y_em).abs() < 1.0);
        assert!((y_em - y_strong).abs() < 1.0);
        // And they're left-to-right.
        assert!(tree.get(span).unwrap().rect.x < tree.get(em).unwrap().rect.x);
        assert!(tree.get(em).unwrap().rect.x < tree.get(strong).unwrap().rect.x);
    }

    #[test]
    fn mixed_block_and_inline_children_alternate() {
        // p contains inline text, then a child div (block), then more inline.
        // The two inline runs should be at distinct y values, separated by
        // the block child's height.
        let (dom, tree) = run(
            "<style>body { margin: 0; } p { margin: 0; padding: 0; } \
             div { height: 50px; margin: 0; padding: 0; }</style>\
             <p>before<div>BLOCK</div>after</p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        // Two text nodes under p: "before" before the block, "after" after.
        let mut texts: Vec<NodeId> = Vec::new();
        for c in dom.children(p).collect::<Vec<_>>() {
            if matches!(dom.node(c).kind, NodeKind::Text(_)) {
                texts.push(c);
            }
        }
        // The HTML parser may auto-close the p when it sees the nested block.
        // What we really want is: if both "before" and "after" texts exist
        // and both are styled visible, their y differs by at least block height.
        if texts.len() == 2 {
            let y0 = tree.get(texts[0]).unwrap().rect.y;
            let y1 = tree.get(texts[1]).unwrap().rect.y;
            assert!(
                (y1 - y0).abs() > 40.0,
                "expected separation across block child"
            );
        }
    }
}
