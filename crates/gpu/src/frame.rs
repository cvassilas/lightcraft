//! A render handed to the desktop window as a texture on the shared device
//! ([`crate::share_window_device`]): the window draws it straight from GPU memory, without copying
//! the pixels into egui's texture and uploading them again (docs/rapidraw-port.md, phase 2; the
//! idea comes from RapidRAW, the code does not).
//!
//! A caller asks with [`with_frame`] around a render; a GPU render on the shared device then
//! records a copy of its 8-bit result into a texture before the readback, in the same submission.
//! The texture holds exactly the pixels of the returned [`Rendered::image`](lightcraft_pipeline::Rendered),
//! so the CPU image stays the reference and anything that needs pixels still has them.
//!
//! No frame is made, and the caller uploads the CPU image as before, for: CPU renders, a renderer
//! on its own device, overlays drawn on the CPU, windows cut back after the readback, and renders
//! for a display profile (those are corrected on the CPU after the render).

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex, PoisonError};

use crate::ctx::{Buf, Gpu, groups2};
use crate::render::Cx;

/// The texture format of a frame: the 8-bit sRGB-encoded bytes the per-pixel stage writes, as
/// they are (egui-wgpu gives its own textures this format and decodes sRGB in its shader).
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Bytes per row of a buffer-to-texture copy must be a multiple of this.
const ROW_ALIGN: usize = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;

/// Textures of dropped frames, kept for the next frames of the same size (a slider drag makes one
/// per tick).
static POOL: Mutex<Vec<wgpu::Texture>> = Mutex::new(Vec::new());
const POOL_MAX: usize = 4;

/// A render's 8-bit result in a texture on the shared device.
pub struct Frame {
    texture: Option<wgpu::Texture>,
    view: wgpu::TextureView,
    pub width: usize,
    pub height: usize,
}

impl Frame {
    /// The texture to draw (`egui_wgpu::Renderer::register_native_texture`).
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// The texture itself (tests read it back).
    pub fn texture(&self) -> Option<&wgpu::Texture> {
        self.texture.as_ref()
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}×{})", self.width, self.height)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if let Some(t) = self.texture.take() {
            let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
            if pool.len() < POOL_MAX {
                pool.push(t);
            }
        }
    }
}

thread_local! {
    static WANT: Cell<bool> = const { Cell::new(false) };
    static MADE: RefCell<Option<Arc<Frame>>> = const { RefCell::new(None) };
}

/// Run `f` (a render) asking for its result as a [`Frame`] too: the frame when a GPU render on the
/// shared window device produced one, else `None` (see the module docs).
pub fn with_frame<R>(f: impl FnOnce() -> R) -> (R, Option<Arc<Frame>>) {
    MADE.with(|m| m.borrow_mut().take());
    WANT.with(|w| w.set(true));
    let r = f();
    WANT.with(|w| w.set(false));
    (r, MADE.with(|m| m.borrow_mut().take()))
}

/// Whether the render running on this thread was asked for a frame. `LIGHTCRAFT_GPU_FRAMES=0`
/// turns frames off (the loupe then uploads every render, as before: for A/B timings, or a driver
/// that draws them wrongly).
pub(crate) fn wanted() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    WANT.with(|w| w.get())
        && crate::shares_window_device()
        && !*OFF.get_or_init(|| std::env::var_os("LIGHTCRAFT_GPU_FRAMES").is_some_and(|v| v == "0"))
}

/// The render succeeded: hand its frame to [`with_frame`].
pub(crate) fn deliver(frame: Option<Frame>) {
    if let Some(f) = frame {
        MADE.with(|m| *m.borrow_mut() = Some(Arc::new(f)));
    }
}

fn texture(gpu: &Gpu, w: usize, h: usize) -> wgpu::Texture {
    let size = wgpu::Extent3d { width: w as u32, height: h as u32, depth_or_array_layers: 1 };
    let reused = {
        let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
        pool.iter().position(|t| t.size() == size).map(|i| pool.swap_remove(i))
    };
    reused.unwrap_or_else(|| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("lightcraft frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    })
}

/// Record a copy of `out` (the per-pixel stage's `w × h` RGBA8 words) into a new frame. Rows of a
/// buffer-to-texture copy must start every 256 bytes: unless `w` is a multiple of 64 the rows are
/// first spread out to that stride (`pad_rows`). `None` when the texture would exceed the device's
/// limits.
pub(crate) fn record(cx: &mut Cx<'_>, out: &Buf, w: usize, h: usize) -> Option<Frame> {
    let max = cx.gpu.device.limits().max_texture_dimension_2d as usize;
    if w == 0 || h == 0 || w > max || h > max {
        return None;
    }
    let stride = (w * 4).div_ceil(ROW_ALIGN) * ROW_ALIGN / 4;
    let padded;
    let src = if stride == w {
        out
    } else {
        padded = cx.gpu.buffer(stride * h);
        cx.run("pad_rows", &[w as u32, h as u32, stride as u32], &[Some(out), Some(&padded)], groups2(w, h, [16, 16]));
        &padded
    };
    let texture = texture(cx.gpu, w, h);
    cx.encoder().copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: src.raw(),
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some((stride * 4) as u32), rows_per_image: Some(h as u32) },
        },
        wgpu::TexelCopyTextureInfo { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        texture.size(),
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Some(Frame { texture: Some(texture), view, width: w, height: h })
}
