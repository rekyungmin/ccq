//! Maildir queue semantics. Pure operations over the on-disk layout — they
//! return data and outcomes; the dispatch layer renders them (human via
//! `output::Reporter`, machine via JSONL). Every state transition is an atomic
//! no-clobber move; the loser of any race fails cleanly rather than corrupting.
//!
//! Error policy: a missing queue dir reads as empty, and a file that races away
//! mid-scan (a concurrent claim/reap) is skipped — but a real I/O failure
//! (permission denied, corrupt message) propagates as an operational error
//! (exit 1) rather than masquerading as "empty" or "lost the race".

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CcqError, Result};
use crate::message::{self, CurName, Message, NewName};
use crate::{clock, darwin, fs_atomic, owner};

/// The maildir subdirectories every queue (root or sub-key channel) is made of.
/// Owned here because the maildir layout is `queue`'s domain; `resolve` reuses it
/// when materializing a queue dir rather than re-hardcoding the names.
pub const SUBDIRS: [&str; 4] = ["tmp", "new", "cur", "done"];

pub struct Queue {
    pub dir: PathBuf,
}

pub struct PendingEntry {
    pub name: NewName,
    pub msg: Message,
    pub path: PathBuf,
}

pub struct ProcEntry {
    pub name: CurName,
    pub msg: Message,
    /// True when the claim is older than the stale-warn threshold (⚠ in `list`).
    pub stale_warn: bool,
    pub age_s: i64,
}

pub struct DoneEntry {
    pub done_at: i64,
    pub msg: Message,
}

#[derive(Clone, Copy)]
pub struct Counts {
    pub pending: usize,
    pub processing: usize,
    pub archived: usize,
}

/// Per-id outcome of a batch mutation (the dispatch maps these to i18n messages).
pub enum ClaimOutcome {
    Ok(Box<Message>),
    Failed, // already claimed, missing, or lost the race
}

pub enum FinishOutcome {
    Ok,
    NotProcessing,
    NotOwner,
    Raced,
}

impl Queue {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read_msg(path: &Path) -> Result<Message> {
        let data = std::fs::read(path)?;
        serde_json::from_slice(&data)
            .map_err(|e| CcqError::op(format!("ccq: corrupt message {}: {e}", path.display())))
    }

    // ── Send ────────────────────────────────────────────────────────────────

    /// Enqueue a message; returns the resulting pending count. Retries on the
    /// (astronomically rare) same-second id collision with a fresh id.
    pub fn send(&self, from: &str, body: &str) -> Result<usize> {
        let tmp = self.sub("tmp");
        let new = self.sub("new");
        let epoch = clock::now_epoch();
        for _ in 0..8 {
            let id = darwin::random_id();
            let msg = Message {
                id: id.clone(),
                ts: clock::now_iso8601(),
                from: from.to_string(),
                msg: body.to_string(),
            };
            let json = serde_json::to_vec(&msg)
                .map_err(|e| CcqError::op(format!("ccq: encode failed: {e}")))?;
            let dest = new.join(format!("{epoch}-{id}.json"));
            match fs_atomic::publish(&tmp, &dest, &json) {
                Ok(()) => return self.count("new"),
                Err(e) if fs_atomic::is_exists(&e) => continue, // id collision: retry
                Err(e) => return Err(CcqError::Io(e)),
            }
        }
        Err(CcqError::op("ccq: publish conflict — please retry"))
    }

    // ── Reap (conservative) ───────────────────────────────────────────────────

    /// Return dead-owner claims to pending. Reaps only on definite death
    /// (`kill→ESRCH`) or a confirmed PID-reuse (alive but start-sig differs);
    /// never on an uninspectable sig — that's left for a human (⚠ + release).
    pub fn reap(&self) -> Result<()> {
        let new = self.sub("new");
        for path in json_files(&self.sub("cur"))? {
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(c) = message::parse_cur(fname) else {
                continue; // stray / non-conforming file — not ours
            };
            let dead = if !darwin::pid_alive(c.pid) {
                true
            } else {
                darwin::proc_info(c.pid).is_some_and(|i| i.sig != c.sig)
            };
            if !dead {
                continue;
            }
            let dest = new.join(format!("{}.json", c.stem));
            let _ = fs_atomic::move_excl(&path, &dest); // best-effort; ignore races
        }
        Ok(())
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    pub fn count(&self, sub: &str) -> Result<usize> {
        Ok(json_files(&self.sub(sub))?.len())
    }

    pub fn counts(&self) -> Result<Counts> {
        Ok(Counts {
            pending: self.count("new")?,
            processing: self.count("cur")?,
            archived: self.count("done")?,
        })
    }

    /// Pending entries, oldest first (by epoch then id).
    pub fn pending(&self) -> Result<Vec<PendingEntry>> {
        let mut out = Vec::new();
        for path in json_files(&self.sub("new"))? {
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(name) = message::parse_new(fname) else {
                continue; // stray file
            };
            let Some(msg) = read_or_skip(&path)? else {
                continue; // raced away
            };
            out.push(PendingEntry { name, msg, path });
        }
        out.sort_by(|a, b| {
            a.name
                .epoch
                .cmp(&b.name.epoch)
                .then(a.name.id.cmp(&b.name.id))
        });
        Ok(out)
    }

    /// Processing (claimed) entries, with age + stale-warn flag.
    pub fn processing(&self, stale_secs: i64) -> Result<Vec<ProcEntry>> {
        let now = clock::now_epoch();
        let mut out = Vec::new();
        for path in json_files(&self.sub("cur"))? {
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(name) = message::parse_cur(fname) else {
                continue;
            };
            let Some(msg) = read_or_skip(&path)? else {
                continue;
            };
            let age_s = now - name.claimed_at;
            let stale_warn = age_s > stale_secs;
            out.push(ProcEntry {
                name,
                msg,
                stale_warn,
                age_s,
            });
        }
        out.sort_by_key(|e| e.name.claimed_at);
        Ok(out)
    }

    /// Completed entries, newest first.
    pub fn archived(&self) -> Result<Vec<DoneEntry>> {
        let mut out = Vec::new();
        for path in json_files(&self.sub("done"))? {
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(name) = message::parse_new(fname) else {
                continue;
            };
            let Some(msg) = read_or_skip(&path)? else {
                continue;
            };
            out.push(DoneEntry {
                done_at: name.epoch,
                msg,
            });
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.done_at));
        Ok(out)
    }

