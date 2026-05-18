//! WebGL 1.0 binding backed by `naga` + `wgpu`.
//!
//! The JS facade still exposes the full stub surface (so feature
//! detection probes don't crash), but the load-bearing methods now
//! drive real GPU work:
//!   * `shaderSource` / `compileShader` translate GLSL ES → WGSL via
//!     `naga`'s glsl frontend. Compile errors surface through
//!     `getShaderInfoLog` so libraries that gate behaviour on that
//!     string see real diagnostics.
//!   * `linkProgram` records which vertex + fragment shader pair is
//!     active.
//!   * `bufferData` stores vertex bytes against a buffer handle.
//!   * `drawArrays(TRIANGLES, first, count)` builds a wgpu pipeline,
//!     submits a render pass into a per-canvas texture, copies the
//!     result back into the canvas pixmap so paint can composite.
//!
//! Limitations remain (uniforms, textures, multi-attribute layouts —
//! see the `webgl_gpu` module header). Triangle-clear and
//! single-position-attribute draws are what's verified to work.

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
use crate::webgl_gpu::{self, CanvasTarget, ShaderStage, WebGlGpu};

#[derive(Clone)]
struct ShaderEntry {
    stage: ShaderStage,
    source: String,
    /// Set when `compileShader` succeeds. Empty string when not yet
    /// compiled; non-empty error log when it fails.
    info_log: String,
    /// Translated WGSL on success.
    wgsl: Option<String>,
}

#[derive(Default, Clone)]
struct ProgramEntry {
    vertex_shader: Option<u32>,
    fragment_shader: Option<u32>,
    info_log: String,
    linked: bool,
}

#[derive(Default)]
pub struct WebGlState {
    pub clear_color: [f32; 4],
    pub viewport: [i32; 4],
    /// Counter handing out fake handle ids for shaders / programs /
    /// buffers — pages that round-trip values through these calls
    /// expect distinct integers.
    pub next_handle: u32,
    #[allow(dead_code)] // reserved for uniform binding once we wire bind groups
    pub uniform_locations: HashMap<(u32, String), u32>,
    #[allow(dead_code)] // reserved for attribute binding once we wire vertex layouts
    pub attrib_locations: HashMap<(u32, String), u32>,

    shaders: HashMap<u32, ShaderEntry>,
    programs: HashMap<u32, ProgramEntry>,
    buffers: HashMap<u32, Vec<u8>>,
    bound_array_buffer: Option<u32>,
    current_program: Option<u32>,
    target: Option<CanvasTarget>,
}

pub type WebGlContexts = Rc<RefCell<HashMap<NodeId, Rc<RefCell<WebGlState>>>>>;

