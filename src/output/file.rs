//! Atomic file writing and raw stdout writing.
//!
//! An agent tool result is often produced while other processes are reading the
//! previous capture, so encoded bytes are written to a temporary file in the
//! destination directory and then renamed into place. A reader therefore never
//! observes a half-written image.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Write `bytes` to `path`, replacing the file atomically where the platform
/// allows.
pub fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let directory = parent.unwrap_or_else(|| Path::new("."));

    let temporary = temporary_path(directory, path);
    let write_result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(error) = write_result {
        // Best effort cleanup; the original failure is the one that matters.
        let _ = fs::remove_file(&temporary);
        return Err(Error::output_failed(format!(
            "could not write {}: {error}",
            path.display()
        )));
    }

    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(Error::output_failed(format!(
            "could not move the finished image into {}: {error}",
            path.display()
        )));
    }

    Ok(())
}

/// Write `bytes` to standard output.
pub fn write_stdout(bytes: &[u8]) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle
        .write_all(bytes)
        .and_then(|_| handle.flush())
        .map_err(|e| Error::output_failed(format!("could not write to stdout: {e}")))
}

/// Write `text` to standard output with a trailing newline.
pub fn write_stdout_text(text: &str) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle
        .write_all(text.as_bytes())
        .and_then(|_| handle.write_all(b"\n"))
        .and_then(|_| handle.flush())
        .map_err(|e| Error::output_failed(format!("could not write to stdout: {e}")))
}

/// Write a human readable line to standard error.
pub fn write_stderr(message: &str) {
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = writeln!(handle, "{message}");
}

/// Build a sibling path used as the staging file for an atomic write.
///
/// The name includes the process id and a counter so that concurrent `eensh`
/// invocations writing the same destination cannot collide.
fn temporary_path(directory: &Path, destination: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = destination
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("eensh");

    directory.join(format!(
        ".{file_name}.eensh-{}-{sequence}.tmp",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_a_file_atomically() {
        let directory = std::env::temp_dir().join(format!("eensh-test-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("out.bin");

        write_file(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        write_file(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        // No staging files are left behind.
        let leftovers: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left temporary files: {leftovers:?}");

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn reports_a_clear_error_for_an_unwritable_directory() {
        let path = Path::new("/this/directory/does/not/exist/out.png");
        let error = write_file(path, b"data").unwrap_err();
        assert_eq!(error.code(), "output_failed");
    }

    #[test]
    fn temporary_paths_are_unique_and_siblings() {
        let directory = Path::new("/tmp");
        let destination = directory.join("shot.png");
        let a = temporary_path(directory, &destination);
        let b = temporary_path(directory, &destination);
        assert_ne!(a, b);
        assert_eq!(a.parent().unwrap(), directory);
    }
}
