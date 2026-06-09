#[cfg(not(target_os = "macos"))]
compile_error!("ccq is macOS-only (it uses proc_pidinfo, kqueue, and renameatx_np)");

mod cli;
mod clock;
mod compat;
mod darwin;
mod error;
mod fs_atomic;
mod manage;
mod message;
mod output;
mod owner;
mod queue;
mod resolve;
mod wait;

use std::ffi::OsString;
use std::io::Read;
use std::process::ExitCode;

use cli::{Command, GlobalOpts, SendInput};
use error::{CcqError, Result};
use message::{AllView, ArchivedView, MessageView};
use output::{Lang, Reporter, StatusAllView};
use queue::{ClaimOutcome, FinishOutcome, Queue};

fn main() -> ExitCode {
    // LLM pipelines pipe us into `head`/`jq`; exit cleanly on a broken pipe
    // instead of panicking on the resulting write error.
    reset_sigpipe();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if e.should_report() {
                eprintln!("{e}");
            }
            ExitCode::from(e.exit_code())
        }
    }
}

fn run() -> Result<()> {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let (argv, warn) = compat::rewrite(raw);
    if let Some(w) = warn {
        eprintln!("{w}");
    }
    let cli = cli::parse(argv)?;
    let o = &cli.opts;
    cli::validate_opts(&cli.cmd, o)?; // reject command-specific flags on commands that ignore them
    let lang = Lang::from_env_and_flag(o.lang.as_deref());
    let rep = Reporter::new(lang, o.json);

    // Informational commands are forgiving — never gated by `--all`/flag validation.
    match cli.cmd {
        Command::Version => {
            if o.build_hash {
                println!(
                    "{} ({})",
                    env!("CARGO_PKG_VERSION"),
                    option_env!("CCQ_BUILD_HASH").unwrap_or("dev")
                );
            } else {
                println!("{}", env!("CARGO_PKG_VERSION"));
            }
            return Ok(());
        }
        Command::Help => {
            print_help(lang);
            return Ok(());
        }
        _ => {}
    }

    // Channel/worktree targeting is per-resolution; `--all` spans every queue, so the
    // two are mutually exclusive. Reject both the flags and their env equivalents —
    // an opt-in the user explicitly set should error, not be silently ignored.
    let worktree_own = o.worktree || env_flag("CCQ_WORKTREE");

    // `--all` operates across every queue (not a single root). Valid only for
    // `list`/`status`; anywhere else it's a usage error (never silently ignored).
    if o.all {
        let rooted =
            o.root.is_some() || std::env::var_os("CCQ_ROOT").is_some_and(|v| !v.is_empty());
        let keyed = o.key.is_some()
            || o.no_key
            || std::env::var_os("CCQ_KEY").is_some_and(|v| !v.is_empty());
        if rooted || keyed || worktree_own {
            return Err(CcqError::usage(
                "ccq: --all spans every queue — it can't combine with --root/--key/--no-key/--worktree",
            ));
        }
        return match cli.cmd {
            Command::Status => do_status_all(&rep),
            Command::List => do_list_all(&rep, o),
            _ => Err(CcqError::usage(
                "ccq: --all is only valid for `list` and `status`",
            )),
        };
    }

    match &cli.cmd {
        Command::Install => return manage::install(&rep),
        Command::Doctor => return manage::doctor(&rep),
        Command::Init => return manage::init(o, &rep),
        _ => {}
    }

    // Everything else resolves a queue root (+ optional sub-key channel). A linked
    // worktree resolves to its main working tree by default; `--worktree` keeps the
    // worktree's own queue. (`--root`/`CCQ_ROOT` force an exact root, bypassing the
    // worktree logic entirely, so `--worktree` is simply moot there.)
    let res = resolve::resolve_root(o.dir.as_deref(), o.root.as_deref(), worktree_own)?;
    let key = resolve_key(o)?;
    // The queue dir as a pure path (no I/O). A channel nests under the root path; it
    // is materialized lazily (only `send` calls ensure_maildir), so a typo'd --key on
    // a read never mints a channel.
    let dir = {
        let base = resolve::queue_dir_path(&res.root);
        match key.as_deref() {
            Some(k) => base.join("keys").join(k),
            None => base,
        }
    };
    let target = res.root.display().to_string();

    // Pure-read commands resolve + display only — they must NOT materialize the store.
    match cli.cmd {
        Command::Root => {
            rep.render_root(&res, &dir.display().to_string(), key.as_deref());
            return Ok(());
        }
        Command::Config => return manage::config(&rep, &res, &dir, key.as_deref()),
        _ => {}
    }

    // Everything else acts on the queue → materialize the root dir + collision guard.
    let base = resolve::queue_dir(&res.root)?;
    let q = Queue::new(dir.clone());

    match cli.cmd {
        Command::Send(input) => {
            let body = read_body(input)?;
            let from = o.from.clone().unwrap_or_else(manage::default_from);
            resolve::ensure_maildir(&dir)?; // materialize the (possibly keyed) maildir on publish
            let n = q.send(&from, &body)?;
            rep.queued(&target, n);
        }
        Command::List => do_list(&q, &rep, o, &target)?,
        Command::Status => {
            q.reap()?;
            let keys = resolve::keys_of(&base);
            rep.render_status(
                &res,
                &dir.display().to_string(),
                key.as_deref(),
                &keys,
                q.counts()?,
            );
        }
        Command::Wait => wait::wait(&q, o, &rep)?,
        Command::Claim(ids) => do_claim(&q, &rep, &ids)?,
        Command::Next => do_next(&q, &rep)?,
        Command::Done(ids) => {
            let r = do_finish(&rep, &ids, "done", |id| q.done(id, o.force));
            q.trim_history(manage::history_keep()); // cap done/ even on partial failure
            r?;
        }
        Command::Release(ids) => do_finish(&rep, &ids, "released", |id| q.release(id, o.force))?,
        Command::Rm(ids) => do_rm(&q, &rep, &ids)?,
        Command::Clear => {
            if !o.yes {
                return Err(CcqError::usage(
                    "ccq: clear drains the pending queue — pass --yes to confirm",
                ));
            }
            let n = q.clear()?;
            println!("cleared pending queue → {target} ({n} removed)");
        }
        // handled above (Root/Config returned early; the rest before resolution)
        Command::Root
        | Command::Config
        | Command::Version
        | Command::Help
        | Command::Install
        | Command::Doctor
        | Command::Init => {}
    }
    Ok(())
}

