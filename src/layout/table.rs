//! Table layout.
//!
//! Two-pass algorithm:
//!  1. Walk `<tr>` descendants, placing each `<td>` / `<th>` into a virtual
//!     grid; track per-column "rows remaining" to skip cells already
//!     occupied by an earlier `rowspan`.
//!  2. Equal-width columns (the toy simplification — real CSS measures
//!     intrinsic min/max widths per column and distributes available space).
//!     Then lay out each cell at its grid position with `cell_x = col*W`,
//!     `cell_y = sum(row_heights[0..row])`; row height is the max
//!     border-box height of cells starting in that row.
//!  3. After all rows are laid out, fix the height of every cell with
//!     `rowspan > 1` to span the sum of its row heights.
//!
//! Skipped: `border-collapse`, `border-spacing`, `<caption>`, `<col>` /
//! `<colgroup>` widths, `table-layout: fixed`, intrinsic column sizing.

use super::block;
use super::replaced::ImageCache;
use super::text::TextLayout;
use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{ComputedStyle, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};

#[derive(Debug)]
struct PlacedCell {
    node: NodeId,
    row: usize,
    col: usize,
    col_span: usize,
    row_span: usize,
}

pub fn layout_table(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    images: &ImageCache,
    table_node: NodeId,
    style: &ComputedStyle,
    containing: Rect,
    tree: &mut BoxTree,
) -> f32 {
    let margin = style.margin;
    let border = style.border_width;
    let padding = style.padding;

    let rows = find_rows(dom, table_node);
    let (placed, max_cols) = build_grid(dom, &rows);

    // Compute the box geometry up-front so an empty / degenerate table still
    // gets a (0, 0) box at its containing position.
    let cb_width = containing.width;
    let content_width = (cb_width
        - margin.left
        - margin.right
        - border.left
        - border.right
        - padding.left
        - padding.right)
        .max(0.0);
    let border_box_x = containing.x + margin.left;
    let border_box_y = containing.y + margin.top;
    let content_x = border_box_x + border.left + padding.left;
    let content_y = border_box_y + border.top + padding.top;

    if rows.is_empty() || max_cols == 0 {
        let rect = Rect {
            x: border_box_x,
            y: border_box_y,
            width: 0.0,
            height: 0.0,
        };
        tree.boxes[table_node.index()] = Some(LayoutBox {
            kind: BoxKind::Block,
            rect,
            padding,
            border,
            margin,
        });
        return margin.top + margin.bottom;
    }

    let col_width = content_width / max_cols as f32;
    let num_rows = rows.len();

    // Lay out each cell, accumulate row heights.
    let mut row_heights = vec![0.0f32; num_rows];
    let mut row_y = content_y;
    for row_idx in 0..num_rows {
        let mut row_max_h = 0.0f32;
        for cell in placed.iter().filter(|p| p.row == row_idx) {
            let cell_x = content_x + cell.col as f32 * col_width;
            let cell_w = col_width * cell.col_span as f32;
            let cb = Rect {
                x: cell_x,
                y: row_y,
                width: cell_w,
                height: 0.0,
            };
            let h = block::layout(dom, styles, text, images, cell.node, cb, tree);
            if h > row_max_h {
                row_max_h = h;
            }
        }
        row_heights[row_idx] = row_max_h;
        row_y += row_max_h;
    }

    // Stretch rowspan-N cells to their full vertical extent.
    for cell in &placed {
        if cell.row_span > 1 {
            let end = (cell.row + cell.row_span).min(num_rows);
            let total: f32 = row_heights[cell.row..end].iter().sum();
            if let Some(b) = tree.boxes[cell.node.index()].as_mut() {
                b.rect.height = total;
            }
        }
    }

    let total_content_height: f32 = row_heights.iter().sum();
    let border_box_height =
        total_content_height + border.top + border.bottom + padding.top + padding.bottom;
    let rect = Rect {
        x: border_box_x,
        y: border_box_y,
        width: content_width + border.left + border.right + padding.left + padding.right,
        height: border_box_height,
    };
    tree.boxes[table_node.index()] = Some(LayoutBox {
        kind: BoxKind::Block,
        rect,
        padding,
        border,
        margin,
    });

    margin.top + border_box_height + margin.bottom
}

