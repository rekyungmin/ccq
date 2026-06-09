//! Queue-root resolution + storage location + the encoding collision guard.
//!
//! A *starting directory* (`-d`, else cwd) resolves to a *queue root* by walking
//! up (precedence: `--root`/`CCQ_ROOT` exact → nearest `.ccq/` marker, ceiling at
//! `$HOME` → linked-worktree → main working tree by default (`--worktree` opts
//! out) → enclosing `.git` → the start dir). The root is canonicalised and
//! encoded (`/`,`.`→`-`) into a per-queue directory under the agent-neutral store
//! `$CCQ_HOME` (default `$XDG_STATE_HOME/ccq` → `~/.local/state/ccq`).

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::error::{CcqError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Flag,
    Env,
    Marker,
    Git,
    /// A linked worktree resolved to its main working tree (default; `--worktree` opts out).
    MainWorktree,
    LaunchDir,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Flag => "flag",
            Via::Env => "env",
            Via::Marker => "marker",
            Via::Git => "git",
            Via::MainWorktree => "git-main",
            Via::LaunchDir => "launchdir",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Resolution {
    pub root: PathBuf,
    pub via: Via,
    pub marker: Option<PathBuf>,
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The agent-neutral queue store root (holds one dir per queue).
pub fn store_root() -> PathBuf {
    if let Some(h) = env::var_os("CCQ_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(x) = env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(x).join("ccq");
    }
    home_dir().join(".local/state/ccq")
}

/// Physical absolute path (resolves symlinks; `/tmp`→`/private/tmp`). Like `pwd -P`.
pub fn canon(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// Encode a canonical root path into the queue-dir name: `/` and `.` → `-`.
/// Operates on raw bytes (macOS paths aren't guaranteed UTF-8); the lossy String
/// is safe because the `path.txt` collision guard catches any aliasing.
pub fn encode_key(root: &Path) -> String {
    let mut bytes = root.as_os_str().as_bytes().to_vec();
    for b in &mut bytes {
        if *b == b'/' || *b == b'.' {
            *b = b'-';
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn find_marker(start: &Path) -> Option<PathBuf> {
    let home = home_dir();
    let mut cur = start;
    loop {
        // Ceiling: a marker exactly at $HOME is honored only when we *start* there.
        // Otherwise a stray ~/.ccq would capture every repo under home.
        if cur == home {
            return (cur == start && cur.join(".ccq").is_dir()).then(|| cur.to_path_buf());
        }
        if cur.join(".ccq").is_dir() {
            return Some(cur.to_path_buf());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start;
    loop {
        // `.git` is a dir for a normal repo, a file for a worktree/submodule.
        if cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return None,
        }
    }
}

/// Resolve the *main* working tree from a (possibly worktree) start dir, purely by
/// reading git's on-disk pointers (no fork): worktree `.git` file → `gitdir:` →
/// `<gitdir>/commondir` → `<main>/.git` → its parent. Returns `None` for anything
/// that is *not* a linked worktree with a real main working tree — a plain repo,
/// an indirect `.git` (`--separate-git-dir`/symlink), a submodule (`modules/`), a
/// bare main (no working tree), or a malformed `.git`/`commondir` — in which case
/// the caller just uses the normal git root.
fn find_main_worktree(start: &Path) -> Option<PathBuf> {
    let git_root = find_git_root(start)?;
    let dotgit = git_root.join(".git");
    // A directory `.git` is a plain repo / the main itself — nothing to redirect.
    if std::fs::symlink_metadata(&dotgit).ok()?.is_dir() {
        return None;
    }
    let content = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir_raw = content
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("gitdir:"))?;
    let gitdir = resolve_relative(&git_root, gitdir_raw.trim());
    // Only a *linked worktree* redirects. Discriminate by the admin-dir name (the
    // gitdir's parent) — structural, not a raw substring, so it is robust to a repo
    // living under a path that merely contains "worktrees", and to relative gitdirs:
    //   <common>/worktrees/<id>  → linked worktree (redirect)
    //   <super>/.git/modules/<n> → submodule (distinct repo — its own queue)
    //   anything else            → indirect `.git` (separate-git-dir/symlink)
    if gitdir.parent().and_then(|p| p.file_name()) != Some(std::ffi::OsStr::new("worktrees")) {
        return None;
    }
    // `commondir` is relative to the gitdir; resolve + canonicalize → main `.git`.
    let common_raw = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = canon(&resolve_relative(&gitdir, common_raw.trim()))?;
    // A real main working tree exists iff the common dir is a `.git` inside it.
    if common.file_name() == Some(std::ffi::OsStr::new(".git")) {
        let parent = common.parent()?;
        if parent.join(".git").is_dir() {
            return Some(parent.to_path_buf());
        }
    }
    None // bare main: commondir is the bare repo, no working tree
}

/// Join `rel` onto `base` when relative; take it as-is when absolute.
fn resolve_relative(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Resolve the queue root from the optional `-d` start dir and `--root` exact dir.
/// **By default a linked worktree resolves to its main working tree** (worktrees
/// are transparent — the project's queue is shared across checkouts; use a sub-key
/// for intentional lanes). `worktree_own` (`--worktree`/`CCQ_WORKTREE`) opts out
/// and keeps the worktree's own queue. An explicit `--root`/`CCQ_ROOT` and a
/// `.ccq/` marker both still win over the redirect.
pub fn resolve_root(
    dir: Option<&Path>,
    root: Option<&Path>,
    worktree_own: bool,
) -> Result<Resolution> {
    if let Some(r) = root {
        let c = canon(r)
            .ok_or_else(|| CcqError::usage(format!("ccq: no such directory: {}", r.display())))?;
        return Ok(Resolution {
            root: c,
            via: Via::Flag,
            marker: None,
        });
    }
    if let Some(rv) = env::var_os("CCQ_ROOT").filter(|s| !s.is_empty()) {
        let r = PathBuf::from(rv);
        let c = canon(&r).ok_or_else(|| {
            CcqError::usage(format!("ccq: CCQ_ROOT: no such directory: {}", r.display()))
        })?;
        return Ok(Resolution {
            root: c,
            via: Via::Env,
            marker: None,
        });
    }

    let start_raw = match dir {
        Some(d) => d.to_path_buf(),
        None => env::current_dir()
            .map_err(|e| CcqError::usage(format!("ccq: cannot resolve cwd: {e}")))?,
    };
    let start = canon(&start_raw).ok_or_else(|| {
        CcqError::usage(format!("ccq: no such directory: {}", start_raw.display()))
    })?;

    // A `.ccq/` marker is a deliberate "this dir owns its queue" — it overrides the
    // worktree→main redirect.
    if let Some(dir) = find_marker(&start) {
        let marker = dir.join(".ccq");
        return Ok(Resolution {
            root: dir,
            via: Via::Marker,
            marker: Some(marker),
        });
    }

    // Default: a linked worktree resolves to its main working tree. `--worktree`
    // opts out. After redirecting, the main root is used directly — no second
    // marker walk (it must not escape toward $HOME).
    if !worktree_own && let Some(main) = find_main_worktree(&start) {
        return Ok(Resolution {
            root: main,
            via: Via::MainWorktree,
            marker: None,
        });
    }

    if let Some(dir) = find_git_root(&start) {
        return Ok(Resolution {
            root: dir,
            via: Via::Git,
            marker: None,
        });
    }
    Ok(Resolution {
        root: start,
        via: Via::LaunchDir,
        marker: None,
    })
}

/// The on-disk queue dir for a root — pure path computation, **no I/O** (no
/// creation, no collision check). For read-only commands (`root`/`config`) that
/// only display the path and must not materialize the store.
pub fn queue_dir_path(root: &Path) -> PathBuf {
    store_root().join(encode_key(root))
}

/// Resolve (and lazily create) the on-disk queue directory for a root, enforcing
/// the collision guard. Returns the queue dir (containing tmp/new/cur/done).
pub fn queue_dir(root: &Path) -> Result<PathBuf> {
    let dir = queue_dir_path(root);
    let pathtxt = dir.join("path.txt");
    let root_bytes = root.as_os_str().as_bytes();

    match std::fs::read(&pathtxt) {
        Ok(stored) => {
            let stored = stored.strip_suffix(b"\n").unwrap_or(&stored);
            if stored != root_bytes {
                return Err(CcqError::op(format!(
                    "ccq: queue key collision — {} encodes to an existing queue for {}",
                    root.display(),
                    String::from_utf8_lossy(stored)
                )));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if queue_nonempty(&dir) {
                return Err(CcqError::op(format!(
                    "ccq: queue at {} has messages but no path.txt — run `ccq doctor`",
                    dir.display()
                )));
            }
        }
        Err(e) => return Err(CcqError::Io(e)),
    }

    ensure_maildir(&dir)?;
    if !pathtxt.exists() {
        let mut data = root_bytes.to_vec();
        data.push(b'\n');
        // create_new closes the collision-guard TOCTOU: if a racing process wrote
        // path.txt first, re-read and compare rather than clobber.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pathtxt)
        {
            Ok(mut f) => f.write_all(&data)?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stored = std::fs::read(&pathtxt)?;
                let stored = stored.strip_suffix(b"\n").unwrap_or(&stored);
                if stored != root_bytes {
                    return Err(CcqError::op(format!(
                        "ccq: queue key collision — {} encodes to an existing queue for {}",
                        root.display(),
                        String::from_utf8_lossy(stored)
                    )));
                }
            }
            Err(e) => return Err(CcqError::Io(e)),
        }
    }
    Ok(dir)
}

/// Reserved sub-key slugs (`default`/`all`/`keys`) that would collide with the
/// layout or the escape-hatch semantics. `.`/`..` are already rejected by the
/// leading-char rule below.
const RESERVED_KEYS: &[&str] = &["default", "all", "keys"];

/// Validate a sub-key slug: `^[a-z0-9][a-z0-9._-]{0,63}$`, reserved words barred.
/// Lowercase-only avoids aliasing on case-insensitive filesystems (macOS APFS),
/// where a per-key dir has no `path.txt` collision guard of its own.
pub fn validate_key(key: &str) -> Result<()> {
    let bytes = key.as_bytes();
    let shape_ok = (1..=64).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
        && bytes
            .iter()
            .all(|&b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
    if !shape_ok {
        return Err(CcqError::usage(format!(
            "ccq: invalid --key '{key}' (1–64 chars of [a-z0-9._-], must start with [a-z0-9])"
        )));
    }
    if RESERVED_KEYS.contains(&key) {
        return Err(CcqError::usage(format!("ccq: --key '{key}' is reserved")));
    }
    Ok(())
}

/// Create the maildir subdirs (`tmp/new/cur/done`) for a queue dir if absent.
/// Channels are materialized **lazily** — only a producer (`send`) calls this, so
/// a typo'd `--key` on a read-only command never mints a spurious channel. The
/// root maildir is still created eagerly by `queue_dir` (with its `path.txt`
/// guard); this is idempotent for the root and the first-write step for a channel.
pub fn ensure_maildir(dir: &Path) -> Result<()> {
    for sub in crate::queue::SUBDIRS {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    Ok(())
}

/// Sub-key slugs under a root queue dir that have a `new/` subdir, sorted. Used
/// for discovery (`status`/`--all`) — one level only, never recursive. Slugs that
/// fail `validate_key` (e.g. a hand-made `keys/Review`) are skipped so machine
/// output only ever names CLI-creatable channels.
pub fn keys_of(root_queue_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root_queue_dir.join("keys")) else {
        return out;
    };
    for e in rd.flatten() {
        if !e.path().join("new").is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if validate_key(&name).is_ok() {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// A discovered queue in the store (for `--all` cross-queue commands). A keyed
/// sub-queue carries the same `key`/`path` as its root plus a `subkey`.
pub struct QueueRef {
    pub dir: PathBuf,
    pub key: String,
    /// The real root path from `path.txt`, if recorded.
    pub path: Option<String>,
    /// The sub-key slug, when this ref is a `keys/<slug>/` sub-queue.
    pub subkey: Option<String>,
}

/// The real root path recorded in a queue dir's `path.txt`, if any.
fn read_path_txt(dir: &Path) -> Option<String> {
    std::fs::read(dir.join("path.txt"))
        .ok()
        .map(|b| String::from_utf8_lossy(b.strip_suffix(b"\n").unwrap_or(&b)).into_owned())
}

/// Every queue in the store — each root (dir with a `new/` subdir) plus its
/// `keys/<slug>/` sub-queues (one level only). Sorted by (root key, subkey), so
/// a root sorts before its sub-queues.
pub fn all_queues() -> Vec<QueueRef> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(store_root()) else {
        return out;
    };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.join("new").is_dir() {
            continue;
        }
        let key = e.file_name().to_string_lossy().into_owned();
        let path = read_path_txt(&dir);
        for slug in keys_of(&dir) {
            out.push(QueueRef {
                dir: dir.join("keys").join(&slug),
                key: key.clone(),
                path: path.clone(),
                subkey: Some(slug),
            });
        }
        out.push(QueueRef {
            dir,
            key,
            path,
            subkey: None,
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key).then(a.subkey.cmp(&b.subkey)));
    out
}

fn queue_nonempty(dir: &Path) -> bool {
    if ["new", "cur", "done"]
        .iter()
        .any(|s| has_json(&dir.join(s)))
    {
        return true;
    }
    // Channels inherit the root's identity, so messages in any keys/<slug>/ must
    // also block re-claiming this store dir when path.txt was lost (§6 collision).
    std::fs::read_dir(dir.join("keys"))
        .map(|rd| {
            rd.flatten().any(|e| {
                ["new", "cur", "done"]
                    .iter()
                    .any(|s| has_json(&e.path().join(s)))
            })
        })
        .unwrap_or(false)
}

fn has_json(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_is_lossless_for_ascii_paths() {
        assert_eq!(
            encode_key(Path::new("/Users/x/code/cortex")),
            "-Users-x-code-cortex"
        );
        // both `/` and `.` map to `-` (the lossy case the collision guard covers)
        assert_eq!(encode_key(Path::new("/a/b.c")), "-a-b-c");
        assert_eq!(encode_key(Path::new("/a/b-c")), "-a-b-c");
    }
}
