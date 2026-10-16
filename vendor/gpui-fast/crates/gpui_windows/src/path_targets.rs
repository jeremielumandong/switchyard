//! Path batches rasterized into a texture no larger than they need, and an
//! unchanged batch not rasterized again. Switchyard patch, ported from
//! AgentOps' GPUI patch; see `vendor/gpui-fast/PARITY-PATCH.md`.
//!
//! Upstream clears, rasterizes (4× MSAA) and resolves a window-sized texture
//! for every path batch, however small the paths are. On a software adapter
//! that is the cost of several full frames for a popover's arrow. A batch
//! whose paths fit in a bounded rectangle uses a texture of that size instead,
//! placed by translating the (window-sized) viewport by a whole, even number
//! of pixels, so sample positions and the shader's `ddx`/`ddy` quads are the
//! same as upstream's. The resolved texture of the last batch is kept, and a
//! batch whose vertices are the same draws from it without rasterizing.

use anyhow::Result;
use gpui::{Path, Point, ScaledPixels, point};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
};

use crate::directx_renderer::{
    DirectXResources, PathRasterizationSprite, create_path_intermediate_msaa_texture_and_view,
    create_path_intermediate_texture,
};

/// Cropped textures grow in steps of this many pixels, so that consecutive
/// batches of slightly different sizes share one texture.
const TARGET_BUCKET: u32 = 64;
/// The largest cropped texture (8 MiB resolved, 32 MiB with 4× MSAA);
/// larger batches use the window-sized texture.
const TARGET_MAX_PIXELS: u64 = 1024 * 1024;

struct PathTarget {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
    srv: Option<ID3D11ShaderResourceView>,
    msaa_texture: ID3D11Texture2D,
    msaa_view: Option<ID3D11RenderTargetView>,
}

/// The textures one path batch is drawn through.
pub(crate) struct PathViews<'a> {
    pub(crate) texture: &'a ID3D11Texture2D,
    pub(crate) srv: &'a Option<ID3D11ShaderResourceView>,
    pub(crate) msaa_texture: &'a ID3D11Texture2D,
    pub(crate) msaa_view: &'a Option<ID3D11RenderTargetView>,
    /// Where the texture's top-left pixel sits in the window.
    pub(crate) origin: Point<ScaledPixels>,
    pub(crate) size: [f32; 2],
}

/// The renderer's path textures beyond the window-sized ones, kept from
/// frame to frame.
#[derive(Default)]
pub(crate) struct PathTargets {
    cropped: Option<PathTarget>,
    /// Whether the current batch uses `cropped` rather than the window-sized
    /// texture, and where it sits.
    using_cropped: bool,
    origin: Point<ScaledPixels>,
    /// Whether the texture in use holds the resolved last batch, whose
    /// vertices the frame state keeps.
    resolved: bool,
    /// Draw every batch through the window-sized texture, as upstream does
    /// (the reference in pixel tests).
    #[cfg(test)]
    pub(crate) window_sized_only: bool,
    /// How many batches were rasterized.
    #[cfg(test)]
    pub(crate) rasterizations: usize,
}

impl PathTargets {
    /// Releases the cropped texture: the window's size or device changed.
    pub(crate) fn release(&mut self) {
        self.cropped = None;
        self.using_cropped = false;
        self.origin = Point::default();
        self.resolved = false;
    }

    /// Something else drew into the path textures.
    pub(crate) fn invalidate(&mut self) {
        self.resolved = false;
    }

    /// Whether the last batch used the cropped texture.
    #[cfg(test)]
    pub(crate) fn cropped(&self) -> bool {
        self.using_cropped
    }

    pub(crate) fn mark_resolved(&mut self) {
        self.resolved = true;
        #[cfg(test)]
        {
            self.rasterizations += 1;
        }
    }

    /// Picks (and if needed allocates) the texture `paths` are rasterized
    /// into on a `width` × `height` window. Returns whether that texture
    /// still holds the last batch resolved at the same place; the caller then
    /// checks whether its vertices are the same.
    pub(crate) fn select(
        &mut self,
        device: &ID3D11Device,
        paths: &[Path<ScaledPixels>],
        width: u32,
        height: u32,
    ) -> Result<bool> {
        let retained = self
            .cropped
            .as_ref()
            .map(|target| [target.width, target.height]);
        let plan = crop_plan(paths, width, height, retained);
        #[cfg(test)]
        let plan = plan.filter(|_| !self.window_sized_only);
        let origin = plan.map_or(Point::default(), |plan| plan.origin);
        let size_matches = plan.is_none_or(|plan| retained == Some([plan.width, plan.height]));
        if self.resolved
            && size_matches
            && self.using_cropped == plan.is_some()
            && self.origin == origin
        {
            return Ok(true);
        }
        // Invalidate before changing anything: a failed allocation must not
        // leave a partial result that looks reusable.
        self.resolved = false;
        if let Some(plan) = plan.filter(|_| !size_matches) {
            self.cropped = None;
            let (texture, srv) = create_path_intermediate_texture(device, plan.width, plan.height)?;
            let (msaa_texture, msaa_view) =
                create_path_intermediate_msaa_texture_and_view(device, plan.width, plan.height)?;
            self.cropped = Some(PathTarget {
                width: plan.width,
                height: plan.height,
                texture,
                srv,
                msaa_texture,
                msaa_view,
            });
        }
        self.using_cropped = plan.is_some();
        self.origin = origin;
        Ok(false)
    }

