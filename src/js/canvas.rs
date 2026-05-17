//! Canvas 2D rendering surface (toy subset).
//!
//! Supports `fillStyle`, `strokeStyle`, `lineWidth`, `fillRect`,
//! `strokeRect`, `clearRect`, `beginPath`/`moveTo`/`lineTo`/`closePath`,
//! `fill`/`stroke`, plus `save`/`restore` for the color + lineWidth
//! state stack.
//!
//! Skipped on purpose:
//!  * `arc`, `arcTo`, `bezierCurveTo` — path commands beyond straight lines.
//!  * `drawImage`, `getImageData`, `putImageData`.
//!  * `fillText` / `strokeText` — text rendering goes through cosmic-text
//!    everywhere else and wiring that into a canvas context is its own pass.
//!  * Patterns and gradients as `fillStyle`. Only color strings work.
//!  * `globalAlpha`, blend modes, transforms beyond identity.
//!
//! Rendering: each `<canvas>` element gets its own `tiny_skia::Pixmap`
//! the first time JS calls `getContext('2d')`. The paint layer
//! composites the pixmap onto the page at the canvas's box rect.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string, object::FunctionObjectBuilder, object::ObjectInitializer, property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::dom::NodeId;

/// One canvas's pixmap plus a recording of the current path. Real
/// browsers also track transforms, line dash, miter limit, etc; we
/// only need enough to draw rects and line segments.
pub struct CanvasSurface {
    pub pixmap: Pixmap,
    pub path: PathBuilder,
}

impl CanvasSurface {
    fn new(width: u32, height: u32) -> Option<Self> {
        let pixmap = Pixmap::new(width.max(1), height.max(1))?;
        Some(Self {
            pixmap,
            path: PathBuilder::new(),
        })
    }
}

pub type CanvasSurfaces = Rc<RefCell<HashMap<NodeId, CanvasSurface>>>;

thread_local! {
    pub(crate) static JS_CANVAS_SURFACES: RefCell<Option<CanvasSurfaces>> =
        const { RefCell::new(None) };
}

const CTX_NODE_KEY: &str = "__canvas_node";

pub fn get_or_create_context(
    ctx: &mut Context,
    node: NodeId,
    width: u32,
    height: u32,
) -> JsValue {
    JS_CANVAS_SURFACES.with(|slot| {
        let Some(rc) = slot.borrow().as_ref().cloned() else {
            return JsValue::null();
        };
        {
            let mut map = rc.borrow_mut();
            if !map.contains_key(&node) {
                let Some(surface) = CanvasSurface::new(width, height) else {
                    return JsValue::null();
                };
                map.insert(node, surface);
            }
        }
        let realm = ctx.realm().clone();
        let getter = |f: fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>| {
            FunctionObjectBuilder::new(&realm, NativeFunction::from_fn_ptr(f)).build()
        };
        let mut b = ObjectInitializer::new(ctx);
        b.property(
            js_string!(CTX_NODE_KEY),
            JsValue::from(node.index() as u32),
            Attribute::READONLY,
        );
        // Style state as plain properties — read/written by the
        // drawing fns each call.
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
        // Static fields the spec defines.
        b.property(
            js_string!("globalAlpha"),
            JsValue::from(1.0_f64),
            Attribute::all(),
        );

        for (name, fr) in [
            ("fillRect", ctx_fill_rect as fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>),
            ("strokeRect", ctx_stroke_rect),
            ("clearRect", ctx_clear_rect),
            ("beginPath", ctx_begin_path),
            ("closePath", ctx_close_path),
            ("moveTo", ctx_move_to),
            ("lineTo", ctx_line_to),
            ("fill", ctx_fill),
            ("stroke", ctx_stroke),
            ("save", ctx_save),
            ("restore", ctx_restore),
        ] {
            let _ = getter;
            b.function(NativeFunction::from_fn_ptr(fr), js_string!(name), 0);
        }
        JsValue::from(b.build())
    })
}

fn surface_for(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_NODE_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

fn parse_color(s: &str) -> Option<tiny_skia::Color> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('#') {
        return parse_hex(rest);
    }
    if let Some(inner) = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))
    {
        let inner = inner.trim_end_matches(')');
        return parse_rgb_args(inner);
    }
    parse_named(s)
}

