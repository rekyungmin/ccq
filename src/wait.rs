//! Block until a message is pending, then exit (the harness treats process exit
//! as the wake signal). Primary path is event-driven via kqueue `EVFILT_VNODE`
//! on `new/` — instant wake, zero CPU while blocked. Falls back to a poll loop if
//! the watch can't be set up. Either way a bounded timeout maps to exit 124.
//!
//! Correctness: kqueue events are wake-ups, not truth — we always re-`reap` and
//! recount on every wake, and we re-check *after* registering the watch to close
//! the register-after-arrival TOCTOU window.

use std::thread::sleep;
use std::time::Duration;

use crate::cli::GlobalOpts;
use crate::clock;
use crate::darwin::{DirWatcher, Wake};
use crate::error::{CcqError, Result};
use crate::output::Reporter;
use crate::queue::Queue;

pub fn wait(q: &Queue, o: &GlobalOpts, rep: &Reporter) -> Result<()> {
    let interval = o.interval.unwrap_or(2).max(1);
    let deadline = o
        .timeout
        .map(|t| clock::now_epoch().saturating_add(t as i64));

    // Return immediately if work is already present (no TOCTOU, no block).
    if emit_if_ready(q, rep)? {
        return Ok(());
    }

    let new_dir = q.dir.join("new");
    match DirWatcher::new(&new_dir) {
        Ok(watcher) => wait_kqueue(q, rep, &watcher, deadline),
        Err(_) => wait_poll(q, rep, deadline, interval), // unsupported FS / missing dir
    }
}

fn wait_kqueue(
    q: &Queue,
    rep: &Reporter,
    watcher: &DirWatcher,
    deadline: Option<i64>,
) -> Result<()> {
    // Re-check after registering: a file may have landed between the first scan
    // and arming the watch.
    if emit_if_ready(q, rep)? {
        return Ok(());
    }
    loop {
        let timeout = match deadline {
            Some(d) => {
                let remaining = d - clock::now_epoch();
                if remaining <= 0 {
                    return Err(CcqError::WaitTimeout);
                }
                Some(Duration::from_secs(remaining as u64))
            }
            None => None,
        };
        match watcher.wait(timeout) {
            Ok(Wake::Changed) => {
                if emit_if_ready(q, rep)? {
                    return Ok(());
                }
                // Spurious / unrelated change (e.g. a claim moved a file out) — loop.
            }
            Ok(Wake::TimedOut) => return Err(CcqError::WaitTimeout),
            // A signal interrupted the wait — loop, recomputing the deadline so the
            // timeout can't be extended by repeated EINTRs.
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(CcqError::Io(e)),
        }
    }
}

fn wait_poll(q: &Queue, rep: &Reporter, deadline: Option<i64>, interval: u64) -> Result<()> {
    loop {
        if emit_if_ready(q, rep)? {
            return Ok(());
        }
        let nap = match deadline {
            Some(d) => {
                let remaining = d - clock::now_epoch();
                if remaining <= 0 {
                    return Err(CcqError::WaitTimeout);
                }
                interval.min(remaining as u64) // don't oversleep the deadline
            }
            None => interval,
        };
        sleep(Duration::from_secs(nap));
    }
}

/// Reap, then if anything is pending print it (JSONL or human table) and report
/// `true`. The woken session uses this output to claim immediately.
fn emit_if_ready(q: &Queue, rep: &Reporter) -> Result<bool> {
    q.reap()?;
    let pending = q.pending()?;
    if pending.is_empty() {
        return Ok(false);
    }
    if rep.json {
        for e in &pending {
            rep.emit_new(&e.msg);
        }
    } else {
        rep.render_list(&pending, &[]);
    }
    Ok(true)
}
