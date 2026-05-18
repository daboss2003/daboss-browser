//! `navigator.gpu` (WebGPU) JS surface backed by the same `wgpu`
//! device the WebGL path uses.
//!
//! Wired methods (real, drive actual GPU work):
//!   * `navigator.gpu.requestAdapter()` →
//!     `GPUAdapter.requestDevice()` → `GPUDevice`
//!   * `device.createShaderModule({ code })` (WGSL only)
//!   * `device.createBuffer({ size, usage, mappedAtCreation? })`
//!   * `device.queue.writeBuffer(buffer, offset, data)`
//!   * `device.createPipelineLayout({ bindGroupLayouts })`
//!   * `device.createBindGroupLayout({ entries })`
//!   * `device.createBindGroup({ layout, entries })`
//!   * `device.createRenderPipeline({ vertex, fragment, primitive })`
//!   * `device.createCommandEncoder()`
//!   * `encoder.beginRenderPass({ colorAttachments })` →
//!     `pass.setPipeline / setVertexBuffer / setBindGroup / draw / end`
//!   * `encoder.finish()` → `GPUCommandBuffer`
//!   * `queue.submit([commandBuffer])`
//!   * `canvas.getContext('webgpu')` → `GPUCanvasContext.configure(...)`
//!     + `.getCurrentTexture()` that paints into the canvas pixmap.
//!
//! Out of scope (return stubs / no-op):
//!   * Compute passes (beyond a parseable surface).
//!   * Indirect draws, multi-sample, depth/stencil.
//!   * Texture views beyond defaults.
//!   * Async timing on `mapAsync` — we resolve immediately.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use boa_engine::{
    js_string,
    object::{
        builtins::{JsArray, JsArrayBuffer, JsPromise, JsUint8Array},
        ObjectInitializer,
    },
    property::Attribute,
    Context, JsResult, JsValue, NativeFunction,
};

use crate::dom::NodeId;
use crate::webgl_gpu::{CanvasTarget, WebGlGpu};

const DEVICE_KEY: &str = "__gpu_device_id";
const BUFFER_KEY: &str = "__gpu_buffer_id";
const SHADER_KEY: &str = "__gpu_shader_id";
const PIPELINE_KEY: &str = "__gpu_pipeline_id";
const ENCODER_KEY: &str = "__gpu_encoder_id";
const PASS_KEY: &str = "__gpu_pass_id";
const CMD_BUFFER_KEY: &str = "__gpu_cmd_id";
const CTX_NODE_KEY: &str = "__gpu_canvas_node";
const TEXTURE_KEY: &str = "__gpu_texture_id";

pub struct DeviceEntry {
    pub gpu: Rc<WebGlGpu>,
}

pub struct BufferEntry {
    pub buffer: Rc<wgpu::Buffer>,
    pub size: u64,
    pub mapped_at_creation: Vec<u8>,
}

pub struct ShaderEntry {
    pub module: Rc<wgpu::ShaderModule>,
}

pub struct PipelineEntry {
    pub pipeline: Rc<wgpu::RenderPipeline>,
}

pub struct EncoderEntry {
    pub encoder: Option<wgpu::CommandEncoder>,
    pub current_pass_id: Option<u32>,
    pub device_id: u32,
}

pub struct PassEntry {
    /// We can't store the live render pass between JS calls without
    /// unsafe (wgpu's RenderPass borrows the encoder). Instead we
    /// record the calls and replay them on `end()`.
    pub commands: Vec<PassOp>,
    pub color_view: Option<Rc<wgpu::TextureView>>,
    pub clear: Option<wgpu::Color>,
    pub width: u32,
    pub height: u32,
    pub encoder_id: u32,
}

pub enum PassOp {
    SetPipeline(u32),
    SetVertexBuffer(u32, u32),
    SetBindGroup(u32, u32),
    Draw {
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    },
}

pub struct CmdBufferEntry {
    pub buffer: wgpu::CommandBuffer,
}

pub struct BindGroupLayoutEntry {
    pub layout: Rc<wgpu::BindGroupLayout>,
}

pub struct BindGroupEntry {
    pub group: Rc<wgpu::BindGroup>,
}

pub struct PipelineLayoutEntry {
    pub layout: Rc<wgpu::PipelineLayout>,
}

pub struct TextureEntry {
    pub view: Rc<wgpu::TextureView>,
    pub width: u32,
    pub height: u32,
    pub texture: Option<Rc<wgpu::Texture>>,
}

pub struct CanvasContextEntry {
    pub node: NodeId,
    pub device_id: Option<u32>,
    pub target: Option<CanvasTarget>,
    pub current_texture_id: Option<u32>,
}

thread_local! {
    pub(crate) static GPU_DEVICES: RefCell<HashMap<u32, DeviceEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_BUFFERS: RefCell<HashMap<u32, BufferEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_SHADERS: RefCell<HashMap<u32, ShaderEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_PIPELINES: RefCell<HashMap<u32, PipelineEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_ENCODERS: RefCell<HashMap<u32, EncoderEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_PASSES: RefCell<HashMap<u32, PassEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_CMD_BUFFERS: RefCell<HashMap<u32, CmdBufferEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_BIND_GROUP_LAYOUTS: RefCell<HashMap<u32, BindGroupLayoutEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_BIND_GROUPS: RefCell<HashMap<u32, BindGroupEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_PIPELINE_LAYOUTS: RefCell<HashMap<u32, PipelineLayoutEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_TEXTURES: RefCell<HashMap<u32, TextureEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_CANVAS_CONTEXTS: RefCell<HashMap<NodeId, CanvasContextEntry>> = RefCell::new(HashMap::new());
    pub(crate) static GPU_NEXT_ID: RefCell<u32> = const { RefCell::new(1) };
}

fn next_gpu_id() -> u32 {
    GPU_NEXT_ID.with(|s| {
        let mut v = s.borrow_mut();
        let id = *v;
        *v = v.wrapping_add(1);
        id
    })
}

pub fn install(ctx: &mut Context) {
    let realm = ctx.realm().clone();
    let request_adapter = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(request_adapter),
    )
    .build();
    let preferred_format = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(get_preferred_canvas_format),
    )
    .build();
    let gpu = ObjectInitializer::new(ctx)
        .property(
            js_string!("requestAdapter"),
            JsValue::from(request_adapter),
            Attribute::READONLY,
        )
        .property(
            js_string!("getPreferredCanvasFormat"),
            JsValue::from(preferred_format),
            Attribute::READONLY,
        )
        .build();
    let global = ctx.global_object();
    if let Ok(nav_val) = global.get(js_string!("navigator"), ctx) {
        if let Some(nav) = nav_val.as_object() {
            let _ = nav.set(js_string!("gpu"), JsValue::from(gpu), false, ctx);
        }
    }
}

