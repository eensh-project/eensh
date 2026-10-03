//! X11 display handle and error trapping.
//!
//! `libX11` is loaded dynamically at runtime, so `eensh` does not require X11
//! development headers or a link-time dependency in order to build. If the
//! library is missing, or the requested display cannot be opened, the failure
//! is reported as [`Error::DisplayUnavailable`].

use std::cell::Cell;
use std::ffi::CString;
use std::os::raw::{c_int, c_ulong};
use std::sync::Arc;

use x11_dl::xlib::{Display as XDisplay, XErrorEvent, XImage, XWindowAttributes, Xlib};

use crate::error::Error;
use crate::geometry::Rect;

/// Signature of an installed X11 error handler.
type ErrorHandler = Option<unsafe extern "C" fn(*mut XDisplay, *mut XErrorEvent) -> c_int>;

/// Why no `XSetIOErrorHandler` is installed.
///
/// Xlib calls the IO error handler when the connection itself fails, and then --
/// whether the handler returns or not -- it terminates the process. Installing one
/// therefore cannot contain the failure; it only replaces Xlib's `XIO: fatal IO
/// error ...` message with silence, which would make the problem harder to
/// diagnose for no benefit.
///
/// Containment is achieved instead by never letting Xlib discover the loss:
/// [`Display::begin`] probes the connection before every call that would touch the
/// server, and returns a structured error when the peer has gone.
const _NO_IO_HANDLER: () = ();

/// Signature of `XGetWindowAttributes`, used as a concrete coercion target.
type GetWindowAttributesFn =
    unsafe extern "C" fn(*mut XDisplay, c_ulong, *mut XWindowAttributes) -> c_int;

thread_local! {
    /// Whether the trap is currently armed.
    ///
    /// Arming stays thread-local: it only says "this thread is inside a call that
    /// wants X errors recorded", which is a property of the call, not of the
    /// connection.
    static TRAP_ARMED: Cell<bool> = const { Cell::new(false) };
}

/// The error code recorded by the most recent X11 error.
///
/// # Why this is process-global rather than thread-local
///
/// Xlib dispatches its error handler on whichever thread happens to be inside an
/// Xlib call when the error is reported. A `Display` can legitimately be *created*
/// on one thread and *used* on another: a persistent session opens its connection
/// while serving a `session create` request and then captures on later connection
/// threads. With a thread-local trap, the handler would consult the arming flag of
/// a different thread from the one that armed it, decide the trap was not armed,
/// and silently drop the error code -- so `BadWindow` would degrade into a generic
/// `capture_failed` with no indication why.
///
/// A single slot is sufficient rather than a slot per connection because the
/// sequence "clear, make one Xlib call, sync, take" is only meaningful within one
/// thread. Two threads doing that concurrently could interleave and read each
/// other's code, which would make an error *misattributed*; the atomic compare and
/// exchange on `take` makes the window small, and in practice a connection is used
/// by one thread at a time, which the per-session mutex already guarantees.
static LAST_X_ERROR: AtomicCInt = AtomicCInt::new(0);

/// A `c_int` in an atomic.
///
/// Aliased because `c_int` is `i32` on every supported target, and the alias keeps
/// the intent readable at each use.
type AtomicCInt = std::sync::atomic::AtomicI32;

/// Global X11 error handler installed while a [`Display`] is alive.
///
/// The handler deliberately does not print anything: it only records the error
/// code so that the calling code can convert it into a structured `eensh`
/// error. Returning `0` tells Xlib to continue.
unsafe extern "C" fn record_x_error(_display: *mut XDisplay, event: *mut XErrorEvent) -> c_int {
    TRAP_ARMED.with(|armed| {
        if armed.get() && !event.is_null() {
            LAST_X_ERROR.store(
                (*event).error_code as c_int,
                std::sync::atomic::Ordering::SeqCst,
            );
        }
    });
    0
}

