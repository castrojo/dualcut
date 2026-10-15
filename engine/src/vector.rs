//! Vello-rendered vector shapes (feature = "vector").
//!
//! M3 approach: shapes are rasterized once at compile time to cached PNGs
//! (keyed by shape/fill/size) and enter the GES timeline as image clips —
//! so GES-level transforms and opacity/position animations apply to them
//! like any other clip. Live per-frame vector animation (path morphs)
//! comes later with a real Vello source element.

use crate::document::{parse_color, ShapeKind};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use vello::kurbo::{Affine, BezPath, Circle, Ellipse, Point, RoundedRect, Stroke};
use vello::peniko::{Color, Fill};
use vello::wgpu;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// One renderer reused across frames — creating it is the expensive
    /// part (shader compilation), and vellosrc renders every frame.
    renderer: std::sync::Mutex<Renderer>,
}

static GPU: OnceLock<Option<Gpu>> = OnceLock::new();

fn gpu() -> Option<&'static Gpu> {
    GPU.get_or_init(|| {
        // Vulkan only: wgpu's GL backend lacks the compute features
        // Vello's shaders need (ARB_arrays_of_arrays panics observed in
        // the wild, #26); no Vulkan means no vector rendering rather
        // than a crashed app.
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .ok()?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        // Shader compilation panics on unsupported drivers; degrade to
        // "no vector rendering" instead of unwinding through GStreamer.
        let renderer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Renderer::new(&device, RendererOptions::default())
        }))
        .ok()?
        .ok()?;
        Some(Gpu { device, queue, renderer: std::sync::Mutex::new(renderer) })
    })
    .as_ref()
}

fn star_path(center: Point, points: u32, outer: f64, inner: f64) -> BezPath {
    let mut path = BezPath::new();
    for i in 0..(points * 2) {
        let r = if i % 2 == 0 { outer } else { inner };
        let a = std::f64::consts::PI * (i as f64) / (points as f64) - std::f64::consts::FRAC_PI_2;
        let p = Point::new(center.x + r * a.cos(), center.y + r * a.sin());
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.close_path();
    path
}

fn polygon_path(center: Point, sides: u32, radius: f64) -> BezPath {
    let mut path = BezPath::new();
    for i in 0..sides {
        let a = 2.0 * std::f64::consts::PI * (i as f64) / (sides as f64)
            - std::f64::consts::FRAC_PI_2;
        let p = Point::new(center.x + radius * a.cos(), center.y + radius * a.sin());
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    path.close_path();
    path
}

fn build_shape_scene(kind: ShapeKind, fill: Color, w: f64, h: f64) -> Scene {
    let mut scene = Scene::new();
    let cx = w / 2.0;
    let cy = h / 2.0;
    let r = w.min(h) / 2.0 - 2.0;
    match kind {
        ShapeKind::Rect => scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            fill,
            None,
            &RoundedRect::new(0.0, 0.0, w, h, w.min(h) * 0.08),
        ),
        ShapeKind::Circle => {
            scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &Circle::new((cx, cy), r))
        }
        ShapeKind::Ellipse => scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            fill,
            None,
            &Ellipse::new((cx, cy), (w / 2.0 - 2.0, h / 2.0 - 2.0), 0.0),
        ),
        ShapeKind::Star => scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            fill,
            None,
            &star_path(Point::new(cx, cy), 5, r, r * 0.42),
        ),
        ShapeKind::Polygon => scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            fill,
            None,
            &polygon_path(Point::new(cx, cy), 6, r),
        ),
        ShapeKind::Line => {
            let mut path = BezPath::new();
            path.move_to((2.0, cy));
            path.line_to((w - 2.0, cy));
            scene.stroke(&Stroke::new(h.max(4.0) * 0.35), Affine::IDENTITY, fill, None, &path);
        }
        ShapeKind::Arrow => {
            let shaft = h * 0.28;
            let head = (w * 0.28).min(h);
            let mut path = BezPath::new();
            path.move_to((2.0, cy - shaft / 2.0));
            path.line_to((w - head, cy - shaft / 2.0));
            path.line_to((w - head, cy - h / 2.0 + 2.0));
            path.line_to((w - 2.0, cy));
            path.line_to((w - head, cy + h / 2.0 - 2.0));
            path.line_to((w - head, cy + shaft / 2.0));
            path.line_to((2.0, cy + shaft / 2.0));
            path.close_path();
            scene.fill(Fill::NonZero, Affine::IDENTITY, fill, None, &path);
        }
    }
    scene
}

