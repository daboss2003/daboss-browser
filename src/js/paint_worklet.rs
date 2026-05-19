//! CSS Paint Worklets — actually executing.
//!
//! Today's surface (in `web_apis.rs`) exposes
//! `CSS.paintWorklet.addModule()`, but the call is a stub that
//! resolves without running the worklet body. This module wires the
//! rest of the pipeline:
//!
//!  * `registerPaint("name", Class)` — a global the worklet code
//!    calls. We capture the class object in a per-document registry.
//!
//!  * `addModule(url)` — fetches the URL via the existing client and
//!    evaluates the body in the document's JS context (the spec
//!    uses a separate Worklet context for isolation; we collapse
//!    to the main context for the toy). If the URL is a `data:`
//!    JavaScript blob we decode and eval directly.
//!
//!  * `invoke_for(node, name, width, height)` — instantiates the
//!    registered class via `new`, builds a tiny canvas shim that
//!    records draw commands, calls `paint(ctx, geom)`, and stashes
//!    the resulting `Vec<DrawCmd>` in a per-node thread-local. The
//!    painter reads from it during the next paint pass.
//!
//! The canvas shim supports `fillStyle` (CSS colour) and
//! `fillRect(x, y, w, h)` — enough for a working demo. Path ops and
//! gradients are TODOs.

use std::cell::RefCell;
use std::collections::HashMap;

use boa_engine::{
    js_string, object::ObjectInitializer, property::Attribute, Context, JsError, JsObject,
    JsResult, JsValue, NativeFunction,
};

use crate::css::Color;
use crate::dom::NodeId;

#[derive(Debug, Clone)]
pub enum DrawCmd {
    FillRect {
        dx: f32,
        dy: f32,
        dw: f32,
        dh: f32,
        color: Color,
    },
}

/// One registered paint worklet, keyed by the name passed to
/// `registerPaint`. We store the constructor (a JsObject — boa
/// distinguishes ordinary objects from class constructors at call
/// time, but for invocation we treat both uniformly).
#[derive(Debug, Clone)]
pub struct PaintWorkletDef {
    pub class: JsObject,
}

thread_local! {
    /// `name -> definition`. Cleared between page loads via
    /// [`clear`].
    static PAINT_WORKLETS: RefCell<HashMap<String, PaintWorkletDef>> =
        RefCell::new(HashMap::new());

    /// `NodeId -> draw commands recorded for its `paint(name)` bg`.
    /// Populated by [`invoke_for`]; consumed by the painter at
    /// paint time.
    static PAINT_WORKLET_COMMANDS: RefCell<HashMap<NodeId, Vec<DrawCmd>>> =
        RefCell::new(HashMap::new());

    /// In-flight recorder state. The native `fillRect` closure reads
    /// the canvas object's `fillStyle` string property and appends a
    /// `DrawCmd` here. Cleared at the start of every `invoke_for`
    /// call and read back after the worklet returns.
    static ACTIVE_RECORDER: RefCell<Option<Vec<DrawCmd>>> =
        const { RefCell::new(None) };
}

/// Snapshot draw commands recorded for `node`. Returns `None` when
/// the node has no `paint()` background or the worklet failed to
/// execute (in which case the painter falls back to background
/// colour without a panic).
pub fn commands_for(node: NodeId) -> Option<Vec<DrawCmd>> {
    PAINT_WORKLET_COMMANDS.with(|s| s.borrow().get(&node).cloned())
}

/// Clear any recorded commands / registered worklets. Called by the
/// shell on navigation.
pub fn clear_all() {
    PAINT_WORKLETS.with(|s| s.borrow_mut().clear());
    PAINT_WORKLET_COMMANDS.with(|s| s.borrow_mut().clear());
}

/// Seed draw commands directly under `node` — used by tests to
/// exercise the painter's replay path without spinning up a full
/// JS context.
#[cfg(test)]
pub fn seed_commands_for(node: NodeId, cmds: Vec<DrawCmd>) {
    PAINT_WORKLET_COMMANDS.with(|s| {
        s.borrow_mut().insert(node, cmds);
    });
}

/// Clear only the per-node draw commands. Called at the start of
/// each paint pass so stale entries don't survive a re-layout that
/// changed the box rect.
pub fn clear_commands() {
    PAINT_WORKLET_COMMANDS.with(|s| s.borrow_mut().clear());
}

/// Native `registerPaint(name, ClassOrFactory)` implementation. The
/// class object is captured by reference; subsequent
/// `invoke_for(node, name, ...)` calls instantiate it per element.
pub fn register_paint(
    _: &JsValue,
    args: &[JsValue],
    _ctx: &mut Context,
) -> JsResult<JsValue> {
    let name = match args.first() {
        Some(JsValue::String(s)) => s.to_std_string_escaped(),
        _ => return Ok(JsValue::undefined()),
    };
    let class = match args.get(1).and_then(|v| v.as_object().cloned()) {
        Some(o) => o,
        None => return Ok(JsValue::undefined()),
    };
    PAINT_WORKLETS.with(|s| {
        s.borrow_mut().insert(name, PaintWorkletDef { class });
    });
    Ok(JsValue::undefined())
}

