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

    // First pass: figure out how many columns we actually need.
    // Templates + areas set a baseline; explicit placements past the
    // template width grow the implicit grid (CSS Grid spec §6.1).
    let template_cols = style.grid_template_columns.len();
    let area_cols = style
        .grid_template_areas
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0);
    let mut num_cols = template_cols.max(area_cols).max(1);
    let max_explicit_col = max_explicit_column(styles, &items, &area_map);
    if max_explicit_col + 1 > num_cols {
        num_cols = max_explicit_col + 1;
    }

    // Resolve column widths. Any implicit (extra) tracks beyond the
    // template inherit `grid-auto-columns`.
    let mut explicit_tracks: Vec<GridTrack> = style.grid_template_columns.clone();
    while explicit_tracks.len() < num_cols {
        explicit_tracks.push(style.grid_auto_columns.clone());
    }
    let column_widths = resolve_tracks(&explicit_tracks, content_width, col_gap);

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

    // Pass 2: shift each item to its final y based on row_start, then
    // apply justify-self / align-self within its cell.
    for (idx, p) in placements.iter().enumerate() {
        let item_style = styles.get(items[idx]);
        let justify_self = item_style.justify_self.unwrap_or(style.justify_items);
        let align_self = item_style.align_self.unwrap_or(style.align_items);

        let row_end = (p.row_start + p.row_span).min(row_heights.len());
        let cell_y = row_ys[p.row_start];
        let cell_h: f32 = row_heights[p.row_start..row_end].iter().sum::<f32>()
            + row_gap * (p.row_span.saturating_sub(1) as f32);
        let col_end = (p.col_start + p.col_span).min(column_widths.len());
        let cell_w: f32 = (p.col_start..col_end)
            .map(|c| column_widths[c])
            .sum::<f32>()
            + col_gap * (p.col_span.saturating_sub(1) as f32);

        let Some(current_box) = tree.boxes[items[idx].index()].as_ref() else {
            continue;
        };
        let item_w = current_box.rect.width;
        let item_h = current_box.rect.height;

        // Inline-axis (`justify-self`) placement within cell.
        let dx = match justify_self {
            crate::css::AlignItems::FlexEnd => (cell_w - item_w).max(0.0),
            crate::css::AlignItems::Center => ((cell_w - item_w) / 2.0).max(0.0),
            crate::css::AlignItems::Stretch | crate::css::AlignItems::Baseline => 0.0,
            _ => 0.0,
        };
        // Block-axis (`align-self`) placement within cell.
        let dy = (cell_y - current_box.rect.y)
            + match align_self {
                crate::css::AlignItems::FlexEnd => (cell_h - item_h).max(0.0),
                crate::css::AlignItems::Center => ((cell_h - item_h) / 2.0).max(0.0),
                _ => 0.0,
            };

        if dx.abs() > 0.001 || dy.abs() > 0.001 {
            shift_subtree_xy(dom, items[idx], dx, dy, tree);
        }

        // Stretch the item box to fill the cell when alignment is
        // `stretch` (the spec default).
        if let Some(b) = tree.boxes[items[idx].index()].as_mut() {
            if matches!(justify_self, crate::css::AlignItems::Stretch)
                && b.rect.width < cell_w
            {
                b.rect.width = cell_w;
            }
            if matches!(align_self, crate::css::AlignItems::Stretch) && p.row_span >= 1
            {
                b.rect.height = b.rect.height.max(cell_h);
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

/// Largest column index any explicitly-placed item references. Used
/// to size the implicit grid past the template (CSS Grid §6.1).
fn max_explicit_column(
    styles: &StyleTree,
    items: &[NodeId],
    area_map: &std::collections::HashMap<String, (usize, usize, usize, usize)>,
) -> usize {
    let mut max_col = 0usize;
    for &node in items {
        let p = &styles.get(node).grid_placement;
        if let Some(name) = &p.area {
            if let Some((_, _, _, c1)) = area_map.get(name).copied() {
                if c1 > max_col {
                    max_col = c1;
                }
            }
            continue;
        }
        let start = match &p.column_start {
            Some(GridLine::Index(n)) => (*n as i32 - 1).max(0) as usize,
            Some(GridLine::Name(n)) => area_map.get(n).map(|t| t.1).unwrap_or(0),
            _ => 0,
        };
        let span = match (&p.column_start, &p.column_end) {
            (_, Some(GridLine::Span(n))) => (*n as usize).max(1),
            (Some(GridLine::Index(a)), Some(GridLine::Index(b))) => {
                ((*b as i32 - *a as i32).max(1)) as usize
            }
            _ => 1,
        };
        let end_col = start + span - 1;
        if end_col > max_col {
            max_col = end_col;
        }
    }
    max_col
}

/// Resolve a list of `GridTrack`s into pixel widths that sum (with gaps) to
/// the available content width. Fixed and percentage tracks consume their
/// declared share; `fr` tracks divide the remaining space; `auto` tracks
/// are treated like a single fr track for the toy. `minmax(min, max)`
/// clamps the resolved track size between the two endpoints.
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
            GridTrack::MinMax(min, max) => {
                // The base size is the min track resolved against the
                // available space; the max contributes fr weight (if
                // fr-typed) so the track can grow.
                let base = resolve_single_track(min, usable, 0.0);
                widths[i] = base;
                fixed_total += base;
                if let GridTrack::Fr(f) = max.as_ref() {
                    fr_total += *f;
                }
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
                GridTrack::MinMax(min, max) => {
                    if let GridTrack::Fr(f) = max.as_ref() {
                        let want = unit * f;
                        let min_px = resolve_single_track(min, usable, 0.0);
                        let max_px = match max.as_ref() {
                            GridTrack::Px(px) => *px,
                            GridTrack::Percent(p) => usable * p / 100.0,
                            _ => f32::INFINITY,
                        };
                        widths[i] = want.max(min_px).min(max_px);
                    }
                }
                _ => {}
            }
        }
    }
    widths
}

/// Resolve a single non-minmax track for the `min` side of a `minmax()`.
/// Handles only the leaf-level variants; `fr` / `Auto` collapse to 0.
fn resolve_single_track(t: &GridTrack, usable: f32, fr_unit: f32) -> f32 {
    match t {
        GridTrack::Px(px) => *px,
        GridTrack::Percent(p) => usable * p / 100.0,
        GridTrack::Fr(f) => fr_unit * f,
        GridTrack::Auto => 0.0,
        GridTrack::MinMax(min, _) => resolve_single_track(min, usable, fr_unit),
    }
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
