//! Flexbox layout — toy subset of CSS Flexible Box.
//!
//! Supports:
//!  * `flex-direction: row | column | row-reverse | column-reverse`
//!  * `justify-content: flex-start | flex-end | center | space-between |
//!     space-around | space-evenly` (main axis)
//!  * `align-items: flex-start | flex-end | center | stretch | baseline`
//!     (cross axis; `baseline` falls back to flex-start)
//!  * `flex-wrap: nowrap | wrap` (wrap-reverse falls back to wrap)
//!  * `gap` between adjacent items (both row and column directions)
//!  * `flex-grow` (distribution of extra main-axis space)
//!  * `flex-basis` (item's main-axis size hint; `auto` uses natural width)
//!
//! Skipped: `flex-shrink` semantics (we always shrink uniformly when
//! overflowing in nowrap), `align-content`, `order`, baseline alignment.

use super::block;
use super::replaced::ImageCache;
use super::text::TextLayout;
use super::{BoxKind, BoxTree, LayoutBox, Rect};
use crate::css::{
    AlignContent, AlignItems, ComputedStyle, Dimension, FlexDirection, FlexWrap, JustifyContent,
    StyleTree,
};
use crate::dom::{Dom, NodeId, NodeKind};

#[allow(clippy::too_many_arguments)]
pub fn layout_flex(
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
    let direction = style.flex_direction;
    let wrap = style.flex_wrap;
    let (row_gap, col_gap) = style.gap;
    let is_row = matches!(direction, FlexDirection::Row | FlexDirection::RowReverse);
    let main_gap = if is_row { col_gap } else { row_gap };
    let cross_gap = if is_row { row_gap } else { col_gap };

    // Container box dimensions (same as the block path).
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

    // Gather flex items: each non-`display: none` element child becomes an
    // item. Text nodes inside the flex container are wrapped into one
    // anonymous block per CSS spec — toy version: skip raw text.
    let mut item_nodes: Vec<NodeId> = dom
        .children(node)
        .filter(|c| match &dom.node(*c).kind {
            NodeKind::Element { .. } => {
                styles.get(*c).display != crate::css::Display::None
            }
            _ => false,
        })
        .collect();
    // Reorder by `order` (stable, so DOM order breaks ties).
    item_nodes.sort_by_key(|n| styles.get(*n).order);

    if item_nodes.is_empty() {
        // Empty flex container — fall through to a zero-content block.
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

    // Cross-axis (height for rows; width for columns) of the available main line.
    let main_size = if is_row {
        content_width
    } else {
        // For column flex, the container's "main axis" is vertical. We don't
        // have an explicit height; lay items end-to-end and let the
        // container grow.
        f32::INFINITY
    };

    // Measure each item's base size along the main axis.
    let mut items: Vec<FlexItem> = item_nodes
        .iter()
        .map(|&n| measure_item(dom, styles, text, images, n, content_width, is_row, tree))
        .collect();

    // Pack items into one or more lines.
    let lines: Vec<Vec<usize>> = pack_lines(&items, main_size, main_gap, wrap);

    // Pre-compute each line's natural cross-axis size so align-content can
    // distribute extra space.
    let mut line_cross: Vec<f32> = lines
        .iter()
        .map(|l| l.iter().map(|i| items[*i].base_cross).fold(0.0_f32, f32::max))
        .collect();
    let natural_cross: f32 = line_cross.iter().sum::<f32>()
        + cross_gap * line_cross.len().saturating_sub(1) as f32;
    let container_cross = if is_row {
        match style.height {
            Dimension::Length(h) => h,
            _ => natural_cross,
        }
    } else {
        match style.width {
            Dimension::Length(w) => w,
            _ => natural_cross,
        }
    };
    let mut extra_cross = (container_cross - natural_cross).max(0.0);
    if style.align_content == AlignContent::Stretch && !line_cross.is_empty() {
        let per = extra_cross / line_cross.len() as f32;
        for c in line_cross.iter_mut() {
            *c += per;
        }
        extra_cross = 0.0;
    }
    let (cross_start, line_step_extra) =
        align_content_offsets(style.align_content, extra_cross, line_cross.len());

    // Lay each line out.
    let mut cross_cursor = (if is_row { content_y } else { content_x }) + cross_start;
    let mut total_main = 0.0_f32;
    let mut total_cross = 0.0_f32;

    for (line_idx_outer, line_idxs) in lines.iter().enumerate() {
        // Sum of base sizes + gaps on this line.
        let count = line_idxs.len();
        let gap_total = main_gap * (count.saturating_sub(1) as f32);
        let base_sum: f32 = line_idxs.iter().map(|i| items[*i].base_main).sum();
        let cross_max: f32 = line_cross[line_idx_outer];

        // Distribute extra space via flex-grow.
        let line_main_target = if main_size.is_finite() {
            main_size
        } else {
            base_sum + gap_total
        };
        let extra = line_main_target - base_sum - gap_total;
        if extra > 0.0 {
            let total_grow: f32 = line_idxs.iter().map(|i| items[*i].grow).sum();
            if total_grow > 0.0 {
                for &i in line_idxs {
                    items[i].main_size = items[i].base_main + extra * (items[i].grow / total_grow);
                }
            } else {
                for &i in line_idxs {
                    items[i].main_size = items[i].base_main;
                }
            }
        } else if extra < 0.0 && wrap == FlexWrap::NoWrap {
            // Shrink everyone proportionally to fit.
            let shrink_total: f32 = line_idxs
                .iter()
                .map(|i| items[*i].base_main * items[*i].shrink.max(0.0))
                .sum();
            if shrink_total > 0.0 {
                for &i in line_idxs {
                    let share = items[i].base_main * items[i].shrink.max(0.0) / shrink_total;
                    items[i].main_size = (items[i].base_main + extra * share).max(0.0);
                }
            } else {
                for &i in line_idxs {
                    items[i].main_size = items[i].base_main;
                }
            }
        } else {
            for &i in line_idxs {
                items[i].main_size = items[i].base_main;
            }
        }

        // Determine starting main offset based on justify-content.
        let used_main: f32 = line_idxs.iter().map(|i| items[*i].main_size).sum::<f32>() + gap_total;
        let free = (line_main_target - used_main).max(0.0);
        let (main_start, main_step_extra) =
            justify_offsets(style.justify_content, free, line_idxs.len());

        let mut main_cursor = if is_row { content_x } else { content_y } + main_start;
        for (i_idx, &i) in line_idxs.iter().enumerate() {
            // Cross-axis size depends on align-items.
            let item_cross = match style.align_items {
                AlignItems::Stretch => cross_max,
                _ => items[i].base_cross,
            };
            // Cross-axis offset.
            let cross_off = match style.align_items {
                AlignItems::FlexEnd => cross_max - item_cross,
                AlignItems::Center => (cross_max - item_cross) * 0.5,
                _ => 0.0,
            };

            // Translate to (x, y) and lay out this item with the chosen size.
            let (cb_x, cb_y, cb_w) = if is_row {
                (main_cursor, cross_cursor + cross_off, items[i].main_size)
            } else {
                (cross_cursor + cross_off, main_cursor, item_cross)
            };
            let cb = Rect {
                x: cb_x,
                y: cb_y,
                width: cb_w,
                height: 0.0,
            };
            // Re-lay-out the child as a block within the chosen width.
            // Block layout returns the consumed margin-box height.
            let consumed = block::layout(dom, styles, text, images, items[i].node, cb, tree);

            // For row flex with stretch, force the box height to match line_height.
            if is_row && style.align_items == AlignItems::Stretch && cross_max > consumed {
                if let Some(b) = tree.boxes[items[i].node.index()].as_mut() {
                    b.rect.height = cross_max - items[i].outer_margin_y();
                }
            }

            // Advance main cursor (size + gap + extra from justify).
            main_cursor += items[i].main_size;
            if i_idx + 1 < line_idxs.len() {
                main_cursor += main_gap + main_step_extra;
            }
        }

        // Advance cross-axis past this line, including align-content gap.
        let advance = cross_max + cross_gap + line_step_extra;
        cross_cursor += advance;
        total_cross += advance;
        total_main = total_main.max(used_main);
    }

    let content_height = if is_row {
        // Subtract the trailing cross_gap we added for the last line.
        (total_cross - cross_gap).max(0.0)
    } else {
        // Column flex: total main consumption.
        let line_sums: f32 = lines.iter().map(|l| {
            l.iter().map(|i| items[*i].main_size).sum::<f32>()
                + main_gap * (l.len().saturating_sub(1) as f32)
        }).sum();
        let line_count = lines.len() as f32;
        line_sums + cross_gap * (line_count - 1.0).max(0.0)
    };

    let final_height = match style.height {
        Dimension::Length(h) => h,
        _ => content_height,
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
        crate::css::Display::InlineFlex => BoxKind::InlineBlock,
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

struct FlexItem {
    node: NodeId,
    /// Pre-grow base size along main axis (from `flex-basis` or content).
    base_main: f32,
    /// Cross-axis natural size.
    base_cross: f32,
    /// Final allocated size along main axis (filled in by the grow pass).
    main_size: f32,
    grow: f32,
    shrink: f32,
    margin_y: (f32, f32),
}

impl FlexItem {
    fn outer_margin_y(&self) -> f32 {
        self.margin_y.0 + self.margin_y.1
    }
}

fn measure_item(
    dom: &Dom,
    styles: &StyleTree,
    text: &mut TextLayout,
    images: &ImageCache,
    node: NodeId,
    cb_width: f32,
    is_row: bool,
    tree: &mut BoxTree,
) -> FlexItem {
    let style = styles.get(node);
    let grow = style.flex_grow;
    let shrink = style.flex_shrink;

    // Tentative block layout to learn the item's natural cross-axis size
    // and to populate its descendants' boxes.
    let cb = Rect {
        x: 0.0,
        y: 0.0,
        width: cb_width,
        height: 0.0,
    };
    block::layout(dom, styles, text, images, node, cb, tree);
    let b = tree
        .get(node)
        .cloned()
        .unwrap_or(LayoutBox {
            kind: BoxKind::Block,
            rect: Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 },
            padding: Default::default(),
            border: Default::default(),
            margin: Default::default(),
        });
    let block_main = if is_row { b.rect.width } else { b.rect.height };
    let base_cross = if is_row { b.rect.height } else { b.rect.width };

    // Compute the flex base size on the main axis:
    //   - explicit width/height wins (Length, Percent),
    //   - explicit `flex-basis` wins,
    //   - otherwise treat the natural content size as 0 if the item is
    //     "auto-sized with grow > 0" — i.e. the author wants flex to
    //     distribute space — and as the block-measured size otherwise.
    let explicit_main = if is_row { style.width } else { style.height };
    let base_main = match (style.flex_basis, explicit_main) {
        (Dimension::Length(px), _) => px,
        (Dimension::Percent(p), _) => cb_width * p / 100.0,
        (Dimension::Auto, Dimension::Length(px)) => px,
        (Dimension::Auto, Dimension::Percent(p)) => cb_width * p / 100.0,
        (Dimension::Auto, Dimension::Auto) => {
            if grow > 0.0 {
                0.0
            } else {
                block_main
            }
        }
    };

    FlexItem {
        node,
        base_main,
        base_cross,
        main_size: base_main,
        grow,
        shrink,
        margin_y: (b.margin.top, b.margin.bottom),
    }
}

fn pack_lines(items: &[FlexItem], main_size: f32, gap: f32, wrap: FlexWrap) -> Vec<Vec<usize>> {
    if wrap == FlexWrap::NoWrap || !main_size.is_finite() {
        return vec![(0..items.len()).collect()];
    }
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut consumed = 0.0_f32;
    for (i, it) in items.iter().enumerate() {
        let next_consumed = if current.is_empty() {
            it.base_main
        } else {
            consumed + gap + it.base_main
        };
        if next_consumed > main_size && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            consumed = it.base_main;
            current.push(i);
        } else {
            consumed = next_consumed;
            current.push(i);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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

    fn find(dom: &Dom, root: NodeId, tag: &str, class: &str) -> Option<NodeId> {
        if let NodeKind::Element { tag: t, attrs } = &dom.node(root).kind {
            if t == tag
                && attrs
                    .iter()
                    .find(|(k, _)| k == "class")
                    .map(|(_, v)| v.split_ascii_whitespace().any(|c| c == class))
                    .unwrap_or(false)
            {
                return Some(root);
            }
        }
        for c in dom.children(root).collect::<Vec<_>>() {
            if let Some(r) = find(dom, c, tag, class) {
                return Some(r);
            }
        }
        None
    }

    #[test]
    fn flex_row_lays_children_side_by_side() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .row { display: flex; margin: 0; padding: 0; } \
             .a, .b { width: 100px; height: 50px; margin: 0; padding: 0; }</style>\
             <div class=row><div class=a></div><div class=b></div></div>",
            1000.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        let ra = tree.get(a).unwrap().rect;
        let rb = tree.get(b).unwrap().rect;
        assert!((ra.y - rb.y).abs() < 1.0, "expected same y, got {} vs {}", ra.y, rb.y);
        assert!(rb.x > ra.x + 50.0);
    }

    #[test]
    fn flex_column_stacks_children_vertically() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .col { display: flex; flex-direction: column; margin: 0; padding: 0; } \
             .a, .b { width: 100px; height: 50px; margin: 0; padding: 0; }</style>\
             <div class=col><div class=a></div><div class=b></div></div>",
            1000.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        let ra = tree.get(a).unwrap().rect;
        let rb = tree.get(b).unwrap().rect;
        assert!(rb.y >= ra.y + 50.0);
    }

    #[test]
    fn justify_center_centers_items_on_main_axis() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .row { display: flex; justify-content: center; margin: 0; padding: 0; } \
             .a { width: 100px; height: 50px; margin: 0; padding: 0; }</style>\
             <div class=row><div class=a></div></div>",
            1000.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let ra = tree.get(a).unwrap().rect;
        // Container is 1000px wide, item 100px → centered means x ≈ 450.
        assert!((ra.x - 450.0).abs() < 5.0, "expected x ≈ 450, got {}", ra.x);
    }

    #[test]
    fn gap_inserts_space_between_items() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .row { display: flex; gap: 20px; margin: 0; padding: 0; } \
             .a, .b { width: 100px; height: 50px; margin: 0; padding: 0; }</style>\
             <div class=row><div class=a></div><div class=b></div></div>",
            1000.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        let ra = tree.get(a).unwrap().rect;
        let rb = tree.get(b).unwrap().rect;
        let gap = rb.x - (ra.x + ra.width);
        assert!((gap - 20.0).abs() < 1.0, "expected 20px gap, got {gap}");
    }

    #[test]
    fn order_reorders_items() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .row { display: flex; margin: 0; padding: 0; } \
             .a, .b { width: 100px; height: 50px; margin: 0; padding: 0; } \
             .a { order: 2; } .b { order: 1; }</style>\
             <div class=row><div class=a></div><div class=b></div></div>",
            1000.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        // b has lower order → appears first → smaller x.
        let ra = tree.get(a).unwrap().rect;
        let rb = tree.get(b).unwrap().rect;
        assert!(rb.x < ra.x, "b.x ({}) should be left of a.x ({})", rb.x, ra.x);
    }

    #[test]
    fn box_sizing_border_box_includes_padding() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .b { box-sizing: border-box; width: 200px; padding: 30px; \
                  margin: 0; height: 100px; }</style>\
             <div class=b></div>",
            1000.0,
        );
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        let rb = tree.get(b).unwrap().rect;
        // With border-box, the declared 200px already INCLUDES padding, so
        // the border-box rect is exactly 200×100.
        assert!((rb.width - 200.0).abs() < 0.5, "width = {}", rb.width);
        assert!((rb.height - 100.0).abs() < 0.5, "height = {}", rb.height);
    }

    #[test]
    fn min_max_width_clamp() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .a { width: 50px; min-width: 100px; height: 40px; margin: 0; padding: 0; }\
             .b { width: 1000px; max-width: 200px; height: 40px; margin: 0; padding: 0; }</style>\
             <div class=a></div><div class=b></div>",
            1500.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        assert!((tree.get(a).unwrap().rect.width - 100.0).abs() < 0.5);
        assert!((tree.get(b).unwrap().rect.width - 200.0).abs() < 0.5);
    }

    #[test]
    fn flex_grow_distributes_extra_space() {
        let (dom, tree) = run(
            "<style>body { margin: 0; } \
             .row { display: flex; margin: 0; padding: 0; } \
             .a, .b { height: 50px; margin: 0; padding: 0; } \
             .a { flex-grow: 1; } .b { flex-grow: 2; }</style>\
             <div class=row><div class=a></div><div class=b></div></div>",
            900.0,
        );
        let a = find(&dom, dom.document(), "div", "a").unwrap();
        let b = find(&dom, dom.document(), "div", "b").unwrap();
        let ra = tree.get(a).unwrap().rect;
        let rb = tree.get(b).unwrap().rect;
        // With grow 1:2 and zero base sizes filling 900px, the split should
        // be 300 : 600 (B is twice A).
        let ratio = rb.width / ra.width;
        assert!(
            (ratio - 2.0).abs() < 0.1,
            "expected 1:2 ratio, got {} : {} = {}",
            ra.width, rb.width, ratio
        );
    }
}

