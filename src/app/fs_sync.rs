//! Durable file and directory flushes.
//!
//! `File::open(..).sync_all()` is the Unix idiom, but Windows refuses to flush a
//! handle it cannot write through and cannot open a directory as a file at all,
//! so both operations fail there with "access is denied". These helpers keep the
//! Unix durability guarantees and degrade to a no-op where the platform cannot
//! provide one.

use std::fs;
use std::io;
use std::path::Path;

/// Flushes a file's contents to disk.
#[cfg(unix)]
pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    // FlushFileBuffers requires write access, so reopen the file for writing
    // instead of reading.
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Flushes a directory entry so new files and renames survive a crash.
#[cfg(unix)]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

/// Windows cannot open a directory as a file, so there is no handle to flush.
#[cfg(windows)]
pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    let _ = path;
    Ok(())
}
