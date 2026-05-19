//! WebGL 1.0 binding backed by `naga` + `wgpu`.
//!
//! Real-API behaviours implemented here:
//!   * Shaders: GLSL ES → WGSL via `naga`. `compileShader` records
//!     compile errors; `getShaderInfoLog` reads them back.
//!   * Programs: track vertex+fragment shader pair, link status,
//!     per-name uniform / attribute locations.
//!   * Buffers: `bufferData` stores raw bytes against ARRAY_BUFFER or
//!     ELEMENT_ARRAY_BUFFER handles.
//!   * Vertex attributes: `vertexAttribPointer` records (buffer,
//!     size, type, stride, offset); `enableVertexAttribArray` toggles
//!     active slots.
//!   * Uniforms: `uniform1f` / `uniform2f` / `uniform3f` / `uniform4f`
//!     / `uniform1i` / `uniform1fv` / `uniformMatrix4fv` write into a
//!     per-program packed uniform buffer. The toy uses a single
//!     `@group(0) @binding(0)` block for all uniforms.
//!   * Textures: `createTexture` + `texImage2D` upload RGBA bytes;
//!     `bindTexture` + `activeTexture` track active binding for the
//!     fragment stage at draw time.
//!   * `drawArrays` and `drawElements` build a real wgpu pipeline +
//!     bind group from the above state and render into the canvas
//!     pixmap.
//!
//! Not yet wired: framebuffer objects, renderbuffers, cube maps,
//! anisotropic filtering, multi-sample render targets, vertex array
//! objects, getError flow beyond stubs.

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
use crate::webgl_gpu::{
    self, AttribComponent, AttribLayout, CanvasTarget, DrawDesc, IndexFormat, ShaderStage,
    TranslatedShader, UploadedTexture, WebGlGpu,
};

#[derive(Clone)]
struct ShaderEntry {
    stage: ShaderStage,
    source: String,
    info_log: String,
    translated: Option<TranslatedShader>,
}

#[derive(Default, Clone)]
struct ProgramEntry {
    vertex_shader: Option<u32>,
    fragment_shader: Option<u32>,
    info_log: String,
    linked: bool,
    /// Name → packed-uniform-buffer byte offset. Filled on `getUniformLocation`.
    uniform_offsets: HashMap<String, u32>,
    /// Name → vertex attribute location. Filled on `getAttribLocation`.
    attrib_locations: HashMap<String, u32>,
    /// Next uniform offset to hand out, in bytes. Round up to 16 per
    /// allocation to keep std140-like alignment.
    next_uniform_offset: u32,
    /// Next attribute location index.
    next_attrib_location: u32,
}

#[derive(Clone)]
struct AttribState {
    buffer_id: u32,
    size: u32,
    component: AttribComponent,
    stride: u32,
    offset: u64,
    enabled: bool,
}

#[derive(Clone)]
struct TextureEntry {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Default)]
pub struct WebGlState {
    pub clear_color: [f32; 4],
    pub viewport: [i32; 4],
    pub next_handle: u32,

    shaders: HashMap<u32, ShaderEntry>,
    programs: HashMap<u32, ProgramEntry>,
    /// Buffer handles → raw bytes. Stores both ARRAY_BUFFER and
    /// ELEMENT_ARRAY_BUFFER data; the binding state tracks which.
    buffers: HashMap<u32, Vec<u8>>,
    bound_array_buffer: Option<u32>,
    bound_element_array_buffer: Option<u32>,
    current_program: Option<u32>,
    /// 8 attribute slots (WebGL 1's `MAX_VERTEX_ATTRIBS` is at least 8).
    attribs: [Option<AttribState>; 8],
    /// Per-program uniform buffer bytes. Indexed by program handle.
    uniform_buffers: HashMap<u32, Vec<u8>>,
    textures: HashMap<u32, TextureEntry>,
    active_texture_unit: u32,
    /// Per-unit bound texture (only unit 0 is consulted at draw time).
    bound_textures: HashMap<u32, u32>,
    target: Option<CanvasTarget>,
}

pub type WebGlContexts = Rc<RefCell<HashMap<NodeId, Rc<RefCell<WebGlState>>>>>;

thread_local! {
    pub(crate) static JS_WEBGL: RefCell<Option<WebGlContexts>> = const { RefCell::new(None) };
    pub(crate) static JS_WEBGL_GPU: RefCell<Option<Rc<WebGlGpu>>> =
        const { RefCell::new(None) };
}

const CTX_NODE_KEY: &str = "__webgl_node";

