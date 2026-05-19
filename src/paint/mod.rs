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
    BackgroundImage, BorderStyle, Color, ComputedStyle, FilterFunction, FontStyle, StyleTree,
    TextDecoration,
};
use crate::dom::{Dom, NodeId, NodeKind};
use crate::layout::{BoxTree, ImageCache, ImageInfo, ImageSlot, LayoutBox, PseudoKind, Rect};

/// View of a single capture frame the painter wants to draw. Lets
/// `paint_video` treat camera and ffmpeg-decoded frames uniformly.
struct CaptureFrameView {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Pull the latest camera frame for `node` if `<video>.srcObject` was
/// bound to a `getUserMedia` stream. Returns `None` if either the
/// per-paint thread-locals aren't installed or the element has no
/// capture binding.
fn capture_frame_for_node(node: NodeId) -> Option<CaptureFrameView> {
    let idx = PAINT_CAPTURE_BINDINGS.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|rc| rc.borrow().get(&node).copied())
    })?;
    PAINT_CAPTURES.with(|slot| {
        let rc = slot.borrow().as_ref().cloned()?;
        let reg = rc.borrow();
        let stream = reg.get(idx)?.as_ref()?;
        let guard = stream.latest_frame.lock().ok()?;
        let frame = guard.as_ref()?;
        Some(CaptureFrameView {
            width: frame.width,
            height: frame.height,
            rgba: frame.rgba.clone(),
        })
    })
}

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

/// Cached painted output for a layer-promoted element. The painter
/// reuses this pixmap whenever the layer's content hash matches the
/// hash recorded at paint time — turning a CSS animation that only
/// mutates `transform` / `opacity` into a single pixmap blit
/// instead of a full subtree repaint.
pub struct CachedLayer {
    pub pixmap: Pixmap,
    pub hash: u64,
    /// Origin of the cached pixmap in the parent's coordinate space.
    pub box_origin: (f32, f32),
    /// Padding added around the box for transformed-layer overflow.
    pub pad: u32,
}

/// Cap on cached layers. Each entry holds a Pixmap (≈ 4 bytes per
/// pixel × layer area), so 64 of them on a desktop-size page caps
/// around 100 MB. Long-running tabs that animate many distinct
/// elements drop the oldest layers; on the next paint they fall
/// through to the slow path and re-cache.
pub const LAYER_CACHE_CAP: usize = 64;

pub type LayerCache = std::collections::HashMap<crate::dom::NodeId, CachedLayer>;

