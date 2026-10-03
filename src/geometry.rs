//! Geometry primitives and the coordinate transform between source and image
//! space.
//!
//! ## Conventions
//!
//! All coordinates use a **top-left origin**, `x` increases to the right and
//! `y` increases downwards. Units are **source pixels** unless a type is
//! explicitly named `Image*`.
//!
//! The mapping from returned-image coordinates back to source-desktop
//! coordinates is:
//!
//! ```text
//! source_x = offset_x + image_x * scale_x
//! source_y = offset_y + image_y * scale_y
//! ```

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// A rectangle in integer pixel space.
///
/// The origin is the top-left corner and the rectangle is *not* inclusive of
/// its right/bottom edges: a rect at `(0, 0)` of size `1 x 1` covers exactly the
/// pixel `(0, 0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Create a rectangle, validating the size.
    ///
    /// Zero-sized rectangles are rejected because they can never correspond to
    /// a real captured image.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            return Err(Error::invalid_region(format!(
                "dimensions must be non-zero, got {width}x{height}"
            )));
        }
        Ok(Rect {
            x,
            y,
            width,
            height,
        })
    }

    /// Create a rectangle covering a whole `width x height` display.
    pub fn display(width: u32, height: u32) -> Result<Self, Error> {
        Self::new(0, 0, width, height)
    }

    /// Exclusive right edge.
    pub fn right(&self) -> i64 {
        self.x as i64 + self.width as i64
    }

    /// Exclusive bottom edge.
    pub fn bottom(&self) -> i64 {
        self.y as i64 + self.height as i64
    }

    /// Number of pixels in the rectangle, computed without overflow.
    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// True when both dimensions are zero.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Verify that this rectangle lies fully inside `bounds`.
    ///
    /// Silent clipping is never performed; an out-of-bounds rectangle is a hard
    /// error.
    pub fn ensure_within(&self, bounds: &Rect) -> Result<(), Error> {
        let inside_x = self.x as i64 >= bounds.x as i64 && self.right() <= bounds.right();
        let inside_y = self.y as i64 >= bounds.y as i64 && self.bottom() <= bounds.bottom();
        if !inside_x || !inside_y {
            return Err(Error::invalid_region(format!(
                "region {}x{}+{}+{} exceeds display bounds {}x{}+{}+{}",
                self.width,
                self.height,
                self.x,
                self.y,
                bounds.width,
                bounds.height,
                bounds.x,
                bounds.y,
            )));
        }
        Ok(())
    }
}

/// The kind of capture that produced a frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureTarget {
    /// The full root-window / display area.
    Desktop,
    /// A rectangular region of the source desktop.
    Region,
    /// A specific X11 window identified by its window ID.
    Window { id: String },
    /// An image decoded from a file rather than captured from a display.
    ///
    /// Used by `eensh diff`, which compares saved observations without touching
    /// X11. It carries no display because a decoded image has no screen geometry.
    File { path: String },
}

/// Describes where the captured pixels came from on the source desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceGeometry {
    /// The target that was captured. Flattened so that the JSON response reads
    /// `"source": { "kind": "desktop", ... }` rather than nesting the kind.
    #[serde(flatten)]
    pub target: CaptureTarget,
    /// The X11 display that was used, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Source x offset of the captured rectangle within the desktop.
    pub x: i32,
    /// Source y offset of the captured rectangle within the desktop.
    pub y: i32,
    /// Width of the captured region in source pixels.
    pub width: u32,
    /// Height of the captured region in source pixels.
    pub height: u32,
}

