//! Rasterise the box tree into a `tiny-skia` `Pixmap`.
//!
//! Walks every node depth-first, painting in this order per node:
//!   1. Box shadow (offset rect with the shadow color)
//!   2. Background fill (solid, gradient, or image)
//!   3. Border (top/right/bottom/left sides)
//!   4. `::before` pseudo (text + decoration)
//!   5. Own content — text node, `<img>`, etc.
//!   6. Children (recursive)
//!   7. `::after` pseudo (text + decoration)
//!
//! The paint pipeline carries a `PaintCtx` down the recursion: an
//! accumulated alpha factor (for nested `opacity`) and a translation offset
//! (for `transform: translate(...)`). Both compose naturally — each
//! element's opacity multiplies into the running factor, each translate
//! adds into the running offset.
//!
//! Border-radius rounds the background corners (and we use the same path
//! for box-shadow); border sides are drawn unrounded (toy simplification —
//! rounded mitres are non-trivial).

use cosmic_text::{
    Attrs, Buffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, Style as CtStyle,
    SwashCache, Weight, Wrap,
};
use tiny_skia::{
    Color as SkColor, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap,
    PixmapPaint, Point, Rect as SkRect, Shader, SpreadMode, Transform,
};

use crate::css::{
    BackgroundImage, BorderStyle, Color, ComputedStyle, FontStyle, StyleTree, TextDecoration,
};
use crate::dom::{Dom, NodeId, NodeKind};
use crate::layout::{BoxTree, ImageCache, ImageInfo, ImageSlot, LayoutBox, PseudoKind, Rect};

pub fn paint(
    dom: &Dom,
    styles: &StyleTree,
    tree: &BoxTree,
    images: &ImageCache,
    width: u32,
    height: u32,
) -> Option<Pixmap> {
    let mut painter = Painter::new(width, height)?;
    painter.fill_background(Color::WHITE);
    let ctx = PaintCtx::root();
    painter.paint_subtree(dom, styles, tree, images, dom.document(), ctx);
    Some(painter.pixmap)
}

#[derive(Debug, Clone, Copy)]
struct PaintCtx {
    /// Multiplicative alpha factor (composes via multiplication on recursion).
    alpha: f32,
    /// Cumulative translation offset (composes via addition on recursion).
    tx: f32,
    ty: f32,
}

impl PaintCtx {
    fn root() -> Self {
        Self {
            alpha: 1.0,
            tx: 0.0,
            ty: 0.0,
        }
    }
    fn with(&self, style: &ComputedStyle) -> Self {
        let (dx, dy) = style.transform_translate.unwrap_or((0.0, 0.0));
        Self {
            alpha: self.alpha * style.opacity,
            tx: self.tx + dx,
            ty: self.ty + dy,
        }
    }
}

