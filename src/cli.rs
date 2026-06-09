//! Hand-rolled flat-verb parser (via `lexopt`). Global options may appear before
//! or after the verb (lenient). Unknown options are a usage error (exit 2). The
//! legacy-syntax compatibility rewrite (`compat`) runs *before* this.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use lexopt::Parser;
use lexopt::prelude::*;

use crate::error::{CcqError, Result};

#[derive(Debug, Default)]
pub struct GlobalOpts {
    pub dir: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub from: Option<String>,
    pub json: bool,
    pub lang: Option<String>,
    pub force: bool,
    pub all: bool,
    pub yes: bool,
    pub timeout: Option<u64>,
    pub interval: Option<u64>,
    pub label: Option<String>,
    pub key: Option<String>,
    pub no_key: bool,
    pub worktree: bool,
    pub state: Option<String>,
    pub build_hash: bool,
}

#[derive(Debug)]
pub enum SendInput {
    Body(String),
    Stdin,
}

#[derive(Debug)]
pub enum Command {
    Send(SendInput),
    List,
    Status,
    Root,
    Wait,
    Claim(Vec<String>),
    Next,
    Done(Vec<String>),
    Release(Vec<String>),
    Rm(Vec<String>),
    Clear,
    Init,
    Config,
    Install,
    Doctor,
    Version,
    Help,
}

pub struct Cli {
    pub cmd: Command,
    pub opts: GlobalOpts,
}

fn lex_err(e: lexopt::Error) -> CcqError {
    CcqError::usage(format!("ccq: {e}"))
}

pub fn parse<I>(argv: I) -> Result<Cli>
where
    I: IntoIterator<Item = OsString>,
{
    let mut parser = Parser::from_args(argv);
    let mut o = GlobalOpts::default();
    let mut verb: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut stdin_marker = false;
    let mut want_version_flag = false;
    let mut want_help_flag = false;

    while let Some(arg) = parser.next().map_err(lex_err)? {
        match arg {
            Short('d') | Long("dir") => {
                o.dir = Some(PathBuf::from(parser.value().map_err(lex_err)?));
            }
            Long("root") => {
                o.root = Some(PathBuf::from(parser.value().map_err(lex_err)?));
            }
            Short('f') | Long("from") => o.from = Some(val(&mut parser)?),
            Long("json") => o.json = true,
            Long("lang") => o.lang = Some(val(&mut parser)?),
            Long("force") => o.force = true,
            Long("all") => o.all = true,
            Long("yes") => o.yes = true,
            Long("timeout") => o.timeout = Some(num(&mut parser, "--timeout")?),
            Long("interval") => o.interval = Some(num(&mut parser, "--interval")?),
            Long("label") => o.label = Some(val(&mut parser)?),
            Long("key") => o.key = Some(val(&mut parser)?),
            Long("no-key") => o.no_key = true,
            Long("worktree") => o.worktree = true,
            Long("state") => o.state = Some(val(&mut parser)?),
            Long("build-hash") => o.build_hash = true,
            Short('h') | Long("help") => want_help_flag = true,
            Short('V') | Long("version") => want_version_flag = true,
            Value(v) => {
                let s = v
                    .string()
                    .map_err(|_| CcqError::usage("ccq: non-UTF-8 argument"))?;
                if verb.is_none() {
                    verb = Some(s);
                } else if s == "-" {
                    stdin_marker = true;
                } else {
                    positionals.push(s);
                }
            }
            other => {
                return Err(CcqError::usage(format!(
                    "ccq: unknown option: {}",
                    other.unexpected()
                )));
            }
        }
    }

    if want_help_flag {
        return Ok(Cli {
            cmd: Command::Help,
            opts: o,
        });
    }
    if want_version_flag && verb.is_none() {
        return Ok(Cli {
            cmd: Command::Version,
            opts: o,
        });
    }

    let cmd = match verb.as_deref() {
        None => Command::Help,
        Some("send") => Command::Send(send_input(&positionals, stdin_marker)?),
        Some("list" | "ls") => Command::List,
        Some("status") => Command::Status,
        Some("root") => Command::Root,
        Some("wait") => Command::Wait,
        Some("claim") => Command::Claim(require_ids(positionals, "claim")?),
        Some("next") => Command::Next,
        Some("done") => Command::Done(require_ids(positionals, "done")?),
        Some("release") => Command::Release(require_ids(positionals, "release")?),
        Some("rm" | "remove") => Command::Rm(require_ids(positionals, "rm")?),
        Some("clear") => Command::Clear,
        Some("init") => Command::Init,
        Some("config") => Command::Config,
        Some("install") => Command::Install,
        Some("doctor") => Command::Doctor,
        Some("version") => Command::Version,
        Some("help") => Command::Help,
        Some(v) => {
            return Err(CcqError::usage(format!(
                "ccq: unknown command: {v} (see -h)"
            )));
        }
    };
    Ok(Cli { cmd, opts: o })
}

