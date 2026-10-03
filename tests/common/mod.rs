//! Shared helpers for the integration tests.
//!
//! The tests need three things: a real X server to capture from, a way to put
//! *known* pixels on screen so that capture can be verified rather than merely
//! exercised, and a way to read encoded images back.
//!
//! ## Xvfb
//!
//! A private `Xvfb` display is started on demand. If `Xvfb` is not installed the
//! tests that need it are *skipped* with a clear message rather than reported as
//! passing silently. Set `EENSH_REQUIRE_XVFB=1` to turn a skip into a failure,
//! which is what a CI job should do.
//!
//! ## Serialisation
//!
//! Tests within one integration test binary run in parallel. Window capture
//! reads the pixels visible on the root window, so two overlapping test windows
//! would contaminate each other's results. Every test that inspects screen
//! pixels therefore takes a global lock — crude, but it keeps the results
//! deterministic.

#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::CString;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use x11_dl::xlib::{Display as XDisplay, XWindowAttributes, Xlib};

/// `IsViewable` from `X.h`.
const IS_VIEWABLE: i32 = 2;

/// A test harness error handler.
///
/// Xlib's default handler prints the error and calls `exit()`, which would kill
/// the whole test binary. Since these tests deliberately provoke errors (for
/// example by capturing a destroyed window), the harness installs a handler that
/// simply ignores them.
unsafe extern "C" fn ignore_x_error(
    _display: *mut XDisplay,
    _event: *mut x11_dl::xlib::XErrorEvent,
) -> std::os::raw::c_int {
    0
}

/// Serialises tests that inspect pixels on the shared test display.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An `Xvfb` process and the display name it serves.
pub struct Xvfb {
    child: Child,
    display: String,
    socket: PathBuf,
}

impl Xvfb {
    /// Start an `Xvfb` with the given virtual screen size.
    ///
    /// Returns `None` when no `Xvfb` binary is available.
    pub fn start(width: u32, height: u32) -> Option<Xvfb> {
        let binary = find_xvfb()?;

        for number in 90..130u32 {
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
            let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
            // A live server holds both the socket and the lock. A stale socket
            // with no lock is inert, but is skipped anyway so that the tests do
            // not depend on the state of someone else's /tmp.
            if socket.exists() || lock.exists() {
                continue;
            }

            let display = format!(":{number}");
            let child = Command::new(&binary)
                .arg(&display)
                .arg("-screen")
                .arg("0")
                .arg(format!("{width}x{height}x24"))
                .arg("-nolisten")
                .arg("tcp")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;

            let mut server = Xvfb {
                child,
                display,
                socket,
            };

            if wait_for_display(&server.display, Duration::from_secs(10)) {
                return Some(server);
            }

            let _ = server.child.kill();
            let _ = server.child.wait();
        }

        None
    }

    /// The display name, for example `":91"`.
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Wait for the socket to disappear so an immediate rerun can reuse the
        // display number.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && self.socket.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Locate an `Xvfb` binary, or report that the tests should be skipped.
pub fn find_xvfb() -> Option<PathBuf> {
    let required = std::env::var("EENSH_REQUIRE_XVFB").is_ok_and(|v| v != "0");
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(path) = std::env::var("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("Xvfb"));
        }
    }
    candidates.push(PathBuf::from("/usr/bin/Xvfb"));
    candidates.push(PathBuf::from("/usr/local/bin/Xvfb"));
    // Snaps are a common place to find Xvfb on systems without the X server
    // development packages installed.
    if let Ok(entries) = std::fs::read_dir("/snap/kf6-core24") {
        for entry in entries.flatten() {
            candidates.push(entry.path().join("usr/bin/Xvfb"));
        }
    }

    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if required {
        panic!("EENSH_REQUIRE_XVFB is set but no Xvfb binary could be found");
    }
    eprintln!("note: skipping test because no Xvfb binary was found on this machine");
    None
}