impl SourceGeometry {
    /// Build source geometry for a full-desktop capture.
    pub fn desktop(display: Option<String>, width: u32, height: u32) -> Self {
        SourceGeometry {
            target: CaptureTarget::Desktop,
            display,
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Build source geometry for a rectangular region capture.
    pub fn region(display: Option<String>, x: i32, y: i32, width: u32, height: u32) -> Self {
        SourceGeometry {
            target: CaptureTarget::Region,
            display,
            x,
            y,
            width,
            height,
        }
    }

    /// Build source geometry for a window capture.
    pub fn window(
        display: Option<String>,
        id: String,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Self {
        SourceGeometry {
            target: CaptureTarget::Window { id },
            display,
            x,
            y,
            width,
            height,
        }
    }

    /// The source rectangle.
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// Explicit mapping from returned-image coordinates to source coordinates.
///
/// `offset_*` is the source-space position of image pixel `(0, 0)` and
/// `scale_*` is the number of source pixels per image pixel.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub origin: TransformOrigin,
    pub offset_x: i32,
    pub offset_y: i32,
    pub scale_x: f64,
    pub scale_y: f64,
}

/// The origin convention used by a [`Transform`]. Always `top-left` in Phase 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TransformOrigin {
    #[default]
    TopLeft,
}

impl std::fmt::Display for TransformOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransformOrigin::TopLeft => f.write_str("top-left"),
        }
    }
}

impl Transform {
    /// Build a transform from a source rectangle and an output image size.
    ///
    /// The returned scale factors are exact rational values expressed as `f64`.
    pub fn new(source: &Rect, image_width: u32, image_height: u32) -> Result<Self, Error> {
        if image_width == 0 || image_height == 0 {
            return Err(Error::internal("image dimensions must be non-zero"));
        }
        Ok(Transform {
            origin: TransformOrigin::TopLeft,
            offset_x: source.x,
            offset_y: source.y,
            scale_x: source.width as f64 / image_width as f64,
            scale_y: source.height as f64 / image_height as f64,
        })
    }

    /// Map an image-space point to the corresponding source-desktop point.
    ///
    /// The result is rounded to the nearest source pixel.
    pub fn image_to_source(&self, image_x: f64, image_y: f64) -> (i32, i32) {
        let sx = self.offset_x as f64 + image_x * self.scale_x;
        let sy = self.offset_y as f64 + image_y * self.scale_y;
        (sx.round() as i32, sy.round() as i32)
    }

    /// Map a source-desktop point to the corresponding image-space point.
    ///
    /// The result is rounded to the nearest image pixel.
    pub fn source_to_image(&self, source_x: f64, source_y: f64) -> (i32, i32) {
        let ix = (source_x - self.offset_x as f64) / self.scale_x;
        let iy = (source_y - self.offset_y as f64) / self.scale_y;
        (ix.round() as i32, iy.round() as i32)
    }
}

/// Compute output dimensions for a proportional resize to a target width.
///
/// The aspect ratio is preserved. The height is derived from the *rounded*
/// width so that the returned integer dimensions are consistent with the scale
/// factors actually applied downstream. At least one pixel is always produced.
pub fn resize_to_width(source_width: u32, source_height: u32, target_width: u32) -> (u32, u32) {
    let height = (source_height as f64 * target_width as f64 / source_width as f64).round();
    (target_width, (height as u32).max(1))
}

/// Compute output dimensions for a proportional resize to a target height.
pub fn resize_to_height(source_width: u32, source_height: u32, target_height: u32) -> (u32, u32) {
    let width = (source_width as f64 * target_height as f64 / source_height as f64).round();
    ((width as u32).max(1), target_height)
}

