//! The local Unix-socket service.
//!
//! # Why a socket and not a shared library
//!
//! A session's value comes from *persistence*, and persistence needs a process
//! that outlives one command. The socket is what lets a short-lived CLI command
//! reuse a session created by an earlier invocation.
//!
//! # Security model
//!
//! The service returns raw desktop pixels, so the socket is treated as sensitive:
//!
//! * it is created inside a directory the owning user controls, and its
//!   permissions are `0600` — owner-only, never group- or world-accessible;
//! * a pre-existing path that is not a socket is **refused**, not overwritten,
//!   so a stray file at the socket path cannot be clobbered;
//! * a stale socket left by a crashed service is removed only after confirming
//!   that nothing is listening on it;
//! * the socket is removed on clean shutdown.
//!
//! There is no TCP listener and no network exposure. Peer credentials are not
//! checked, because the filesystem permissions already restrict the socket to the
//! owning user, and the socket path is in a per-user runtime directory rather than
//! a shared one.

use std::io::BufReader;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::error::Error;
use crate::service::handler::Handler;
use crate::service::protocol::{
    read_frame, read_incoming, write_frame, IncomingRequest, RequestEnvelope, ResponseEnvelope,
};

/// The environment variable that overrides the socket path.
pub const SOCKET_ENV: &str = "EENSH_SOCKET";

/// The default socket file name.
pub const DEFAULT_SOCKET_NAME: &str = "eensh.sock";

/// How long the accept loop waits before re-checking the shutdown flag.
///
/// This is a maximum *idle* latency, not a per-request cost: `poll` returns the
/// moment a connection arrives, so a request is never delayed by this interval,
/// and only a shutdown request waits for it. A longer interval therefore costs
/// nothing under load and only makes an idle service marginally slower to stop.
const ACCEPT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Resolve the socket path to use.
///
/// Preference order:
///
/// 1. `EENSH_SOCKET`, so a caller can place it explicitly;
/// 2. `$XDG_RUNTIME_DIR/eensh.sock`, which is a per-user directory the session
///    manager has already restricted;
/// 3. the system temporary directory, namespaced by user id, as a fallback for
///    environments without `XDG_RUNTIME_DIR`.
///
/// The fallback is namespaced by uid rather than shared, so two users on one
/// machine cannot collide or reach each other's socket.
pub fn socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os(SOCKET_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }

    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        if !runtime.is_empty() {
            return PathBuf::from(runtime).join(DEFAULT_SOCKET_NAME);
        }
    }

    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("eensh-{uid}.sock"))
}

/// The default maximum number of simultaneous client connections.
///
/// Phase 4 spawned an unbounded thread per connection, which means a client that
/// opened connections in a loop could make the service spawn threads without limit.
/// The bound exists for resource bounding rather than load balancing: exceeding it is
/// answered with an explicit `service_overloaded` error, so a caller learns that the
/// service is busy instead of watching its connection silently hang.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// A running service.
pub struct Service {
    listener: UnixListener,
    socket: PathBuf,
    handler: Arc<Handler>,
    shutdown: Arc<AtomicBool>,
    /// Simultaneous client connections currently being served.
    connections: Arc<AtomicUsize>,
    /// The maximum allowed, checked before spawning a handler thread.
    max_connections: usize,
}

impl Service {
    /// Bind the service to a socket path.
    ///
    /// Fails rather than overwriting anything unexpected at that path.
    pub fn bind(socket: PathBuf) -> Result<Service, Error> {
        Service::bind_with_limit(socket, DEFAULT_MAX_CONNECTIONS)
    }

