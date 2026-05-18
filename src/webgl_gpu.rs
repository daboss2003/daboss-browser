//! Headless wgpu device used as the WebGL backend.
//!
//! Driven by `js::webgl::WebGlState`. Each `drawArrays` / `drawElements`
//! call assembles a `DrawDesc` describing every WebGL state knob that
//! affects rendering and forwards it here. We synthesise a wgpu
//! pipeline + bind group, render into a per-canvas texture, and copy
//! the result back into the canvas pixmap so paint can composite.
//!
//! Pipeline cache is not implemented yet — each draw rebuilds the
//! pipeline. Fine for the toy; a `(program, attribs, format)` key
//! would be the obvious follow-up.

use std::sync::Arc;

use wgpu::util::DeviceExt;

/// One-time wgpu setup shared across all WebGL canvases on a page.
pub struct WebGlGpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
}

impl WebGlGpu {
    pub fn new() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: None,
                force_fallback_adapter: false,
            },
        ))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("daboss webgl headless device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;
        Some(Self {
            device: Arc::new(device),
            queue: Arc::new(queue),
        })
    }
}

/// Per-canvas wgpu resources reused across draws. Created lazily on the
/// first `drawArrays` and rebuilt when the canvas dimensions change.
pub struct CanvasTarget {
    pub width: u32,
    pub height: u32,
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub readback: wgpu::Buffer,
    pub padded_bpr: u32,
}

impl CanvasTarget {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("daboss webgl canvas"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let unpadded_bpr = width * 4;
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bpr = (unpadded_bpr + alignment - 1) / alignment * alignment;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("daboss webgl readback"),
            size: (padded_bpr as u64) * (height as u64),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        Self {
            width,
            height,
            texture,
            view,
            readback,
            padded_bpr,
        }
    }
}

/// Translate GLSL ES source to WGSL using naga, then post-process the
/// emitted WGSL so it works inside our pipeline:
///   * Each `var<uniform>` block gets `@group(0) @binding(0)`.
///   * Each `texture_2d<f32>` gets `@group(0) @binding(1)`.
///   * Each `sampler` gets `@group(0) @binding(2)`.
/// Returns `(wgsl, has_uniform, has_texture)` on success. The flags
/// drive whether `draw` builds a uniform buffer / texture binding.
pub fn glsl_to_wgsl(
    source: &str,
    stage: ShaderStage,
) -> Result<TranslatedShader, String> {
    use naga::back::wgsl::WriterFlags;
    use naga::front::glsl::{Frontend, Options};
    use naga::valid::{Capabilities, ValidationFlags, Validator};
    let mut frontend = Frontend::default();
    let options = Options::from(match stage {
        ShaderStage::Vertex => naga::ShaderStage::Vertex,
        ShaderStage::Fragment => naga::ShaderStage::Fragment,
    });
    let module = frontend
        .parse(&options, source)
        .map_err(|errs| errs_to_log(&errs.errors))?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::empty())
        .validate(&module)
        .map_err(|e| format!("validate: {e:?}"))?;
    let wgsl_raw = naga::back::wgsl::write_string(&module, &info, WriterFlags::empty())
        .map_err(|e| format!("wgsl emit: {e:?}"))?;
    let (wgsl, has_uniform, has_texture) = patch_bindings(&wgsl_raw);
    Ok(TranslatedShader {
        wgsl,
        has_uniform,
        has_texture,
    })
}

/// Output of `glsl_to_wgsl` — carries flags the draw path needs to
/// build the right bind group entries.
#[derive(Clone)]
pub struct TranslatedShader {
    pub wgsl: String,
    pub has_uniform: bool,
    pub has_texture: bool,
}

/// Inject `@group(0) @binding(N)` decorations onto each top-level
/// `var<uniform>` / `var<sampler>` / `var<texture_2d>` declaration.
/// naga emits them without binding attributes for GLSL inputs; we
/// stamp ours on so wgpu can match the bind group layout we build at
/// draw time.
fn patch_bindings(src: &str) -> (String, bool, bool) {
    let mut out = String::with_capacity(src.len() + 64);
    let mut has_uniform = false;
    let mut has_texture = false;
    for line in src.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("var<uniform>")
            && !out.contains("@group(0) @binding(0) var<uniform>")
        {
            out.push_str("@group(0) @binding(0) ");
            has_uniform = true;
        } else if trimmed.starts_with("var ")
            && trimmed.contains("texture_2d")
            && !out.contains("@binding(1) var ")
        {
            out.push_str("@group(0) @binding(1) ");
            has_texture = true;
        } else if trimmed.starts_with("var ")
            && trimmed.contains("sampler")
            && !trimmed.contains("texture_2d")
            && !out.contains("@binding(2) var ")
        {
            out.push_str("@group(0) @binding(2) ");
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, has_uniform, has_texture)
}

