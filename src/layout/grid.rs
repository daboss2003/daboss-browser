//! CSS Grid — toy implementation.
//!
//! Supports:
//!  * `grid-template-columns` / `grid-template-rows` with `Npx`, `N%`, `Nfr`,
//!    `auto`, and `repeat(n, ...)` (expanded at parse time).
//!  * `grid-template-areas` (named regions across rows).
//!  * Explicit placement via `grid-area`, `grid-column`, `grid-row`
//!    (numeric, named, or `span N`).
//!  * Auto-placement: items without explicit placement fill the first
//!    free cell in row-major order. `grid-auto-flow: dense` packs items
//!    back into earlier holes.
//!  * `row-gap` / `column-gap` between tracks.
//!
//! Not implemented: implicit grid track auto-creation (items placed
//! outside template fall into a single overflow row), `align-self` /
//! `justify-self` per item, `subgrid`, `minmax()`.

use super::block;
use super::replaced::ImageCache;
use super::text::TextLayout;
use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{ComputedStyle, Dimension, GridAutoFlow, GridLine, GridTrack, StyleTree};
use crate::dom::{Dom, NodeId, NodeKind};

#[allow(clippy::too_many_arguments)]
pub fn layout_grid(
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
    let (row_gap, col_gap) = style.gap;

    let cb_width = containing.width;
    let content_width = match style.width {
        Dimension::Length(w) => w,
        Dimension::Percent(p) => (cb_width * p / 100.0
            - border.left
            - border.right
            - padding.left
            - padding.right)
            .max(0.0),
        Dimension::Auto => (cb_width
            - margin.left
            - margin.right
            - border.left
            - border.right
            - padding.left
            - padding.right)
            .max(0.0),
    };
    let border_box_x = containing.x + margin.left;
    let border_box_y = containing.y + margin.top;
    let content_x = border_box_x + border.left + padding.left;
    let content_y = border_box_y + border.top + padding.top;

    // Pull named areas if any; they define rows × columns names.
    let area_map = build_area_map(&style.grid_template_areas);

    // Resolve column widths.
    let column_widths = resolve_tracks(&style.grid_template_columns, content_width, col_gap);
    let mut num_cols = column_widths.len().max(1);
    if !style.grid_template_areas.is_empty() {
        // Areas grid dictates the column count when no template-columns set.
        let cols_from_areas = style
            .grid_template_areas
            .iter()
            .map(|r| r.len())
            .max()
            .unwrap_or(1);
        if column_widths.len() < cols_from_areas {
            num_cols = cols_from_areas;
        }
    }
    let column_widths = if column_widths.len() < num_cols {
        // Pad with auto for any extra columns implied by areas.
        let mut w = column_widths;
        while w.len() < num_cols {
            w.push(content_width / num_cols as f32);
        }
        w
    } else {
        column_widths
    };

    // Collect grid items.
    let items: Vec<NodeId> = dom
        .children(node)
        .filter(|c| match &dom.node(*c).kind {
            NodeKind::Element { .. } => {
                styles.get(*c).display != crate::css::Display::None
            }
            _ => false,
        })
        .collect();

    // Resolve each item's grid placement to (row_start, col_start, row_span,
    // col_span). Auto-placement is run after explicit placements are claimed.
    let placements = resolve_placements(
        styles,
        &items,
        num_cols,
        &area_map,
        style.grid_auto_flow,
    );

    // Compute column x positions.
    let mut col_xs = vec![content_x; num_cols];
    for c in 1..num_cols {
        col_xs[c] = col_xs[c - 1] + column_widths[c - 1] + col_gap;
    }

    // Group placements by row, then lay each row out. Row heights = max
    // cell content height for items starting in that row.
    let max_row = placements
        .iter()
        .map(|p| p.row_start + p.row_span)
        .max()
        .unwrap_or(0);
    if max_row == 0 {
        // empty grid
        let rect = Rect {
            x: border_box_x,
            y: border_box_y,
            width: content_width + border.left + border.right + padding.left + padding.right,
            height: border.top + border.bottom + padding.top + padding.bottom,
        };
        tree.boxes[node.index()] = Some(LayoutBox {
            kind: BoxKind::Block,
            rect,
            padding,
            border,
            margin,
        });
        return margin.top + rect.height + margin.bottom;
    }
    let mut row_heights = vec![0.0_f32; max_row];

    // Pass 1: tentative layout — for each item, lay out content with its
    // resolved column span. Record content height; assign to the starting row.
    let mut item_heights = vec![0.0_f32; placements.len()];
    let mut row_ys = vec![content_y; max_row];
    // First lay out items, recording content heights and finalising row heights.
    for (idx, p) in placements.iter().enumerate() {
        let col_start = p.col_start.min(num_cols.saturating_sub(1));
        let col_span = p.col_span.max(1).min(num_cols - col_start);
        let cell_x = col_xs[col_start];
        let cell_w = (col_start..col_start + col_span)
            .map(|c| column_widths[c])
            .sum::<f32>()
            + col_gap * (col_span.saturating_sub(1) as f32);
        let cb = Rect {
            x: cell_x,
            y: 0.0, // tentative; fixed up after row heights are known
            width: cell_w,
            height: 0.0,
        };
        let h = block::layout(dom, styles, text, images, items[idx], cb, tree);
        item_heights[idx] = h;
        // For single-row items, contribute directly to row height.
        if p.row_span == 1 {
            if p.row_start < row_heights.len() && h > row_heights[p.row_start] {
                row_heights[p.row_start] = h;
            }
        }
    }
    // Grow last spanned row to absorb multi-row item overflow.
    for (idx, p) in placements.iter().enumerate() {
        if p.row_span <= 1 {
            continue;
        }
        let end = (p.row_start + p.row_span).min(row_heights.len());
        let current: f32 = row_heights[p.row_start..end].iter().sum::<f32>()
            + row_gap * (p.row_span as f32 - 1.0);
        if item_heights[idx] > current {
            row_heights[end - 1] += item_heights[idx] - current;
        }
    }

    // Compute final row y positions.
    let mut acc = content_y;
    for (i, h) in row_heights.iter().enumerate() {
        row_ys[i] = acc;
        acc += h + row_gap;
    }

    // Pass 2: shift each item's subtree to its final y based on row_start.
    for (idx, p) in placements.iter().enumerate() {
        let target_y = row_ys[p.row_start];
        let current_box = tree.boxes[items[idx].index()].as_ref();
        if let Some(b) = current_box {
            let dy = target_y - b.rect.y;
            if dy.abs() > 0.001 {
                shift_subtree(dom, items[idx], dy, tree);
            }
            // For multi-row spans, stretch the item's box height to cover them.
            if p.row_span > 1 {
                let end = (p.row_start + p.row_span).min(row_heights.len());
                let total: f32 = row_heights[p.row_start..end].iter().sum::<f32>()
                    + row_gap * (p.row_span as f32 - 1.0);
                if let Some(b) = tree.boxes[items[idx].index()].as_mut() {
                    b.rect.height = total;
                }
            }
        }
    }

    let total_rows_height: f32 = if row_heights.is_empty() {
        0.0
    } else {
        row_heights.iter().sum::<f32>() + row_gap * (row_heights.len() - 1) as f32
    };

    let final_height = match style.height {
        Dimension::Length(h) => h,
        _ => total_rows_height,
    };
    let border_box_height = final_height
        + border.top
        + border.bottom
        + padding.top
        + padding.bottom;
    let border_box_width =
        content_width + border.left + border.right + padding.left + padding.right;

    let rect = Rect {
        x: border_box_x,
        y: border_box_y,
        width: border_box_width,
        height: border_box_height,
    };
    let kind = match style.display {
        crate::css::Display::InlineGrid => BoxKind::InlineBlock,
        _ => BoxKind::Block,
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

#[derive(Debug)]
struct Placement {
    row_start: usize,
    col_start: usize,
    row_span: usize,
    col_span: usize,
}

/// Resolve every item's `grid-area` / `grid-column` / `grid-row` /
/// `grid-template-areas` into a concrete `Placement`. Items without
/// explicit placement get auto-placed into the first free cell (row-major;
/// dense fills earlier holes).
fn resolve_placements(
    styles: &StyleTree,
    items: &[NodeId],
    num_cols: usize,
    area_map: &std::collections::HashMap<String, (usize, usize, usize, usize)>,
    auto_flow: GridAutoFlow,
) -> Vec<Placement> {
    let dense = matches!(
        auto_flow,
        GridAutoFlow::RowDense | GridAutoFlow::ColumnDense
    );

    // Occupied cells: track which (row, col) slots are taken.
    use std::collections::HashSet;
    let mut occupied: HashSet<(usize, usize)> = HashSet::new();
    let mut placements: Vec<Placement> = Vec::with_capacity(items.len());

    // First pass: place items with explicit placement (area name or
    // numeric/named line).
    let mut explicit_indices: Vec<usize> = Vec::new();
    for (i, &node) in items.iter().enumerate() {
        let p = styles.get(node).grid_placement.clone();
        if explicit(&p, area_map) {
            let placement = resolve_explicit(&p, area_map, num_cols);
            for r in placement.row_start..placement.row_start + placement.row_span {
                for c in placement.col_start..placement.col_start + placement.col_span {
                    occupied.insert((r, c));
                }
            }
            placements.push(placement);
            explicit_indices.push(i);
        } else {
            placements.push(Placement {
                row_start: 0,
                col_start: 0,
                row_span: 1,
                col_span: 1,
            });
        }
    }

    // Auto-place the rest.
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    for (i, &node) in items.iter().enumerate() {
        if explicit_indices.contains(&i) {
            continue;
        }
        let style = styles.get(node);
        let p = &style.grid_placement;
        // Span widths from grid-column: span N etc.
        let col_span = match &p.column_end {
            Some(GridLine::Span(n)) => (*n as usize).max(1),
            _ => 1,
        }
        .min(num_cols.max(1));
        let row_span = match &p.row_end {
            Some(GridLine::Span(n)) => (*n as usize).max(1),
            _ => 1,
        };

        // Find the first free cell that fits this span.
        let (r, c) = if dense {
            find_free_cell(&occupied, 0, 0, col_span, row_span, num_cols)
        } else {
            find_free_cell(&occupied, cursor_row, cursor_col, col_span, row_span, num_cols)
        };
        for rr in r..r + row_span {
            for cc in c..c + col_span {
                occupied.insert((rr, cc));
            }
        }
        if !dense {
            cursor_row = r;
            cursor_col = c + col_span;
            if cursor_col >= num_cols {
                cursor_col = 0;
                cursor_row += 1;
            }
        }
        placements[i] = Placement {
            row_start: r,
            col_start: c,
            row_span,
            col_span,
        };
    }
    placements
}

fn explicit(
    p: &crate::css::GridPlacement,
    area_map: &std::collections::HashMap<String, (usize, usize, usize, usize)>,
) -> bool {
    if let Some(name) = &p.area {
        return area_map.contains_key(name);
    }
    let line_is_set = |l: &Option<GridLine>| match l {
        None | Some(GridLine::Auto) | Some(GridLine::Span(_)) => false,
        _ => true,
    };
    line_is_set(&p.column_start) || line_is_set(&p.row_start)
}

fn resolve_explicit(
    p: &crate::css::GridPlacement,
    area_map: &std::collections::HashMap<String, (usize, usize, usize, usize)>,
    num_cols: usize,
) -> Placement {
    if let Some(name) = &p.area {
        if let Some((r0, c0, r1, c1)) = area_map.get(name).copied() {
            return Placement {
                row_start: r0,
                col_start: c0,
                row_span: (r1 - r0 + 1).max(1),
                col_span: (c1 - c0 + 1).max(1),
            };
        }
    }
    let row = match &p.row_start {
        Some(GridLine::Index(n)) => (*n as i32 - 1).max(0) as usize,
        Some(GridLine::Name(n)) => area_map.get(n).map(|t| t.0).unwrap_or(0),
        _ => 0,
    };
    let col = match &p.column_start {
        Some(GridLine::Index(n)) => (*n as i32 - 1).max(0) as usize,
        Some(GridLine::Name(n)) => area_map.get(n).map(|t| t.1).unwrap_or(0),
        _ => 0,
    };
    let row_span = match (&p.row_start, &p.row_end) {
        (_, Some(GridLine::Span(n))) => (*n as usize).max(1),
        (Some(GridLine::Index(a)), Some(GridLine::Index(b))) => {
            ((*b as i32 - *a as i32).max(1)) as usize
        }
        _ => 1,
    };
    let col_span = match (&p.column_start, &p.column_end) {
        (_, Some(GridLine::Span(n))) => (*n as usize).max(1),
        (Some(GridLine::Index(a)), Some(GridLine::Index(b))) => {
            ((*b as i32 - *a as i32).max(1)) as usize
        }
        _ => 1,
    };
    Placement {
        row_start: row,
        col_start: col.min(num_cols.saturating_sub(1)),
        row_span,
        col_span: col_span.min(num_cols - col.min(num_cols.saturating_sub(1))),
    }
}

fn find_free_cell(
    occupied: &std::collections::HashSet<(usize, usize)>,
    start_row: usize,
    start_col: usize,
    col_span: usize,
    row_span: usize,
    num_cols: usize,
) -> (usize, usize) {
    let mut r = start_row;
    let mut c = start_col;
    loop {
        // Does (r..r+row_span, c..c+col_span) fit and stay clear?
        let fits = c + col_span <= num_cols
            && (r..r + row_span).all(|rr| (c..c + col_span).all(|cc| !occupied.contains(&(rr, cc))));
        if fits {
            return (r, c);
        }
        c += 1;
        if c + col_span > num_cols {
            c = 0;
            r += 1;
        }
        // Sanity bound to prevent infinite loop on absurd inputs.
        if r > 10_000 {
            return (r, 0);
        }
    }
}

fn build_area_map(
    rows: &[Vec<String>],
) -> std::collections::HashMap<String, (usize, usize, usize, usize)> {
    let mut map: std::collections::HashMap<String, (usize, usize, usize, usize)> =
        std::collections::HashMap::new();
    for (r, row) in rows.iter().enumerate() {
        for (c, name) in row.iter().enumerate() {
            if name == "." {
                continue;
            }
            map.entry(name.clone())
                .and_modify(|e| {
                    if r < e.0 {
                        e.0 = r;
                    }
                    if c < e.1 {
                        e.1 = c;
                    }
                    if r > e.2 {
                        e.2 = r;
                    }
                    if c > e.3 {
                        e.3 = c;
                    }
                })
                .or_insert((r, c, r, c));
        }
    }
    map
}

fn shift_subtree(dom: &Dom, node: NodeId, dy: f32, tree: &mut BoxTree) {
    if let Some(b) = tree.boxes.get_mut(node.index()).and_then(|s| s.as_mut()) {
        b.rect.y += dy;
    }
    let kids: Vec<NodeId> = dom.children(node).collect();
    for c in kids {
        shift_subtree(dom, c, dy, tree);
    }
}

/// Resolve a list of `GridTrack`s into pixel widths that sum (with gaps) to
/// the available content width. Fixed and percentage tracks consume their
/// declared share; `fr` tracks divide the remaining space; `auto` tracks
/// are treated like a single fr track for the toy.
fn resolve_tracks(tracks: &[GridTrack], available: f32, gap: f32) -> Vec<f32> {
    if tracks.is_empty() {
        return vec![available];
    }
    let gap_total = gap * tracks.len().saturating_sub(1) as f32;
    let usable = (available - gap_total).max(0.0);

    let mut widths = vec![0.0_f32; tracks.len()];
    let mut fixed_total = 0.0_f32;
    let mut fr_total = 0.0_f32;
    for (i, t) in tracks.iter().enumerate() {
        match t {
            GridTrack::Px(px) => {
                widths[i] = *px;
                fixed_total += *px;
            }
            GridTrack::Percent(p) => {
                let w = usable * p / 100.0;
                widths[i] = w;
                fixed_total += w;
            }
            GridTrack::Fr(f) => {
                fr_total += *f;
            }
            GridTrack::Auto => {
                fr_total += 1.0;
            }
        }
    }
    let remaining = (usable - fixed_total).max(0.0);
    if fr_total > 0.0 {
        let unit = remaining / fr_total;
        for (i, t) in tracks.iter().enumerate() {
            match t {
                GridTrack::Fr(f) => widths[i] = unit * f,
                GridTrack::Auto => widths[i] = unit,
                _ => {}
            }
        }
    }
    widths
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

    fn find_all(dom: &Dom, root: NodeId, tag: &str, class: &str) -> Vec<NodeId> {
        let mut out = Vec::new();
        walk(dom, root, tag, class, &mut out);
        out
    }

    fn walk(dom: &Dom, id: NodeId, tag: &str, class: &str, out: &mut Vec<NodeId>) {
        if let NodeKind::Element { tag: t, attrs } = &dom.node(id).kind {
            if t == tag
                && attrs
                    .iter()
                    .find(|(k, _)| k == "class")
                    .map(|(_, v)| v.split_ascii_whitespace().any(|c| c == class))
                    .unwrap_or(false)
            {
                out.push(id);
            }
        }
        for c in dom.children(id).collect::<Vec<_>>() {
            walk(dom, c, tag, class, out);
        }
    }

    #[test]
    fn grid_template_columns_fr_splits_equally() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .g { display: grid; grid-template-columns: 1fr 1fr 1fr; margin: 0; padding: 0; } \
             .c { height: 50px; margin: 0; padding: 0; }</style>\
             <div class=g>\
               <div class=c></div><div class=c></div><div class=c></div>\
             </div>",
            900.0,
        );
        let cells = find_all(&dom, dom.document(), "div", "c");
        assert_eq!(cells.len(), 3);
        let widths: Vec<f32> = cells
            .iter()
            .map(|n| tree.get(*n).unwrap().rect.width)
            .collect();
        for w in &widths {
            assert!((w - 300.0).abs() < 1.0, "expected ~300, got {w}");
        }
    }

    #[test]
    fn grid_repeat_expands() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .g { display: grid; grid-template-columns: repeat(4, 1fr); margin: 0; padding: 0; } \
             .c { height: 50px; margin: 0; padding: 0; }</style>\
             <div class=g>\
               <div class=c></div><div class=c></div><div class=c></div><div class=c></div>\
             </div>",
            800.0,
        );
        let cells = find_all(&dom, dom.document(), "div", "c");
        assert_eq!(cells.len(), 4);
        for c in &cells {
            assert!((tree.get(*c).unwrap().rect.width - 200.0).abs() < 1.0);
        }
    }

    #[test]
    fn grid_wraps_after_columns_filled() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .g { display: grid; grid-template-columns: 1fr 1fr; margin: 0; padding: 0; } \
             .c { height: 50px; margin: 0; padding: 0; }</style>\
             <div class=g>\
               <div class=c></div><div class=c></div>\
               <div class=c></div><div class=c></div>\
             </div>",
            800.0,
        );
        let cells = find_all(&dom, dom.document(), "div", "c");
        let y0 = tree.get(cells[0]).unwrap().rect.y;
        let y2 = tree.get(cells[2]).unwrap().rect.y;
        assert!(y2 > y0 + 49.0, "third cell should be on the next row");
    }
}