/// Look up a worklet definition by name. Mostly for tests; the live
/// invoke path goes through [`invoke_for`].
pub fn lookup(name: &str) -> Option<PaintWorkletDef> {
    PAINT_WORKLETS.with(|s| s.borrow().get(name).cloned())
}

/// Run `name`'s paint worklet for an element with the given box
/// dimensions. Records draw commands in `PAINT_WORKLET_COMMANDS`
/// under `node`. No-op when the worklet isn't registered or the JS
/// call throws — failure modes are silent because a misbehaving
/// worklet shouldn't break rendering.
pub fn invoke_for(
    ctx: &mut Context,
    node: NodeId,
    name: &str,
    width: f32,
    height: f32,
) {
    let def = match lookup(name) {
        Some(d) => d,
        None => return,
    };
    // Install a fresh recorder. Any fillRect call inside the worklet
    // appends to this Vec; we drain it after the call returns.
    ACTIVE_RECORDER.with(|s| *s.borrow_mut() = Some(Vec::new()));

    let recorder = build_recorder_canvas(ctx);
    let geom = match make_geometry(ctx, width, height) {
        Ok(g) => g,
        Err(_) => {
            ACTIVE_RECORDER.with(|s| s.borrow_mut().take());
            return;
        }
    };
    // Try to instantiate the class via `new` first; some authors
    // register a plain function instead, in which case we silently
    // skip — invoking a non-constructor would throw.
    let instance = match def
        .class
        .construct(&[], Some(&def.class), ctx)
    {
        Ok(o) => o,
        Err(_) => {
            ACTIVE_RECORDER.with(|s| s.borrow_mut().take());
            return;
        }
    };
    // Look up `paint` on the instance.
    let paint_fn = match instance.get(js_string!("paint"), ctx) {
        Ok(v) => v,
        Err(_) => {
            ACTIVE_RECORDER.with(|s| s.borrow_mut().take());
            return;
        }
    };
    let Some(paint_obj) = paint_fn.as_callable() else {
        ACTIVE_RECORDER.with(|s| s.borrow_mut().take());
        return;
    };
    let _: Result<JsValue, JsError> = paint_obj.call(
        &JsValue::from(instance),
        &[JsValue::from(recorder), JsValue::from(geom)],
        ctx,
    );
    let cmds = ACTIVE_RECORDER
        .with(|s| s.borrow_mut().take())
        .unwrap_or_default();
    PAINT_WORKLET_COMMANDS.with(|s| {
        s.borrow_mut().insert(node, cmds);
    });
}

/// Build the canvas-like recorder object. `fillRect` is a plain
/// fn-pointer native that reads the canvas's mutable `fillStyle`
/// string property + appends a `DrawCmd` into the thread-local
/// `ACTIVE_RECORDER`. Using a fn-pointer avoids `from_copy_closure`
/// constraints — no shared state captured in the closure.
fn build_recorder_canvas(ctx: &mut Context) -> JsObject {
    ObjectInitializer::new(ctx)
        .property(
            js_string!("fillStyle"),
            JsValue::from(js_string!("rgb(0, 0, 0)")),
            Attribute::WRITABLE | Attribute::ENUMERABLE | Attribute::CONFIGURABLE,
        )
        .function(
            NativeFunction::from_fn_ptr(fill_rect_native),
            js_string!("fillRect"),
            4,
        )
        .build()
}

fn fill_rect_native(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let color = match this.as_object() {
        Some(obj) => match obj.get(js_string!("fillStyle"), ctx) {
            Ok(JsValue::String(s)) => parse_css_color(&s.to_std_string_escaped())
                .unwrap_or(Color::rgb(0, 0, 0)),
            _ => Color::rgb(0, 0, 0),
        },
        None => Color::rgb(0, 0, 0),
    };
    let dx = num_arg(args, 0);
    let dy = num_arg(args, 1);
    let dw = num_arg(args, 2);
    let dh = num_arg(args, 3);
    ACTIVE_RECORDER.with(|s| {
        if let Some(buf) = s.borrow_mut().as_mut() {
            buf.push(DrawCmd::FillRect {
                dx,
                dy,
                dw,
                dh,
                color,
            });
        }
    });
    Ok(JsValue::undefined())
}

fn num_arg(args: &[JsValue], i: usize) -> f32 {
    match args.get(i) {
        Some(JsValue::Rational(n)) => *n as f32,
        Some(JsValue::Integer(n)) => *n as f32,
        _ => 0.0,
    }
}

fn make_geometry(ctx: &mut Context, width: f32, height: f32) -> JsResult<JsObject> {
    Ok(ObjectInitializer::new(ctx)
        .property(
            js_string!("width"),
            JsValue::from(width as f64),
            Attribute::READONLY,
        )
        .property(
            js_string!("height"),
            JsValue::from(height as f64),
            Attribute::READONLY,
        )
        .build())
}