thread_local! {
    pub(crate) static JS_WEBGL: RefCell<Option<WebGlContexts>> = const { RefCell::new(None) };
    /// Lazily-initialised wgpu device shared across canvases on the
    /// page. Created on the first `drawArrays`, never destroyed (kept
    /// alive for the engine's lifetime via the per-page thread-local
    /// teardown pattern).
    pub(crate) static JS_WEBGL_GPU: RefCell<Option<Rc<WebGlGpu>>> =
        const { RefCell::new(None) };
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

    // Methods that record handle allocations but don't produce GPU
    // work yet. Each returns a fresh integer so JS round-trips between
    // create / attach / bind continue to compare distinct values.
    let stubs: &[&str] = &[
        "deleteBuffer",
        "bufferSubData",
        "deleteShader",
        "deleteProgram",
        "getAttribLocation",
        "vertexAttribPointer",
        "enableVertexAttribArray",
        "disableVertexAttribArray",
        "getUniformLocation",
        "uniform1f",
        "uniform2f",
        "uniform3f",
        "uniform4f",
        "uniform1i",
        "uniform1fv",
        "uniformMatrix4fv",
        "createTexture",
        "deleteTexture",
        "bindTexture",
        "texImage2D",
        "texParameteri",
        "activeTexture",
        "enable",
        "disable",
        "blendFunc",
        "depthFunc",
        "cullFace",
        "drawElements",
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

    // Methods backed by real state / GPU work.
    b.function(
        NativeFunction::from_fn_ptr(webgl_create_buffer),
        js_string!("createBuffer"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_bind_buffer),
        js_string!("bindBuffer"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_buffer_data),
        js_string!("bufferData"),
        3,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_create_shader),
        js_string!("createShader"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_shader_source),
        js_string!("shaderSource"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_compile_shader),
        js_string!("compileShader"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_get_shader_parameter),
        js_string!("getShaderParameter"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_get_shader_info_log),
        js_string!("getShaderInfoLog"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_create_program),
        js_string!("createProgram"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_attach_shader),
        js_string!("attachShader"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_link_program),
        js_string!("linkProgram"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_get_program_parameter),
        js_string!("getProgramParameter"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_get_program_info_log),
        js_string!("getProgramInfoLog"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(webgl_use_program),
        js_string!("useProgram"),
        1,
    );
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
    b.function(
        NativeFunction::from_fn_ptr(webgl_draw_arrays),
        js_string!("drawArrays"),
        3,
    );

    // Constant enum-style properties scripts pull from the context.
    for (k, v) in webgl_constants() {
        b.property(js_string!(k), JsValue::from(v), Attribute::READONLY);
    }

    JsValue::from(b.build())
}

fn state_for(this: &JsValue, ctx: &mut Context) -> Option<Rc<RefCell<WebGlState>>> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_NODE_KEY), ctx).ok()?;
    let node = NodeId::from_raw(v.to_u32(ctx).ok()?);
    JS_WEBGL.with(|slot| slot.borrow().as_ref().and_then(|rc| rc.borrow().get(&node).cloned()))
}

fn node_for(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_NODE_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

fn new_handle(state: &Rc<RefCell<WebGlState>>) -> u32 {
    let mut s = state.borrow_mut();
    s.next_handle = s.next_handle.wrapping_add(1);
    s.next_handle
}

fn webgl_stub(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    if let Some(state) = state_for(this, ctx) {
        return Ok(JsValue::from(new_handle(&state)));
    }
    Ok(JsValue::from(0))
}

fn webgl_create_buffer(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(0));
    };
    let id = new_handle(&state);
    state.borrow_mut().buffers.insert(id, Vec::new());
    Ok(JsValue::from(id))
}

fn webgl_bind_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let target = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let handle = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    // ARRAY_BUFFER = 0x8892
    if target == 0x8892 {
        state.borrow_mut().bound_array_buffer = if handle == 0 { None } else { Some(handle) };
    }
    Ok(JsValue::undefined())
}

fn webgl_buffer_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // bufferData(target, data, usage)
    let target = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if target != 0x8892 {
        return Ok(JsValue::undefined());
    }
    let data_arg = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let bytes = extract_typed_array_bytes(&data_arg, ctx);
    let Some(buf_id) = state.borrow().bound_array_buffer else {
        return Ok(JsValue::undefined());
    };
    if let Some(slot) = state.borrow_mut().buffers.get_mut(&buf_id) {
        *slot = bytes;
    }
    Ok(JsValue::undefined())
}

fn extract_typed_array_bytes(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
    // Boa exposes Float32Array as an object with `length` and indexed
    // numeric properties; reading via `buffer` requires more plumbing.
    // For our toy: iterate index keys, write each value as f32 LE.
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut out = Vec::with_capacity(len as usize * 4);
    for i in 0..len {
        let v = obj
            .get(i, ctx)
            .ok()
            .and_then(|v| v.to_number(ctx).ok())
            .unwrap_or(0.0) as f32;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn webgl_create_shader(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(0));
    };
    let ty = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let stage = match ty {
        0x8B31 => ShaderStage::Vertex,
        0x8B30 => ShaderStage::Fragment,
        _ => return Ok(JsValue::from(0)),
    };
    let id = new_handle(&state);
    state.borrow_mut().shaders.insert(
        id,
        ShaderEntry {
            stage,
            source: String::new(),
            info_log: String::new(),
            wgsl: None,
        },
    );
    Ok(JsValue::from(id))
}

