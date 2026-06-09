//! Black-box conformance tests for the CLI contract. Each test isolates state
//! with its own `CCQ_HOME` tempdir and pins `CCQ_OWNER_PID` to the (long-lived)
//! test-runner pid so claim/done ownership is stable.

use std::path::Path;
use std::process::Command as StdCommand;

use assert_cmd::Command;
use tempfile::TempDir;

struct Fix {
    _home: TempDir,
    proj: TempDir,
    home_path: std::path::PathBuf,
}

impl Fix {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let home_path = home.path().to_path_buf();
        Self {
            _home: home,
            proj: TempDir::new().unwrap(),
            home_path,
        }
    }

    /// A `ccq` command targeting this fixture's queue (isolated store + stable owner).
    fn ccq(&self) -> Command {
        let mut c = Command::cargo_bin("ccq").unwrap();
        c.env("CCQ_HOME", &self.home_path)
            .env("CCQ_OWNER_PID", std::process::id().to_string())
            .env_remove("CCQ_LANG")
            .arg("-d")
            .arg(self.proj.path());
        c
    }

    fn proj(&self) -> &Path {
        self.proj.path()
    }

    fn send(&self, body: &str) {
        self.ccq().arg("send").arg(body).assert().success();
    }

    /// First pending id (parsed from `list --json`).
    fn first_id(&self) -> String {
        let out = self.ccq().args(["list", "--json"]).output().unwrap();
        let line = String::from_utf8(out.stdout).unwrap();
        extract_id(line.lines().next().unwrap())
    }
}

fn extract_id(json_line: &str) -> String {
    let after = json_line.split("\"id\":\"").nth(1).unwrap();
    after.split('"').next().unwrap().to_string()
}

#[test]
fn lifecycle_send_list_claim_done() {
    let f = Fix::new();
    f.send("first");
    f.send("second");
    // status counts
    f.ccq()
        .arg("status")
        .assert()
        .success()
        .stdout(predicates::str::contains("pending: 2"));

    let id = f.first_id();
    // claim prints the body as a claimed JSON view
    f.ccq()
        .args(["claim", &id])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"state\":\"claimed\""))
        .stdout(predicates::str::contains(&id));
    f.ccq()
        .arg("status")
        .assert()
        .stdout(predicates::str::contains("processing: 1"));
    // done by the owner succeeds
    f.ccq().args(["done", &id]).assert().success();
    f.ccq()
        .arg("status")
        .assert()
        .stdout(predicates::str::contains("archived: 1"));
}

#[test]
fn next_dequeues_oldest_then_empty() {
    let f = Fix::new();
    f.send("only");
    f.ccq()
        .arg("next")
        .assert()
        .success()
        .stdout(predicates::str::contains("\"state\":\"claimed\""));
    // queue now empty → exit 4
    f.ccq().arg("next").assert().code(4);
}

#[test]
fn exit_codes() {
    let f = Fix::new();
    // claim of a missing id → partial/race (3)
    f.ccq().args(["claim", "deadbeefdeadbeef"]).assert().code(3);
    // unknown verb → usage (2)
    f.ccq().arg("bogusverb").assert().code(2);
    // clear without --yes → usage (2)
    f.ccq().arg("clear").assert().code(2);
    // bad -d → usage (2)
    Command::cargo_bin("ccq")
        .unwrap()
        .env("CCQ_HOME", f.home_path.clone())
        .args(["-d", "/no/such/dir", "status"])
        .assert()
        .code(2);
}

#[test]
fn all_spans_queues_and_rejects_elsewhere() {
    let home = TempDir::new().unwrap();
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let send = |proj: &Path, msg: &str| {
        Command::cargo_bin("ccq")
            .unwrap()
            .env("CCQ_HOME", home.path())
            .env("CCQ_OWNER_PID", std::process::id().to_string())
            .args(["-d", proj.to_str().unwrap(), "send", msg])
            .assert()
            .success();
    };
    send(a.path(), "to-a");
    send(b.path(), "to-b");
    // status --all reports every queue (one JSONL object each)
    let out = Command::cargo_bin("ccq")
        .unwrap()
        .env("CCQ_HOME", home.path())
        .args(["status", "--all", "--json"])
        .output()
        .unwrap();
    let s = String::from_utf8(out.stdout).unwrap();
    assert_eq!(s.lines().count(), 2, "two queues expected in status --all");
    // --all on a non-query verb is a usage error (exit 2), never silently ignored
    Command::cargo_bin("ccq")
        .unwrap()
        .env("CCQ_HOME", home.path())
        .args(["claim", "deadbeefdeadbeef", "--all"])
        .assert()
        .code(2);
}

