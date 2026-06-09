//! The on-disk message model and filename grammar.
//!
//! On-disk PROTOCOL is **2** (the `cur/` signature changed from the bash
//! `cksum(ps -o lstart)` to a microsecond start-time integer — see `darwin`).
//!
//! ```text
//! new/  <epoch>-<id>.json
//! cur/  <epoch>-<id>.<pid>.<sig>.<claimed_at>.json
//! done/ <done_at>-<id>.json
//! ```
//! `<id>` is 16 lowercase hex chars; `<epoch>`/`<claimed_at>`/`<done_at>` are
//! Unix seconds; `<pid>` is the owner pid; `<sig>` is the owner start-time sig.

use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 2;

/// The message body, exactly as stored in the JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub ts: String,
    pub from: String,
    pub msg: String,
}

/// What `--json` emits for read commands: the body plus a `state` discriminator
/// and (when claimed) owner metadata. Field names are stable and lang-independent.
#[derive(Debug, Serialize)]
pub struct MessageView<'a> {
    #[serde(flatten)]
    pub msg: &'a Message,
    pub state: &'static str, // "new" | "claimed"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

impl<'a> MessageView<'a> {
    pub fn new(msg: &'a Message) -> Self {
        Self {
            msg,
            state: "new",
            pid: None,
            claimed_at: None,
            age_s: None,
            stale: None,
        }
    }
    pub fn claimed(msg: &'a Message, c: &CurName, now: i64, stale: bool) -> Self {
        Self {
            msg,
            state: "claimed",
            pid: Some(c.pid),
            claimed_at: Some(c.claimed_at),
            age_s: Some(now - c.claimed_at),
            stale: Some(stale),
        }
    }
}

/// `--json` view of an archived (`done/`) message: the body plus `state:"done"`
/// and the completion epoch.
#[derive(Serialize)]
pub struct ArchivedView<'a> {
    #[serde(flatten)]
    pub msg: &'a Message,
    pub state: &'static str,
    pub done_at: i64,
}

impl<'a> ArchivedView<'a> {
    pub fn new(msg: &'a Message, done_at: i64) -> Self {
        Self {
            msg,
            state: "done",
            done_at,
        }
    }
}

/// `--json` view for `list --all`: any message view (`MessageView` for
/// pending/claimed, `ArchivedView` for done) plus the queue it belongs to.
/// `encoded_key` is the encoded root (matches `status --all`/`status`, for
/// joining); `subkey` is the channel (`null` = the root queue). `T` must not
/// itself carry `encoded_key`/`subkey` fields (no flatten collision).
#[derive(Serialize)]
pub struct AllView<'a, T: serde::Serialize> {
    #[serde(flatten)]
    pub inner: T,
    pub encoded_key: &'a str,
    pub subkey: Option<&'a str>,
}

/// A parsed `new/` (or `done/`) filename: the leading epoch and the id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewName {
    pub epoch: i64,
    pub id: String,
    /// The `<epoch>-<id>` stem (no extension), reused to build the cur/ name.
    pub stem: String,
}

/// A parsed `cur/` filename, carrying the owner identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurName {
    /// The original `<epoch>-<id>` stem (to restore on release/reap).
    pub stem: String,
    pub id: String,
    pub pid: i32,
    pub sig: u64,
    pub claimed_at: i64,
}

/// Parse `<epoch>-<id>` (from a `new/`/`done/` filename, with or without `.json`).
pub fn parse_new(file_name: &str) -> Option<NewName> {
    let stem = file_name.strip_suffix(".json").unwrap_or(file_name);
    let (epoch_s, id) = stem.split_once('-')?;
    let epoch: i64 = epoch_s.parse().ok()?;
    if id.is_empty() {
        return None;
    }
    Some(NewName {
        epoch,
        id: id.to_string(),
        stem: stem.to_string(),
    })
}

/// Parse `<epoch>-<id>.<pid>.<sig>.<claimed_at>` (a `cur/` filename).
pub fn parse_cur(file_name: &str) -> Option<CurName> {
    let core = file_name.strip_suffix(".json").unwrap_or(file_name);
    // Peel the three trailing dot-separated numeric fields, newest-suffix first.
    let (rest, claimed_at) = core.rsplit_once('.')?;
    let (rest, sig) = rest.rsplit_once('.')?;
    let (stem, pid) = rest.rsplit_once('.')?;
    let claimed_at: i64 = claimed_at.parse().ok()?;
    let sig: u64 = sig.parse().ok()?;
    let pid: i32 = pid.parse().ok()?;
    let id = stem.split_once('-').map(|(_, id)| id.to_string())?;
    if id.is_empty() {
        return None;
    }
    Some(CurName {
        stem: stem.to_string(),
        id,
        pid,
        sig,
        claimed_at,
    })
}

/// Build the `cur/` filename for a freshly-claimed message.
pub fn cur_filename(stem: &str, pid: i32, sig: u64, claimed_at: i64) -> String {
    format!("{stem}.{pid}.{sig}.{claimed_at}.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_name() {
        let n = parse_new("1780000000-a1b2c3d4e5f60718.json").unwrap();
        assert_eq!(n.epoch, 1_780_000_000);
        assert_eq!(n.id, "a1b2c3d4e5f60718");
        assert_eq!(n.stem, "1780000000-a1b2c3d4e5f60718");
        assert!(parse_new("garbage").is_none());
    }

    #[test]
    fn cur_filename_roundtrips() {
        let stem = "1780000000-a1b2c3d4e5f60718";
        let fname = cur_filename(stem, 12345, 1_780_000_000_123_456, 1_780_000_050);
        let c = parse_cur(&fname).unwrap();
        assert_eq!(c.stem, stem);
        assert_eq!(c.id, "a1b2c3d4e5f60718");
        assert_eq!(c.pid, 12345);
        assert_eq!(c.sig, 1_780_000_000_123_456);
        assert_eq!(c.claimed_at, 1_780_000_050);
    }

    #[test]
    fn parse_cur_rejects_new_name() {
        assert!(parse_cur("1780000000-a1b2c3d4e5f60718.json").is_none());
    }
}