fn webgl_shader_source(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let id = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let src = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    if let Some(s) = state.borrow_mut().shaders.get_mut(&id) {
        s.source = src;
        s.wgsl = None;
        s.info_log.clear();
    }
    Ok(JsValue::undefined())
}

fn webgl_compile_shader(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let id = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let (stage, source) = {
        let s = state.borrow();
        match s.shaders.get(&id) {
            Some(e) => (e.stage, e.source.clone()),
            None => return Ok(JsValue::undefined()),
        }
    };
    match webgl_gpu::glsl_to_wgsl(&source, stage) {
        Ok(wgsl) => {
            if let Some(e) = state.borrow_mut().shaders.get_mut(&id) {
                e.wgsl = Some(wgsl);
                e.info_log.clear();
            }
        }
        Err(log) => {
            if let Some(e) = state.borrow_mut().shaders.get_mut(&id) {
                e.wgsl = None;
                e.info_log = log;
            }
        }
    }
    Ok(JsValue::undefined())
}

fn webgl_get_shader_parameter(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let id = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let pname = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    // COMPILE_STATUS = 0x8B81
    if pname == 0x8B81 {
        let ok = state
            .borrow()
            .shaders
            .get(&id)
            .map(|e| e.wgsl.is_some())
            .unwrap_or(false);
        return Ok(JsValue::from(ok));
    }
    Ok(JsValue::from(true))
}

fn webgl_get_shader_info_log(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let id = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let log = state
        .borrow()
        .shaders
        .get(&id)
        .map(|e| e.info_log.clone())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(log)))
}

fn webgl_create_program(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(0));
    };
    let id = new_handle(&state);
    state.borrow_mut().programs.insert(id, ProgramEntry::default());
    Ok(JsValue::from(id))
}

fn webgl_attach_shader(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let shader = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let stage = state.borrow().shaders.get(&shader).map(|e| e.stage);
    if let (Some(stage), Some(p)) = (stage, state.borrow_mut().programs.get_mut(&prog)) {
        match stage {
            ShaderStage::Vertex => p.vertex_shader = Some(shader),
            ShaderStage::Fragment => p.fragment_shader = Some(shader),
        }
    }
    Ok(JsValue::undefined())
}

fn webgl_link_program(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let s = state.borrow();
    let (vs, fs) = match s.programs.get(&prog) {
        Some(p) => (p.vertex_shader, p.fragment_shader),
        None => return Ok(JsValue::undefined()),
    };
    let vs_ok = vs.and_then(|id| s.shaders.get(&id)).map(|e| e.wgsl.is_some()).unwrap_or(false);
    let fs_ok = fs.and_then(|id| s.shaders.get(&id)).map(|e| e.wgsl.is_some()).unwrap_or(false);
    drop(s);
    if let Some(p) = state.borrow_mut().programs.get_mut(&prog) {
        p.linked = vs_ok && fs_ok;
        if !p.linked {
            p.info_log = "link failed: missing or invalid shader stage".to_string();
        } else {
            p.info_log.clear();
        }
    }
    Ok(JsValue::undefined())
}

fn webgl_get_program_parameter(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(false));
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let pname = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    // LINK_STATUS = 0x8B82
    if pname == 0x8B82 {
        let ok = state
            .borrow()
            .programs
            .get(&prog)
            .map(|p| p.linked)
            .unwrap_or(false);
        return Ok(JsValue::from(ok));
    }
    Ok(JsValue::from(true))
}

fn webgl_get_program_info_log(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(js_string!("")));
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let log = state
        .borrow()
        .programs
        .get(&prog)
        .map(|p| p.info_log.clone())
        .unwrap_or_default();
    Ok(JsValue::from(js_string!(log)))
}