pub fn get_or_create_context(ctx: &mut Context, node: NodeId) -> JsValue {
    get_or_create_context_versioned(ctx, node, false)
}

/// Same as `get_or_create_context` but lets the caller select
/// WebGL 1 vs WebGL 2. The WebGL 2 surface keeps the entire WebGL
/// 1 entry-point list and adds: VAOs, instanced draw / divisor,
/// integer attrib pointer, 3D / array textures + immutable
/// storage, multiple render targets, sampler objects, query
/// objects, uniform buffer block binding, transform feedback,
/// sync objects, and the `uniform*ui` family. Most additions are
/// state-tracking stubs that accept the call shape so pages can
/// initialise without crashing; a real driver wires each to the
/// underlying wgpu equivalent.
pub fn get_or_create_context_versioned(
    ctx: &mut Context,
    node: NodeId,
    is_webgl2: bool,
) -> JsValue {
    let state = JS_WEBGL.with(|slot| {
        let map = slot.borrow();
        map.as_ref().map(|rc| {
            let mut m = rc.borrow_mut();
            m.entry(node)
                .or_insert_with(|| Rc::new(RefCell::new(WebGlState::default())))
                .clone()
        })
    });
    if state.is_none() {
        return JsValue::null();
    }

    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(CTX_NODE_KEY),
        JsValue::from(node.index() as u32),
        Attribute::READONLY,
    );

    let stubs: &[&str] = &[
        "deleteBuffer",
        "bufferSubData",
        "deleteShader",
        "deleteProgram",
        "disableVertexAttribArray",
        "uniform1i",
        "texParameteri",
        "deleteTexture",
        "enable",
        "disable",
        "blendFunc",
        "depthFunc",
        "cullFace",
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

    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("createBuffer", NativeFunction::from_fn_ptr(webgl_create_buffer), 0),
        ("bindBuffer", NativeFunction::from_fn_ptr(webgl_bind_buffer), 2),
        ("bufferData", NativeFunction::from_fn_ptr(webgl_buffer_data), 3),
        ("createShader", NativeFunction::from_fn_ptr(webgl_create_shader), 1),
        ("shaderSource", NativeFunction::from_fn_ptr(webgl_shader_source), 2),
        ("compileShader", NativeFunction::from_fn_ptr(webgl_compile_shader), 1),
        ("getShaderParameter", NativeFunction::from_fn_ptr(webgl_get_shader_parameter), 2),
        ("getShaderInfoLog", NativeFunction::from_fn_ptr(webgl_get_shader_info_log), 1),
        ("createProgram", NativeFunction::from_fn_ptr(webgl_create_program), 0),
        ("attachShader", NativeFunction::from_fn_ptr(webgl_attach_shader), 2),
        ("linkProgram", NativeFunction::from_fn_ptr(webgl_link_program), 1),
        ("getProgramParameter", NativeFunction::from_fn_ptr(webgl_get_program_parameter), 2),
        ("getProgramInfoLog", NativeFunction::from_fn_ptr(webgl_get_program_info_log), 1),
        ("useProgram", NativeFunction::from_fn_ptr(webgl_use_program), 1),
        ("clearColor", NativeFunction::from_fn_ptr(webgl_clear_color), 4),
        ("clear", NativeFunction::from_fn_ptr(webgl_clear), 1),
        ("viewport", NativeFunction::from_fn_ptr(webgl_viewport), 4),
        ("drawArrays", NativeFunction::from_fn_ptr(webgl_draw_arrays), 3),
        ("drawElements", NativeFunction::from_fn_ptr(webgl_draw_elements), 4),
        ("getAttribLocation", NativeFunction::from_fn_ptr(webgl_get_attrib_location), 2),
        ("vertexAttribPointer", NativeFunction::from_fn_ptr(webgl_vertex_attrib_pointer), 6),
        ("enableVertexAttribArray", NativeFunction::from_fn_ptr(webgl_enable_vertex_attrib_array), 1),
        ("getUniformLocation", NativeFunction::from_fn_ptr(webgl_get_uniform_location), 2),
        ("uniform1f", NativeFunction::from_fn_ptr(webgl_uniform_1f), 2),
        ("uniform2f", NativeFunction::from_fn_ptr(webgl_uniform_2f), 3),
        ("uniform3f", NativeFunction::from_fn_ptr(webgl_uniform_3f), 4),
        ("uniform4f", NativeFunction::from_fn_ptr(webgl_uniform_4f), 5),
        ("uniform1fv", NativeFunction::from_fn_ptr(webgl_uniform_1fv), 2),
        ("uniformMatrix4fv", NativeFunction::from_fn_ptr(webgl_uniform_matrix_4fv), 3),
        ("createTexture", NativeFunction::from_fn_ptr(webgl_create_texture), 0),
        ("bindTexture", NativeFunction::from_fn_ptr(webgl_bind_texture), 2),
        ("texImage2D", NativeFunction::from_fn_ptr(webgl_tex_image_2d), 9),
        ("activeTexture", NativeFunction::from_fn_ptr(webgl_active_texture), 1),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }

    if is_webgl2 {
        // WebGL 2 additions. Most are state-tracking stubs that
        // accept the call so feature-detection + initial setup
        // calls don't trip; a few (VAOs, sampler / query objects)
        // mint real handles via webgl_make_handle so subsequent
        // bind/use calls match.
        let webgl2_stateful: &[(&str, NativeFunction, usize)] = &[
            ("createVertexArray", NativeFunction::from_fn_ptr(webgl_make_handle), 0),
            ("createSampler", NativeFunction::from_fn_ptr(webgl_make_handle), 0),
            ("createQuery", NativeFunction::from_fn_ptr(webgl_make_handle), 0),
            ("createTransformFeedback", NativeFunction::from_fn_ptr(webgl_make_handle), 0),
            ("fenceSync", NativeFunction::from_fn_ptr(webgl_make_handle), 2),
            ("getUniformBlockIndex", NativeFunction::from_fn_ptr(webgl_make_handle), 2),
            ("getFragDataLocation", NativeFunction::from_fn_ptr(webgl_make_handle), 2),
        ];
        for (name, f, arity) in webgl2_stateful {
            b.function(f.clone(), js_string!(*name), *arity);
        }

        let webgl2_stubs: &[&str] = &[
            "bindVertexArray",
            "deleteVertexArray",
            "isVertexArray",
            "bindSampler",
            "deleteSampler",
            "samplerParameteri",
            "samplerParameterf",
            "isSampler",
            "bindBufferBase",
            "bindBufferRange",
            "uniformBlockBinding",
            "getActiveUniformBlockParameter",
            "getActiveUniformBlockName",
            "getActiveUniforms",
            "getUniformIndices",
            "drawArraysInstanced",
            "drawElementsInstanced",
            "drawRangeElements",
            "drawBuffers",
            "vertexAttribDivisor",
            "vertexAttribIPointer",
            "vertexAttribI4i",
            "vertexAttribI4ui",
            "texImage3D",
            "texSubImage3D",
            "texStorage2D",
            "texStorage3D",
            "copyTexSubImage3D",
            "compressedTexImage3D",
            "compressedTexSubImage3D",
            "uniform1ui",
            "uniform2ui",
            "uniform3ui",
            "uniform4ui",
            "uniform1uiv",
            "uniform2uiv",
            "uniform3uiv",
            "uniform4uiv",
            "uniformMatrix2x3fv",
            "uniformMatrix3x2fv",
            "uniformMatrix2x4fv",
            "uniformMatrix4x2fv",
            "uniformMatrix3x4fv",
            "uniformMatrix4x3fv",
            "beginQuery",
            "endQuery",
            "deleteQuery",
            "isQuery",
            "getQuery",
            "getQueryParameter",
            "beginTransformFeedback",
            "endTransformFeedback",
            "pauseTransformFeedback",
            "resumeTransformFeedback",
            "bindTransformFeedback",
            "deleteTransformFeedback",
            "isTransformFeedback",
            "transformFeedbackVaryings",
            "getTransformFeedbackVarying",
            "deleteSync",
            "isSync",
            "clientWaitSync",
            "waitSync",
            "getSyncParameter",
            "invalidateFramebuffer",
            "invalidateSubFramebuffer",
            "readBuffer",
            "copyBufferSubData",
            "getBufferSubData",
            "renderbufferStorageMultisample",
            "blitFramebuffer",
            "framebufferTextureLayer",
            "getInternalformatParameter",
            "readPixels",
            "clearBufferiv",
            "clearBufferuiv",
            "clearBufferfv",
            "clearBufferfi",
        ];
        for name in webgl2_stubs {
            b.function(
                NativeFunction::from_fn_ptr(webgl_stub),
                js_string!(*name),
                1,
            );
        }

        // WebGL 2 introduces a handful of additional GLenum
        // constants over WebGL 1. Pages probe these via
        // `gl.UNIFORM_BUFFER`, `gl.RGBA8`, etc.
        for (k, v) in webgl2_constants() {
            b.property(js_string!(*k), JsValue::from(*v), Attribute::READONLY);
        }
    }

    for (k, v) in webgl_constants() {
        b.property(js_string!(k), JsValue::from(v), Attribute::READONLY);
    }
    JsValue::from(b.build())
}