fn val(p: &mut Parser) -> Result<String> {
    p.value()
        .map_err(lex_err)?
        .string()
        .map_err(|_| CcqError::usage("ccq: non-UTF-8 value"))
}

fn num(p: &mut Parser, flag: &str) -> Result<u64> {
    val(p)?
        .parse()
        .map_err(|_| CcqError::usage(format!("ccq: {flag} requires an integer (seconds)")))
}

fn send_input(positionals: &[String], stdin_marker: bool) -> Result<SendInput> {
    if stdin_marker {
        return Ok(SendInput::Stdin);
    }
    if !positionals.is_empty() {
        return Ok(SendInput::Body(positionals.join(" ")));
    }
    if !std::io::stdin().is_terminal() {
        return Ok(SendInput::Stdin); // piped body
    }
    Err(CcqError::usage(
        "ccq: no message (pass text, '-', or pipe via stdin)",
    ))
}

fn require_ids(ids: Vec<String>, verb: &str) -> Result<Vec<String>> {
    if ids.is_empty() {
        Err(CcqError::usage(format!(
            "ccq: {verb} requires at least one id"
        )))
    } else {
        Ok(ids)
    }
}

/// The verb name for a command (for diagnostics).
fn verb_name(cmd: &Command) -> &'static str {
    match cmd {
        Command::Send(_) => "send",
        Command::List => "list",
        Command::Status => "status",
        Command::Root => "root",
        Command::Wait => "wait",
        Command::Claim(_) => "claim",
        Command::Next => "next",
        Command::Done(_) => "done",
        Command::Release(_) => "release",
        Command::Rm(_) => "rm",
        Command::Clear => "clear",
        Command::Init => "init",
        Command::Config => "config",
        Command::Install => "install",
        Command::Doctor => "doctor",
        Command::Version => "version",
        Command::Help => "help",
    }
}

/// Reject a command-specific flag that was set on a command that ignores it
/// (e.g. `status --timeout 5`, `send --state done`) — a silent no-op is a footgun
/// for an agent caller. Broad flags (`-d`/`--root`/`--json`/`--lang`/`--key`/
/// `--no-key`/`--worktree`) are tolerated everywhere; `--all` is policed in `main`.
/// `help`/`version` are forgiving (informational — never error on a stray flag).
pub fn validate_opts(cmd: &Command, o: &GlobalOpts) -> Result<()> {
    use Command::*;
    if matches!(cmd, Help | Version) {
        return Ok(());
    }
    // (flag is set, the only command that accepts it, flag name)
    let checks = [
        (o.from.is_some(), matches!(cmd, Send(_)), "--from"),
        (o.force, matches!(cmd, Done(_) | Release(_)), "--force"),
        (o.timeout.is_some(), matches!(cmd, Wait), "--timeout"),
        (o.interval.is_some(), matches!(cmd, Wait), "--interval"),
        (o.label.is_some(), matches!(cmd, Init), "--label"),
        (o.state.is_some(), matches!(cmd, List), "--state"),
        (o.yes, matches!(cmd, Clear), "--yes"),
        (o.build_hash, false, "--build-hash"), // version-only, handled by the early skip
    ];
    for (set, allowed, name) in checks {
        if set && !allowed {
            return Err(CcqError::usage(format!(
                "ccq: {name} is not valid for `{}`",
                verb_name(cmd)
            )));
        }
    }
    Ok(())
}
