//! Baseline sequential JPEG encoding.
//!
//! This is a small, self-contained encoder rather than a binding to a C
//! library. That choice is deliberate: `eensh` is meant to build on any machine
//! with a stock Rust toolchain, and it keeps the dependency graph identical
//! across platforms.
//!
//! Characteristics:
//!
//! * 8-bit, 3-component, 4:4:4 (no chroma subsampling), so no colour is thrown
//!   away before quantisation — agent screenshots are full of small text where
//!   that matters;
//! * baseline sequential Huffman coding with the standard Annex K tables;
//! * quality 1–100 mapped onto the standard quality-scaled quantisation tables.
//!
//! JPEG is only ever used as an *output* format. The raw framebuffer is never
//! stored in a lossy form.

use std::sync::OnceLock;

use crate::error::Error;
use crate::frame::Frame;

/// Quality used when `--quality` is not supplied.
///
/// 80 is a good default for agent observation: text stays legible while payload
/// size stays close to half of a lossless capture.
pub const DEFAULT_QUALITY: u8 = 80;

/// Lowest accepted quality.
pub const MIN_QUALITY: u8 = 1;

/// Highest accepted quality.
pub const MAX_QUALITY: u8 = 100;

/// Encode a frame as a baseline JPEG image.
pub fn encode(frame: &Frame, quality: u8) -> Result<Vec<u8>, Error> {
    if !(MIN_QUALITY..=MAX_QUALITY).contains(&quality) {
        return Err(Error::invalid_arguments(format!(
            "JPEG quality must be between {MIN_QUALITY} and {MAX_QUALITY}, got {quality}"
        )));
    }

    let width = frame.width() as usize;
    let height = frame.height() as usize;
    if width == 0 || height == 0 {
        return Err(Error::encode_failed(
            "cannot encode a zero-sized image".to_string(),
        ));
    }

    let pixels = frame.pixels.data();
    let encoder = Encoder::new(width, height, quality);
    Ok(encoder.encode(pixels))
}

/// Zig-zag scan order: `ZIGZAG[i]` is the natural (row-major) index of the `i`th
/// coefficient in the order they appear in the bitstream.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Standard luminance quantisation table, in natural (row-major) order.
const STD_LUMA_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];

