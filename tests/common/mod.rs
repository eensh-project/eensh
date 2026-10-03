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
use std::collections::HashSet;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use x11_dl::xlib::{Display as XDisplay, XWindowAttributes, Xlib};

use eensh::service::unix;

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
    lock: PathBuf,
}

impl Xvfb {
    /// Start an `Xvfb` with the given virtual screen size.
    ///
    /// Returns `None` when no `Xvfb` binary is available.
    pub fn start(width: u32, height: u32) -> Option<Xvfb> {
        let binary = find_xvfb()?;

        // A previous run that was hard-killed can leave a lock file behind; Xvfb
        // itself checks the recorded PID before trusting a lock, so do the same
        // and reclaim the display numbers instead of leaking them. This matters
        // because the search range is finite.
        remove_stale_locks();

        for number in DISPLAY_RANGE {
            let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
            let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
            // A live server holds both the socket and the lock.
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
                lock,
            };

            if wait_for_display(&server.display, Duration::from_secs(10)) {
                return Some(server);
            }

            server.terminate();
        }

        None
    }

    /// The display name, for example `":91"`.
    pub fn display(&self) -> &str {
        &self.display
    }

    /// Stop the server, giving it a chance to clean up after itself.
    ///
    /// `Child::kill` sends `SIGKILL`, which Xvfb cannot act on, so its lock file
    /// would be left behind and the display number leaked. `SIGTERM` lets it shut
    /// down properly; `SIGKILL` is only the fallback if it does not exit in time.
    fn terminate(&mut self) {
        let pid = self.child.id() as i32;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }

        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();

        // Wait for the socket to disappear so an immediate rerun can reuse the
        // display number.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && self.socket.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }

        // If the server still did not clean up, remove the lock ourselves, but
        // only when it really does belong to the process we just stopped.
        remove_lock_if_owned(&self.lock, pid);
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// The display numbers the harness will try.
///
/// Wide enough that a machine with many live or stale displays still finds a free
/// one.
const DISPLAY_RANGE: std::ops::Range<u32> = 90..400;

/// Remove lock files whose recorded process is gone.
///
/// This mirrors what an X server does when it starts: a lock naming a dead PID is
/// stale and may be reclaimed. Only genuinely stale locks in the harness's own
/// range are touched.
fn remove_stale_locks() {
    for number in DISPLAY_RANGE {
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        if let Some(pid) = read_lock_pid(&lock) {
            if !process_exists(pid) {
                let _ = std::fs::remove_file(&lock);
            }
        }
    }
}

/// Remove a lock file only if it names `expected_pid` and that process is gone.
fn remove_lock_if_owned(lock: &PathBuf, expected_pid: i32) {
    if let Some(pid) = read_lock_pid(lock) {
        if pid == expected_pid && !process_exists(pid) {
            let _ = std::fs::remove_file(lock);
        }
    }
}

/// Read the PID an X server recorded in its lock file.
fn read_lock_pid(lock: &PathBuf) -> Option<i32> {
    let contents = std::fs::read_to_string(lock).ok()?;
    contents.trim().parse::<i32>().ok()
}

