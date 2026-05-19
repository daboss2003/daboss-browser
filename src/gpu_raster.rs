//! GPU rasterisation via wgpu.
//!
//! This module is the first step toward moving paint off the main
//! thread. It exposes:
//!
//! * `GpuRasterizer` — a headless wgpu device + queue + pipeline that
//!   takes a list of axis-aligned colored rects and produces a
//!   `tiny_skia::Pixmap` of the rasterised result. The pipeline is a
//!   single render pass with one draw call per rect (each one a
//!   pre-multiplied alpha-blended fill).
//!
//! * `CompositorThread` — owns a `GpuRasterizer` on a dedicated
//!   worker thread. Callers send `RasterRequest` over an mpsc
//!   channel and receive `Pixmap`s back. Because wgpu's `Device` and
//!   `Queue` are `Send + Sync`, the GPU work happens entirely off the
//!   UI thread.
//!
//! What this is NOT (yet):
//!   * A drop-in replacement for the tiny-skia painter. The CPU
//!     painter still owns the production paint path. This module is
//!     the worker side of the compositor architecture; later steps
//!     will route the existing per-tile damage pipeline through it
//!     so dirty tiles can be rasterised on a worker thread in
//!     parallel with the main thread.
//!   * GPU glyph rasterisation. Text still falls back to cosmic-text
//!     + swash CPU rasterisation. The pipeline here only does
//!     primitive-shape rects; covering glyphs would mean uploading
//!     pre-rasterised glyph atlases as textures and switching the
//!     fragment shader to sample them.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use tiny_skia::Pixmap;

/// One axis-aligned filled rectangle. Coordinates are in pixel space
/// of the destination pixmap (origin top-left). `color` is RGBA in
/// [0..1], pre-multiplied — i.e. R/G/B already scaled by A. The
/// shader emits the colour directly, so callers that want to alpha-
/// blend over an existing pixmap should pre-multiply themselves.
#[derive(Debug, Clone, Copy)]
pub struct GpuRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub color: [f32; 4],
}

/// Headless wgpu rasteriser. One per process (or one per worker
/// thread); creating a Device is heavy so we share it across calls.
pub struct GpuRasterizer {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipeline: wgpu::RenderPipeline,
}

impl GpuRasterizer {
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
                label: Some("daboss gpu raster device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;
        let device = Arc::new(device);
        let queue = Arc::new(queue);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("daboss gpu raster shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("daboss gpu raster layout"),
                bind_group_layouts: &[],
                push_constant_ranges: &[],
            });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("daboss gpu raster pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RectVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x4,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState {
                        // Pre-multiplied source colour: replace
                        // destination by src + dst*(1 - src.a).
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        Some(Self {
            device,
            queue,
            pipeline,
        })
    }

    /// Rasterise `rects` into a `width × height` pixmap with a
    /// transparent background. Returns `None` if any GPU op fails or
    /// the readback can't be mapped.
    pub fn rasterize(
        &self,
        rects: &[GpuRect],
        width: u32,
        height: u32,
    ) -> Option<Pixmap> {
        if width == 0 || height == 0 {
            return None;
        }
        // Build vertex buffer: 6 vertices per rect (two triangles),
        // each carrying NDC position + the rect's premultiplied colour.
        let mut verts: Vec<RectVertex> = Vec::with_capacity(rects.len() * 6);
        let w_f = width as f32;
        let h_f = height as f32;
        for r in rects {
            // NDC: x_ndc = x/W * 2 - 1, y_ndc = 1 - y/H * 2.
            let x0 = (r.x / w_f) * 2.0 - 1.0;
            let x1 = ((r.x + r.w) / w_f) * 2.0 - 1.0;
            let y0 = 1.0 - (r.y / h_f) * 2.0;
            let y1 = 1.0 - ((r.y + r.h) / h_f) * 2.0;
            let c = r.color;
            verts.push(RectVertex { pos: [x0, y0], color: c });
            verts.push(RectVertex { pos: [x1, y0], color: c });
            verts.push(RectVertex { pos: [x0, y1], color: c });
            verts.push(RectVertex { pos: [x1, y0], color: c });
            verts.push(RectVertex { pos: [x1, y1], color: c });
            verts.push(RectVertex { pos: [x0, y1], color: c });
        }
        let vertex_bytes = vertices_to_bytes(&verts);
        let vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("daboss gpu raster vertices"),
            size: vertex_bytes.len().max(1) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&vertex_buffer, 0, &vertex_bytes);

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("daboss gpu raster target"),
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
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("daboss gpu raster readback"),
            size: (padded_bpr as u64) * (height as u64),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("daboss gpu raster encoder"),
            });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daboss gpu raster pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !verts.is_empty() {
                rp.set_pipeline(&self.pipeline);
                rp.set_vertex_buffer(0, vertex_buffer.slice(..));
                rp.draw(0..verts.len() as u32, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bpr),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        // Map readback synchronously by polling the device.
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        // Drive the device until the map completes. `poll(Wait)` is
        // the supported synchronous path for headless work.
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().ok()?.ok()?;

        let mapped = slice.get_mapped_range();
        let mut pixmap = Pixmap::new(width, height)?;
        let src_stride = padded_bpr as usize;
        let dst_stride = (width * 4) as usize;
        for row in 0..height as usize {
            let s = row * src_stride;
            let d = row * dst_stride;
            pixmap.data_mut()[d..d + dst_stride]
                .copy_from_slice(&mapped[s..s + dst_stride]);
        }
        drop(mapped);
        readback.unmap();
        Some(pixmap)
    }
}