/// Prepare for a call that talks to the server.
///
/// Placed at the start of every such method. The liveness probe must come *before*
/// the Xlib call, not after it: Xlib discovers a dead connection inside its own IO
/// error handler and calls `exit` once that handler returns, so there is no
/// opportunity to react afterwards. The only way to contain the loss is to not
/// make the call.
///
/// Reset the recorded error code and arm recording for the current thread.
///
/// Arming happens per call rather than once at connection open, because the
/// connection may be used from a different thread from the one that opened it, and
/// the arming flag is thread-local. Arming here means "the thread making this call
/// wants X errors recorded", which is exactly the scope that matters.
fn clear_trap() {
    TRAP_ARMED.with(|armed| armed.set(true));
    LAST_X_ERROR.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// The error to report when the connection has been lost.
fn connection_lost() -> Error {
    Error::target_lost(
        "the X11 connection for this session has been lost, so the session can no \
         longer be used. Create a new session; the connection is not transparently \
         re-established, because a reconnect would invalidate frame continuity and \
         the resolved target geometry."
            .to_string(),
    )
}

/// Return and clear the recorded error code.
fn take_trap() -> c_int {
    LAST_X_ERROR.swap(0, std::sync::atomic::Ordering::SeqCst)
}

/// BadWindow / BadDrawable style errors that mean "this XID does not exist".
const X_ERROR_BAD_WINDOW: c_int = 3;

/// An open connection to an X11 display.
///
/// The connection owns the underlying `Display*` and closes it on drop. A
/// capture backend may freely outlive any encoder or output stage, but never
/// the other way around.
pub struct Display {
    xlib: Arc<Xlib>,
    handle: *mut XDisplay,
    name: String,
    previous_handler: ErrorHandler,
}

// The `Display*` is only ever used from a single thread at a time and Xlib's
// connection is internally locked when `XInitThreads` has been called. `eensh`
// is a short-lived single-threaded CLI, but marking these allows `Display` to
// be moved into worker threads by later phases.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    /// Open a connection to the named X11 display, for example `":99"`.
    pub fn open(name: &str) -> Result<Self, Error> {
        let xlib = Xlib::open().map_err(|e| {
            Error::display_unavailable(name, format!("libX11 could not be loaded: {e}"))
        })?;

        let c_name = CString::new(name).map_err(|_| {
            Error::display_unavailable(name, "display name contains an interior NUL byte")
        })?;

        let handle = unsafe { (xlib.XOpenDisplay)(c_name.as_ptr()) };
        if handle.is_null() {
            return Err(Error::display_unavailable(
                name,
                "no such display, or the server refused the connection",
            ));
        }

        // Install the error trap and remember the previous handler so it can be
        // restored when this display is closed. The handler itself is installed
        // once here; the per-call arming flag is what makes it record anything, and
        // it is set by `clear_trap` on whichever thread is making the call.
        let previous_handler: ErrorHandler =
            unsafe { (xlib.XSetErrorHandler)(Some(record_x_error)) };

        Ok(Display {
            xlib: Arc::new(xlib),
            handle,
            name: name.to_string(),
            previous_handler,
        })
    }

    /// Raw Xlib function table.
    pub fn xlib(&self) -> &Xlib {
        &self.xlib
    }

    /// Raw `Display*`.
    pub fn handle(&self) -> *mut XDisplay {
        self.handle
    }

    /// The display name this connection was opened with.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The root window of the default screen.
    pub fn root_window(&self) -> Result<u64, Error> {
        let root = unsafe { (self.xlib.XDefaultRootWindow)(self.handle) };
        if root == 0 {
            return Err(Error::capture_failed(
                "the display reported no root window".to_string(),
            ));
        }
        Ok(root)
    }

    /// Geometry of the root window, in pixels.
    pub fn root_geometry(&self) -> Result<Rect, Error> {
        let root = self.root_window()?;
        self.geometry_of(root)
    }

    /// Geometry of an arbitrary window, in pixels.
    pub fn geometry_of(&self, window: u64) -> Result<Rect, Error> {
        let mut root_return: u64 = 0;
        let mut x: c_int = 0;
        let mut y: c_int = 0;
        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut border_width: u32 = 0;
        let mut depth: u32 = 0;

        self.begin()?;
        let status = unsafe {
            (self.xlib.XGetGeometry)(
                self.handle,
                window,
                &mut root_return,
                &mut x,
                &mut y,
                &mut width,
                &mut height,
                &mut border_width,
                &mut depth,
            )
        };
        self.sync();
        if self.connection_closed() {
            return Err(connection_lost());
        }

        if status == 0 || width == 0 || height == 0 {
            let code = take_trap();
            if code == X_ERROR_BAD_WINDOW {
                return Err(Error::WindowNotFound {
                    window_id: format!("0x{window:x}"),
                });
            }
            return Err(Error::capture_failed(format!(
                "could not read the geometry of window 0x{window:x}"
            )));
        }

        Rect::new(x, y, width, height)
    }

    /// Flush all pending requests and wait for the server to process them.
    ///
    /// Errors generated by earlier requests are delivered to the error handler
    /// during this call.
    ///
    /// The connection is also checked for liveness here. `XSync` cannot report a
    /// dead connection directly: Xlib notices it in its own IO error handler,
    /// whose default action is to print a message and terminate the process. That
    /// is unacceptable for a service, where one session's display going away must
    /// not take unrelated sessions with it. The check is a `poll` for readability
    /// on the connection socket followed by a peek for end of stream, which is
    /// exactly what a closed peer looks like and does not consume any data.
    pub fn sync(&self) {
        // Checked *before* the flush, not after. Xlib reports a dead connection
        // from inside its own IO error handler, whose default action is to print a
        // message and terminate the process -- and it does so from within `XSync`.
        // There is no way to observe the loss afterwards, so the call must not
        // happen at all once the peer has gone.
        if self.connection_closed() {
            return;
        }

        unsafe {
            (self.xlib.XSync)(self.handle, 0);
        }
    }

    /// Probe liveness and arm the error trap, before making a server call.
    ///
    /// Returns [`Error::TargetLost`] when the probe shows the peer has closed.
    ///
    /// Deliberately *not* memoised in a shared flag. A process-wide "a connection
    /// died" flag would make one session's loss poison every other session in the
    /// service, including sessions created afterwards: the new connection would be
    /// refused before it was even probed. Liveness is a property of *this*
    /// connection, so it is asked about this connection every time.
    ///
    /// A connection that dies in the window between this probe and the call is not
    /// covered. That window is unavoidable: Xlib terminates the process from its
    /// own IO error handler, so the loss cannot be reacted to after the fact.
    fn begin(&self) -> Result<(), Error> {
        if self.connection_closed() {
            return Err(connection_lost());
        }
        clear_trap();
        Ok(())
    }

    /// Whether the server side of the connection has closed.
    ///
    /// A closed peer is reported as readable with nothing to read, so the socket
    /// is probed without blocking and without consuming a byte.
    ///
    /// Returns `true` for a dead connection unconditionally, so that callers get a
    /// single reliable predicate whether the loss was discovered here or by Xlib's
    /// IO error handler.
    #[allow(dead_code)]
    fn connection_closed(&self) -> bool {
        let fd = unsafe { (self.xlib.XConnectionNumber)(self.handle) };
        if fd < 0 {
            return false;
        }

        let mut descriptor = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };

        // A zero timeout: this must never block, because it runs on every call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if ready <= 0 {
            // Nothing pending, so the connection is either healthy or simply idle.
            return false;
        }

        // Something is pending. Peek at it: end of stream reports zero bytes, and
        // `MSG_PEEK` leaves any real data in the buffer for Xlib to consume.
        let mut byte = 0u8;
        let peeked = unsafe {
            libc::recv(
                fd,
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };

        peeked == 0
    }

    /// Read the attributes of a window, converting a `BadWindow` into
    /// [`Error::WindowNotFound`].
    pub fn window_attributes(&self, window: u64) -> Result<XWindowAttributes, Error> {
        let mut attributes: XWindowAttributes = unsafe { std::mem::zeroed() };

        self.begin()?;
        let get_attributes: GetWindowAttributesFn = self.xlib.XGetWindowAttributes;
        let status = unsafe { get_attributes(self.handle, window, &mut attributes) };
        self.sync();
        if self.connection_closed() {
            return Err(connection_lost());
        }

        if status == 0 {
            let code = take_trap();
            if code == X_ERROR_BAD_WINDOW {
                return Err(Error::WindowNotFound {
                    window_id: format!("0x{window:x}"),
                });
            }
            return Err(Error::capture_failed(format!(
                "could not read attributes for window 0x{window:x}"
            )));
        }

        Ok(attributes)
    }

    /// Translate a point from one window's coordinate space to another's.
    pub fn translate_coordinates(
        &self,
        source: u64,
        destination: u64,
        x: c_int,
        y: c_int,
    ) -> Result<(c_int, c_int), Error> {
        let mut dest_x: c_int = 0;
        let mut dest_y: c_int = 0;
        let mut child: u64 = 0;

        self.begin()?;
        let status = unsafe {
            (self.xlib.XTranslateCoordinates)(
                self.handle,
                source,
                destination,
                x,
                y,
                &mut dest_x,
                &mut dest_y,
                &mut child,
            )
        };
        self.sync();
        if self.connection_closed() {
            return Err(connection_lost());
        }

        if status == 0 {
            return Err(Error::capture_failed(
                "could not translate window coordinates to the root window".to_string(),
            ));
        }

        Ok((dest_x, dest_y))
    }

    /// Copy a rectangle out of a window into a freshly allocated `XImage`.
    ///
    /// Returns the `XImage` together with the rectangle that was actually read.
    /// The returned image must be released with [`Display::destroy_image`].
    pub fn get_image(&self, window: u64, rect: &Rect) -> Result<(*mut XImage, Rect), Error> {
        self.begin()?;
        let image = unsafe {
            (self.xlib.XGetImage)(
                self.handle,
                window,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                ALL_PLANES,
                ZPIXMAP,
            )
        };
        self.sync();
        if self.connection_closed() {
            return Err(connection_lost());
        }

        if image.is_null() {
            let code = take_trap();
            if code == X_ERROR_BAD_WINDOW {
                return Err(Error::WindowNotFound {
                    window_id: format!("0x{window:x}"),
                });
            }
            return Err(Error::capture_failed(format!(
                "XGetImage failed for window 0x{window:x} at {}x{}+{}+{}",
                rect.width, rect.height, rect.x, rect.y
            )));
        }

        Ok((image, *rect))
    }

    /// Release an `XImage` returned by [`Display::get_image`].
    ///
    /// # Safety
    ///
    /// `image` must be a pointer returned by [`Display::get_image`] and must
    /// not have been destroyed already.
    pub unsafe fn destroy_image(&self, image: *mut XImage) {
        if !image.is_null() {
            (self.xlib.XDestroyImage)(image);
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        TRAP_ARMED.with(|armed| armed.set(false));
        LAST_X_ERROR.store(0, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            (self.xlib.XSetErrorHandler)(self.previous_handler);
            if !self.handle.is_null() {
                (self.xlib.XCloseDisplay)(self.handle);
            }
        }
        self.handle = std::ptr::null_mut();
    }
}

impl std::fmt::Debug for Display {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Display").field("name", &self.name).finish()
    }
}

/// `ZPixmap` from `X.h`. Declared locally to avoid depending on whether the
/// binding crate re-exports the constant.
const ZPIXMAP: c_int = 2;

/// `AllPlanes` from `X.h`: all bits set for the plane mask type.
const ALL_PLANES: c_ulong = c_ulong::MAX;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_bogus_display_reports_display_unavailable() {
        // Display numbers this high are never assigned in practice; the
        // important property is that the failure is classified correctly and
        // does not panic.
        let result = Display::open(":54321");
        match result {
            Err(Error::DisplayUnavailable { .. }) => {}
            Err(other) => panic!("expected display_unavailable, got {other:?}"),
            Ok(_) => panic!("unexpectedly opened a display that should not exist"),
        }
    }
}