/// Poll until the display accepts connections.
fn wait_for_display(display: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if can_open(display) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Try to open a display once.
fn can_open(display: &str) -> bool {
    let Ok(xlib) = Xlib::open() else {
        return false;
    };
    let Ok(name) = CString::new(display) else {
        return false;
    };
    let handle = unsafe { (xlib.XOpenDisplay)(name.as_ptr()) };
    if handle.is_null() {
        return false;
    }
    unsafe { (xlib.XCloseDisplay)(handle) };
    true
}

/// The colour masks of the test display's default visual.
#[derive(Debug, Clone, Copy)]
pub struct VisualMasks {
    pub red: u64,
    pub green: u64,
    pub blue: u64,
}

/// A connection to the test display, plus the windows created on it.
pub struct Screen {
    xlib: Xlib,
    handle: *mut XDisplay,
    root: u64,
    windows: RefCell<Vec<u64>>,
}

// Test bodies hold `serial()` and use the connection from a single thread.
unsafe impl Send for Screen {}

impl Screen {
    /// Connect to the display.
    pub fn open(display: &str) -> Screen {
        let xlib = Xlib::open().expect("libX11 must be loadable for integration tests");
        let name = CString::new(display).unwrap();
        let handle = unsafe { (xlib.XOpenDisplay)(name.as_ptr()) };
        assert!(!handle.is_null(), "could not open display {display}");
        // Without this, any X protocol error would terminate the test process
        // via Xlib's default handler.
        unsafe {
            (xlib.XSetErrorHandler)(Some(ignore_x_error));
        }
        let root = unsafe { (xlib.XDefaultRootWindow)(handle) };
        Screen {
            xlib,
            handle,
            root,
            windows: RefCell::new(Vec::new()),
        }
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    /// The root window's size.
    pub fn screen_size(&self) -> (u32, u32) {
        let (_, _, width, height) = self.geometry(self.root);
        (width, height)
    }

    /// The colour masks of the default visual, used to build pixel values.
    pub fn visual_masks(&self) -> VisualMasks {
        let mut attributes: XWindowAttributes = unsafe { std::mem::zeroed() };
        let status =
            unsafe { (self.xlib.XGetWindowAttributes)(self.handle, self.root, &mut attributes) };
        assert_ne!(status, 0, "could not read root window attributes");
        assert!(!attributes.visual.is_null());
        let visual = unsafe { &*attributes.visual };
        VisualMasks {
            red: visual.red_mask,
            green: visual.green_mask,
            blue: visual.blue_mask,
        }
    }

    /// Create a window of the given size, tracked for cleanup on drop.
    pub fn create_window(&self, width: u32, height: u32, x: i32, y: i32) -> u64 {
        let window = unsafe {
            (self.xlib.XCreateSimpleWindow)(
                self.handle,
                self.root,
                x,
                y,
                width,
                height,
                0, // border width
                0, // border pixel (black)
                0, // background pixel (black)
            )
        };
        assert_ne!(window, 0, "XCreateSimpleWindow failed");
        self.windows.borrow_mut().push(window);
        window
    }

    /// Map a window and wait until the server reports it as viewable.
    pub fn map(&self, window: u64) {
        unsafe {
            (self.xlib.XMapWindow)(self.handle, window);
            (self.xlib.XSync)(self.handle, 0);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut attributes: XWindowAttributes = unsafe { std::mem::zeroed() };
            let status =
                unsafe { (self.xlib.XGetWindowAttributes)(self.handle, window, &mut attributes) };
            if status != 0 && attributes.map_state == IS_VIEWABLE {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("window {window:#x} never became viewable");
    }

    /// Fill a rectangle with an explicit pixel value.
    pub fn fill(&self, window: u64, x: i32, y: i32, width: u32, height: u32, pixel: u64) {
        let gc = unsafe { (self.xlib.XCreateGC)(self.handle, window, 0, std::ptr::null_mut()) };
        assert!(!gc.is_null(), "XCreateGC failed");
        unsafe {
            (self.xlib.XSetForeground)(self.handle, gc, pixel);
            (self.xlib.XFillRectangle)(self.handle, window, gc, x, y, width, height);
            (self.xlib.XFreeGC)(self.handle, gc);
            (self.xlib.XSync)(self.handle, 0);
        }
    }

    /// Destroy a window and wait for the server to act on it.
    ///
    /// The window is removed from the cleanup list so that `Drop` does not try
    /// to destroy it a second time.
    pub fn destroy(&self, window: u64) {
        self.windows.borrow_mut().retain(|id| *id != window);
        unsafe {
            (self.xlib.XDestroyWindow)(self.handle, window);
            (self.xlib.XSync)(self.handle, 0);
        }
    }

    /// Window geometry as `(x, y, width, height)`.
    pub fn geometry(&self, window: u64) -> (i32, i32, u32, u32) {
        let mut root_return: u64 = 0;
        let mut x = 0;
        let mut y = 0;
        let mut width = 0;
        let mut height = 0;
        let mut border = 0;
        let mut depth = 0;
        let status = unsafe {
            (self.xlib.XGetGeometry)(
                self.handle,
                window,
                &mut root_return,
                &mut x,
                &mut y,
                &mut width,
                &mut height,
                &mut border,
                &mut depth,
            )
        };
        assert_ne!(status, 0, "XGetGeometry failed");
        (x, y, width, height)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let windows = self.windows.borrow().clone();
        unsafe {
            for window in windows {
                (self.xlib.XDestroyWindow)(self.handle, window);
            }
            (self.xlib.XCloseDisplay)(self.handle);
        }
    }
}

/// Pack an 8-bit RGB triple into a pixel using the server's channel masks.
///
/// For a 24-bit TrueColor visual each mask starts on a byte boundary, so the
/// channel value can be shifted straight into place.
pub fn rgb_to_pixel(masks: VisualMasks, rgb: [u8; 3]) -> u64 {
    let channel = |mask: u64, value: u8| -> u64 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).count_ones();
        let scaled = if bits >= 8 {
            (value as u64) << (bits - 8)
        } else {
            (value as u64) >> (8 - bits)
        };
        (scaled << shift) & mask
    };

    channel(masks.red, rgb[0]) | channel(masks.green, rgb[1]) | channel(masks.blue, rgb[2])
}

/// The `eensh` binary built by Cargo.
pub fn eensh_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_eensh"))
}

/// Run `eensh` with the given arguments and capture its output.
pub fn run_eensh(args: &[&str]) -> Output {
    Command::new(eensh_binary())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run eensh")
}

/// Run `eensh`, returning `(exit_code, stdout, stderr)`.
pub fn run_eensh_text(args: &[&str]) -> (i32, String, String) {
    let output = run_eensh(args);
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// A decoded image, always as tightly packed `Rgb8`.
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl DecodedImage {
    /// The RGB triple at `(x, y)`.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        assert!(x < self.width && y < self.height, "pixel out of range");
        let index = ((y * self.width + x) * 3) as usize;
        [self.rgb[index], self.rgb[index + 1], self.rgb[index + 2]]
    }

    /// Assert that a pixel matches an expected colour within `tolerance` per
    /// channel. JPEG is lossy, so exact comparisons would be wrong.
    pub fn expect_pixel(&self, x: u32, y: u32, expected: [u8; 3], tolerance: i32) {
        let actual = self.pixel(x, y);
        for channel in 0..3 {
            let difference = (actual[channel] as i32 - expected[channel] as i32).abs();
            assert!(
                difference <= tolerance,
                "pixel ({x}, {y}) was {actual:?}, expected {expected:?} within {tolerance}"
            );
        }
    }
}

/// Decode a complete JPEG byte stream.
pub fn decode_jpeg(bytes: &[u8]) -> DecodedImage {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)
        .expect("output should be a decodable JPEG")
        .to_rgb8();
    DecodedImage {
        width: decoded.width(),
        height: decoded.height(),
        rgb: decoded.into_raw(),
    }
}