/// A boolean env flag: set when present and not an explicit off value
/// (case-insensitive: `0`/`false`/`no`/`off`/empty are off).
fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

/// The effective sub-key channel: `--no-key` forces the root queue; otherwise
/// `--key`, then `CCQ_KEY`. The chosen key is validated (usage error on a bad
/// slug) so a misroute is caught before any file is touched.
fn resolve_key(o: &GlobalOpts) -> Result<Option<String>> {
    if o.no_key {
        if o.key.is_some() {
            return Err(CcqError::usage(
                "ccq: --key and --no-key are mutually exclusive",
            ));
        }
        return Ok(None);
    }
    let key = o
        .key
        .clone()
        .or_else(|| std::env::var("CCQ_KEY").ok().filter(|s| !s.is_empty()));
    if let Some(k) = &key {
        resolve::validate_key(k)?;
    }
    Ok(key)
}

/// Human label for a queue ref in `--all` listings: real path (or encoded key)
/// plus a `#subkey` suffix for sub-queues.
fn queue_label(qr: &resolve::QueueRef) -> String {
    let base = qr.path.as_deref().unwrap_or(&qr.key);
    match &qr.subkey {
        Some(sk) => format!("{base}#{sk}"),
        None => base.to_string(),
    }
}

/// `--state` → (want_new, want_cur, want_done). Default (None) = active. Shared by
/// `list` and `list --all`. Bad value → usage error.
fn parse_states(state: Option<&str>) -> Result<(bool, bool, bool)> {
    Ok(match state {
        None => (true, true, false),
        Some("pending") => (true, false, false),
        Some("processing") => (false, true, false),
        Some("done") => (false, false, true),
        Some("all") => (true, true, true),
        Some(other) => {
            return Err(CcqError::usage(format!(
                "ccq: invalid --state '{other}' (pending|processing|done|all)"
            )));
        }
    })
}

fn do_list(q: &Queue, rep: &Reporter, o: &GlobalOpts, target: &str) -> Result<()> {
    q.reap()?;
    let (want_new, want_cur, want_done) = parse_states(o.state.as_deref())?;

    if rep.json {
        let now = clock::now_epoch();
        if want_new {
            for e in q.pending()? {
                rep.emit_new(&e.msg);
            }
        }
        if want_cur {
            for e in q.processing(manage::stale_warn())? {
                rep.emit_view(&output::claimed_view(&e, now));
            }
        }
        if want_done {
            for e in q.archived()? {
                output::emit_json(&ArchivedView::new(&e.msg, e.done_at));
            }
        }
        return Ok(());
    }

    rep.header(target);
    if want_new || want_cur {
        let pending = if want_new { q.pending()? } else { Vec::new() };
        let processing = if want_cur {
            q.processing(manage::stale_warn())?
        } else {
            Vec::new()
        };
        rep.render_list(&pending, &processing);
    }
    if want_done {
        rep.render_history(&q.archived()?);
    }
    Ok(())
}

