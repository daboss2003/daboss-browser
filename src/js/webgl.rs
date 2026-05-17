//! WebGL 1.0 binding (toy).
//!
//! Exposes the full method surface that `canvas.getContext('webgl')` /
//! `'webgl2'` advertises so scripts probing for these APIs don't
//! crash. Only `clear()` / `clearColor()` actually render — they
//! write into the canvas pixmap (which paint composites onto the
//! page). Shader-pipeline calls (createShader / linkProgram /
//! drawArrays / etc.) record state but don't translate GLSL → WGSL
//! to drive wgpu draw passes. Wiring that in is its own follow-up;
//! a full WebGL→wgpu translation layer is a multi-day project.
//!
//! What's intentionally NOT here:
//!  * Real shader compilation / draw rendering.
//!  * Textures (2D / cubemap upload, mipmaps).
//!  * Framebuffers / renderbuffers.
//!  * Extensions surface (`getExtension()` returns null).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::{
    js_string,
    object::ObjectInitializer,
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;

#[derive(Default)]
pub struct WebGlState {
    pub clear_color: [f32; 4],
    pub viewport: [i32; 4],
    /// Counter handing out fake handle ids for shaders / programs /
    /// buffers — pages that round-trip values through these calls
    /// expect distinct integers.
    pub next_handle: u32,
    #[allow(dead_code)] // backing store for getUniformLocation once shader exec lands
    pub uniform_locations: HashMap<(u32, String), u32>,
    #[allow(dead_code)] // backing store for getAttribLocation once shader exec lands
    pub attrib_locations: HashMap<(u32, String), u32>,
}

pub type WebGlContexts = Rc<RefCell<HashMap<NodeId, Rc<RefCell<WebGlState>>>>>;

thread_local! {
    pub(crate) static JS_WEBGL: RefCell<Option<WebGlContexts>> = const { RefCell::new(None) };
}

const CTX_NODE_KEY: &str = "__webgl_node";

/// Build a WebGL rendering context for the given `<canvas>` node.
/// Returns the JS handle.
pub fn get_or_create_context(ctx: &mut Context, node: NodeId) -> JsValue {
    let state = JS_WEBGL.with(|slot| {
        let map = slot.borrow();
        map.as_ref().map(|rc| {
            let mut m = rc.borrow_mut();
            m.entry(node)
                .or_insert_with(|| Rc::new(RefCell::new(WebGlState::default())))
                .clone()
        })
    });
    let Some(_state) = state else {
        return JsValue::null();
    };

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(CTX_NODE_KEY),
        JsValue::from(node.index() as u32),
        Attribute::READONLY,
    );

    // The full surface. Everything not-rendering returns a placeholder.
    let stubs: &[&str] = &[
        // Buffers
        "createBuffer",
        "deleteBuffer",
        "bindBuffer",
        "bufferData",
        "bufferSubData",
        // Shaders / programs
        "createShader",
        "deleteShader",
        "shaderSource",
        "compileShader",
        "getShaderParameter",
        "getShaderInfoLog",
        "createProgram",
        "deleteProgram",
        "attachShader",
        "linkProgram",
        "getProgramParameter",
        "getProgramInfoLog",
        "useProgram",
        // Attributes
        "getAttribLocation",
        "vertexAttribPointer",
        "enableVertexAttribArray",
        "disableVertexAttribArray",
        // Uniforms
        "getUniformLocation",
        "uniform1f",
        "uniform2f",
        "uniform3f",
        "uniform4f",
        "uniform1i",
        "uniform1fv",
        "uniformMatrix4fv",
        // Textures
        "createTexture",
        "deleteTexture",
        "bindTexture",
        "texImage2D",
        "texParameteri",
        "activeTexture",
        // State
        "enable",
        "disable",
        "blendFunc",
        "depthFunc",
        "cullFace",
        // Drawing
        "drawArrays",
        "drawElements",
        // Misc
        "getParameter",
        "getError",
        "getExtension",
        "getSupportedExtensions",
        "isContextLost",
        "pixelStorei",
        "scissor",
    ];

    for name in stubs {
        b.function(
            NativeFunction::from_fn_ptr(webgl_stub),
            js_string!(*name),
            1,
        );
    }

    // The two methods that actually render.
    b.function(
        NativeFunction::from_fn_ptr(webgl_clear_color),
        js_string!("clearColor"),
        4,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_clear),
        js_string!("clear"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_viewport),
        js_string!("viewport"),
        4,
    );

    // Constant enum-style properties scripts pull from the context.
    for (k, v) in webgl_constants() {
        b.property(js_string!(k), JsValue::from(v), Attribute::READONLY);
    }

    Ok::<JsValue, ()>(JsValue::from(b.build())).unwrap()
}

fn state_for(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<WebGlState>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_NODE_KEY), ctx).ok()?;
    let node = NodeId::from_raw(v.to_u32(ctx).ok()?);
    JS_WEBGL.with(|slot| slot.borrow().as_ref().and_then(|rc| rc.borrow().get(&node).cloned()))
}

