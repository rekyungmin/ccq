//! The macOS syscall island. Every `unsafe` block lives here behind a safe
//! wrapper with a `// SAFETY:` note. Replaces the bash `ps`/`lsof`/`cksum`/
//! `uuidgen` forks with single in-process syscalls (all exposed by `libc`).

use std::ffi::{CString, c_void};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::time::Duration;

/// Process identity: start-time signature (PID-reuse detector) + parent pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: i32,
    /// `start_tv_sec * 1_000_000 + start_tv_usec` — monotone, fork-free, finer
    /// than the bash `cksum(ps -o lstart)` second-granularity signature.
    pub sig: u64,
}

/// 16 hex chars (64 bits) of CSPRNG — the message id. `arc4random_buf` is the
/// macOS-native, non-blocking CSPRNG (no `getrandom` crate, no `/dev/urandom` open).
pub fn random_id() -> String {
    let mut buf = [0u8; 8];
    // SAFETY: arc4random_buf writes exactly buf.len() bytes into a valid, owned,
    // properly-aligned buffer; it never fails and reads no input.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr() as *mut c_void, buf.len()) };
    let mut s = String::with_capacity(16);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Look up a process's start-time signature and ppid in one syscall.
/// Returns `None` if the pid does not exist (or info is inaccessible).
pub fn proc_info(pid: i32) -> Option<ProcInfo> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: PROC_PIDTBSDINFO fills a proc_bsdinfo; we pass a pointer to a
    // zeroed, correctly-sized, owned struct and the matching size. Return value
    // is the bytes written; a short write means "no such pid / not permitted".
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if n == size {
        Some(ProcInfo {
            ppid: info.pbi_ppid as i32,
            sig: info
                .pbi_start_tvsec
                .wrapping_mul(1_000_000)
                .wrapping_add(info.pbi_start_tvusec),
        })
    } else {
        None
    }
}

/// The process command name (`pbi_comm`, ≤16 chars) — used to recognise a
/// long-lived agent ancestor (claude/codex/…) when selecting the claim owner.
pub fn proc_comm(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: identical contract to proc_info above.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if n != size {
        return None;
    }
    // pbi_comm is a NUL-padded fixed array of c_char.
    let bytes: Vec<u8> = info
        .pbi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Liveness via `kill(pid, 0)`: `ESRCH` ⇒ dead, `0`/`EPERM` ⇒ alive. A
/// permission error means the process exists but we may not signal it — that is
/// still *alive*, and must not be reaped.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill with signal 0 performs only an existence/permission check; it
    // delivers no signal and mutates nothing.
    let r = unsafe { libc::kill(pid, 0) };
    if r == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Atomic, no-clobber rename via `renameatx_np(RENAME_EXCL)` — a single syscall
/// that moves `from`→`to` and fails with `EEXIST` if `to` already exists. The
/// publish/claim/done/release primitive; ccq never overwrites.
pub fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let cfrom = cpath(from)?;
    let cto = cpath(to)?;
    // SAFETY: both CStrings outlive the call; AT_FDCWD resolves the relative-or-
    // absolute paths against the cwd; RENAME_EXCL makes it fail rather than clobber.
    let r = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            cfrom.as_ptr(),
            libc::AT_FDCWD,
            cto.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Outcome of blocking on a directory watch.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    /// The directory changed (a file may have been created/renamed/removed).
    Changed,
    /// The watch timeout elapsed with no change.
    TimedOut,
}

/// A kqueue watch on a directory's vnode. `wait` blocks with zero CPU until the
/// directory's contents change (a file linked/renamed into it) or a timeout.
/// fds are `OwnedFd` (closed on drop). Used for the event-driven `wait` fast path.
pub struct DirWatcher {
    kq: OwnedFd,
    _dir: OwnedFd, // kept open: the watch is on this vnode
}

impl DirWatcher {
    pub fn new(dir: &Path) -> io::Result<Self> {
        let cdir = cpath(dir)?;
        // SAFETY: open with a valid CString path; O_RDONLY|O_CLOEXEC is a plain
        // directory open. We take ownership of the returned fd.
        let dfd = unsafe { libc::open(cdir.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if dfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let dir_fd = unsafe { OwnedFd::from_raw_fd(dfd) };

        // SAFETY: kqueue() takes no args and returns a new fd or -1.
        let kqfd = unsafe { libc::kqueue() };
        if kqfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let kq = unsafe { OwnedFd::from_raw_fd(kqfd) };

        let change = libc::kevent {
            ident: dir_fd.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
            data: 0,
            udata: ptr::null_mut(),
        };
        // SAFETY: register one change, request no events (nevents=0). Pointers are
        // valid for the call; kq/dir_fd outlive it.
        let rc =
            unsafe { libc::kevent(kq.as_raw_fd(), &change, 1, ptr::null_mut(), 0, ptr::null()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { kq, _dir: dir_fd })
    }

    /// Block until the directory changes or `timeout` elapses (`None` = forever).
    /// `EINTR` is surfaced to the caller, which recomputes the deadline before
    /// re-waiting — so repeated signals can't silently extend `--timeout`.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<Wake> {
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: i64::from(d.subsec_nanos()) as _,
        });
        let tsp = ts
            .as_ref()
            .map_or(ptr::null(), |t| t as *const libc::timespec);
        let mut ev: libc::kevent = unsafe { mem::zeroed() };
        // SAFETY: request up to one event into a valid, owned kevent; timeout
        // pointer is null (block) or points to our live timespec.
        let n = unsafe { libc::kevent(self.kq.as_raw_fd(), ptr::null(), 0, &mut ev, 1, tsp) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if n == 0 {
            Wake::TimedOut
        } else {
            Wake::Changed
        })
    }
}

/// Session id (leader pid) of the calling process — a stable owner fallback that
/// survives the short-lived shells an agent spawns per tool call.
pub fn getsid() -> i32 {
    // SAFETY: getsid(0) queries the caller's own session; no pointers, no mutation.
    unsafe { libc::getsid(0) }
}

/// Parent pid of the calling process.
pub fn getppid() -> i32 {
    // SAFETY: argless query of the caller's own ppid.
    unsafe { libc::getppid() }
}

/// Short hostname (the part before the first `.`), for the default `user@host`
/// sender label. Replaces the `hostname -s` fork.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes up to len bytes into our owned buffer and
    // NUL-terminates when there's room.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let full = String::from_utf8_lossy(&buf[..end]);
    full.split('.').next().unwrap_or("localhost").to_string()
}