/// Cross-axis distribution of extra space between flex lines. Mirrors
/// `justify_offsets` but for the cross axis with `align-content` values.
fn align_content_offsets(a: AlignContent, free: f32, count: usize) -> (f32, f32) {
    if count == 0 || free <= 0.0 {
        return (0.0, 0.0);
    }
    match a {
        AlignContent::FlexStart | AlignContent::Stretch => (0.0, 0.0),
        AlignContent::FlexEnd => (free, 0.0),
        AlignContent::Center => (free * 0.5, 0.0),
        AlignContent::SpaceBetween => {
            if count > 1 {
                (0.0, free / (count - 1) as f32)
            } else {
                (0.0, 0.0)
            }
        }
        AlignContent::SpaceAround => {
            let slot = free / count as f32;
            (slot * 0.5, slot)
        }
        AlignContent::SpaceEvenly => {
            let slot = free / (count + 1) as f32;
            (slot, slot)
        }
    }
}

/// Returns `(start_offset, per_gap_extra)` where `start_offset` is the
/// distance before the first item and `per_gap_extra` is added between
/// consecutive items in addition to the explicit `gap`.
fn justify_offsets(j: JustifyContent, free: f32, count: usize) -> (f32, f32) {
    if count == 0 {
        return (0.0, 0.0);
    }
    match j {
        JustifyContent::FlexStart => (0.0, 0.0),
        JustifyContent::FlexEnd => (free, 0.0),
        JustifyContent::Center => (free * 0.5, 0.0),
        JustifyContent::SpaceBetween => {
            if count > 1 {
                (0.0, free / (count - 1) as f32)
            } else {
                (0.0, 0.0)
            }
        }
        JustifyContent::SpaceAround => {
            let slot = free / count as f32;
            (slot * 0.5, slot)
        }
        JustifyContent::SpaceEvenly => {
            let slot = free / (count + 1) as f32;
            (slot, slot)
        }
    }
}
