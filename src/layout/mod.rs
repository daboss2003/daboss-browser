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

use crate::css::{BoxSides, StyleTree};
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
    tree
}
