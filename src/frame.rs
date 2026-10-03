//! The raw captured frame: the central abstraction of the capture pipeline.
//!
//! The X11 backend produces a [`Frame`] and nothing else. Encoding, resizing,
//! base64 conversion and presentation all happen *after* this point and can be
//! swapped or repeated without re-capturing. That separation is what later
//! phases (frame comparison, temporal observation, persistent capture) depend
//! on, so it is deliberately enforced by the module boundaries.

use std::time::SystemTime;

use crate::error::Error;
use crate::geometry::{Rect, SourceGeometry};

/// A single logical pixel, kept in a single canonical in-memory layout.
///
/// Phase 1 uses one pixel format only. The enum exists so that additional
/// formats can be added later without changing the `Frame` shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Packed 8-bit RGB with no padding, in `R, G, B` order.
    Rgb8,
}

impl PixelFormat {
    /// Number of bytes required for one pixel.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb8 => 3,
        }
    }
}

/// A contiguous, row-major, tightly packed pixel buffer.
///
/// The buffer is exactly `width * height * bytes_per_pixel` bytes long; there
/// is no row padding.
#[derive(Clone, PartialEq, Eq)]
pub struct PixelBuffer {
    width: u32,
    height: u32,
    format: PixelFormat,
    data: Vec<u8>,
}

