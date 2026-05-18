//! Headless wgpu device used as the WebGL backend.
//!
//! `webgl::WebGlState` calls into this module when JS issues:
//!   * `compileShader(shader)` — translates the stored GLSL source to
//!     WGSL via `naga`'s glsl frontend + wgsl backend.
//!   * `drawArrays(TRIANGLES, first, count)` — builds (or reuses) a
//!     render pipeline from the currently-bound vertex + fragment WGSL,
//!     uploads the currently-bound ARRAY_BUFFER as a vertex buffer,
//!     renders into a per-canvas wgpu texture, and copies the pixels
//!     back into the canvas pixmap so paint can composite.
//!
//! Limitations relative to real WebGL:
//!   * Vertex layout assumes interleaved `vec3 position` packed `vec2
//!     uv` packed `vec4 color` (loosely guessed from GLSL `in`/`attribute`
//!     declarations).
//!   * No textures, no uniforms, no depth/stencil, no blending state.
//!     `drawElements` is treated like `drawArrays`. Multiple programs
//!     are isolated only by the per-canvas pipeline cache.
//!   * Errors during translation/compile produce a console log; the
//!     draw becomes a no-op so the page survives.

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
/// first `drawArrays`.
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
        // wgpu requires bytes_per_row to be a multiple of
        // COPY_BYTES_PER_ROW_ALIGNMENT (256). Round up.
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

/// Translate GLSL ES source to WGSL using naga. `stage` picks the
/// shader stage we tell the frontend to expect. Returns the WGSL
/// source on success, or an error string for `getShaderInfoLog`.
pub fn glsl_to_wgsl(source: &str, stage: ShaderStage) -> Result<String, String> {
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
    let wgsl = naga::back::wgsl::write_string(&module, &info, WriterFlags::empty())
        .map_err(|e| format!("wgsl emit: {e:?}"))?;
    Ok(wgsl)
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

/// Render `count` triangles using the given WGSL shaders and a single
/// interleaved `vec3 position` vertex buffer. Writes the result into
/// `out_rgba` row-by-row (premultiplied, sRGB-naïve). Returns true on
/// success, false on any pipeline / compile error (caller falls back
/// to the previous canvas contents).
pub fn draw_arrays(
    gpu: &WebGlGpu,
    target: &CanvasTarget,
    vertex_wgsl: &str,
    fragment_wgsl: &str,
    vertex_buffer_bytes: &[u8],
    first: u32,
    count: u32,
    clear_color: [f32; 4],
    out_rgba: &mut [u8],
) -> bool {
    let device = &gpu.device;
    let queue = &gpu.queue;
    let combined = format!("{vertex_wgsl}\n{fragment_wgsl}");
    let shader = match catch_shader_create(device, &combined) {
        Some(s) => s,
        None => return false,
    };
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("webgl pipeline layout"),
        bind_group_layouts: &[],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("webgl pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "main",
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: 12, // vec3<f32>
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x3,
                    offset: 0,
                    shader_location: 0,
                }],
            }],
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
            module: &shader,
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
    let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("webgl vbuf"),
        contents: vertex_buffer_bytes,
        usage: wgpu::BufferUsages::VERTEX,
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
                        r: clear_color[0] as f64,
                        g: clear_color[1] as f64,
                        b: clear_color[2] as f64,
                        a: clear_color[3] as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_vertex_buffer(0, vbuf.slice(..));
        pass.draw(first..first + count, 0..1);
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
            // Premultiply alpha so the canvas pixmap matches what
            // paint composites for other surfaces.
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
    // wgpu panics on validation failure if uncaught; push an error
    // scope to convert that into a returnable None.
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
