//! Pixel tests of the software adapter paths on WARP: solid quad shaders and
//! native fills (`software_rendering`), and cropped and reused path textures
//! (`path_targets`), each against upstream's drawing of the same scene.
//!
//! The scenes are drawn through `fast::frame` into a composition swap chain's
//! buffer (no window needed), as `DirectXRenderer::render` draws a frame.

use std::slice;

use anyhow::{Context as _, Result};
use gpui::{
    Background, Bounds, ContentMask, Corners, Edges, Hsla, Path, PathBuilder, Quad, Rgba,
    ScaledPixels, Scene, linear_color_stop, linear_gradient, point, px, rgba, size,
};
use windows::Win32::{
    Foundation::{HMODULE, HWND},
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        },
        Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory6},
    },
};

use crate::directx_renderer::{
    DirectXGlobalElements, DirectXRenderPipelines, DirectXRendererDevices, DirectXResources,
};
use crate::fast::frame::{FrameState, Target, draw_scene, upload};
use crate::fast::layers::tile_cache::create_globals;
use crate::software_rendering::SoftwareRendering;
use crate::{DirectXAtlas, DirectXDevices};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 400;

#[test]
fn software_quads_match_the_general_shader() {
    let Some(mut warp) = Warp::new() else {
        eprintln!("skipped: no WARP device");
        return;
    };
    for scene in [quad_scene(), mixed_scene(0., false), mixed_scene(0., true)] {
        let general = warp.render(&scene, false);
        let software = warp.render(&scene, true);
        assert_close(&general, &software);
    }
}

#[test]
fn cropped_paths_match_window_sized_paths() {
    let Some(mut warp) = Warp::new() else {
        eprintln!("skipped: no WARP device");
        return;
    };
    for shift in [0., 3.5, 117.25] {
        let scene = mixed_scene(shift, false);
        warp.frame = FrameState::default();
        warp.frame.paths.window_sized_only = true;
        let window_sized = warp.render(&scene, false);
        warp.frame = FrameState::default();
        let cropped = warp.render(&scene, false);
        assert!(
            warp.frame.paths.cropped(),
            "the paths fit a cropped texture"
        );
        assert_close(&window_sized, &cropped);
    }
    // A batch too large to crop draws through the window-sized texture.
    warp.frame = FrameState::default();
    warp.render(&mixed_scene(0., true), false);
    assert!(!warp.frame.paths.cropped());
}

#[test]
fn unchanged_paths_are_reused_and_changed_paths_drawn_again() {
    let Some(mut warp) = Warp::new() else {
        eprintln!("skipped: no WARP device");
        return;
    };
    let first = mixed_scene(0., false);
    let drawn = warp.render(&first, true);
    let rasterized = warp.frame.paths.rasterizations;
    for _ in 0..5 {
        assert_eq!(warp.render(&first, true), drawn);
    }
    assert_eq!(warp.frame.paths.rasterizations, rasterized);

    let moved = mixed_scene(9., false);
    let redrawn = warp.render(&moved, true);
    assert!(warp.frame.paths.rasterizations > rasterized);
    let mut reference = Warp::new().expect("a second WARP device");
    reference.frame.paths.window_sized_only = true;
    assert_close(&reference.render(&moved, false), &redrawn);
}

/// Opaque backgrounds (filled natively), rounded and bordered cards,
/// translucent and fractional quads, and clipping: one solid batch.
fn quad_scene() -> Scene {
    let mut scene = Scene::default();
    scene.insert_primitive(quad(0., 0., WIDTH as f32, HEIGHT as f32, rgba(0x1e1e2eff)));
    scene.insert_primitive(quad(20., 20., 300., 200., rgba(0x313244ff)));
    let mut card = quad(40., 40., 200., 120., rgba(0x45475aff));
    card.corner_radii = Corners::all(ScaledPixels(8.));
    card.border_widths = Edges::all(ScaledPixels(1.));
    card.border_color = rgba(0x89b4faff).into();
    scene.insert_primitive(card);
    scene.insert_primitive(quad(100.5, 80.25, 260.5, 150.75, rgba(0xf38ba880)));
    let mut clipped = quad(300., 100., 300., 280., rgba(0xa6e3a1ff));
    clipped.content_mask.bounds = bounds(310., 110., 200., 150.);
    scene.insert_primitive(clipped);
    scene.finish();
    scene
}

