//! Block layout. Recursive walk that produces a border-box rectangle for
//! every visible element and stacks them vertically inside their containing
//! block. The return value of each call is the *outer* height consumed
//! (margin-box), which the parent uses to position the next sibling.
//!
//! Limits in Phase 4a:
//! - inline elements are laid out as if they were blocks (one per line);
//!   Phase 4b replaces this with line-box layout via `cosmic-text`.
//! - text nodes always occupy one line of `font_size * line_height`.
//! - margin collapsing between adjacent blocks is **not** implemented;
//!   margins simply add. CSS does collapse them, but the visual difference
//!   is small for a toy.
//! - `box-sizing: border-box` is not honored; we assume the default
//!   `content-box` where `width` is the content width.

use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{BoxSides, ComputedStyle, Dimension, Display, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};

pub fn layout(
    dom: &Dom,
    styles: &StyleTree,
    node: NodeId,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    match &dom.node(node).kind {
        NodeKind::Document => layout_children(dom, styles, node, containing, tree),
        NodeKind::Element { .. } => {
            let style = styles.get(node);
            if style.display == Display::None {
                return 0.0;
            }
            layout_block(dom, styles, node, style, containing, tree)
        }
        NodeKind::Text(s) => {
            if s.trim().is_empty() {
                return 0.0; // collapse whitespace-only text
            }
            let style = styles.get(node);
            layout_text(node, style, containing, tree)
        }
        NodeKind::Comment(_) | NodeKind::Doctype(_) => 0.0,
    }
}

fn layout_children(
    dom: &Dom,
    styles: &StyleTree,
    node: NodeId,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let mut y = containing.y;
    let kids: Vec<NodeId> = dom.children(node).collect();
    for child in kids {
        let cb = Rect { y, ..containing };
        let h = layout(dom, styles, child, cb, tree);
        y += h;
    }
    y - containing.y
}

fn layout_block(
    dom: &Dom,
    styles: &StyleTree,
    node: NodeId,
    style: &ComputedStyle,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let margin = style.margin;
    let border = style.border_width;
    let padding = style.padding;

    let cb_width = containing.width;

    // Content width. Defaults to "fill the container" when auto.
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

    // Lay out children inside the content rect.
    let mut child_y = content_y;
    let kids: Vec<NodeId> = dom.children(node).collect();
    for child in kids {
        let cb = Rect {
            x: content_x,
            y: child_y,
            width: content_width,
            height: 0.0,
        };
        let h = layout(dom, styles, child, cb, tree);
        child_y += h;
    }
    let computed_content_height = child_y - content_y;

    // Explicit height wins; otherwise auto = sum of children.
    let content_height = match style.height {
        Dimension::Length(h) => h,
        // Percent heights need an explicit containing-block height (which we
        // don't track for the toy). Fall back to auto.
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

fn layout_text(
    node: NodeId,
    style: &ComputedStyle,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let height = style.font_size * style.line_height;
    let rect = Rect {
        x: containing.x,
        y: containing.y,
        width: containing.width,
        height,
    };
    tree.boxes[node.index()] = Some(LayoutBox {
        kind: BoxKind::Text,
        rect,
        padding: BoxSides::default(),
        border: BoxSides::default(),
        margin: BoxSides::default(),
    });
    height
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
        // body default UA: 8px margin all sides.
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
        assert_eq!(a.rect.height, 50.0);
        assert_eq!(b.rect.height, 50.0);
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
        // body content width is 1000 - 16 = 984; half of that is 492.
        assert!((b.rect.width - 492.0).abs() < 0.001);
    }

    #[test]
    fn padding_expands_border_box() {
        let (dom, tree) = run(
            "<style>.x { width: 100px; height: 100px; padding: 20px; margin: 0; }</style>\
             <div class=x></div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let div = dom
            .children(body)
            .find(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .unwrap();
        let b = tree.get(div).unwrap();
        // width is content width; padding adds on top.
        assert_eq!(b.rect.width, 140.0);
        assert_eq!(b.rect.height, 140.0);
        assert_eq!(b.padding.top, 20.0);
    }

    #[test]
    fn margin_shifts_position() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } .x { margin: 30px; width: 100px; height: 100px; padding: 0; }</style>\
             <div class=x></div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let div = dom
            .children(body)
            .find(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .unwrap();
        let b = tree.get(div).unwrap();
        assert_eq!(b.rect.x, 30.0);
        assert_eq!(b.rect.y, 30.0);
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
    fn nested_block_inherits_content_width() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } .outer { padding: 10px; width: 200px; } \
             .inner { margin: 0; padding: 0; height: 30px; }</style>\
             <div class=outer><div class=inner></div></div>",
            1000.0,
        );
        let body = find(&dom, dom.document(), "body").unwrap();
        let outer = dom
            .children(body)
            .find(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .unwrap();
        let inner = dom
            .children(outer)
            .find(|id| matches!(dom.node(*id).kind, NodeKind::Element { .. }))
            .unwrap();
        let b = tree.get(inner).unwrap();
        // outer has content width 200, padding 10 each side. inner fills the
        // 200px content (auto width inside content area, no margins).
        assert_eq!(b.rect.x, 10.0); // outer padding-left
        assert_eq!(b.rect.width, 200.0); // outer content width
    }
}