fn parse_hex(rest: &str) -> Option<tiny_skia::Color> {
    match rest.len() {
        3 => {
            let r = u8::from_str_radix(&rest[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&rest[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&rest[2..3].repeat(2), 16).ok()?;
            Some(tiny_skia::Color::from_rgba8(r, g, b, 255))
        }
        6 => {
            let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
            let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
            let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
            Some(tiny_skia::Color::from_rgba8(r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&rest[0..2], 16).ok()?;
            let g = u8::from_str_radix(&rest[2..4], 16).ok()?;
            let b = u8::from_str_radix(&rest[4..6], 16).ok()?;
            let a = u8::from_str_radix(&rest[6..8], 16).ok()?;
            Some(tiny_skia::Color::from_rgba8(r, g, b, a))
        }
        _ => None,
    }
}

fn parse_rgb_args(s: &str) -> Option<tiny_skia::Color> {
    let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
    if parts.len() < 3 {
        return None;
    }
    let r = parts[0].parse::<f32>().ok()?.clamp(0.0, 255.0) as u8;
    let g = parts[1].parse::<f32>().ok()?.clamp(0.0, 255.0) as u8;
    let b = parts[2].parse::<f32>().ok()?.clamp(0.0, 255.0) as u8;
    let a = if parts.len() >= 4 {
        (parts[3].parse::<f32>().ok()?.clamp(0.0, 1.0) * 255.0) as u8
    } else {
        255
    };
    Some(tiny_skia::Color::from_rgba8(r, g, b, a))
}

fn parse_named(s: &str) -> Option<tiny_skia::Color> {
    Some(match s.to_ascii_lowercase().as_str() {
        "black" => tiny_skia::Color::from_rgba8(0, 0, 0, 255),
        "white" => tiny_skia::Color::from_rgba8(255, 255, 255, 255),
        "red" => tiny_skia::Color::from_rgba8(255, 0, 0, 255),
        "green" => tiny_skia::Color::from_rgba8(0, 128, 0, 255),
        "blue" => tiny_skia::Color::from_rgba8(0, 0, 255, 255),
        "yellow" => tiny_skia::Color::from_rgba8(255, 255, 0, 255),
        "cyan" => tiny_skia::Color::from_rgba8(0, 255, 255, 255),
        "magenta" => tiny_skia::Color::from_rgba8(255, 0, 255, 255),
        "gray" | "grey" => tiny_skia::Color::from_rgba8(128, 128, 128, 255),
        "transparent" => tiny_skia::Color::from_rgba8(0, 0, 0, 0),
        _ => return None,
    })
}

fn read_fill_color(this: &JsValue, ctx: &mut Context) -> tiny_skia::Color {
    let s = this
        .as_object()
        .and_then(|o| o.get(js_string!("fillStyle"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "#000".to_string());
    parse_color(&s).unwrap_or_else(|| tiny_skia::Color::from_rgba8(0, 0, 0, 255))
}

fn read_stroke_color(this: &JsValue, ctx: &mut Context) -> tiny_skia::Color {
    let s = this
        .as_object()
        .and_then(|o| o.get(js_string!("strokeStyle"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "#000".to_string());
    parse_color(&s).unwrap_or_else(|| tiny_skia::Color::from_rgba8(0, 0, 0, 255))
}

fn read_line_width(this: &JsValue, ctx: &mut Context) -> f32 {
    this.as_object()
        .and_then(|o| o.get(js_string!("lineWidth"), ctx).ok())
        .and_then(|v| v.to_number(ctx).ok())
        .map(|n| n as f32)
        .unwrap_or(1.0)
        .max(0.0)
}

fn read_args_4(args: &[JsValue], ctx: &mut Context) -> Option<(f32, f32, f32, f32)> {
    let x = args.first()?.to_number(ctx).ok()? as f32;
    let y = args.get(1)?.to_number(ctx).ok()? as f32;
    let w = args.get(2)?.to_number(ctx).ok()? as f32;
    let h = args.get(3)?.to_number(ctx).ok()? as f32;
    Some((x, y, w, h))
}

fn read_args_2(args: &[JsValue], ctx: &mut Context) -> Option<(f32, f32)> {
    let x = args.first()?.to_number(ctx).ok()? as f32;
    let y = args.get(1)?.to_number(ctx).ok()? as f32;
    Some((x, y))
}

fn with_surface<R>(node: NodeId, f: impl FnOnce(&mut CanvasSurface) -> R) -> Option<R> {
    JS_CANVAS_SURFACES.with(|slot| {
        let rc = slot.borrow().as_ref().cloned()?;
        let mut map = rc.borrow_mut();
        let surface = map.get_mut(&node)?;
        Some(f(surface))
    })
}

fn ctx_fill_rect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some((x, y, w, h)) = read_args_4(args, ctx) else {
        return Ok(JsValue::undefined());
    };
    let color = read_fill_color(this, ctx);
    with_surface(node, |s| {
        let mut paint = Paint::default();
        paint.set_color(color);
        if let Some(rect) = Rect::from_xywh(x, y, w.max(0.0), h.max(0.0)) {
            s.pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        }
    });
    Ok(JsValue::undefined())
}

fn ctx_stroke_rect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some((x, y, w, h)) = read_args_4(args, ctx) else {
        return Ok(JsValue::undefined());
    };
    let color = read_stroke_color(this, ctx);
    let lw = read_line_width(this, ctx);
    with_surface(node, |s| {
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            let mut stroke = Stroke::default();
            stroke.width = lw;
            s.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    });
    Ok(JsValue::undefined())
}

fn ctx_clear_rect(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some((x, y, w, h)) = read_args_4(args, ctx) else {
        return Ok(JsValue::undefined());
    };
    with_surface(node, |s| {
        // No spec-correct "clear to transparent" via fill_rect (that
        // overpaints). Manually zero the pixels.
        let pw = s.pixmap.width() as i32;
        let ph = s.pixmap.height() as i32;
        let x0 = x.floor().max(0.0) as i32;
        let y0 = y.floor().max(0.0) as i32;
        let x1 = (x + w).ceil().min(pw as f32) as i32;
        let y1 = (y + h).ceil().min(ph as f32) as i32;
        let stride = pw as usize * 4;
        let data = s.pixmap.data_mut();
        for row in y0.max(0)..y1.max(0) {
            let off = row as usize * stride;
            for col in x0.max(0)..x1.max(0) {
                let i = off + col as usize * 4;
                if i + 4 <= data.len() {
                    data[i] = 0;
                    data[i + 1] = 0;
                    data[i + 2] = 0;
                    data[i + 3] = 0;
                }
            }
        }
    });
    Ok(JsValue::undefined())
}

fn ctx_begin_path(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    with_surface(node, |s| {
        s.path = PathBuilder::new();
    });
    Ok(JsValue::undefined())
}

fn ctx_close_path(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    with_surface(node, |s| {
        s.path.close();
    });
    Ok(JsValue::undefined())
}

fn ctx_move_to(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some((x, y)) = read_args_2(args, ctx) else {
        return Ok(JsValue::undefined());
    };
    with_surface(node, |s| s.path.move_to(x, y));
    Ok(JsValue::undefined())
}

fn ctx_line_to(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some((x, y)) = read_args_2(args, ctx) else {
        return Ok(JsValue::undefined());
    };
    with_surface(node, |s| s.path.line_to(x, y));
    Ok(JsValue::undefined())
}

fn ctx_fill(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let color = read_fill_color(this, ctx);
    with_surface(node, |s| {
        // PathBuilder::finish consumes; clone via take + recreate.
        let pb = std::mem::take(&mut s.path);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            s.pixmap
                .fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    });
    Ok(JsValue::undefined())
}

fn ctx_stroke(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = surface_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let color = read_stroke_color(this, ctx);
    let lw = read_line_width(this, ctx);
    with_surface(node, |s| {
        let pb = std::mem::take(&mut s.path);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.set_color(color);
            let mut stroke = Stroke::default();
            stroke.width = lw;
            s.pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    });
    Ok(JsValue::undefined())
}

fn ctx_save(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // We store the stack as a JS array on the context object.
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    use boa_engine::object::builtins::JsArray;
    let stack = obj
        .get(js_string!("__state_stack"), ctx)
        .ok()
        .and_then(|v| v.as_object().and_then(|o| JsArray::from_object(o.clone()).ok()))
        .unwrap_or_else(|| {
            let a = JsArray::new(ctx);
            let _ = obj.set(
                js_string!("__state_stack"),
                JsValue::from(a.clone()),
                false,
                ctx,
            );
            a
        });
    let fs = obj
        .get(js_string!("fillStyle"), ctx)
        .unwrap_or(JsValue::undefined());
    let ss = obj
        .get(js_string!("strokeStyle"), ctx)
        .unwrap_or(JsValue::undefined());
    let lw = obj
        .get(js_string!("lineWidth"), ctx)
        .unwrap_or(JsValue::undefined());
    let entry = ObjectInitializer::new(ctx)
        .property(js_string!("fillStyle"), fs, Attribute::all())
        .property(js_string!("strokeStyle"), ss, Attribute::all())
        .property(js_string!("lineWidth"), lw, Attribute::all())
        .build();
    let _ = stack.push(JsValue::from(entry), ctx);
    Ok(JsValue::undefined())
}

fn ctx_restore(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use boa_engine::object::builtins::JsArray;
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(stack_val) = obj.get(js_string!("__state_stack"), ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(stack_obj) = stack_val.as_object() else {
        return Ok(JsValue::undefined());
    };
    let Ok(stack) = JsArray::from_object(stack_obj.clone()) else {
        return Ok(JsValue::undefined());
    };
    if stack.length(ctx).unwrap_or(0) == 0 {
        return Ok(JsValue::undefined());
    }
    let Ok(top) = stack.pop(ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(top_obj) = top.as_object() else {
        return Ok(JsValue::undefined());
    };
    for key in ["fillStyle", "strokeStyle", "lineWidth"] {
        if let Ok(v) = top_obj.get(js_string!(key), ctx) {
            let _ = obj.set(js_string!(key), v, false, ctx);
        }
    }
    Ok(JsValue::undefined())
}
