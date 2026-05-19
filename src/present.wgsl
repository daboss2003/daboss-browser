// Fullscreen-triangle presenter: covers the whole viewport with one
// triangle so we can sample the rasterised page texture without any
// vertex buffer / index buffer setup.

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Three vertices that form a triangle covering [-1, 3] on both axes,
    // i.e. a tri that fully contains the [-1, 1] clip-space quad.
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    // Texture coords go [0, 2] so the [-1, 1] visible portion samples
    // [0, 1] like a normal quad.
    var uv = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(pos[vi], 0.0, 1.0);
    out.uv = uv[vi];
    return out;
}

@group(0) @binding(0) var page_tex: texture_2d<f32>;
@group(0) @binding(1) var page_smp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(page_tex, page_smp, in.uv);
}

// Overlay quad shader: emits a quad covering [dst_x, dst_x + w]
// in pixel space, normalised to clip-space via the viewport size
// passed in via push constants surfaced as a uniform. We use a
// vertex-index trick instead of vertex buffers so the host side
// stays simple.

struct OverlayUniforms {
    // (dst_x, dst_y, w, h) in pixels.
    rect: vec4<f32>,
    // (viewport_w, viewport_h, _, _).
    viewport: vec4<f32>,
};

@group(0) @binding(2) var<uniform> overlay: OverlayUniforms;

@vertex
fn overlay_vs(@builtin(vertex_index) vi: u32) -> VsOut {
    // Build a quad from two triangles via vertex index.
    // Map vi -> (u, v) corner of the quad in [0, 1].
    var u: f32 = 0.0;
    var v: f32 = 0.0;
    switch vi {
        case 0u: { u = 0.0; v = 0.0; }
        case 1u: { u = 1.0; v = 0.0; }
        case 2u: { u = 0.0; v = 1.0; }
        case 3u: { u = 0.0; v = 1.0; }
        case 4u: { u = 1.0; v = 0.0; }
        default: { u = 1.0; v = 1.0; }
    }
    let dst_x = overlay.rect.x + u * overlay.rect.z;
    let dst_y = overlay.rect.y + v * overlay.rect.w;
    let vw = overlay.viewport.x;
    let vh = overlay.viewport.y;
    // Pixel coords → clip space.
    let clip_x = dst_x / vw * 2.0 - 1.0;
    let clip_y = 1.0 - dst_y / vh * 2.0;
    var out: VsOut;
    out.pos = vec4<f32>(clip_x, clip_y, 0.0, 1.0);
    out.uv = vec2<f32>(u, v);
    return out;
}

@fragment
fn overlay_fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(page_tex, page_smp, in.uv);
}