impl PixelBuffer {
    /// Create a pixel buffer, validating that the buffer length matches the
    /// declared dimensions.
    pub fn new(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> Result<Self, Error> {
        if width == 0 || height == 0 {
            return Err(Error::internal(format!(
                "pixel buffer dimensions must be non-zero, got {width}x{height}"
            )));
        }
        let expected = width as usize * height as usize * format.bytes_per_pixel();
        if data.len() != expected {
            return Err(Error::internal(format!(
                "pixel buffer length mismatch: expected {expected} bytes for {width}x{height} {:?}, got {}",
                format,
                data.len()
            )));
        }
        Ok(PixelBuffer {
            width,
            height,
            format,
            data,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// The raw bytes, tightly packed row-major.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consume the buffer and return its raw bytes.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

impl std::fmt::Debug for PixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelBuffer")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// A raw captured image plus the metadata needed to interpret it.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Where these pixels came from on the source desktop.
    pub source_geometry: SourceGeometry,
    /// The in-memory pixel layout of [`Frame::pixels`].
    pub pixel_format: PixelFormat,
    /// The raw pixels.
    pub pixels: PixelBuffer,
    /// The monotonic instant at which the capture completed.
    pub captured_at: std::time::Instant,
}

impl Frame {
    /// Assemble a frame from a captured geometry and pixel buffer.
    pub fn new(
        source_geometry: SourceGeometry,
        pixels: PixelBuffer,
        captured_at: std::time::Instant,
    ) -> Self {
        Frame {
            source_geometry,
            pixel_format: pixels.format(),
            pixels,
            captured_at,
        }
    }

    /// The source rectangle covered by this frame.
    pub fn source_rect(&self) -> Rect {
        self.source_geometry.rect()
    }

    pub fn width(&self) -> u32 {
        self.pixels.width()
    }

    pub fn height(&self) -> u32 {
        self.pixels.height()
    }

    /// Copy a sub-rectangle out of this frame into a new frame.
    ///
    /// `rect` is in frame-local coordinates and must lie entirely inside the
    /// frame; the call fails rather than clipping, matching how capture treats
    /// regions. The result is still a raw frame, so it can be encoded, compared,
    /// or cropped again.
    ///
    /// The returned frame's source geometry is rewritten to describe the cropped
    /// rectangle in the *original source* coordinate space, so a transform
    /// computed from it remains correct.
    pub fn crop(&self, rect: &Rect) -> Result<Frame, Error> {
        let frame_rect = Rect {
            x: 0,
            y: 0,
            width: self.width(),
            height: self.height(),
        };
        rect.ensure_within(&frame_rect)?;

        let bytes_per_pixel = self.pixel_format.bytes_per_pixel();
        let source_stride = self.width() as usize * bytes_per_pixel;
        let row_bytes = rect.width as usize * bytes_per_pixel;
        let mut data = Vec::with_capacity(row_bytes * rect.height as usize);

        for row in rect.y as usize..(rect.y as usize + rect.height as usize) {
            let start = row * source_stride + rect.x as usize * bytes_per_pixel;
            data.extend_from_slice(&self.pixels.data()[start..start + row_bytes]);
        }

        let pixels = PixelBuffer::new(rect.width, rect.height, self.pixel_format, data)?;

        let mut source_geometry = self.source_geometry.clone();
        // Shift the recorded source origin by the crop offset so that a transform
        // derived from the cropped frame still maps into the original desktop.
        source_geometry.x = source_geometry.x.checked_add(rect.x).ok_or_else(|| {
            Error::InvalidRegion("crop origin overflows the source geometry".into())
        })?;
        source_geometry.y = source_geometry.y.checked_add(rect.y).ok_or_else(|| {
            Error::InvalidRegion("crop origin overflows the source geometry".into())
        })?;
        source_geometry.width = rect.width;
        source_geometry.height = rect.height;

        Ok(Frame {
            source_geometry,
            pixel_format: self.pixel_format,
            pixels,
            captured_at: self.captured_at,
        })
    }

    /// Wall-clock timestamp of the capture, derived from the monotonic instant
    /// recorded at capture time.
    ///
    /// This is a best-effort conversion; the authoritative time source for
    /// durations is the monotonic `captured_at` instant.
    pub fn captured_wall_clock(&self) -> SystemTime {
        let elapsed = self.captured_at.elapsed();
        SystemTime::now()
            .checked_sub(elapsed)
            .unwrap_or_else(SystemTime::now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::CaptureTarget;

    fn geometry() -> SourceGeometry {
        SourceGeometry {
            target: CaptureTarget::Desktop,
            display: Some(":99".into()),
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        }
    }

    #[test]
    fn pixel_buffer_validates_length() {
        assert!(PixelBuffer::new(2, 1, PixelFormat::Rgb8, vec![0; 6]).is_ok());
        assert!(PixelBuffer::new(2, 1, PixelFormat::Rgb8, vec![0; 5]).is_err());
        assert!(PixelBuffer::new(0, 1, PixelFormat::Rgb8, vec![]).is_err());
    }

    #[test]
    fn frame_exposes_dimensions_and_rect() {
        let pixels = PixelBuffer::new(2, 1, PixelFormat::Rgb8, vec![1; 6]).unwrap();
        let frame = Frame::new(geometry(), pixels, std::time::Instant::now());
        assert_eq!(frame.width(), 2);
        assert_eq!(frame.height(), 1);
        assert_eq!(frame.source_rect().pixel_count(), 2);
    }

    #[test]
    fn crop_extracts_the_requested_rows_and_columns() {
        // A 4x3 frame where each pixel encodes its own coordinates.
        let mut data = Vec::new();
        for y in 0..3u8 {
            for x in 0..4u8 {
                data.extend_from_slice(&[x, y, 0]);
            }
        }
        let pixels = PixelBuffer::new(4, 3, PixelFormat::Rgb8, data).unwrap();
        let frame = Frame::new(geometry(), pixels, std::time::Instant::now());

        let cropped = frame.crop(&Rect::new(1, 1, 2, 2).unwrap()).unwrap();
        assert_eq!((cropped.width(), cropped.height()), (2, 2));
        assert_eq!(
            cropped.pixels.data(),
            &[1, 1, 0, 2, 1, 0, 1, 2, 0, 2, 2, 0],
            "cropped pixels should be the (1,1)..(2,2) block"
        );
    }

    #[test]
    fn crop_shifts_the_source_origin_so_transforms_stay_correct() {
        let mut geometry = geometry();
        geometry.x = 100;
        geometry.y = 200;
        geometry.width = 4;
        geometry.height = 3;
        let pixels = PixelBuffer::new(4, 3, PixelFormat::Rgb8, vec![0; 36]).unwrap();
        let frame = Frame::new(geometry, pixels, std::time::Instant::now());

        let cropped = frame.crop(&Rect::new(1, 2, 2, 1).unwrap()).unwrap();
        assert_eq!(cropped.source_geometry.x, 101);
        assert_eq!(cropped.source_geometry.y, 202);
        assert_eq!(cropped.source_geometry.width, 2);
        assert_eq!(cropped.source_geometry.height, 1);
    }

    #[test]
    fn crop_rejects_a_rectangle_outside_the_frame() {
        let pixels = PixelBuffer::new(4, 4, PixelFormat::Rgb8, vec![0; 48]).unwrap();
        let frame = Frame::new(geometry(), pixels, std::time::Instant::now());

        assert!(frame.crop(&Rect::new(0, 0, 5, 4).unwrap()).is_err());
        assert!(frame.crop(&Rect::new(3, 3, 2, 2).unwrap()).is_err());
        assert!(frame.crop(&Rect::new(0, 0, 4, 4).unwrap()).is_ok());
    }
}