#[test]
fn corrupt_message_is_op_error_not_empty() {
    let f = Fix::new();
    f.send("good");
    // a file with a valid name but junk content is corruption, not "empty" —
    // list must surface it as exit 1 rather than silently skipping it.
    let new = queue_dir_of(&f).join("new");
    std::fs::write(new.join("1780000000-deadbeefdeadbeef.json"), b"{ not json").unwrap();
    f.ccq().arg("list").assert().code(1);
}

#[test]
fn invalid_state_is_usage_error() {
    let f = Fix::new();
    // an unrecognized --state must be a usage error, not silent empty output
    f.ccq().args(["list", "--state", "bogus"]).assert().code(2);
    // the valid ones still work
    for s in ["pending", "processing", "done", "all"] {
        f.ccq().args(["list", "--state", s]).assert().success();
    }
}

#[test]
fn irrelevant_flag_is_usage_error() {
    let f = Fix::new();
    // a command-specific flag set on a command that ignores it must error, not no-op.
    f.ccq().args(["status", "--timeout", "5"]).assert().code(2);
    f.ccq()
        .args(["send", "--state", "done", "x"])
        .assert()
        .code(2);
    f.ccq().args(["list", "--from", "bob"]).assert().code(2);
    // …but the flag on its rightful command is fine.
    f.ccq().args(["list", "--state", "done"]).assert().success();
    f.send("x");
    f.ccq()
        .args(["send", "--from", "ci", "y"])
        .assert()
        .success();
}

#[test]
fn wait_timeout_exits_124() {
    let f = Fix::new();
    f.ccq().arg("status").assert().success(); // create queue dirs
    f.ccq().args(["wait", "--timeout", "1"]).assert().code(124);
}

#[test]
fn json_is_byte_identical_across_lang() {
    let f = Fix::new();
    f.send("payload");
    let en = f
        .ccq()
        .env("CCQ_LANG", "en")
        .args(["list", "--json"])
        .output()
        .unwrap()
        .stdout;
    let ko = f
        .ccq()
        .env("CCQ_LANG", "ko")
        .args(["list", "--json"])
        .output()
        .unwrap()
        .stdout;
    assert_eq!(en, ko, "--json must not be localized");
}

#[test]
fn list_header_goes_to_stderr_not_stdout() {
    let f = Fix::new();
    f.send("x");
    let out = f.ccq().arg("list").output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("📂"), "header must be on stderr");
    assert!(!stdout.contains("📂"), "stdout must carry data only");
}

#[test]
fn reap_returns_dead_claim_to_pending() {
    let f = Fix::new();
    f.send("orphan");
    let id = f.first_id();
    // Move the message into cur/ owned by a guaranteed-dead pid.
    let dead = spawn_then_dead_pid();
    let qdir = queue_dir_of(&f);
    let new = qdir.join("new");
    let cur = qdir.join("cur");
    let entry = std::fs::read_dir(&new).unwrap().next().unwrap().unwrap();
    let stem = entry
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".json")
        .to_string();
    std::fs::rename(
        entry.path(),
        cur.join(format!("{stem}.{dead}.12345.{}.json", 1_700_000_000)),
    )
    .unwrap();
    // A query triggers reap; the dead-owned claim returns to pending.
    f.ccq()
        .arg("status")
        .assert()
        .stdout(predicates::str::contains("pending: 1"));
    let _ = id;
}

