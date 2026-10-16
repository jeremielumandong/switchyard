//! Cheaper quad drawing on software adapters (WARP, "Microsoft Basic Render
//! Driver"), where every pixel shader invocation is CPU time. Switchyard patch,
//! ported from AgentOps' GPUI patch; see `vendor/gpui-fast/PARITY-PATCH.md`.
//!
//! - A batch of quads that are all solid-filled with solid borders draws with
//!   `solid_quad_*` shaders: the invariant colors and geometry travel as vertex
//!   outputs instead of per-pixel structured-buffer reads, and the gradient,
//!   pattern and dash branches are gone. Same distance-field math
//!   (`quad_shading.hlsl`), so edges and antialiasing are unchanged.
//! - A large opaque, unrounded, borderless quad on whole pixels is filled with
//!   `ID3D11DeviceContext1::ClearView` instead of shading and blending each
//!   pixel (window and panel backgrounds).
//!
//! Hardware adapters create none of this and draw exactly as before.

use anyhow::{Context as _, Result};
use gpui::{BorderStyle, Quad, Rgba};
use windows::Win32::{
    Foundation::RECT,
    Graphics::{
        Direct3D11::{
            D3D11_FEATURE_D3D11_OPTIONS, D3D11_FEATURE_DATA_D3D11_OPTIONS, ID3D11DeviceContext1,
            ID3D11PixelShader, ID3D11RenderTargetView, ID3D11VertexShader,
        },
        Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE,
    },
};
use windows::core::Interface as _;

use crate::directx_renderer::shader_resources::{RawShaderBytes, ShaderModule, ShaderTarget};
use crate::directx_renderer::{
    DirectXRendererDevices, create_fragment_shader, create_vertex_shader,
};

/// Small quads are cheap to shade, and each native fill splits the batch into
/// one more draw call; only fill quads at least this large.
const MIN_NATIVE_CLEAR_PIXELS: f32 = 16.0 * 1024.0;

/// What a software adapter draws differently.
pub(crate) struct SoftwareRendering {
    solid_quad_vertex: ID3D11VertexShader,
    solid_quad_fragment: ID3D11PixelShader,
    /// `None` when the device does not support `ClearView`.
    clear_context: Option<ID3D11DeviceContext1>,
}

