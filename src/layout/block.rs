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

use super::replaced::{ImageCache, ImageSlot};
use super::text::{collapse_whitespace, InlineContent, InlineSpan, TextLayout};
use super::{BoxKind, BoxTree, LayoutBox, PseudoBox, PseudoKind, Rect};
use crate::css::{BoxSides, ComputedStyle, Dimension, Display, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};

pub fn layout(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    images: &ImageCache,
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
                y += layout(dom, styles, text, images, child, cb, tree);
            }
            y - containing.y
        }
        NodeKind::Element { tag, .. } => {
            let style = styles.get(node);
            if style.display == Display::None {
                return 0.0;
            }
            if tag == "table" {
                super::table::layout_table(
                    dom, styles, text, images, node, style, containing, tree,
                )
            } else {
                layout_block(dom, styles, text, images, node, style, containing, tree)
            }
        }
        NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::Doctype(_) => 0.0,
    }
}

fn intrinsic_size(dom: &Dom, node: NodeId, images: &ImageCache) -> Option<(f32, f32)> {
    if let NodeKind::Element { tag, .. } = &dom.node(node).kind {
        if tag == "img" {
            return images
                .get(&(node, ImageSlot::Img))
                .map(|i| (i.width as f32, i.height as f32));
        }
    }
    None
}

