//! Output layer. The `Reporter` makes the SPEC invariant unfakeable: **stdout
//! carries data only, stderr carries human chrome**. It also owns language
//! (en/ko, folded in here — only ~a dozen human strings exist; `--json` is never
//! localised). `--json` emits JSON Lines (one object per line).

use std::io::Write;

use serde::Serialize;

use crate::clock;
use crate::message::{Message, MessageView};
use crate::queue::{Counts, DoneEntry, PendingEntry, ProcEntry};
use crate::resolve::{self, Resolution};

/// `status --json`: the resolution + counts + discovered channels (one object).
#[derive(Serialize)]
struct StatusView<'a> {
    root: String,
    via: &'a str,
    marker: Option<String>,
    encoded_key: String,
    queue: &'a str,
    key: Option<&'a str>,
    keys: &'a [String],
    pending: usize,
    processing: usize,
    archived: usize,
}

/// `root --json`: the resolution (one object).
#[derive(Serialize)]
struct RootView<'a> {
    root: String,
    via: &'a str,
    marker: Option<String>,
    encoded_key: String,
    queue: &'a str,
    key: Option<&'a str>,
}

/// `status --all` per-queue object: encoded root + optional channel + counts.
/// `path` is the real root path from `path.txt` (when recorded).
#[derive(Serialize)]
pub struct StatusAllView<'a> {
    pub encoded_key: &'a str,
    pub subkey: Option<&'a str>,
    pub path: Option<&'a str>,
    pub pending: usize,
    pub processing: usize,
    pub archived: usize,
}