fn webgl_use_program(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    state.borrow_mut().current_program = if prog == 0 { None } else { Some(prog) };
    Ok(JsValue::undefined())
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
    let Some(node) = node_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
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

fn webgl_draw_arrays(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // drawArrays(mode, first, count)
    let mode = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if mode != 0x0004 {
        // Only TRIANGLES wired in toy.
        return Ok(JsValue::undefined());
    }
    let first = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let count = args.get(2).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if count == 0 {
        return Ok(JsValue::undefined());
    }
    let Some(node) = node_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };

    // Pull the wgsl + buffer bytes out of the state map.
    let (vertex_wgsl, fragment_wgsl, vbuf_bytes, clear_color) = {
        let s = state.borrow();
        let Some(prog_id) = s.current_program else {
            return Ok(JsValue::undefined());
        };
        let Some(prog) = s.programs.get(&prog_id).filter(|p| p.linked) else {
            return Ok(JsValue::undefined());
        };
        let vs = prog.vertex_shader.and_then(|id| s.shaders.get(&id));
        let fs = prog.fragment_shader.and_then(|id| s.shaders.get(&id));
        let (Some(vs), Some(fs)) = (vs, fs) else {
            return Ok(JsValue::undefined());
        };
        let (Some(vs_wgsl), Some(fs_wgsl)) = (vs.wgsl.clone(), fs.wgsl.clone()) else {
            return Ok(JsValue::undefined());
        };
        let buf_id = match s.bound_array_buffer {
            Some(id) => id,
            None => return Ok(JsValue::undefined()),
        };
        let bytes = s.buffers.get(&buf_id).cloned().unwrap_or_default();
        (vs_wgsl, fs_wgsl, bytes, s.clear_color)
    };
    if vbuf_bytes.is_empty() {
        return Ok(JsValue::undefined());
    }

    // Find the canvas pixmap; its dimensions define the render target.
    let dims = super::canvas::JS_CANVAS_SURFACES.with(|slot| -> Option<(u32, u32)> {
        let rc = slot.borrow().as_ref().cloned()?;
        let map = rc.borrow();
        let s = map.get(&node)?;
        Some((s.pixmap.width(), s.pixmap.height()))
    });
    let Some((width, height)) = dims else {
        return Ok(JsValue::undefined());
    };

    let gpu = ensure_gpu();
    let Some(gpu) = gpu else {
        return Ok(JsValue::undefined());
    };

    // Lazily build the per-canvas target once, then reuse for
    // subsequent draws at the same dimensions.
    let target_dims_changed = state
        .borrow()
        .target
        .as_ref()
        .map(|t| t.width != width || t.height != height)
        .unwrap_or(true);
    if target_dims_changed {
        let t = CanvasTarget::new(&gpu.device, width, height);
        state.borrow_mut().target = Some(t);
    }

    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let ok = {
        let s = state.borrow();
        let target = s.target.as_ref().unwrap();
        webgl_gpu::draw_arrays(
            &gpu,
            target,
            &vertex_wgsl,
            &fragment_wgsl,
            &vbuf_bytes,
            first,
            count,
            clear_color,
            &mut rgba,
        )
    };
    if !ok {
        return Ok(JsValue::undefined());
    }

    // Copy our rendered pixels into the canvas pixmap.
    super::canvas::JS_CANVAS_SURFACES.with(|slot| {
        let Some(rc) = slot.borrow().as_ref().cloned() else {
            return;
        };
        let mut map = rc.borrow_mut();
        if let Some(surface) = map.get_mut(&node) {
            surface.pixmap.data_mut().copy_from_slice(&rgba);
        }
    });
    Ok(JsValue::undefined())
}

/// Return the shared `WebGlGpu`, creating it on first use. Returns
/// `None` if no usable adapter is available (e.g. CI environment
/// without a GPU); the draw call falls back to a no-op.
fn ensure_gpu() -> Option<Rc<WebGlGpu>> {
    JS_WEBGL_GPU.with(|slot| {
        if let Some(g) = slot.borrow().as_ref() {
            return Some(g.clone());
        }
        let gpu = WebGlGpu::new()?;
        let rc = Rc::new(gpu);
        *slot.borrow_mut() = Some(rc.clone());
        Some(rc)
    })
}

fn webgl_constants() -> Vec<(&'static str, u32)> {
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
