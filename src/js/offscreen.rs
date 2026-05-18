//! `OffscreenCanvas` + `ImageBitmap` + `createImageBitmap`.
//!
//! OffscreenCanvas allocates its own [`tiny_skia::Pixmap`] in a
//! parallel registry to the on-screen canvases. JS calls
//! `getContext("2d")` to get a context that targets the offscreen
//! pixmap; `transferToImageBitmap()` snapshots the pixels into an
//! `ImageBitmap` (and resets the canvas). `convertToBlob(...)` PNG-
//! encodes the snapshot for transport.
//!
//! `createImageBitmap(source)` is the canonical entrypoint for
//! decoding arbitrary image inputs (Blob / ArrayBuffer / Uint8Array
//! / ImageData) into a pre-decoded `ImageBitmap` ready for fast blit
//! into a canvas via `ctx.drawImage(bitmap, ...)`.
//!
//! For the toy:
//!   * No webgl / webgpu / bitmaprenderer contexts on OffscreenCanvas
//!     yet — only "2d" is wired. The other ids return `null`.
//!   * No structured-clone transfer; passing an OffscreenCanvas
//!     into a Worker copies its bytes via the existing message
//!     pump (which currently round-trips through JSON, so transient
//!     state may not survive).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::{builtins::JsPromise, ObjectInitializer},
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use tiny_skia::Pixmap;

const OFFSCREEN_ID_KEY: &str = "__offscreen_id";
const BITMAP_ID_KEY: &str = "__image_bitmap_id";

pub struct OffscreenSurface {
    pub width: u32,
    pub height: u32,
    pub pixmap: Pixmap,
}

pub struct BitmapEntry {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Combined byte cap covering both `OffscreenCanvas` pixmaps and
/// decoded `ImageBitmap`s. JS calls `close()` to free explicitly,
/// but many libraries forget; this floor stops orphan bitmaps from
/// growing without bound. 128 MiB is enough for ~32 4K RGBA
/// buffers — way past what any single page will actually use.
pub const OFFSCREEN_TOTAL_BYTE_CAP: usize = 128 * 1024 * 1024;

thread_local! {
    pub(crate) static OFFSCREEN_CANVASES: RefCell<HashMap<u32, OffscreenSurface>> =
        RefCell::new(HashMap::new());
    pub(crate) static OFFSCREEN_ORDER: RefCell<Vec<u32>> = RefCell::new(Vec::new());
    pub(crate) static IMAGE_BITMAPS: RefCell<HashMap<u32, BitmapEntry>> =
        RefCell::new(HashMap::new());
    pub(crate) static BITMAP_ORDER: RefCell<Vec<u32>> = RefCell::new(Vec::new());
    pub(crate) static OFFSCREEN_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn total_offscreen_bytes() -> usize {
    let canvas_bytes = OFFSCREEN_CANVASES.with(|r| {
        r.borrow()
            .values()
            .map(|s| (s.width as usize) * (s.height as usize) * 4)
            .sum::<usize>()
    });
    let bitmap_bytes =
        IMAGE_BITMAPS.with(|r| r.borrow().values().map(|b| b.rgba.len()).sum::<usize>());
    canvas_bytes + bitmap_bytes
}

/// Evict oldest entries across both registries until total bytes
/// drop under the cap. Called after each insertion that grows the
/// pool. Prefers evicting the older registry-by-registry; in
/// practice canvases and bitmaps live similarly long.
fn evict_until_under_cap() {
    while total_offscreen_bytes() > OFFSCREEN_TOTAL_BYTE_CAP {
        let evicted_offscreen = OFFSCREEN_ORDER.with(|o| {
            let mut order = o.borrow_mut();
            if order.is_empty() {
                None
            } else {
                Some(order.remove(0))
            }
        });
        if let Some(id) = evicted_offscreen {
            OFFSCREEN_CANVASES.with(|r| {
                r.borrow_mut().remove(&id);
            });
            continue;
        }
        let evicted_bitmap = BITMAP_ORDER.with(|o| {
            let mut order = o.borrow_mut();
            if order.is_empty() {
                None
            } else {
                Some(order.remove(0))
            }
        });
        if let Some(id) = evicted_bitmap {
            IMAGE_BITMAPS.with(|r| {
                r.borrow_mut().remove(&id);
            });
            continue;
        }
        break;
    }
}

fn next_id() -> u32 {
    OFFSCREEN_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    ctx.register_global_callable(
        js_string!("OffscreenCanvas"),
        2,
        NativeFunction::from_fn_ptr(offscreen_ctor),
    )
    .ok();
    ctx.register_global_callable(
        js_string!("createImageBitmap"),
        1,
        NativeFunction::from_fn_ptr(create_image_bitmap),
    )
    .ok();
    // `ImageBitmap` constructor isn't part of the spec; pages call
    // `createImageBitmap`. We still register it as a no-op for
    // feature-detection paths that probe `typeof ImageBitmap`.
    ctx.register_global_callable(
        js_string!("ImageBitmap"),
        0,
        NativeFunction::from_fn_ptr(image_bitmap_ctor),
    )
    .ok();
}

// ============ OffscreenCanvas ============

fn offscreen_ctor(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let width = args
        .first()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0)
        .max(1);
    let height = args
        .get(1)
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0)
        .max(1);
    let id = next_id();
    let pixmap = Pixmap::new(width, height).ok_or_else(|| {
        boa_engine::JsNativeError::error().with_message("OffscreenCanvas: pixmap alloc failed")
    })?;
    OFFSCREEN_CANVASES.with(|r| {
        r.borrow_mut().insert(
            id,
            OffscreenSurface {
                width,
                height,
                pixmap,
            },
        );
    });
    OFFSCREEN_ORDER.with(|o| o.borrow_mut().push(id));
    evict_until_under_cap();
    Ok(build_offscreen_object(ctx, id))
}