/// [`quad_scene`]'s quads with a gradient and a dashed border (the general
/// shader), small paths shifted by `shift` (and with `large`, a path too big
/// for a cropped texture), and a quad above them.
fn mixed_scene(shift: f32, large: bool) -> Scene {
    let mut scene = Scene::default();
    scene.insert_primitive(quad(0., 0., WIDTH as f32, HEIGHT as f32, rgba(0x1e1e2eff)));
    let mut gradient = quad(380., 220., 200., 120., rgba(0));
    gradient.background = linear_gradient(
        45.,
        linear_color_stop(rgba(0xf9e2afff), 0.),
        linear_color_stop(rgba(0x74c7ecff), 1.),
    );
    scene.insert_primitive(gradient);
    let mut dashed = quad(30., 250., 160., 100., rgba(0x313244ff));
    dashed.border_style = gpui::BorderStyle::Dashed;
    dashed.border_widths = Edges::all(ScaledPixels(2.));
    dashed.border_color = rgba(0xcba6f7ff).into();
    scene.insert_primitive(dashed);
    scene.insert_primitive(triangle(
        60. + shift,
        30.,
        90.,
        linear_gradient(
            90.,
            linear_color_stop(rgba(0xff0000ff), 0.),
            linear_color_stop(rgba(0x0000ffff), 1.),
        ),
    ));
    scene.insert_primitive(triangle(200. + shift, 60., 40., rgba(0x94e2d5c0).into()));
    if large {
        scene.insert_primitive(triangle(10., 10., 380., rgba(0xfab38780).into()));
    }
    scene.insert_primitive(quad(70. + shift, 60., 30., 30., rgba(0xeba0acff)));
    scene.finish();
    scene
}

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(width), ScaledPixels(height)),
    )
}

fn window_mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(0., 0., WIDTH as f32, HEIGHT as f32),
    }
}