#[test]
fn foreign_live_claim_needs_force() {
    let f = Fix::new();
    f.send("foreign");
    let qdir = queue_dir_of(&f);
    let new = qdir.join("new");
    let cur = qdir.join("cur");
    let entry = std::fs::read_dir(&new).unwrap().next().unwrap().unwrap();
    let stem = entry
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".json")
        .to_string();
    let id = stem.split_once('-').unwrap().1.to_string();
    // pid 1 (launchd) is always alive but isn't our owner, and the sig won't match.
    std::fs::rename(
        entry.path(),
        cur.join(format!("{stem}.1.999999.{}.json", 1_700_000_000)),
    )
    .unwrap();
    f.ccq().args(["done", &id]).assert().code(3); // not owner
    f.ccq().args(["done", "--force", &id]).assert().success();
}

#[test]
fn claim_race_has_exactly_one_winner() {
    let f = Fix::new();
    f.send("contended");
    let id = f.first_id();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let home = f.home_path.clone();
        let proj = f.proj().to_path_buf();
        let id = id.clone();
        handles.push(std::thread::spawn(move || {
            let status = StdCommand::new(assert_cmd::cargo::cargo_bin("ccq"))
                .env("CCQ_HOME", home)
                .env("CCQ_OWNER_PID", std::process::id().to_string())
                .args(["-d", proj.to_str().unwrap(), "claim", &id])
                .output()
                .unwrap()
                .status;
            i32::from(status.success())
        }));
    }
    let winners: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(winners, 1, "exactly one claimer must win the race");
}

#[test]
fn wait_wakes_on_arrival() {
    let f = Fix::new();
    f.ccq().arg("status").assert().success(); // create dirs
    let proj = f.proj().to_path_buf();
    let home = f.home_path.clone();

    let mut child = StdCommand::new(assert_cmd::cargo::cargo_bin("ccq"))
        .env("CCQ_HOME", &home)
        .env("CCQ_OWNER_PID", std::process::id().to_string())
        .args([
            "-d",
            proj.to_str().unwrap(),
            "wait",
            "--timeout",
            "10",
            "--json",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(300));
    f.send("arrived");

    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "wait should exit 0 when a message arrives"
    );
}

// ── sub-key channels ────────────────────────────────────────────────────────

#[test]
fn key_routes_to_nested_queue() {
    let f = Fix::new();
    f.ccq()
        .args(["send", "--key", "review", "channel msg"])
        .assert()
        .success();
    let base = queue_dir_of(&f);
    assert_eq!(
        count_new(&base.join("keys/review")),
        1,
        "message lands in keys/review/new"
    );
    assert_eq!(count_new(&base), 0, "root queue stays empty");
}

#[test]
fn key_isolation_between_root_and_channel() {
    let f = Fix::new();
    f.send("root msg");
    f.ccq()
        .args(["send", "--key", "review", "review msg"])
        .assert()
        .success();

    let root = f.ccq().args(["list", "--json"]).output().unwrap();
    let root = String::from_utf8(root.stdout).unwrap();
    assert!(root.contains("root msg") && !root.contains("review msg"));

    let keyed = f
        .ccq()
        .args(["list", "--key", "review", "--json"])
        .output()
        .unwrap();
    let keyed = String::from_utf8(keyed.stdout).unwrap();
    assert!(keyed.contains("review msg") && !keyed.contains("root msg"));
}

#[test]
fn no_key_escapes_env_ccq_key() {
    let f = Fix::new();
    // With CCQ_KEY exported, a plain send lands in the channel…
    f.ccq()
        .env("CCQ_KEY", "review")
        .args(["send", "to channel"])
        .assert()
        .success();
    // …but --no-key forces the root queue.
    f.ccq()
        .env("CCQ_KEY", "review")
        .args(["send", "--no-key", "to root"])
        .assert()
        .success();
    let base = queue_dir_of(&f);
    assert_eq!(count_new(&base.join("keys/review")), 1);
    assert_eq!(count_new(&base), 1);
}

#[test]
fn invalid_key_is_usage_error() {
    let f = Fix::new();
    for bad in ["Review", "all", "default", "keys", "../x", ""] {
        f.ccq().args(["send", "--key", bad, "x"]).assert().code(2);
    }
}

#[test]
fn key_and_no_key_conflict_is_usage_error() {
    let f = Fix::new();
    f.ccq()
        .args(["send", "--key", "review", "--no-key", "x"])
        .assert()
        .code(2);
}

