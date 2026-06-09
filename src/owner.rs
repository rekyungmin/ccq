//! Claim-owner identity selection — first-class because getting it wrong makes
//! every claim "born dead" (its owner appears gone immediately) or unfinishable
//! across an agent's per-tool-call shells.
//!
//! Policy: `CCQ_OWNER_PID` (recommended — the harness/skill should export the
//! long-lived session pid) → nearest known long-lived agent ancestor → the
//! session leader (`getsid`) → parent → self. The signature is the owner's
//! process start-time (PID-reuse detector).

use std::env;

use crate::darwin;

/// `comm` prefixes of long-lived agent session processes worth anchoring to.
const AGENT_COMMS: &[&str] = &[
    "claude",
    "codex",
    "cursor",
    "windsurf",
    "aider",
    "cline",
    "antigravity",
    "gemini",
];

/// The pid that owns claims made by this invocation.
pub fn owner_pid() -> i32 {
    if let Ok(v) = env::var("CCQ_OWNER_PID")
        && let Ok(pid) = v.trim().parse::<i32>()
        && pid > 1
    {
        return pid;
    }
    if let Some(pid) = agent_ancestor() {
        return pid;
    }
    let sid = darwin::getsid();
    if sid > 1 {
        return sid;
    }
    let ppid = darwin::getppid();
    if ppid > 1 {
        return ppid;
    }
    std::process::id() as i32
}

/// The owner's start-time signature (0 if the pid can't be inspected).
pub fn owner_sig(pid: i32) -> u64 {
    darwin::proc_info(pid).map_or(0, |i| i.sig)
}

fn agent_ancestor() -> Option<i32> {
    let mut pid = std::process::id() as i32;
    for _ in 0..15 {
        if pid <= 1 {
            break;
        }
        if let Some(comm) = darwin::proc_comm(pid) {
            let base = comm.rsplit('/').next().unwrap_or(&comm);
            if AGENT_COMMS.iter().any(|a| base.starts_with(a)) {
                return Some(pid);
            }
        }
        match darwin::proc_info(pid) {
            Some(info) => pid = info.ppid,
            None => break,
        }
    }
    None
}
