//! Atomic filesystem primitives. Publishing and every state transition is an
//! atomic, no-clobber move (`renameatx_np(RENAME_EXCL)`), with a `link`+`unlink`
//! fallback for the rare volume without `VOL_CAP_INT_RENAME_EXCL`. No `fsync`:
//! race-correctness comes from the atomic rename, not from durability (a message
//! can be lost on hard power-loss, never observed partially-written).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::darwin;

/// True if the error is "destination already exists" (lost a publish/claim race).
pub fn is_exists(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(libc::EEXIST)
}

/// Atomic no-clobber move `from`→`to`. Errors with `AlreadyExists` if `to` exists.
pub fn move_excl(from: &Path, to: &Path) -> io::Result<()> {
    match darwin::rename_excl(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => {
            // Volume lacks RENAME_EXCL: fall back to the classic atomic-create
            // maildir trick — hard_link fails with EEXIST if `to` exists.
            fs::hard_link(from, to)?;
            fs::remove_file(from)
        }
        Err(e) => Err(normalize_exists(e)),
    }
}

/// Write `contents` to a unique temp file under `tmp_dir`, then atomically
/// publish it to `dest` (no-clobber). On a destination collision the caller
/// should retry with a fresh id. Leaves no temp behind on success.
pub fn publish(tmp_dir: &Path, dest: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = unique_temp(tmp_dir);
    {
        // O_CREAT|O_EXCL: never clobber a stray temp; 0600.
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents)?;
    }
    match move_excl(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp); // don't leak the temp on conflict
            Err(normalize_exists(e))
        }
    }
}

fn unique_temp(tmp_dir: &Path) -> PathBuf {
    tmp_dir.join(format!(
        "{}.{}.tmp",
        std::process::id(),
        darwin::random_id()
    ))
}

fn normalize_exists(e: io::Error) -> io::Error {
    if e.raw_os_error() == Some(libc::EEXIST) {
        io::Error::new(io::ErrorKind::AlreadyExists, "destination exists")
    } else {
        e
    }
}
