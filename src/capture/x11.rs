//! Direct X11 capture backend.
//!
//! This module is responsible for exactly one thing: turning an X11 window (or
//! the root window) into a raw [`Frame`]. It never encodes, resizes, base64
//! encodes, or writes output, and it never shells out to an external screenshot
//! program.
//!
//! ## Window capture semantics
//!
//! Windows are captured by reading the **root window pixels** that lie under
//! the window's on-screen rectangle. This means:
//!
//! * the result is the *visible desktop representation*: if the window is
//!   partly or fully occluded, the occluding pixels are what you get;
//! * window decorations drawn by a compositing or reparenting window manager
//!   are included only to the extent that the window's own geometry includes
//!   them;
//! * windows that are not currently viewable (unmapped, iconified) are a hard
//!   error rather than a guess.
//!
//! `XGetImage` on the root window is both faster and more predictable than
//! reading a redirected window pixmap, and it avoids claiming to capture
//! unobscured contents we cannot actually see.

use std::time::Instant;

use x11_dl::xlib::{InputOnly, IsViewable, XImage};

use crate::capture::display::Display;
use crate::error::Error;
use crate::frame::{Frame, PixelBuffer, PixelFormat};
use crate::geometry::{Rect, SourceGeometry};

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRequest {
    /// The full root window.
    Desktop,
    /// A rectangle in root-window coordinates.
    Region(Rect),
    /// A specific X11 window by ID.
    Window(u64),
}

/// Execute a capture request against an open display.
pub fn capture(display: &Display, request: &CaptureRequest) -> Result<Frame, Error> {
    match request {
        CaptureRequest::Desktop => capture_desktop(display),
        CaptureRequest::Region(rect) => capture_region(display, rect),
        CaptureRequest::Window(id) => capture_window(display, *id),
    }
}

/// Capture the whole root window.
pub fn capture_desktop(display: &Display) -> Result<Frame, Error> {
    let root = display.root_window()?;
    let bounds = display.geometry_of(root)?;
    capture_root_rect(
        display,
        root,
        &bounds,
        SourceGeometry::desktop(
            Some(display.name().to_string()),
            bounds.width,
            bounds.height,
        ),
    )
}

/// Capture a rectangle of the root window.
///
/// The rectangle must lie entirely within the display; silent clipping never
/// happens.
pub fn capture_region(display: &Display, requested: &Rect) -> Result<Frame, Error> {
    let root = display.root_window()?;
    let bounds = display.geometry_of(root)?;

    if requested.width == 0 || requested.height == 0 {
        return Err(Error::invalid_region("region dimensions must be non-zero"));
    }
    requested.ensure_within(&bounds)?;

    capture_root_rect(
        display,
        root,
        requested,
        SourceGeometry::region(
            Some(display.name().to_string()),
            requested.x,
            requested.y,
            requested.width,
            requested.height,
        ),
    )
}

/// Capture a window by ID, using the visible desktop pixels under it.
pub fn capture_window(display: &Display, window: u64) -> Result<Frame, Error> {
    let root = display.root_window()?;
    let bounds = display.geometry_of(root)?;

    let attributes = display.window_attributes(window)?;

    if attributes.class == InputOnly {
        return Err(Error::capture_failed(format!(
            "window 0x{window:x} is InputOnly and has no pixels to capture"
        )));
    }
    if attributes.map_state != IsViewable {
        return Err(Error::capture_failed(format!(
            "window 0x{window:x} is not viewable (it is unmapped or obscured by an ancestor), \
             so its pixels cannot be read from the desktop"
        )));
    }

    // Window geometry is reported relative to the parent, so translate to root
    // coordinates first.
    let (abs_x, abs_y) = display.translate_coordinates(window, root, 0, 0)?;

    let window_rect = Rect::new(
        abs_x,
        abs_y,
        attributes.width.max(1) as u32,
        attributes.height.max(1) as u32,
    )?;

    // A window may extend past the edges of the screen. Capture only the part
    // that is actually on-screen rather than failing outright, and report the
    // clipped rectangle in the source geometry so the mapping stays honest.
    let clipped = intersect(&window_rect, &bounds).ok_or_else(|| {
        Error::capture_failed(format!(
            "window 0x{window:x} lies entirely outside the display and has no visible pixels"
        ))
    })?;

    capture_root_rect(
        display,
        root,
        &clipped,
        SourceGeometry::window(
            Some(display.name().to_string()),
            format!("0x{window:x}"),
            clipped.x,
            clipped.y,
            clipped.width,
            clipped.height,
        ),
    )
}