/// Standard chrominance quantisation table, in natural (row-major) order.
const STD_CHROMA_QUANT: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// `BITS` for the standard luminance DC Huffman table (index 0 unused).
const DC_LUMA_BITS: [u8; 17] = [0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
/// `HUFFVAL` for the standard luminance DC Huffman table.
const DC_LUMA_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// `BITS` for the standard chrominance DC Huffman table (index 0 unused).
const DC_CHROMA_BITS: [u8; 17] = [0, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
/// `HUFFVAL` for the standard chrominance DC Huffman table.
const DC_CHROMA_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// `BITS` for the standard luminance AC Huffman table (index 0 unused).
const AC_LUMA_BITS: [u8; 17] = [0, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d];
/// `HUFFVAL` for the standard luminance AC Huffman table.
const AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// `BITS` for the standard chrominance AC Huffman table (index 0 unused).
const AC_CHROMA_BITS: [u8; 17] = [0, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];
/// `HUFFVAL` for the standard chrominance AC Huffman table.
const AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09, 0x23, 0x33, 0x52, 0xf0,
    0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25, 0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26,
    0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
    0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
    0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// A fully built Huffman table: code and length for each symbol.
struct HuffmanTable {
    codes: [u16; 256],
    lengths: [u8; 256],
}

impl HuffmanTable {
    fn build(bits: &[u8; 17], values: &[u8]) -> Self {
        let mut table = HuffmanTable {
            codes: [0; 256],
            lengths: [0; 256],
        };
        let mut code: u16 = 0;
        let mut index = 0usize;
        for length in 1..=16u8 {
            for _ in 0..bits[length as usize] {
                let symbol = values[index] as usize;
                table.codes[symbol] = code;
                table.lengths[symbol] = length;
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        table
    }
}

/// Entropy-coded bit stream writer. Handles `0xFF` byte stuffing.
struct BitWriter {
    output: Vec<u8>,
    accumulator: u32,
    pending_bits: u32,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        BitWriter {
            output: Vec::with_capacity(capacity),
            accumulator: 0,
            pending_bits: 0,
        }
    }

    /// Write the low `count` bits of `value`, most significant bit first.
    fn write_bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 16);
        if count == 0 {
            return;
        }
        let masked = value & ((1u32 << count) - 1);
        self.accumulator = (self.accumulator << count) | masked;
        self.pending_bits += count;
        while self.pending_bits >= 8 {
            self.pending_bits -= 8;
            let byte = ((self.accumulator >> self.pending_bits) & 0xff) as u8;
            self.push_byte(byte);
        }
    }

    fn push_byte(&mut self, byte: u8) {
        self.output.push(byte);
        if byte == 0xff {
            self.output.push(0x00);
        }
    }

    /// Pad the final partial byte with 1-bits, as the standard requires.
    fn flush(&mut self) {
        while self.pending_bits > 0 {
            let pad = (8 - self.pending_bits).min(8);
            self.write_bits(0xff, pad);
        }
        // write_bits drains in multiples of 8, so pending_bits is 0 here.
        self.accumulator = 0;
    }
}

/// One colour component being encoded.
struct Component {
    id: u8,
    horizontal_sampling: usize,
    vertical_sampling: usize,
    quant_table: [u16; 64],
    /// Which quantisation table this component references in the frame header.
    quant_selector: u8,
    /// Index into the DC Huffman table array.
    dc_table: usize,
    /// Index into the AC Huffman table array.
    ac_table: usize,
    plane_width: usize,
    plane_height: usize,
    plane: Vec<u8>,
    last_dc: i32,
}

impl Component {
    /// Fetch the 8x8 sample block at block coordinate `(bx, by)`, replicating
    /// the edge sample when the block runs past the plane boundary.
    fn block(&self, bx: usize, by: usize, out: &mut [f32; 64]) {
        for y in 0..8 {
            let py = (by * 8 + y).min(self.plane_height - 1);
            let row = &self.plane[py * self.plane_width..(py + 1) * self.plane_width];
            for x in 0..8 {
                let px = (bx * 8 + x).min(self.plane_width - 1);
                out[y * 8 + x] = row[px] as f32 - 128.0;
            }
        }
    }
}

/// The default 4:4:4 sampling factors, per component.
const SAMPLING: [(usize, usize); 3] = [(1, 1), (1, 1), (1, 1)];

struct Encoder {
    width: usize,
    height: usize,
    components: Vec<Component>,
}

impl Encoder {
    fn new(width: usize, height: usize, quality: u8) -> Self {
        let max_h = SAMPLING.iter().map(|s| s.0).max().unwrap();
        let max_v = SAMPLING.iter().map(|s| s.1).max().unwrap();

        let luma_quant = scale_quant_table(&STD_LUMA_QUANT, quality);
        let chroma_quant = scale_quant_table(&STD_CHROMA_QUANT, quality);

        let planes = build_planes(width, height, max_h, max_v);

        let mut components = Vec::with_capacity(3);
        for (index, (plane, (h, v))) in planes.into_iter().zip(SAMPLING).enumerate() {
            let (quant_selector, dc_table, ac_table, quant_table) = if index == 0 {
                (0u8, 0usize, 0usize, luma_quant)
            } else {
                (1u8, 1usize, 1usize, chroma_quant)
            };
            let plane_width = plane.width;
            let plane_height = plane.height;
            components.push(Component {
                id: index as u8 + 1,
                horizontal_sampling: h,
                vertical_sampling: v,
                quant_table,
                quant_selector,
                dc_table,
                ac_table,
                plane_width,
                plane_height,
                plane: plane.samples,
                last_dc: 0,
            });
        }

        Encoder {
            width,
            height,
            components,
        }
    }

    fn encode(mut self, rgb: &[u8]) -> Vec<u8> {
        let expected = self.width * self.height * 3;
        debug_assert_eq!(rgb.len(), expected);

        let max_h = self.max_h_sampling();
        let max_v = self.max_v_sampling();
        let width = self.width;
        let height = self.height;

        // JPEG stores YCbCr, not RGB. Writing R straight into the luma component
        // and G/B into the chroma components would decode to unrelated colours,
        // so the conversion happens here, before quantisation.
        let mut luma = vec![0u8; width * height];
        let mut blue_chroma = vec![0u8; width * height];
        let mut red_chroma = vec![0u8; width * height];
        for (index, pixel) in rgb.chunks_exact(3).enumerate() {
            let (y, cb, cr) = rgb_to_ycbcr(pixel[0], pixel[1], pixel[2]);
            luma[index] = y;
            blue_chroma[index] = cb;
            red_chroma[index] = cr;
        }

        // Fill each component's plane, honouring its sampling factors.
        for (plane, component) in [luma, blue_chroma, red_chroma]
            .into_iter()
            .zip(self.components.iter_mut())
        {
            let plane_width = component.plane_width;
            fill_plane(
                &plane,
                width,
                (component.horizontal_sampling, component.vertical_sampling),
                (max_h, max_v),
                plane_width,
                &mut component.plane,
            );
        }

        let dc_luma = HuffmanTable::build(&DC_LUMA_BITS, &DC_LUMA_VALUES);
        let dc_chroma = HuffmanTable::build(&DC_CHROMA_BITS, &DC_CHROMA_VALUES);
        let ac_luma = HuffmanTable::build(&AC_LUMA_BITS, &AC_LUMA_VALUES);
        let ac_chroma = HuffmanTable::build(&AC_CHROMA_BITS, &AC_CHROMA_VALUES);
        let dc_tables = [&dc_luma, &dc_chroma];
        let ac_tables = [&ac_luma, &ac_chroma];

        let mut out = Vec::with_capacity(self.width * self.height / 2 + 1024);
        write_headers(&mut out, self.width, self.height, &self.components);

        let mut writer = BitWriter::new(self.width * self.height / 4 + 64);

        let y_plane_width = self.components[0].plane_width;
        let y_plane_height = self.components[0].plane_height;
        let mcu_cols = y_plane_width.div_ceil(8 * max_h);
        let mcu_rows = y_plane_height.div_ceil(8 * max_v);

        let basis = dct_basis();
        let mut samples = [0f32; 64];
        let mut coefficients = [0f32; 64];
        let mut quantised = [0i32; 64];

        for my in 0..mcu_rows {
            for mx in 0..mcu_cols {
                for component in self.components.iter_mut() {
                    for v in 0..component.vertical_sampling {
                        for h in 0..component.horizontal_sampling {
                            let bx = mx * component.horizontal_sampling + h;
                            let by = my * component.vertical_sampling + v;

                            component.block(bx, by, &mut samples);
                            fdct(&samples, &mut coefficients, basis);
                            quantise(&coefficients, &component.quant_table, &mut quantised);

                            let dc = quantised[0];
                            let diff = dc - component.last_dc;
                            component.last_dc = dc;

                            let dc_table = dc_tables[component.dc_table];
                            let ac_table = ac_tables[component.ac_table];
                            write_dc(&mut writer, dc_table, diff);
                            write_ac(&mut writer, ac_table, &quantised);
                        }
                    }
                }
            }
        }

        writer.flush();
        out.extend_from_slice(&writer.output);
        out.push(0xff);
        out.push(0xd9); // EOI
        out
    }

    fn max_h_sampling(&self) -> usize {
        SAMPLING.iter().map(|s| s.0).max().unwrap()
    }

    fn max_v_sampling(&self) -> usize {
        SAMPLING.iter().map(|s| s.1).max().unwrap()
    }
}

/// A single component's full-resolution sample plane.
struct Plane {
    width: usize,
    height: usize,
    samples: Vec<u8>,
}

/// Convert one 8-bit RGB pixel to 8-bit YCbCr using the JFIF integer
/// approximations of the ITU-R BT.601 coefficients.
fn rgb_to_ycbcr(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let r = r as i32;
    let g = g as i32;
    let b = b as i32;

    let y = (77 * r + 150 * g + 29 * b + 128) >> 8;
    let cb = ((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128;
    let cr = ((128 * r - 107 * g - 21 * b + 128) >> 8) + 128;

    (
        y.clamp(0, 255) as u8,
        cb.clamp(0, 255) as u8,
        cr.clamp(0, 255) as u8,
    )
}

/// Copy a full-resolution sample plane into a component plane, applying the
/// component's sampling factors.
///
/// Phase 1 always uses 4:4:4, where this is a straight copy. The arithmetic is
/// kept general so that subsampled encoding can be added later without changing
/// the call sites.
fn fill_plane(
    source: &[u8],
    source_width: usize,
    sampling: (usize, usize),
    max_sampling: (usize, usize),
    target_width: usize,
    target: &mut [u8],
) {
    let (h, v) = sampling;
    let (max_h, max_v) = max_sampling;

    for (index, &value) in source.iter().enumerate() {
        let px = index % source_width;
        let py = index / source_width;
        let cx = px * h / max_h;
        let cy = py * v / max_v;
        target[cy * target_width + cx] = value;
    }
}

fn build_planes(width: usize, height: usize, max_h: usize, max_v: usize) -> Vec<Plane> {
    SAMPLING
        .iter()
        .map(|(h, v)| {
            let plane_width = (width * h).div_ceil(max_h).max(1);
            let plane_height = (height * v).div_ceil(max_v).max(1);
            Plane {
                width: plane_width,
                height: plane_height,
                samples: vec![0u8; plane_width * plane_height],
            }
        })
        .collect()
}

/// The 8x8 DCT-II basis matrix, computed once per process.
fn dct_basis() -> &'static [[f64; 8]; 8] {
    static BASIS: OnceLock<[[f64; 8]; 8]> = OnceLock::new();
    BASIS.get_or_init(|| {
        let mut matrix = [[0f64; 8]; 8];
        for (u, row) in matrix.iter_mut().enumerate() {
            let scale = if u == 0 {
                (1.0f64 / 8.0).sqrt()
            } else {
                (2.0f64 / 8.0).sqrt()
            };
            for (x, cell) in row.iter_mut().enumerate() {
                *cell =
                    scale * (((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI) / 16.0).cos();
            }
        }
        matrix
    })
}

/// Forward 8x8 DCT-II on a level-shifted sample block.
fn fdct(samples: &[f32; 64], out: &mut [f32; 64], basis: &[[f64; 8]; 8]) {
    let mut intermediate = [0f64; 64];

    // Rows: transform along x for every y.
    for y in 0..8 {
        for u in 0..8 {
            let mut sum = 0f64;
            for x in 0..8 {
                sum += samples[y * 8 + x] as f64 * basis[u][x];
            }
            intermediate[y * 8 + u] = sum;
        }
    }

    // Columns: transform along y for every u.
    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0f64;
            for y in 0..8 {
                sum += intermediate[y * 8 + u] * basis[v][y];
            }
            out[v * 8 + u] = sum as f32;
        }
    }
}

/// Divide by the quantisation table and round to the nearest integer.
///
/// Coefficients are clamped to the range the standard Huffman tables can
/// represent, which for 8-bit input is never reached in practice but keeps the
/// encoder total.
fn quantise(coefficients: &[f32; 64], table: &[u16; 64], out: &mut [i32; 64]) {
    for i in 0..64 {
        let divisor = table[i] as f32;
        let value = (coefficients[i] / divisor).round() as i32;
        // The standard DC and AC Huffman tables can express categories up to 11,
        // so a magnitude of 1023 is the largest symbol that can be emitted.
        // Clamping here is unreachable for 8-bit input in practice, but it keeps
        // the encoder total rather than emitting a corrupt symbol.
        out[i] = value.clamp(-1023, 1023);
    }
}

/// Scale a standard quantisation table for the requested quality.
fn scale_quant_table(base: &[u16; 64], quality: u8) -> [u16; 64] {
    let q = quality.clamp(MIN_QUALITY, MAX_QUALITY) as i32;
    let factor = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let mut table = [0u16; 64];
    for (slot, value) in table.iter_mut().zip(base.iter()) {
        let scaled = (*value as i32 * factor + 50) / 100;
        *slot = scaled.clamp(1, 255) as u16;
    }
    table
}

/// Number of bits needed to represent `value` (0 for zero).
fn magnitude_category(value: i32) -> u32 {
    let magnitude = value.unsigned_abs();
    if magnitude == 0 {
        0
    } else {
        32 - magnitude.leading_zeros()
    }
}

/// Emit the DC difference for one block.
fn write_dc(writer: &mut BitWriter, table: &HuffmanTable, difference: i32) {
    let category = magnitude_category(difference);
    let category = category.min(11) as usize;
    writer.write_bits(table.codes[category] as u32, table.lengths[category] as u32);
    if category > 0 {
        let bits = if difference < 0 {
            (difference + (1 << category) - 1) as u32
        } else {
            difference as u32
        };
        writer.write_bits(bits, category as u32);
    }
}

/// Emit the AC coefficients of one block in zig-zag order.
fn write_ac(writer: &mut BitWriter, table: &HuffmanTable, quantised: &[i32; 64]) {
    let mut run = 0usize;
    for i in 1..64 {
        let value = quantised[ZIGZAG[i]];
        if value == 0 {
            run += 1;
            continue;
        }
        // ZRL (run of 16 zeros) cannot be emitted after the final coefficient.
        while run >= 16 {
            let symbol = 0xf0usize;
            writer.write_bits(table.codes[symbol] as u32, table.lengths[symbol] as u32);
            run -= 16;
        }
        let category = magnitude_category(value).min(10) as usize;
        let symbol = (run << 4) | category;
        writer.write_bits(table.codes[symbol] as u32, table.lengths[symbol] as u32);
        let bits = if value < 0 {
            (value + (1 << category) - 1) as u32
        } else {
            value as u32
        };
        writer.write_bits(bits, category as u32);
        run = 0;
    }
    if run > 0 {
        // EOB
        writer.write_bits(table.codes[0x00] as u32, table.lengths[0x00] as u32);
    }
}

fn push_marker(out: &mut Vec<u8>, marker: u8) {
    out.push(0xff);
    out.push(marker);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

/// Write every marker segment up to and including SOS.
fn write_headers(out: &mut Vec<u8>, width: usize, height: usize, components: &[Component]) {
    push_marker(out, 0xd8); // SOI

    // APP0 / JFIF
    push_marker(out, 0xe0);
    push_u16(out, 16);
    out.extend_from_slice(b"JFIF\0");
    out.push(1); // major version
    out.push(1); // minor version
    out.push(0); // density units: none
    push_u16(out, 1); // x density
    push_u16(out, 1); // y density
    out.push(0); // thumbnail width
    out.push(0); // thumbnail height

    // Quantisation tables, in zig-zag order.
    let luma = &components[0].quant_table;
    push_marker(out, 0xdb);
    push_u16(out, 67);
    out.push(0x00);
    write_zigzag_table(out, luma);

    let chroma = &components[1].quant_table;
    push_marker(out, 0xdb);
    push_u16(out, 67);
    out.push(0x01);
    write_zigzag_table(out, chroma);

    // Frame header.
    push_marker(out, 0xc0); // SOF0, baseline DCT
    push_u16(out, (8 + 3 * components.len()) as u16);
    out.push(8); // sample precision
    push_u16(out, height as u16);
    push_u16(out, width as u16);
    out.push(components.len() as u8);
    for component in components {
        out.push(component.id);
        out.push(((component.horizontal_sampling as u8) << 4) | component.vertical_sampling as u8);
        out.push(component.quant_selector);
    }

    // Huffman tables.
    write_huffman_table(out, 0x00, &DC_LUMA_BITS, &DC_LUMA_VALUES);
    write_huffman_table(out, 0x10, &AC_LUMA_BITS, &AC_LUMA_VALUES);
    write_huffman_table(out, 0x01, &DC_CHROMA_BITS, &DC_CHROMA_VALUES);
    write_huffman_table(out, 0x11, &AC_CHROMA_BITS, &AC_CHROMA_VALUES);

    // Scan header.
    push_marker(out, 0xda);
    push_u16(out, (6 + 2 * components.len()) as u16);
    out.push(components.len() as u8);
    for component in components {
        out.push(component.id);
        out.push(((component.dc_table as u8) << 4) | component.ac_table as u8);
    }
    out.push(0); // Ss
    out.push(63); // Se
    out.push(0); // Ah / Al
}

/// Write a quantisation table in zig-zag order.
fn write_zigzag_table(out: &mut Vec<u8>, table: &[u16; 64]) {
    for index in ZIGZAG {
        out.push(table[index] as u8);
    }
}

/// Write a Huffman table definition segment.
fn write_huffman_table(out: &mut Vec<u8>, class_and_id: u8, bits: &[u8; 17], values: &[u8]) {
    let length = 2 + 1 + 16 + values.len();
    push_marker(out, 0xc4);
    push_u16(out, length as u16);
    out.push(class_and_id);
    out.extend_from_slice(&bits[1..17]);
    out.extend_from_slice(values);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelBuffer, PixelFormat};
    use crate::geometry::{CaptureTarget, SourceGeometry};
    use std::time::Instant;

    fn frame_from_rgb(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&pixel(x, y));
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

    fn decode(bytes: &[u8]) -> image::RgbImage {
        image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
            .expect("output must be a decodable JPEG")
            .to_rgb8()
    }

    #[test]
    fn produces_a_complete_jpeg() {
        let frame = frame_from_rgb(16, 16, |x, y| [(x * 4) as u8, (y * 4) as u8, 0x40]);
        let bytes = encode(&frame, 80).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xd8], "must start with SOI");
        assert_eq!(
            &bytes[bytes.len() - 2..],
            &[0xff, 0xd9],
            "must end with EOI"
        );
    }

    #[test]
    fn declared_dimensions_match_the_encoded_image() {
        let frame = frame_from_rgb(37, 21, |x, y| [(x % 256) as u8, (y % 256) as u8, 7]);
        let bytes = encode(&frame, 75).unwrap();
        let decoded = decode(&bytes);
        assert_eq!(decoded.width(), 37);
        assert_eq!(decoded.height(), 21);
    }

    #[test]
    fn solid_colour_survives_approximately() {
        let frame = frame_from_rgb(32, 32, |_, _| [200, 40, 90]);
        let bytes = encode(&frame, 100).unwrap();
        let decoded = decode(&bytes);
        let pixels = decoded.as_raw();
        for pixel in pixels.chunks_exact(3) {
            assert!(
                (pixel[0] as i32 - 200).abs() <= 6
                    && (pixel[1] as i32 - 40).abs() <= 6
                    && (pixel[2] as i32 - 90).abs() <= 6,
                "unexpected colour {pixel:?}"
            );
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        let frame = frame_from_rgb(24, 24, |x, y| {
            [((x * 11) % 256) as u8, ((y * 7) % 256) as u8, 33]
        });
        let a = encode(&frame, 80).unwrap();
        let b = encode(&frame, 80).unwrap();
        assert_eq!(a, b, "identical input must produce identical bytes");
    }

    #[test]
    fn higher_quality_produces_a_larger_stream() {
        let frame = frame_from_rgb(64, 64, |x, y| {
            [
                ((x * 3) % 256) as u8,
                ((y * 5) % 256) as u8,
                ((x + y) % 256) as u8,
            ]
        });
        let low = encode(&frame, 40).unwrap();
        let high = encode(&frame, 95).unwrap();
        assert!(
            high.len() > low.len(),
            "quality 95 ({} bytes) should be larger than quality 40 ({} bytes)",
            high.len(),
            low.len()
        );
    }

    #[test]
    fn non_multiple_of_eight_dimensions_are_handled() {
        for (w, h) in [(1, 1), (7, 3), (9, 17), (100, 1), (1, 100)] {
            let frame = frame_from_rgb(w, h, |x, y| [(x % 256) as u8, (y % 256) as u8, 128]);
            let bytes = encode(&frame, 70).unwrap();
            let decoded = decode(&bytes);
            assert_eq!((decoded.width(), decoded.height()), (w, h));
        }
    }

    #[test]
    fn rejects_out_of_range_quality() {
        let frame = frame_from_rgb(8, 8, |_, _| [0, 0, 0]);
        assert!(matches!(encode(&frame, 0), Err(Error::InvalidArguments(_))));
        assert!(matches!(
            encode(&frame, 101),
            Err(Error::InvalidArguments(_))
        ));
    }

    #[test]
    fn default_quality_is_in_range() {
        assert!((MIN_QUALITY..=MAX_QUALITY).contains(&DEFAULT_QUALITY));
    }

    #[test]
    fn quant_table_scaling_is_correct_at_the_anchors() {
        // Quality 50 maps the standard table through a factor of 1.
        assert_eq!(scale_quant_table(&STD_LUMA_QUANT, 50), STD_LUMA_QUANT);
        // Very low quality must never produce a zero divisor.
        let coarse = scale_quant_table(&STD_LUMA_QUANT, 1);
        assert!(coarse.iter().all(|&v| (1..=255).contains(&v)));
        let fine = scale_quant_table(&STD_LUMA_QUANT, 100);
        assert!(fine.iter().all(|&v| v >= 1));
        assert!(fine[0] < STD_LUMA_QUANT[0]);
    }

    #[test]
    fn huffman_tables_match_the_standard_codes() {
        // Spot-check known code lengths and bit patterns from Annex K. A
        // transcription error in BITS or HUFFVAL would break these.
        let dc_luma = HuffmanTable::build(&DC_LUMA_BITS, &DC_LUMA_VALUES);
        assert_eq!(dc_luma.lengths[0], 2); // symbol 0 -> "00"
        assert_eq!(dc_luma.codes[0], 0b00);
        assert_eq!(dc_luma.lengths[6], 4); // symbol 6 -> "1110"
        assert_eq!(dc_luma.codes[6], 0b1110);
        assert_eq!(dc_luma.lengths[11], 9);

        let ac_luma = HuffmanTable::build(&AC_LUMA_BITS, &AC_LUMA_VALUES);
        assert_eq!(ac_luma.lengths[0x01], 2);
        assert_eq!(ac_luma.codes[0x01], 0b00);
        // End-of-block in the luminance AC table.
        assert_eq!(ac_luma.lengths[0x00], 4);
        assert_eq!(ac_luma.codes[0x00], 0b1010);

        let ac_chroma = HuffmanTable::build(&AC_CHROMA_BITS, &AC_CHROMA_VALUES);
        // End-of-block in the chrominance AC table.
        assert_eq!(ac_chroma.lengths[0x00], 2);
        assert_eq!(ac_chroma.codes[0x00], 0b00);
    }

    #[test]
    fn huffman_tables_are_prefix_free() {
        for (name, bits, values) in [
            ("dc_luma", &DC_LUMA_BITS, &DC_LUMA_VALUES[..]),
            ("dc_chroma", &DC_CHROMA_BITS, &DC_CHROMA_VALUES[..]),
            ("ac_luma", &AC_LUMA_BITS, &AC_LUMA_VALUES[..]),
            ("ac_chroma", &AC_CHROMA_BITS, &AC_CHROMA_VALUES[..]),
        ] {
            let total: usize = bits[1..].iter().map(|&b| b as usize).sum();
            assert_eq!(total, values.len(), "{name}: BITS and HUFFVAL disagree");

            let table = HuffmanTable::build(bits, values);
            for &symbol in values {
                assert!(
                    table.lengths[symbol as usize] > 0,
                    "{name}: symbol 0x{symbol:02x} was never assigned a code"
                );
            }

            // No code may be a prefix of another, or the stream would not decode
            // uniquely. (The standard DC tables deliberately leave the all-ones
            // code unused, so Kraft equality would be the wrong assertion here.)
            for &a in values {
                for &b in values {
                    if a == b {
                        continue;
                    }
                    let len_a = table.lengths[a as usize] as u32;
                    let len_b = table.lengths[b as usize] as u32;
                    if len_a <= len_b {
                        let prefix = (table.codes[b as usize] >> (len_b - len_a)) as u8;
                        assert_ne!(
                            prefix, table.codes[a as usize] as u8,
                            "{name}: code for 0x{a:02x} is a prefix of the code for 0xb:02x"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rgb_to_ycbcr_matches_reference_values() {
        // Neutral grey must land exactly on the achromatic axis.
        assert_eq!(rgb_to_ycbcr(128, 128, 128), (128, 128, 128));
        assert_eq!(rgb_to_ycbcr(255, 255, 255), (255, 128, 128));
        assert_eq!(rgb_to_ycbcr(0, 0, 0), (0, 128, 128));

        // A mid-tone red-ish pixel, checked against the floating point formula.
        let (y, cb, cr) = rgb_to_ycbcr(200, 40, 90);
        assert!((y as i32 - 94).abs() <= 1, "luma was {y}");
        assert!((cb as i32 - 126).abs() <= 1, "Cb was {cb}");
        assert!((cr as i32 - 204).abs() <= 1, "Cr was {cr}");

        // Pure primaries exercise the clamping paths.
        for pixel in [(255, 0, 0), (0, 255, 0), (0, 0, 255)] {
            let _ = rgb_to_ycbcr(pixel.0, pixel.1, pixel.2);
        }
    }

    #[test]
    fn magnitude_category_is_bounded_for_eight_bit_input() {
        assert_eq!(magnitude_category(0), 0);
        assert_eq!(magnitude_category(1), 1);
        assert_eq!(magnitude_category(-1), 1);
        assert_eq!(magnitude_category(2), 2);
        assert_eq!(magnitude_category(-3), 2);
        assert_eq!(magnitude_category(2047), 11);
    }

    #[test]
    fn quantised_coefficients_stay_within_huffman_range() {
        // Even an extreme 8-bit block must not produce a category the standard
        // tables cannot express.
        let basis = dct_basis();
        let mut samples = [0f32; 64];
        for (i, sample) in samples.iter_mut().enumerate() {
            *sample = if i % 2 == 0 { 127.0 } else { -128.0 };
        }
        let mut coefficients = [0f32; 64];
        fdct(&samples, &mut coefficients, basis);
        let table = scale_quant_table(&STD_LUMA_QUANT, 100);
        let mut quantised = [0i32; 64];
        quantise(&coefficients, &table, &mut quantised);
        assert!(quantised.iter().all(|v| v.unsigned_abs() <= 1023));
    }

    #[test]
    fn bit_writer_stuffs_ff_bytes() {
        let mut writer = BitWriter::new(16);
        writer.write_bits(0xff, 8);
        writer.flush();
        assert_eq!(writer.output, vec![0xff, 0x00]);
    }

    #[test]
    fn bit_writer_pads_with_ones() {
        let mut writer = BitWriter::new(16);
        writer.write_bits(0b101, 3);
        writer.flush();
        assert_eq!(writer.output, vec![0b1011_1111]);
    }

    #[test]
    fn dct_of_a_constant_block_has_energy_only_in_dc() {
        let basis = dct_basis();
        let samples = [10f32; 64];
        let mut out = [0f32; 64];
        fdct(&samples, &mut out, basis);
        // DC should be 8 * 10 = 80, everything else ~0.
        assert!((out[0] - 80.0).abs() < 0.01, "DC was {}", out[0]);
        for (i, value) in out.iter().enumerate().skip(1) {
            assert!(value.abs() < 0.01, "coefficient {i} was {value}");
        }
    }
}