/// Serialize a view to a single JSON line on stdout — the `--json` machine
/// contract. The single funnel for every typed view (no ad-hoc `json!` macros).
pub fn emit_json(v: &impl Serialize) {
    println!(
        "{}",
        serde_json::to_string(v).expect("view structs always serialize")
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Ko,
}

impl Lang {
    pub fn from_env_and_flag(flag: Option<&str>) -> Lang {
        if let Some(f) = flag {
            return if f.starts_with("ko") {
                Lang::Ko
            } else {
                Lang::En
            };
        }
        match std::env::var("CCQ_LANG") {
            Ok(v) if v.starts_with("ko") => Lang::Ko,
            _ => Lang::En,
        }
    }
}

pub struct Reporter {
    pub lang: Lang,
    pub json: bool,
}

impl Reporter {
    pub fn new(lang: Lang, json: bool) -> Self {
        Self { lang, json }
    }

    // ── chrome (stderr) ───────────────────────────────────────────────────────

    /// The `📂 <root>` header that precedes human listings (stderr only).
    pub fn header(&self, target: &str) {
        eprintln!("📂 {target}");
    }

    // ── data (stdout) ─────────────────────────────────────────────────────────

    pub fn queued(&self, target: &str, pending: usize) {
        match self.lang {
            Lang::Ko => println!("queued → {target} ({pending}건 대기)"),
            Lang::En => println!("queued → {target} ({pending} pending)"),
        }
    }

    pub fn queue_empty(&self) {
        match self.lang {
            Lang::Ko => println!("(큐 비어있음)"),
            Lang::En => println!("(queue empty)"),
        }
    }

    pub fn ok_line(&self, verb_ok: &str, id: &str) {
        // verb_ok is a pre-localised label ("done"/"released"/"removed"/...).
        println!("{verb_ok}: {id}");
    }

    pub fn id_failed(&self, line: &str) {
        eprintln!("{line}");
    }

    // ── list ──────────────────────────────────────────────────────────────────

    pub fn render_list(&self, pending: &[PendingEntry], processing: &[ProcEntry]) {
        if pending.is_empty() && processing.is_empty() {
            self.queue_empty();
            return;
        }
        let mut out = std::io::stdout().lock();
        for (i, e) in pending.iter().enumerate() {
            let _ = writeln!(
                out,
                "{:>2}. {}  {}  {}  {}",
                i + 1,
                e.name.id,
                clock::display_ts(&e.msg.ts),
                e.msg.from,
                preview(&e.msg.msg, 60),
            );
        }
        if !processing.is_empty() {
            match self.lang {
                Lang::Ko => {
                    let _ = writeln!(out, "-- 처리중 ({}건) --", processing.len());
                }
                Lang::En => {
                    let _ = writeln!(out, "-- processing ({}) --", processing.len());
                }
            }
            for e in processing {
                let age = self.age(e.age_s);
                let warn = if e.stale_warn {
                    self.stale_warn()
                } else {
                    String::new()
                };
                let _ = writeln!(
                    out,
                    "    {}  {}  {}\t(pid {}, {}){}",
                    e.name.id,
                    e.msg.from,
                    preview(&e.msg.msg, 40),
                    e.name.pid,
                    age,
                    warn,
                );
            }
        }
    }

    fn age(&self, secs: i64) -> String {
        if secs >= 3600 {
            match self.lang {
                Lang::Ko => format!("{}시간째", secs / 3600),
                Lang::En => format!("{}h", secs / 3600),
            }
        } else {
            match self.lang {
                Lang::Ko => format!("{}분째", secs / 60),
                Lang::En => format!("{}m", secs / 60),
            }
        }
    }

    fn stale_warn(&self) -> String {
        match self.lang {
            Lang::Ko => "  ⚠ 장기 클레임 — 잊힌 거면: ccq release --force <id>".to_string(),
            Lang::En => {
                "  ⚠ long-running claim — if forgotten: ccq release --force <id>".to_string()
            }
        }
    }

    // ── status / root (with resolution metadata) ───────────────────────────────

    pub fn render_status(
        &self,
        res: &Resolution,
        dir: &str,
        key: Option<&str>,
        keys: &[String],
        c: Counts,
    ) {
        if self.json {
            emit_json(&StatusView {
                root: res.root.display().to_string(),
                via: res.via.as_str(),
                marker: res.marker.as_ref().map(|m| m.display().to_string()),
                encoded_key: resolve::encode_key(&res.root),
                queue: dir,
                key,
                keys,
                pending: c.pending,
                processing: c.processing,
                archived: c.archived,
            });
            return;
        }
        let marker = res
            .marker
            .as_ref()
            .map_or("—".into(), |m| m.display().to_string());
        println!("root:   {}", res.root.display());
        println!("via:    {}", res.via.as_str());
        println!("marker: {marker}");
        println!("queue:  {dir}");
        println!("key:    {}", key.unwrap_or("—"));
        if !keys.is_empty() {
            println!("keys:   {}", keys.join(", "));
        }
        match self.lang {
            Lang::Ko => println!(
                "대기: {}건 | 처리중: {}건 | 완료보관: {}건",
                c.pending, c.processing, c.archived
            ),
            Lang::En => println!(
                "pending: {} | processing: {} | archived: {}",
                c.pending, c.processing, c.archived
            ),
        }
    }

    pub fn render_root(&self, res: &Resolution, dir: &str, key: Option<&str>) {
        if self.json {
            emit_json(&RootView {
                root: res.root.display().to_string(),
                via: res.via.as_str(),
                marker: res.marker.as_ref().map(|m| m.display().to_string()),
                encoded_key: resolve::encode_key(&res.root),
                queue: dir,
                key,
            });
        } else {
            println!("{}", res.root.display());
        }
    }

    // ── json message lines ──────────────────────────────────────────────────────

    pub fn emit_new(&self, msg: &Message) {
        emit_json(&MessageView::new(msg));
    }

    pub fn emit_view(&self, v: &MessageView) {
        emit_json(v);
    }

    // ── history ───────────────────────────────────────────────────────────────

    pub fn render_history(&self, done: &[DoneEntry]) {
        if done.is_empty() {
            match self.lang {
                Lang::Ko => println!("(완료 이력 없음)"),
                Lang::En => println!("(no completed history)"),
            }
            return;
        }
        let mut out = std::io::stdout().lock();
        for e in done.iter().take(20) {
            let _ = writeln!(
                out,
                "{}  {}  {}  {}",
                clock::epoch_to_md_hm(e.done_at),
                e.msg.id,
                e.msg.from,
                preview(&e.msg.msg, 50),
            );
        }
        if done.len() > 20 {
            match self.lang {
                Lang::Ko => println!("    … (최근 20건만 표시, 보관 {}건)", done.len()),
                Lang::En => println!("    … (showing latest 20, {} archived)", done.len()),
            }
        }
    }
}

/// Single-line preview, collapsing newlines and truncating to `max` chars + ellipsis.
fn preview(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() > max {
        let cut: String = one.chars().take(max).collect();
        format!("{cut}…")
    } else {
        one
    }
}

/// JSONL view for a claimed entry (used by `list --json` for processing items).
/// `ProcEntry` already owns its `CurName`, so we borrow it directly.
pub fn claimed_view<'a>(e: &'a ProcEntry, now: i64) -> MessageView<'a> {
    MessageView::claimed(&e.msg, &e.name, now, e.stale_warn)
}