/// Minimal CSS colour parser sufficient for worklet fillStyle —
/// supports `rgb(r, g, b)`, `rgba(r, g, b, a)`, `#rrggbb`, and the
/// canonical color keywords (red/green/blue/black/white).
fn parse_css_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('#') {
        return parse_hex_color(rest);
    }
    if let Some(inner) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<f32> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 3 {
            return Some(Color::rgb(
                parts[0].clamp(0.0, 255.0) as u8,
                parts[1].clamp(0.0, 255.0) as u8,
                parts[2].clamp(0.0, 255.0) as u8,
            ));
        }
    }
    if let Some(inner) = s.strip_prefix("rgba(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<f32> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 4 {
            return Some(Color {
                r: parts[0].clamp(0.0, 255.0) as u8,
                g: parts[1].clamp(0.0, 255.0) as u8,
                b: parts[2].clamp(0.0, 255.0) as u8,
                a: (parts[3].clamp(0.0, 1.0) * 255.0) as u8,
            });
        }
    }
    match s.to_ascii_lowercase().as_str() {
        "red" => Some(Color::rgb(255, 0, 0)),
        "green" => Some(Color::rgb(0, 128, 0)),
        "blue" => Some(Color::rgb(0, 0, 255)),
        "black" => Some(Color::rgb(0, 0, 0)),
        "white" => Some(Color::rgb(255, 255, 255)),
        "yellow" => Some(Color::rgb(255, 255, 0)),
        "magenta" => Some(Color::rgb(255, 0, 255)),
        "cyan" => Some(Color::rgb(0, 255, 255)),
        "transparent" => Some(Color::TRANSPARENT),
        _ => None,
    }
}

fn parse_hex_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        return Some(Color::rgb(r, g, b));
    }
    if s.len() == 3 {
        let exp = |c: u8| (c << 4) | c;
        let r = u8::from_str_radix(&s[0..1], 16).ok()?;
        let g = u8::from_str_radix(&s[1..2], 16).ok()?;
        let b = u8::from_str_radix(&s[2..3], 16).ok()?;
        return Some(Color::rgb(exp(r), exp(g), exp(b)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_rgb_and_keyword_colors() {
        let c = parse_css_color("#ff8800").unwrap();
        assert_eq!((c.r, c.g, c.b), (0xff, 0x88, 0x00));
        let c = parse_css_color("rgb(10, 20, 30)").unwrap();
        assert_eq!((c.r, c.g, c.b), (10, 20, 30));
        let c = parse_css_color("rgba(10, 20, 30, 0.5)").unwrap();
        assert_eq!((c.r, c.g, c.b, c.a), (10, 20, 30, 127));
        let c = parse_css_color("red").unwrap();
        assert_eq!((c.r, c.g, c.b), (255, 0, 0));
        assert!(parse_css_color("not-a-color").is_none());
    }

    #[test]
    fn registry_round_trip() {
        clear_all();
        // Standalone Context for the test — we only need to feed
        // registerPaint a name + an object handle.
        let mut ctx = Context::default();
        let obj = boa_engine::object::JsObject::with_null_proto();
        let result = register_paint(
            &JsValue::undefined(),
            &[
                JsValue::from(js_string!("checker")),
                JsValue::from(obj.clone()),
            ],
            &mut ctx,
        );
        assert!(result.is_ok());
        assert!(lookup("checker").is_some());
        assert!(lookup("does-not-exist").is_none());
        clear_all();
    }

    #[test]
    fn invoke_for_runs_paint_method_and_records_fillrect() {
        // End-to-end: install registerPaint, eval a worklet that
        // calls `ctx.fillStyle = "red"; ctx.fillRect(0,0,w,h)`,
        // then invoke for a fake node. The recorder should hold
        // one FillRect with the right colour.
        clear_all();
        let mut ctx = Context::default();
        // Hook up registerPaint as a global.
        let realm = ctx.realm().clone();
        let f = boa_engine::object::FunctionObjectBuilder::new(
            &realm,
            NativeFunction::from_fn_ptr(register_paint),
        )
        .build();
        ctx.register_global_property(
            js_string!("registerPaint"),
            JsValue::from(f),
            Attribute::WRITABLE | Attribute::CONFIGURABLE,
        )
        .unwrap();
        let src = r#"
            class FillBox {
                paint(c, geom) {
                    c.fillStyle = "rgb(11, 22, 33)";
                    c.fillRect(0, 0, geom.width, geom.height);
                }
            }
            registerPaint("fillbox", FillBox);
        "#;
        ctx.eval(boa_engine::Source::from_bytes(src.as_bytes())).unwrap();
        assert!(lookup("fillbox").is_some());
        let node = NodeId::from_raw(42);
        invoke_for(&mut ctx, node, "fillbox", 50.0, 60.0);
        let cmds = commands_for(node).expect("commands recorded");
        assert_eq!(cmds.len(), 1);
        if let DrawCmd::FillRect { dw, dh, color, .. } = cmds[0] {
            assert!((dw - 50.0).abs() < 0.01);
            assert!((dh - 60.0).abs() < 0.01);
            assert_eq!((color.r, color.g, color.b), (11, 22, 33));
        }
        clear_all();
    }
}
