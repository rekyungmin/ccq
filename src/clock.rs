//! Time helpers via `jiff` — local-zone formatting with a numeric offset, and
//! epoch arithmetic. Replaces the bash `date`/`strflocaltime` forks.

use jiff::{Timestamp, Zoned, tz::TimeZone};

/// Current wall-clock as ISO-8601 with a numeric offset, e.g. `2026-06-09T15:55:00+0900`.
/// This is the `ts` field stored in every message body (kept across `--lang`).
pub fn now_iso8601() -> String {
    Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%z").to_string()
}

/// Current Unix time in whole seconds (filename epochs).
pub fn now_epoch() -> i64 {
    Timestamp::now().as_second()
}

/// Format a filename epoch as local `MM-DD HH:MM` (history/log display).
pub fn epoch_to_md_hm(epoch: i64) -> String {
    match Timestamp::from_second(epoch) {
        Ok(ts) => ts
            .to_zoned(TimeZone::system())
            .strftime("%m-%d %H:%M")
            .to_string(),
        Err(_) => "?".to_string(),
    }
}

/// Strip the trailing numeric TZ offset and replace `T` with a space, for the
/// human `list` rendering (the offset must never leak into the table — there is a
/// regression test that checks this).
pub fn display_ts(ts: &str) -> String {
    let body = ts
        .rsplit_once(['+', '-'])
        .map(|(head, _off)| head)
        .filter(|head| head.len() >= 19) // only strip a real offset, not a date dash
        .unwrap_or(ts);
    body.replacen('T', " ", 1)
}