/// `status --all`: counts for every queue in the store.
fn do_status_all(rep: &Reporter) -> Result<()> {
    for qr in resolve::all_queues() {
        let q = Queue::new(qr.dir.clone());
        q.reap()?;
        let c = q.counts()?;
        if rep.json {
            output::emit_json(&StatusAllView {
                encoded_key: &qr.key,
                subkey: qr.subkey.as_deref(),
                path: qr.path.as_deref(),
                pending: c.pending,
                processing: c.processing,
                archived: c.archived,
            });
        } else {
            let label = queue_label(&qr);
            println!(
                "{label}  pending: {} | processing: {} | archived: {}",
                c.pending, c.processing, c.archived
            );
        }
    }
    Ok(())
}

/// `list --all`: messages across every queue, honoring `--state` (default =
/// active: pending+processing). Queues with nothing in the selected states are
/// skipped. JSON objects carry `encoded_key` + `subkey` so consumers can attribute
/// each message to its queue.
fn do_list_all(rep: &Reporter, o: &GlobalOpts) -> Result<()> {
    let (want_new, want_cur, want_done) = parse_states(o.state.as_deref())?;
    let now = clock::now_epoch();
    for qr in resolve::all_queues() {
        let q = Queue::new(qr.dir.clone());
        q.reap()?;
        let pending = if want_new { q.pending()? } else { Vec::new() };
        let processing = if want_cur {
            q.processing(manage::stale_warn())?
        } else {
            Vec::new()
        };
        let archived = if want_done { q.archived()? } else { Vec::new() };
        if pending.is_empty() && processing.is_empty() && archived.is_empty() {
            continue;
        }
        if rep.json {
            let subkey = qr.subkey.as_deref();
            for e in &pending {
                output::emit_json(&AllView {
                    inner: MessageView::new(&e.msg),
                    encoded_key: &qr.key,
                    subkey,
                });
            }
            for e in &processing {
                output::emit_json(&AllView {
                    inner: output::claimed_view(e, now),
                    encoded_key: &qr.key,
                    subkey,
                });
            }
            for e in &archived {
                output::emit_json(&AllView {
                    inner: ArchivedView::new(&e.msg, e.done_at),
                    encoded_key: &qr.key,
                    subkey,
                });
            }
        } else {
            let mut header_printed = false;
            if !pending.is_empty() || !processing.is_empty() {
                rep.header(&queue_label(&qr));
                header_printed = true;
                rep.render_list(&pending, &processing);
            }
            if !archived.is_empty() {
                if !header_printed {
                    rep.header(&queue_label(&qr));
                }
                rep.render_history(&archived);
            }
        }
    }
    Ok(())
}

fn do_claim(q: &Queue, rep: &Reporter, ids: &[String]) -> Result<()> {
    q.reap()?;
    let pid = owner::owner_pid();
    let sig = owner::owner_sig(pid);
    let cepoch = clock::now_epoch();
    let mut failed = false;
    for id in ids {
        match q.claim(id, pid, sig, cepoch)? {
            ClaimOutcome::Ok(m) => emit_claimed(rep, &m, pid, cepoch),
            ClaimOutcome::Failed => {
                rep.id_failed(&format!("claim-failed: {id} (already claimed or missing)"));
                failed = true;
            }
        }
    }
    if failed {
        Err(CcqError::Partial)
    } else {
        Ok(())
    }
}

fn do_next(q: &Queue, rep: &Reporter) -> Result<()> {
    q.reap()?;
    let pid = owner::owner_pid();
    let sig = owner::owner_sig(pid);
    let cepoch = clock::now_epoch();
    // Try the oldest; on a lost race retry the next, re-reading until truly empty.
    for _round in 0..100 {
        let pending = q.pending()?;
        if pending.is_empty() {
            return Err(CcqError::Empty);
        }
        for e in &pending {
            if let ClaimOutcome::Ok(m) = q.claim(&e.name.id, pid, sig, cepoch)? {
                emit_claimed(rep, &m, pid, cepoch);
                return Ok(());
            }
        }
    }
    Err(CcqError::Empty)
}

fn emit_claimed(rep: &Reporter, m: &message::Message, pid: i32, cepoch: i64) {
    let view = MessageView {
        msg: m,
        state: "claimed",
        pid: Some(pid),
        claimed_at: Some(cepoch),
        age_s: Some(0),
        stale: Some(false),
    };
    rep.emit_view(&view);
}