/// Mint a fresh opaque handle to back a WebGL 2 `createXxx`
/// call. The handle is just a monotonic u32 stored on the
/// state's handle counter — pages compare it to `null` to detect
/// allocation failure.
fn webgl_make_handle(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::null());
    };
    let h = new_handle(&state);
    Ok(JsValue::from(h))
}

/// GLenum values that WebGL 2 adds on top of WebGL 1. Curated for
/// the constants pages most commonly probe; not every single new
/// enum is listed here (the spec defines hundreds).
fn webgl2_constants() -> &'static [(&'static str, u32)] {
    &[
        // Buffer targets
        ("COPY_READ_BUFFER", 0x8F36),
        ("COPY_WRITE_BUFFER", 0x8F37),
        ("PIXEL_PACK_BUFFER", 0x88EB),
        ("PIXEL_UNPACK_BUFFER", 0x88EC),
        ("TRANSFORM_FEEDBACK_BUFFER", 0x8C8E),
        ("UNIFORM_BUFFER", 0x8A11),
        // Buffer usage hints
        ("STREAM_READ", 0x88E1),
        ("STREAM_COPY", 0x88E2),
        ("STATIC_READ", 0x88E5),
        ("STATIC_COPY", 0x88E6),
        ("DYNAMIC_READ", 0x88E9),
        ("DYNAMIC_COPY", 0x88EA),
        // Sized internal formats
        ("R8", 0x8229),
        ("R16F", 0x822D),
        ("R32F", 0x822E),
        ("R8UI", 0x8232),
        ("RG8", 0x822B),
        ("RG16F", 0x822F),
        ("RG32F", 0x8230),
        ("RGB8", 0x8051),
        ("SRGB8", 0x8C41),
        ("RGB565", 0x8D62),
        ("RGB16F", 0x881B),
        ("RGB32F", 0x8815),
        ("RGBA8", 0x8058),
        ("SRGB8_ALPHA8", 0x8C43),
        ("RGB5_A1", 0x8057),
        ("RGBA4", 0x8056),
        ("RGBA16F", 0x881A),
        ("RGBA32F", 0x8814),
        ("DEPTH_COMPONENT24", 0x81A6),
        ("DEPTH_COMPONENT32F", 0x8CAC),
        ("DEPTH24_STENCIL8", 0x88F0),
        // Texture targets
        ("TEXTURE_3D", 0x806F),
        ("TEXTURE_2D_ARRAY", 0x8C1A),
        ("TEXTURE_WRAP_R", 0x8072),
        ("TEXTURE_MIN_LOD", 0x813A),
        ("TEXTURE_MAX_LOD", 0x813B),
        ("TEXTURE_BASE_LEVEL", 0x813C),
        ("TEXTURE_MAX_LEVEL", 0x813D),
        ("TEXTURE_COMPARE_MODE", 0x884C),
        ("TEXTURE_COMPARE_FUNC", 0x884D),
        ("MAX_3D_TEXTURE_SIZE", 0x8073),
        ("MAX_ARRAY_TEXTURE_LAYERS", 0x88FF),
        ("MAX_COLOR_ATTACHMENTS", 0x8CDF),
        ("MAX_DRAW_BUFFERS", 0x8824),
        ("COLOR_ATTACHMENT0", 0x8CE0),
        ("COLOR_ATTACHMENT1", 0x8CE1),
        ("COLOR_ATTACHMENT2", 0x8CE2),
        ("COLOR_ATTACHMENT3", 0x8CE3),
        ("COLOR_ATTACHMENT4", 0x8CE4),
        ("COLOR_ATTACHMENT5", 0x8CE5),
        ("COLOR_ATTACHMENT6", 0x8CE6),
        ("COLOR_ATTACHMENT7", 0x8CE7),
        // Pixel types
        ("UNSIGNED_INT_2_10_10_10_REV", 0x8368),
        ("UNSIGNED_INT_10F_11F_11F_REV", 0x8C3B),
        ("UNSIGNED_INT_5_9_9_9_REV", 0x8C3E),
        ("FLOAT_32_UNSIGNED_INT_24_8_REV", 0x8DAD),
        ("UNSIGNED_INT_24_8", 0x84FA),
        ("HALF_FLOAT", 0x140B),
        // Query targets
        ("ANY_SAMPLES_PASSED", 0x8C2F),
        ("ANY_SAMPLES_PASSED_CONSERVATIVE", 0x8D6A),
        ("TRANSFORM_FEEDBACK_PRIMITIVES_WRITTEN", 0x8C88),
        ("QUERY_RESULT", 0x8866),
        ("QUERY_RESULT_AVAILABLE", 0x8867),
        // Sync
        ("SYNC_GPU_COMMANDS_COMPLETE", 0x9117),
        ("ALREADY_SIGNALED", 0x911A),
        ("TIMEOUT_EXPIRED", 0x911B),
        ("CONDITION_SATISFIED", 0x911C),
        ("WAIT_FAILED", 0x911D),
        ("SYNC_FLUSH_COMMANDS_BIT", 0x00000001),
        // Misc
        ("READ_BUFFER", 0x0C02),
        ("FRAGMENT_SHADER_DERIVATIVE_HINT", 0x8B8B),
        ("MIN", 0x8007),
        ("MAX", 0x8008),
        ("MIN_PROGRAM_TEXEL_OFFSET", 0x8904),
        ("MAX_PROGRAM_TEXEL_OFFSET", 0x8905),
        ("MAX_VARYING_COMPONENTS", 0x8B4B),
        ("MAX_VERTEX_OUTPUT_COMPONENTS", 0x9122),
        ("MAX_FRAGMENT_INPUT_COMPONENTS", 0x9125),
        ("RASTERIZER_DISCARD", 0x8C89),
        ("VERTEX_ARRAY_BINDING", 0x85B5),
        ("UNPACK_ROW_LENGTH", 0x0CF2),
        ("UNPACK_IMAGE_HEIGHT", 0x806E),
        ("UNPACK_SKIP_ROWS", 0x0CF3),
        ("UNPACK_SKIP_PIXELS", 0x0CF4),
        ("UNPACK_SKIP_IMAGES", 0x806D),
        ("PACK_ROW_LENGTH", 0x0D02),
        ("PACK_SKIP_ROWS", 0x0D03),
        ("PACK_SKIP_PIXELS", 0x0D04),
    ]
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
    let opt = if handle == 0 { None } else { Some(handle) };
    match target {
        0x8892 => state.borrow_mut().bound_array_buffer = opt,
        0x8893 => state.borrow_mut().bound_element_array_buffer = opt,
        _ => {}
    }
    Ok(JsValue::undefined())
}