fn build_offscreen_object(ctx: &mut Context, id: u32) -> JsValue {
    let (w, h) = OFFSCREEN_CANVASES
        .with(|r| r.borrow().get(&id).map(|s| (s.width, s.height)))
        .unwrap_or((1, 1));
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(OFFSCREEN_ID_KEY),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(js_string!("width"), JsValue::from(w), Attribute::all());
    b.property(js_string!("height"), JsValue::from(h), Attribute::all());
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("getContext", NativeFunction::from_fn_ptr(offscreen_get_context), 1),
        (
            "transferToImageBitmap",
            NativeFunction::from_fn_ptr(offscreen_transfer_to_image_bitmap),
            0,
        ),
        ("convertToBlob", NativeFunction::from_fn_ptr(offscreen_convert_to_blob), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn offscreen_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(OFFSCREEN_ID_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn offscreen_get_context(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = offscreen_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let ty = args
        .first()
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ty != "2d" {
        return Ok(JsValue::null());
    }
    // Build a tiny 2D context bound to this offscreen pixmap.
    Ok(build_offscreen_context(ctx, id))
}

const CTX_OFFSCREEN_ID: &str = "__offscreen_ctx_id";

fn build_offscreen_context(ctx: &mut Context, id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(CTX_OFFSCREEN_ID),
        JsValue::from(id),
        Attribute::READONLY,
    );
    b.property(
        js_string!("fillStyle"),
        JsValue::from(js_string!("#000000")),
        Attribute::all(),
    );
    b.property(
        js_string!("strokeStyle"),
        JsValue::from(js_string!("#000000")),
        Attribute::all(),
    );
    b.property(
        js_string!("lineWidth"),
        JsValue::from(1.0_f64),
        Attribute::all(),
    );
    b.property(
        js_string!("globalAlpha"),
        JsValue::from(1.0_f64),
        Attribute::all(),
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_fill_rect),
        js_string!("fillRect"),
        4,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_clear_rect),
        js_string!("clearRect"),
        4,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_draw_image),
        js_string!("drawImage"),
        9,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_get_image_data),
        js_string!("getImageData"),
        4,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_put_image_data),
        js_string!("putImageData"),
        7,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_noop),
        js_string!("save"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_noop),
        js_string!("restore"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_noop),
        js_string!("beginPath"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(off_ctx_noop),
        js_string!("closePath"),
        0,
    );
    JsValue::from(b.build())
}

fn off_ctx_noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn off_ctx_id(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_OFFSCREEN_ID), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn off_ctx_fill_rect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = off_ctx_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let x = args.first().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let w = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let h = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    // Read fillStyle off the context.
    let fill = this
        .as_object()
        .and_then(|o| o.get(js_string!("fillStyle"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "#000000".to_string());
    let alpha = this
        .as_object()
        .and_then(|o| o.get(js_string!("globalAlpha"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(1.0) as f32;
    let color = parse_color(&fill).unwrap_or((0, 0, 0, 255));
    OFFSCREEN_CANVASES.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(surface) = map.get_mut(&id) {
            fill_rect(surface, x, y, w, h, color, alpha);
        }
    });
    Ok(JsValue::undefined())
}

fn off_ctx_clear_rect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = off_ctx_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let x = args.first().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let w = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let h = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    OFFSCREEN_CANVASES.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(surface) = map.get_mut(&id) {
            fill_rect(surface, x, y, w, h, (0, 0, 0, 0), 1.0);
        }
    });
    Ok(JsValue::undefined())
}