/// Decode a complete PNG byte stream.
pub fn decode_png(bytes: &[u8]) -> DecodedImage {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder
        .read_info()
        .expect("output should be a decodable PNG");
    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buffer).expect("frame should decode");
    buffer.truncate(info.buffer_size());
    DecodedImage {
        width: info.width,
        height: info.height,
        rgb: buffer,
    }
}

/// Create (and clear) a temporary directory for a test's output files.
pub fn temp_dir(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("eensh-it-{name}-{}", std::process::id()));
    if directory.exists() {
        let _ = std::fs::remove_dir_all(&directory);
    }
    std::fs::create_dir_all(&directory).expect("could not create the test output directory");
    directory
}

/// Path to a file inside a test's temporary directory.
pub fn temp_file(name: &str, file: &str) -> PathBuf {
    temp_dir(name).join(file)
}

/// Start an `Xvfb` display, or return `None` when the tests should be skipped.
///
/// Both the server and its display name must be kept alive for the duration of
/// the test.
pub fn xvfb(width: u32, height: u32) -> Option<(Xvfb, String)> {
    let server = Xvfb::start(width, height)?;
    let display = server.display().to_string();
    Some((server, display))
}

/// True when the platform's `Xvfb` is actually required to be present.
pub fn xvfb_required() -> bool {
    std::env::var("EENSH_REQUIRE_XVFB").is_ok_and(|v| v != "0")
}

#[cfg(test)]
mod self_tests {
    use super::*;

    #[test]
    fn rgb_to_pixel_matches_the_standard_rgb_layout() {
        let masks = VisualMasks {
            red: 0x00ff0000,
            green: 0x0000ff00,
            blue: 0x000000ff,
        };
        assert_eq!(rgb_to_pixel(masks, [0xff, 0x00, 0x00]), 0x00ff0000);
        assert_eq!(rgb_to_pixel(masks, [0x00, 0xff, 0x00]), 0x0000ff00);
        assert_eq!(rgb_to_pixel(masks, [0x00, 0x00, 0xff]), 0x000000ff);
        assert_eq!(rgb_to_pixel(masks, [0x40, 0x80, 0xc0]), 0x004080c0);
    }

    #[test]
    fn rgb_to_pixel_scales_narrower_channels() {
        let masks = VisualMasks {
            red: 0x000000f0,
            green: 0x0000000f,
            blue: 0,
        };
        // 0xff is the brightest value a 4-bit (or 8-bit) channel can hold.
        assert_eq!(rgb_to_pixel(masks, [0xff, 0xff, 0x00]), 0xff);
    }

    #[test]
    fn rgb_to_pixel_handles_equal_values() {
        let masks = VisualMasks {
            red: 0xff0000,
            green: 0x00ff00,
            blue: 0x0000ff,
        };
        assert_eq!(rgb_to_pixel(masks, [0x80, 0x80, 0x80]), 0x808080);
    }
}