fn webgl_buffer_data(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let target = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let data_arg = args.get(1).cloned().unwrap_or(JsValue::undefined());
    let element_kind = ElementKind::from_u32(target);
    let bytes = extract_typed_array_bytes(&data_arg, ctx, element_kind);
    let buf_id = match target {
        0x8892 => state.borrow().bound_array_buffer,
        0x8893 => state.borrow().bound_element_array_buffer,
        _ => None,
    };
    let Some(buf_id) = buf_id else {
        return Ok(JsValue::undefined());
    };
    if let Some(slot) = state.borrow_mut().buffers.get_mut(&buf_id) {
        *slot = bytes;
    }
    Ok(JsValue::undefined())
}

enum ElementKind {
    F32,
    U16,
    U32,
    U8,
}

impl ElementKind {
    fn from_u32(target: u32) -> Self {
        // ELEMENT_ARRAY_BUFFER usually carries u16 (default) but we
        // accept u32 too. Default to u16 for index buffers; f32 for
        // anything else.
        if target == 0x8893 {
            Self::U16
        } else {
            Self::F32
        }
    }
}

/// Best-effort byte extraction from a TypedArray-like JS object. We
/// look at the constructor's name (Float32Array / Uint16Array / etc)
/// to choose width; falls back to floats.
fn extract_typed_array_bytes(val: &JsValue, ctx: &mut Context, default_kind: ElementKind) -> Vec<u8> {
    let Some(obj) = val.as_object() else {
        return Vec::new();
    };
    let len = obj
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    if len == 0 {
        return Vec::new();
    }
    let kind = match obj
        .get(js_string!("BYTES_PER_ELEMENT"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
    {
        Some(4) => {
            // Could be Float32 or Uint32. We default Float32 for
            // ARRAY_BUFFER and Uint32 for element buffers.
            match default_kind {
                ElementKind::U16 | ElementKind::U32 => ElementKind::U32,
                _ => ElementKind::F32,
            }
        }
        Some(2) => ElementKind::U16,
        Some(1) => ElementKind::U8,
        _ => default_kind,
    };
    let mut out = Vec::new();
    for i in 0..len {
        let v = obj
            .get(i, ctx)
            .ok()
            .and_then(|v| v.to_number(ctx).ok())
            .unwrap_or(0.0);
        match kind {
            ElementKind::F32 => out.extend_from_slice(&(v as f32).to_le_bytes()),
            ElementKind::U16 => out.extend_from_slice(&(v as u16).to_le_bytes()),
            ElementKind::U32 => out.extend_from_slice(&(v as u32).to_le_bytes()),
            ElementKind::U8 => out.push(v as u8),
        }
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
            translated: None,
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
        s.translated = None;
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
        Ok(translated) => {
            if let Some(e) = state.borrow_mut().shaders.get_mut(&id) {
                e.translated = Some(translated);
                e.info_log.clear();
            }
        }
        Err(log) => {
            if let Some(e) = state.borrow_mut().shaders.get_mut(&id) {
                e.translated = None;
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
    if pname == 0x8B81 {
        let ok = state
            .borrow()
            .shaders
            .get(&id)
            .map(|e| e.translated.is_some())
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
    let vs_ok = vs
        .and_then(|id| s.shaders.get(&id))
        .map(|e| e.translated.is_some())
        .unwrap_or(false);
    let fs_ok = fs
        .and_then(|id| s.shaders.get(&id))
        .map(|e| e.translated.is_some())
        .unwrap_or(false);
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

// ---------- attributes ----------

fn webgl_get_attrib_location(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(-1_i32));
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let name = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let mut s = state.borrow_mut();
    let Some(p) = s.programs.get_mut(&prog) else {
        return Ok(JsValue::from(-1_i32));
    };
    if let Some(loc) = p.attrib_locations.get(&name) {
        return Ok(JsValue::from(*loc as i32));
    }
    if p.next_attrib_location >= 8 {
        return Ok(JsValue::from(-1_i32));
    }
    let loc = p.next_attrib_location;
    p.attrib_locations.insert(name, loc);
    p.next_attrib_location += 1;
    Ok(JsValue::from(loc as i32))
}

fn webgl_vertex_attrib_pointer(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let loc = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0) as usize;
    let size = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let ty = args.get(2).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let normalized = args
        .get(3)
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let stride = args.get(4).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let offset = args.get(5).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let buf = state.borrow().bound_array_buffer.unwrap_or(0);
    if loc >= 8 {
        return Ok(JsValue::undefined());
    }
    let component = match (ty, normalized) {
        // UNSIGNED_BYTE = 0x1401, normalized → Unorm8x4 etc.
        (0x1401, true) => AttribComponent::UnsignedByteNormalized,
        _ => AttribComponent::Float,
    };
    state.borrow_mut().attribs[loc] = Some(AttribState {
        buffer_id: buf,
        size,
        component,
        stride,
        offset: offset as u64,
        enabled: state.borrow().attribs[loc].as_ref().map(|a| a.enabled).unwrap_or(false),
    });
    Ok(JsValue::undefined())
}

fn webgl_enable_vertex_attrib_array(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let loc = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0) as usize;
    if loc >= 8 {
        return Ok(JsValue::undefined());
    }
    let mut s = state.borrow_mut();
    if let Some(a) = s.attribs[loc].as_mut() {
        a.enabled = true;
    } else {
        s.attribs[loc] = Some(AttribState {
            buffer_id: 0,
            size: 0,
            component: AttribComponent::Float,
            stride: 0,
            offset: 0,
            enabled: true,
        });
    }
    Ok(JsValue::undefined())
}

// ---------- uniforms ----------

fn webgl_get_uniform_location(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::null());
    };
    let prog = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let name = args
        .get(1)
        .map(|v| v.to_string(ctx).map(|s| s.to_std_string_escaped()))
        .transpose()?
        .unwrap_or_default();
    let mut s = state.borrow_mut();
    let Some(p) = s.programs.get_mut(&prog) else {
        return Ok(JsValue::null());
    };
    if let Some(off) = p.uniform_offsets.get(&name) {
        return Ok(JsValue::from(*off));
    }
    // Reserve 64 bytes (mat4) per uniform — coarse but keeps any
    // type fit. Real reflection would size per-uniform.
    let off = p.next_uniform_offset;
    p.uniform_offsets.insert(name, off);
    p.next_uniform_offset += 64;
    Ok(JsValue::from(off))
}

fn write_uniform_bytes(state: &Rc<RefCell<WebGlState>>, offset: u32, bytes: &[u8]) {
    let mut s = state.borrow_mut();
    let Some(prog) = s.current_program else {
        return;
    };
    let buf = s.uniform_buffers.entry(prog).or_default();
    let end = offset as usize + bytes.len();
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset as usize..end].copy_from_slice(bytes);
}

fn webgl_uniform_1f(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let v = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    write_uniform_bytes(&state, off, &v.to_le_bytes());
    Ok(JsValue::undefined())
}

fn webgl_uniform_2f(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let x = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let y = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    write_uniform_bytes(&state, off, &bytes);
    Ok(JsValue::undefined())
}

fn webgl_uniform_3f(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let x = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let y = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let z = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&z.to_le_bytes());
    write_uniform_bytes(&state, off, &bytes);
    Ok(JsValue::undefined())
}

