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

/// Signature of `XGetWindowAttributes`, used as a concrete coercion target.
type GetWindowAttributesFn =
    unsafe extern "C" fn(*mut XDisplay, c_ulong, *mut XWindowAttributes) -> c_int;

thread_local! {
    /// Error code recorded by the most recent X11 error, if the trap is armed.
    static LAST_X_ERROR: Cell<c_int> = const { Cell::new(0) };
    /// Whether the trap is currently armed.
    static TRAP_ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Global X11 error handler installed while a [`Display`] is alive.
///
/// The handler deliberately does not print anything: it only records the error
/// code so that the calling code can convert it into a structured `eensh`
/// error. Returning `0` tells Xlib to continue.
unsafe extern "C" fn record_x_error(_display: *mut XDisplay, event: *mut XErrorEvent) -> c_int {
    TRAP_ARMED.with(|armed| {
        if armed.get() && !event.is_null() {
            LAST_X_ERROR.with(|slot| slot.set((*event).error_code as c_int));
        }
    });
    0
}

/// Reset the recorded error code, if the trap is armed.
fn clear_trap() {
    TRAP_ARMED.with(|armed| {
        if armed.get() {
            LAST_X_ERROR.with(|slot| slot.set(0));
        }
    });
}

/// Return and clear the recorded error code.
fn take_trap() -> c_int {
    LAST_X_ERROR.with(|slot| slot.replace(0))
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
        // restored when this display is closed.
        let previous_handler: ErrorHandler =
            unsafe { (xlib.XSetErrorHandler)(Some(record_x_error)) };
        TRAP_ARMED.with(|armed| armed.set(true));

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

        clear_trap();
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
    pub fn sync(&self) {
        unsafe {
            (self.xlib.XSync)(self.handle, 0);
        }
    }

    /// Read the attributes of a window, converting a `BadWindow` into
    /// [`Error::WindowNotFound`].
    pub fn window_attributes(&self, window: u64) -> Result<XWindowAttributes, Error> {
        let mut attributes: XWindowAttributes = unsafe { std::mem::zeroed() };

        clear_trap();
        let get_attributes: GetWindowAttributesFn = self.xlib.XGetWindowAttributes;
        let status = unsafe { get_attributes(self.handle, window, &mut attributes) };
        self.sync();

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

        clear_trap();
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
        clear_trap();
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