/// Read `rect` from `window` and build a [`Frame`].
fn capture_root_rect(
    display: &Display,
    window: u64,
    rect: &Rect,
    source_geometry: SourceGeometry,
) -> Result<Frame, Error> {
    // Guard against absurd allocations before touching the X server.
    let pixel_bytes = rect
        .pixel_count()
        .checked_mul(PixelFormat::Rgb8.bytes_per_pixel() as u64)
        .ok_or_else(|| Error::invalid_region("region is too large to allocate"))?;
    let max_pixels = 1u64 << 32; // 4 gigapixels, far beyond any real display
    if rect.pixel_count() > max_pixels {
        return Err(Error::invalid_region(format!(
            "region of {}x{} pixels is unreasonably large",
            rect.width, rect.height
        )));
    }
    debug_assert!(pixel_bytes > 0);

    let (image, read_rect) = display.get_image(window, rect)?;

    let copied = unsafe { copy_image_to_rgb8(display, image) };
    unsafe { display.destroy_image(image) };
    let pixels = copied?;

    let captured_at = Instant::now();

    if read_rect.width == 0 || read_rect.height == 0 {
        return Err(Error::capture_failed(
            "the capture produced an empty image".to_string(),
        ));
    }

    let buffer = PixelBuffer::new(read_rect.width, read_rect.height, PixelFormat::Rgb8, pixels)?;

    Ok(Frame::new(source_geometry, buffer, captured_at))
}

/// Intersection of two rectangles, or `None` when they do not overlap.
fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let left = a.x.max(b.x) as i64;
    let top = a.y.max(b.y) as i64;
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    if right <= left || bottom <= top {
        return None;
    }
    Some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// `LSBFirst` from `X.h`, the value of `XImage::byte_order` on a little-endian
/// server.
const LSB_FIRST: std::os::raw::c_int = 0;

/// Describes how to pull 8-bit RGB out of an `XImage`'s raw bytes.
#[derive(Debug, Clone, Copy)]
struct ChannelLayout {
    red: MaskInfo,
    green: MaskInfo,
    blue: MaskInfo,
    /// Bytes occupied by one pixel in the server's representation.
    unit_bytes: usize,
    /// True when the server's byte order is the opposite of the client's.
    swap_bytes: bool,
}

#[derive(Debug, Clone, Copy)]
struct MaskInfo {
    shift: u32,
    bits: u32,
}

impl MaskInfo {
    fn from_mask(mask: u64) -> Option<Self> {
        if mask == 0 {
            return None;
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).count_ones();
        // Masks must be contiguous.
        if (mask >> shift) != ((1u64 << bits) - 1) {
            return None;
        }
        Some(MaskInfo { shift, bits })
    }
}

/// Copy an `XImage` into a tightly packed `Rgb8` buffer.
///
/// # Safety
///
/// `image` must be a valid, non-null pointer returned by `XGetImage` and must
/// not have been destroyed.
unsafe fn copy_image_to_rgb8(display: &Display, image: *mut XImage) -> Result<Vec<u8>, Error> {
    let img = &*image;
    let width = img.width;
    let height = img.height;

    if width <= 0 || height <= 0 {
        return Err(Error::capture_failed(
            "the X server returned a zero-sized image".to_string(),
        ));
    }

    let layout = describe_layout(display, img)?;
    let stride = img.bytes_per_line as usize;
    let unit_bytes = layout.unit_bytes;

    if unit_bytes == 0 || stride < width as usize * unit_bytes {
        return Err(Error::capture_failed(
            "the X server returned an image with an unusable stride".to_string(),
        ));
    }

    let width = width as usize;
    let height = height as usize;

    let mut out = vec![0u8; width * height * 3];
    let base = img.data as *const u8;
    if base.is_null() {
        return Err(Error::capture_failed(
            "the X server returned an image with no pixel data".to_string(),
        ));
    }

    // Fast path: 8-bit components, no byte swapping needed.
    if unit_bytes == 4 && !layout.swap_bytes {
        let r = layout.red.shift as usize / 8;
        let g = layout.green.shift as usize / 8;
        let b = layout.blue.shift as usize / 8;
        if layout.red.bits == 8 && layout.green.bits == 8 && layout.blue.bits == 8 {
            for y in 0..height {
                let row = std::slice::from_raw_parts(base.add(y * stride), width * 4);
                let out_row = &mut out[y * width * 3..(y + 1) * width * 3];
                for (pixel, dst) in row.chunks_exact(4).zip(out_row.chunks_exact_mut(3)) {
                    dst[0] = pixel[r];
                    dst[1] = pixel[g];
                    dst[2] = pixel[b];
                }
            }
            return Ok(out);
        }
    }

    if layout.swap_bytes {
        return Err(Error::capture_failed(
            "the X server uses an unsupported byte order for this visual".to_string(),
        ));
    }

    // General path: arbitrary bit depths and component widths, scaled to 8 bits.
    for y in 0..height {
        let row = std::slice::from_raw_parts(base.add(y * stride), width * unit_bytes);
        let out_row = &mut out[y * width * 3..(y + 1) * width * 3];
        for (unit, dst) in row
            .chunks_exact(unit_bytes)
            .zip(out_row.chunks_exact_mut(3))
        {
            let value = read_unit(unit);
            dst[0] = scale_to_u8(value, layout.red);
            dst[1] = scale_to_u8(value, layout.green);
            dst[2] = scale_to_u8(value, layout.blue);
        }
    }

    Ok(out)
}