fn webgl_uniform_4f(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let x = args.get(1).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let y = args.get(2).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let z = args.get(3).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let w = args.get(4).and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&z.to_le_bytes());
    bytes.extend_from_slice(&w.to_le_bytes());
    write_uniform_bytes(&state, off, &bytes);
    Ok(JsValue::undefined())
}

fn webgl_uniform_1fv(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let Some(arr) = args.get(1).and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(len as usize * 4);
    for i in 0..len {
        let v = arr.get(i, ctx).ok().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    write_uniform_bytes(&state, off, &bytes);
    Ok(JsValue::undefined())
}

fn webgl_uniform_matrix_4fv(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let off = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let _transpose = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
    let Some(arr) = args.get(2).and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut bytes = Vec::with_capacity(len as usize * 4);
    for i in 0..len {
        let v = arr.get(i, ctx).ok().and_then(|v| v.to_number(ctx).ok()).unwrap_or(0.0) as f32;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    write_uniform_bytes(&state, off, &bytes);
    Ok(JsValue::undefined())
}

// ---------- textures ----------

fn webgl_create_texture(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::from(0));
    };
    let id = new_handle(&state);
    state.borrow_mut().textures.insert(
        id,
        TextureEntry {
            width: 0,
            height: 0,
            rgba: Vec::new(),
        },
    );
    Ok(JsValue::from(id))
}