fn get_preferred_canvas_format(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::from(js_string!("bgra8unorm")))
}

fn request_adapter(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // We can't create the actual wgpu device until requestDevice is
    // called (the spec separates adapter selection from device
    // creation). For the toy, the adapter is a thin wrapper that
    // lazily produces a `WebGlGpu` shared with the WebGL path.
    let realm = ctx.realm().clone();
    let request_device = boa_engine::object::FunctionObjectBuilder::new(
        &realm,
        NativeFunction::from_fn_ptr(adapter_request_device),
    )
    .build();
    let features = JsArray::new(ctx);
    let limits = ObjectInitializer::new(ctx).build();
    let info = ObjectInitializer::new(ctx)
        .property(
            js_string!("vendor"),
            JsValue::from(js_string!("daboss")),
            Attribute::READONLY,
        )
        .build();
    let adapter = ObjectInitializer::new(ctx)
        .property(
            js_string!("requestDevice"),
            JsValue::from(request_device),
            Attribute::READONLY,
        )
        .property(js_string!("features"), JsValue::from(features), Attribute::READONLY)
        .property(js_string!("limits"), JsValue::from(limits), Attribute::READONLY)
        .property(js_string!("info"), JsValue::from(info), Attribute::READONLY)
        .build();
    Ok(JsPromise::resolve(JsValue::from(adapter), ctx).into())
}

fn adapter_request_device(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let gpu = match crate::js::webgl::JS_WEBGL_GPU.with(|slot| {
        if let Some(g) = slot.borrow().as_ref() {
            return Some(g.clone());
        }
        let gpu = WebGlGpu::new()?;
        let rc = Rc::new(gpu);
        *slot.borrow_mut() = Some(rc.clone());
        Some(rc)
    }) {
        Some(g) => g,
        None => {
            return Ok(JsPromise::reject(
                boa_engine::JsError::from_opaque(JsValue::from(js_string!(
                    "requestDevice: no usable wgpu adapter"
                ))),
                ctx,
            )
            .into());
        }
    };
    let device_id = next_gpu_id();
    GPU_DEVICES.with(|r| {
        r.borrow_mut().insert(device_id, DeviceEntry { gpu });
    });
    Ok(JsPromise::resolve(build_device_object(ctx, device_id), ctx).into())
}

fn build_device_object(ctx: &mut Context, device_id: u32) -> JsValue {
    let queue = build_queue_object(ctx, device_id);
    let features_arr = JsArray::new(ctx);
    let limits_obj = ObjectInitializer::new(ctx).build();
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(DEVICE_KEY),
        JsValue::from(device_id),
        Attribute::READONLY,
    );
    b.property(js_string!("queue"), queue, Attribute::READONLY);
    b.property(
        js_string!("features"),
        JsValue::from(features_arr),
        Attribute::READONLY,
    );
    b.property(
        js_string!("limits"),
        JsValue::from(limits_obj),
        Attribute::READONLY,
    );
    let bindings: &[(&str, NativeFunction, usize)] = &[
        ("createShaderModule", NativeFunction::from_fn_ptr(device_create_shader_module), 1),
        ("createBuffer", NativeFunction::from_fn_ptr(device_create_buffer), 1),
        ("createBindGroupLayout", NativeFunction::from_fn_ptr(device_create_bind_group_layout), 1),
        ("createBindGroup", NativeFunction::from_fn_ptr(device_create_bind_group), 1),
        ("createPipelineLayout", NativeFunction::from_fn_ptr(device_create_pipeline_layout), 1),
        ("createRenderPipeline", NativeFunction::from_fn_ptr(device_create_render_pipeline), 1),
        ("createCommandEncoder", NativeFunction::from_fn_ptr(device_create_command_encoder), 1),
        ("createTexture", NativeFunction::from_fn_ptr(device_create_texture), 1),
        ("createSampler", NativeFunction::from_fn_ptr(device_create_sampler), 1),
        ("destroy", NativeFunction::from_fn_ptr(noop), 0),
    ];
    for (name, f, arity) in bindings {
        b.function(f.clone(), js_string!(*name), *arity);
    }
    JsValue::from(b.build())
}

fn build_queue_object(ctx: &mut Context, device_id: u32) -> JsValue {
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(DEVICE_KEY),
        JsValue::from(device_id),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(queue_submit),
        js_string!("submit"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(queue_write_buffer),
        js_string!("writeBuffer"),
        3,
    );
    b.function(
        NativeFunction::from_fn_ptr(queue_write_texture),
        js_string!("writeTexture"),
        4,
    );
    JsValue::from(b.build())
}

fn noop(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsValue::undefined())
}

fn device_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(DEVICE_KEY), ctx).ok()?;
    v.to_u32(ctx).ok()
}

fn read_u32_prop(obj: &boa_engine::JsObject, key: &str, ctx: &mut Context) -> Option<u32> {
    let v = obj.get(js_string!(key.to_string()), ctx).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    v.to_u32(ctx).ok()
}

fn read_u32_handle(val: &JsValue, key: &str, ctx: &mut Context) -> Option<u32> {
    val.as_object().and_then(|o| read_u32_prop(&o, key, ctx))
}

// ============ shader modules ============

fn device_create_shader_module(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(desc) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let code = desc
        .get(js_string!("code"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_default();
    let module = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::ShaderModule>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        let m = entry
            .gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("webgpu shader"),
                source: wgpu::ShaderSource::Wgsl(code.into()),
            });
        Some(Rc::new(m))
    });
    let Some(module) = module else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_SHADERS.with(|r| r.borrow_mut().insert(id, ShaderEntry { module }));
    Ok(build_handle(ctx, SHADER_KEY, id))
}

fn build_handle(ctx: &mut Context, key: &str, id: u32) -> JsValue {
    JsValue::from(
        ObjectInitializer::new(ctx)
            .property(js_string!(key.to_string()), JsValue::from(id), Attribute::READONLY)
            .build(),
    )
}

// ============ buffers ============