impl SoftwareRendering {
    /// `None` on a hardware adapter.
    pub(crate) fn new(devices: &DirectXRendererDevices) -> Result<Option<Self>> {
        let desc = unsafe { devices.adapter.GetDesc1() }.context("Reading DirectX adapter")?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            return Ok(None);
        }
        let solid_quad_vertex = create_vertex_shader(
            &devices.device,
            RawShaderBytes::new(ShaderModule::SolidQuad, ShaderTarget::Vertex)?.as_bytes(),
        )?;
        let solid_quad_fragment = create_fragment_shader(
            &devices.device,
            RawShaderBytes::new(ShaderModule::SolidQuad, ShaderTarget::Fragment)?.as_bytes(),
        )?;
        let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
        let clear_view = unsafe {
            devices.device.CheckFeatureSupport(
                D3D11_FEATURE_D3D11_OPTIONS,
                (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS).cast(),
                std::mem::size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.ClearView.as_bool();
        let clear_context = clear_view
            .then(|| devices.device_context.cast().ok())
            .flatten();
        log::info!("Software DirectX adapter: specialized quad shaders, ClearView: {clear_view}");
        Ok(Some(Self {
            solid_quad_vertex,
            solid_quad_fragment,
            clear_context,
        }))
    }

    /// The shaders to draw `quads` with, when the whole batch can use the
    /// solid specialization. Gradients, patterns and dashed borders cannot.
    pub(crate) fn solid_quad_shaders(
        &self,
        quads: &[Quad],
    ) -> Option<(&ID3D11VertexShader, &ID3D11PixelShader)> {
        let solid = !quads.is_empty()
            && quads.iter().all(|quad| {
                quad.background.as_solid().is_some() && quad.border_style == BorderStyle::Solid
            });
        solid.then_some((&self.solid_quad_vertex, &self.solid_quad_fragment))
    }

    /// Fills `rect` of `target` with `color`, without a shader.
    pub(crate) fn clear(
        &self,
        target: &ID3D11RenderTargetView,
        rect: RECT,
        color: [f32; 4],
    ) -> bool {
        let Some(context) = &self.clear_context else {
            return false;
        };
        unsafe { context.ClearView(target, &color, Some(&[rect])) };
        true
    }

    pub(crate) fn can_clear(&self) -> bool {
        self.clear_context.is_some()
    }
}

/// The rectangle and color to fill natively in place of shading `quad` on a
/// `width` × `height` target, if that gives the same pixels: an opaque solid
/// fill with no rounding or border, on whole pixels (no antialiased edge).
pub(crate) fn native_quad_clear(quad: &Quad, width: u32, height: u32) -> Option<(RECT, [f32; 4])> {
    // Most quads are small: reject them before checking anything else.
    // Clipping can only make the filled area smaller.
    if quad.bounds.size.width.0 * quad.bounds.size.height.0 < MIN_NATIVE_CLEAR_PIXELS {
        return None;
    }
    let color = quad.background.as_solid()?;
    if color.a != 1.
        || ![color.h, color.s, color.l]
            .iter()
            .all(|v| (0.0..=1.0).contains(v))
        || ![
            quad.corner_radii.top_left.0,
            quad.corner_radii.top_right.0,
            quad.corner_radii.bottom_left.0,
            quad.corner_radii.bottom_right.0,
            quad.border_widths.top.0,
            quad.border_widths.right.0,
            quad.border_widths.bottom.0,
            quad.border_widths.left.0,
        ]
        .iter()
        .all(|v| *v == 0.)
    {
        return None;
    }
    for bounds in [quad.bounds, quad.content_mask.bounds] {
        if ![
            bounds.left().0,
            bounds.top().0,
            bounds.right().0,
            bounds.bottom().0,
        ]
        .iter()
        .all(|v| v.is_finite() && v.fract() == 0.)
        {
            return None;
        }
    }
    let left = quad
        .bounds
        .left()
        .0
        .max(quad.content_mask.bounds.left().0)
        .max(0.);
    let top = quad
        .bounds
        .top()
        .0
        .max(quad.content_mask.bounds.top().0)
        .max(0.);
    let right = quad
        .bounds
        .right()
        .0
        .min(quad.content_mask.bounds.right().0)
        .min(width as f32);
    let bottom = quad
        .bounds
        .bottom()
        .0
        .min(quad.content_mask.bounds.bottom().0)
        .min(height as f32);
    if left >= right || top >= bottom || (right - left) * (bottom - top) < MIN_NATIVE_CLEAR_PIXELS {
        return None;
    }
    let color = Rgba::from(color);
    Some((
        RECT {
            left: left as i32,
            top: top as i32,
            right: right as i32,
            bottom: bottom as i32,
        },
        [color.r, color.g, color.b, color.a],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        Background, Bounds, ContentMask, Corners, Edges, Hsla, ScaledPixels, linear_color_stop,
        linear_gradient, point, size,
    };

    fn quad(x: f32, y: f32, w: f32, h: f32, background: Background) -> Quad {
        let bounds = Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(w), ScaledPixels(h)),
        );
        Quad {
            order: 0,
            border_style: BorderStyle::Solid,
            bounds,
            content_mask: ContentMask {
                bounds: Bounds::new(
                    point(ScaledPixels(0.), ScaledPixels(0.)),
                    size(ScaledPixels(1920.), ScaledPixels(1080.)),
                ),
            },
            background,
            border_color: Hsla::default(),
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
        }
    }

    fn opaque() -> Background {
        Hsla {
            h: 0.6,
            s: 0.2,
            l: 0.3,
            a: 1.,
        }
        .into()
    }

    #[test]
    fn large_opaque_rectangles_are_filled_within_their_clip() {
        let mut panel = quad(-10., 100., 2000., 600., opaque());
        panel.content_mask.bounds.size.height = ScaledPixels(500.);
        let (rect, color) = native_quad_clear(&panel, 1920, 1080).unwrap();
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (0, 100, 1920, 500)
        );
        assert_eq!(color[3], 1.);
    }

    #[test]
    fn edges_rounding_borders_translucency_and_small_quads_keep_the_shader() {
        assert!(native_quad_clear(&quad(0.5, 0., 400., 400., opaque()), 1920, 1080).is_none());
        let mut rounded = quad(0., 0., 400., 400., opaque());
        rounded.corner_radii.top_left = ScaledPixels(4.);
        assert!(native_quad_clear(&rounded, 1920, 1080).is_none());
        let mut bordered = quad(0., 0., 400., 400., opaque());
        bordered.border_widths.left = ScaledPixels(1.);
        assert!(native_quad_clear(&bordered, 1920, 1080).is_none());
        let translucent = Hsla {
            a: 0.5,
            ..Hsla::default()
        };
        assert!(
            native_quad_clear(&quad(0., 0., 400., 400., translucent.into()), 1920, 1080).is_none()
        );
        assert!(native_quad_clear(&quad(0., 0., 100., 100., opaque()), 1920, 1080).is_none());
        let gradient = linear_gradient(
            0.,
            linear_color_stop(Hsla::default(), 0.),
            linear_color_stop(Hsla::default(), 1.),
        );
        assert!(native_quad_clear(&quad(0., 0., 400., 400., gradient), 1920, 1080).is_none());
    }
}

#[cfg(test)]
#[path = "software_rendering_tests.rs"]
mod pixel_tests;