fn errs_to_log(errors: &[naga::front::glsl::Error]) -> String {
    let mut buf = String::new();
    for e in errors {
        buf.push_str(&format!("{:?}\n", e.kind));
    }
    buf
}

#[derive(Copy, Clone)]
pub enum ShaderStage {
    Vertex,
    Fragment,
}

/// Description of one attribute slot. Mirrors WebGL's
/// `vertexAttribPointer` arguments.
#[derive(Clone)]
pub struct AttribLayout {
    pub location: u32,
    pub buffer_id: u32,
    pub size: u32,
    pub component: AttribComponent,
    pub stride: u32,
    pub offset: u64,
}

#[derive(Copy, Clone)]
pub enum AttribComponent {
    Float,
    UnsignedByteNormalized,
}

impl AttribComponent {
    fn wgpu_format(&self, size: u32) -> wgpu::VertexFormat {
        match (self, size) {
            (Self::Float, 1) => wgpu::VertexFormat::Float32,
            (Self::Float, 2) => wgpu::VertexFormat::Float32x2,
            (Self::Float, 3) => wgpu::VertexFormat::Float32x3,
            (Self::Float, 4) => wgpu::VertexFormat::Float32x4,
            (Self::UnsignedByteNormalized, 4) => wgpu::VertexFormat::Unorm8x4,
            _ => wgpu::VertexFormat::Float32x3,
        }
    }
}

/// Texture bound at draw time.
pub struct UploadedTexture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// All the state a single `drawArrays` / `drawElements` needs from JS.
pub struct DrawDesc<'a> {
    pub vertex_shader: &'a TranslatedShader,
    pub fragment_shader: &'a TranslatedShader,
    pub attribs: &'a [AttribLayout],
    pub buffers: &'a std::collections::HashMap<u32, Vec<u8>>,
    pub uniform_bytes: &'a [u8],
    pub texture: Option<&'a UploadedTexture>,
    pub clear_color: [f32; 4],
    pub first: u32,
    pub count: u32,
    pub index_buffer: Option<(IndexFormat, &'a [u8])>,
}

#[derive(Copy, Clone)]
pub enum IndexFormat {
    Uint16,
    Uint32,
}

impl IndexFormat {
    fn wgpu(&self) -> wgpu::IndexFormat {
        match self {
            Self::Uint16 => wgpu::IndexFormat::Uint16,
            Self::Uint32 => wgpu::IndexFormat::Uint32,
        }
    }
}