/// Serialise the vertex array into a contiguous little-endian byte
/// buffer matching the vertex attributes declared on the pipeline
/// (`Float32x2` position, `Float32x4` color = 24 bytes per vertex).
/// We hand-pack rather than `unsafe`-cast a slice so the file stays
/// inside the crate's `#![forbid(unsafe_code)]` envelope.
fn vertices_to_bytes(verts: &[RectVertex]) -> Vec<u8> {
    let mut out = Vec::with_capacity(verts.len() * std::mem::size_of::<RectVertex>());
    for v in verts {
        out.extend_from_slice(&v.pos[0].to_le_bytes());
        out.extend_from_slice(&v.pos[1].to_le_bytes());
        out.extend_from_slice(&v.color[0].to_le_bytes());
        out.extend_from_slice(&v.color[1].to_le_bytes());
        out.extend_from_slice(&v.color[2].to_le_bytes());
        out.extend_from_slice(&v.color[3].to_le_bytes());
    }
    out
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RectVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

const SHADER: &str = r#"
struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
) -> VsOut {
    var out: VsOut;
    out.clip = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

// ----------------------------------------------------------------
// Compositor thread.
// ----------------------------------------------------------------

/// One request the compositor thread can service. `reply` is a
/// single-shot channel — the worker sends the resulting pixmap (or
/// `None` on failure) back to the caller.
pub struct RasterRequest {
    pub rects: Vec<GpuRect>,
    pub width: u32,
    pub height: u32,
    pub reply: Sender<Option<Pixmap>>,
}

/// Owns a worker thread + a wgpu device. Drop the handle to stop the
/// thread; sends on a closed channel become send errors which the
/// caller can choose to ignore (the typical exit-time race).
pub struct CompositorThread {
    sender: Sender<RasterRequest>,
    handle: Option<JoinHandle<()>>,
}

impl CompositorThread {
    /// Spawn the worker. Returns `None` if the wgpu device fails to
    /// initialise on the worker thread (no adapter, etc.). The
    /// allocation happens on the worker so `GpuRasterizer::new` —
    /// which `block_on`s an async call — runs there instead of on
    /// the caller's runtime.
    pub fn spawn() -> Option<Self> {
        let (tx, rx): (Sender<RasterRequest>, Receiver<RasterRequest>) = mpsc::channel();
        // Probe the device on the caller's thread first so we can
        // fail fast. The worker re-creates its own device — wgpu's
        // Device doesn't move between threads safely without
        // Arc-wrapping, which we already do, but reconstructing is
        // simpler than a hand-off dance.
        let _probe = GpuRasterizer::new()?;
        drop(_probe);
        let handle = std::thread::Builder::new()
            .name("daboss compositor".into())
            .spawn(move || {
                let rasteriser = match GpuRasterizer::new() {
                    Some(r) => r,
                    None => {
                        // Drain incoming requests with None so the
                        // caller sees the failure rather than a
                        // perpetual hang.
                        while let Ok(req) = rx.recv() {
                            let _ = req.reply.send(None);
                        }
                        return;
                    }
                };
                while let Ok(req) = rx.recv() {
                    let pix = rasteriser.rasterize(&req.rects, req.width, req.height);
                    let _ = req.reply.send(pix);
                }
            })
            .ok()?;
        Some(Self {
            sender: tx,
            handle: Some(handle),
        })
    }

    /// Submit a raster request and block until the worker replies.
    /// `None` if the worker is gone or the GPU op failed.
    pub fn rasterize(
        &self,
        rects: Vec<GpuRect>,
        width: u32,
        height: u32,
    ) -> Option<Pixmap> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.sender
            .send(RasterRequest {
                rects,
                width,
                height,
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()?
    }
}

impl Drop for CompositorThread {
    fn drop(&mut self) {
        // Closing the sender lets the worker's recv() return Err,
        // breaking the loop. Then we wait for it to exit so resources
        // are released deterministically.
        // The sender is already inside `self`; constructing a new
        // sender and dropping the original is the standard pattern.
        let (dummy_tx, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, dummy_tx);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_pixel(p: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * p.width() + x) * 4) as usize;
        let d = p.data();
        (d[i], d[i + 1], d[i + 2], d[i + 3])
    }

    #[test]
    fn rasterize_single_red_rect_fills_pixels() {
        let r = match GpuRasterizer::new() {
            Some(r) => r,
            None => {
                eprintln!("no wgpu adapter — skipping GPU test");
                return;
            }
        };
        let rects = vec![GpuRect {
            x: 0.0,
            y: 0.0,
            w: 16.0,
            h: 16.0,
            color: [1.0, 0.0, 0.0, 1.0],
        }];
        let pixmap = r.rasterize(&rects, 16, 16).expect("rasterise");
        let (red_r, red_g, red_b, red_a) = read_pixel(&pixmap, 8, 8);
        assert!(
            red_r > 240 && red_g < 20 && red_b < 20 && red_a > 240,
            "expected opaque red at center, got rgba=({red_r}, {red_g}, {red_b}, {red_a})"
        );
    }

    #[test]
    fn rasterize_two_rects_in_separate_regions() {
        let r = match GpuRasterizer::new() {
            Some(r) => r,
            None => {
                eprintln!("no wgpu adapter — skipping GPU test");
                return;
            }
        };
        let rects = vec![
            GpuRect {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
                color: [0.0, 1.0, 0.0, 1.0], // green left half
            },
            GpuRect {
                x: 8.0,
                y: 0.0,
                w: 8.0,
                h: 16.0,
                color: [0.0, 0.0, 1.0, 1.0], // blue right half
            },
        ];
        let pixmap = r.rasterize(&rects, 16, 16).expect("rasterise");
        // Left side should be green.
        let (lr, lg, lb, _) = read_pixel(&pixmap, 2, 8);
        assert!(
            lr < 20 && lg > 200 && lb < 20,
            "left should be green, got rgb=({lr}, {lg}, {lb})"
        );
        // Right side should be blue.
        let (rr, rg, rb, _) = read_pixel(&pixmap, 12, 8);
        assert!(
            rr < 20 && rg < 20 && rb > 200,
            "right should be blue, got rgb=({rr}, {rg}, {rb})"
        );
    }

    #[test]
    fn compositor_thread_rasterizes_off_main_thread() {
        let compositor = match CompositorThread::spawn() {
            Some(c) => c,
            None => {
                eprintln!("no wgpu adapter — skipping GPU thread test");
                return;
            }
        };
        let rects = vec![GpuRect {
            x: 0.0,
            y: 0.0,
            w: 8.0,
            h: 8.0,
            color: [1.0, 1.0, 0.0, 1.0],
        }];
        let pixmap = compositor.rasterize(rects, 8, 8).expect("rasterise");
        let (r, g, b, a) = read_pixel(&pixmap, 4, 4);
        assert!(
            r > 240 && g > 240 && b < 20 && a > 240,
            "expected yellow center, got rgba=({r}, {g}, {b}, {a})"
        );
    }
}