/// Rasterize a shape to raw RGBA pixels (row-major, tightly packed).
/// `rotate` is radians about the center — used by vellosrc for live frames.
pub fn render_shape_rgba(
    kind: ShapeKind,
    fill_hex: &str,
    width: u32,
    height: u32,
    rotate: f64,
) -> Result<Vec<u8>> {
    if let Some(gpu) = gpu() {
        render_shape_rgba_gpu(gpu, kind, fill_hex, width, height, rotate)
    } else {
        render_shape_rgba_cpu(kind, fill_hex, width, height, rotate)
    }
}

/// GPU path: Vello rasterization (#60 fallback split).
fn render_shape_rgba_gpu(
    gpu: &Gpu,
    kind: ShapeKind,
    fill_hex: &str,
    width: u32,
    height: u32,
    rotate: f64,
) -> Result<Vec<u8>> {
    let mut renderer = gpu.renderer.lock().unwrap();

    let argb = parse_color(fill_hex);
    let color = Color::from_rgba8(
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
        ((argb >> 24) & 0xff) as u8,
    );
    let mut scene = build_shape_scene(kind, color, width as f64, height as f64);
    if rotate != 0.0 {
        let rotated = {
            let mut s = Scene::new();
            s.append(
                &scene,
                Some(Affine::rotate_about(
                    rotate,
                    vello::kurbo::Point::new(width as f64 / 2.0, height as f64 / 2.0),
                )),
            );
            s
        };
        scene = rotated;
    }

    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("shape target"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    renderer
        .render_to_texture(
            &gpu.device,
            &gpu.queue,
            &scene,
            &view,
            &RenderParams {
                base_color: Color::from_rgba8(0, 0, 0, 0),
                width,
                height,
                antialiasing_method: AaConfig::Area,
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let bytes_per_row = (width * 4).next_multiple_of(256);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let data = slice.get_mapped_range().context("mapping readback buffer")?;

    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let row = &data[(y * bytes_per_row) as usize..][..(width * 4) as usize];
        pixels.extend_from_slice(row);
    }
    Ok(pixels)
}

// ── CPU fallback (#60) ───────────────────────────────────────────────
//
// When Vulkan isn't available, shapes are rasterized pixel-by-pixel on
// the CPU. No anti-aliasing (the shapes render once to a cached PNG, so
// aliasing is tolerable), no external deps beyond std. Each shape maps to
// a point-containment test; the same fill color is written for every
// pixel inside the shape.

/// CPU fallback: returns true if the point (px, py) is inside the shape.
fn cpu_shape_contains(
    kind: ShapeKind,
    px: f64,
    py: f64,
    w: f64,
    h: f64,
) -> bool {
    let cx = w / 2.0;
    let cy = h / 2.0;
    let r = w.min(h) / 2.0 - 2.0;
    match kind {
        ShapeKind::Rect => {
            let cr = w.min(h) * 0.08;
            cpu_rounded_rect_contains(px, py, 0.0, 0.0, w, h, cr)
        }
        ShapeKind::Circle => (px - cx).hypot(py - cy) <= r,
        ShapeKind::Ellipse => {
            let rx = w / 2.0 - 2.0;
            let ry = h / 2.0 - 2.0;
            let dx = (px - cx) / rx;
            let dy = (py - cy) / ry;
            dx * dx + dy * dy <= 1.0
        }
        ShapeKind::Star => {
            let inner = r * 0.42;
            let verts = cpu_star_vertices(cx, cy, 5, r, inner);
            cpu_point_in_polygon(px, py, &verts)
        }
        ShapeKind::Polygon => {
            let verts = cpu_polygon_vertices(cx, cy, 6, r);
            cpu_point_in_polygon(px, py, &verts)
        }
        ShapeKind::Line => py >= cy - h.max(4.0) * 0.175 && py <= cy + h.max(4.0) * 0.175,
        ShapeKind::Arrow => cpu_arrow_contains(px, py, w, h),
    }
}

fn cpu_rounded_rect_contains(px: f64, py: f64, x: f64, y: f64, w: f64, h: f64, r: f64) -> bool {
    if px < x || px > x + w || py < y || py > y + h {
        return false;
    }
    // Inside the central rectangle.
    if px >= x + r && px <= x + w - r {
        return true;
    }
    if py >= y + r && py <= y + h - r {
        return true;
    }
    // Corner check: distance to the nearest corner centre.
    let (cx, cy) = if px < x + r {
        if py < y + r {
            (x + r, y + r)
        } else {
            (x + r, y + h - r)
        }
    } else if py < y + r {
        (x + w - r, y + r)
    } else {
        (x + w - r, y + h - r)
    };
    (px - cx).hypot(py - cy) <= r
}

fn cpu_star_vertices(cx: f64, cy: f64, points: u32, outer: f64, inner: f64) -> Vec<(f64, f64)> {
    (0..points * 2)
        .map(|i| {
            let r = if i % 2 == 0 { outer } else { inner };
            let a =
                std::f64::consts::PI * (i as f64) / (points as f64) - std::f64::consts::FRAC_PI_2;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

fn cpu_polygon_vertices(cx: f64, cy: f64, sides: u32, radius: f64) -> Vec<(f64, f64)> {
    (0..sides)
        .map(|i| {
            let a = 2.0 * std::f64::consts::PI * (i as f64) / (sides as f64)
                - std::f64::consts::FRAC_PI_2;
            (cx + radius * a.cos(), cy + radius * a.sin())
        })
        .collect()
}

/// Winding-number point-in-polygon test (handles self-intersecting stars).
fn cpu_point_in_polygon(px: f64, py: f64, verts: &[(f64, f64)]) -> bool {
    let n = verts.len();
    let mut wn = 0i32;
    for i in 0..n {
        let (x0, y0) = verts[i];
        let (x1, y1) = verts[(i + 1) % n];
        if y0 <= py {
            if y1 > py && (x1 - x0) * (py - y0) - (px - x0) * (y1 - y0) > 0.0 {
                wn += 1;
            }
        } else if y1 <= py && (x1 - x0) * (py - y0) - (px - x0) * (y1 - y0) < 0.0 {
            wn -= 1;
        }
    }
    wn != 0
}

fn cpu_arrow_contains(px: f64, py: f64, w: f64, h: f64) -> bool {
    let cy = h / 2.0;
    let shaft = h * 0.28;
    let head = (w * 0.28).min(h);
    // Arrow body: shaft rectangle
    if px >= 2.0 && px <= w - head {
        let shalf = shaft / 2.0;
        if py >= cy - shalf && py <= cy + shalf {
            return true;
        }
    }
    // Arrow head: triangle from (w-head, cy-h/2+2) -> (w-head, cy+h/2-2) -> (w-2, cy)
    let verts = [
        (w - head, cy - h / 2.0 + 2.0),
        (w - head, cy + h / 2.0 - 2.0),
        (w - 2.0, cy),
    ];
    cpu_point_in_polygon(px, py, &verts)
}

fn render_shape_rgba_cpu(
    kind: ShapeKind,
    fill_hex: &str,
    width: u32,
    height: u32,
    rotate: f64,
) -> Result<Vec<u8>> {
    let argb = parse_color(fill_hex);
    let (a, r, g, b) = (
        ((argb >> 24) & 0xff) as u8,
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
    );
    let w = width as f64;
    let h = height as f64;
    let cx = w / 2.0;
    let cy = h / 2.0;
    let cos_rot = (-rotate).cos();
    let sin_rot = (-rotate).sin();

    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for py in 0..height {
        for px in 0..width {
            // Apply inverse rotation so we test the un-rotated shape.
            let (fx, fy) = if rotate != 0.0 {
                let dx = (px as f64 + 0.5) - cx;
                let dy = (py as f64 + 0.5) - cy;
                (cx + dx * cos_rot - dy * sin_rot, cy + dx * sin_rot + dy * cos_rot)
            } else {
                (px as f64 + 0.5, py as f64 + 0.5)
            };
            if cpu_shape_contains(kind, fx, fy, w, h) {
                let idx = (py * width + px) as usize * 4;
                pixels[idx] = r;
                pixels[idx + 1] = g;
                pixels[idx + 2] = b;
                pixels[idx + 3] = a;
            }
        }
    }
    Ok(pixels)
}

/// Rasterize a shape to a transparent PNG in `cache_dir`, returning its
/// path. Cached by shape/fill/size.
pub fn shape_png(
    cache_dir: &Path,
    kind: ShapeKind,
    fill_hex: &str,
    width: u32,
    height: u32,
) -> Result<PathBuf> {
    shape_png_maybe_inverted(cache_dir, kind, fill_hex, width, height, 0.0, false)
}

/// As [`shape_png`], but with an optional soft (`feather`, Gaussian sigma
/// in pixels) edge and/or the painted/unpainted regions swapped
/// (`invert`): opaque where the shape was absent, transparent where it
/// was present. Used to bake a freeform shape mask matte (#41): rather
/// than a GStreamer element inverting/feathering a live alpha stream (no
/// plain video-invert element exists in the available gst-plugins-{good,
/// bad} set, and `videobalance`'s `contrast` clamps at 0 instead of
/// negating), both transforms are baked into the raster once and cached.
pub fn shape_png_maybe_inverted(
    cache_dir: &Path,
    kind: ShapeKind,
    fill_hex: &str,
    width: u32,
    height: u32,
    feather: f64,
    invert: bool,
) -> Result<PathBuf> {
    let file = cache_dir.join(format!(
        "shape-{kind:?}-{}-{width}x{height}-f{feather}{}.png",
        fill_hex.trim_start_matches('#'),
        if invert { "-inv" } else { "" }
    ));
    if file.exists() {
        return Ok(file);
    }
    std::fs::create_dir_all(cache_dir)?;
    let mut pixels = render_shape_rgba(kind, fill_hex, width, height, 0.0)?;
    if invert {
        let argb = parse_color(fill_hex);
        let (r, g, b) = (((argb >> 16) & 0xff) as u8, ((argb >> 8) & 0xff) as u8, (argb & 0xff) as u8);
        let (px_chunks, _rest) = pixels.as_chunks_mut::<4>();
        for px in px_chunks {
            *px = if px[3] == 0 { [r, g, b, 255] } else { [0, 0, 0, 0] };
        }
    }
    let mut img = image::RgbaImage::from_raw(width, height, pixels).context("image from raw")?;
    if feather > 0.0 {
        img = image::imageops::blur(&img, feather as f32);
    }
    img.save(&file).with_context(|| format!("saving {}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test should succeed regardless of GPU availability (#60):
    /// Vello is tried first, CPU fallback runs when Vulkan isn't present.

    fn tmp_cache(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dualcut-vector-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn shape_png_has_the_requested_dimensions() {
        let cache = tmp_cache("dims");
        let path = shape_png(&cache, ShapeKind::Rect, "#ff0000", 64, 32).unwrap();
        let img = image::open(&path).expect("valid png").to_rgba8();
        assert_eq!((img.width(), img.height()), (64, 32));
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn rect_shape_fills_the_whole_canvas_opaquely() {
        let cache = tmp_cache("rect-fill");
        let path = shape_png(&cache, ShapeKind::Rect, "#ff0000", 40, 40).unwrap();
        let img = image::open(&path).expect("valid png").to_rgba8();
        // A rect shape has no margin, so even a corner pixel should be
        // opaque and roughly the requested red.
        let px = img.get_pixel(1, 1);
        assert!(px[3] > 200, "corner should be opaque, got alpha={}", px[3]);
        assert!(px[0] > 150 && px[1] < 100, "corner should be reddish, got {px:?}");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn circle_shape_is_transparent_outside_the_circle() {
        let cache = tmp_cache("circle-corner");
        let path = shape_png(&cache, ShapeKind::Circle, "#00ff00", 60, 60).unwrap();
        let img = image::open(&path).expect("valid png").to_rgba8();
        // A circle inscribed in a square canvas never reaches the
        // corners -- unlike Rect, this actually distinguishes shape logic
        // from "the whole canvas is painted."
        let corner = img.get_pixel(1, 1);
        let center = img.get_pixel(30, 30);
        assert_eq!(corner[3], 0, "corner outside a circle should be fully transparent");
        assert!(center[3] > 200, "center inside a circle should be opaque");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn invert_swaps_painted_and_unpainted_regions() {
        let cache = tmp_cache("invert");
        let normal = shape_png_maybe_inverted(
            &cache,
            ShapeKind::Circle,
            "#0000ff",
            60,
            60,
            0.0,
            false
        ).unwrap();
        let inverted = shape_png_maybe_inverted(
            &cache,
            ShapeKind::Circle,
            "#0000ff",
            60,
            60,
            0.0,
            true
        ).unwrap();
        let normal = image::open(&normal).expect("valid png").to_rgba8();
        let inverted = image::open(&inverted).expect("valid png").to_rgba8();
        // Center (inside the circle): opaque normally, transparent inverted.
        assert!(normal.get_pixel(30, 30)[3] > 200);
        assert_eq!(inverted.get_pixel(30, 30)[3], 0);
        // Corner (outside the circle): transparent normally, opaque inverted.
        assert_eq!(normal.get_pixel(1, 1)[3], 0);
        assert!(inverted.get_pixel(1, 1)[3] > 200);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn feathering_softens_the_edge_instead_of_a_hard_cutoff() {
        let cache = tmp_cache("feather");
        let sharp = shape_png_maybe_inverted(
            &cache,
            ShapeKind::Circle,
            "#ffffff",
            80,
            80,
            0.0,
            false
        ).unwrap();
        let soft = shape_png_maybe_inverted(
            &cache,
            ShapeKind::Circle,
            "#ffffff",
            80,
            80,
            8.0,
            false
        ).unwrap();
        let sharp = image::open(&sharp).expect("valid png").to_rgba8();
        let soft = image::open(&soft).expect("valid png").to_rgba8();
        // Scan outward from the center along one row; the feathered
        // version's alpha should fall off gradually (more intermediate
        // values near the edge) rather than jumping straight from opaque
        // to zero like the unfeathered raster.
        let row = 40;
        let sharp_intermediate =
            (0..80).filter(|&x| { let a = sharp.get_pixel(x, row)[3]; a > 10 && a < 245 }).count();
        let soft_intermediate =
            (0..80).filter(|&x| { let a = soft.get_pixel(x, row)[3]; a > 10 && a < 245 }).count();
        assert!(
            soft_intermediate > sharp_intermediate,
            "feathered edge should have more intermediate-alpha pixels: sharp={sharp_intermediate} soft={soft_intermediate}"
        );
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn results_are_cached_by_content_not_regenerated() {
        let cache = tmp_cache("cache");
        let first = shape_png(&cache, ShapeKind::Star, "#123456", 32, 32).unwrap();
        let mtime1 = std::fs::metadata(&first).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = shape_png(&cache, ShapeKind::Star, "#123456", 32, 32).unwrap();
        let mtime2 = std::fs::metadata(&second).unwrap().modified().unwrap();
        assert_eq!(first, second, "identical params should reuse the same cache path");
        assert_eq!(mtime1, mtime2, "second call should not have rewritten the file");
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn different_shapes_produce_different_cache_files() {
        let cache = tmp_cache("distinct");
        let rect = shape_png(&cache, ShapeKind::Rect, "#ffffff", 32, 32).unwrap();
        let circle = shape_png(&cache, ShapeKind::Circle, "#ffffff", 32, 32).unwrap();
        assert_ne!(rect, circle);
        let _ = std::fs::remove_dir_all(&cache);
    }
}