/// Render `desc` into `target` and copy the pixels back into
/// `out_rgba`. Returns true on success; false if any pipeline /
/// shader stage failed (caller treats as a no-op).
pub fn draw(
    gpu: &WebGlGpu,
    target: &CanvasTarget,
    desc: &DrawDesc<'_>,
    out_rgba: &mut [u8],
) -> bool {
    let device = &gpu.device;
    let queue = &gpu.queue;

    // Sanity check: must have at least one attrib and a vertex buffer.
    if desc.attribs.is_empty() {
        return false;
    }
    if !desc.vertex_shader.has_uniform && !desc.fragment_shader.has_uniform
        && desc.uniform_bytes.is_empty()
    {
        // Shaders don't reference uniforms; skip the bind group entry.
    }

    // Build vertex buffer layouts — one slot per attribute, each
    // sourcing from its own buffer slot. Real WebGL packs multiple
    // attribs sharing a buffer into one VertexBufferLayout, but
    // wgpu accepts one VBL per slot so we use that for simplicity.
    let mut vertex_layouts: Vec<wgpu::VertexBufferLayout> = Vec::new();
    let mut vertex_buffers: Vec<wgpu::Buffer> = Vec::new();
    // Cheap holder for the per-slot VertexAttribute so its lifetime
    // extends through the pipeline-descriptor borrow.
    let mut per_slot_attribs: Vec<[wgpu::VertexAttribute; 1]> = Vec::with_capacity(desc.attribs.len());
    for a in desc.attribs {
        let format = a.component.wgpu_format(a.size);
        per_slot_attribs.push([wgpu::VertexAttribute {
            format,
            offset: a.offset,
            shader_location: a.location,
        }]);
    }
    for (i, a) in desc.attribs.iter().enumerate() {
        let bytes = match desc.buffers.get(&a.buffer_id) {
            Some(b) if !b.is_empty() => b,
            _ => return false,
        };
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("webgl attrib buf"),
            contents: bytes,
            usage: wgpu::BufferUsages::VERTEX,
        });
        let stride = if a.stride == 0 {
            // Tightly packed.
            let bytes_per_elem = match a.component {
                AttribComponent::Float => 4 * a.size,
                AttribComponent::UnsignedByteNormalized => a.size,
            };
            bytes_per_elem as u64
        } else {
            a.stride as u64
        };
        vertex_layouts.push(wgpu::VertexBufferLayout {
            array_stride: stride,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &per_slot_attribs[i],
        });
        vertex_buffers.push(buf);
    }

    let vs_shader = match catch_shader_create(device, &desc.vertex_shader.wgsl) {
        Some(s) => s,
        None => return false,
    };
    let fs_shader = match catch_shader_create(device, &desc.fragment_shader.wgsl) {
        Some(s) => s,
        None => return false,
    };

    // Bind group: optional uniform buffer + optional texture+sampler.
    let needs_uniform =
        desc.vertex_shader.has_uniform || desc.fragment_shader.has_uniform;
    let needs_texture =
        desc.vertex_shader.has_texture || desc.fragment_shader.has_texture;

    let mut bgl_entries: Vec<wgpu::BindGroupLayoutEntry> = Vec::new();
    if needs_uniform {
        bgl_entries.push(wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        });
    }
    if needs_texture {
        bgl_entries.push(wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
        bgl_entries.push(wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        });
    }
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("webgl bgl"),
        entries: &bgl_entries,
    });

    // Build the uniform buffer (always at least 16 bytes — wgpu
    // rejects 0-size uniform buffers).
    let uniform_bytes = if desc.uniform_bytes.is_empty() {
        vec![0u8; 16]
    } else {
        let mut padded = desc.uniform_bytes.to_vec();
        while padded.len() < 16 {
            padded.push(0);
        }
        // Round up to 16-byte alignment.
        let rem = padded.len() % 16;
        if rem != 0 {
            padded.extend(std::iter::repeat(0).take(16 - rem));
        }
        padded
    };
    let uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("webgl uniforms"),
        contents: &uniform_bytes,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });

    // Texture + sampler (or a tiny 1x1 white if shader needs one but
    // the page hasn't uploaded — keeps the pipeline valid).
    let (texture_view, sampler) = if needs_texture {
        let (w, h, rgba) = match desc.texture {
            Some(t) => (t.width.max(1), t.height.max(1), t.rgba.clone()),
            None => (1, 1, vec![255u8, 255, 255, 255]),
        };
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("webgl tex"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("webgl sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        (Some(view), Some(sampler))
    } else {
        (None, None)
    };

    let mut bg_entries: Vec<wgpu::BindGroupEntry> = Vec::new();
    if needs_uniform {
        bg_entries.push(wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buf.as_entire_binding(),
        });
    }
    if needs_texture {
        let view_ref = texture_view.as_ref().unwrap();
        let sampler_ref = sampler.as_ref().unwrap();
        bg_entries.push(wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::TextureView(view_ref),
        });
        bg_entries.push(wgpu::BindGroupEntry {
            binding: 2,
            resource: wgpu::BindingResource::Sampler(sampler_ref),
        });
    }
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("webgl bg"),
        layout: &bgl,
        entries: &bg_entries,
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("webgl pipeline layout"),
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("webgl pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &vs_shader,
            entry_point: "main",
            compilation_options: Default::default(),
            buffers: &vertex_layouts,
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &fs_shader,
            entry_point: "main",
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview: None,
        cache: None,
    });

    let index_buf = desc.index_buffer.map(|(fmt, bytes)| {
        let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("webgl indices"),
            contents: bytes,
            usage: wgpu::BufferUsages::INDEX,
        });
        (fmt, buf)
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("webgl encoder"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("webgl pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: desc.clear_color[0] as f64,
                        g: desc.clear_color[1] as f64,
                        b: desc.clear_color[2] as f64,
                        a: desc.clear_color[3] as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        for (i, buf) in vertex_buffers.iter().enumerate() {
            pass.set_vertex_buffer(i as u32, buf.slice(..));
        }
        if let Some((fmt, ref buf)) = index_buf {
            pass.set_index_buffer(buf.slice(..), fmt.wgpu());
            pass.draw_indexed(desc.first..desc.first + desc.count, 0, 0..1);
        } else {
            pass.draw(desc.first..desc.first + desc.count, 0..1);
        }
    }
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
    queue.submit(Some(encoder.finish()));
    let slice = target.readback.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device.poll(wgpu::Maintain::Wait);
    if rx.recv().map(|r| r.is_err()).unwrap_or(true) {
        target.readback.unmap();
        return false;
    }
    {
        let mapped = slice.get_mapped_range();
        let row_bytes = (target.width * 4) as usize;
        let padded = target.padded_bpr as usize;
        for y in 0..target.height as usize {
            let src = &mapped[y * padded..y * padded + row_bytes];
            let dst = &mut out_rgba[y * row_bytes..y * row_bytes + row_bytes];
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
    }
    target.readback.unmap();
    true
}

fn catch_shader_create(device: &wgpu::Device, wgsl: &str) -> Option<wgpu::ShaderModule> {
    device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("webgl shader"),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let err = pollster::block_on(device.pop_error_scope());
    if let Some(e) = err {
        eprintln!("[webgl] shader create error: {e}");
        return None;
    }
    Some(module)
}
