// Frames for the window (`frame.rs`): copy `w × h` RGBA8 words into rows `stride` words apart, the
// row alignment a buffer-to-texture copy needs. Bindings: src, dst (u32). P: w, h, stride.
@compute @workgroup_size(16, 16)
fn pad_rows(@builtin(global_invocation_id) g: vec3<u32>) {
    let w = pu(0u);
    let h = pu(1u);
    let stride = pu(2u);
    if (g.x >= w || g.y >= h) {
        return;
    }
    dst[g.y * stride + g.x] = src[g.y * w + g.x];
}