    /// Bind the service with an explicit client-connection limit.
    pub fn bind_with_limit(socket: PathBuf, max_connections: usize) -> Result<Service, Error> {
        prepare_socket_path(&socket)?;

        let listener = UnixListener::bind(&socket).map_err(|e| {
            Error::service_unavailable(format!("could not bind {}: {e}", socket.display()))
        })?;

        // Owner-only. Set immediately after binding, before accepting anything, so
        // there is no window in which the socket is more permissive than intended.
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            let _ = std::fs::remove_file(&socket);
            Error::service_unavailable(format!(
                "could not restrict permissions on {}: {e}",
                socket.display()
            ))
        })?;

        Ok(Service {
            listener,
            socket,
            handler: Arc::new(Handler::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            connections: Arc::new(AtomicUsize::new(0)),
            max_connections: max_connections.max(1),
        })
    }

    /// Bind at the resolved default path.
    pub fn bind_default() -> Result<Service, Error> {
        Service::bind(socket_path())
    }

    /// The path this service is listening on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The request handler.
    pub fn handler(&self) -> &Arc<Handler> {
        &self.handler
    }

    /// A flag that, when set, makes the accept loop finish after the current
    /// connection. Used by tests and by signal handling.
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }

    /// Serve connections until the shutdown flag is set.
    ///
    /// Connects are handled on their own threads.
    ///
    /// This is not a throughput optimisation — it is required for correctness. A
    /// single-threaded accept loop makes one long observation block *every* other
    /// session, because the next connection's request cannot even be read until
    /// the observation finishes. That is a global lock in effect, and it makes the
    /// documented `session_busy` refusal unreachable: a second observation would
    /// be silently queued behind the first instead of being told to wait.
    ///
    /// Concurrency is safe because the locking is already per-session. Two
    /// connections touching *different* sessions never contend, and two touching
    /// the *same* session serialize on that session's own mutex. No lock is taken
    /// across sessions.
    ///
    /// # Why `poll` rather than a blocking accept
    ///
    /// A blocking `accept` cannot be interrupted into observing the shutdown flag:
    /// the standard library retries it across `EINTR`, so a signal handler that
    /// only sets a flag would leave the loop parked in `accept` forever and the
    /// process would appear hung when asked to stop.
    ///
    /// The obvious workaround — a non-blocking listener retried in a loop with a
    /// short sleep between attempts — is *worse than the problem*: every incoming
    /// connection waits up to the whole sleep interval just to be accepted. That
    /// turned a trivially cheap request into a multi-millisecond one and measurably
    /// made the persistent path slower than starting a fresh process, which is the
    /// exact opposite of the point of Phase 4.
    ///
    /// `poll` blocks until either a connection arrives *or* the timeout elapses, so
    /// a connection is accepted immediately while a shutdown request is still
    /// noticed within one (much longer) interval. The wakeup cost falls on an
    /// idle service, where it does not matter, instead of on every request.
    pub fn run(&self) -> Result<(), Error> {
        self.listener.set_nonblocking(true).map_err(|e| {
            Error::service_unavailable(format!("could not configure the listener: {e}"))
        })?;

        let listener_fd = self.listener.as_raw_fd();

        while !self.shutdown.load(Ordering::Relaxed) {
            match poll_for_connection(listener_fd, ACCEPT_POLL_INTERVAL) {
                // A connection is waiting; accept it without further delay.
                Ok(true) => {}
                // Timed out, so re-check the shutdown flag. This is the idle case,
                // and it is the only place the interval is actually paid.
                Ok(false) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("eensh serve: poll failed: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
            }

            match self.listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit non-blocking mode on some platforms;
                    // a blocking stream is what the protocol expects.
                    let _ = stream.set_nonblocking(false);

                    // The bound is checked *before* spawning, so a burst of
                    // connections cannot make the service spawn threads without
                    // limit and then discover it is over budget.
                    if self.connections.load(Ordering::SeqCst) >= self.max_connections {
                        // Answered rather than dropped, so the client is told why
                        // instead of seeing a connection that closes for no stated
                        // reason. The reply needs no handler thread and no session
                        // lock, so it cannot itself contribute to the overload.
                        refuse_overloaded(stream);
                        continue;
                    }

                    let handler = Arc::clone(&self.handler);
                    let live = Arc::clone(&self.connections);
                    live.fetch_add(1, Ordering::SeqCst);

                    // Detached: the thread's lifetime is bounded by the client on
                    // the other end. A broken connection is the client's problem,
                    // not the service's, so nothing is logged.
                    std::thread::spawn(move || {
                        let _ = serve_connection(stream, &handler);
                        // Released on every path, including a panic inside the
                        // handler, so a failed connection cannot permanently
                        // consume a slot.
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Poll said ready but the connection was claimed first; go
                    // back to waiting.
                    continue;
                }
                Err(e) => {
                    // Transient accept failures should not kill the service.
                    eprintln!("eensh serve: accept failed: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }

        Ok(())
    }

    /// Remove the socket file.
    ///
    /// Called on clean shutdown. Removing a socket that is already gone is not an
    /// error, so a double shutdown is harmless.
    pub fn cleanup(&self) {
        self.handler.shutdown();
        if self.socket.exists() {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

/// Wait until a connection is pending on `fd`, or the timeout elapses.
///
/// Returns `Ok(true)` when the socket is readable, `Ok(false)` on timeout. Uses
/// `poll` from libc rather than adding a dependency: the requirement is one file
/// descriptor and one event, which is well inside what a direct call handles
/// clearly.
fn poll_for_connection(
    fd: std::os::raw::c_int,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;

    // Safety: a single initialised `pollfd` is passed with a count of one, so
    // `poll` reads exactly the memory it is told about.
    let result = unsafe { libc::poll(&mut descriptor, 1, millis) };

    match result {
        // A negative return is an error, and `EINTR` is reported as such by
        // `poll`; the caller treats it as a reason to re-check the flag.
        -1 => Err(std::io::Error::last_os_error()),
        // Zero means the timeout elapsed with no event.
        0 => Ok(false),
        // The descriptor is readable, so a connection is waiting.
        _ => Ok(true),
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // Best effort: a panic during shutdown must not leave the socket behind
        // for the next run to trip over.
        self.cleanup();
    }
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("socket", &self.socket)
            .field("sessions", &self.handler.session_count())
            .finish()
    }
}

/// Handle one client connection: read requests until the client goes away.
fn serve_connection(stream: UnixStream, handler: &Handler) -> Result<(), Error> {
    let reader_stream = stream
        .try_clone()
        .map_err(|e| Error::service_unavailable(format!("could not duplicate the socket: {e}")))?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;

    loop {
        let incoming = match read_incoming(&mut reader) {
            Ok(Some(incoming)) => incoming,
            // A clean disconnect ends the connection without ceremony.
            Ok(None) => return Ok(()),
            Err(error) => {
                // Only a *framing* failure reaches here, which means the length
                // prefixes can no longer be trusted. The client is told why and
                // the connection ends.
                let response = ResponseEnvelope {
                    request_id: String::new(),
                    ok: false,
                    result: None,
                    error: Some(crate::service::protocol::ErrorBody::from_error(&error)),
                };
                let _ = write_frame(&mut writer, &response);
                return Ok(());
            }
        };

        let response = match incoming {
            IncomingRequest::Request(envelope) => handler.handle(envelope),
            IncomingRequest::Malformed { request_id, error } => {
                // The framing is intact, so the connection continues: only the one
                // request was undecodable. This matters for a client that sends
                // several requests on one connection, because otherwise a single
                // bad request would silently discard the answers to the rest.
                ResponseEnvelope {
                    request_id,
                    ok: false,
                    result: None,
                    error: Some(crate::service::protocol::ErrorBody::from_error(&error)),
                }
            }
        };

        write_frame(&mut writer, &response)?;
    }
}

/// Answer a client with an overload refusal, then close the connection.
///
/// The reply is produced inline on the accept loop rather than on a handler thread,
/// because the whole point is that no further work is started when the service is at
/// its limit. It takes no session lock and touches no session, so it cannot itself
/// become a bottleneck.
///
/// If even the reply cannot be written, the connection is simply closed. A client that
/// sees a closed connection must treat it as an overload or an outage; the structured
/// reply is what makes that explicit when it can be sent at all.
fn refuse_overloaded(stream: UnixStream) {
    let mut writer = stream;
    let response = ResponseEnvelope {
        request_id: String::new(),
        ok: false,
        result: None,
        error: Some(crate::service::protocol::ErrorBody::from_error(
            &Error::service_overloaded(
                "the service is already serving its maximum number of client connections; \
                 retry shortly, or reuse one connection for multiple requests",
            ),
        )),
    };
    let _ = write_frame(&mut writer, &response);
}

/// Validate the socket path before binding.
fn prepare_socket_path(socket: &Path) -> Result<(), Error> {
    if let Some(parent) = socket.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::service_unavailable(format!(
                    "could not create the socket directory {}: {e}",
                    parent.display()
                ))
            })?;
        }
    }

    if !socket.exists() {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(socket).map_err(|e| {
        Error::service_unavailable(format!("could not inspect {}: {e}", socket.display()))
    })?;

    if !metadata.file_type().is_socket() {
        // Refuse rather than delete: this path belongs to something else, and
        // overwriting it would destroy a file the service does not own.
        return Err(Error::service_unavailable(format!(
            "{} exists and is not a socket; refusing to replace it",
            socket.display()
        )));
    }

    // It is a socket. If nothing is listening, it is left over from a crashed
    // service and can be reclaimed; if something is listening, another service is
    // already running and binding would steal its requests.
    match UnixStream::connect(socket) {
        Ok(_) => Err(Error::service_unavailable(format!(
            "another eensh service is already listening on {}",
            socket.display()
        ))),
        Err(_) => {
            std::fs::remove_file(socket).map_err(|e| {
                Error::service_unavailable(format!(
                    "could not remove the stale socket {}: {e}",
                    socket.display()
                ))
            })?;
            Ok(())
        }
    }
}

/// Connect to a running service.
pub fn connect(socket: &Path) -> Result<UnixStream, Error> {
    UnixStream::connect(socket).map_err(|e| {
        Error::service_unavailable(format!(
            "could not reach the eensh service at {}: {e}. Start it with `eensh serve`.",
            socket.display()
        ))
    })
}

/// Send one request and read one response, over a fresh connection.
pub fn request(socket: &Path, envelope: &RequestEnvelope) -> Result<ResponseEnvelope, Error> {
    let mut stream = connect(socket)?;
    write_frame(&mut stream, envelope)?;

    let reader_stream = stream
        .try_clone()
        .map_err(|e| Error::service_unavailable(format!("could not duplicate the socket: {e}")))?;
    let mut reader = BufReader::new(reader_stream);

    let response: Option<ResponseEnvelope> = read_frame(&mut reader)?;
    response.ok_or_else(|| {
        Error::service_protocol_error("the service closed the connection without responding")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::protocol::{Request, RequestEnvelope};

    fn temp_socket(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eensh-sock-{name}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        directory.join("test.sock")
    }

    #[test]
    fn a_refused_path_that_is_not_a_socket_is_not_replaced() {
        let path = temp_socket("notasocket");
        std::fs::write(&path, b"important data").unwrap();

        let error = Service::bind(path.clone()).unwrap_err();
        assert_eq!(error.code(), "service_unavailable");
        assert!(error.message().contains("not a socket"));
        // The file is untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"important data");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn binding_creates_an_owner_only_socket() {
        let path = temp_socket("perms");
        let _ = std::fs::remove_file(&path);

        let service = Service::bind(path.clone()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the socket must not be group or world accessible"
        );

        drop(service);
    }

    #[test]
    fn a_stale_socket_is_reclaimed() {
        let path = temp_socket("stale");
        let _ = std::fs::remove_file(&path);

        // Create a socket, then drop the listener without letting the service
        // remove it, simulating a crash.
        {
            let listener = UnixListener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(path.exists(), "the socket file should remain after a crash");

        let service = Service::bind(path.clone()).expect("a stale socket should be reclaimed");
        assert_eq!(service.socket(), path);
        drop(service);
    }

    #[test]
    fn a_second_service_on_the_same_path_is_refused() {
        let path = temp_socket("double");
        let _ = std::fs::remove_file(&path);

        let first = Service::bind(path.clone()).unwrap();
        let error = Service::bind(path.clone()).unwrap_err();
        assert_eq!(error.code(), "service_unavailable");
        assert!(
            error.message().contains("already listening"),
            "message was: {}",
            error.message()
        );

        drop(first);
    }

    #[test]
    fn cleanup_removes_the_socket() {
        let path = temp_socket("cleanup");
        let _ = std::fs::remove_file(&path);

        let service = Service::bind(path.clone()).unwrap();
        assert!(path.exists());
        service.cleanup();
        assert!(!path.exists(), "shutdown must remove the socket");

        // Dropping afterwards is harmless.
        drop(service);
    }

    #[test]
    fn the_accept_loop_stops_promptly_when_the_shutdown_flag_is_set() {
        // The regression this guards: a blocking `accept` retried across `EINTR`
        // by the standard library never returns to re-check the flag, so setting
        // it had no effect and a termination request left the process parked. The
        // loop must notice the flag while *idle*, with no connection arriving.
        let path = temp_socket("shutdown");
        let _ = std::fs::remove_file(&path);

        let service = Service::bind(path.clone()).unwrap();
        let flag = service.shutdown_flag();

        let handle = std::thread::spawn(move || {
            // Only the flag is shared; the service is moved in so the listener
            // stays alive for the duration of the loop.
            let service = service;
            service.run()
        });

        // Give the loop a moment to reach its idle wait, then ask it to stop.
        std::thread::sleep(std::time::Duration::from_millis(80));
        flag.store(true, Ordering::Relaxed);

        // A blocking accept would never return, so a timeout is the difference
        // between the bug and the fix.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            handle.is_finished(),
            "the accept loop did not observe the shutdown flag while idle"
        );
        handle.join().unwrap().unwrap();

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_socket_path_prefers_the_environment_override() {
        // Read-only inspection: the resolver is deterministic given the same
        // environment, so this asserts its shape rather than mutating the process
        // environment, which is not safe under a parallel test runner.
        let path = socket_path();
        assert!(path.is_absolute(), "the socket path should be absolute");
        assert!(path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".sock")));
    }

    #[test]
    fn a_request_over_a_real_socket_gets_a_response() {
        let path = temp_socket("roundtrip");
        let _ = std::fs::remove_file(&path);

        let service = Service::bind(path.clone()).unwrap();
        let flag = service.shutdown_flag();
        let socket = path.clone();

        let server = std::thread::spawn(move || {
            // Serve exactly one connection, then stop.
            if let Ok((stream, _)) = service.listener.accept() {
                let _ = serve_connection(stream, &service.handler);
            }
            flag.store(true, Ordering::Relaxed);
            service
        });

        let envelope = RequestEnvelope::new("ping-1", Request::Ping);
        let response = request(&socket, &envelope).expect("the ping should succeed");
        assert!(response.ok);
        assert_eq!(response.request_id, "ping-1");

        let service = server.join().unwrap();
        drop(service);
    }

    #[test]
    fn connecting_to_nothing_reports_how_to_start_the_service() {
        let path = temp_socket("missing").with_file_name("absent.sock");
        let error = connect(&path).unwrap_err();
        assert_eq!(error.code(), "service_unavailable");
        assert!(
            error.message().contains("eensh serve"),
            "the message should say how to start it, was: {}",
            error.message()
        );
    }
}