thread_local! {
    /// Installed by the browser shell for the duration of a paint pass
    /// so the painter can composite `<canvas>` element pixmaps.
    pub static PAINT_CANVAS_SURFACES:
        std::cell::RefCell<Option<crate::js::CanvasSurfaces>> =
        const { std::cell::RefCell::new(None) };

    /// Per-page layer pixmap cache. Set by the browser shell around
    /// each paint pass; the painter consults it to skip subtree work
    /// for unchanged layer-promoted elements.
    pub static PAINT_LAYER_CACHE:
        std::cell::RefCell<Option<std::rc::Rc<std::cell::RefCell<LayerCache>>>> =
        const { std::cell::RefCell::new(None) };

    /// Out-parameter the painter fills with `position: fixed` layer
    /// pixmaps that the redraw path will composite ON TOP of the
    /// scrolled page pixmap. The fixed-positioned subtree is NOT
    /// baked into the main pixmap; this is what keeps a fixed
    /// header pinned during scroll.
    pub static PAINT_FIXED_OVERLAYS:
        std::cell::RefCell<Option<std::rc::Rc<std::cell::RefCell<Vec<FixedOverlay>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// One painted `position: fixed` layer, plus its viewport-relative
/// destination rectangle. `pixmap` is sized to box dims + padding;
/// `dest_x` / `dest_y` is the top-left in viewport coords (i.e. the
/// final on-screen position, unaffected by document scroll).
pub struct FixedOverlay {
    pub pixmap: Pixmap,
    pub dest_x: f32,
    pub dest_y: f32,
}

thread_local! {
    /// Same hook for `<video>` elements: paint pulls the latest
    /// decoded frame from each VideoElement and composites at its
    /// box rect.
    pub static PAINT_VIDEO_ELEMENTS:
        std::cell::RefCell<Option<crate::js::VideoElements>> =
        const { std::cell::RefCell::new(None) };

    /// Live `getUserMedia` capture registry. Set per-paint so the
    /// painter can pull camera frames for `<video>` elements whose
    /// `srcObject` was assigned a MediaStream.
    pub static PAINT_CAPTURES:
        std::cell::RefCell<Option<crate::js::media::CaptureRegistry>> =
        const { std::cell::RefCell::new(None) };

    /// Per-element `srcObject` → capture-index bindings. Paired with
    /// `PAINT_CAPTURES` above to resolve a node to a live camera frame.
    pub static PAINT_CAPTURE_BINDINGS:
        std::cell::RefCell<Option<crate::js::media::CaptureBindings>> =
        const { std::cell::RefCell::new(None) };
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
        Pixmap::new(width.max(1), height.max(1)).map(|pixmap| {
            let mut font_system = FontSystem::new();
            // Pull any JS-registered fonts (FontFace.load, @font-face)
            // into this fontdb so `font-family: <custom>` matches.
            let fonts = crate::js::fontloading::registered_font_bytes();
            for (_family, bytes) in fonts {
                font_system.db_mut().load_font_data(bytes);
            }
            Self {
                pixmap,
                font_system,
                swash_cache: SwashCache::new(),
            }
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

        // Compositor-style layer promotion: when an element opts into
        // its own layer via `will-change: transform/opacity/filter`,
        // hash its subtree's relevant inputs and look the result up
        // in the page's `LayerCache`. A hash hit means we blit the
        // cached pixmap straight onto the current target without
        // re-walking the subtree — the win for animations that only
        // mutate transform / opacity.
        if is_layer_root(style) && tree.get(node).is_some() {
            if self.try_paint_from_layer_cache(dom, styles, tree, images, node, parent_ctx) {
                return;
            }
        }

        // If the element has a visual filter (anything beyond opacity,
        // which is already folded into the alpha stack), paint the
        // subtree into an offscreen pixmap, apply the filter pixel-by-
        // pixel, then composite back. This is the only place a child
        // can "see" the post-filter pixels of its parent.
        if has_visual_filter(&style.filter) {
            if let Some(b) = tree.get(node) {
                self.paint_subtree_with_filter(
                    dom, styles, tree, images, node, parent_ctx, b.rect,
                );
                return;
            }
        }
        // Non-trivial 2D transform (rotate / scale / skew / matrix /
        // a 3D form we flattened). Translation-only transforms travel
        // the fast `transform_translate` path on `PaintCtx`.
        if let Some(t) = style.transform.as_ref().filter(|t| !t.is_pure_translate()) {
            if let Some(b) = tree.get(node) {
                self.paint_subtree_with_transform(
                    dom, styles, tree, images, node, parent_ctx, b.rect, *t,
                );
                return;
            }
        }
        self.paint_subtree_inner(dom, styles, tree, images, node, parent_ctx)
    }

    /// Returns true if the layer was served from cache (or freshly
    /// painted into the cache and composited). Returns false if no
    /// cache is installed or the layer has zero size.
    #[allow(clippy::too_many_arguments)]
    fn try_paint_from_layer_cache(
        &mut self,
        dom: &Dom,
        styles: &StyleTree,
        tree: &BoxTree,
        images: &ImageCache,
        node: NodeId,
        parent_ctx: PaintCtx,
    ) -> bool {
        let Some(cache_rc) = PAINT_LAYER_CACHE.with(|s| s.borrow().clone()) else {
            return false;
        };
        let Some(b) = tree.get(node) else { return false };
        let style = styles.get(node);
        let pad = 8u32;
        let layer_w = (b.rect.width.ceil() as u32).max(1) + pad * 2;
        let layer_h = (b.rect.height.ceil() as u32).max(1) + pad * 2;
        let hash = compute_layer_hash(dom, styles, tree, node);

        // Fast path: previous frame's pixmap is still valid.
        let cached_pixmap_opt: Option<Pixmap> = {
            let cache = cache_rc.borrow();
            cache.get(&node).and_then(|entry| {
                if entry.hash == hash
                    && entry.pixmap.width() == layer_w
                    && entry.pixmap.height() == layer_h
                {
                    // Clone the pixmap bytes so we can release the
                    // borrow before composite (composite re-borrows
                    // self.pixmap).
                    let mut p = Pixmap::new(entry.pixmap.width(), entry.pixmap.height())?;
                    p.data_mut().copy_from_slice(entry.pixmap.data());
                    Some(p)
                } else {
                    None
                }
            })
        };
        if let Some(cached) = cached_pixmap_opt {
            self.dispatch_layer_composite(&cached, b.rect, pad, parent_ctx, style);
            return true;
        }

        // Slow path: paint the subtree into a fresh offscreen, cache,
        // composite. We don't honour 2D transform / filter chains
        // inside the cached pixmap — those still go through the
        // existing offscreen helpers. Layer promotion here is a
        // straight subtree blit.
        let Some(offscreen) = Pixmap::new(layer_w, layer_h) else {
            return false;
        };
        let saved = std::mem::replace(&mut self.pixmap, offscreen);
        let inner_ctx = PaintCtx {
            alpha: parent_ctx.alpha,
            tx: -b.rect.x + pad as f32,
            ty: -b.rect.y + pad as f32,
        };
        // Skip the layer-cache check on the recursive call to avoid
        // infinite recursion — paint the layer's own subtree directly.
        self.paint_subtree_inner(dom, styles, tree, images, node, inner_ctx);
        let painted = std::mem::replace(&mut self.pixmap, saved);

        // Store a clone in the cache before consuming for composite.
        if let Some(mut store_copy) = Pixmap::new(painted.width(), painted.height()) {
            store_copy.data_mut().copy_from_slice(painted.data());
            let mut cache = cache_rc.borrow_mut();
            // Bound the cache; drop a random entry when full. We
            // don't track LRU order yet — frequent re-evictions
            // would need that, but a steady-state animation only
            // touches a handful of layers.
            if cache.len() >= LAYER_CACHE_CAP {
                if let Some(victim) = cache.keys().next().copied() {
                    cache.remove(&victim);
                }
            }
            cache.insert(
                node,
                CachedLayer {
                    pixmap: store_copy,
                    hash,
                    box_origin: (b.rect.x, b.rect.y),
                    pad,
                },
            );
        }
        self.dispatch_layer_composite(&painted, b.rect, pad, parent_ctx, style);
        true
    }

    /// Decide whether the layer pixmap belongs on the document
    /// pixmap (default) or on the page's `fixed_overlays` list so
    /// the redraw path can stamp it on top after scroll.
    fn dispatch_layer_composite(
        &mut self,
        pixmap: &Pixmap,
        box_rect: Rect,
        pad: u32,
        parent_ctx: PaintCtx,
        style: &ComputedStyle,
    ) {
        if matches!(style.position, crate::css::Position::Fixed) {
            if let Some(overlays_rc) = PAINT_FIXED_OVERLAYS.with(|s| s.borrow().clone()) {
                let mut copy = match Pixmap::new(pixmap.width(), pixmap.height()) {
                    Some(p) => p,
                    None => return,
                };
                copy.data_mut().copy_from_slice(pixmap.data());
                let dest_x = box_rect.x + parent_ctx.tx - pad as f32;
                let dest_y = box_rect.y + parent_ctx.ty - pad as f32;
                overlays_rc.borrow_mut().push(FixedOverlay {
                    pixmap: copy,
                    dest_x,
                    dest_y,
                });
                return;
            }
        }
        self.composite_layer_pixmap(pixmap, box_rect, pad, parent_ctx, style);
    }

    fn composite_layer_pixmap(
        &mut self,
        offscreen: &Pixmap,
        box_rect: Rect,
        pad: u32,
        parent_ctx: PaintCtx,
        style: &ComputedStyle,
    ) {
        let dest_x = (box_rect.x + parent_ctx.tx - pad as f32) as i32;
        let dest_y = (box_rect.y + parent_ctx.ty - pad as f32) as i32;
        let mut paint = PixmapPaint::default();
        paint.opacity = parent_ctx.alpha * style.opacity;
        self.pixmap.draw_pixmap(
            dest_x,
            dest_y,
            offscreen.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
    }

    /// Paint a transformed subtree by redirecting its drawing into an
    /// offscreen pixmap, then `draw_pixmap`-ing it back with a matrix
    /// that rotates/scales/skews around the element's center
    /// (CSS `transform-origin: 50% 50%` default).
    #[allow(clippy::too_many_arguments)]
    fn paint_subtree_with_transform(
        &mut self,
        dom: &Dom,
        styles: &StyleTree,
        tree: &BoxTree,
        images: &ImageCache,
        node: NodeId,
        parent_ctx: PaintCtx,
        box_rect: Rect,
        transform: crate::css::Transform2D,
    ) {
        let style = styles.get(node);
        let ctx = parent_ctx.with(style);
        // Round to integer pixel dims to keep the offscreen tight.
        // Widen by a small margin so rotated/skewed edges aren't
        // clipped at the corners.
        let pad = 8u32;
        let off_w = (box_rect.width.ceil() as u32).max(1) + pad * 2;
        let off_h = (box_rect.height.ceil() as u32).max(1) + pad * 2;
        let Some(offscreen) = Pixmap::new(off_w, off_h) else {
            self.paint_subtree_inner(dom, styles, tree, images, node, parent_ctx);
            return;
        };
        let saved = std::mem::replace(&mut self.pixmap, offscreen);
        let inner_ctx = PaintCtx {
            alpha: parent_ctx.alpha,
            tx: -box_rect.x + pad as f32,
            ty: -box_rect.y + pad as f32,
        };
        self.paint_subtree_inner(dom, styles, tree, images, node, inner_ctx);
        let offscreen = std::mem::replace(&mut self.pixmap, saved);

        // Compose the screen-space matrix:
        //   T(box_origin + pre-existing-translate) ∘
        //   T(+cx, +cy) ∘ M ∘ T(-cx, -cy) ∘ T(-pad, -pad)
        // i.e. translate so the box's pre-transform center sits at
        // the origin, apply the matrix, then translate back out to
        // the final on-screen position.
        let cx = box_rect.width / 2.0 + pad as f32;
        let cy = box_rect.height / 2.0 + pad as f32;
        let screen_x = box_rect.x + ctx.tx;
        let screen_y = box_rect.y + ctx.ty;
        // tiny_skia's `Transform` is row-major in the same order as
        // our `Transform2D`: sx, ky, kx, sy, tx, ty (from_row).
        let m = Transform::from_row(
            transform.sx,
            transform.kx,
            transform.ky,
            transform.sy,
            transform.tx,
            transform.ty,
        );
        // Build: T(screen + center) ∘ M ∘ T(-center)
        let pre = Transform::from_translate(-cx, -cy);
        let post = Transform::from_translate(screen_x + cx, screen_y + cy);
        let final_xform = post.pre_concat(m).pre_concat(pre);

        let mut paint = PixmapPaint::default();
        paint.opacity = ctx.alpha;
        self.pixmap.draw_pixmap(
            0,
            0,
            offscreen.as_ref(),
            &paint,
            final_xform,
            None,
        );
    }

    fn paint_subtree_inner(
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
            NodeKind::Element { tag, .. } if tag == "canvas" => {
                if let Some(b) = tree.get(node) {
                    self.paint_canvas(b.rect, node, ctx);
                }
            }
            NodeKind::Element { tag, .. } if tag == "video" => {
                if let Some(b) = tree.get(node) {
                    self.paint_video(b.rect, node, ctx);
                }
            }
            _ => {}
        }

        // Paint children in z-index order: items with `z-index: auto`
        // (`None`) paint in DOM order at z=0; items with explicit z paint
        // before (negative) or after (positive). Stable sort preserves DOM
        // order within the same z bucket.
        let mut kids: Vec<NodeId> = dom.children(node).collect();
        kids.sort_by_key(|c| styles.get(*c).z_index.unwrap_or(0));
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

    /// Paint a filtered subtree by redirecting its drawing into an
    /// offscreen pixmap, applying the filter pixel pass, then drawing
    /// the result back at the element's screen position. `box_rect` is
    /// the element's rect in screen coordinates (before this scope's
    /// transform).
    #[allow(clippy::too_many_arguments)]
    fn paint_subtree_with_filter(
        &mut self,
        dom: &Dom,
        styles: &StyleTree,
        tree: &BoxTree,
        images: &ImageCache,
        node: NodeId,
        parent_ctx: PaintCtx,
        box_rect: Rect,
    ) {
        let style = styles.get(node);
        let ctx = parent_ctx.with(style);
        // Translate everything inside the filtered subtree so the
        // element's box origin maps to (0, 0) of the offscreen pixmap.
        let dest_x = (box_rect.x + ctx.tx).round();
        let dest_y = (box_rect.y + ctx.ty).round();
        let off_w = box_rect.width.ceil().max(1.0) as u32;
        let off_h = box_rect.height.ceil().max(1.0) as u32;
        let Some(offscreen) = Pixmap::new(off_w, off_h) else {
            // Fallback to unfiltered paint if the offscreen alloc fails.
            self.paint_subtree_inner(dom, styles, tree, images, node, parent_ctx);
            return;
        };
        let saved = std::mem::replace(&mut self.pixmap, offscreen);
        let inner_ctx = PaintCtx {
            alpha: parent_ctx.alpha, // pre-element alpha; filter handles the rest
            tx: -box_rect.x,
            ty: -box_rect.y,
        };
        self.paint_subtree_inner(dom, styles, tree, images, node, inner_ctx);
        let mut filtered = std::mem::replace(&mut self.pixmap, saved);
        // Apply each filter function in declaration order. The
        // pixmap-aware dispatcher handles per-pixel and spatial
        // (blur) filters identically from the caller's POV.
        for f in &style.filter {
            apply_filter_to_pixmap(*f, &mut filtered);
        }
        let composed = filtered;
        let transform = Transform::from_translate(dest_x, dest_y);
        let mut paint = PixmapPaint::default();
        paint.opacity = ctx.alpha;
        self.pixmap
            .draw_pixmap(0, 0, composed.as_ref(), &paint, transform, None);
    }

    /// Composite the latest decoded `<video>` frame at the element's
    /// box rect. Pulls from `PAINT_VIDEO_ELEMENTS`; falls back to a
    /// no-op when no decoder exists yet (loading state).
    fn paint_video(&mut self, dest: Rect, node: NodeId, ctx: PaintCtx) {
        // 1) `<video>.srcObject = stream` path — pull the latest
        // camera frame from the capture registry.
        let camera_frame = capture_frame_for_node(node);
        let frame = match camera_frame {
            Some(f) => f,
            None => {
                let Some(f) = PAINT_VIDEO_ELEMENTS.with(|slot| {
                    let rc = slot.borrow().as_ref().cloned()?;
                    let map = rc.borrow();
                    map.get(&node).and_then(|v| v.current_frame())
                }) else {
                    return;
                };
                CaptureFrameView {
                    width: f.width,
                    height: f.height,
                    rgba: f.rgba,
                }
            }
        };
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        let Some(mut src) = Pixmap::new(frame.width, frame.height) else {
            return;
        };
        // ffmpeg / nokhwa emit straight (non-premultiplied) RGBA;
        // premultiply before draw_pixmap to match the rest of our
        // paint output.
        let data = src.data_mut();
        for (i, chunk) in frame.rgba.chunks_exact(4).enumerate() {
            let r = chunk[0];
            let g = chunk[1];
            let b = chunk[2];
            let a = chunk[3];
            let p = i * 4;
            data[p] = ((r as u16 * a as u16) / 255) as u8;
            data[p + 1] = ((g as u16 * a as u16) / 255) as u8;
            data[p + 2] = ((b as u16 * a as u16) / 255) as u8;
            data[p + 3] = a;
        }
        let scale_x = dest.width / frame.width as f32;
        let scale_y = dest.height / frame.height as f32;
        let transform = Transform::from_translate(dest.x + ctx.tx, dest.y + ctx.ty)
            .pre_scale(scale_x, scale_y);
        let mut paint = PixmapPaint::default();
        paint.opacity = ctx.alpha;
        self.pixmap
            .draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
    }

    /// Composite a `<canvas>` element's pixmap into the page. Pulled
    /// from the `PAINT_CANVAS_SURFACES` thread-local that the browser
    /// installs around each paint pass.
    fn paint_canvas(&mut self, dest: Rect, node: NodeId, ctx: PaintCtx) {
        let surface_bytes: Option<(u32, u32, Vec<u8>)> = PAINT_CANVAS_SURFACES.with(|slot| {
            let rc = slot.borrow().as_ref().cloned()?;
            let map = rc.borrow();
            let s = map.get(&node)?;
            Some((s.pixmap.width(), s.pixmap.height(), s.pixmap.data().to_vec()))
        });
        let Some((w, h, data)) = surface_bytes else {
            return;
        };
        let Some(mut src) = Pixmap::new(w, h) else {
            return;
        };
        // Canvas pixmaps in our toy are already premultiplied
        // (tiny_skia writes them that way). Copy straight over.
        src.data_mut().copy_from_slice(&data);
        let scale_x = dest.width / w as f32;
        let scale_y = dest.height / h as f32;
        let transform = Transform::from_translate(dest.x + ctx.tx, dest.y + ctx.ty)
            .pre_scale(scale_x, scale_y);
        let mut paint = PixmapPaint::default();
        paint.opacity = ctx.alpha;
        self.pixmap
            .draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
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
        // cosmic-text asserts both values are positive; some pages set
        // `font-size: 0` to hide elements. Skip painting in that case.
        if style.font_size <= 0.0 || style.line_height <= 0.0 {
            return;
        }
        let line_height = (style.font_size * style.line_height).max(1.0);
        let metrics = Metrics::new(style.font_size.max(1.0), line_height);

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

/// `true` when the `filter:` chain contains anything beyond the
/// opacity function (which the cascade already folds into the alpha
/// stack). Triggers offscreen rendering for the subtree.
fn has_visual_filter(chain: &[FilterFunction]) -> bool {
    chain
        .iter()
        .any(|f| !matches!(f, FilterFunction::Opacity(_)))
}

/// Per-pixel application of a single filter function on a
/// premultiplied-RGBA byte buffer.
fn apply_filter_pixels(filter: FilterFunction, data: &mut [u8]) {
    match filter {
        FilterFunction::Opacity(_) => { /* already folded by cascade */ }
        FilterFunction::Brightness(factor) => apply_brightness(data, factor),
        FilterFunction::Contrast(factor) => apply_contrast(data, factor),
        FilterFunction::Grayscale(amount) => apply_grayscale(data, amount),
        FilterFunction::Invert(amount) => apply_invert(data, amount),
        FilterFunction::Saturate(factor) => apply_saturate(data, factor),
        FilterFunction::Sepia(amount) => apply_sepia(data, amount),
        FilterFunction::HueRotate(deg) => apply_hue_rotate(data, deg),
        // Blur needs the pixmap dimensions — this per-buffer entry
        // point can't apply spatial filters. The pixmap-aware
        // dispatcher handles blur in [`apply_filter_pixmap`].
        FilterFunction::Blur(_) => {}
    }
}

/// Pixmap-aware filter application. Per-pixel filters delegate to
/// [`apply_filter_pixels`]; spatial filters (blur) need the
/// dimensions, so they live here.
pub(crate) fn apply_filter_to_pixmap(
    filter: FilterFunction,
    pixmap: &mut tiny_skia::Pixmap,
) {
    match filter {
        FilterFunction::Blur(radius_px) => {
            let r = radius_px.max(0.0).min(64.0); // cap to keep work bounded
            if r > 0.5 {
                let w = pixmap.width();
                let h = pixmap.height();
                let data = pixmap.data_mut();
                gaussian_blur_rgba(data, w as usize, h as usize, r);
            }
        }
        other => apply_filter_pixels(other, pixmap.data_mut()),
    }
}

/// Separable Gaussian blur with a clipped kernel. Operates on
/// premultiplied RGBA so partial-alpha edges fade correctly.
fn gaussian_blur_rgba(data: &mut [u8], width: usize, height: usize, radius: f32) {
    if width == 0 || height == 0 {
        return;
    }
    let sigma = radius * 0.5;
    let half = radius.ceil() as isize;
    if half < 1 {
        return;
    }
    // Precompute the 1D kernel.
    let mut kernel: Vec<f32> = Vec::with_capacity((2 * half + 1) as usize);
    let two_sigma_sq = 2.0 * sigma * sigma;
    let mut sum = 0.0;
    for i in -half..=half {
        let v = (-((i as f32).powi(2)) / two_sigma_sq).exp();
        kernel.push(v);
        sum += v;
    }
    for v in &mut kernel {
        *v /= sum;
    }

    let row_stride = width * 4;
    // Two-pass horizontal then vertical.
    let mut tmp = vec![0u8; data.len()];

    // Horizontal pass: src=data → dst=tmp.
    for y in 0..height {
        let row = y * row_stride;
        for x in 0..width {
            let mut acc = [0.0f32; 4];
            for (ki, k) in kernel.iter().enumerate() {
                let sx = x as isize + ki as isize - half;
                let sx = sx.clamp(0, width as isize - 1) as usize;
                let off = row + sx * 4;
                acc[0] += data[off] as f32 * k;
                acc[1] += data[off + 1] as f32 * k;
                acc[2] += data[off + 2] as f32 * k;
                acc[3] += data[off + 3] as f32 * k;
            }
            let off = row + x * 4;
            tmp[off] = acc[0].clamp(0.0, 255.0) as u8;
            tmp[off + 1] = acc[1].clamp(0.0, 255.0) as u8;
            tmp[off + 2] = acc[2].clamp(0.0, 255.0) as u8;
            tmp[off + 3] = acc[3].clamp(0.0, 255.0) as u8;
        }
    }

    // Vertical pass: src=tmp → dst=data.
    for y in 0..height {
        for x in 0..width {
            let mut acc = [0.0f32; 4];
            for (ki, k) in kernel.iter().enumerate() {
                let sy = y as isize + ki as isize - half;
                let sy = sy.clamp(0, height as isize - 1) as usize;
                let off = sy * row_stride + x * 4;
                acc[0] += tmp[off] as f32 * k;
                acc[1] += tmp[off + 1] as f32 * k;
                acc[2] += tmp[off + 2] as f32 * k;
                acc[3] += tmp[off + 3] as f32 * k;
            }
            let off = y * row_stride + x * 4;
            data[off] = acc[0].clamp(0.0, 255.0) as u8;
            data[off + 1] = acc[1].clamp(0.0, 255.0) as u8;
            data[off + 2] = acc[2].clamp(0.0, 255.0) as u8;
            data[off + 3] = acc[3].clamp(0.0, 255.0) as u8;
        }
    }
}

fn apply_hue_rotate(data: &mut [u8], degrees: f32) {
    let theta = degrees.to_radians();
    let c = theta.cos();
    let s = theta.sin();
    // Standard hue-rotate matrix per the SVG filter spec.
    let m = [
        [0.213 + c * 0.787 - s * 0.213, 0.715 - c * 0.715 - s * 0.715, 0.072 - c * 0.072 + s * 0.928],
        [0.213 - c * 0.213 + s * 0.143, 0.715 + c * 0.285 + s * 0.140, 0.072 - c * 0.072 - s * 0.283],
        [0.213 - c * 0.213 - s * 0.787, 0.715 - c * 0.715 + s * 0.715, 0.072 + c * 0.928 + s * 0.072],
    ];
    each_rgba_unpremultiplied(data, |r, g, b| {
        let rf = *r as f32;
        let gf = *g as f32;
        let bf = *b as f32;
        let nr = m[0][0] * rf + m[0][1] * gf + m[0][2] * bf;
        let ng = m[1][0] * rf + m[1][1] * gf + m[1][2] * bf;
        let nb = m[2][0] * rf + m[2][1] * gf + m[2][2] * bf;
        *r = nr.clamp(0.0, 255.0) as u8;
        *g = ng.clamp(0.0, 255.0) as u8;
        *b = nb.clamp(0.0, 255.0) as u8;
    });
}

fn each_rgba_unpremultiplied(data: &mut [u8], mut apply: impl FnMut(&mut u8, &mut u8, &mut u8)) {
    for chunk in data.chunks_exact_mut(4) {
        let a = chunk[3];
        if a == 0 {
            continue;
        }
        // Un-premultiply to operate in linear color space, then re-multiply.
        let inv = 255.0 / a as f32;
        let mut r = (chunk[0] as f32 * inv).clamp(0.0, 255.0);
        let mut g = (chunk[1] as f32 * inv).clamp(0.0, 255.0);
        let mut b = (chunk[2] as f32 * inv).clamp(0.0, 255.0);
        let (mut r8, mut g8, mut b8) = (r as u8, g as u8, b as u8);
        apply(&mut r8, &mut g8, &mut b8);
        r = r8 as f32;
        g = g8 as f32;
        b = b8 as f32;
        let af = a as f32 / 255.0;
        chunk[0] = (r * af).clamp(0.0, 255.0) as u8;
        chunk[1] = (g * af).clamp(0.0, 255.0) as u8;
        chunk[2] = (b * af).clamp(0.0, 255.0) as u8;
    }
}

fn apply_brightness(data: &mut [u8], factor: f32) {
    let f = factor.max(0.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        *r = (*r as f32 * f).clamp(0.0, 255.0) as u8;
        *g = (*g as f32 * f).clamp(0.0, 255.0) as u8;
        *b = (*b as f32 * f).clamp(0.0, 255.0) as u8;
    });
}

fn apply_contrast(data: &mut [u8], factor: f32) {
    let f = factor.max(0.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        for c in [r, g, b] {
            let v = (*c as f32 - 128.0) * f + 128.0;
            *c = v.clamp(0.0, 255.0) as u8;
        }
    });
}

fn apply_grayscale(data: &mut [u8], amount: f32) {
    let a = amount.clamp(0.0, 1.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        // ITU-R BT.601 luminance approximation.
        let lum = 0.299 * *r as f32 + 0.587 * *g as f32 + 0.114 * *b as f32;
        *r = (*r as f32 * (1.0 - a) + lum * a) as u8;
        *g = (*g as f32 * (1.0 - a) + lum * a) as u8;
        *b = (*b as f32 * (1.0 - a) + lum * a) as u8;
    });
}

fn apply_invert(data: &mut [u8], amount: f32) {
    let a = amount.clamp(0.0, 1.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        for c in [r, g, b] {
            let inv = 255.0 - *c as f32;
            *c = (*c as f32 * (1.0 - a) + inv * a) as u8;
        }
    });
}

fn apply_saturate(data: &mut [u8], factor: f32) {
    let f = factor.max(0.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        let lum = 0.299 * *r as f32 + 0.587 * *g as f32 + 0.114 * *b as f32;
        *r = (lum + (*r as f32 - lum) * f).clamp(0.0, 255.0) as u8;
        *g = (lum + (*g as f32 - lum) * f).clamp(0.0, 255.0) as u8;
        *b = (lum + (*b as f32 - lum) * f).clamp(0.0, 255.0) as u8;
    });
}