fn webgl_bind_texture(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let _target = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let tex = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let unit = state.borrow().active_texture_unit;
    if tex == 0 {
        state.borrow_mut().bound_textures.remove(&unit);
    } else {
        state.borrow_mut().bound_textures.insert(unit, tex);
    }
    Ok(JsValue::undefined())
}

fn webgl_active_texture(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let unit_enum = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0x84C0);
    // TEXTURE0 = 0x84C0; convert enum to integer unit index.
    let unit = unit_enum.saturating_sub(0x84C0);
    state.borrow_mut().active_texture_unit = unit;
    Ok(JsValue::undefined())
}

fn webgl_tex_image_2d(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // texImage2D(target, level, internalformat, width, height, border, format, type, pixels)
    // OR texImage2D(target, level, internalformat, format, type, source) for HTMLImageElement-style.
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // Support only the 9-arg flat-buffer form for the toy.
    if args.len() < 9 {
        return Ok(JsValue::undefined());
    }
    let _target = args[0].to_u32(ctx).unwrap_or(0);
    let _level = args[1].to_u32(ctx).unwrap_or(0);
    let _internal_format = args[2].to_u32(ctx).unwrap_or(0);
    let width = args[3].to_u32(ctx).unwrap_or(0);
    let height = args[4].to_u32(ctx).unwrap_or(0);
    let _border = args[5].to_u32(ctx).unwrap_or(0);
    let _format = args[6].to_u32(ctx).unwrap_or(0);
    let _type_ = args[7].to_u32(ctx).unwrap_or(0);
    let pixels = &args[8];
    let bytes = extract_typed_array_bytes(pixels, ctx, ElementKind::U8);
    let unit = state.borrow().active_texture_unit;
    let Some(tex_id) = state.borrow().bound_textures.get(&unit).copied() else {
        return Ok(JsValue::undefined());
    };
    if let Some(entry) = state.borrow_mut().textures.get_mut(&tex_id) {
        entry.width = width;
        entry.height = height;
        entry.rgba = bytes;
    }
    Ok(JsValue::undefined())
}

