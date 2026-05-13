//! Text shaping wrapper around `cosmic-text`. Owns one `FontSystem`
//! (which scans system fonts the first time it's constructed). Provides
//! two entry points:
//!
//! * `measure`     — quick "how many lines, how tall" check used by the
//!                   block-level layout when a text node sits on its own.
//! * `shape_inline` — full shaping with per-span attrs (weight, style) used
//!                    by the inline formatting context. Returns the laid-out
//!                    glyph positions so callers can attribute them back to
//!                    their source DOM nodes.

use std::ops::Range;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Style, Weight, Wrap};

use crate::css::{ComputedStyle, FontStyle, StyleTree};
use crate::dom::NodeId;

pub struct TextLayout {
    system: FontSystem,
}

impl TextLayout {
    pub fn new() -> Self {
        Self {
            system: FontSystem::new(),
        }
    }

    /// Measure how wide `text` would be if it were never wrapped — i.e. its
    /// "max content" intrinsic width. Used by table layout to size columns
    /// against the natural widths of their cells.
    pub fn measure_natural_width(&mut self, text: &str, style: &ComputedStyle) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let line_height = style.font_size * style.line_height;
        let metrics = Metrics::new(style.font_size, line_height);
        let mut buffer = Buffer::new(&mut self.system, metrics);
        // Effectively unbounded width: cosmic-text won't wrap.
        buffer.set_size(&mut self.system, Some(f32::MAX / 2.0), None);
        buffer.set_wrap(&mut self.system, Wrap::None);
        buffer.set_text(&mut self.system, text, attrs_from_style(style), Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.system, false);
        let mut max_w = 0.0f32;
        for run in buffer.layout_runs() {
            if run.line_w > max_w {
                max_w = run.line_w;
            }
        }
        max_w
    }

    /// Shape an inline run made of contiguous spans from different DOM nodes.
    /// Returns absolute glyph positions (relative to the IFC origin) so the
    /// caller can union glyph rects per source node.
    pub fn shape_inline(
        &mut self,
        content: &InlineContent,
        max_width: f32,
        parent_style: &ComputedStyle,
        styles: &StyleTree,
    ) -> ShapedText {
        if content.text.is_empty() || max_width <= 0.0 {
            return ShapedText::default();
        }
        let line_height = parent_style.font_size * parent_style.line_height;
        let metrics = Metrics::new(parent_style.font_size, line_height);
        let mut buffer = Buffer::new(&mut self.system, metrics);
        buffer.set_size(&mut self.system, Some(max_width), None);
        buffer.set_wrap(&mut self.system, Wrap::Word);

        // Build rich-text spans from the text-owning spans only (element
        // spans nest and would overlap; the text spans are non-overlapping
        // and together cover the whole string).
        let mut text_spans: Vec<&InlineSpan> = content
            .spans
            .iter()
            .filter(|s| !s.is_element)
            .collect();
        text_spans.sort_by_key(|s| s.range.start);

        let mut rich: Vec<(&str, Attrs<'static>)> = Vec::new();
        let mut cursor = 0usize;
        for span in &text_spans {
            if span.range.start > cursor {
                rich.push((&content.text[cursor..span.range.start], Attrs::new()));
            }
            let slice = &content.text[span.range.start..span.range.end];
            rich.push((slice, attrs_from_style(styles.get(span.node))));
            cursor = span.range.end;
        }
        if cursor < content.text.len() {
            rich.push((&content.text[cursor..], Attrs::new()));
        }

        buffer.set_rich_text(&mut self.system, rich, Attrs::new(), Shaping::Advanced);
        buffer.shape_until_scroll(&mut self.system, false);

        let mut glyphs = Vec::new();
        let mut total_height = 0.0_f32;
        let mut max_w = 0.0_f32;
        for run in buffer.layout_runs() {
            for g in run.glyphs.iter() {
                glyphs.push(ShapedGlyph {
                    text_start: g.start,
                    x: g.x,
                    y: run.line_top,
                    width: g.w,
                    height: run.line_height,
                });
            }
            let bottom = run.line_top + run.line_height;
            if bottom > total_height {
                total_height = bottom;
            }
            if run.line_w > max_w {
                max_w = run.line_w;
            }
        }

        ShapedText {
            glyphs,
            total_height,
            total_width: max_w,
        }
    }
}

fn attrs_from_style(style: &ComputedStyle) -> Attrs<'static> {
    Attrs::new()
        .family(Family::Serif)
        .weight(Weight(style.font_weight))
        .style(match style.font_style {
            FontStyle::Italic => Style::Italic,
            FontStyle::Normal => Style::Normal,
        })
}

#[derive(Debug, Default)]
#[allow(dead_code)] // total_width used by inline auto-sizing in 4d
pub struct ShapedText {
    pub glyphs: Vec<ShapedGlyph>,
    pub total_height: f32,
    pub total_width: f32,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // text_end retained for future per-glyph hit testing in 4d/6
pub struct ShapedGlyph {
    pub text_start: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Accumulated text + spans for an inline formatting context. `text` is
/// the concatenation of every collapsed text-node content within the
/// inline run. `spans` records, for each contributing node, the byte range
/// it occupies in `text`.
#[derive(Debug, Default)]
pub struct InlineContent {
    pub text: String,
    pub spans: Vec<InlineSpan>,
}

#[derive(Debug)]
pub struct InlineSpan {
    pub range: Range<usize>,
    pub node: NodeId,
    /// Element spans nest text spans; their range is the union of their
    /// inline descendants. Used to compute bounding rects per inline
    /// element. Text spans don't nest and are leaves.
    pub is_element: bool,
}

/// CSS `white-space: normal` collapse — runs of whitespace become a single
/// space. Leading/trailing whitespace is preserved as a single space if the
/// source had any (so inline siblings can keep the gap between them).
/// Whitespace-only input collapses to a single space (significant between
/// two inline siblings).
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let starts_ws = s.chars().next().is_some_and(|c| c.is_whitespace());
    let ends_ws = s.chars().next_back().is_some_and(|c| c.is_whitespace());
    let mut last_was_ws = false;
    let mut content_seen = false;
    for c in s.chars() {
        if c.is_whitespace() {
            last_was_ws = true;
        } else {
            if last_was_ws && content_seen {
                out.push(' ');
            }
            out.push(c);
            content_seen = true;
            last_was_ws = false;
        }
    }
    if !content_seen {
        if starts_ws || ends_ws {
            out.push(' ');
        }
        return out;
    }
    if starts_ws {
        out.insert(0, ' ');
    }
    if ends_ws {
        out.push(' ');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_internal() {
        assert_eq!(collapse_whitespace("hello world"), "hello world");
        assert_eq!(collapse_whitespace("hello   world"), "hello world");
        // Tab and newline are whitespace too; the leading \t survives as a
        // single leading space (for inline-sibling joining); the internal
        // run collapses.
        assert_eq!(collapse_whitespace("\thello\nworld"), " hello world");
    }

    #[test]
    fn collapse_preserves_edges_if_source_had_them() {
        assert_eq!(collapse_whitespace(" hello "), " hello ");
        assert_eq!(collapse_whitespace("  hello"), " hello");
        assert_eq!(collapse_whitespace("hello  "), "hello ");
    }

    #[test]
    fn collapse_empty_and_whitespace_only() {
        assert_eq!(collapse_whitespace(""), "");
        assert_eq!(collapse_whitespace("   "), " "); // becomes the inter-sibling space
    }
}