fn webgl_stub(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Hand out a fresh fake handle id so callers that do
    // `var prog = gl.createProgram(); gl.attachShader(prog, ...)`
    // see distinct integers and don't trip on `0 == 0`.
    if let Some(state) = state_for(this, ctx) {
        let mut s = state.borrow_mut();
        s.next_handle = s.next_handle.wrapping_add(1);
        return Ok(JsValue::from(s.next_handle));
    }
    Ok(JsValue::from(0))
}

fn webgl_clear_color(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let r = args.first().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let g = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let b = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let a = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(1.0) as f32;
    state.borrow_mut().clear_color = [r, g, b, a];
    Ok(JsValue::undefined())
}

fn webgl_clear(this: &JsValue, _args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let [r, g, b, a] = state.borrow().clear_color;
    let node_idx = this
        .as_object()
        .and_then(|o| o.get(js_string!(CTX_NODE_KEY), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .map(NodeId::from_raw);
    let Some(node) = node_idx else {
        return Ok(JsValue::undefined());
    };
    // Find the canvas pixmap and overwrite every pixel with the
    // current clear color. Premultiplied RGBA.
    super::canvas::JS_CANVAS_SURFACES.with(|slot| {
        let Some(rc) = slot.borrow().as_ref().cloned() else {
            return;
        };
        let mut map = rc.borrow_mut();
        if let Some(surface) = map.get_mut(&node) {
            let a8 = (a.clamp(0.0, 1.0) * 255.0) as u8;
            let r8 = ((r.clamp(0.0, 1.0) * a.clamp(0.0, 1.0)) * 255.0) as u8;
            let g8 = ((g.clamp(0.0, 1.0) * a.clamp(0.0, 1.0)) * 255.0) as u8;
            let b8 = ((b.clamp(0.0, 1.0) * a.clamp(0.0, 1.0)) * 255.0) as u8;
            let data = surface.pixmap.data_mut();
            for chunk in data.chunks_exact_mut(4) {
                chunk[0] = r8;
                chunk[1] = g8;
                chunk[2] = b8;
                chunk[3] = a8;
            }
        }
    });
    Ok(JsValue::undefined())
}

fn webgl_viewport(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let x = args.first().and_then(|v| v.to_i32(ctx).ok()).unwrap_or(0);
    let y = args.get(1).and_then(|v| v.to_i32(ctx).ok()).unwrap_or(0);
    let w = args.get(2).and_then(|v| v.to_i32(ctx).ok()).unwrap_or(0);
    let h = args.get(3).and_then(|v| v.to_i32(ctx).ok()).unwrap_or(0);
    state.borrow_mut().viewport = [x, y, w, h];
    Ok(JsValue::undefined())
}

fn webgl_constants() -> Vec<(&'static str, u32)> {
    // Only the most commonly probed enum values. Real WebGL has
    // hundreds; pages that read others will get `undefined` and the
    // spec-correct ones can be added on demand.
    vec![
        ("COLOR_BUFFER_BIT", 0x4000),
        ("DEPTH_BUFFER_BIT", 0x100),
        ("STENCIL_BUFFER_BIT", 0x400),
        ("TRIANGLES", 0x0004),
        ("TRIANGLE_STRIP", 0x0005),
        ("TRIANGLE_FAN", 0x0006),
        ("LINES", 0x0001),
        ("LINE_STRIP", 0x0003),
        ("POINTS", 0x0000),
        ("ARRAY_BUFFER", 0x8892),
        ("ELEMENT_ARRAY_BUFFER", 0x8893),
        ("STATIC_DRAW", 0x88E4),
        ("DYNAMIC_DRAW", 0x88E8),
        ("FLOAT", 0x1406),
        ("UNSIGNED_BYTE", 0x1401),
        ("UNSIGNED_SHORT", 0x1403),
        ("VERTEX_SHADER", 0x8B31),
        ("FRAGMENT_SHADER", 0x8B30),
        ("COMPILE_STATUS", 0x8B81),
        ("LINK_STATUS", 0x8B82),
        ("TEXTURE_2D", 0x0DE1),
        ("TEXTURE0", 0x84C0),
        ("BLEND", 0x0BE2),
        ("CULL_FACE", 0x0B44),
        ("DEPTH_TEST", 0x0B71),
        ("RGBA", 0x1908),
        ("CLAMP_TO_EDGE", 0x812F),
        ("LINEAR", 0x2601),
        ("NEAREST", 0x2600),
        ("TEXTURE_MIN_FILTER", 0x2801),
        ("TEXTURE_MAG_FILTER", 0x2800),
        ("TEXTURE_WRAP_S", 0x2802),
        ("TEXTURE_WRAP_T", 0x2803),
        ("SRC_ALPHA", 0x0302),
        ("ONE_MINUS_SRC_ALPHA", 0x0303),
    ]
}
