//! Layout. Walks the styled DOM and assigns every visible node a rectangle
//! `(x, y, width, height)` in pixels.
//!
//! Phase 4a (this file) implements **block layout only**: every visible
//! element is treated as a block, stacked vertically inside its containing
//! block. Inline elements (`<span>`, `<em>`, `<a>`) are stacked vertically
//! too rather than sharing lines — Phase 4b fixes this with cosmic-text
//! shaping and line boxes. Text nodes get a single-line height for now.

mod block;
mod text;

use crate::css::{BoxSides, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};
use text::TextLayout;

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

#[derive(Debug)]
#[allow(dead_code)] // viewport consumed by paint scrolling / scale in phase 5
pub struct BoxTree {
    pub boxes: Vec<Option<LayoutBox>>, // indexed by NodeId.index()
    pub viewport: Rect,
}

impl BoxTree {
    pub fn get(&self, id: NodeId) -> Option<&LayoutBox> {
        self.boxes.get(id.index()).and_then(|b| b.as_ref())
    }

    /// Prints the DOM with its rect annotation per element. Used as the
    /// stdout demo from Phase 4 onwards (replaces `Dom::print`).
    pub fn print(&self, dom: &Dom) {
        self.print_node(dom, dom.document(), 0);
    }

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
                    return; // skip whitespace-only nodes in print
                }
                format!("\"{}\"", truncate(trimmed, 60))
            }
            NodeKind::Comment(_) => return,
            NodeKind::Doctype(s) => format!("<!DOCTYPE {s}>"),
        };
        println!("{indent}{label}{rect_str}");
        let kids: Vec<NodeId> = dom.children(node).collect();
        for c in kids {
            self.print_node(dom, c, depth + 1);
        }
    }
}

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

pub fn layout(dom: &Dom, styles: &StyleTree, viewport: Rect) -> BoxTree {
    let mut tree = BoxTree {
        boxes: vec![None; styles.styles.len()],
        viewport,
    };
    let mut text = TextLayout::new();
    block::layout(dom, styles, &mut text, dom.document(), viewport, &mut tree);
    tree
}