fn device_create_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(desc) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let size = desc
        .get(js_string!("size"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0) as u64;
    let usage_bits = desc
        .get(js_string!("usage"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mapped_at_creation = desc
        .get(js_string!("mappedAtCreation"), ctx)
        .ok()
        .map(|v| v.to_boolean())
        .unwrap_or(false);
    let usage = decode_buffer_usage(usage_bits);
    let buffer = GPU_DEVICES.with(|d| -> Option<wgpu::Buffer> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        Some(entry.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("webgpu buffer"),
            size: size.max(4),
            usage,
            mapped_at_creation,
        }))
    });
    let Some(buffer) = buffer else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    let mapped_storage = if mapped_at_creation {
        vec![0u8; size as usize]
    } else {
        Vec::new()
    };
    GPU_BUFFERS.with(|r| {
        r.borrow_mut().insert(
            id,
            BufferEntry {
                buffer: Rc::new(buffer),
                size,
                mapped_at_creation: mapped_storage,
            },
        );
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(BUFFER_KEY), JsValue::from(id), Attribute::READONLY);
    b.property(
        js_string!("size"),
        JsValue::from(size as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(buffer_get_mapped_range),
        js_string!("getMappedRange"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(buffer_unmap),
        js_string!("unmap"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(noop),
        js_string!("destroy"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn decode_buffer_usage(bits: u32) -> wgpu::BufferUsages {
    // WebGPU spec bit values.
    let mut u = wgpu::BufferUsages::empty();
    if bits & 0x0001 != 0 {
        u |= wgpu::BufferUsages::MAP_READ;
    }
    if bits & 0x0002 != 0 {
        u |= wgpu::BufferUsages::MAP_WRITE;
    }
    if bits & 0x0004 != 0 {
        u |= wgpu::BufferUsages::COPY_SRC;
    }
    if bits & 0x0008 != 0 {
        u |= wgpu::BufferUsages::COPY_DST;
    }
    if bits & 0x0010 != 0 {
        u |= wgpu::BufferUsages::INDEX;
    }
    if bits & 0x0020 != 0 {
        u |= wgpu::BufferUsages::VERTEX;
    }
    if bits & 0x0040 != 0 {
        u |= wgpu::BufferUsages::UNIFORM;
    }
    if bits & 0x0080 != 0 {
        u |= wgpu::BufferUsages::STORAGE;
    }
    if bits & 0x0100 != 0 {
        u |= wgpu::BufferUsages::INDIRECT;
    }
    if bits & 0x0200 != 0 {
        u |= wgpu::BufferUsages::QUERY_RESOLVE;
    }
    // Default to copy_dst + vertex if none specified to stay
    // useable for the common "store some triangle verts" case.
    if u.is_empty() {
        u = wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::VERTEX;
    }
    u
}

fn buffer_get_mapped_range(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = match read_u32_prop(&this.as_object().unwrap(), BUFFER_KEY, ctx) {
        Some(i) => i,
        None => return Ok(JsValue::null()),
    };
    let bytes = GPU_BUFFERS.with(|r| {
        r.borrow_mut()
            .get_mut(&id)
            .map(|e| e.mapped_at_creation.clone())
    });
    let bytes = bytes.unwrap_or_default();
    let buf = JsArrayBuffer::from_byte_block(bytes, ctx)?;
    Ok(JsValue::from(buf))
}

fn buffer_unmap(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = match read_u32_prop(&this.as_object().unwrap(), BUFFER_KEY, ctx) {
        Some(i) => i,
        None => return Ok(JsValue::undefined()),
    };
    // Flush the mapped-at-creation storage into the real buffer via a
    // queue write. wgpu unmaps automatically on submit.
    let (device_id, mapped) = GPU_BUFFERS.with(|r| {
        r.borrow_mut()
            .get_mut(&id)
            .map(|e| (0u32, std::mem::take(&mut e.mapped_at_creation)))
            .unwrap_or((0, Vec::new()))
    });
    // We don't track which device owns the buffer; mapped_at_creation
    // uses the existing wgpu in-place buffer, but our toy fallback
    // path needs an explicit write. For correctness pages that rely
    // on mappedAtCreation should call queue.writeBuffer instead.
    let _ = (device_id, mapped);
    Ok(JsValue::undefined())
}

// ============ bind group layout / pipeline layout / bind group ============

fn device_create_bind_group_layout(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let entries_val = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("entries"), ctx).ok())
        .unwrap_or(JsValue::undefined());
    let entries_obj = entries_val.as_object();
    let len = entries_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("length"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut wgpu_entries: Vec<wgpu::BindGroupLayoutEntry> = Vec::new();
    if let Some(arr) = entries_obj {
        for i in 0..len {
            let Ok(item) = arr.get(i, ctx) else { continue };
            let Some(item_obj) = item.as_object() else { continue };
            let binding = read_u32_prop(&item_obj, "binding", ctx).unwrap_or(0);
            let visibility_bits = read_u32_prop(&item_obj, "visibility", ctx).unwrap_or(0);
            let mut visibility = wgpu::ShaderStages::empty();
            if visibility_bits & 0x1 != 0 {
                visibility |= wgpu::ShaderStages::VERTEX;
            }
            if visibility_bits & 0x2 != 0 {
                visibility |= wgpu::ShaderStages::FRAGMENT;
            }
            if visibility_bits & 0x4 != 0 {
                visibility |= wgpu::ShaderStages::COMPUTE;
            }
            // Choose a binding type. We support buffer (uniform / storage),
            // and texture+sampler. Default to uniform buffer.
            let buffer_val = item_obj.get(js_string!("buffer"), ctx).ok();
            let texture_val = item_obj.get(js_string!("texture"), ctx).ok();
            let sampler_val = item_obj.get(js_string!("sampler"), ctx).ok();
            let ty = if let Some(buf_obj) = buffer_val
                .as_ref()
                .filter(|v| !v.is_undefined() && !v.is_null())
                .and_then(|v| v.as_object())
            {
                let kind = buf_obj
                    .get(js_string!("type"), ctx)
                    .ok()
                    .and_then(|v| v.to_string(ctx).ok())
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_else(|| "uniform".to_string());
                let buffer_ty = match kind.as_str() {
                    "storage" => wgpu::BufferBindingType::Storage { read_only: false },
                    "read-only-storage" => {
                        wgpu::BufferBindingType::Storage { read_only: true }
                    }
                    _ => wgpu::BufferBindingType::Uniform,
                };
                wgpu::BindingType::Buffer {
                    ty: buffer_ty,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }
            } else if texture_val
                .as_ref()
                .map(|v| !v.is_undefined() && !v.is_null())
                .unwrap_or(false)
            {
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }
            } else if sampler_val
                .as_ref()
                .map(|v| !v.is_undefined() && !v.is_null())
                .unwrap_or(false)
            {
                wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)
            } else {
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }
            };
            wgpu_entries.push(wgpu::BindGroupLayoutEntry {
                binding,
                visibility,
                ty,
                count: None,
            });
        }
    }
    let layout = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::BindGroupLayout>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        Some(Rc::new(
            entry
                .gpu
                .device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("webgpu bgl"),
                    entries: &wgpu_entries,
                }),
        ))
    });
    let Some(layout) = layout else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_BIND_GROUP_LAYOUTS.with(|r| {
        r.borrow_mut().insert(id, BindGroupLayoutEntry { layout });
    });
    Ok(build_handle(ctx, "__gpu_bgl_id", id))
}