/// Assemble a byte-value from a pixel unit stored in little-endian order.
fn read_unit(unit: &[u8]) -> u64 {
    let mut value = 0u64;
    for (i, byte) in unit.iter().enumerate() {
        value |= (*byte as u64) << (8 * i);
    }
    value
}

fn scale_to_u8(value: u64, info: MaskInfo) -> u8 {
    if info.bits == 0 {
        return 0;
    }
    let mask = (1u64 << info.bits) - 1;
    let raw = (value >> info.shift) & mask;
    if info.bits == 8 {
        return raw as u8;
    }
    let max = mask as f64;
    ((raw as f64 / max) * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Work out how to decode the pixels of an `XImage`.
unsafe fn describe_layout(display: &Display, img: &XImage) -> Result<ChannelLayout, Error> {
    let _ = display;
    let red = MaskInfo::from_mask(img.red_mask)
        .ok_or_else(|| Error::capture_failed("unsupported visual: no red channel mask"))?;
    let green = MaskInfo::from_mask(img.green_mask)
        .ok_or_else(|| Error::capture_failed("unsupported visual: no green channel mask"))?;
    let blue = MaskInfo::from_mask(img.blue_mask)
        .ok_or_else(|| Error::capture_failed("unsupported visual: no blue channel mask"))?;

    let bpp = img.bits_per_pixel;
    if bpp == 0 || bpp % 8 != 0 || bpp > 32 {
        return Err(Error::capture_failed(format!(
            "unsupported visual: {bpp} bits per pixel"
        )));
    }

    let unit_bytes = (bpp / 8) as usize;
    // The XImage reports the server's byte order directly, which is the only
    // reliable way to know whether each pixel unit needs reversing. For a local
    // display this always matches the client, but a remote display may not.
    let server_little_endian = img.byte_order == LSB_FIRST;
    let client_little_endian = cfg!(target_endian = "little");
    let swap_bytes = server_little_endian != client_little_endian;

    Ok(ChannelLayout {
        red,
        green,
        blue,
        unit_bytes,
        swap_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_computes_overlap() {
        let a = Rect::new(0, 0, 100, 100).unwrap();
        let b = Rect::new(50, 50, 100, 100).unwrap();
        assert_eq!(intersect(&a, &b), Some(Rect::new(50, 50, 50, 50).unwrap()));
    }

    #[test]
    fn intersect_returns_none_when_disjoint() {
        let a = Rect::new(0, 0, 10, 10).unwrap();
        let b = Rect::new(20, 20, 10, 10).unwrap();
        assert_eq!(intersect(&a, &b), None);
    }

    #[test]
    fn intersect_handles_negative_origins() {
        let a = Rect::new(-50, -50, 100, 100).unwrap();
        let b = Rect::new(0, 0, 100, 100).unwrap();
        assert_eq!(intersect(&a, &b), Some(Rect::new(0, 0, 50, 50).unwrap()));
    }

    #[test]
    fn mask_info_parses_standard_rgb_masks() {
        let red = MaskInfo::from_mask(0x00ff0000).unwrap();
        assert_eq!((red.shift, red.bits), (16, 8));
        let green = MaskInfo::from_mask(0x0000ff00).unwrap();
        assert_eq!((green.shift, green.bits), (8, 8));
        let blue = MaskInfo::from_mask(0x000000ff).unwrap();
        assert_eq!((blue.shift, blue.bits), (0, 8));
    }

    #[test]
    fn mask_info_rejects_non_contiguous_masks() {
        assert!(MaskInfo::from_mask(0b1010).is_none());
        assert!(MaskInfo::from_mask(0).is_none());
    }

    #[test]
    fn scale_to_u8_handles_sixteen_bit_components() {
        let info = MaskInfo { shift: 0, bits: 16 };
        assert_eq!(scale_to_u8(0x0000, info), 0);
        assert_eq!(scale_to_u8(0xffff, info), 255);
        assert_eq!(scale_to_u8(0x8000, info), 128);
    }
}