#[test]
fn all_rejects_key_flags() {
    let f = Fix::new();
    f.ccq()
        .args(["status", "--all", "--key", "review"])
        .assert()
        .code(2);
    f.ccq()
        .args(["status", "--all", "--no-key"])
        .assert()
        .code(2);
}

#[test]
fn invalid_ccq_key_env_is_usage_error() {
    let f = Fix::new();
    f.ccq()
        .env("CCQ_KEY", "BadCaps")
        .args(["list"])
        .assert()
        .code(2);
}

#[test]
fn read_with_key_does_not_create_channel() {
    let f = Fix::new();
    // A read-only command with a (valid) key must NOT mint a channel on disk…
    f.ccq()
        .args(["status", "--key", "ghost"])
        .assert()
        .success();
    f.ccq().args(["list", "--key", "ghost"]).assert().success();
    let base = queue_dir_of(&f);
    assert!(
        !base.join("keys/ghost").exists(),
        "read-only commands must not materialize a channel"
    );
    // …and the root status must not discover any channel.
    let out = f.ccq().args(["status", "--json"]).output().unwrap();
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.contains("\"keys\":[]"), "no channels discovered: {s}");
}

#[test]
fn status_lists_keys_sorted() {
    let f = Fix::new();
    f.ccq()
        .args(["send", "--key", "review", "x"])
        .assert()
        .success();
    f.ccq()
        .args(["send", "--key", "deploy", "y"])
        .assert()
        .success();
    let out = f.ccq().args(["status", "--json"]).output().unwrap();
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(
        s.contains("\"keys\":[\"deploy\",\"review\"]"),
        "status lists sorted channel keys: {s}"
    );
}

#[test]
fn all_descends_into_keys() {
    let f = Fix::new();
    f.send("root msg");
    f.ccq()
        .args(["send", "--key", "review", "review msg"])
        .assert()
        .success();
    let out = f
        .ccq()
        .args(["status", "--all", "--json"])
        .output()
        .unwrap();
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(
        s.contains("\"subkey\":\"review\""),
        "status --all surfaces the sub-queue: {s}"
    );
    assert!(s.contains("\"subkey\":null"), "and the root queue: {s}");
}

// ── worktree → main, by default (--worktree opts out) ───────────────────────