    /// The textures [`select`](Self::select) picked.
    pub(crate) fn views<'a>(&'a self, resources: &'a DirectXResources) -> PathViews<'a> {
        match self.cropped.as_ref().filter(|_| self.using_cropped) {
            Some(target) => PathViews {
                texture: &target.texture,
                srv: &target.srv,
                msaa_texture: &target.msaa_texture,
                msaa_view: &target.msaa_view,
                origin: self.origin,
                size: [target.width as f32, target.height as f32],
            },
            None => PathViews {
                texture: &resources.path_intermediate_texture,
                srv: &resources.path_intermediate_srv,
                msaa_texture: &resources.path_intermediate_msaa_texture,
                msaa_view: &resources.path_intermediate_msaa_view,
                origin: Point::default(),
                size: [resources.viewport.Width, resources.viewport.Height],
            },
        }
    }
}

/// The vertices `paths` are rasterized with into a texture at `origin`.
pub(crate) fn rasterization_vertices(
    paths: &[Path<ScaledPixels>],
    origin: Point<ScaledPixels>,
) -> impl Iterator<Item = PathRasterizationSprite> + '_ {
    paths.iter().flat_map(move |path| {
        let bounds = path.clipped_bounds();
        path.vertices
            .iter()
            .map(move |vertex| PathRasterizationSprite {
                xy_position: vertex.xy_position,
                st_position: vertex.st_position,
                color: path.color,
                bounds,
                screen_origin: origin,
            })
    })
}

