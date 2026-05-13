//! Table layout.
//!
//! High-level flow per `<table>`:
//!  1. Walk the table, classifying children: `<caption>`, `<col>` /
//!     `<colgroup>` (column hints), and rows (`<tr>` direct or via
//!     `<thead>` / `<tbody>` / `<tfoot>`).
//!  2. Build a grid of placed cells from the rows, accounting for
//!     `rowspan` and `colspan`.
//!  3. Compute column widths:
//!       - `table-layout: auto` (default) measures each cell's intrinsic
//!         no-wrap width via cosmic-text, taking the max per column. For
//!         colspan cells, the spanning content width is distributed among
//!         the spanned columns.
//!       - `table-layout: fixed` uses `<col width=...>` hints (or equal
//!         widths as fallback) and skips intrinsic measurement.
//!     Then the column widths are scaled to fill (or fit) the available
//!     content width.
//!  4. Lay out `<caption>` above the grid as a normal block.
//!  5. Lay out cells row-by-row at their column positions, separated by
//!     `border-spacing`. Record content heights; tentative row heights come
//!     from non-spanning cells only.
//!  6. For each `rowspan` cell whose content exceeds its tentative spanned
//!     height, grow the last spanned row to absorb the extra height.
//!  7. Shift every cell + descendant subtree to its final row position
//!     based on the now-finalised row heights.
//!  8. Set the final cell heights (rowspan cells span their full vertical
//!     extent), then set the table's own box.

use super::block;
use super::replaced::ImageCache;
use super::text::TextLayout;
use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{ComputedStyle, StyleTree, TableLayout};
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
    let (bs_h, bs_v) = style.border_spacing;

    // Discover structure.
    let caption = find_caption(dom, table_node);
    let col_hints = collect_col_widths(dom, table_node);
    let rows = find_rows(dom, table_node);
    let (placed, max_cols) = build_grid(dom, &rows);

    // Box geometry.
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

    // Empty / degenerate table.
    if rows.is_empty() || max_cols == 0 {
        // Caption (if any) still renders.
        let mut grid_y = content_y;
        if let Some(cap) = caption {
            let cb = Rect {
                x: content_x,
                y: grid_y,
                width: content_width,
                height: 0.0,
            };
            grid_y += block::layout(dom, styles, text, images, cap, cb, tree);
        }
        let total_h = grid_y - content_y;
        let rect = Rect {
            x: border_box_x,
            y: border_box_y,
            width: content_width + border.left + border.right + padding.left + padding.right,
            height: total_h + border.top + border.bottom + padding.top + padding.bottom,
        };
        tree.boxes[table_node.index()] = Some(LayoutBox {
            kind: BoxKind::Block,
            rect,
            padding,
            border,
            margin,
        });
        return margin.top + rect.height + margin.bottom;
    }

    // Total horizontal border-spacing eats into the available content width:
    // there's spacing before column 0, between every pair, and after the last.
    let spacing_total_h = bs_h * (max_cols as f32 + 1.0);
    let usable_width = (content_width - spacing_total_h).max(0.0);
    let col_widths = compute_column_widths(
        dom,
        styles,
        text,
        &placed,
        max_cols,
        usable_width,
        style.table_layout,
        &col_hints,
    );

    // Column x positions: start at content_x + bs_h, advance by width + bs_h.
    let mut col_xs = vec![content_x + bs_h; max_cols];
    for c in 1..max_cols {
        col_xs[c] = col_xs[c - 1] + col_widths[c - 1] + bs_h;
    }

    // Caption first (top placement; CSS caption-side: bottom not supported).
    let mut grid_y = content_y + bs_v;
    if let Some(cap) = caption {
        let cb = Rect {
            x: content_x,
            y: content_y,
            width: content_width,
            height: 0.0,
        };
        let cap_h = block::layout(dom, styles, text, images, cap, cb, tree);
        grid_y = content_y + cap_h + bs_v;
    }

    // Pass 1: lay out cells row by row at TENTATIVE row positions.
    // Row heights are computed from non-spanning cells only.
    let num_rows = rows.len();
    let mut tentative_row_heights = vec![0.0f32; num_rows];
    let mut cell_content_heights = vec![0.0f32; placed.len()];
    let mut row_y = grid_y;
    for row_idx in 0..num_rows {
        let mut row_max: f32 = 0.0;
        for (i, cell) in placed.iter().enumerate().filter(|(_, c)| c.row == row_idx) {
            let cell_x = col_xs[cell.col];
            let cell_w: f32 = col_widths[cell.col..cell.col + cell.col_span].iter().sum::<f32>()
                + bs_h * (cell.col_span as f32 - 1.0);
            let cb = Rect {
                x: cell_x,
                y: row_y,
                width: cell_w,
                height: 0.0,
            };
            let h = block::layout(dom, styles, text, images, cell.node, cb, tree);
            cell_content_heights[i] = h;
            if cell.row_span == 1 && h > row_max {
                row_max = h;
            }
        }
        tentative_row_heights[row_idx] = row_max;
        row_y += row_max + bs_v;
    }

    // Pass 2: grow last spanned row to absorb rowspan content overflow.
    let mut final_row_heights = tentative_row_heights.clone();
    for (i, cell) in placed.iter().enumerate() {
        if cell.row_span > 1 {
            let end = (cell.row + cell.row_span).min(num_rows);
            let cur: f32 = final_row_heights[cell.row..end].iter().sum::<f32>()
                + bs_v * (cell.row_span as f32 - 1.0);
            if cell_content_heights[i] > cur {
                let extra = cell_content_heights[i] - cur;
                final_row_heights[end - 1] += extra;
            }
        }
    }

    // Pass 3: shift each cell + descendants to its final y based on cumulative
    // row height delta. Rows before any growth stay put; later rows shift down.
    let mut cumulative_delta = 0.0f32;
    let mut shift_per_row = vec![0.0f32; num_rows];
    for r in 0..num_rows {
        shift_per_row[r] = cumulative_delta;
        cumulative_delta += final_row_heights[r] - tentative_row_heights[r];
    }
    for cell in &placed {
        let shift = shift_per_row[cell.row];
        if shift.abs() > 0.001 {
            shift_subtree(dom, cell.node, shift, tree);
        }
    }

    // Pass 4: set every cell's final height (rowspan cells span their range).
    for cell in &placed {
        let end = (cell.row + cell.row_span).min(num_rows);
        let height: f32 = final_row_heights[cell.row..end].iter().sum::<f32>()
            + bs_v * (cell.row_span as f32 - 1.0);
        if let Some(b) = tree.boxes[cell.node.index()].as_mut() {
            b.rect.height = height;
        }
    }

    // Table content height: caption + spacing + rows + spacing.
    let caption_h = caption.map_or(0.0, |c| {
        tree.boxes[c.index()].as_ref().map_or(0.0, |b| b.rect.height)
    });
    let total_rows_height: f32 = final_row_heights.iter().sum();
    let total_content_height =
        caption_h + bs_v + total_rows_height + bs_v * num_rows as f32;

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