fn do_finish(
    rep: &Reporter,
    ids: &[String],
    ok_label: &str,
    op: impl Fn(&str) -> Result<FinishOutcome>,
) -> Result<()> {
    let prefix = if ok_label == "released" {
        "release"
    } else {
        ok_label
    };
    let mut failed = false;
    for id in ids {
        match op(id)? {
            FinishOutcome::Ok => rep.ok_line(ok_label, id),
            FinishOutcome::NotProcessing => {
                rep.id_failed(&format!("{prefix}-failed: {id} (not in processing list)"));
                failed = true;
            }
            FinishOutcome::NotOwner => {
                rep.id_failed(&format!(
                    "{prefix}-failed: {id} (claimed by another session — use --force)"
                ));
                failed = true;
            }
            FinishOutcome::Raced => {
                rep.id_failed(&format!(
                    "{prefix}-failed: {id} (raced — re-check queue state)"
                ));
                failed = true;
            }
        }
    }
    if failed {
        Err(CcqError::Partial)
    } else {
        Ok(())
    }
}

fn do_rm(q: &Queue, rep: &Reporter, ids: &[String]) -> Result<()> {
    let mut failed = false;
    for id in ids {
        if q.rm(id)? {
            rep.ok_line("removed", id);
        } else {
            rep.id_failed(&format!("rm-failed: {id} (not in pending list)"));
            failed = true;
        }
    }
    if failed {
        Err(CcqError::Partial)
    } else {
        Ok(())
    }
}

fn read_body(input: SendInput) -> Result<String> {
    match input {
        SendInput::Body(s) => Ok(s),
        SendInput::Stdin => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            let s = s.strip_suffix('\n').map(str::to_string).unwrap_or(s);
            if s.is_empty() {
                return Err(CcqError::usage("ccq: empty message"));
            }
            Ok(s)
        }
    }
}

fn reset_sigpipe() {
    // SAFETY: restoring the default SIGPIPE disposition is a standard, race-free
    // process-global op; Rust sets it to SIG_IGN at startup, which turns broken
    // pipes into EPIPE write errors we'd otherwise unwrap-panic on.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn print_help(lang: Lang) {
    let body = match lang {
        Lang::Ko => HELP_KO,
        Lang::En => HELP_EN,
    };
    println!("{body}");
}

const HELP_EN: &str = "\
ccq — per-project, agent-agnostic message queue

Producer:
  ccq send [MESSAGE]        enqueue (or `-` / piped stdin)
Query:
  ccq list [--state ...]    pending+processing (pending|processing|done|all)
  ccq status                resolved root + counts
  ccq root                  print the resolved queue root
Block:
  ccq wait                  block until a message arrives, then exit
Process:
  ccq claim <id>...         reserve (prints body JSON)
  ccq next                  claim+print the oldest pending
  ccq done <id>...          complete
  ccq release <id>...       return to pending
  ccq rm <id>...            delete a pending message
  ccq clear --yes           drain the pending queue
Setup:
  ccq init [--label N]      mark this dir a queue root (.ccq/)
  ccq config / doctor / install / version

Global: -d <dir> | --root <dir> | --from <s> | --json | --lang en|ko | --force | --all
        --key <slug> | --no-key   sub-key channel within the root (CCQ_KEY)
        --worktree                keep a worktree's own queue (default: a worktree → the main repo; CCQ_WORKTREE)";

const HELP_KO: &str = "\
ccq — 프로젝트별, 에이전트 중립 메시지 큐

보내기:
  ccq send [메시지]         큐에 추가 (`-`/파이프 stdin 가능)
조회:
  ccq list [--state ...]    대기+처리중 (pending|processing|done|all)
  ccq status                해석된 루트 + 카운트
  ccq root                  해석된 큐 루트 출력
블록:
  ccq wait                  메시지 도착까지 블록 후 종료
처리:
  ccq claim <id>...         선점 (본문 JSON 출력)
  ccq next                  가장 오래된 대기 건 claim+출력
  ccq done <id>...          완료
  ccq release <id>...       대기로 반납
  ccq rm <id>...            대기 메시지 삭제
  ccq clear --yes           대기 큐 비우기
설정:
  ccq init [--label N]      이 디렉토리를 큐 루트로 (.ccq/)
  ccq config / doctor / install / version

전역: -d <dir> | --root <dir> | --from <s> | --json | --lang en|ko | --force | --all
      --key <slug> | --no-key   루트 안의 sub-key 채널 (CCQ_KEY)
      --worktree                워크트리 자기 큐 사용 (기본: 워크트리 → 메인 레포; CCQ_WORKTREE)";
