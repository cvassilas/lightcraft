//! One device for the window and the renderer (docs/rapidraw-port.md, phase 2): a device created the
//! way the desktop window creates its own (`backend::window_device_limits`) and offered with
//! `share_window_device` is the one renders run on, and they match the CPU reference. Its own test
//! binary: the renderer's device is created once per process. Skips without a GPU adapter.

use std::sync::Arc;

use lightcraft_develop::DevelopSettings;
use lightcraft_pipeline::{RenderRequest, SourceInfo, render};

/// A device as `apps/lightcraft` `window_gpu` creates the window's: the compute backends, the
/// adapter's limits. One per process, like the window's (the tests run side by side).
fn window_like_device() -> Option<(wgpu::Device, wgpu::Queue, wgpu::AdapterInfo)> {
    static DEVICE: std::sync::OnceLock<Option<(wgpu::Device, wgpu::Queue, wgpu::AdapterInfo)>> = std::sync::OnceLock::new();
    DEVICE.get_or_init(create_window_like_device).clone()
}

fn create_window_like_device() -> Option<(wgpu::Device, wgpu::Queue, wgpu::AdapterInfo)> {
    let backends = lightcraft_gpu::backend::compute_backends()?;
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("window-like"),
        required_limits: lightcraft_gpu::backend::window_device_limits(&adapter),
        ..Default::default()
    }))
    .ok()?;
    Some((device, queue, adapter.get_info()))
}

#[test]
fn renders_run_on_the_shared_window_device() {
    let Some((device, queue, info)) = window_like_device() else {
        lightcraft_testkit::skip!(Gpu, "no GPU adapter");
        return;
    };
    // kept unless the other test offered it first
    lightcraft_gpu::share_window_device(&device, &queue, info.clone());
    if !lightcraft_gpu::available() {
        lightcraft_testkit::skip!(Gpu, "no GPU adapter the pipeline accepts ({:?})", lightcraft_gpu::unavailable_reason());
        return;
    }
    assert!(lightcraft_gpu::shares_window_device(), "renders use the offered device");
    // too late now: the renderer has its device
    assert!(!lightcraft_gpu::share_window_device(&device, &queue, info));

    let src = Arc::new(lightcraft_scenes::demo_library()[2].render(900, 600));
    let info = SourceInfo { raw: true, ..Default::default() };
    let mut s = DevelopSettings::default();
    s.light.exposure = 0.4;
    s.effects.clarity = 20.0;
    let req = RenderRequest::fit(900, 600);
    let g = lightcraft_gpu::render(&src, &info, &s, &req, None).expect("gpu render on the shared device");
    let c = render(&src, &info, &s, &req);
    let max = g.image.data.iter().zip(&c.image.data).flat_map(|(p, q)| (0..3).map(move |k| p[k].abs_diff(q[k]))).max().unwrap_or(0);
    assert!(max <= 1, "GPU render on the shared device differs from the CPU by {max} LSB");
}

/// A render asked for a frame (`with_frame`) on the shared device hands back a texture holding
/// exactly its image's pixels, at a width that needs padded rows and at one that doesn't. Overlays
/// (drawn on the CPU) and CPU renders give none.
#[test]
fn frames_hold_the_rendered_pixels() {
    let Some((device, queue, info)) = window_like_device() else {
        lightcraft_testkit::skip!(Gpu, "no GPU adapter");
        return;
    };
    lightcraft_gpu::share_window_device(&device, &queue, info);
    if !lightcraft_gpu::available() || !lightcraft_gpu::shares_window_device() {
        lightcraft_testkit::skip!(Gpu, "no GPU adapter the pipeline accepts ({:?})", lightcraft_gpu::unavailable_reason());
        return;
    }
    let src = Arc::new(lightcraft_scenes::demo_library()[3].render(1203, 800));
    let info = SourceInfo { raw: true, ..Default::default() };
    let mut s = DevelopSettings::default();
    s.light.shadows = 30.0;
    for (w, h) in [(901, 600), (1024, 681)] {
        let req = RenderRequest::fit(w, h);
        let (r, frame) = lightcraft_gpu::with_frame(|| lightcraft_gpu::render(&src, &info, &s, &req, None));
        let img = r.expect("gpu render").image;
        let frame = frame.expect("a frame from the shared device");
        assert_eq!((frame.width, frame.height), (img.width, img.height));
        let texels = read_texture(&device, &queue, frame.texture().expect("texture"));
        assert!(texels == img.data, "{}×{}: the frame's texels differ from the image", img.width, img.height);
    }
    // a frame is only made when asked for (none is left over for the next caller)…
    assert!(lightcraft_gpu::render(&src, &info, &s, &RenderRequest::fit(400, 300), None).is_some());
    let ((), frame) = lightcraft_gpu::with_frame(|| ());
    assert!(frame.is_none());
    // …and not for an overlay, which is drawn over the image on the CPU
    let req = RenderRequest { overlay: lightcraft_pipeline::Overlay::Spots(50), ..RenderRequest::fit(400, 300) };
    let (r, frame) = lightcraft_gpu::with_frame(|| lightcraft_gpu::render(&src, &info, &s, &req, None));
    if r.is_some() {
        assert!(frame.is_none(), "an overlay render made a frame");
    }
}

fn read_texture(device: &wgpu::Device, queue: &wgpu::Queue, t: &wgpu::Texture) -> Vec<[u8; 4]> {
    let (w, h) = (t.width() as usize, t.height() as usize);
    let stride = (w * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (stride * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        t.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h as u32) },
        },
        t.size(),
    );
    queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let bytes = buf.slice(..).get_mapped_range().expect("mapped");
    (0..h).flat_map(|y| bytes[y * stride..y * stride + w * 4].chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]).collect::<Vec<_>>()).collect()
}
