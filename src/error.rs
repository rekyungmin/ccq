//! Typed errors whose variants map directly to the fixed exit-code contract
//! (SPEC §4.2). `main` turns these into a `process::ExitCode`.

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum CcqError {
    /// Operational/runtime error (store unreadable, publish conflict, …). Exit 1.
    #[error("{0}")]
    Op(String),
    /// Usage error (unknown verb, bad args, bad -d/--root path). Exit 2 — "your
    /// fault, fix the call".
    #[error("{0}")]
    Usage(String),
    /// Partial failure / lost race in a batch (claim/done/release/rm). Exit 3 —
    /// "re-query and retry". Per-id results were already reported, so this carries
    /// no user-facing message.
    #[error("")]
    Partial,
    /// Nothing available (`next` on an empty queue). Exit 4. No message.
    #[error("")]
    Empty,
    /// `wait --timeout` elapsed with nothing. Exit 124 (GNU `timeout(1)`). No message.
    #[error("")]
    WaitTimeout,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl CcqError {
    pub fn op(msg: impl Into<String>) -> Self {
        Self::Op(msg.into())
    }
    pub fn usage(msg: impl Into<String>) -> Self {
        Self::Usage(msg.into())
    }

    /// The process exit code for this error (the agent branches on this).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Op(_) | Self::Io(_) => 1,
            Self::Usage(_) => 2,
            Self::Partial => 3,
            Self::Empty => 4,
            Self::WaitTimeout => 124,
        }
    }

    /// Whether `main` should print this error to stderr. `Partial`/`Empty`/
    /// `WaitTimeout` are silent — they signal purely through the exit code (and any
    /// per-id chrome was already written).
    pub fn should_report(&self) -> bool {
        matches!(self, Self::Op(_) | Self::Usage(_) | Self::Io(_))
    }
}

pub type Result<T> = std::result::Result<T, CcqError>;
