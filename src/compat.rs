//! One-release legacy-syntax argv compatibility rewrite (removed before 1.0). Runs
//! before `cli::parse`. Translates the legacy bash CLI's flag-verbs and command
//! shapes to the flat-verb grammar and emits a single stderr deprecation note. clap
//! could not express "a global flag that is also a legacy command", which is one
//! reason the parser is hand-rolled.

use std::ffi::OsString;

const DEP: &str = "ccq: legacy syntax is deprecated — see `ccq help` (removed before 1.0)";

const VERBS: &[&str] = &[
    "send", "list", "ls", "status", "root", "wait", "claim", "next", "done", "release", "rm",
    "remove", "clear", "init", "config", "install", "doctor", "version", "help",
];

/// Rewrite legacy argv to the current grammar; returns `(argv, Some(warning))` if anything changed.
pub fn rewrite(argv: Vec<OsString>) -> (Vec<OsString>, Option<&'static str>) {
    let mut warned: Option<&'static str> = None;
    let mut out: Vec<OsString> = Vec::with_capacity(argv.len() + 2);

    // Token-level rewrites: legacy flag-verbs and aliases.
    for a in argv {
        match a.to_str() {
            Some("--claim") => push_verb(&mut out, "claim", &mut warned),
            Some("--done") => push_verb(&mut out, "done", &mut warned),
            Some("--release") => push_verb(&mut out, "release", &mut warned),
            Some("--rm") => push_verb(&mut out, "rm", &mut warned),
            Some("-l") => push_verb(&mut out, "list", &mut warned),
            Some("--counts" | "-c") => {
                out.push("status".into());
                out.push("--json".into());
                warned = Some(DEP);
            }
            Some("log" | "history") => {
                out.push("list".into());
                out.push("--state".into());
                out.push("all".into());
                warned = Some(DEP);
            }
            _ => out.push(a),
        }
    }

    // Legacy `ccq --json` (no verb) meant "list as json". Safe to map (read-only).
    let has_verb = out
        .iter()
        .any(|a| a.to_str().is_some_and(|s| VERBS.contains(&s)));
    if !has_verb && out.iter().any(|a| a.to_str() == Some("--json")) {
        out.insert(0, "list".into());
        warned = Some(DEP);
    }

    // Deliberately NOT mapped (the redesign removed these footguns; with a clean
    // cutover there's no reason to resurrect them):
    //   • bare `ccq "msg"` → send  — a typo'd verb must be a usage error, not a
    //     silently-enqueued message (SPEC D1).
    //   • `clear` → `clear --yes`  — the --yes guard must stay meaningful.

    (out, warned)
}

fn push_verb(out: &mut Vec<OsString>, verb: &str, warned: &mut Option<&'static str>) {
    out.push(verb.into());
    *warned = Some(DEP);
}