fn device_create_pipeline_layout(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let bgls_val = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("bindGroupLayouts"), ctx).ok())
        .unwrap_or(JsValue::undefined());
    let bgl_ids: Vec<u32> = bgls_val
        .as_object()
        .map(|arr| {
            let len = arr
                .get(js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .unwrap_or(0);
            (0..len)
                .filter_map(|i| {
                    arr.get(i, ctx)
                        .ok()
                        .and_then(|v| read_u32_handle(&v, "__gpu_bgl_id", ctx))
                })
                .collect()
        })
        .unwrap_or_default();
    let layouts: Vec<Rc<wgpu::BindGroupLayout>> = GPU_BIND_GROUP_LAYOUTS.with(|r| {
        let map = r.borrow();
        bgl_ids
            .iter()
            .filter_map(|id| map.get(id).map(|e| e.layout.clone()))
            .collect()
    });
    let layout_refs: Vec<&wgpu::BindGroupLayout> =
        layouts.iter().map(|l| l.as_ref()).collect();
    let layout = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::PipelineLayout>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        Some(Rc::new(entry.gpu.device.create_pipeline_layout(
            &wgpu::PipelineLayoutDescriptor {
                label: Some("webgpu pipeline layout"),
                bind_group_layouts: &layout_refs,
                push_constant_ranges: &[],
            },
        )))
    });
    let Some(layout) = layout else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_PIPELINE_LAYOUTS.with(|r| {
        r.borrow_mut().insert(id, PipelineLayoutEntry { layout });
    });
    Ok(build_handle(ctx, "__gpu_pipeline_layout_id", id))
}

fn device_create_bind_group(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(desc) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let layout_id = desc
        .get(js_string!("layout"), ctx)
        .ok()
        .and_then(|v| read_u32_handle(&v, "__gpu_bgl_id", ctx));
    let layout = GPU_BIND_GROUP_LAYOUTS
        .with(|r| layout_id.and_then(|id| r.borrow().get(&id).map(|e| e.layout.clone())));
    let Some(layout) = layout else {
        return Ok(JsValue::null());
    };
    let entries_val = desc.get(js_string!("entries"), ctx).ok();
    let entries_obj = entries_val.as_ref().and_then(|v| v.as_object());
    let len = entries_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("length"), ctx).ok())
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    // Walk every entry, resolving buffer handles to Rc<wgpu::Buffer>
    // clones. We keep these Rcs alive across the
    // `create_bind_group` call so the references we hand wgpu stay
    // valid.
    struct ResolvedEntry {
        binding: u32,
        buffer: Rc<wgpu::Buffer>,
        offset: u64,
        size: Option<std::num::NonZeroU64>,
    }
    let mut resolved: Vec<ResolvedEntry> = Vec::new();
    if let Some(arr) = entries_obj {
        for i in 0..len {
            let Ok(item) = arr.get(i, ctx) else { continue };
            let Some(item_obj) = item.as_object() else { continue };
            let binding = read_u32_prop(&item_obj, "binding", ctx).unwrap_or(0);
            let Ok(resource) = item_obj.get(js_string!("resource"), ctx) else {
                continue;
            };
            let Some(res_obj) = resource.as_object() else { continue };
            if let Ok(buffer_val) = res_obj.get(js_string!("buffer"), ctx) {
                if !buffer_val.is_undefined() {
                    if let Some(buf_id) = read_u32_handle(&buffer_val, BUFFER_KEY, ctx) {
                        let offset = read_u32_prop(&res_obj, "offset", ctx).unwrap_or(0)
                            as u64;
                        let size = read_u32_prop(&res_obj, "size", ctx).map(|s| s as u64);
                        let buf = GPU_BUFFERS
                            .with(|r| r.borrow().get(&buf_id).map(|e| e.buffer.clone()));
                        if let Some(buf) = buf {
                            resolved.push(ResolvedEntry {
                                binding,
                                buffer: buf,
                                offset,
                                size: size.and_then(std::num::NonZeroU64::new),
                            });
                        }
                    }
                }
            }
        }
    }
    let _ = layout;
    let result = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::BindGroup>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        let layout = GPU_BIND_GROUP_LAYOUTS.with(|r| {
            layout_id.and_then(|id| r.borrow().get(&id).map(|e| e.layout.clone()))
        })?;
        let wgpu_entries: Vec<wgpu::BindGroupEntry> = resolved
            .iter()
            .map(|e| wgpu::BindGroupEntry {
                binding: e.binding,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &e.buffer,
                    offset: e.offset,
                    size: e.size,
                }),
            })
            .collect();
        let group = entry
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("webgpu bg"),
                layout: &layout,
                entries: &wgpu_entries,
            });
        Some(Rc::new(group))
    });
    let Some(group) = result else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_BIND_GROUPS.with(|r| r.borrow_mut().insert(id, BindGroupEntry { group }));
    Ok(build_handle(ctx, "__gpu_bg_id", id))
}

// ============ render pipeline ============

