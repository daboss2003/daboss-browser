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