/// Whether `previous` are the vertices `paths` would be rasterized with.
pub(crate) fn same_vertices(
    previous: &[PathRasterizationSprite],
    paths: &[Path<ScaledPixels>],
    origin: Point<ScaledPixels>,
) -> bool {
    let mut current = rasterization_vertices(paths, origin);
    previous.iter().all(|previous| {
        current.next().is_some_and(|current| {
            previous.xy_position == current.xy_position
                && previous.st_position == current.st_position
                && previous.color == current.color
                && previous.bounds == current.bounds
                && previous.screen_origin == current.screen_origin
        })
    }) && current.next().is_none()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CropPlan {
    origin: Point<ScaledPixels>,
    width: u32,
    height: u32,
}

/// The cropped texture for `paths`, or `None` for the window-sized one: when
/// it would be larger than [`TARGET_MAX_PIXELS`] or half the window. A
/// retained texture only grows, so that alternating batches do not reallocate.
fn crop_plan(
    paths: &[Path<ScaledPixels>],
    width: u32,
    height: u32,
    retained: Option<[u32; 2]>,
) -> Option<CropPlan> {
    let [left, top, right, bottom] = crop_bounds(paths, width, height)?;
    let bucket = |span: f32| (span.max(1.0) as u32).div_ceil(TARGET_BUCKET) * TARGET_BUCKET;
    let mut target_width = bucket(right - left);
    let mut target_height = bucket(bottom - top);
    if let Some([retained_width, retained_height]) = retained {
        target_width = target_width.max(retained_width);
        target_height = target_height.max(retained_height);
    }
    let target_pixels = u64::from(target_width) * u64::from(target_height);
    let window_pixels = u64::from(width) * u64::from(height);
    if target_pixels > TARGET_MAX_PIXELS || target_pixels * 2 > window_pixels {
        return None;
    }
    Some(CropPlan {
        origin: point(ScaledPixels(left), ScaledPixels(top)),
        width: target_width,
        height: target_height,
    })
}

/// The window-clipped bounds of `paths`, with a transparent pixel around them
/// for the linear sampler, and the top-left corner on even pixels so that the
/// path shader's 2×2 derivative quads line up with upstream's.
fn crop_bounds(paths: &[Path<ScaledPixels>], width: u32, height: u32) -> Option<[f32; 4]> {
    if paths.is_empty() {
        return None;
    }
    let mut left = width as f32;
    let mut top = height as f32;
    let mut right = 0.0f32;
    let mut bottom = 0.0f32;
    for path in paths {
        let bounds = path.clipped_bounds();
        let edges = [
            bounds.left().0,
            bounds.top().0,
            bounds.right().0,
            bounds.bottom().0,
        ];
        if edges.iter().any(|edge| !edge.is_finite()) {
            return None;
        }
        left = left.min(edges[0].clamp(0.0, width as f32));
        top = top.min(edges[1].clamp(0.0, height as f32));
        right = right.max(edges[2].clamp(0.0, width as f32));
        bottom = bottom.max(edges[3].clamp(0.0, height as f32));
    }
    left = ((left.floor() - 1.0).max(0.0) / 2.0).floor() * 2.0;
    top = ((top.floor() - 1.0).max(0.0) / 2.0).floor() * 2.0;
    right = (right.ceil() + 1.0).min(width as f32);
    bottom = (bottom.ceil() + 1.0).min(height as f32);
    Some([left, top, right, bottom])
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, ContentMask, PathBuilder, px, size};

    fn rectangle(x: f32, y: f32, width: f32, height: f32) -> Path<ScaledPixels> {
        let mut builder = PathBuilder::fill();
        builder.move_to(gpui::point(px(x), px(y)));
        builder.line_to(gpui::point(px(x + width), px(y)));
        builder.line_to(gpui::point(px(x + width), px(y + height)));
        builder.line_to(gpui::point(px(x), px(y + height)));
        builder.close();
        let mut path = builder.build().unwrap().scale(1.0);
        path.content_mask = ContentMask {
            bounds: Bounds::new(
                point(ScaledPixels(-10_000.), ScaledPixels(-10_000.)),
                size(ScaledPixels(20_000.), ScaledPixels(20_000.)),
            ),
        };
        path
    }

    #[test]
    fn small_batches_get_a_bucketed_texture_on_even_pixels() {
        let plan = crop_plan(&[rectangle(101.5, 51.25, 40., 20.)], 1920, 1080, None).unwrap();
        assert_eq!(plan.origin, point(ScaledPixels(100.), ScaledPixels(50.)));
        assert_eq!((plan.width, plan.height), (64, 64));
    }

    #[test]
    fn clipping_and_window_edges_bound_the_texture() {
        let mut clipped = rectangle(100., 100., 1500., 900.);
        clipped.content_mask.bounds = Bounds::new(
            point(ScaledPixels(100.), ScaledPixels(100.)),
            size(ScaledPixels(100.), ScaledPixels(30.)),
        );
        let plan = crop_plan(&[clipped], 1920, 1080, None).unwrap();
        assert_eq!((plan.width, plan.height), (128, 64));
        let plan = crop_plan(&[rectangle(-10., -20., 40., 50.)], 1920, 1080, None).unwrap();
        assert_eq!(plan.origin, point(ScaledPixels(0.), ScaledPixels(0.)));
    }

    #[test]
    fn large_batches_and_small_windows_use_the_window_texture() {
        assert!(crop_plan(&[rectangle(0., 0., 1500., 900.)], 1920, 1080, None).is_none());
        let far_apart = [
            rectangle(0., 0., 10., 10.),
            rectangle(1800., 1000., 10., 10.),
        ];
        assert!(crop_plan(&far_apart, 1920, 1080, None).is_none());
        assert!(crop_plan(&[rectangle(10., 10., 20., 20.)], 64, 64, None).is_none());
    }

    #[test]
    fn a_retained_texture_only_grows() {
        let plan = crop_plan(&[rectangle(0., 0., 10., 10.)], 1920, 1080, Some([256, 128])).unwrap();
        assert_eq!((plan.width, plan.height), (256, 128));
    }

    #[test]
    fn non_finite_geometry_uses_the_window_texture() {
        let mut path = rectangle(0., 0., 10., 10.);
        path.bounds.origin.x = ScaledPixels(f32::NAN);
        assert!(crop_plan(&[path], 1920, 1080, None).is_none());
    }

    #[test]
    fn vertices_match_only_the_same_batch_at_the_same_place() {
        let paths = [rectangle(10., 10., 20., 20.)];
        let origin = point(ScaledPixels(8.), ScaledPixels(8.));
        let vertices = rasterization_vertices(&paths, origin).collect::<Vec<_>>();
        assert!(same_vertices(&vertices, &paths, origin));
        assert!(!same_vertices(&vertices, &paths, Point::default()));
        assert!(!same_vertices(
            &vertices,
            &[rectangle(10., 10., 20., 21.)],
            origin
        ));
        assert!(!same_vertices(&vertices[1..], &paths, origin));
    }
}