/// Whether a process is still alive.
fn process_exists(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
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

// ============================================================================
// Scripted painting
// ============================================================================

/// One rectangle to paint.
#[derive(Debug, Clone, Copy)]
pub struct PaintOp {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub rgb: [u8; 3],
}

/// Paint on a schedule from a background thread, holding its own X connection.
///
/// A scripted painter is what makes temporal tests deterministic rather than
/// racy: the paints happen at known offsets from the start of the observation,
/// and the observer is expected to end up in the corresponding state.
///
/// The painter holds its own connection so it never races the observer's, and it
/// holds that connection open until it is dropped, so Xvfb does not reset the
/// root window while a scenario is still running.
pub struct Painter {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Painter {
    /// Start painting according to `script`, expressed as
    /// `(delay_from_start, paints)`.
    pub fn start(display: String, script: Vec<(Duration, Vec<PaintOp>)>) -> Painter {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            let screen = Screen::open(&display);
            let masks = screen.visual_masks();

            let started = Instant::now();
            for (delay, ops) in script {
                // Sleep in small slices so the stop flag is honoured promptly.
                while started.elapsed() < delay {
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    let remaining = delay.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                for op in &ops {
                    screen.fill(
                        screen.root(),
                        op.x,
                        op.y,
                        op.width,
                        op.height,
                        rgb_to_pixel(masks, op.rgb),
                    );
                }
            }

            // Hold the connection open until told to stop, so Xvfb does not reset
            // the root window while the observer is still capturing.
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        Painter {
            stop,
            handle: Some(handle),
        }
    }

    /// Start with a single immediate paint, then hold the connection open.
    pub fn paint_once(display: String, ops: Vec<PaintOp>) -> Painter {
        Painter::start(display, vec![(Duration::ZERO, ops)])
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A rectangle covering a fraction of a small test screen.
///
/// On the usual 400x300 test screen a 100x60 rectangle is 6% of the area, which
/// clears the default 0.5% threshold comfortably.
pub fn paint_rect(x: i32, y: i32, rgb: [u8; 3]) -> PaintOp {
    PaintOp {
        x,
        y,
        width: 100,
        height: 60,
        rgb,
    }
}

// ============================================================================
// Service harness
// ============================================================================

/// A running `eensh serve` bound to a private socket.
///
/// The socket lives in a per-test temporary directory rather than the default
/// location, so tests never interfere with a developer's real service and never
/// leave a socket in a shared path. Dropping the handle terminates the service
/// and waits for it, which also removes the socket.
pub struct ServiceProcess {
    child: std::process::Child,
    socket: PathBuf,
    directory: PathBuf,
}

impl ServiceProcess {
    /// Start a service on a private socket.
    pub fn start(name: &str) -> ServiceProcess {
        let directory = temp_dir(&format!("service-{name}"));
        let socket = directory.join("eensh.sock");
        ServiceProcess::start_at(&socket)
    }

    /// Start a service on an explicit socket path.
    ///
    /// Used by the restart test, which needs a path that already holds a stale
    /// socket file from a simulated crash.
    pub fn start_at(socket: &Path) -> ServiceProcess {
        let directory = socket
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&directory).expect("the socket directory should be creatable");
        let socket = socket.to_path_buf();

        let child = Command::new(eensh_binary())
            .arg("serve")
            .arg("--socket")
            .arg(&socket)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start eensh serve");

        let service = ServiceProcess {
            child,
            socket,
            directory,
        };

        // Wait until the service actually answers, rather than merely until the
        // path exists. The distinction matters when a *stale* socket file is being
        // reclaimed: the file is present before the new service has bound it, so
        // an existence check can return while the service is still starting, and
        // then fail to connect.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if unix::connect(&service.socket).is_ok() {
                return service;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "the service never became reachable at {}",
            service.socket.display()
        );
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The service process id, for measuring its memory.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The service process's resident set size in bytes, if it can be read.
    ///
    /// Read from `/proc`, which is the most direct available answer on Linux and
    /// avoids adding a dependency. Returns `None` rather than failing when the
    /// value is unavailable, so a measurement test can report honestly instead of
    /// asserting something it could not observe.
    pub fn resident_bytes(&self) -> Option<u64> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).ok()?;
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                let kilobytes: u64 = value.trim().trim_end_matches(" kB").trim().parse().ok()?;
                return Some(kilobytes * 1024);
            }
        }
        None
    }

    /// The socket path as a string, for passing to the CLI.
    pub fn socket_arg(&self) -> String {
        self.socket.display().to_string()
    }

    /// Run a CLI command against this service, returning `(exit, stdout, stderr)`.
    pub fn run(&self, args: &[&str]) -> (i32, String, String) {
        let mut full = vec!["session"];
        full.extend_from_slice(args);
        full.push("--socket");
        let socket = self.socket_arg();
        full.push(&socket);
        run_eensh_text(&full)
    }

    /// Run a CLI command, expecting JSON on stdout, and parse it.
    pub fn run_json(&self, args: &[&str]) -> (i32, serde_json::Value) {
        let (code, stdout, stderr) = self.run(args);
        let value = serde_json::from_str(stdout.trim()).unwrap_or_else(|error| {
            panic!("stdout is not JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}")
        });
        (code, value)
    }

    /// Run a CLI command that is expected to fail, and parse the JSON error.
    ///
    /// Failures are reported on stderr, as they are for every command except the
    /// observation ones, whose JSON is the result rather than a diagnostic.
    pub fn run_error(&self, args: &[&str]) -> (i32, serde_json::Value) {
        let (code, stdout, stderr) = self.run(args);
        let value = serde_json::from_str(stderr.trim()).unwrap_or_else(|error| {
            panic!("stderr is not JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}")
        });
        (code, value)
    }

    /// Send a raw JSON string over the socket, bypassing the CLI entirely.
    ///
    /// Used for the malformed-request and protocol-version tests, which are
    /// about the wire contract rather than about the CLI.
    pub fn send_raw(&self, body: &str) -> Result<String, String> {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&self.socket).map_err(|e| e.to_string())?;
        let bytes = body.as_bytes();
        let length = (bytes.len() as u32).to_be_bytes();
        stream.write_all(&length).map_err(|e| e.to_string())?;
        stream.write_all(bytes).map_err(|e| e.to_string())?;
        stream.flush().map_err(|e| e.to_string())?;

        // Read the length-prefixed reply.
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).map_err(|e| e.to_string())?;
        let size = u32::from_be_bytes(header) as usize;
        let mut body = vec![0u8; size];
        stream.read_exact(&mut body).map_err(|e| e.to_string())?;
        String::from_utf8(body).map_err(|e| e.to_string())
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        // Ask politely first: the service is expected to exit and remove its
        // socket. A process that ignores this is a failure of the thing under
        // test, and is reported rather than hidden by a forceful kill.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = false;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }

        if !exited {
            eprintln!("eensh serve ignored SIGTERM; killing it");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        // Best effort: a killed process leaves its socket behind.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Whether a session identifier is registered with a service.
pub fn session_exists(service: &ServiceProcess, session_id: &str) -> bool {
    let (code, stdout, _) = service.run(&["list", "--json"]);
    if code != 0 {
        return false;
    }
    stdout.contains(session_id)
}

/// Decode standard base64.
///
/// Hand-written rather than pulled from a dependency: the tests only need to
/// prove that the bytes the service sent are a decodable image, and a decoder
/// this small is easier to trust than one more crate in the tree.
pub fn base64_decode(input: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut lookup = [255u8; 256];
    for (index, byte) in TABLE.iter().enumerate() {
        lookup[*byte as usize] = index as u8;
    }

    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;

    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = lookup[byte as usize];
        if value == 255 {
            continue;
        }
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
        }
    }

    output
}

