//! The remote desktop image shared between the session (which draws into it on the
//! runtime) and the UI (which copies it out when told to).
//!
//! The session writes each updated rectangle and asks to notify the UI only when the
//! image goes from "seen" to "changed", so at most one notification is in flight however
//! fast the server draws: a slow UI sees fewer, larger updates instead of a growing queue.

use std::sync::{Arc, Mutex};

/// A copy of the image for the UI: BGRA, opaque, `width * 4` bytes per row.
#[derive(Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Pixels.
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for FrameSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSnapshot")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Inner {
    width: u16,
    height: u16,
    bgra: Vec<u8>,
    changed: bool,
}

/// The shared desktop image. Cloning shares it.
#[derive(Clone, Default)]
pub struct FrameBuffer(Arc<Mutex<Inner>>);

impl std::fmt::Debug for FrameBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("FrameBuffer")
            .field("width", &g.width)
            .field("height", &g.height)
            .finish_non_exhaustive()
    }
}

impl FrameBuffer {
    /// An empty image.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The image's size.
    pub fn size(&self) -> (u16, u16) {
        let g = self.lock();
        (g.width, g.height)
    }

    /// Start over at a new size (black). Returns whether the UI needs a notification.
    pub fn resize(&self, width: u16, height: u16) -> bool {
        let mut g = self.lock();
        g.width = width;
        g.height = height;
        g.bgra.clear();
        g.bgra
            .resize(usize::from(width) * usize::from(height) * 4, 0);
        for px in g.bgra.as_chunks_mut::<4>().0 {
            px[3] = 0xFF;
        }
        !std::mem::replace(&mut g.changed, true)
    }

    /// Copy the inclusive rectangle (`left`, `top`)–(`right`, `bottom`) from `src` (BGRA,
    /// `stride` bytes per row, the same size as this image). Returns whether the UI needs
    /// a notification (it had seen everything before).
    pub fn write_rect(
        &self,
        src: &[u8],
        stride: usize,
        left: u16,
        top: u16,
        right: u16,
        bottom: u16,
    ) -> bool {
        let mut g = self.lock();
        let (w, h) = (usize::from(g.width), usize::from(g.height));
        if w == 0 || h == 0 {
            return false;
        }
        let x0 = usize::from(left).min(w - 1);
        let x1 = usize::from(right).min(w - 1);
        let y0 = usize::from(top).min(h - 1);
        let y1 = usize::from(bottom).min(h - 1);
        if x0 > x1 || y0 > y1 {
            return false;
        }
        let row_bytes = (x1 - x0 + 1) * 4;
        for y in y0..=y1 {
            let s = y * stride + x0 * 4;
            let d = (y * w + x0) * 4;
            let (Some(from), Some(to)) =
                (src.get(s..s + row_bytes), g.bgra.get_mut(d..d + row_bytes))
            else {
                break;
            };
            to.copy_from_slice(from);
            // Opaque: decoders leave alpha at 0 for some codecs.
            for px in to.as_chunks_mut::<4>().0 {
                px[3] = 0xFF;
            }
        }
        !std::mem::replace(&mut g.changed, true)
    }

    /// A copy of the image if it changed since the last call.
    pub fn take(&self) -> Option<FrameSnapshot> {
        let mut g = self.lock();
        if !std::mem::replace(&mut g.changed, false) || g.width == 0 || g.height == 0 {
            return None;
        }
        Some(FrameSnapshot {
            width: g.width,
            height: g.height,
            bgra: g.bgra.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangles_are_copied_and_notifications_coalesce() {
        let fb = FrameBuffer::new();
        assert!(fb.resize(4, 2), "first change notifies");
        assert!(!fb.resize(4, 2), "not seen yet: no second notification");
        let snap = fb.take().unwrap();
        assert_eq!((snap.width, snap.height), (4, 2));
        assert!(snap.bgra.chunks(4).all(|p| p == [0, 0, 0, 0xFF]));
        assert!(fb.take().is_none(), "nothing new");

        // A 4×2 source, stride 16; copy x 1..=2 of row 1.
        let src: Vec<u8> = (0..32u8).collect();
        assert!(fb.write_rect(&src, 16, 1, 1, 2, 1));
        assert!(!fb.write_rect(&src, 16, 0, 0, 0, 0));
        let snap = fb.take().unwrap();
        // Row 1, x 1: bytes 20..24 with alpha forced opaque.
        assert_eq!(&snap.bgra[20..24], &[20, 21, 22, 0xFF]);
        assert_eq!(&snap.bgra[24..28], &[24, 25, 26, 0xFF]);
        // Pixel (0,0) came from the second write.
        assert_eq!(&snap.bgra[0..4], &[0, 1, 2, 0xFF]);
        // Untouched pixels stay black.
        assert_eq!(&snap.bgra[28..32], &[0, 0, 0, 0xFF]);
    }

    #[test]
    fn out_of_range_rectangles_are_clamped() {
        let fb = FrameBuffer::new();
        fb.resize(2, 2);
        fb.take();
        let src = vec![9u8; 16];
        assert!(fb.write_rect(&src, 8, 1, 1, 500, 500));
        let snap = fb.take().unwrap();
        assert_eq!(&snap.bgra[12..16], &[9, 9, 9, 0xFF]);
        // Empty image: nothing to do.
        let empty = FrameBuffer::new();
        assert!(!empty.write_rect(&src, 8, 0, 0, 1, 1));
        assert!(empty.take().is_none());
    }
}
