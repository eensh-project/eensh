//! PNG encoding.
//!
//! Thin wrapper over the pure-Rust `png` crate. The frame is already in the
//! tightly packed `Rgb8` layout that PNG wants, so the pixel data is handed
//! over without any conversion or copying beyond what the compressor needs.

use crate::encode::PngEffort;
use crate::error::Error;
use crate::frame::Frame;

/// Encode a frame as a PNG image.
pub fn encode(frame: &Frame, effort: PngEffort) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();

    {
        let mut encoder = png::Encoder::new(&mut output, frame.width(), frame.height());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(match effort {
            PngEffort::Fast => png::Compression::Fast,
            PngEffort::Default => png::Compression::Balanced,
            PngEffort::Best => png::Compression::High,
        });

        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::encode_failed(format!("PNG header could not be written: {e}")))?;

        writer.write_image_data(frame.pixels.data()).map_err(|e| {
            Error::encode_failed(format!("PNG pixel data could not be written: {e}"))
        })?;

        writer
            .finish()
            .map_err(|e| Error::encode_failed(format!("PNG stream could not be finalised: {e}")))?;
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::{CaptureTarget, SourceGeometry};
    use std::time::Instant;

    fn frame(width: u32, height: u32) -> Frame {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.push((x % 256) as u8);
                data.push((y % 256) as u8);
                data.push(((x + y) % 256) as u8);
            }
        }
        let pixels = PixelBuffer::new(width, height, PixelFormat::Rgb8, data).unwrap();
        Frame::new(
            SourceGeometry {
                target: CaptureTarget::Desktop,
                display: None,
                x: 0,
                y: 0,
                width,
                height,
            },
            pixels,
            Instant::now(),
        )
    }

    /// Decode the PNG bytes back into a frame's worth of samples.
    fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().expect("output must be a valid PNG");
        let mut buffer = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buffer).expect("frame must decode");
        buffer.truncate(info.buffer_size());
        (info.width, info.height, buffer)
    }

    #[test]
    fn produces_a_valid_png_signature() {
        let bytes = encode(&frame(8, 8), PngEffort::Fast).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn encoded_dimensions_match_the_frame() {
        let bytes = encode(&frame(17, 5), PngEffort::Fast).unwrap();
        let (width, height, _) = decode(&bytes);
        assert_eq!((width, height), (17, 5));
    }

    #[test]
    fn pixels_round_trip_losslessly() {
        let source = frame(9, 9);
        let bytes = encode(&source, PngEffort::Best).unwrap();
        let (width, height, pixels) = decode(&bytes);
        assert_eq!((width, height), (9, 9));
        assert_eq!(pixels, source.pixels.data());
    }

    #[test]
    fn all_effort_levels_produce_decodable_output() {
        for effort in [PngEffort::Fast, PngEffort::Default, PngEffort::Best] {
            let bytes = encode(&frame(6, 6), effort).unwrap();
            let (width, height, _) = decode(&bytes);
            assert_eq!(
                (width, height),
                (6, 6),
                "effort {effort:?} produced wrong size"
            );
        }
    }
}