/// Run `eensh` with arguments and parse a JSON stdout response.
///
/// Returns `(exit, value, stderr)`. Observation commands put their JSON on
/// stdout, so this is what both the standalone and session observation tests use.
/// When stdout is not JSON the value is `Null` and the stderr string explains what
/// both streams contained, so a failure message is useful rather than merely
/// "expected a value".
pub fn run_json_stdout(args: &[&str]) -> (i32, serde_json::Value, String) {
    let (code, stdout, stderr) = run_eensh_text(args);
    match serde_json::from_str(stdout.trim()) {
        Ok(value) => (code, value, stderr),
        Err(error) => (
            code,
            serde_json::Value::Null,
            format!("stdout is not JSON ({error}); stdout: {stdout:?} stderr: {stderr:?}"),
        ),
    }
}

/// Run `eensh` with arguments and parse a JSON stderr response.
///
/// Failures are reported on stderr, as they are for every other `eensh` command
/// except the observation ones.
pub fn run_json_stderr(args: &[&str]) -> (i32, serde_json::Value) {
    let (code, _stdout, stderr) = run_eensh_text(args);
    let value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|error| panic!("stderr is not JSON ({error}): {stderr:?}"));
    (code, value)
}

/// Start an Xvfb plus a service, or `None` when Xvfb is unavailable.
///
/// The two are started together so a caller cannot accidentally start a service
/// without a display, which would make every session creation fail for a reason
/// unrelated to the test.
pub fn xvfb_service(name: &str, width: u32, height: u32) -> Option<(Xvfb, String, ServiceProcess)> {
    if find_xvfb().is_none() {
        assert!(!xvfb_required(), "Xvfb is required but missing");
        return None;
    }
    let (server, display) = xvfb(width, height)?;
    let service = ServiceProcess::start(name);
    Some((server, display, service))
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
    // The name identifies the test and is deliberately *not* qualified by the
    // process id. A pid-qualified path can never be reclaimed by a later run,
    // because the next run has a different pid and therefore a different path, so
    // every run would leave its directories behind permanently.
    //
    // The directory is wiped exactly *once per test*, not once per call. A test
    // commonly asks for several paths from the same directory (`before.png`,
    // `after.png`, `changed.png`), and wiping on each call would delete the files
    // the earlier calls named. Tracking the names already prepared in this process
    // gives the intended behaviour: stale state from a previous run is cleared,
    // while repeated calls within one test share the directory.
    let directory = std::env::temp_dir().join(format!("eensh-it-{name}"));

    let prepared = PREPARED_DIRECTORIES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut prepared = prepared
        .lock()
        .expect("the prepared-directory set is not poisoned");

    if prepared.insert(name.to_string()) {
        // First use of this name in this process, so any leftover from a previous
        // run is removed before the test starts.
        if directory.exists() {
            let _ = std::fs::remove_dir_all(&directory);
        }
        std::fs::create_dir_all(&directory).expect("could not create the test output directory");
    }

    directory
}

/// Test directories already prepared in this process, by name.
static PREPARED_DIRECTORIES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

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