// ---------- drawArrays / drawElements ----------

fn webgl_draw_arrays(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let mode = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if mode != 0x0004 {
        return Ok(JsValue::undefined());
    }
    let first = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let count = args.get(2).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if count == 0 {
        return Ok(JsValue::undefined());
    }
    do_draw(&state, this, ctx, first, count, None)
}

fn webgl_draw_elements(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(state) = state_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let mode = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if mode != 0x0004 {
        return Ok(JsValue::undefined());
    }
    let count = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let ty = args.get(2).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0x1403);
    let _offset = args.get(3).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    if count == 0 {
        return Ok(JsValue::undefined());
    }
    let fmt = match ty {
        // UNSIGNED_INT = 0x1405
        0x1405 => IndexFormat::Uint32,
        // UNSIGNED_SHORT = 0x1403 (default)
        _ => IndexFormat::Uint16,
    };
    do_draw(&state, this, ctx, 0, count, Some(fmt))
}

fn do_draw(
    state: &Rc<RefCell<WebGlState>>,
    this: &JsValue,
    ctx: &mut Context,
    first: u32,
    count: u32,
    index_fmt: Option<IndexFormat>,
) -> JsResult<JsValue> {
    let Some(node) = node_for(this, ctx) else {
        return Ok(JsValue::undefined());
    };

    let (
        vertex_shader,
        fragment_shader,
        attribs,
        uniform_bytes,
        texture,
        clear_color,
        index_bytes,
    ) = {
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
        let (Some(vs_t), Some(fs_t)) = (vs.translated.clone(), fs.translated.clone()) else {
            return Ok(JsValue::undefined());
        };
        let mut attribs = Vec::new();
        for (i, slot) in s.attribs.iter().enumerate() {
            if let Some(a) = slot {
                if a.enabled && a.size > 0 {
                    attribs.push(AttribLayout {
                        location: i as u32,
                        buffer_id: a.buffer_id,
                        size: a.size,
                        component: a.component,
                        stride: a.stride,
                        offset: a.offset,
                    });
                }
            }
        }
        let uniform_bytes = s.uniform_buffers.get(&prog_id).cloned().unwrap_or_default();
        let texture = s
            .bound_textures
            .get(&0)
            .copied()
            .and_then(|id| s.textures.get(&id))
            .cloned();
        let index_bytes = if index_fmt.is_some() {
            s.bound_element_array_buffer
                .and_then(|id| s.buffers.get(&id))
                .cloned()
        } else {
            None
        };
        (vs_t, fs_t, attribs, uniform_bytes, texture, s.clear_color, index_bytes)
    };

    let dims = super::canvas::JS_CANVAS_SURFACES.with(|slot| -> Option<(u32, u32)> {
        let rc = slot.borrow().as_ref().cloned()?;
        let map = rc.borrow();
        let s = map.get(&node)?;
        Some((s.pixmap.width(), s.pixmap.height()))
    });
    let Some((width, height)) = dims else {
        return Ok(JsValue::undefined());
    };

    let Some(gpu) = ensure_gpu() else {
        return Ok(JsValue::undefined());
    };

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

    // Clone buffer map so we can pass a stable reference to draw().
    let buffers: HashMap<u32, Vec<u8>> = state.borrow().buffers.clone();
    let tex_upload = texture.map(|t| UploadedTexture {
        width: t.width,
        height: t.height,
        rgba: t.rgba,
    });
    let index_buffer_view = index_bytes.as_deref().and_then(|b| {
        index_fmt.map(|f| (f, b))
    });

    let mut rgba = vec![0u8; (width * height * 4) as usize];
    let ok = {
        let s = state.borrow();
        let target = s.target.as_ref().unwrap();
        webgl_gpu::draw(
            &gpu,
            target,
            &DrawDesc {
                vertex_shader: &vertex_shader,
                fragment_shader: &fragment_shader,
                attribs: &attribs,
                buffers: &buffers,
                uniform_bytes: &uniform_bytes,
                texture: tex_upload.as_ref(),
                clear_color,
                first,
                count,
                index_buffer: index_buffer_view,
            },
            &mut rgba,
        )
    };
    if !ok {
        return Ok(JsValue::undefined());
    }

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
        ("UNSIGNED_INT", 0x1405),
        ("VERTEX_SHADER", 0x8B31),
        ("FRAGMENT_SHADER", 0x8B30),
        ("COMPILE_STATUS", 0x8B81),
        ("LINK_STATUS", 0x8B82),
        ("TEXTURE_2D", 0x0DE1),
        ("TEXTURE0", 0x84C0),
        ("TEXTURE1", 0x84C1),
        ("TEXTURE2", 0x84C2),
        ("TEXTURE3", 0x84C3),
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
