//! Deterministic image resizing of raw frames.
//!
//! Phase 1 uses a separable box filter with `f32` accumulation. It is
//! deterministic, allocation-light, and fast at the downscale ratios agent
//! workloads actually use. Photographic quality is explicitly *not* a goal
//! here.
//!
//! Note that resizing happens on raw pixels, before encoding. That ordering is
//! deliberate: encoding first and resizing later would be both slower and
//! lossier.

use crate::error::Error;
use crate::frame::{Frame, PixelBuffer, PixelFormat};

/// Resize a frame to the requested dimensions.
///
/// `target_width` and `target_height` must both be non-zero. When they already
/// match the source, the frame is returned unchanged.
pub fn resize(frame: &Frame, target_width: u32, target_height: u32) -> Result<Frame, Error> {
    if target_width == 0 || target_height == 0 {
        return Err(Error::ResizeFailed(
            "target dimensions must be non-zero".to_string(),
        ));
    }

    let source_width = frame.width();
    let source_height = frame.height();

    if source_width == target_width && source_height == target_height {
        return Ok(frame.clone());
    }

    let resized = resize_rgb8(
        frame.pixels.data(),
        source_width,
        source_height,
        target_width,
        target_height,
    )?;

    let buffer = PixelBuffer::new(target_width, target_height, PixelFormat::Rgb8, resized)?;

    Ok(Frame {
        source_geometry: frame.source_geometry.clone(),
        pixel_format: PixelFormat::Rgb8,
        pixels: buffer,
        captured_at: frame.captured_at,
    })
}

