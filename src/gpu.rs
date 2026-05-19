//! GPU-backed presenter for the rasterised page buffer.
//!
//! Replaces `softbuffer`'s CPU framebuffer with a wgpu surface so the
//! final present goes through the GPU. We don't change the rasterisation
//! pipeline — `tiny_skia` + `cosmic-text` still produce CPU pixels —
//! but instead of memcpy'ing into a software framebuffer we upload them
//! as a 2D texture and render a full-screen triangle that samples it.
//!
//! This isn't compositing in the "layer model + damage tracking" sense.
//! It's a single texture upload per frame plus a fullscreen-quad draw.
//! What it gets us:
//!   * GPU presentation path (compositor-friendly, vsync-aware via
//!     `PresentMode::AutoVsync`).
//!   * Trivial path for future GPU-side scaling / colour-space
//!     management.
//!   * A foundation for adding real layers (canvas, video, fixed
//!     elements as separate textures) without re-doing the surface
//!     plumbing.

use std::sync::Arc;

use winit::window::Window;

pub struct GpuPresenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    /// Pipeline that draws a textured quad at an arbitrary pixel
    /// rect — used to composite per-overlay layer textures (e.g.
    /// `position: fixed` headers) on top of the main page without
    /// CPU-blending them into the framebuffer.
    overlay_pipeline: wgpu::RenderPipeline,
    /// Reusable uniform buffer carrying the (dst_rect, viewport)
    /// shader uniforms. Overwritten before each overlay draw.
    overlay_uniform: wgpu::Buffer,
}

/// Caller-supplied description of one composited overlay layer.
/// `bgra` is pre-multiplied BGRA8 pixel data sized to `width * height`.
pub struct OverlayLayer<'a> {
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub dest_x: f32,
    pub dest_y: f32,
}

impl GpuPresenter {
    pub fn new(window: Arc<Window>) -> Option<Self> {
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface: wgpu::Surface<'static> = match instance.create_surface(window) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gpu] create_surface: {e}");
                return None;
            }
        };

        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            },
        ))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("daboss device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .ok()?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(*f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Bgra8Unorm))
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("daboss page texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("daboss sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("daboss bind group layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    // Overlay shader reads the (dst_rect, viewport)
                    // uniform from binding 2. The main present
                    // pipeline ignores it; we still attach the
                    // buffer to keep one shared bind-group layout.
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        // Uniform buffer that carries the overlay quad's pixel
        // rect + viewport size each draw. 32 bytes (two vec4s).
        let overlay_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("daboss overlay uniforms"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("daboss bind group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: overlay_uniform.as_entire_binding(),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("daboss shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("present.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("daboss pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("daboss pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let overlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("daboss overlay pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "overlay_vs",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "overlay_fs",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Some(Self {
            surface,
            device,
            queue,
            config,
            texture,
            texture_view,
            sampler,
            bind_group_layout,
            bind_group,
            pipeline,
            overlay_pipeline,
            overlay_uniform,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.config.width == width && self.config.height == height {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);

        // Reallocate the page texture + bind group to match.
        let new_tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("daboss page texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let new_view = new_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let new_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("daboss bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&new_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.overlay_uniform.as_entire_binding(),
                },
            ],
        });
        self.texture = new_tex;
        self.texture_view = new_view;
        self.bind_group = new_bind;
    }

    /// Upload the BGRA8 page pixels and present.
    pub fn present(&mut self, bgra: &[u8], width: u32, height: u32) {
        if width != self.config.width || height != self.config.height {
            self.resize(width, height);
        }
        // Upload pixels into the texture.
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bgra,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[gpu] get_current_texture: {e:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("daboss encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daboss present pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }

    /// Present the main framebuffer with N overlay layers stamped
    /// on top via GPU-side textured quads. Avoids the per-pixel CPU
    /// blend that `present` would otherwise need for each
    /// `position: fixed` element.
    pub fn present_with_overlays(
        &mut self,
        main_bgra: &[u8],
        main_w: u32,
        main_h: u32,
        overlays: &[OverlayLayer<'_>],
    ) {
        if main_w != self.config.width || main_h != self.config.height {
            self.resize(main_w, main_h);
        }
        // Upload main framebuffer.
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            main_bgra,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(main_w * 4),
                rows_per_image: Some(main_h),
            },
            wgpu::Extent3d {
                width: main_w,
                height: main_h,
                depth_or_array_layers: 1,
            },
        );

        // Allocate one ad-hoc texture + bind group per overlay this
        // frame. Overlays are typically a handful (≤ a few), each
        // small, so per-frame allocation is cheap.
        struct OverlayResource {
            #[allow(dead_code)]
            texture: wgpu::Texture,
            #[allow(dead_code)]
            view: wgpu::TextureView,
            bind_group: wgpu::BindGroup,
            dest_x: f32,
            dest_y: f32,
            width: f32,
            height: f32,
        }
        let mut overlay_resources: Vec<OverlayResource> = Vec::with_capacity(overlays.len());
        for ov in overlays {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("daboss overlay tex"),
                size: wgpu::Extent3d {
                    width: ov.width.max(1),
                    height: ov.height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                ov.bgra,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(ov.width * 4),
                    rows_per_image: Some(ov.height),
                },
                wgpu::Extent3d {
                    width: ov.width,
                    height: ov.height,
                    depth_or_array_layers: 1,
                },
            );
            let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("daboss overlay bind"),
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: self.overlay_uniform.as_entire_binding(),
                    },
                ],
            });
            overlay_resources.push(OverlayResource {
                texture: tex,
                view,
                bind_group: bg,
                dest_x: ov.dest_x,
                dest_y: ov.dest_y,
                width: ov.width as f32,
                height: ov.height as f32,
            });
        }

        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[gpu] get_current_texture: {e:?}");
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // First pass: draw the main fullscreen quad with a Clear
        // load. Subsequent overlay draws use Load so we composite on
        // top of the existing colour.
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("daboss compose encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daboss main present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        // One render pass per overlay so we can rewrite the uniform
        // before each draw. Submitting all overlays in one pass
        // would require an indirect uniform offset or multiple
        // uniform buffers; per-overlay passes keep this simple.
        for ov in &overlay_resources {
            let uniforms: [f32; 8] = [
                ov.dest_x,
                ov.dest_y,
                ov.width,
                ov.height,
                main_w as f32,
                main_h as f32,
                0.0,
                0.0,
            ];
            let mut bytes = [0u8; 32];
            for (i, f) in uniforms.iter().enumerate() {
                bytes[i * 4..(i + 1) * 4].copy_from_slice(&f.to_le_bytes());
            }
            self.queue.write_buffer(&self.overlay_uniform, 0, &bytes);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("daboss overlay pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.overlay_pipeline);
            pass.set_bind_group(0, &ov.bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }
}