fn find_rows(dom: &Dom, table_node: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    walk_rows(dom, table_node, &mut out);
    out
}

fn walk_rows(dom: &Dom, node: NodeId, out: &mut Vec<NodeId>) {
    let kids: Vec<NodeId> = dom.children(node).collect();
    for child in kids {
        if let NodeKind::Element { tag, .. } = &dom.node(child).kind {
            match tag.as_str() {
                "tr" => out.push(child),
                "thead" | "tbody" | "tfoot" => walk_rows(dom, child, out),
                "table" => {} // do not descend into nested tables
                _ => {}       // skip caption, col, colgroup, etc.
            }
        }
    }
}

fn find_cells(dom: &Dom, tr_node: NodeId) -> Vec<NodeId> {
    dom.children(tr_node)
        .filter(|c| {
            matches!(
                &dom.node(*c).kind,
                NodeKind::Element { tag, .. } if tag == "td" || tag == "th"
            )
        })
        .collect()
}

fn build_grid(dom: &Dom, rows: &[NodeId]) -> (Vec<PlacedCell>, usize) {
    let mut placed = Vec::new();
    let mut cell_remaining: Vec<usize> = Vec::new(); // per column: rows remaining (incl. current)
    let mut max_cols = 0usize;

    for (row_idx, &tr) in rows.iter().enumerate() {
        if row_idx > 0 {
            // Each new row consumes one "remaining" from every column.
            for s in &mut cell_remaining {
                if *s > 0 {
                    *s -= 1;
                }
            }
        }

        let mut col = skip_occupied(&cell_remaining, 0);
        for cell_node in find_cells(dom, tr) {
            let col_span = attr_uint(dom, cell_node, "colspan", 1);
            let row_span = attr_uint(dom, cell_node, "rowspan", 1);

            placed.push(PlacedCell {
                node: cell_node,
                row: row_idx,
                col,
                col_span,
                row_span,
            });

            for c in col..col + col_span {
                while c >= cell_remaining.len() {
                    cell_remaining.push(0);
                }
                cell_remaining[c] = row_span;
            }
            col += col_span;
            col = skip_occupied(&cell_remaining, col);
        }

        if col > max_cols {
            max_cols = col;
        }
    }

    (placed, max_cols)
}

fn skip_occupied(cell_remaining: &[usize], from: usize) -> usize {
    let mut c = from;
    while c < cell_remaining.len() && cell_remaining[c] > 0 {
        c += 1;
    }
    c
}

fn attr_uint(dom: &Dom, node: NodeId, name: &str, default: usize) -> usize {
    if let NodeKind::Element { attrs, .. } = &dom.node(node).kind {
        if let Some((_, v)) = attrs.iter().find(|(k, _)| k == name) {
            return v.parse::<usize>().unwrap_or(default).max(1);
        }
    }
    default
}

#[cfg(test)]
mod tests {
    use super::super::{layout, Rect};
    use super::*;
    use crate::css;
    use crate::dom::Dom;
    use crate::html;
    use crate::layout::replaced::ImageCache;

    fn run(html_src: &str, viewport_w: f32) -> (Dom, super::BoxTree) {
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
        let tree = layout(&dom, &styles, &images, viewport);
        (dom, tree)
    }

    fn find_all(dom: &Dom, root: NodeId, tag: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        walk(dom, root, tag, &mut out);
        out
    }

    fn walk(dom: &Dom, id: NodeId, tag: &str, out: &mut Vec<NodeId>) {
        if let NodeKind::Element { tag: t, .. } = &dom.node(id).kind {
            if t == tag {
                out.push(id);
            }
        }
        for c in dom.children(id).collect::<Vec<_>>() {
            walk(dom, c, tag, out);
        }
    }