fn off_ctx_draw_image(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = off_ctx_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let src = args.first().cloned().unwrap_or(JsValue::undefined());
    // Resolve the source: ImageBitmap or OffscreenCanvas.
    let src_bytes = if let Some(bm_id) = src
        .as_object()
        .and_then(|o| o.get(js_string!(BITMAP_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    {
        IMAGE_BITMAPS.with(|r| r.borrow().get(&bm_id).map(|b| (b.width, b.height, b.rgba.clone())))
    } else if let Some(oc_id) = src
        .as_object()
        .and_then(|o| o.get(js_string!(OFFSCREEN_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    {
        OFFSCREEN_CANVASES.with(|r| {
            r.borrow()
                .get(&oc_id)
                .map(|s| (s.width, s.height, s.pixmap.data().to_vec()))
        })
    } else {
        None
    };
    let Some((sw, sh, src_data)) = src_bytes else {
        return Ok(JsValue::undefined());
    };
    let dx = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let dy = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let dw = args
        .get(3)
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(sw as f64) as i32;
    let dh = args
        .get(4)
        .and_then(|v| v.to_number(ctx).ok())
        .unwrap_or(sh as f64) as i32;
    OFFSCREEN_CANVASES.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(surface) = map.get_mut(&id) {
            blit_scaled(surface, &src_data, sw, sh, dx, dy, dw, dh);
        }
    });
    Ok(JsValue::undefined())
}

fn off_ctx_get_image_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = off_ctx_id(this, ctx) else {
        return Ok(JsValue::null());
    };
    let x = args.first().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let y = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let w = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let h = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let bytes = OFFSCREEN_CANVASES.with(|r| {
        let map = r.borrow();
        let surface = map.get(&id)?;
        let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for j in 0..h {
            for i in 0..w {
                let sx = x + i;
                let sy = y + j;
                if sx < 0 || sy < 0 || sx >= surface.width as i32 || sy >= surface.height as i32 {
                    out.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let idx =
                    ((sy as u32 * surface.width + sx as u32) * 4) as usize;
                let data = surface.pixmap.data();
                if idx + 4 > data.len() {
                    out.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                // Pixmap is premultiplied; ImageData spec returns straight RGBA.
                let r = data[idx];
                let g = data[idx + 1];
                let b = data[idx + 2];
                let a = data[idx + 3];
                let (rr, gg, bb) = if a == 0 {
                    (0, 0, 0)
                } else {
                    (
                        ((r as u16 * 255 + (a as u16 / 2)) / a as u16) as u8,
                        ((g as u16 * 255 + (a as u16 / 2)) / a as u16) as u8,
                        ((b as u16 * 255 + (a as u16 / 2)) / a as u16) as u8,
                    )
                };
                out.extend_from_slice(&[rr, gg, bb, a]);
            }
        }
        Some(out)
    });
    let bytes = bytes.unwrap_or_default();
    let arr = boa_engine::object::builtins::JsUint8Array::from_iter(
        bytes.iter().copied(),
        ctx,
    )?;
    let data_obj = ObjectInitializer::new(ctx)
        .property(
            js_string!("width"),
            JsValue::from(w.max(0) as u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("height"),
            JsValue::from(h.max(0) as u32),
            Attribute::READONLY,
        )
        .property(js_string!("data"), JsValue::from(arr), Attribute::READONLY)
        .build();
    Ok(JsValue::from(data_obj))
}

fn off_ctx_put_image_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = off_ctx_id(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let img_data = args.first().cloned().unwrap_or(JsValue::undefined());
    let dx = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let dy = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as i32;
    let Some(obj) = img_data.as_object() else {
        return Ok(JsValue::undefined());
    };
    let w = obj
        .get(js_string!("width"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let h = obj
        .get(js_string!("height"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let data = obj
        .get(js_string!("data"), ctx)
        .ok()
        .map(|v| read_bytes(&v, ctx))
        .unwrap_or_default();
    OFFSCREEN_CANVASES.with(|r| {
        let mut map = r.borrow_mut();
        if let Some(surface) = map.get_mut(&id) {
            blit_straight_rgba(surface, &data, w as i32, h as i32, dx, dy);
        }
    });
    Ok(JsValue::undefined())
}

fn offscreen_transfer_to_image_bitmap(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(id) = offscreen_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    // Take the pixmap bytes + dims, reset the source canvas to transparent.
    let (w, h, rgba) = OFFSCREEN_CANVASES.with(|r| {
        let mut map = r.borrow_mut();
        let Some(surface) = map.get_mut(&id) else {
            return (0u32, 0u32, Vec::new());
        };
        let w = surface.width;
        let h = surface.height;
        let bytes = surface.pixmap.data().to_vec();
        // Reset by re-allocating the pixmap.
        if let Some(blank) = Pixmap::new(w.max(1), h.max(1)) {
            surface.pixmap = blank;
        }
        (w, h, bytes)
    });
    if w == 0 || h == 0 {
        return Ok(JsValue::null());
    }
    let bitmap_id = store_image_bitmap(w, h, unpremultiply_rgba(&rgba));
    Ok(build_image_bitmap_object(ctx, bitmap_id))
}

fn offscreen_convert_to_blob(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(id) = offscreen_id_of(this, ctx) else {
        return Ok(JsPromise::resolve(JsValue::null(), ctx).into());
    };
    let bytes = OFFSCREEN_CANVASES.with(|r| {
        let map = r.borrow();
        let surface = map.get(&id)?;
        let mut buf = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut buf, surface.width, surface.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            let straight = unpremultiply_rgba(surface.pixmap.data());
            writer.write_image_data(&straight).ok()?;
        }
        Some(buf)
    });
    let bytes = bytes.unwrap_or_default();
    let blob_id = super::file::store_blob(bytes.clone(), "image/png".to_string());
    let blob = ObjectInitializer::new(ctx)
        .property(
            js_string!("__blob_id"),
            JsValue::from(blob_id),
            Attribute::READONLY,
        )
        .property(
            js_string!("size"),
            JsValue::from(bytes.len() as u32),
            Attribute::READONLY,
        )
        .property(
            js_string!("type"),
            JsValue::from(js_string!("image/png")),
            Attribute::READONLY,
        )
        .build();
    Ok(JsPromise::resolve(JsValue::from(blob), ctx).into())
}

// ============ ImageBitmap ============

fn image_bitmap_ctor(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Err(boa_engine::JsNativeError::typ()
        .with_message("Use createImageBitmap() to create an ImageBitmap")
        .into())
}

pub fn store_image_bitmap(width: u32, height: u32, rgba: Vec<u8>) -> u32 {
    let id = next_id();
    IMAGE_BITMAPS.with(|r| {
        r.borrow_mut().insert(
            id,
            BitmapEntry {
                width,
                height,
                rgba,
            },
        );
    });
    BITMAP_ORDER.with(|o| o.borrow_mut().push(id));
    evict_until_under_cap();
    id
}

fn build_image_bitmap_object(ctx: &mut Context, id: u32) -> JsValue {
    let (w, h) = IMAGE_BITMAPS
        .with(|r| r.borrow().get(&id).map(|b| (b.width, b.height)))
        .unwrap_or((0, 0));
    ObjectInitializer::new(ctx)
        .property(js_string!(BITMAP_ID_KEY), JsValue::from(id), Attribute::READONLY)
        .property(js_string!("width"), JsValue::from(w), Attribute::READONLY)
        .property(js_string!("height"), JsValue::from(h), Attribute::READONLY)
        .function(
            NativeFunction::from_fn_ptr(image_bitmap_close),
            js_string!("close"),
            0,
        )
        .build()
        .into()
}

fn image_bitmap_close(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(obj) = this.as_object() {
        if let Ok(v) = obj.get(js_string!(BITMAP_ID_KEY), ctx) {
            if let Ok(id) = v.to_u32(ctx) {
                IMAGE_BITMAPS.with(|r| {
                    r.borrow_mut().remove(&id);
                });
                BITMAP_ORDER.with(|o| o.borrow_mut().retain(|x| *x != id));
            }
        }
    }
    Ok(JsValue::undefined())
}

// ============ createImageBitmap ============

fn create_image_bitmap(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(source) = args.first() else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "createImageBitmap: missing source"
            ))),
            ctx,
        )
        .into());
    };
    // 1) ImageBitmap → clone.
    if let Some(bm_id) = source
        .as_object()
        .and_then(|o| o.get(js_string!(BITMAP_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    {
        let cloned = IMAGE_BITMAPS.with(|r| {
            r.borrow().get(&bm_id).map(|b| BitmapEntry {
                width: b.width,
                height: b.height,
                rgba: b.rgba.clone(),
            })
        });
        if let Some(b) = cloned {
            let id = next_id();
            IMAGE_BITMAPS.with(|r| {
                r.borrow_mut().insert(id, b);
            });
            return Ok(JsPromise::resolve(build_image_bitmap_object(ctx, id), ctx).into());
        }
    }
    // 2) OffscreenCanvas → snapshot bytes.
    if let Some(oc_id) = source
        .as_object()
        .and_then(|o| o.get(js_string!(OFFSCREEN_ID_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    {
        let snap = OFFSCREEN_CANVASES.with(|r| {
            r.borrow().get(&oc_id).map(|s| {
                (
                    s.width,
                    s.height,
                    unpremultiply_rgba(s.pixmap.data()),
                )
            })
        });
        if let Some((w, h, rgba)) = snap {
            let id = store_image_bitmap(w, h, rgba);
            return Ok(JsPromise::resolve(build_image_bitmap_object(ctx, id), ctx).into());
        }
    }
    // 3) Blob / ArrayBuffer / Uint8Array → decode via `image` crate.
    let bytes = if let Some(blob_id) = source
        .as_object()
        .and_then(|o| o.get(js_string!("__blob_id"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
    {
        super::file::read_blob_bytes(blob_id)
    } else {
        Some(read_bytes(source, ctx))
    };
    let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
        return Ok(JsPromise::reject(
            boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                "createImageBitmap: unsupported source"
            ))),
            ctx,
        )
        .into());
    };
    let decoded = match image::load_from_memory(&bytes) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(format!(
                    "createImageBitmap decode: {e}"
                )))),
                ctx,
            )
            .into());
        }
    };
    let w = decoded.width();
    let h = decoded.height();
    let id = store_image_bitmap(w, h, decoded.into_raw());
    Ok(JsPromise::resolve(build_image_bitmap_object(ctx, id), ctx).into())
}

// ============ pixmap helpers ============

fn parse_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix('#') {
        let bytes: Vec<u8> = stripped
            .as_bytes()
            .iter()
            .copied()
            .collect();
        let hex_pair = |i: usize| -> Option<u8> {
            let lo = std::str::from_utf8(&bytes[i..i + 2]).ok()?;
            u8::from_str_radix(lo, 16).ok()
        };
        match bytes.len() {
            6 => Some((hex_pair(0)?, hex_pair(2)?, hex_pair(4)?, 255)),
            8 => Some((hex_pair(0)?, hex_pair(2)?, hex_pair(4)?, hex_pair(6)?)),
            3 => {
                let dup = |c: u8| -> Option<u8> {
                    let s = format!("{}{}", c as char, c as char);
                    u8::from_str_radix(&s, 16).ok()
                };
                Some((dup(bytes[0])?, dup(bytes[1])?, dup(bytes[2])?, 255))
            }
            _ => None,
        }
    } else if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
    {
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() >= 3 {
            let r = parts[0].parse::<u8>().ok()?;
            let g = parts[1].parse::<u8>().ok()?;
            let b = parts[2].parse::<u8>().ok()?;
            let a = parts.get(3).and_then(|p| {
                p.parse::<f32>().ok().map(|f| (f.clamp(0.0, 1.0) * 255.0) as u8)
            }).unwrap_or(255);
            Some((r, g, b, a))
        } else {
            None
        }
    } else {
        match s {
            "black" => Some((0, 0, 0, 255)),
            "white" => Some((255, 255, 255, 255)),
            "red" => Some((255, 0, 0, 255)),
            "green" => Some((0, 128, 0, 255)),
            "blue" => Some((0, 0, 255, 255)),
            "transparent" => Some((0, 0, 0, 0)),
            _ => None,
        }
    }
}

fn fill_rect(
    surface: &mut OffscreenSurface,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: (u8, u8, u8, u8),
    alpha: f32,
) {
    let pw = surface.width as i32;
    let ph = surface.height as i32;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(pw);
    let y1 = (y + h).min(ph);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let a = ((color.3 as f32 * alpha.clamp(0.0, 1.0)) as u8).max(0);
    let r = ((color.0 as u16 * a as u16) / 255) as u8;
    let g = ((color.1 as u16 * a as u16) / 255) as u8;
    let b = ((color.2 as u16 * a as u16) / 255) as u8;
    let data = surface.pixmap.data_mut();
    let w = surface.width as i32;
    for j in y0..y1 {
        for i in x0..x1 {
            let idx = ((j * w + i) * 4) as usize;
            data[idx] = r;
            data[idx + 1] = g;
            data[idx + 2] = b;
            data[idx + 3] = a;
        }
    }
}

fn blit_scaled(
    surface: &mut OffscreenSurface,
    src: &[u8],
    sw: u32,
    sh: u32,
    dx: i32,
    dy: i32,
    dw: i32,
    dh: i32,
) {
    if sw == 0 || sh == 0 || dw <= 0 || dh <= 0 {
        return;
    }
    let pw = surface.width as i32;
    let ph = surface.height as i32;
    let data = surface.pixmap.data_mut();
    for j in 0..dh {
        let dy_pix = dy + j;
        if dy_pix < 0 || dy_pix >= ph {
            continue;
        }
        let sy = ((j as f32 / dh as f32) * sh as f32) as u32;
        for i in 0..dw {
            let dx_pix = dx + i;
            if dx_pix < 0 || dx_pix >= pw {
                continue;
            }
            let sx = ((i as f32 / dw as f32) * sw as f32) as u32;
            let src_idx = ((sy * sw + sx) * 4) as usize;
            if src_idx + 4 > src.len() {
                continue;
            }
            let r = src[src_idx];
            let g = src[src_idx + 1];
            let b = src[src_idx + 2];
            let a = src[src_idx + 3];
            // Source is straight RGBA (ImageBitmap convention); the
            // destination is premultiplied.
            let dst_idx = ((dy_pix * pw + dx_pix) * 4) as usize;
            data[dst_idx] = ((r as u16 * a as u16) / 255) as u8;
            data[dst_idx + 1] = ((g as u16 * a as u16) / 255) as u8;
            data[dst_idx + 2] = ((b as u16 * a as u16) / 255) as u8;
            data[dst_idx + 3] = a;
        }
    }
}

fn blit_straight_rgba(
    surface: &mut OffscreenSurface,
    src: &[u8],
    sw: i32,
    sh: i32,
    dx: i32,
    dy: i32,
) {
    let pw = surface.width as i32;
    let ph = surface.height as i32;
    let data = surface.pixmap.data_mut();
    for j in 0..sh {
        let dy_pix = dy + j;
        if dy_pix < 0 || dy_pix >= ph {
            continue;
        }
        for i in 0..sw {
            let dx_pix = dx + i;
            if dx_pix < 0 || dx_pix >= pw {
                continue;
            }
            let src_idx = ((j * sw + i) * 4) as usize;
            if src_idx + 4 > src.len() {
                continue;
            }
            let r = src[src_idx];
            let g = src[src_idx + 1];
            let b = src[src_idx + 2];
            let a = src[src_idx + 3];
            let dst_idx = ((dy_pix * pw + dx_pix) * 4) as usize;
            data[dst_idx] = ((r as u16 * a as u16) / 255) as u8;
            data[dst_idx + 1] = ((g as u16 * a as u16) / 255) as u8;
            data[dst_idx + 2] = ((b as u16 * a as u16) / 255) as u8;
            data[dst_idx + 3] = a;
        }
    }
}

fn unpremultiply_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    for px in src.chunks_exact(4) {
        let r = px[0];
        let g = px[1];
        let b = px[2];
        let a = px[3];
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            out.push(((r as u16 * 255 + (a as u16 / 2)) / a as u16) as u8);
            out.push(((g as u16 * 255 + (a as u16 / 2)) / a as u16) as u8);
            out.push(((b as u16 * 255 + (a as u16 / 2)) / a as u16) as u8);
            out.push(a);
        }
    }
    out
}

fn read_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    use boa_engine::object::builtins::{JsArrayBuffer, JsUint8Array};
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    if let Ok(u8a) = JsUint8Array::from_object(obj.clone()) {
        let len = u8a.length(ctx).unwrap_or(0);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = u8a.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    if let Ok(ab) = JsArrayBuffer::from_object(obj.clone()) {
        let len = ab.byte_length();
        let view = match JsUint8Array::from_array_buffer(ab, ctx) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(v) = view.at(i as i64, ctx) {
                if let Ok(n) = v.to_u32(ctx) {
                    out.push(n as u8);
                }
            }
        }
        return out;
    }
    Vec::new()
}

// Suppress unused if Rc isn't used elsewhere.
#[allow(dead_code)]
fn _keep_rc() -> Rc<()> {
    Rc::new(())
}