// ---------------- Structure discovery ----------------

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
                "table" => {}
                _ => {}
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

fn find_caption(dom: &Dom, table_node: NodeId) -> Option<NodeId> {
    dom.children(table_node).find(|c| {
        matches!(&dom.node(*c).kind, NodeKind::Element { tag, .. } if tag == "caption")
    })
}

/// Walk `<col>` and `<colgroup>` children of `<table>` and turn each `<col>`
/// (or each col in a colgroup) into a `width=...` hint. `None` means "no hint".
fn collect_col_widths(dom: &Dom, table_node: NodeId) -> Vec<Option<f32>> {
    let mut out: Vec<Option<f32>> = Vec::new();
    for child in dom.children(table_node).collect::<Vec<_>>() {
        if let NodeKind::Element { tag, attrs } = &dom.node(child).kind {
            match tag.as_str() {
                "col" => {
                    push_col_hint(attrs, attr_uint(dom, child, "span", 1), &mut out);
                }
                "colgroup" => {
                    let group_span = attr_uint(dom, child, "span", 0);
                    let group_width = parse_attr_length(attrs, "width");
                    let group_kids: Vec<NodeId> = dom.children(child).collect();
                    let has_col_children = group_kids.iter().any(|c| {
                        matches!(&dom.node(*c).kind, NodeKind::Element { tag, .. } if tag == "col")
                    });
                    if has_col_children {
                        for g in group_kids {
                            if let NodeKind::Element { tag: gt, attrs: ga } = &dom.node(g).kind {
                                if gt == "col" {
                                    push_col_hint(ga, attr_uint(dom, g, "span", 1), &mut out);
                                }
                            }
                        }
                    } else if group_span > 0 {
                        for _ in 0..group_span {
                            out.push(group_width);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn push_col_hint(attrs: &[(String, String)], span: usize, out: &mut Vec<Option<f32>>) {
    let w = parse_attr_length(attrs, "width");
    for _ in 0..span {
        out.push(w);
    }
}

fn parse_attr_length(attrs: &[(String, String)], name: &str) -> Option<f32> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| {
            // Accept "100", "100px"; ignore percentages for now.
            let trimmed = v.trim_end_matches("px");
            trimmed.parse::<f32>().ok()
        })
}

fn build_grid(dom: &Dom, rows: &[NodeId]) -> (Vec<PlacedCell>, usize) {
    let mut placed = Vec::new();
    let mut cell_remaining: Vec<usize> = Vec::new();
    let mut max_cols = 0usize;

    for (row_idx, &tr) in rows.iter().enumerate() {
        if row_idx > 0 {
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

// ---------------- Column-width algorithm ----------------

fn compute_column_widths(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    placed: &[PlacedCell],
    max_cols: usize,
    usable_width: f32,
    table_layout: TableLayout,
    col_hints: &[Option<f32>],
) -> Vec<f32> {
    if max_cols == 0 {
        return Vec::new();
    }

    // Start every column at zero, then layer hints / measurements on top.
    let mut col_max = vec![0.0f32; max_cols];

    // <col width=...> hints first.
    for (i, hint) in col_hints.iter().enumerate() {
        if i >= max_cols {
            break;
        }
        if let Some(w) = hint {
            col_max[i] = *w;
        }
    }

    if table_layout == TableLayout::Auto {
        for cell in placed {
            let cell_w = measure_cell_natural_width(dom, styles, text, cell.node);
            if cell.col_span == 1 {
                if cell_w > col_max[cell.col] {
                    col_max[cell.col] = cell_w;
                }
            } else {
                // For a spanning cell: ensure the spanned columns sum to at
                // least cell_w. Distribute the deficit evenly across cols
                // that don't already exceed it.
                let span_sum: f32 = col_max[cell.col..cell.col + cell.col_span].iter().sum();
                if cell_w > span_sum {
                    let extra = cell_w - span_sum;
                    let per_col = extra / cell.col_span as f32;
                    for c in cell.col..cell.col + cell.col_span {
                        col_max[c] += per_col;
                    }
                }
            }
        }
    } else {
        // `table-layout: fixed`. Any column without a hint defaults to equal
        // share of the remaining width.
        let hinted_total: f32 = col_max.iter().sum();
        let unhinted_cols = col_max.iter().filter(|w| **w == 0.0).count();
        if unhinted_cols > 0 {
            let remaining = (usable_width - hinted_total).max(0.0);
            let per = remaining / unhinted_cols as f32;
            for w in &mut col_max {
                if *w == 0.0 {
                    *w = per;
                }
            }
        }
    }

    // Scale to exactly fill the usable width (this is the toy simplification:
    // real CSS distinguishes min vs max content widths and constrains both).
    let total: f32 = col_max.iter().sum();
    if total > 0.0 && (total - usable_width).abs() > 0.001 {
        let scale = usable_width / total;
        for w in &mut col_max {
            *w *= scale;
        }
    }
    if total == 0.0 {
        let per = usable_width / max_cols as f32;
        return vec![per; max_cols];
    }
    col_max
}

/// Concatenate every text node inside this cell (descending through children)
/// and measure its no-wrap width via cosmic-text. Approximates max-content.
fn measure_cell_natural_width(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    cell: NodeId,
) -> f32 {
    let mut acc = String::new();
    collect_cell_text(dom, cell, &mut acc);
    if acc.trim().is_empty() {
        return 0.0;
    }
    // The cell's own style sets the font; descendants inherit it. For toy
    // purposes one style is good enough for natural-width measurement.
    let style = styles.get(cell);
    text.measure_natural_width(&acc, style)
}

fn collect_cell_text(dom: &Dom, node: NodeId, out: &mut String) {
    match &dom.node(node).kind {
        NodeKind::Text(s) => {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(s.trim());
        }
        NodeKind::Element { .. } => {
            for c in dom.children(node).collect::<Vec<_>>() {
                collect_cell_text(dom, c, out);
            }
        }
        _ => {}
    }
}

// ---------------- Subtree shifting ----------------

fn shift_subtree(dom: &Dom, node: NodeId, shift: f32, tree: &mut BoxTree) {
    if let Some(b) = tree.boxes.get_mut(node.index()).and_then(|s| s.as_mut()) {
        b.rect.y += shift;
    }
    for c in dom.children(node).collect::<Vec<_>>() {
        shift_subtree(dom, c, shift, tree);
    }
}

// ---------------- Tests ----------------

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
    fn intrinsic_widths_uneven_columns() {
        // Column 0 has wide content, column 1 has a single letter. The wide
        // column should end up considerably wider than the narrow one.
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 0; } \
             td { margin: 0; padding: 0; }</style>\
             <table>\
               <tr><td>This is a much longer cell of text</td><td>X</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        let wide = tree.get(tds[0]).unwrap();
        let narrow = tree.get(tds[1]).unwrap();
        assert!(
            wide.rect.width > narrow.rect.width * 2.0,
            "expected wide > narrow*2, got {} vs {}",
            wide.rect.width, narrow.rect.width
        );
    }

    #[test]
    fn caption_renders_above_grid() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 0; } \
             td, caption { margin: 0; padding: 0; }</style>\
             <table>\
               <caption>My table</caption>\
               <tr><td>A</td></tr>\
             </table>",
            1000.0,
        );
        let cap = find_all(&dom, dom.document(), "caption")[0];
        let td = find_all(&dom, dom.document(), "td")[0];
        let cap_box = tree.get(cap).unwrap();
        let td_box = tree.get(td).unwrap();
        // Caption sits above the cell (smaller y).
        assert!(cap_box.rect.y < td_box.rect.y);
    }

    #[test]
    fn border_spacing_adds_gaps_between_cells() {
        // border-spacing: 10px → 10px gap before col 0, between cols, after.
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 10px; } \
             td { margin: 0; padding: 0; }</style>\
             <table>\
               <tr><td>A</td><td>B</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        let a = tree.get(tds[0]).unwrap();
        let b = tree.get(tds[1]).unwrap();
        // Cell A's left edge should be at content_x + 10px (table left padding
        // is zero per our test stylesheet; body margin is also zero).
        assert!(a.rect.x >= 9.0 && a.rect.x <= 11.0, "A.x = {}", a.rect.x);
        // The gap between A and B should be ~10px.
        let gap = b.rect.x - (a.rect.x + a.rect.width);
        assert!(gap >= 9.0 && gap <= 11.0, "gap = {gap}");
    }

    #[test]
    fn rowspan_growth_distributes_to_last_row() {
        // X has rowspan=2 and tall content. Rows 0 and 1 have short cells.
        // Without distribution, X would overflow; with distribution, row 1
        // grows so X's total height covers its content.
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 0; } \
             td { margin: 0; padding: 0; } \
             .tall { height: 100px; }</style>\
             <table>\
               <tr><td rowspan=2 class=tall>X</td><td>A</td></tr>\
               <tr><td>B</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        let x = tree.get(tds[0]).unwrap();
        let a = tree.get(tds[1]).unwrap();
        let b = tree.get(tds[2]).unwrap();
        // X's height should be at least its explicit 100px.
        assert!(x.rect.height >= 99.0, "x.height = {}", x.rect.height);
        // B should sit below A by row 0's height; the gap should equal
        // row 0 height (A's box-height).
        assert!(b.rect.y > a.rect.y);
        // X spans rows 0 + 1: its bottom should be at or below B's bottom.
        let x_bottom = x.rect.y + x.rect.height;
        let b_bottom = b.rect.y + b.rect.height;
        assert!(
            (x_bottom - b_bottom).abs() < 1.0,
            "x bottom {x_bottom} vs b bottom {b_bottom}"
        );
    }

    #[test]
    fn col_width_hint_is_honored() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 0; \
             table-layout: fixed; } td { margin: 0; padding: 0; }</style>\
             <table>\
               <col width=\"100\"><col width=\"300\">\
               <tr><td>A</td><td>B</td></tr>\
             </table>",
            1000.0,
        );
        let tds = find_all(&dom, dom.document(), "td");
        let a = tree.get(tds[0]).unwrap();
        let b = tree.get(tds[1]).unwrap();
        // With table-layout: fixed and an unbounded body width 1000px, columns
        // are scaled from (100, 300) to fit 1000 → (250, 750).
        let ratio = b.rect.width / a.rect.width;
        assert!(
            (ratio - 3.0).abs() < 0.1,
            "expected 1:3 ratio, got {} : {} = {}",
            a.rect.width,
            b.rect.width,
            ratio
        );
    }

    #[test]
    fn simple_2x2_grid_still_works() {
        // Regression: the previous equal-width tests should still pass with
        // empty cells (no text → intrinsic width 0 → falls back to equal).
        let (dom, tree) = run(
            "<style>body { margin: 0; } table { border-spacing: 0; } \
             td { margin: 0; padding: 0; }</style>\
             <table>\
               <tr><td></td><td></td></tr>\
               <tr><td></td><td></td></tr>\
             </table>",
            1000.0,
        );
        let cells = find_all(&dom, dom.document(), "td");
        assert_eq!(cells.len(), 4);
        let a = tree.get(cells[0]).unwrap();
        let b = tree.get(cells[1]).unwrap();
        assert!((a.rect.width - 500.0).abs() < 1.0);
        assert!((b.rect.width - 500.0).abs() < 1.0);
        assert!((a.rect.y - b.rect.y).abs() < 0.5);
    }
}