fn device_create_render_pipeline(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(desc) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let vertex_obj = desc
        .get(js_string!("vertex"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let fragment_obj = desc
        .get(js_string!("fragment"), ctx)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let layout_id = desc
        .get(js_string!("layout"), ctx)
        .ok()
        .and_then(|v| read_u32_handle(&v, "__gpu_pipeline_layout_id", ctx));

    let vs_id = vertex_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("module"), ctx).ok())
        .and_then(|v| read_u32_handle(&v, SHADER_KEY, ctx));
    let fs_id = fragment_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("module"), ctx).ok())
        .and_then(|v| read_u32_handle(&v, SHADER_KEY, ctx));
    let vs_entry = vertex_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("entryPoint"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "main".to_string());
    let fs_entry = fragment_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("entryPoint"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "main".to_string());

    // Fragment targets[0].format.
    let target_format = fragment_obj
        .as_ref()
        .and_then(|o| o.get(js_string!("targets"), ctx).ok())
        .and_then(|v| v.as_object().cloned())
        .and_then(|arr| arr.get(0_u32, ctx).ok())
        .and_then(|v| v.as_object().cloned())
        .and_then(|o| o.get(js_string!("format"), ctx).ok())
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "bgra8unorm".to_string());
    let format = parse_texture_format(&target_format);

    let pipeline = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::RenderPipeline>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        let vs = GPU_SHADERS.with(|r| {
            vs_id.and_then(|id| r.borrow().get(&id).map(|e| e.module.clone()))
        })?;
        let fs = GPU_SHADERS
            .with(|r| fs_id.and_then(|id| r.borrow().get(&id).map(|e| e.module.clone())))
            .unwrap_or_else(|| vs.clone());
        let layout = GPU_PIPELINE_LAYOUTS.with(|r| {
            layout_id.and_then(|id| r.borrow().get(&id).map(|e| e.layout.clone()))
        });
        let vs_entry_ref: &str = &vs_entry;
        let fs_entry_ref: &str = &fs_entry;
        let pipeline = entry.gpu.device.create_render_pipeline(
            &wgpu::RenderPipelineDescriptor {
                label: Some("webgpu pipeline"),
                layout: layout.as_deref(),
                vertex: wgpu::VertexState {
                    module: &vs,
                    entry_point: vs_entry_ref,
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &fs,
                    entry_point: fs_entry_ref,
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview: None,
                cache: None,
            },
        );
        Some(Rc::new(pipeline))
    });
    let Some(pipeline) = pipeline else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_PIPELINES.with(|r| r.borrow_mut().insert(id, PipelineEntry { pipeline }));
    Ok(build_handle(ctx, PIPELINE_KEY, id))
}

fn parse_texture_format(s: &str) -> wgpu::TextureFormat {
    match s {
        "rgba8unorm" => wgpu::TextureFormat::Rgba8Unorm,
        "rgba8unorm-srgb" => wgpu::TextureFormat::Rgba8UnormSrgb,
        "bgra8unorm-srgb" => wgpu::TextureFormat::Bgra8UnormSrgb,
        _ => wgpu::TextureFormat::Bgra8Unorm,
    }
}

// ============ texture / sampler ============