/// Resize a tightly packed `Rgb8` buffer using a separable box filter.
fn resize_rgb8(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, Error> {
    let sw = source_width as usize;
    let sh = source_height as usize;
    let tw = target_width as usize;
    let th = target_height as usize;

    let expected = sw
        .checked_mul(sh)
        .and_then(|p| p.checked_mul(3))
        .ok_or_else(|| Error::ResizeFailed("source dimensions overflow".to_string()))?;
    if source.len() != expected {
        return Err(Error::ResizeFailed(format!(
            "source buffer is {} bytes but {sw}x{sh} requires {expected}",
            source.len()
        )));
    }

    let out_len = tw
        .checked_mul(th)
        .and_then(|p| p.checked_mul(3))
        .ok_or_else(|| Error::ResizeFailed("target dimensions overflow".to_string()))?;

    // Pass 1: horizontal, producing a tw x sh intermediate buffer.
    let mut horizontal = vec![0f32; tw * sh * 3];
    for y in 0..sh {
        let src_row = &source[y * sw * 3..(y + 1) * sw * 3];
        let dst_row = &mut horizontal[y * tw * 3..(y + 1) * tw * 3];
        for x in 0..tw {
            let (start, end) = source_span(x, tw, sw);
            let count = (end - start) as f32;
            let mut acc = [0f32; 3];
            for sx in start..end {
                let base = sx as usize * 3;
                acc[0] += src_row[base] as f32;
                acc[1] += src_row[base + 1] as f32;
                acc[2] += src_row[base + 2] as f32;
            }
            let out = x * 3;
            dst_row[out] = acc[0] / count;
            dst_row[out + 1] = acc[1] / count;
            dst_row[out + 2] = acc[2] / count;
        }
    }

    // Pass 2: vertical, producing the final tightly packed u8 buffer.
    let mut output = vec![0u8; out_len];
    for y in 0..th {
        let (start, end) = source_span(y, th, sh);
        let count = (end - start) as f32;
        let dst_row = &mut output[y * tw * 3..(y + 1) * tw * 3];
        for x in 0..tw {
            let mut acc = [0f32; 3];
            for sy in start..end {
                let idx = (sy as usize * tw + x) * 3;
                acc[0] += horizontal[idx];
                acc[1] += horizontal[idx + 1];
                acc[2] += horizontal[idx + 2];
            }
            let out = x * 3;
            dst_row[out] = clamp_u8(acc[0] / count);
            dst_row[out + 1] = clamp_u8(acc[1] / count);
            dst_row[out + 2] = clamp_u8(acc[2] / count);
        }
    }

    Ok(output)
}

/// Half-open source span `[start, end)` that maps onto target index `index`.
///
/// The span always covers at least one source pixel, which is what makes
/// upscaling work as well as downscaling.
fn source_span(index: usize, target_len: usize, source_len: usize) -> (u32, u32) {
    let ratio = source_len as f64 / target_len as f64;
    let start = (index as f64 * ratio).floor() as usize;
    let end = (((index + 1) as f64) * ratio).ceil() as usize;
    let start = start.min(source_len - 1);
    let end = end.max(start + 1).min(source_len);
    (start as u32, end as u32)
}

fn clamp_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{CaptureTarget, SourceGeometry};
    use std::time::Instant;

    fn solid_frame(width: u32, height: u32, rgb: [u8; 3]) -> Frame {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..(width * height) {
            data.extend_from_slice(&rgb);
        }
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        let geometry = SourceGeometry {
            target: CaptureTarget::Desktop,
            display: Some(":99".into()),
            x: 0,
            y: 0,
            width,
            height,
        };
        Frame::new(geometry, pixels, Instant::now())
    }

    #[test]
    fn downscale_averages_solid_colour_exactly() {
        let frame = solid_frame(8, 8, [10, 200, 30]);
        let resized = resize(&frame, 4, 4).unwrap();
        assert_eq!(resized.width(), 4);
        assert_eq!(resized.height(), 4);
        assert!(resized
            .pixels
            .data()
            .chunks_exact(3)
            .all(|p| p == [10, 200, 30]));
    }

    #[test]
    fn resize_preserves_source_geometry() {
        let frame = solid_frame(8, 8, [1, 2, 3]);
        let resized = resize(&frame, 2, 2).unwrap();
        assert_eq!(
            resized.source_geometry, frame.source_geometry,
            "resize must not rewrite the source geometry"
        );
    }

    #[test]
    fn identical_dimensions_return_an_equivalent_frame() {
        let frame = solid_frame(4, 4, [7, 7, 7]);
        let resized = resize(&frame, 4, 4).unwrap();
        assert_eq!(resized.pixels.data(), frame.pixels.data());
    }

    #[test]
    fn upscaling_is_supported_and_deterministic() {
        let frame = solid_frame(2, 2, [9, 9, 9]);
        let a = resize(&frame, 5, 5).unwrap();
        let b = resize(&frame, 5, 5).unwrap();
        assert_eq!(a.pixels.data(), b.pixels.data());
        assert!(a.pixels.data().chunks_exact(3).all(|p| p == [9, 9, 9]));
    }

    #[test]
    fn rejects_zero_target_dimensions() {
        let frame = solid_frame(4, 4, [0, 0, 0]);
        assert!(resize(&frame, 0, 4).is_err());
        assert!(resize(&frame, 4, 0).is_err());
    }

    #[test]
    fn source_span_covers_every_row_for_down_and_up_scaling() {
        // Downscale 10 -> 3: spans must tile the source without gaps.
        let spans: Vec<_> = (0..3).map(|i| source_span(i, 3, 10)).collect();
        assert_eq!(spans[0].0, 0);
        assert_eq!(spans[2].1, 10);
        for window in spans.windows(2) {
            assert!(window[0].1 >= window[1].0, "spans must not leave gaps");
        }
        // Upscale 2 -> 5: every target pixel must cover at least one source px.
        for i in 0..5 {
            let (s, e) = source_span(i, 5, 2);
            assert!(e > s);
            assert!(s < 2 && e <= 2);
        }
    }

    #[test]
    fn resize_is_deterministic_for_mixed_content() {
        let mut data = Vec::new();
        for i in 0..(16 * 16 * 3) {
            data.push((i % 251) as u8);
        }
        let pixels = PixelBuffer::new(16, 16, PixelFormat::Rgb8, data).unwrap();
        let frame = Frame::new(
            SourceGeometry {
                target: CaptureTarget::Desktop,
                display: None,
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            },
            pixels,
            Instant::now(),
        );
        let first = resize(&frame, 7, 5).unwrap();
        let second = resize(&frame, 7, 5).unwrap();
        assert_eq!(first.pixels.data(), second.pixels.data());
    }
}
