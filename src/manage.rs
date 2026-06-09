//! Setup/diagnostic verbs: `init` (drop a `.ccq/` root marker), `doctor`,
//! `install` (self-copy the binary to `~/.local/bin`), and `config` (read-only
//! effective settings + their source).

use std::env;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::cli::GlobalOpts;
use crate::error::{CcqError, Result};
use crate::message::PROTOCOL;
use crate::output::Reporter;
use crate::resolve::{self, Resolution};
use crate::{clock, darwin};

/// `config --json`: effective settings + their provenance (one object).
#[derive(Serialize)]
struct ConfigView<'a> {
    lang: &'a str,
    lang_src: &'a str,
    store: String,
    store_src: &'a str,
    history_keep: usize,
    stale_warn: i64,
    root: String,
    via: &'a str,
    queue: String,
    key: Option<&'a str>,
}

// Thin delegates to the single implementations in `resolve` (no duplicated body).
// `init`/`doctor`/`install` want `$HOME`; `init` wants a usage-error on a bad `-d`.
fn home() -> PathBuf {
    resolve::home_dir()
}

fn canon(p: &Path) -> Result<PathBuf> {
    resolve::canon(p)
        .ok_or_else(|| CcqError::usage(format!("ccq: no such directory: {}", p.display())))
}

/// Designate the start dir (`-d` or cwd) as a queue root by creating `.ccq/`.
pub fn init(o: &GlobalOpts, _rep: &Reporter) -> Result<()> {
    let start = match o.dir.as_deref() {
        Some(d) => canon(d)?,
        None => env::current_dir().map_err(|e| CcqError::op(format!("ccq: cwd: {e}")))?,
    };
    let marker = start.join(".ccq");
    if marker.is_dir() {
        println!("already a root: {}", start.display());
        return Ok(());
    }
    // Advisory (non-fatal) notes.
    if start.join(".git").exists() {
        eprintln!("note: this dir is already the git root");
    }
    if let Some(parent) = start.parent()
        && has_marker_above(parent)
    {
        eprintln!("note: nesting under an existing .ccq/ root above this dir");
    }

    std::fs::create_dir_all(&marker)?;
    let label = o.label.clone().unwrap_or_else(|| {
        start
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let body = serde_json::json!({ "label": label, "created": clock::now_iso8601() });
    std::fs::write(marker.join("root.json"), format!("{body}\n"))?;
    println!("initialized root: {} (label {label})", start.display());
    Ok(())
}

fn has_marker_above(start: &Path) -> bool {
    let home = home();
    let mut cur = start;
    loop {
        if cur.join(".ccq").is_dir() {
            return true;
        }
        if cur == home {
            return false;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

pub fn doctor(_rep: &Reporter) -> Result<()> {
    println!("ccq {} (protocol {PROTOCOL})", env!("CARGO_PKG_VERSION"));
    let store = resolve::store_root();
    let projects = count_queues(&store);
    println!("  · queue store: {} ({projects} queues)", store.display());

    let legacy = home().join(".claude/inbox");
    if legacy.is_dir() && count_queues(&legacy) > 0 {
        eprintln!("  ⚠ legacy store found at {}", legacy.display());
        eprintln!(
            "    migrate once with:  mv {}/* {}/",
            legacy.display(),
            store.display()
        );
    }
    if let Ok(exe) = env::current_exe() {
        println!("  · running binary: {}", exe.display());
    }
    let stable = home().join(".local/bin/ccq");
    if stable.exists() {
        println!("  ✓ stable copy: {}", stable.display());
    } else {
        eprintln!(
            "  ⚠ no stable copy (~/.local/bin/ccq) — run `ccq install` for statusline/terminal use"
        );
    }
    Ok(())
}

fn count_queues(store: &Path) -> usize {
    std::fs::read_dir(store)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().join("new").is_dir())
                .count()
        })
        .unwrap_or(0)
}

/// Copy the running binary to `~/.local/bin/ccq` atomically (temp + rename).
pub fn install(_rep: &Reporter) -> Result<()> {
    let src =
        env::current_exe().map_err(|e| CcqError::op(format!("ccq: cannot find self: {e}")))?;
    let bindir = home().join(".local/bin");
    std::fs::create_dir_all(&bindir)?;
    let dest = bindir.join("ccq");
    let tmp = bindir.join(format!("ccq.tmp.{}", std::process::id()));
    std::fs::copy(&src, &tmp)?;
    std::fs::rename(&tmp, &dest)?; // same dir → atomic replace
    println!("installed: {}", dest.display());
    if let Some(path) = env::var_os("PATH") {
        let on_path = env::split_paths(&path).any(|p| p == bindir);
        if !on_path {
            eprintln!(
                "note: {} is not on PATH — add it to your shell rc",
                bindir.display()
            );
        }
    }
    Ok(())
}

/// Read-only effective settings with provenance.
pub fn config(rep: &Reporter, res: &Resolution, dir: &Path, key: Option<&str>) -> Result<()> {
    let lang = match rep.lang {
        crate::output::Lang::Ko => "ko",
        crate::output::Lang::En => "en",
    };
    let lang_src = if env::var_os("CCQ_LANG").is_some() {
        "env"
    } else {
        "default"
    };
    let store = resolve::store_root();
    let store_src = if env::var_os("CCQ_HOME").is_some() {
        "env CCQ_HOME"
    } else if env::var_os("XDG_STATE_HOME").is_some() {
        "env XDG_STATE_HOME"
    } else {
        "default"
    };

    if rep.json {
        crate::output::emit_json(&ConfigView {
            lang,
            lang_src,
            store: store.display().to_string(),
            store_src,
            history_keep: history_keep(),
            stale_warn: stale_warn(),
            root: res.root.display().to_string(),
            via: res.via.as_str(),
            queue: dir.display().to_string(),
            key,
        });
    } else {
        println!("lang={lang} ({lang_src})");
        println!("store={} ({store_src})", store.display());
        println!(
            "history_keep={} ({})",
            history_keep(),
            env_src("CCQ_HISTORY_KEEP")
        );
        println!(
            "stale_warn={} ({})",
            stale_warn(),
            env_src("CCQ_STALE_WARN")
        );
        println!("root={} (via {})", res.root.display(), res.via.as_str());
        println!("queue={}", dir.display());
        println!("key={}", key.unwrap_or("—"));
    }
    Ok(())
}

fn env_src(key: &str) -> &'static str {
    if env::var_os(key).is_some() {
        "env"
    } else {
        "default"
    }
}

pub fn history_keep() -> usize {
    env::var("CCQ_HISTORY_KEEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

pub fn stale_warn() -> i64 {
    env::var("CCQ_STALE_WARN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(43200)
}

/// Default sender label `user@host`.
pub fn default_from() -> String {
    let user = env::var("USER").unwrap_or_else(|_| "user".to_string());
    format!("{user}@{}", darwin::hostname())
}