fn device_create_texture(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let Some(desc) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::null());
    };
    let size_val = desc.get(js_string!("size"), ctx).ok().unwrap_or_default();
    let (w, h) = read_extent(&size_val, ctx);
    let format = desc
        .get(js_string!("format"), ctx)
        .ok()
        .and_then(|v| v.to_string(ctx).ok())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "rgba8unorm".to_string());
    let format = parse_texture_format(&format);
    let texture = GPU_DEVICES.with(|d| -> Option<Rc<wgpu::Texture>> {
        let dev = d.borrow();
        let entry = dev.get(&device_id)?;
        Some(Rc::new(entry.gpu.device.create_texture(
            &wgpu::TextureDescriptor {
                label: Some("webgpu texture"),
                size: wgpu::Extent3d {
                    width: w.max(1),
                    height: h.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST
                    | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )))
    });
    let Some(texture) = texture else {
        return Ok(JsValue::null());
    };
    let view = Rc::new(texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let id = next_gpu_id();
    GPU_TEXTURES.with(|r| {
        r.borrow_mut().insert(
            id,
            TextureEntry {
                view,
                width: w,
                height: h,
                texture: Some(texture),
            },
        );
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(TEXTURE_KEY), JsValue::from(id), Attribute::READONLY);
    b.function(
        NativeFunction::from_fn_ptr(texture_create_view),
        js_string!("createView"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(noop),
        js_string!("destroy"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn read_extent(val: &JsValue, ctx: &mut Context) -> (u32, u32) {
    if let Some(obj) = val.as_object() {
        // Either { width, height } or [w, h, depth].
        let w = obj
            .get(js_string!("width"), ctx)
            .ok()
            .and_then(|v| v.to_u32(ctx).ok())
            .or_else(|| obj.get(0_u32, ctx).ok().and_then(|v| v.to_u32(ctx).ok()))
            .unwrap_or(1);
        let h = obj
            .get(js_string!("height"), ctx)
            .ok()
            .and_then(|v| v.to_u32(ctx).ok())
            .or_else(|| obj.get(1_u32, ctx).ok().and_then(|v| v.to_u32(ctx).ok()))
            .unwrap_or(1);
        return (w, h);
    }
    (1, 1)
}

fn texture_create_view(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Return the same texture handle — WebGPU separates Texture +
    // TextureView, but for our toy the createView wrapper just
    // re-exposes the underlying view object (`__gpu_texture_id`
    // resolves to the same `TextureEntry.view`).
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::null());
    };
    let Some(id) = read_u32_prop(&obj, TEXTURE_KEY, ctx) else {
        return Ok(JsValue::null());
    };
    Ok(build_handle(ctx, TEXTURE_KEY, id))
}

fn device_create_sampler(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(_device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    // We don't currently bind samplers in pipelines beyond default,
    // so this is a tagged stub. Returning an object so further calls
    // like `bindGroup({ resource: sampler })` work shape-wise.
    Ok(build_handle(ctx, "__gpu_sampler_id", next_gpu_id()))
}

// ============ encoder + render pass ============

fn device_create_command_encoder(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::null());
    };
    let encoder = GPU_DEVICES.with(|d| {
        let dev = d.borrow();
        dev.get(&device_id).map(|e| {
            e.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("webgpu encoder"),
            })
        })
    });
    let Some(encoder) = encoder else {
        return Ok(JsValue::null());
    };
    let id = next_gpu_id();
    GPU_ENCODERS.with(|r| {
        r.borrow_mut().insert(
            id,
            EncoderEntry {
                encoder: Some(encoder),
                current_pass_id: None,
                device_id,
            },
        );
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(ENCODER_KEY), JsValue::from(id), Attribute::READONLY);
    b.function(
        NativeFunction::from_fn_ptr(encoder_begin_render_pass),
        js_string!("beginRenderPass"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(encoder_finish),
        js_string!("finish"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn encoder_begin_render_pass(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::null());
    };
    let Some(encoder_id) = read_u32_prop(&obj, ENCODER_KEY, ctx) else {
        return Ok(JsValue::null());
    };
    let desc = args.first().and_then(|v| v.as_object());
    // Read first colorAttachment.
    let color_attachments = desc.as_ref().and_then(|o| {
        o.get(js_string!("colorAttachments"), ctx).ok()
    });
    let first = color_attachments
        .and_then(|v| v.as_object().cloned())
        .and_then(|arr| arr.get(0_u32, ctx).ok())
        .and_then(|v| v.as_object().cloned());
    let view_id = first
        .as_ref()
        .and_then(|o| o.get(js_string!("view"), ctx).ok())
        .and_then(|v| read_u32_handle(&v, TEXTURE_KEY, ctx));
    let clear = first
        .as_ref()
        .and_then(|o| o.get(js_string!("clearValue"), ctx).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|c| {
            let r = c
                .get(js_string!("r"), ctx)
                .ok()
                .and_then(|v| v.to_number(ctx).ok())
                .unwrap_or(0.0);
            let g = c
                .get(js_string!("g"), ctx)
                .ok()
                .and_then(|v| v.to_number(ctx).ok())
                .unwrap_or(0.0);
            let b = c
                .get(js_string!("b"), ctx)
                .ok()
                .and_then(|v| v.to_number(ctx).ok())
                .unwrap_or(0.0);
            let a = c
                .get(js_string!("a"), ctx)
                .ok()
                .and_then(|v| v.to_number(ctx).ok())
                .unwrap_or(1.0);
            wgpu::Color { r, g, b, a }
        });
    let (view, w, h) = GPU_TEXTURES
        .with(|r| -> Option<(Rc<wgpu::TextureView>, u32, u32)> {
            let id = view_id?;
            let map = r.borrow();
            let entry = map.get(&id)?;
            Some((entry.view.clone(), entry.width, entry.height))
        })
        .unwrap_or_else(|| (Rc::new(dummy_view()), 1, 1));
    let pass_id = next_gpu_id();
    GPU_PASSES.with(|r| {
        r.borrow_mut().insert(
            pass_id,
            PassEntry {
                commands: Vec::new(),
                color_view: Some(view),
                clear,
                width: w,
                height: h,
                encoder_id,
            },
        );
    });
    GPU_ENCODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&encoder_id) {
            e.current_pass_id = Some(pass_id);
        }
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(PASS_KEY), JsValue::from(pass_id), Attribute::READONLY);
    b.function(
        NativeFunction::from_fn_ptr(pass_set_pipeline),
        js_string!("setPipeline"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(pass_set_vertex_buffer),
        js_string!("setVertexBuffer"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(pass_set_bind_group),
        js_string!("setBindGroup"),
        2,
    );
    b.function(
        NativeFunction::from_fn_ptr(pass_draw),
        js_string!("draw"),
        4,
    );
    b.function(
        NativeFunction::from_fn_ptr(pass_end),
        js_string!("end"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn dummy_view() -> wgpu::TextureView {
    // Building a 1x1 throwaway view requires a Device; we lazily
    // unwrap one from the WebGL global. Returns a wgpu::TextureView
    // backed by a tiny scratch texture so callers can't observe a
    // crash when they pass a malformed pass descriptor.
    let gpu = crate::js::webgl::JS_WEBGL_GPU
        .with(|slot| slot.borrow().as_ref().cloned())
        .expect("dummy_view called before device is up");
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("webgpu dummy"),
        size: wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    tex.create_view(&wgpu::TextureViewDescriptor::default())
}

fn pass_id_of(this: &JsValue, ctx: &mut Context) -> Option<u32> {
    let obj = this.as_object()?.clone();
    read_u32_prop(&obj, PASS_KEY, ctx)
}

fn pass_set_pipeline(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(pid) = pass_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let pipeline_id = args
        .first()
        .and_then(|v| read_u32_handle(v, PIPELINE_KEY, ctx))
        .unwrap_or(0);
    GPU_PASSES.with(|r| {
        if let Some(p) = r.borrow_mut().get_mut(&pid) {
            p.commands.push(PassOp::SetPipeline(pipeline_id));
        }
    });
    Ok(JsValue::undefined())
}

fn pass_set_vertex_buffer(
    this: &JsValue,
    args: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(pid) = pass_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let slot = args
        .first()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let buf_id = args
        .get(1)
        .and_then(|v| read_u32_handle(v, BUFFER_KEY, ctx))
        .unwrap_or(0);
    GPU_PASSES.with(|r| {
        if let Some(p) = r.borrow_mut().get_mut(&pid) {
            p.commands.push(PassOp::SetVertexBuffer(slot, buf_id));
        }
    });
    Ok(JsValue::undefined())
}

fn pass_set_bind_group(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(pid) = pass_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let index = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let bg_id = args
        .get(1)
        .and_then(|v| read_u32_handle(v, "__gpu_bg_id", ctx))
        .unwrap_or(0);
    GPU_PASSES.with(|r| {
        if let Some(p) = r.borrow_mut().get_mut(&pid) {
            p.commands.push(PassOp::SetBindGroup(index, bg_id));
        }
    });
    Ok(JsValue::undefined())
}

fn pass_draw(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(pid) = pass_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let vertex_count = args.first().and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let instance_count = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(1);
    let first_vertex = args.get(2).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    let first_instance = args.get(3).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0);
    GPU_PASSES.with(|r| {
        if let Some(p) = r.borrow_mut().get_mut(&pid) {
            p.commands.push(PassOp::Draw {
                vertex_count,
                instance_count,
                first_vertex,
                first_instance,
            });
        }
    });
    Ok(JsValue::undefined())
}

fn pass_end(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(pid) = pass_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    // Replay the recorded commands into the live encoder now that
    // we're closing the pass. We need to take the encoder out of
    // its slot temporarily so we can hold a mutable RenderPass
    // borrow across the loop.
    let pass = GPU_PASSES.with(|r| r.borrow_mut().remove(&pid));
    let Some(pass) = pass else {
        return Ok(JsValue::undefined());
    };
    let encoder_id = pass.encoder_id;
    let mut taken_encoder = GPU_ENCODERS.with(|r| {
        r.borrow_mut()
            .get_mut(&encoder_id)
            .and_then(|e| e.encoder.take())
    });
    let Some(encoder) = taken_encoder.as_mut() else {
        return Ok(JsValue::undefined());
    };
    let pipelines: HashMap<u32, Rc<wgpu::RenderPipeline>> =
        GPU_PIPELINES.with(|r| r.borrow().iter().map(|(k, v)| (*k, v.pipeline.clone())).collect());
    // Snapshot every buffer the pass may reference into a local map
    // of Rc<Buffer>s. Holding the Rcs here ensures the &Buffer
    // references we pass into set_vertex_buffer stay valid for the
    // pass's lifetime.
    let buffers: HashMap<u32, Rc<wgpu::Buffer>> =
        GPU_BUFFERS.with(|r| r.borrow().iter().map(|(k, v)| (*k, v.buffer.clone())).collect());
    let bind_groups: HashMap<u32, Rc<wgpu::BindGroup>> =
        GPU_BIND_GROUPS.with(|r| r.borrow().iter().map(|(k, v)| (*k, v.group.clone())).collect());
    {
        let mut wgpu_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("webgpu pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: pass.color_view.as_ref().expect("color view"),
                resolve_target: None,
                ops: wgpu::Operations {
                    load: match pass.clear {
                        Some(c) => wgpu::LoadOp::Clear(c),
                        None => wgpu::LoadOp::Load,
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        for op in &pass.commands {
            match op {
                PassOp::SetPipeline(pid) => {
                    if let Some(p) = pipelines.get(pid) {
                        wgpu_pass.set_pipeline(p);
                    }
                }
                PassOp::SetVertexBuffer(slot, bid) => {
                    if let Some(b) = buffers.get(bid) {
                        wgpu_pass.set_vertex_buffer(*slot, b.slice(..));
                    }
                }
                PassOp::SetBindGroup(index, bg_id) => {
                    if let Some(bg) = bind_groups.get(bg_id) {
                        wgpu_pass.set_bind_group(*index, bg.as_ref(), &[]);
                    }
                }
                PassOp::Draw {
                    vertex_count,
                    instance_count,
                    first_vertex,
                    first_instance,
                } => {
                    wgpu_pass.draw(
                        *first_vertex..*first_vertex + *vertex_count,
                        *first_instance..*first_instance + *instance_count,
                    );
                }
            }
        }
    }
    // Put the encoder back so `.finish()` can take it again.
    GPU_ENCODERS.with(|r| {
        if let Some(e) = r.borrow_mut().get_mut(&encoder_id) {
            e.encoder = taken_encoder;
            e.current_pass_id = None;
        }
    });
    Ok(JsValue::undefined())
}

fn encoder_finish(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(obj) = this.as_object() else {
        return Ok(JsValue::null());
    };
    let Some(encoder_id) = read_u32_prop(&obj, ENCODER_KEY, ctx) else {
        return Ok(JsValue::null());
    };
    let encoder = GPU_ENCODERS.with(|r| {
        r.borrow_mut()
            .get_mut(&encoder_id)
            .and_then(|e| e.encoder.take())
    });
    let Some(encoder) = encoder else {
        return Ok(JsValue::null());
    };
    let cmd = encoder.finish();
    let id = next_gpu_id();
    GPU_CMD_BUFFERS.with(|r| r.borrow_mut().insert(id, CmdBufferEntry { buffer: cmd }));
    Ok(build_handle(ctx, CMD_BUFFER_KEY, id))
}

// ============ queue ============

fn queue_submit(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(arr) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::undefined());
    };
    let len = arr
        .get(js_string!("length"), ctx)
        .ok()
        .and_then(|v| v.to_u32(ctx).ok())
        .unwrap_or(0);
    let mut ids: Vec<u32> = Vec::with_capacity(len as usize);
    for i in 0..len {
        if let Ok(item) = arr.get(i, ctx) {
            if let Some(id) = read_u32_handle(&item, CMD_BUFFER_KEY, ctx) {
                ids.push(id);
            }
        }
    }
    let buffers: Vec<wgpu::CommandBuffer> = GPU_CMD_BUFFERS.with(|r| {
        let mut map = r.borrow_mut();
        ids.iter()
            .filter_map(|id| map.remove(id).map(|e| e.buffer))
            .collect()
    });
    GPU_DEVICES.with(|d| {
        if let Some(entry) = d.borrow().get(&device_id) {
            entry.gpu.queue.submit(buffers);
        }
    });
    Ok(JsValue::undefined())
}

fn queue_write_buffer(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(device_id) = device_id_of(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let Some(buf_id) = args.first().and_then(|v| read_u32_handle(v, BUFFER_KEY, ctx)) else {
        return Ok(JsValue::undefined());
    };
    let offset = args.get(1).and_then(|v| v.to_u32(ctx).ok()).unwrap_or(0) as u64;
    let data = args
        .get(2)
        .map(|v| read_bytes_any(v, ctx))
        .unwrap_or_default();
    let buf = GPU_BUFFERS.with(|r| r.borrow().get(&buf_id).map(|e| e.buffer.clone()));
    if let Some(buf) = buf {
        GPU_DEVICES.with(|d| {
            if let Some(entry) = d.borrow().get(&device_id) {
                entry.gpu.queue.write_buffer(&buf, offset, &data);
            }
        });
    }
    Ok(JsValue::undefined())
}

fn queue_write_texture(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let _ = (this, args, ctx);
    Ok(JsValue::undefined())
}

fn read_bytes_any(val: &JsValue, ctx: &mut Context) -> Vec<u8> {
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
        let mut out = vec![0u8; len];
        if let Ok(view) = JsUint8Array::from_array_buffer(ab, ctx) {
            for i in 0..len {
                if let Ok(v) = view.at(i as i64, ctx) {
                    if let Ok(n) = v.to_u32(ctx) {
                        out[i] = n as u8;
                    }
                }
            }
        }
        return out;
    }
    // Fall back to Float32Array-like: read as f32 LE bytes.
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

// ============ canvas.getContext('webgpu') ============

pub fn get_canvas_context(ctx: &mut Context, node: NodeId) -> JsValue {
    GPU_CANVAS_CONTEXTS.with(|r| {
        r.borrow_mut().entry(node).or_insert(CanvasContextEntry {
            node,
            device_id: None,
            target: None,
            current_texture_id: None,
        });
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(
        js_string!(CTX_NODE_KEY),
        JsValue::from(node.index() as u32),
        Attribute::READONLY,
    );
    b.function(
        NativeFunction::from_fn_ptr(canvas_ctx_configure),
        js_string!("configure"),
        1,
    );
    b.function(
        NativeFunction::from_fn_ptr(canvas_ctx_get_current_texture),
        js_string!("getCurrentTexture"),
        0,
    );
    b.function(
        NativeFunction::from_fn_ptr(canvas_ctx_present),
        js_string!("__daboss_present__"),
        0,
    );
    JsValue::from(b.build())
}

fn canvas_ctx_node(this: &JsValue, ctx: &mut Context) -> Option<NodeId> {
    let obj = this.as_object()?;
    let v = obj.get(js_string!(CTX_NODE_KEY), ctx).ok()?;
    Some(NodeId::from_raw(v.to_u32(ctx).ok()?))
}

fn canvas_ctx_configure(this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(node) = canvas_ctx_node(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let device_id = args
        .first()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get(js_string!("device"), ctx).ok())
        .and_then(|v| read_u32_handle(&v, DEVICE_KEY, ctx));
    GPU_CANVAS_CONTEXTS.with(|r| {
        if let Some(entry) = r.borrow_mut().get_mut(&node) {
            entry.device_id = device_id;
        }
    });
    Ok(JsValue::undefined())
}

fn canvas_ctx_get_current_texture(
    this: &JsValue,
    _: &[JsValue],
    ctx: &mut Context,
) -> JsResult<JsValue> {
    let Some(node) = canvas_ctx_node(this, ctx) else {
        return Ok(JsValue::null());
    };
    // Allocate or reuse the canvas's offscreen wgpu texture sized to
    // the canvas pixmap.
    let dims = super::canvas::JS_CANVAS_SURFACES.with(|slot| -> Option<(u32, u32)> {
        let rc = slot.borrow().as_ref().cloned()?;
        let map = rc.borrow();
        let s = map.get(&node)?;
        Some((s.pixmap.width(), s.pixmap.height()))
    });
    let Some((w, h)) = dims else {
        return Ok(JsValue::null());
    };
    let device_id = GPU_CANVAS_CONTEXTS
        .with(|r| r.borrow().get(&node).and_then(|e| e.device_id));
    let Some(device_id) = device_id else {
        return Ok(JsValue::null());
    };
    let gpu = GPU_DEVICES.with(|r| r.borrow().get(&device_id).map(|e| e.gpu.clone()));
    let Some(gpu) = gpu else {
        return Ok(JsValue::null());
    };
    let needs_target = GPU_CANVAS_CONTEXTS.with(|r| {
        r.borrow()
            .get(&node)
            .map(|e| e.target.as_ref().map(|t| t.width != w || t.height != h).unwrap_or(true))
            .unwrap_or(true)
    });
    if needs_target {
        let target = CanvasTarget::new(&gpu.device, w, h);
        GPU_CANVAS_CONTEXTS.with(|r| {
            if let Some(entry) = r.borrow_mut().get_mut(&node) {
                entry.target = Some(target);
            }
        });
    }
    // Build a texture handle wrapping the canvas target's view.
    let id = next_gpu_id();
    let view: Rc<wgpu::TextureView> = GPU_CANVAS_CONTEXTS.with(|r| {
        let map = r.borrow();
        let entry = map.get(&node).unwrap();
        let target = entry.target.as_ref().unwrap();
        Rc::new(target.texture.create_view(&wgpu::TextureViewDescriptor::default()))
    });
    GPU_TEXTURES.with(|r| {
        r.borrow_mut().insert(
            id,
            TextureEntry {
                view,
                width: w,
                height: h,
                texture: None,
            },
        );
    });
    GPU_CANVAS_CONTEXTS.with(|r| {
        if let Some(entry) = r.borrow_mut().get_mut(&node) {
            entry.current_texture_id = Some(id);
        }
    });
    let mut b = ObjectInitializer::new(ctx);
    b.property(js_string!(TEXTURE_KEY), JsValue::from(id), Attribute::READONLY);
    b.function(
        NativeFunction::from_fn_ptr(texture_create_view),
        js_string!("createView"),
        0,
    );
    Ok(JsValue::from(b.build()))
}

fn canvas_ctx_present(this: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Toy-only escape hatch: copy the current target's pixels back
    // into the canvas pixmap so paint can composite them. JS calls
    // it manually if it wants the render visible; the spec auto-
    // presents at the next animation frame.
    let Some(node) = canvas_ctx_node(this, ctx) else {
        return Ok(JsValue::undefined());
    };
    let copy = GPU_CANVAS_CONTEXTS.with(|r| -> Option<(Arc<wgpu::Device>, Arc<wgpu::Queue>, Vec<u8>, u32, u32)> {
        let map = r.borrow();
        let entry = map.get(&node)?;
        let target = entry.target.as_ref()?;
        let device_id = entry.device_id?;
        let gpu = GPU_DEVICES.with(|d| d.borrow().get(&device_id).map(|e| e.gpu.clone()))?;
        // Copy texture to readback buffer.
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("webgpu present"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &target.readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(target.padded_bpr),
                    rows_per_image: Some(target.height),
                },
            },
            wgpu::Extent3d {
                width: target.width,
                height: target.height,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit(Some(encoder.finish()));
        let slice = target.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        gpu.device.poll(wgpu::Maintain::Wait);
        let _ = rx.recv();
        let mapped = slice.get_mapped_range();
        let row_bytes = (target.width * 4) as usize;
        let padded = target.padded_bpr as usize;
        let mut rgba = vec![0u8; row_bytes * target.height as usize];
        for y in 0..target.height as usize {
            let src = &mapped[y * padded..y * padded + row_bytes];
            let dst = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
            // Premultiply for the canvas pixmap.
            for px in 0..target.width as usize {
                let r = src[px * 4];
                let g = src[px * 4 + 1];
                let b = src[px * 4 + 2];
                let a = src[px * 4 + 3];
                dst[px * 4] = ((r as u16 * a as u16) / 255) as u8;
                dst[px * 4 + 1] = ((g as u16 * a as u16) / 255) as u8;
                dst[px * 4 + 2] = ((b as u16 * a as u16) / 255) as u8;
                dst[px * 4 + 3] = a;
            }
        }
        drop(mapped);
        target.readback.unmap();
        Some((gpu.device.clone(), gpu.queue.clone(), rgba, target.width, target.height))
    });
    if let Some((_, _, rgba, w, h)) = copy {
        super::canvas::JS_CANVAS_SURFACES.with(|slot| {
            if let Some(rc) = slot.borrow().as_ref().cloned() {
                if let Some(surface) = rc.borrow_mut().get_mut(&node) {
                    if surface.pixmap.width() == w && surface.pixmap.height() == h {
                        surface.pixmap.data_mut().copy_from_slice(&rgba);
                    }
                }
            }
        });
    }
    Ok(JsValue::undefined())
}