    #[test]
    fn simple_2x2_grid() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } table, td { margin: 0; padding: 0; }</style>\
             <table>\
               <tr><td>A</td><td>B</td></tr>\
               <tr><td>C</td><td>D</td></tr>\
             </table>",
            1000.0,
        );
        let cells = find_all(&dom, dom.document(), "td");
        assert_eq!(cells.len(), 4);
        let a = tree.get(cells[0]).unwrap();
        let b = tree.get(cells[1]).unwrap();
        let c = tree.get(cells[2]).unwrap();
        let d = tree.get(cells[3]).unwrap();
        // Equal column widths: 1000 / 2 = 500.
        assert!((a.rect.width - 500.0).abs() < 0.5);
        assert!((b.rect.width - 500.0).abs() < 0.5);
        // A and B share the same y; C and D share a (greater) y.
        assert!((a.rect.y - b.rect.y).abs() < 0.5);
        assert!((c.rect.y - d.rect.y).abs() < 0.5);
        assert!(c.rect.y > a.rect.y);
        // Columns: A and C at x=0; B and D at x=500.
        assert!(a.rect.x < 1.0);
        assert!((b.rect.x - 500.0).abs() < 0.5);
        assert!(c.rect.x < 1.0);
        assert!((d.rect.x - 500.0).abs() < 0.5);
    }

    #[test]
    fn colspan_extends_first_cell() {
        let (dom, tree) = run(
            "<style>body { margin: 0; }</style>\
             <table>\
               <tr><td colspan=2>X</td></tr>\
               <tr><td>A</td><td>B</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        assert_eq!(tds.len(), 3); // X, A, B
        let x = tree.get(tds[0]).unwrap();
        let a = tree.get(tds[1]).unwrap();
        let b = tree.get(tds[2]).unwrap();
        // X spans full table width.
        assert!((x.rect.width - 1000.0).abs() < 0.5);
        // A and B each half.
        assert!((a.rect.width - 500.0).abs() < 0.5);
        assert!((b.rect.width - 500.0).abs() < 0.5);
    }

    #[test]
    fn rowspan_extends_first_cell() {
        let (dom, tree) = run(
            "<style>body { margin: 0; }</style>\
             <table>\
               <tr><td rowspan=2>X</td><td>A</td></tr>\
               <tr><td>B</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        assert_eq!(tds.len(), 3); // X, A, B
        let x = tree.get(tds[0]).unwrap();
        let a = tree.get(tds[1]).unwrap();
        let b = tree.get(tds[2]).unwrap();
        // X and A both in row 0, same y.
        assert!((x.rect.y - a.rect.y).abs() < 0.5);
        // B is in row 1, below A.
        assert!(b.rect.y > a.rect.y);
        // X's height should at least cover the height of A (row 0) plus B (row 1).
        assert!(x.rect.height >= a.rect.height + b.rect.height - 0.5);
        // X sits in column 0. Row 1's column 0 is occupied by X's rowspan, so
        // B falls through to column 1 — under A.
        assert!(x.rect.x < 1.0);
        assert!((a.rect.x - 500.0).abs() < 0.5);
        assert!((b.rect.x - 500.0).abs() < 0.5);
    }

    #[test]
    fn implicit_tbody_works() {
        // No explicit <tbody>; rows are direct children of <table>.
        let (dom, tree) = run(
            "<table><tr><td>A</td></tr></table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        assert_eq!(tds.len(), 1);
        assert!(tree.get(tds[0]).is_some());
    }

    #[test]
    fn explicit_tbody_works() {
        // The parser may already auto-insert tbody for some markup, but explicit
        // tbody must also be traversed.
        let (dom, tree) = run(
            "<table><tbody><tr><td>A</td></tr></tbody></table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        assert_eq!(tds.len(), 1);
        assert!(tree.get(tds[0]).is_some());
    }
}