#[test]
fn worktree_redirects_to_main_by_default() {
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree(ws.path());

    // No flag: a worktree resolves to the main working tree, via=git-main.
    let out = ccq_in(home.path(), &wt)
        .args(["root", "--json"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("\"via\":\"git-main\""),
        "worktree resolves to main by default"
    );
    // a plain send from the worktree lands in the MAIN repo's queue.
    ccq_in(home.path(), &wt)
        .args(["send", "for main"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 0);
}

#[test]
fn worktree_opt_out_keeps_own_queue() {
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree(ws.path());

    let out = ccq_in(home.path(), &wt)
        .args(["root", "--json", "--worktree"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("\"via\":\"git\""),
        "--worktree keeps the worktree's own queue (via=git)"
    );
    ccq_in(home.path(), &wt)
        .args(["send", "--worktree", "mine"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 0);
}

#[test]
fn worktree_opt_out_via_env() {
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree(ws.path());
    ccq_in(home.path(), &wt)
        .env("CCQ_WORKTREE", "1")
        .args(["send", "mine"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 0);
}

#[test]
fn worktree_redirect_composes_with_key() {
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree(ws.path());
    // default redirect + sub-key → the main repo's review channel.
    ccq_in(home.path(), &wt)
        .args(["send", "--key", "review", "x"])
        .assert()
        .success();
    assert_eq!(
        count_new(&encoded_queue_dir(home.path(), &main).join("keys/review")),
        1
    );
}

#[test]
fn worktree_redirect_with_relative_gitdir() {
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree_rel(ws.path());
    ccq_in(home.path(), &wt)
        .args(["send", "x"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 0);
}

#[test]
fn submodule_uses_own_queue() {
    // A submodule is a distinct repo (gitdir under modules/) → its own queue,
    // never redirected, no note.
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let sub = fake_submodule(ws.path());
    let out = ccq_in(home.path(), &sub)
        .args(["send", "x"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stderr).unwrap().is_empty(), "no note");
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &sub)), 1);
}

#[test]
fn bare_worktree_uses_own_queue() {
    // A worktree of a bare repo has no main working tree → its own queue, no note.
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let wt = fake_bare_worktree(ws.path());
    let out = ccq_in(home.path(), &wt)
        .args(["send", "x"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stderr).unwrap().is_empty(), "no note");
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 1);
}

#[test]
fn separate_gitdir_uses_own_queue() {
    // A plain repo whose `.git` is a *file* (git --separate-git-dir / symlink)
    // whose gitdir is under neither worktrees/ nor modules/ → own queue, via=git.
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let proj = ws.path().join("proj");
    let realgit = ws.path().join("store/proj.git");
    std::fs::create_dir_all(&realgit).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join(".git"),
        format!("gitdir: {}\n", realgit.display()),
    )
    .unwrap();
    let out = ccq_in(home.path(), &proj)
        .args(["root", "--json"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("\"via\":\"git\"")
    );
    assert!(String::from_utf8(out.stderr).unwrap().is_empty(), "no note");
}

#[test]
fn all_rejects_worktree_and_key_env() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    // env opt-ins are rejected by --all too (not just the flags).
    ccq_in(home.path(), proj.path())
        .env("CCQ_WORKTREE", "1")
        .args(["status", "--all"])
        .assert()
        .code(2);
    ccq_in(home.path(), proj.path())
        .env("CCQ_KEY", "review")
        .args(["status", "--all"])
        .assert()
        .code(2);
}

#[test]
fn all_rejects_root() {
    // --all spans every queue; an exact --root/CCQ_ROOT target contradicts it.
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    ccq_in(home.path(), proj.path())
        .args(["status", "--all", "--root", proj.path().to_str().unwrap()])
        .assert()
        .code(2);
    ccq_in(home.path(), proj.path())
        .env("CCQ_ROOT", proj.path().to_str().unwrap())
        .args(["status", "--all"])
        .assert()
        .code(2);
}

#[test]
fn all_honors_state() {
    // `list --all` honors --state: done is shown only when explicitly requested,
    // never silently dropped, across every queue.
    let f = Fix::new();
    f.send("active one");
    let id = f.first_id();
    f.ccq().args(["claim", &id]).assert().success();
    f.ccq().args(["done", &id]).assert().success(); // now archived
    f.send("still active");

    // default --all = active only → the done message is absent.
    let active = f.ccq().args(["list", "--all", "--json"]).output().unwrap();
    let active = String::from_utf8(active.stdout).unwrap();
    assert!(active.contains("still active") && !active.contains("active one"));

    // --all --state done → the archived message appears, with encoded_key + subkey.
    let done = f
        .ccq()
        .args(["list", "--all", "--state", "done", "--json"])
        .output()
        .unwrap();
    let done = String::from_utf8(done.stdout).unwrap();
    assert!(
        done.contains("active one"),
        "done shown across queues: {done}"
    );
    assert!(done.contains("\"state\":\"done\"") && done.contains("\"encoded_key\""));
}

#[test]
fn help_and_version_ignore_all() {
    // Informational commands are forgiving — `--all` must not turn them into errors.
    let f = Fix::new();
    f.ccq().args(["help", "--all"]).assert().success();
    f.ccq().args(["version", "--all"]).assert().success();
}

#[test]
fn marker_overrides_worktree_redirect() {
    // A `.ccq/` marker inside a worktree wins over the worktree→main redirect.
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let (main, wt) = fake_worktree(ws.path());
    std::fs::create_dir(wt.join(".ccq")).unwrap();
    let out = ccq_in(home.path(), &wt)
        .args(["root", "--json"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("\"via\":\"marker\""),
        "marker overrides the redirect"
    );
    ccq_in(home.path(), &wt)
        .args(["send", "x"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 0);
}

#[test]
fn real_git_worktree_redirects_to_main() {
    // Guard against drift in real Git's `.git`-file/commondir layout (the fake
    // tests can't catch that). Skipped when `git` is unavailable.
    if !git_available() {
        return;
    }
    let home = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let main = ws.path().join("repo");
    std::fs::create_dir(&main).unwrap();
    let git = |args: &[&str], cwd: &Path| {
        let ok = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"], &main);
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
        &main,
    );
    let wt = ws.path().join("wt");
    git(
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feat"],
        &main,
    );

    // Default: a real linked worktree resolves to the main repo's queue.
    ccq_in(home.path(), &wt)
        .args(["send", "from real wt"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &main)), 1);
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 0);
    // …and --worktree keeps its own queue.
    ccq_in(home.path(), &wt)
        .args(["send", "--worktree", "mine"])
        .assert()
        .success();
    assert_eq!(count_new(&encoded_queue_dir(home.path(), &wt)), 1);
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Whether a usable `git` is on PATH (real-Git integration tests skip if not).
fn git_available() -> bool {
    StdCommand::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A `ccq` command with an isolated store, stable owner, and clean channel/worktree
/// env, targeting `dir` via `-d`.
fn ccq_in(home: &Path, dir: &Path) -> Command {
    let mut c = Command::cargo_bin("ccq").unwrap();
    c.env("CCQ_HOME", home)
        .env("CCQ_OWNER_PID", std::process::id().to_string())
        .env_remove("CCQ_LANG")
        .env_remove("CCQ_KEY")
        .env_remove("CCQ_WORKTREE")
        .env_remove("CCQ_ROOT")
        .arg("-d")
        .arg(dir);
    c
}

/// Build a fake git main-repo + linked worktree (no real git): `wt/.git` (file) →
/// `main/.git/worktrees/wt` → `commondir` (`../..`) → `main/.git` (dir).
/// Returns (main_root, worktree_root).
fn fake_worktree(ws: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let main = ws.join("main");
    let wt = ws.join("wt");
    let gitdir = main.join(".git/worktrees/wt");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    (main, wt)
}

/// Like `fake_worktree`, but the `.git` file uses a *relative* gitdir (git may
/// write these), to exercise the relative-path resolution branch.
fn fake_worktree_rel(ws: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let main = ws.join("main");
    let wt = ws.join("wt");
    std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(main.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
    // gitdir relative to the worktree dir (wt): ../main/.git/worktrees/wt
    std::fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    (main, wt)
}

/// A fake submodule: `.git` file whose gitdir is under `/modules/` (not a worktree).
fn fake_submodule(ws: &Path) -> std::path::PathBuf {
    let sub = ws.join("sub");
    let gitdir = ws.join("super/.git/modules/sub");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    sub
}

/// A fake worktree of a *bare* repo: `commondir` resolves to a dir not named `.git`,
/// so there is no main working tree. Returns the worktree root.
fn fake_bare_worktree(ws: &Path) -> std::path::PathBuf {
    let bare = ws.join("repo.git"); // bare repo dir (not ".git")
    let wt = ws.join("wt");
    let gitdir = bare.join("worktrees/wt");
    std::fs::create_dir_all(&gitdir).unwrap();
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
    wt
}

/// The on-disk queue dir for an arbitrary canonicalized path (encode `/`,`.`→`-`).
fn encoded_queue_dir(home: &Path, target: &Path) -> std::path::PathBuf {
    let canon = std::fs::canonicalize(target).unwrap();
    let mut bytes: Vec<u8> = canon.to_string_lossy().bytes().collect();
    for b in &mut bytes {
        if *b == b'/' || *b == b'.' {
            *b = b'-';
        }
    }
    home.join(String::from_utf8(bytes).unwrap())
}

/// Count `.json` messages in a queue dir's `new/` (0 if the dir is absent).
fn count_new(queue_dir: &Path) -> usize {
    std::fs::read_dir(queue_dir.join("new"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .count()
        })
        .unwrap_or(0)
}

/// The on-disk queue dir for the fixture's project (encode `/` and `.` → `-`).
fn queue_dir_of(f: &Fix) -> std::path::PathBuf {
    encoded_queue_dir(&f.home_path, f.proj())
}

/// Spawn a trivial child, wait for it to exit, and return its now-dead pid.
fn spawn_then_dead_pid() -> u32 {
    let child = StdCommand::new("true").spawn().unwrap();
    let pid = child.id();
    let _ = child.wait_with_output();
    pid
}