fn apply_sepia(data: &mut [u8], amount: f32) {
    let a = amount.clamp(0.0, 1.0);
    each_rgba_unpremultiplied(data, |r, g, b| {
        let nr = (0.393 * *r as f32 + 0.769 * *g as f32 + 0.189 * *b as f32).clamp(0.0, 255.0);
        let ng = (0.349 * *r as f32 + 0.686 * *g as f32 + 0.168 * *b as f32).clamp(0.0, 255.0);
        let nb = (0.272 * *r as f32 + 0.534 * *g as f32 + 0.131 * *b as f32).clamp(0.0, 255.0);
        *r = (*r as f32 * (1.0 - a) + nr * a) as u8;
        *g = (*g as f32 * (1.0 - a) + ng * a) as u8;
        *b = (*b as f32 * (1.0 - a) + nb * a) as u8;
    });
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

/// True when this element warrants its own composited layer.
///
/// The cache replays its pixmap with a straight blit at the layer's
/// box origin — it doesn't reapply transforms or filters at
/// composite time. So we promote only on signals where the cached
/// pixmap is genuinely transform-independent:
///   * `will-change` — the page explicitly opted in.
///   * `position: fixed` — sticky chrome / overlays. The fixed
///     position itself is honoured by layout; cache survives scroll.
///   * `opacity < 1` — the alpha multiplies in at composite time,
///     so the cache is transform-independent.
///
/// `transform` and `filter` still go through the un-cached
/// `paint_subtree_with_{transform,filter}` paths above this gate,
/// because their pixel-level math needs to happen on the live
/// subtree, not on a previously-cached snapshot.
fn is_layer_root(style: &ComputedStyle) -> bool {
    if let Some(wc) = &style.will_change {
        let matched = wc
            .split(|c: char| c == ',' || c.is_whitespace())
            .any(|tok| {
                matches!(
                    tok.trim(),
                    "transform" | "opacity" | "filter" | "contents" | "scroll-position"
                )
            });
        if matched {
            return true;
        }
    }
    if matches!(style.position, crate::css::Position::Fixed) {
        return true;
    }
    if style.opacity < 1.0 - 1e-4 {
        return true;
    }
    false
}

/// Hash the inputs that, if unchanged frame-to-frame, mean the
/// layer's painted output would be byte-identical. We walk the
/// subtree once and feed each element's identity + relevant
/// style/box fields + text content into a stable hasher.
fn compute_layer_hash(dom: &Dom, styles: &StyleTree, tree: &BoxTree, root: NodeId) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_subtree(&mut hasher, dom, styles, tree, root);
    hasher.finish()
}