    fn find_pending(&self, id: &str) -> Result<Option<PendingEntry>> {
        Ok(self.pending()?.into_iter().find(|e| e.name.id == id))
    }

    fn find_proc(&self, id: &str) -> Result<Option<(CurName, PathBuf)>> {
        for path in json_files(&self.sub("cur"))? {
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(c) = message::parse_cur(fname)
                && c.id == id
            {
                return Ok(Some((c, path)));
            }
        }
        Ok(None)
    }

    // ── Mutations ─────────────────────────────────────────────────────────────

    /// Claim a specific pending id. Atomic: exactly one racer wins. `Ok(Failed)`
    /// means a lost race / missing id; `Err` means a real I/O failure.
    pub fn claim(&self, id: &str, pid: i32, sig: u64, cepoch: i64) -> Result<ClaimOutcome> {
        let Some(entry) = self.find_pending(id)? else {
            return Ok(ClaimOutcome::Failed);
        };
        let cur = self
            .sub("cur")
            .join(message::cur_filename(&entry.name.stem, pid, sig, cepoch));
        match fs_atomic::move_excl(&entry.path, &cur) {
            Ok(()) => Ok(ClaimOutcome::Ok(Box::new(Self::read_msg(&cur)?))),
            Err(e) if is_race(&e) => Ok(ClaimOutcome::Failed),
            Err(e) => Err(CcqError::Io(e)),
        }
    }

    pub fn done(&self, id: &str, force: bool) -> Result<FinishOutcome> {
        let Some((c, path)) = self.find_proc(id)? else {
            return Ok(FinishOutcome::NotProcessing);
        };
        if !force && !owner_matches(&c) {
            return Ok(FinishOutcome::NotOwner);
        }
        let dest = self
            .sub("done")
            .join(format!("{}-{}.json", clock::now_epoch(), c.id));
        match fs_atomic::move_excl(&path, &dest) {
            Ok(()) => Ok(FinishOutcome::Ok),
            Err(e) if is_race(&e) => Ok(FinishOutcome::Raced),
            Err(e) => Err(CcqError::Io(e)),
        }
    }

    pub fn release(&self, id: &str, force: bool) -> Result<FinishOutcome> {
        let Some((c, path)) = self.find_proc(id)? else {
            return Ok(FinishOutcome::NotProcessing);
        };
        if !force && !owner_matches(&c) {
            return Ok(FinishOutcome::NotOwner);
        }
        let dest = self.sub("new").join(format!("{}.json", c.stem));
        match fs_atomic::move_excl(&path, &dest) {
            Ok(()) => Ok(FinishOutcome::Ok),
            Err(e) if is_race(&e) => Ok(FinishOutcome::Raced),
            Err(e) => Err(CcqError::Io(e)),
        }
    }

    /// Delete a pending message. `Ok(false)` = not pending (or raced away);
    /// `Err` = a real removal failure.
    pub fn rm(&self, id: &str) -> Result<bool> {
        let Some(e) = self.find_pending(id)? else {
            return Ok(false);
        };
        match std::fs::remove_file(&e.path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false), // raced
            Err(err) => Err(CcqError::Io(err)),
        }
    }

    /// Drain the pending queue (processing + history kept). Returns removed count.
    pub fn clear(&self) -> Result<usize> {
        let mut n = 0;
        for path in json_files(&self.sub("new"))? {
            match std::fs::remove_file(&path) {
                Ok(()) => n += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {} // raced away
                Err(e) => return Err(CcqError::Io(e)),
            }
        }
        Ok(n)
    }

    /// Trim `done/` to the newest `keep` entries. Best-effort cleanup — a failure
    /// here must not fail the `done` that triggered it.
    pub fn trim_history(&self, keep: usize) {
        let Ok(mut paths) = json_files(&self.sub("done")) else {
            return;
        };
        if paths.len() <= keep {
            return;
        }
        // Oldest first by filename (epoch-prefixed); remove the overflow.
        paths.sort();
        let overflow = paths.len() - keep;
        for path in paths.into_iter().take(overflow) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn owner_matches(c: &CurName) -> bool {
    let op = owner::owner_pid();
    c.pid == op && c.sig == owner::owner_sig(op)
}

/// A failed move is a *race* (expected, recoverable) when the destination already
/// exists or the source vanished; any other errno is an operational failure.
fn is_race(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
    )
}

/// Read a message, returning `Ok(None)` if the file raced away (a concurrent
/// claim/reap moved it) and `Err` on a real read/parse failure (permission, corruption).
fn read_or_skip(path: &Path) -> Result<Option<Message>> {
    match Queue::read_msg(path) {
        Ok(m) => Ok(Some(m)),
        Err(CcqError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// `*.json` paths in a directory. A missing dir is empty; any other `read_dir`
/// error (e.g. permission denied) propagates rather than reading as empty.
fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CcqError::Io(e)),
    };
    let mut out = Vec::new();
    for entry in rd {
        let path = entry.map_err(CcqError::Io)?.path();
        if path.extension().is_some_and(|x| x == "json") {
            out.push(path);
        }
    }
    Ok(out)
}