struct Painter {
    pixmap: Pixmap,
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl Painter {
    fn new(width: u32, height: u32) -> Option<Self> {
        Pixmap::new(width.max(1), height.max(1)).map(|pixmap| Self {
            pixmap,
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        })
    }

    fn fill_background(&mut self, color: Color) {
        self.pixmap.fill(color_to_sk(color));
    }

    fn paint_subtree(
        &mut self,
        dom: &Dom,
        styles: &StyleTree,
        tree: &BoxTree,
        images: &ImageCache,
        node: NodeId,
        parent_ctx: PaintCtx,
    ) {
        let style = styles.get(node);
        let ctx = parent_ctx.with(style);
        if ctx.alpha < 0.001 {
            return; // fully transparent subtree
        }

        if let Some(b) = tree.get(node) {
            if b.rect.y + ctx.ty >= self.pixmap.height() as f32 {
                return;
            }
            self.paint_box_shadow(b, style, ctx);
            self.paint_background(b, style, ctx, node, images);
            self.paint_border(b, style, ctx);
        }

        if let Some(p) = tree.pseudo_boxes.get(&(node, PseudoKind::Before)) {
            if let Some(s) = styles.before_style(node) {
                let pseudo_ctx = ctx.with(s);
                self.paint_text(p.rect, &p.text, s, pseudo_ctx);
            }
        }

        match &dom.node(node).kind {
            NodeKind::Text(s) => {
                if let Some(b) = tree.get(node) {
                    self.paint_text(b.rect, s, style, ctx);
                }
            }
            NodeKind::Element { tag, .. } if tag == "img" => {
                if let (Some(b), Some(info)) =
                    (tree.get(node), images.get(&(node, ImageSlot::Img)))
                {
                    self.paint_image(b.rect, info, ctx);
                }
            }
            _ => {}
        }

        let kids: Vec<NodeId> = dom.children(node).collect();
        for child in kids {
            self.paint_subtree(dom, styles, tree, images, child, ctx);
        }

        if let Some(p) = tree.pseudo_boxes.get(&(node, PseudoKind::After)) {
            if let Some(s) = styles.after_style(node) {
                let pseudo_ctx = ctx.with(s);
                self.paint_text(p.rect, &p.text, s, pseudo_ctx);
            }
        }
    }

    // ---------- backgrounds, borders, shadows ----------

    fn paint_box_shadow(&mut self, b: &LayoutBox, style: &ComputedStyle, ctx: PaintCtx) {
        let Some(shadow) = style.box_shadow else {
            return;
        };
        let mut color = shadow.color;
        color.a = ((color.a as f32) * ctx.alpha).clamp(0.0, 255.0) as u8;
        if color.a == 0 {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color_to_sk(color));
        // No real blur — render the shadow as an offset rounded rect of the
        // same shape as the element, expanded slightly by half the blur.
        let grow = shadow.blur * 0.5;
        let x = b.rect.x + shadow.offset_x - grow + ctx.tx;
        let y = b.rect.y + shadow.offset_y - grow + ctx.ty;
        let w = b.rect.width + 2.0 * grow;
        let h = b.rect.height + 2.0 * grow;
        self.fill_rounded_or_rect(x, y, w, h, style.border_radius, &paint);
    }

    fn paint_background(
        &mut self,
        b: &LayoutBox,
        style: &ComputedStyle,
        ctx: PaintCtx,
        node: NodeId,
        images: &ImageCache,
    ) {
        let x = b.rect.x + ctx.tx;
        let y = b.rect.y + ctx.ty;
        let w = b.rect.width.max(0.0);
        let h = b.rect.height.max(0.0);
        let radius = style.border_radius;

        // Solid background color first (so a translucent gradient/image still
        // shows the underlying color band).
        if style.background_color.a > 0 {
            let mut c = style.background_color;
            c.a = ((c.a as f32) * ctx.alpha).clamp(0.0, 255.0) as u8;
            let mut paint = Paint::default();
            paint.set_color(color_to_sk(c));
            self.fill_rounded_or_rect(x, y, w, h, radius, &paint);
        }

        match &style.background_image {
            Some(BackgroundImage::Url(_)) => {
                if let Some(info) = images.get(&(node, ImageSlot::Background)) {
                    self.paint_image(
                        Rect {
                            x: b.rect.x,
                            y: b.rect.y,
                            width: w,
                            height: h,
                        },
                        info,
                        ctx,
                    );
                }
            }
            Some(BackgroundImage::LinearGradient { angle_deg, stops }) => {
                self.paint_linear_gradient(x, y, w, h, *angle_deg, stops, ctx);
            }
            None => {}
        }
    }

    fn paint_border(&mut self, b: &LayoutBox, style: &ComputedStyle, ctx: PaintCtx) {
        if style.border_style == BorderStyle::None {
            return;
        }
        let mut color = style.border_color;
        color.a = ((color.a as f32) * ctx.alpha).clamp(0.0, 255.0) as u8;
        if color.a == 0 {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color_to_sk(color));
        let x = b.rect.x + ctx.tx;
        let y = b.rect.y + ctx.ty;
        let w = b.rect.width.max(0.0);
        let h = b.rect.height.max(0.0);
        let bs = b.border;
        let fill = |this: &mut Painter, r: SkRect| {
            this.pixmap
                .fill_rect(r, &paint, Transform::identity(), None);
        };
        if bs.top > 0.0 {
            if let Some(r) = SkRect::from_xywh(x, y, w, bs.top) {
                fill(self, r);
            }
        }
        if bs.right > 0.0 {
            if let Some(r) = SkRect::from_xywh(x + w - bs.right, y, bs.right, h) {
                fill(self, r);
            }
        }
        if bs.bottom > 0.0 {
            if let Some(r) = SkRect::from_xywh(x, y + h - bs.bottom, w, bs.bottom) {
                fill(self, r);
            }
        }
        if bs.left > 0.0 {
            if let Some(r) = SkRect::from_xywh(x, y, bs.left, h) {
                fill(self, r);
            }
        }
    }

    fn fill_rounded_or_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = radius.min(w * 0.5).min(h * 0.5);
        if r <= 0.5 {
            if let Some(rect) = SkRect::from_xywh(x, y, w, h) {
                self.pixmap
                    .fill_rect(rect, paint, Transform::identity(), None);
            }
            return;
        }
        // Approximate a rounded rect via two filled rects + four quadrant
        // arcs from PathBuilder.
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.quad_to(x + w, y, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.quad_to(x + w, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.quad_to(x, y + h, x, y + h - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        if let Some(path) = pb.finish() {
            self.pixmap.fill_path(
                &path,
                paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    fn paint_linear_gradient(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        angle_deg: f32,
        stops: &[(f32, Color)],
        ctx: PaintCtx,
    ) {
        if stops.is_empty() || w <= 0.0 || h <= 0.0 {
            return;
        }
        // Map angle (CSS: 0deg = upward) to a direction vector. 180deg →
        // top-to-bottom; 90deg → left-to-right.
        let rad = (angle_deg.to_radians()) - std::f32::consts::FRAC_PI_2;
        // Diagonal of box, projected onto the direction.
        let half = (w.max(h)) * 0.5;
        let cx = x + w * 0.5;
        let cy = y + h * 0.5;
        let dirx = rad.cos();
        let diry = rad.sin();
        let p0 = Point::from_xy(cx - dirx * half, cy - diry * half);
        let p1 = Point::from_xy(cx + dirx * half, cy + diry * half);

        let sk_stops: Vec<GradientStop> = stops
            .iter()
            .map(|(pos, c)| {
                let mut c = *c;
                c.a = ((c.a as f32) * ctx.alpha).clamp(0.0, 255.0) as u8;
                GradientStop::new(*pos, color_to_sk(c))
            })
            .collect();

        let Some(shader) =
            LinearGradient::new(p0, p1, sk_stops, SpreadMode::Pad, Transform::identity())
        else {
            return;
        };
        let mut paint = Paint::default();
        paint.shader = shader;
        if let Some(rect) = SkRect::from_xywh(x, y, w, h) {
            self.pixmap
                .fill_rect(rect, &paint, Transform::identity(), None);
        }
    }

    fn paint_image(&mut self, dest: Rect, info: &ImageInfo, ctx: PaintCtx) {
        if info.width == 0 || info.height == 0 {
            return;
        }
        let mut src = match Pixmap::new(info.width, info.height) {
            Some(p) => p,
            None => return,
        };
        let dst = src.data_mut();
        for (i, chunk) in info.rgba.chunks_exact(4).enumerate() {
            let r = chunk[0];
            let g = chunk[1];
            let b = chunk[2];
            let a = chunk[3];
            let p = i * 4;
            dst[p] = ((r as u16 * a as u16) / 255) as u8;
            dst[p + 1] = ((g as u16 * a as u16) / 255) as u8;
            dst[p + 2] = ((b as u16 * a as u16) / 255) as u8;
            dst[p + 3] = a;
        }
        let scale_x = dest.width / info.width as f32;
        let scale_y = dest.height / info.height as f32;
        let transform = Transform::from_translate(dest.x + ctx.tx, dest.y + ctx.ty)
            .pre_scale(scale_x, scale_y);
        let mut paint = PixmapPaint::default();
        paint.opacity = ctx.alpha;
        self.pixmap
            .draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }

    // ---------- text ----------

    fn paint_text(&mut self, rect: Rect, text: &str, style: &ComputedStyle, ctx: PaintCtx) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let line_height = style.font_size * style.line_height;
        let metrics = Metrics::new(style.font_size, line_height);

        let pmap_w = self.pixmap.width() as i32;
        let pmap_h = self.pixmap.height() as i32;
        let pixmap = &mut self.pixmap;
        let font_system = &mut self.font_system;
        let swash_cache = &mut self.swash_cache;

        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_size(font_system, Some(rect.width.max(1.0)), None);
        buffer.set_wrap(font_system, Wrap::Word);
        let attrs = Attrs::new()
            .family(family_from_style(style))
            .weight(Weight(style.font_weight))
            .style(match style.font_style {
                FontStyle::Italic => CtStyle::Italic,
                _ => CtStyle::Normal,
            });
        buffer.set_text(font_system, text, attrs, Shaping::Advanced);
        buffer.shape_until_scroll(font_system, false);

        let mut color = style.color;
        color.a = ((color.a as f32) * ctx.alpha).clamp(0.0, 255.0) as u8;
        let ct_color = CtColor::rgba(color.r, color.g, color.b, color.a);

        // Track per-line geometry so we can stamp text-decoration after the
        // glyphs are painted.
        let mut decoration_rows: Vec<(f32, f32, f32)> = Vec::new(); // (line_top, line_w, line_height)

        for run in buffer.layout_runs() {
            decoration_rows.push((run.line_top, run.line_w, run.line_height));
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((rect.x + ctx.tx, rect.y + ctx.ty + run.line_y), 1.0);
                let cache_key = physical.cache_key;
                let glyph_x = physical.x;
                let glyph_y = physical.y;

                let data = pixmap.data_mut();
                swash_cache.with_pixels(font_system, cache_key, ct_color, |x_off, y_off, color| {
                    let px = glyph_x + x_off;
                    let py = glyph_y + y_off;
                    if px < 0 || py < 0 || px >= pmap_w || py >= pmap_h {
                        return;
                    }
                    let idx = (py as usize * pmap_w as usize + px as usize) * 4;
                    let src_a = color.a();
                    if src_a == 0 {
                        return;
                    }
                    let inv_a = 255 - src_a as u16;
                    let sr = (color.r() as u16 * src_a as u16) / 255;
                    let sg = (color.g() as u16 * src_a as u16) / 255;
                    let sb = (color.b() as u16 * src_a as u16) / 255;
                    data[idx] = (sr + (data[idx] as u16 * inv_a) / 255) as u8;
                    data[idx + 1] = (sg + (data[idx + 1] as u16 * inv_a) / 255) as u8;
                    data[idx + 2] = (sb + (data[idx + 2] as u16 * inv_a) / 255) as u8;
                    data[idx + 3] =
                        (src_a as u16 + (data[idx + 3] as u16 * inv_a) / 255) as u8;
                });
            }
        }

        // Text decorations: paint a thin colored line per run at the
        // appropriate vertical offset.
        if style.text_decoration != TextDecoration::None {
            let mut paint = Paint::default();
            paint.set_color(color_to_sk(color));
            let thickness = (style.font_size * 0.07).max(1.0);
            for (line_top, line_w, lh) in decoration_rows {
                let y_offset = match style.text_decoration {
                    TextDecoration::Underline => lh * 0.85,
                    TextDecoration::LineThrough => lh * 0.55,
                    TextDecoration::Overline => lh * 0.05,
                    TextDecoration::None => continue,
                };
                let lx = rect.x + ctx.tx;
                let ly = rect.y + ctx.ty + line_top + y_offset - thickness * 0.5;
                if let Some(r) = SkRect::from_xywh(lx, ly, line_w, thickness) {
                    self.pixmap
                        .fill_rect(r, &paint, Transform::identity(), None);
                }
            }
        }
    }
}

fn color_to_sk(c: Color) -> SkColor {
    SkColor::from_rgba8(c.r, c.g, c.b, c.a)
}

/// Map the first CSS `font-family` to a `cosmic_text::Family`. Generic
/// keywords (`serif`, `sans-serif`, `monospace`, ...) map to the matching
/// generic; everything else is treated as a literal font name (borrowed
/// from the style, hence the lifetime tied to `style`).
fn family_from_style(style: &ComputedStyle) -> Family<'_> {
    if let Some(first) = style.font_family.first() {
        return match first.to_ascii_lowercase().as_str() {
            "serif" => Family::Serif,
            "sans-serif" | "sansserif" | "system-ui" => Family::SansSerif,
            "monospace" => Family::Monospace,
            "cursive" => Family::Cursive,
            "fantasy" => Family::Fantasy,
            _ => Family::Name(first),
        };
    }
    Family::Serif
}

// Silence unused-imports warnings for items referenced only inside the
// `paint_linear_gradient` function via the gradient builder, which the
// compiler currently can't see at the use site.
#[allow(dead_code)]
fn _refs() {
    let _ = Shader::SolidColor;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css;
    use crate::html;
    use crate::layout;
    use crate::layout::ImageCache;

    fn render(html_src: &str, w: u32, h: u32) -> Pixmap {
        let dom = html::parse(html_src);
        let sheets = match css::discover_stylesheets(&dom).into_iter().next() {
            Some(css::StylesheetRef::Embedded(s)) => vec![s],
            _ => vec![],
        };
        let styles = css::style_dom(&dom, &sheets);
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: w as f32,
            height: h as f32,
        };
        let images = ImageCache::new();
        let tree = layout::layout(&dom, &styles, &images, viewport);
        paint(&dom, &styles, &tree, &images, w, h).expect("pixmap")
    }

    #[test]
    fn background_fills_pixmap_with_color() {
        let pixmap = render(
            "<style>body { background-color: rgb(0, 128, 255); margin: 0; height: 50px; }</style>\
             <body></body>",
            10,
            10,
        );
        let data = pixmap.data();
        assert_eq!(data[0], 0);
        assert_eq!(data[1], 128);
        assert_eq!(data[2], 255);
        assert_eq!(data[3], 255);
    }

    #[test]
    fn page_is_white_by_default() {
        let pixmap = render("<body></body>", 4, 4);
        let data = pixmap.data();
        assert_eq!(data[0], 255);
        assert_eq!(data[1], 255);
        assert_eq!(data[2], 255);
    }

    #[test]
    fn text_writes_non_white_pixels() {
        let pixmap = render(
            "<style>body { margin: 0; } p { color: black; }</style>\
             <p>hello</p>",
            200,
            100,
        );
        let data = pixmap.data();
        let any_non_white = (0..data.len() / 4).any(|i| {
            let r = data[i * 4];
            let g = data[i * 4 + 1];
            let b = data[i * 4 + 2];
            r < 250 && g < 250 && b < 250
        });
        assert!(any_non_white);
    }

    #[test]
    fn opacity_dims_colors() {
        // 50% opacity on a fully red box over white produces "pink": red
        // channel stays 255 (red and white both have R=255) but green and
        // blue drop to ~128 because red has 0 in those channels.
        let pixmap = render(
            "<style>body { margin: 0; } \
             .x { background: rgb(255,0,0); opacity: 0.5; height: 20px; }</style>\
             <div class=x></div>",
            10,
            10,
        );
        let data = pixmap.data();
        let idx = (5 * 10 + 5) * 4;
        let g = data[idx + 1];
        assert!(
            g > 50 && g < 200,
            "opacity blending produced g = {g} (expected pink-ish)"
        );
    }
}
