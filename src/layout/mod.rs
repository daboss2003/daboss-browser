//! Layout. Walks the styled DOM and assigns every visible node a rectangle
//! `(x, y, width, height)` in pixels.
//!
//! Phase 4a (this file) implements **block layout only**: every visible
//! element is treated as a block, stacked vertically inside its containing
//! block. Inline elements (`<span>`, `<em>`, `<a>`) are stacked vertically
//! too rather than sharing lines — Phase 4b fixes this with cosmic-text
//! shaping and line boxes. Text nodes get a single-line height for now.

mod block;
mod flex;
mod grid;
mod replaced;
mod table;
mod text;

use std::collections::HashMap;

use crate::css::{AnchorRef, AnchorSide, BoxSides, Position, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};
use text::TextLayout;

#[allow(unused_imports)] // ImageInfo will be re-exported when paint consumes rgba
pub use replaced::{decode_image, ImageCache, ImageInfo, ImageSlot};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Inline and Anonymous become real in phase 4b/4c
pub enum BoxKind {
    Block,
    Inline,
    InlineBlock,
    Text,
    Anonymous,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // kind / padding / border / margin consumed by paint in phase 5
pub struct LayoutBox {
    pub kind: BoxKind,
    /// Border-box: outer edge of border on each side. Content rect is `rect`
    /// minus `border` and `padding`; margin rect is `rect` expanded by margin.
    pub rect: Rect,
    pub padding: BoxSides,
    pub border: BoxSides,
    pub margin: BoxSides,
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
pub enum PseudoKind {
    Before,
    After,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // text retained for paint to render generated content
pub struct PseudoBox {
    pub host: NodeId,
    pub kind: PseudoKind,
    pub rect: Rect,
    pub text: String,
}

#[derive(Debug)]
#[allow(dead_code)] // viewport consumed by paint scrolling / scale in phase 5
pub struct BoxTree {
    pub boxes: Vec<Option<LayoutBox>>, // indexed by NodeId.index()
    pub viewport: Rect,
    /// Generated-content boxes for `::before` / `::after`. Keyed by the
    /// originating element and which pseudo (Before / After).
    pub pseudo_boxes: HashMap<(NodeId, PseudoKind), PseudoBox>,
}

impl BoxTree {
    pub fn get(&self, id: NodeId) -> Option<&LayoutBox> {
        self.boxes.get(id.index()).and_then(|b| b.as_ref())
    }

    /// Prints the DOM with its rect annotation per element. Useful for
    /// debugging and used by the `--png` headless flow's verbose output.
    #[allow(dead_code)]
    pub fn print(&self, dom: &Dom) {
        self.print_node(dom, dom.document(), 0);
    }

    #[allow(dead_code)]
    fn print_node(&self, dom: &Dom, node: NodeId, depth: usize) {
        let indent = "  ".repeat(depth);
        let rect_str = self.get(node).map_or(String::new(), |b| {
            format!(
                "  [{}, {}, {}x{}]",
                b.rect.x as i32,
                b.rect.y as i32,
                b.rect.width as i32,
                b.rect.height as i32
            )
        });
        let label = match &dom.node(node).kind {
            NodeKind::Document => "#document".to_string(),
            NodeKind::Element { tag, .. } => format!("<{tag}>"),
            NodeKind::Text(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return;
                }
                format!("\"{}\"", truncate(trimmed, 60))
            }
            NodeKind::Comment(_) => return,
            NodeKind::Doctype(s) => format!("<!DOCTYPE {s}>"),
        };
        println!("{indent}{label}{rect_str}");

        // ::before pseudo-element prints inside its host's section, before
        // the host's children.
        self.print_pseudo(node, PseudoKind::Before, depth + 1);
        let kids: Vec<NodeId> = dom.children(node).collect();
        for c in kids {
            self.print_node(dom, c, depth + 1);
        }
        self.print_pseudo(node, PseudoKind::After, depth + 1);
    }