/// Compute output dimensions for a uniform scale factor.
pub fn resize_by_scale(source_width: u32, source_height: u32, scale: f64) -> (u32, u32) {
    let width = (source_width as f64 * scale).round();
    let height = (source_height as f64 * scale).round();
    ((width as u32).max(1), (height as u32).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_sized_rects() {
        assert!(Rect::new(0, 0, 0, 10).is_err());
        assert!(Rect::new(0, 0, 10, 0).is_err());
        assert!(Rect::new(0, 0, 10, 10).is_ok());
    }

    #[test]
    fn edges_and_pixel_count() {
        let rect = Rect::new(640, 200, 640, 480).unwrap();
        assert_eq!(rect.right(), 1280);
        assert_eq!(rect.bottom(), 680);
        assert_eq!(rect.pixel_count(), 307_200);
    }

    #[test]
    fn bounds_checking_rejects_out_of_range_even_with_negative_origin() {
        let bounds = Rect::display(1920, 1080).unwrap();
        assert!(Rect::new(0, 0, 1920, 1080)
            .unwrap()
            .ensure_within(&bounds)
            .is_ok());
        // Right edge exactly at the boundary is fine.
        assert!(Rect::new(1920 - 10, 1080 - 10, 10, 10)
            .unwrap()
            .ensure_within(&bounds)
            .is_ok());
        // One pixel past the right/bottom edge is not.
        assert!(Rect::new(1911, 0, 10, 10)
            .unwrap()
            .ensure_within(&bounds)
            .is_err());
        assert!(Rect::new(0, 1071, 10, 10)
            .unwrap()
            .ensure_within(&bounds)
            .is_err());
        // Negative origin is not.
        assert!(Rect::new(-1, 0, 10, 10)
            .unwrap()
            .ensure_within(&bounds)
            .is_err());
    }

    #[test]
    fn identity_transform_round_trips() {
        let source = Rect::display(1920, 1080).unwrap();
        let transform = Transform::new(&source, 1920, 1080).unwrap();
        assert_eq!(transform.scale_x, 1.0);
        assert_eq!(transform.scale_y, 1.0);
        assert_eq!(transform.image_to_source(100.0, 50.0), (100, 50));
        assert_eq!(transform.source_to_image(100.0, 50.0), (100, 50));
    }

    #[test]
    fn scaled_transform_matches_spec_example() {
        let source = Rect::display(1920, 1080).unwrap();
        let transform = Transform::new(&source, 960, 540).unwrap();
        assert_eq!(transform.offset_x, 0);
        assert_eq!(transform.offset_y, 0);
        assert_eq!(transform.scale_x, 2.0);
        assert_eq!(transform.scale_y, 2.0);
    }

    #[test]
    fn cropped_and_scaled_transform_matches_spec_example() {
        let source = Rect::new(640, 200, 640, 480).unwrap();
        let transform = Transform::new(&source, 320, 240).unwrap();
        assert_eq!(transform.offset_x, 640);
        assert_eq!(transform.offset_y, 200);
        assert_eq!(transform.scale_x, 2.0);
        assert_eq!(transform.scale_y, 2.0);
        // image (100, 50) -> source (840, 300)
        assert_eq!(transform.image_to_source(100.0, 50.0), (840, 300));
        // and back again
        assert_eq!(transform.source_to_image(840.0, 300.0), (100, 50));
    }

    #[test]
    fn resize_preserves_aspect_ratio() {
        assert_eq!(resize_to_width(1920, 1080, 960), (960, 540));
        assert_eq!(resize_to_width(1920, 1080, 640), (640, 360));
        assert_eq!(resize_to_height(1920, 1080, 540), (960, 540));
        assert_eq!(resize_by_scale(1920, 1080, 0.5), (960, 540));
    }

    #[test]
    fn resize_handles_odd_dimensions_and_tiny_targets() {
        // 1001x777 -> width 500 rounds the height.
        let (w, h) = resize_to_width(1001, 777, 500);
        assert_eq!(w, 500);
        assert_eq!(h, 388); // round(777 * 500 / 1001) = round(388.11) = 388
                            // Degenerate scale never yields a zero dimension.
        assert_eq!(resize_by_scale(1920, 1080, 0.0001), (1, 1));
        assert_eq!(resize_to_width(1920, 1080, 1), (1, 1));
    }

    #[test]
    fn transform_rejects_zero_image_dimensions() {
        let source = Rect::display(100, 100).unwrap();
        assert!(Transform::new(&source, 0, 10).is_err());
        assert!(Transform::new(&source, 10, 0).is_err());
    }

    #[test]
    fn transform_origin_serializes_as_kebab_case() {
        let value = serde_json::to_value(TransformOrigin::TopLeft).unwrap();
        assert_eq!(value, serde_json::json!("top-left"));
    }
}
