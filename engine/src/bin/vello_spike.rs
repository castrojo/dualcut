//! M0 vector spike: Vello rendering the roadmap's shape set (rounded rect,
//! circle, star) on the GPU, headless, saved as PNG. This is the future
//! shapes/titles compositor; M3 wires it into the GES pipeline as a source.

use anyhow::{Context, Result};
use vello::kurbo::{Affine, BezPath, Circle, Point, RoundedRect};
use vello::peniko::{color::palette, Color, Fill};
use vello::wgpu;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;

fn star(center: Point, points: u32, outer: f64, inner: f64) -> BezPath {
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

fn build_scene() -> Scene {
    let mut scene = Scene::new();
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgb8(0x0b, 0x0d, 0x12),
        None,
        &RoundedRect::new(0.0, 0.0, WIDTH as f64, HEIGHT as f64, 0.0),
    );
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgb8(0x54, 0x68, 0xff),
        None,
        &RoundedRect::new(80.0, 120.0, 520.0, 400.0, 32.0),
    );
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        Color::from_rgb8(0x5d, 0xd3, 0x9e),
        None,
        &Circle::new((700.0, 260.0), 140.0),
    );
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        palette::css::GOLD,
        None,
        &star(Point::new(1020.0, 420.0), 5, 180.0, 72.0),
    );
    scene
}

fn main() -> Result<()> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
        apply_limit_buckets: false,
    }))
    .context("no wgpu adapter")?;
    println!("adapter: {}", adapter.get_info().name);
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .context("requesting device")?;

    let mut renderer =
        Renderer::new(&device, RendererOptions::default()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vello target"),
        size: wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let scene = build_scene();
    renderer
        .render_to_texture(
            &device,
            &queue,
            &scene,
            &view,
            &RenderParams {
                base_color: Color::from_rgb8(0, 0, 0),
                width: WIDTH,
                height: HEIGHT,
                antialiasing_method: AaConfig::Area,
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Read the texture back and save as PNG.
    let bytes_per_row = (WIDTH * 4).next_multiple_of(256);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * HEIGHT) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
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
        wgpu::Extent3d { width: WIDTH, height: HEIGHT, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback buffer"));
    device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let data = slice.get_mapped_range().context("mapping readback buffer")?;

    let mut img = image::RgbaImage::new(WIDTH, HEIGHT);
    for y in 0..HEIGHT {
        let row = &data[(y * bytes_per_row) as usize..][..(WIDTH * 4) as usize];
        for x in 0..WIDTH {
            let i = (x * 4) as usize;
            img.put_pixel(x, y, image::Rgba([row[i], row[i + 1], row[i + 2], row[i + 3]]));
        }
    }
    std::fs::create_dir_all("out")?;
    img.save("out/vello.png")?;
    println!("wrote out/vello.png");
    Ok(())
}