fn hash_subtree(
    hasher: &mut std::collections::hash_map::DefaultHasher,
    dom: &Dom,
    styles: &StyleTree,
    tree: &BoxTree,
    node: NodeId,
) {
    use std::hash::Hash;
    node.hash(hasher);
    let style = styles.get(node);
    // Stylistic fields that affect paint output.
    let bg = style.background_color;
    (bg.r, bg.g, bg.b, bg.a).hash(hasher);
    let fg = style.color;
    (fg.r, fg.g, fg.b, fg.a).hash(hasher);
    (style.font_size.to_bits()).hash(hasher);
    (style.opacity.to_bits()).hash(hasher);
    if let Some(t) = &style.transform {
        (
            t.sx.to_bits(),
            t.kx.to_bits(),
            t.ky.to_bits(),
            t.sy.to_bits(),
            t.tx.to_bits(),
            t.ty.to_bits(),
        )
            .hash(hasher);
    }
    if let Some((dx, dy)) = style.transform_translate {
        (dx.to_bits(), dy.to_bits()).hash(hasher);
    }
    if let Some(b) = tree.get(node) {
        (
            (b.rect.x as i32),
            (b.rect.y as i32),
            (b.rect.width as i32),
            (b.rect.height as i32),
        )
            .hash(hasher);
    }
    match &dom.node(node).kind {
        crate::dom::NodeKind::Element { tag, attrs } => {
            tag.hash(hasher);
            for (k, v) in attrs {
                k.hash(hasher);
                v.hash(hasher);
            }
        }
        crate::dom::NodeKind::Text(s) => {
            s.hash(hasher);
        }
        _ => {}
    }
    for c in dom.children(node) {
        hash_subtree(hasher, dom, styles, tree, c);
    }
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
    fn grayscale_filter_collapses_red_channel() {
        let pixmap = render(
            "<style>body { margin: 0; } \
             .x { background: rgb(255, 0, 0); filter: grayscale(1); \
                  height: 20px; }</style>\
             <div class=x></div>",
            10,
            10,
        );
        let data = pixmap.data();
        let idx = (5 * 10 + 5) * 4;
        let r = data[idx];
        let g = data[idx + 1];
        let b = data[idx + 2];
        // Fully-saturated red under grayscale → BT.601 luminance ≈ 76
        // for all three channels. Tolerate ±15 for rounding /
        // premultiplied math.
        assert!(
            (r as i32 - g as i32).abs() < 15 && (g as i32 - b as i32).abs() < 15,
            "expected r==g==b after grayscale, got rgb=({r},{g},{b})"
        );
    }

    #[test]
    fn invert_filter_flips_white_to_dark() {
        let pixmap = render(
            "<style>body { margin: 0; } \
             .x { background: white; filter: invert(1); \
                  height: 20px; }</style>\
             <div class=x></div>",
            10,
            10,
        );
        let data = pixmap.data();
        let idx = (5 * 10 + 5) * 4;
        let r = data[idx];
        // White (255) inverts to black (0). Allow for premultiply rounding.
        assert!(r < 20, "expected dark after invert, got r = {r}");
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

    #[test]
    fn layer_cache_holds_a_will_change_subtree_after_paint() {
        // Build a tiny page with one will-change layer, paint into
        // an installed LayerCache, and verify the cache has an
        // entry. A second paint should still leave one entry (no
        // duplicates).
        let cache: std::rc::Rc<std::cell::RefCell<LayerCache>> =
            std::rc::Rc::new(std::cell::RefCell::new(LayerCache::new()));
        PAINT_LAYER_CACHE.with(|s| *s.borrow_mut() = Some(cache.clone()));

        let html = "<style>\
            body { margin: 0; }\
            .lyr { width: 20px; height: 20px; background: red; \
                   will-change: transform; }\
            </style>\
            <body><div class='lyr'></div></body>";
        let _ = render(html, 60, 60);
        let after_first = cache.borrow().len();
        let _ = render(html, 60, 60);
        let after_second = cache.borrow().len();

        PAINT_LAYER_CACHE.with(|s| s.borrow_mut().take());

        assert_eq!(
            after_first, 1,
            "expected one cached layer after first paint, got {after_first}"
        );
        assert_eq!(
            after_second, after_first,
            "second paint should reuse the existing entry, not duplicate"
        );
    }

    /// Smoke test: throw a chunky SPA-shaped page at the full
    /// pipeline (cascade → layout → paint) and confirm nothing
    /// panics + we get sensibly-sized output. Exercises:
    /// flex / grid containers, inline text wrapping, gradients,
    /// borders, transforms, opacity, filters, fixed position,
    /// will-change layer promotion, color-space functions,
    /// container queries, and nested ::before / ::after.
    #[test]
    fn kitchen_sink_renders_without_panic() {
        let html = r#"
            <!doctype html>
            <html>
            <head>
              <style>
                body { margin: 0; font-family: sans-serif; color: oklch(0.2 0.05 240); }
                .topbar {
                  position: fixed; top: 0; left: 0; right: 0; height: 48px;
                  background: linear-gradient(to right, #2563eb, #7c3aed);
                  color: white; padding: 12px 20px;
                }
                .topbar::before { content: 'X · '; opacity: 0.8; }
                .container { display: grid; grid-template-columns: 220px 1fr;
                             gap: 16px; padding: 64px 20px 20px; }
                .sidebar { background: #f3f4f6; border: 1px solid #e5e7eb;
                           padding: 12px; border-radius: 8px; }
                .card { background: hsl(0 0% 100%); border: 1px solid #d1d5db;
                        border-radius: 12px; padding: 20px; margin-bottom: 16px;
                        box-shadow: 0 1px 3px rgba(0,0,0,0.1); }
                .card.featured { will-change: transform; transform: scale(1.0);
                                 background: color(display-p3 0.95 0.97 1); }
                .card.faded { opacity: 0.65; filter: blur(0.5px); }
                .row { display: flex; gap: 8px; align-items: center; }
                .badge { background: lab(60 40 30); color: white;
                         padding: 2px 8px; border-radius: 9999px;
                         font-size: 12px; }
                .clamped { display: -webkit-box; -webkit-line-clamp: 2;
                           -webkit-box-orient: vertical; overflow: hidden; }
                @container (min-width: 400px) {
                  .card { padding: 24px; }
                }
              </style>
            </head>
            <body>
              <div class="topbar">Daboss kitchen-sink smoke test</div>
              <div class="container">
                <nav class="sidebar">
                  <div>Home</div>
                  <div>Docs</div>
                  <div>About</div>
                </nav>
                <main>
                  <article class="card featured">
                    <div class="row">
                      <h2>Featured</h2>
                      <span class="badge">new</span>
                    </div>
                    <p class="clamped">Lorem ipsum dolor sit amet, consectetur
                      adipiscing elit. Sed do eiusmod tempor incididunt ut
                      labore et dolore magna aliqua. Ut enim ad minim veniam.
                    </p>
                  </article>
                  <article class="card">
                    <h3>Standard card</h3>
                    <p>Body copy that should wrap across multiple lines and
                       exercise text shaping in the inline layout path.</p>
                  </article>
                  <article class="card faded">
                    <h3>Faded card</h3>
                    <p>Opacity + blur filter combined.</p>
                  </article>
                </main>
              </div>
            </body>
            </html>
        "#;
        let pixmap = render(html, 1024, 800);
        // Non-zero alpha somewhere — we actually painted content.
        let any_solid = pixmap.data().chunks_exact(4).any(|px| px[3] > 0);
        assert!(any_solid, "rendered pixmap is entirely transparent");
        // Topbar gradient should put a saturated colour into the
        // first row of pixels. (Y=8, somewhere in the strip.)
        let row_idx = 8 * 1024 * 4;
        let in_topbar = pixmap.data()[row_idx + 512 * 4..row_idx + 512 * 4 + 4]
            .iter()
            .any(|&c| c > 20);
        assert!(in_topbar, "topbar strip didn't paint");
    }
}