    #[allow(dead_code)]
    fn print_pseudo(&self, host: NodeId, kind: PseudoKind, depth: usize) {
        let Some(p) = self.pseudo_boxes.get(&(host, kind)) else {
            return;
        };
        let indent = "  ".repeat(depth);
        let label = match kind {
            PseudoKind::Before => "::before",
            PseudoKind::After => "::after",
        };
        println!(
            "{indent}{label} \"{}\"  [{}, {}, {}x{}]",
            truncate(&p.text, 60),
            p.rect.x as i32,
            p.rect.y as i32,
            p.rect.width as i32,
            p.rect.height as i32
        );
    }
}

#[allow(dead_code)]
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// Find the deepest box that contains the point `(x, y)` in *page* coords
/// (so the caller should add scroll offset before calling). Uses the
/// painters'-algorithm ordering: descendants paint on top of ancestors and
/// later siblings paint on top of earlier ones, so the deepest match wins.
pub fn hit_test(dom: &Dom, tree: &BoxTree, x: f32, y: f32) -> Option<NodeId> {
    let mut found = None;
    hit_test_walk(dom, tree, dom.document(), x, y, &mut found);
    found
}

fn hit_test_walk(
    dom: &Dom,
    tree: &BoxTree,
    node: NodeId,
    x: f32,
    y: f32,
    out: &mut Option<NodeId>,
) {
    if let Some(b) = tree.get(node) {
        let inside = x >= b.rect.x
            && x < b.rect.x + b.rect.width
            && y >= b.rect.y
            && y < b.rect.y + b.rect.height;
        if inside {
            *out = Some(node);
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for c in kids {
        hit_test_walk(dom, tree, c, x, y, out);
    }
}

pub fn layout(
    dom: &Dom,
    styles: &StyleTree,
    images: &ImageCache,
    viewport: Rect,
) -> BoxTree {
    let mut tree = BoxTree {
        boxes: vec![None; styles.styles.len()],
        viewport,
        pseudo_boxes: HashMap::new(),
    };
    let mut text = TextLayout::new();
    block::layout(dom, styles, &mut text, images, dom.document(), viewport, &mut tree);
    apply_anchor_positioning(dom, styles, &mut tree);
    tree
}

/// Post-layout pass that resolves `anchor()` references on
/// absolutely / fixed-positioned elements. We first collect every node
/// that registered an `anchor-name`, then walk again and, for each
/// positioned element whose `top`/`right`/`bottom`/`left` was an
/// `anchor(...)` call, compute the target edge coordinate from the
/// anchor's box rect and shift the element's subtree.
fn apply_anchor_positioning(dom: &Dom, styles: &StyleTree, tree: &mut BoxTree) {
    // Collect anchors: name → rect.
    let mut anchors: HashMap<String, Rect> = HashMap::new();
    collect_anchors(dom, styles, tree, dom.document(), &mut anchors);
    if anchors.is_empty() {
        return;
    }

    // Walk again, resolving inset anchor() calls for positioned items.
    let mut todo: Vec<NodeId> = vec![dom.document()];
    while let Some(n) = todo.pop() {
        for c in dom.children(n) {
            todo.push(c);
        }
        if !matches!(dom.node(n).kind, NodeKind::Element { .. }) {
            continue;
        }
        let style = styles.get(n);
        if !matches!(style.position, Position::Absolute | Position::Fixed) {
            continue;
        }
        if style.anchor_top.is_none()
            && style.anchor_right.is_none()
            && style.anchor_bottom.is_none()
            && style.anchor_left.is_none()
        {
            continue;
        }
        let Some(b) = tree.boxes.get(n.index()).and_then(|s| s.as_ref()).cloned() else {
            continue;
        };
        let resolve = |aref: &Option<AnchorRef>| -> Option<Rect> {
            let aref = aref.as_ref()?;
            let name = aref.name.clone().or_else(|| style.position_anchor.clone())?;
            anchors.get(&name).copied()
        };

        // Default target = current position.
        let mut target_x = b.rect.x;
        let mut target_y = b.rect.y;

        // Horizontal: left wins over right (same precedence as plain inset).
        if let Some(left_ref) = &style.anchor_left {
            if let Some(arect) = resolve(&Some(left_ref.clone())) {
                target_x = anchor_edge_x(arect, left_ref.side);
            }
        } else if let Some(right_ref) = &style.anchor_right {
            if let Some(arect) = resolve(&Some(right_ref.clone())) {
                let edge = anchor_edge_x(arect, right_ref.side);
                target_x = edge - b.rect.width;
            }
        }
        // Vertical: top wins over bottom.
        if let Some(top_ref) = &style.anchor_top {
            if let Some(arect) = resolve(&Some(top_ref.clone())) {
                target_y = anchor_edge_y(arect, top_ref.side);
            }
        } else if let Some(bot_ref) = &style.anchor_bottom {
            if let Some(arect) = resolve(&Some(bot_ref.clone())) {
                let edge = anchor_edge_y(arect, bot_ref.side);
                target_y = edge - b.rect.height;
            }
        }

        let dx = target_x - b.rect.x;
        let dy = target_y - b.rect.y;
        if dx.abs() > 0.001 || dy.abs() > 0.001 {
            shift_subtree_xy(dom, n, dx, dy, tree);
        }
    }
}

fn collect_anchors(
    dom: &Dom,
    styles: &StyleTree,
    tree: &BoxTree,
    node: NodeId,
    out: &mut HashMap<String, Rect>,
) {
    if let NodeKind::Element { .. } = dom.node(node).kind {
        let style = styles.get(node);
        if let Some(name) = &style.anchor_name {
            if let Some(Some(b)) = tree.boxes.get(node.index()) {
                out.entry(name.clone()).or_insert(b.rect);
            }
        }
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for c in kids {
        collect_anchors(dom, styles, tree, c, out);
    }
}

fn anchor_edge_x(rect: Rect, side: AnchorSide) -> f32 {
    match side {
        AnchorSide::Left | AnchorSide::Start => rect.x,
        AnchorSide::Right | AnchorSide::End => rect.x + rect.width,
        AnchorSide::Center => rect.x + rect.width / 2.0,
        AnchorSide::Top | AnchorSide::Bottom => rect.x, // nonsensical, fall back
    }
}

fn anchor_edge_y(rect: Rect, side: AnchorSide) -> f32 {
    match side {
        AnchorSide::Top | AnchorSide::Start => rect.y,
        AnchorSide::Bottom | AnchorSide::End => rect.y + rect.height,
        AnchorSide::Center => rect.y + rect.height / 2.0,
        AnchorSide::Left | AnchorSide::Right => rect.y, // nonsensical, fall back
    }
}

fn shift_subtree_xy(dom: &Dom, node: NodeId, dx: f32, dy: f32, tree: &mut BoxTree) {
    if let Some(b) = tree.boxes.get_mut(node.index()).and_then(|s| s.as_mut()) {
        b.rect.x += dx;
        b.rect.y += dy;
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for c in kids {
        shift_subtree_xy(dom, c, dx, dy, tree);
    }
}

#[cfg(test)]
mod anchor_tests {
    use super::*;
    use crate::css;
    use crate::html;

    fn run(html_src: &str, vw: f32) -> (Dom, BoxTree) {
        let dom = html::parse(html_src);
        let sheets = match css::discover_stylesheets(&dom).into_iter().next() {
            Some(css::StylesheetRef::Embedded(s)) => vec![s],
            _ => vec![],
        };
        let styles = css::style_dom(&dom, &sheets);
        let images = ImageCache::new();
        let tree = layout(
            &dom,
            &styles,
            &images,
            Rect { x: 0.0, y: 0.0, width: vw, height: 0.0 },
        );
        (dom, tree)
    }

    fn find_class(dom: &Dom, root: NodeId, class: &str) -> Option<NodeId> {
        if let NodeKind::Element { attrs, .. } = &dom.node(root).kind {
            let cls = attrs.iter().find(|(k, _)| k == "class").map(|(_, v)| v.as_str()).unwrap_or("");
            if cls.split_ascii_whitespace().any(|c| c == class) {
                return Some(root);
            }
        }
        for c in dom.children(root).collect::<Vec<_>>() {
            if let Some(f) = find_class(dom, c, class) {
                return Some(f);
            }
        }
        None
    }

    #[test]
    fn anchor_top_aligns_to_anchor_bottom_edge() {
        // Anchor box sits at y=20, height=40 → bottom edge y=60.
        // The popup is `position: absolute; top: anchor(--a bottom)`
        // so its top should snap to y=60.
        let (dom, tree) = run(
            "<style>body { margin: 0; padding: 0; } \
             .anchor { anchor-name: --a; width: 80px; height: 40px; \
                       margin-top: 20px; } \
             .popup  { position: absolute; top: anchor(--a bottom); \
                       left: anchor(--a left); width: 100px; height: 30px; }</style>\
             <div class=anchor></div>\
             <div class=popup></div>",
            500.0,
        );
        let popup = find_class(&dom, dom.document(), "popup").expect("popup");
        let r = tree.get(popup).unwrap().rect;
        assert!(
            (r.y - 60.0).abs() < 1.0,
            "popup.y should be 60 (anchor bottom), got {}",
            r.y
        );
        assert!(
            (r.x - 0.0).abs() < 1.0,
            "popup.x should be 0 (anchor left), got {}",
            r.x
        );
    }

    #[test]
    fn anchor_position_anchor_supplies_default_name() {
        // No name inside anchor() — falls back to position-anchor.
        let (dom, tree) = run(
            "<style>body { margin: 0; padding: 0; } \
             .anchor { anchor-name: --a; width: 50px; height: 20px; } \
             .popup  { position: absolute; position-anchor: --a; \
                       top: anchor(bottom); left: anchor(right); \
                       width: 40px; height: 10px; }</style>\
             <div class=anchor></div>\
             <div class=popup></div>",
            500.0,
        );
        let popup = find_class(&dom, dom.document(), "popup").expect("popup");
        let r = tree.get(popup).unwrap().rect;
        assert!((r.y - 20.0).abs() < 1.0, "popup.y={} expected 20", r.y);
        assert!((r.x - 50.0).abs() < 1.0, "popup.x={} expected 50", r.x);
    }

    #[test]
    fn anchor_right_pulls_element_left_edge_back_by_width() {
        // `right: anchor(--a right)` means popup's right edge sits at
        // the anchor's right edge → popup.x = anchor.right - popup.width.
        let (dom, tree) = run(
            "<style>body { margin: 0; padding: 0; } \
             .anchor { anchor-name: --a; width: 200px; height: 30px; } \
             .popup  { position: absolute; right: anchor(--a right); \
                       top: anchor(--a top); width: 60px; height: 20px; }</style>\
             <div class=anchor></div>\
             <div class=popup></div>",
            500.0,
        );
        let popup = find_class(&dom, dom.document(), "popup").expect("popup");
        let r = tree.get(popup).unwrap().rect;
        // anchor.x = 0, anchor.width = 200 → right edge = 200; popup.x = 200 - 60 = 140.
        assert!((r.x - 140.0).abs() < 1.0, "popup.x={} expected 140", r.x);
    }
}