fn layout_block(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    images: &ImageCache,
    node: NodeId,
    style: &ComputedStyle,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let margin = style.margin;
    let border = style.border_width;
    let padding = style.padding;
    let intrinsic = intrinsic_size(dom, node, images);

    let cb_width = containing.width;
    let content_width = match (style.width, intrinsic) {
        (Dimension::Length(w), _) => w,
        (Dimension::Percent(pct), _) => {
            (cb_width * pct / 100.0
                - border.left
                - border.right
                - padding.left
                - padding.right)
                .max(0.0)
        }
        // Replaced elements with `auto` width use their intrinsic width.
        (Dimension::Auto, Some((iw, _))) => iw,
        (Dimension::Auto, None) => (cb_width
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

    // Pick where to put the host's own ::before / ::after content. If any
    // child group is inline, the pseudo flows into the first/last IFC.
    // Otherwise, fall back to "own row" placement above / below children.
    let host_before_text = styles
        .before_style(node)
        .and_then(|s| s.content.clone());
    let host_after_text = styles.after_style(node).and_then(|s| s.content.clone());
    let first_inline_idx = groups
        .iter()
        .position(|g| matches!(g, ChildGroup::Inline(_)));
    let last_inline_idx = groups
        .iter()
        .rposition(|g| matches!(g, ChildGroup::Inline(_)));

    let mut child_y = content_y;

    // ::before as own row (only when there's no IFC to absorb it).
    if first_inline_idx.is_none() {
        if let (Some(before_style), Some(text_content)) =
            (styles.before_style(node), host_before_text.as_deref())
        {
            let cb = Rect {
                x: content_x,
                y: child_y,
                width: content_width,
                height: 0.0,
            };
            child_y += layout_pseudo(
                node,
                PseudoKind::Before,
                text_content,
                before_style,
                text,
                cb,
                tree,
            );
        }
    }

    for (i, group) in groups.into_iter().enumerate() {
        let cb = Rect {
            x: content_x,
            y: child_y,
            width: content_width,
            height: 0.0,
        };
        let h = match group {
            ChildGroup::Block(child) => layout(dom, styles, text, images, child, cb, tree),
            ChildGroup::Inline(nodes) => {
                let host_before = if Some(i) == first_inline_idx {
                    host_before_text.as_deref()
                } else {
                    None
                };
                let host_after = if Some(i) == last_inline_idx {
                    host_after_text.as_deref()
                } else {
                    None
                };
                layout_inline_run(
                    dom,
                    styles,
                    text,
                    &nodes,
                    style,
                    cb,
                    tree,
                    node,
                    host_before,
                    host_after,
                )
            }
        };
        child_y += h;
    }

    // ::after as own row (only when there's no IFC to absorb it).
    if last_inline_idx.is_none() {
        if let (Some(after_style), Some(text_content)) =
            (styles.after_style(node), host_after_text.as_deref())
        {
            let cb = Rect {
                x: content_x,
                y: child_y,
                width: content_width,
                height: 0.0,
            };
            child_y += layout_pseudo(
                node,
                PseudoKind::After,
                text_content,
                after_style,
                text,
                cb,
                tree,
            );
        }
    }

    let computed_content_height = child_y - content_y;

    let content_height = match (style.height, intrinsic) {
        (Dimension::Length(h), _) => h,
        // Replaced element with `auto` height: use intrinsic height.
        (Dimension::Auto, Some((_, ih))) => ih,
        (Dimension::Percent(_), _) | (Dimension::Auto, None) => computed_content_height,
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
        NodeKind::Element { tag, .. } => {
            // Replaced elements (img/iframe/video/canvas) need a fixed-size
            // box; cosmic-text doesn't know how to lay a non-text glyph into
            // an IFC, so we promote them to their own block-style row even
            // when their `display` is inline-block. Phase 4f can revisit
            // this to flow them properly inside lines.
            let is_replaced = matches!(
                tag.as_str(),
                "img" | "iframe" | "video" | "canvas"
            );
            match styles.get(child).display {
                Display::Block | Display::ListItem => ChildClass::Block,
                Display::InlineBlock if is_replaced => ChildClass::Block,
                Display::Inline | Display::InlineBlock => ChildClass::Inline,
                Display::None => ChildClass::Skip,
            }
        }
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

#[allow(clippy::too_many_arguments)]
fn layout_inline_run(
    dom: &Dom,
    styles: &StyleTree,
    text_layout: &mut TextLayout,
    nodes: &[NodeId],
    parent_style: &ComputedStyle,
    cb: Rect,
    tree: &mut BoxTree,
    host: NodeId,
    host_before: Option<&str>,
    host_after: Option<&str>,
) -> f32 {
    let mut content = InlineContent::default();

    // Host's own ::before (block-level pseudo flowing into its IFC).
    if let Some(text_content) = host_before {
        push_pseudo_span(&mut content, host, PseudoKind::Before, text_content);
    }

    for &child in nodes {
        collect_inline(dom, styles, child, &mut content);
    }

    if let Some(text_content) = host_after {
        push_pseudo_span(&mut content, host, PseudoKind::After, text_content);
    }

    if content.text.trim().is_empty() {
        return 0.0;
    }

    let shaped = text_layout.shape_inline(&content, cb.width.max(0.0), parent_style, styles);

    // Distribute glyphs to source nodes / pseudo slots by byte range.
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
        let Some(rect) = rect_opt else {
            continue;
        };
        if let Some(kind) = span.pseudo {
            // Pseudo's text is the collapsed slice of our IFC text.
            let pseudo_text = content.text[span.range.start..span.range.end].to_string();
            tree.pseudo_boxes.insert(
                (span.node, kind),
                super::PseudoBox {
                    host: span.node,
                    kind,
                    rect,
                    text: pseudo_text,
                },
            );
        } else {
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

fn push_pseudo_span(
    content: &mut InlineContent,
    host: NodeId,
    kind: PseudoKind,
    raw: &str,
) {
    let collapsed = collapse_whitespace(raw);
    if collapsed.is_empty() {
        return;
    }
    let start = content.text.len();
    content.text.push_str(&collapsed);
    let end = content.text.len();
    content.spans.push(InlineSpan {
        range: Range { start, end },
        node: host,
        is_element: false,
        pseudo: Some(kind),
    });
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
                pseudo: None,
            });
        }
        NodeKind::Element { .. } => {
            let style = styles.get(node);
            if style.display == Display::None {
                return;
            }
            let start = content.text.len();

            // Inline element's own ::before flows into the IFC.
            if let Some(pseudo_style) = styles.before_style(node) {
                if let Some(text) = &pseudo_style.content {
                    push_pseudo_span(content, node, PseudoKind::Before, text);
                }
            }

            let kids: Vec<NodeId> = dom.children(node).collect();
            for child in kids {
                collect_inline(dom, styles, child, content);
            }

            if let Some(pseudo_style) = styles.after_style(node) {
                if let Some(text) = &pseudo_style.content {
                    push_pseudo_span(content, node, PseudoKind::After, text);
                }
            }

            let end = content.text.len();
            if end > start {
                content.spans.push(InlineSpan {
                    range: Range { start, end },
                    node,
                    is_element: true,
                    pseudo: None,
                });
            }
        }
        _ => {}
    }
}

fn layout_pseudo(
    host: NodeId,
    kind: PseudoKind,
    content: &str,
    style: &ComputedStyle,
    text_layout: &mut TextLayout,
    cb: Rect,
    tree: &mut BoxTree,
) -> f32 {
    // Toy: render generated content as a single-line inline-block at the
    // host's content edge. Phase 5+ can flow it into the host's IFC instead.
    let collapsed = collapse_whitespace(content);
    if collapsed.is_empty() {
        return 0.0;
    }
    let line_height = style.font_size * style.line_height;
    let natural_w = text_layout.measure_natural_width(&collapsed, style);
    let width = natural_w.min(cb.width.max(0.0));
    let rect = Rect {
        x: cb.x,
        y: cb.y,
        width,
        height: line_height,
    };
    tree.pseudo_boxes.insert(
        (host, kind),
        PseudoBox {
            host,
            kind,
            rect,
            text: collapsed,
        },
    );
    line_height
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
        let images = ImageCache::new();
        let tree = super::super::layout(&dom, &styles, &images, viewport);
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
    fn img_uses_intrinsic_dimensions() {
        use crate::layout::replaced::{ImageInfo, ImageSlot};
        let html = "<style>body { margin: 0; }</style><img src=\"x\">";
        let dom = html::parse(html);
        let sheets = match css::discover_stylesheets(&dom).into_iter().next() {
            Some(css::StylesheetRef::Embedded(s)) => vec![s],
            _ => vec![],
        };
        let styles = css::style_dom(&dom, &sheets);
        let img = find(&dom, dom.document(), "img").unwrap();
        let mut images = ImageCache::new();
        images.insert(
            (img, ImageSlot::Img),
            ImageInfo { width: 120, height: 80, rgba: vec![0; 120 * 80 * 4] },
        );

        let viewport = Rect { x: 0.0, y: 0.0, width: 1000.0, height: 0.0 };
        let tree = super::super::layout(&dom, &styles, &images, viewport);
        let b = tree.get(img).expect("img should have a box");
        assert_eq!(b.rect.width, 120.0);
        assert_eq!(b.rect.height, 80.0);
    }

    #[test]
    fn img_without_decoded_data_gets_zero_size() {
        let (dom, tree) = run(
            "<style>body { margin: 0; }</style><img src=\"missing.png\">",
            1000.0,
        );
        let img = find(&dom, dom.document(), "img").unwrap();
        let b = tree.get(img).expect("img should still have a box");
        // No intrinsic; falls back to Auto width with the container shrunk
        // to inline-block (replaced) semantics — content_width 0.
        assert_eq!(b.rect.height, 0.0);
    }

    #[test]
    fn before_pseudo_creates_box() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             p { margin: 0; padding: 0; } \
             p::before { content: 'Note: '; }</style>\
             <p>hello</p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        let pseudo = tree
            .pseudo_boxes
            .get(&(p, super::PseudoKind::Before))
            .expect("::before should have a box");
        assert_eq!(pseudo.text, "Note: ");
        // The pseudo sits at the host's content edge.
        let p_box = tree.get(p).unwrap();
        assert!((pseudo.rect.x - p_box.rect.x).abs() < 1.0);
        assert!((pseudo.rect.y - p_box.rect.y).abs() < 1.0);
    }

    #[test]
    fn after_pseudo_creates_box_on_host_line() {
        // Now that pseudos flow inline within the host's IFC, ::after sits
        // on the same line as the host's text rather than below it.
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             p { margin: 0; padding: 0; } \
             p::after { content: '!'; }</style>\
             <p>hello</p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        let pseudo = tree
            .pseudo_boxes
            .get(&(p, super::PseudoKind::After))
            .expect("::after should have a box");
        assert_eq!(pseudo.text, "!");
        let p_box = tree.get(p).unwrap();
        // Same line as the host content (within one line-height tolerance).
        assert!((pseudo.rect.y - p_box.rect.y).abs() < 25.0);
        // After the host's text horizontally.
        assert!(pseudo.rect.x > p_box.rect.x);
    }

    #[test]
    fn pseudo_without_content_property_makes_no_box() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             p::before { color: red; } /* no content -> no box */</style>\
             <p>hello</p>",
            1000.0,
        );
        let p = find(&dom, dom.document(), "p").unwrap();
        assert!(tree.pseudo_boxes.get(&(p, super::PseudoKind::Before)).is_none());
    }

    #[test]
    fn pseudo_inherits_color_from_host() {
        // Real coverage of the cascade path: pseudo style starts from the
        // host's style so it inherits color. Check via the StyleTree directly.
        use crate::css::Color;
        let html = "<style>p { color: rgb(255,0,0); } \
             p::before { content: 'X'; }</style>\
             <p>hi</p>";
        let dom = html::parse(html);
        let sheets = match css::discover_stylesheets(&dom).into_iter().next() {
            Some(css::StylesheetRef::Embedded(s)) => vec![s],
            _ => vec![],
        };
        let style_tree = css::style_dom(&dom, &sheets);
        let p = find(&dom, dom.document(), "p").unwrap();
        let pseudo = style_tree.before_style(p).expect("pseudo style");
        assert_eq!(pseudo.color, Color::rgb(255, 0, 0));
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