fn quad(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> Quad {
    Quad {
        bounds: bounds(x, y, width, height),
        content_mask: window_mask(),
        background: Hsla::from(color).into(),
        ..Default::default()
    }
}

fn triangle(x: f32, y: f32, side: f32, color: Background) -> Path<ScaledPixels> {
    let mut builder = PathBuilder::fill();
    builder.move_to(point(px(x), px(y + side)));
    builder.line_to(point(px(x + side / 2.), px(y)));
    builder.line_to(point(px(x + side), px(y + side)));
    builder.close();
    let mut path = builder.build().expect("a triangle").scale(1.0);
    path.content_mask = window_mask();
    path.color = color;
    path
}

/// Upstream rounds texture coordinates and colors slightly differently in
/// places; allow one 8-bit step per channel.
fn assert_close(expected: &[u8], actual: &[u8]) {
    assert_eq!(expected.len(), actual.len());
    let worst = expected
        .iter()
        .zip(actual)
        .enumerate()
        .max_by_key(|(_, (a, b))| a.abs_diff(**b))
        .map(|(index, (a, b))| (index, a.abs_diff(*b)));
    if let Some((index, difference)) = worst {
        let pixel = index / 4;
        assert!(
            difference <= 1,
            "pixel ({}, {}) differs by {difference}",
            pixel as u32 % WIDTH,
            pixel as u32 / WIDTH
        );
    }
}

/// The renderer's objects on a WARP device, drawing into a composition swap
/// chain's buffer.
struct Warp {
    devices: DirectXRendererDevices,
    resources: DirectXResources,
    atlas: DirectXAtlas,
    globals: DirectXGlobalElements,
    params: Option<ID3D11Buffer>,
    pipelines: DirectXRenderPipelines,
    software: SoftwareRendering,
    frame: FrameState,
}

impl Warp {
    fn new() -> Option<Self> {
        let devices = warp_devices()
            .inspect_err(|error| eprintln!("no WARP device: {error:#}"))
            .ok()?;
        let devices = DirectXRendererDevices::new(&devices, false).ok()?;
        let resources =
            DirectXResources::new(&devices, WIDTH, HEIGHT, HWND::default(), false).ok()?;
        let software = SoftwareRendering::new(&devices)
            .expect("software rendering objects")
            .expect("WARP is a software adapter");
        assert!(software.can_clear(), "WARP supports ClearView");
        Some(Self {
            atlas: DirectXAtlas::new(&devices.device, &devices.device_context),
            globals: DirectXGlobalElements::new(&devices.device).ok()?,
            params: create_globals(&devices.device, WIDTH, HEIGHT).ok()?,
            pipelines: DirectXRenderPipelines::new(&devices.device).ok()?,
            software,
            frame: FrameState::default(),
            devices,
            resources,
        })
    }

    /// Draws `scene` as `DirectXRenderer::render` does, with or without the
    /// software adapter paths, and returns the RGBA pixels.
    fn render(&mut self, scene: &Scene, software: bool) -> Vec<u8> {
        self.draw(scene, software).expect("frame drawn")
    }

    fn draw(&mut self, scene: &Scene, software: bool) -> Result<Vec<u8>> {
        let context = &self.devices.device_context;
        let resources = &self.resources;
        unsafe {
            context.ClearRenderTargetView(
                resources
                    .render_target_view
                    .as_ref()
                    .context("render target")?,
                &[0., 0., 0., 0.],
            );
            context.OMSetRenderTargets(Some(slice::from_ref(&resources.render_target_view)), None);
            context.RSSetViewports(Some(slice::from_ref(&resources.viewport)));
            context.VSSetConstantBuffers(0, Some(slice::from_ref(&self.params)));
            context
                .VSSetConstantBuffers(1, Some(slice::from_ref(&self.globals.batch_params_buffer)));
            context.PSSetConstantBuffers(0, Some(slice::from_ref(&self.params)));
        }
        let target = Target {
            device: &self.devices.device,
            device_context: context,
            atlas: &self.atlas,
            globals: &self.globals,
            resources: Some(resources),
            software: software.then_some(&self.software),
        };
        upload(&target, &mut self.pipelines, scene)?;
        draw_scene(&target, &mut self.pipelines, &mut self.frame, scene)?;
        let texture = resources.render_target.as_ref().context("render target")?;
        read_back(&self.devices.device, context, texture)
    }
}

fn warp_devices() -> Result<DirectXDevices> {
    let dxgi_factory: IDXGIFactory6 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }?;
    let adapter: IDXGIAdapter1 = unsafe { dxgi_factory.EnumWarpAdapter() }?;
    let mut device = None;
    let mut device_context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut device_context),
        )
    }?;
    Ok(DirectXDevices {
        adapter,
        dxgi_factory,
        device: device.context("device")?,
        device_context: device_context.context("device context")?,
    })
}

/// The pixels of a BGRA `texture` as RGBA bytes, row by row.
fn read_back(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<Vec<u8>> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let (width, height) = (desc.Width as usize, desc.Height as usize);
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
    let staging: ID3D11Texture2D = staging.context("creating a staging texture")?;
    let mut pixels = vec![0u8; width * height * 4];
    unsafe {
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        // SAFETY: the mapping holds `RowPitch * height` bytes, `RowPitch` is
        // at least a row's `width * 4`, and `pixels` is a fresh allocation.
        let source = mapped.pData as *const u8;
        for row in 0..height {
            std::ptr::copy_nonoverlapping(
                source.add(row * mapped.RowPitch as usize),
                pixels.as_mut_ptr().add(row * width * 4),
                width * 4,
            );
        }
        context.Unmap(&staging, 0);
    }
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(pixels)
}
