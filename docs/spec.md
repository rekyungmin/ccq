# ccq — Design Spec

Status: **implemented in Rust.** This document is the design contract; the implementation
lives in `src/` (a single committed universal macOS binary at `bin/ccq`). It supersedes the
command surface of the prior bash CLI; relative to that predecessor the message *body* is
unchanged, but the `cur/` owner-signature format changed (microsecond start-time, not
`cksum(ps -o lstart)`), so the on-disk **`protocol` is `2`**.

The design is driven by one overriding constraint:

> **The primary caller is an AI agent**, not a human — a coding agent invoking `ccq` through a
> shell tool, reading the text/JSON back, and branching on it. Every decision below optimizes for
> an LLM that should be able to *guess the command and be right*, get *stable machine output*, and
> *predict which queue a command hits* before running it. Occasional human use must stay tolerable.

---

## 0. Positioning — agent-agnostic

`ccq` is **not Claude-specific.** It is a standalone bash + `jq` CLI that any agent (Claude Code,
Codex, Antigravity/Gemini, Cursor, Aider, …) or a human can call. Two consequences:

1. **The queue store is agent-neutral** (§1). Queues do **not** live under `~/.claude/`. A Codex
   session can `ccq send` to a project and a Claude session running in that project picks it up —
   `ccq` is a **cross-agent, per-project message bus.**
2. **Root resolution is agent-neutral** (§2). It relies on the filesystem and `git`, not on any
   agent's transcripts. The one Claude-specific bit (transcript reverse-lookup for cwd-drift
   immunity) is an *optional refinement* layered on top; absent it, resolution still works for
   everyone via `git`/marker/cwd.

---

## 1. Storage layout & paths

### 1.1 Queue store location (the key change)

The queue store root is resolved with this precedence:

1. **`$CCQ_HOME`** — explicit override (env). Highest precedence. Useful for tests (point at a
   `mktemp -d`), CI isolation, or relocating the store.
2. **`$XDG_STATE_HOME/ccq`** — if `XDG_STATE_HOME` is set.
3. **`~/.local/state/ccq`** — default.

Rationale for `XDG_STATE_HOME` (not `XDG_DATA_HOME`, not `~/.claude/inbox`, not `~/.ccq`): a queue
is *current operational state that persists across restarts* plus auto-trimmed history (`done/`,
capped) — the exact thing the freedesktop spec defines `XDG_STATE_HOME` for ("logs, history,
current state"). It is per-machine and not meant to be backed up or synced, which rules out
`XDG_DATA_HOME`. It is consistent with `ccq install` already targeting `~/.local/bin`.

> If you prefer not to honor XDG, `~/.ccq` is a defensible simpler default — but this spec
> standardizes on `$XDG_STATE_HOME/ccq` → `~/.local/state/ccq`.

### 1.2 On-disk layout

```
$CCQ_HOME/                              # default ~/.local/state/ccq
└── <encoded-root-path>/                # one dir per resolved queue root (see §2, §6)
    ├── tmp/                            # being written (pre-publish)
    ├── new/<epoch>-<id>.json           # pending
    ├── cur/<epoch>-<id>.<pid>.<sig>.<claimed-at>.json   # processing (owner in filename)
    ├── done/<done-at>-<id>.json        # completed history (CCQ_HISTORY_KEEP cap, default 200)
    ├── path.txt                        # real (canonical) root path — collision guard (§6)
    └── keys/<slug>/                    # sub-key channels (--key / CCQ_KEY); one level only
        └── {tmp,new,cur,done}/         # each channel is its own maildir; path.txt stays at root
```

**Sub-key channels.** `--key <slug>` (or `CCQ_KEY`) routes to an independent
maildir nested under the resolved root, for several roles/consumers sharing one
checkout. The default (no key) queue keeps its original path (backward
compatible). The `path.txt` collision guard lives only at the root — channels
inherit the root's identity, so they are never separately guarded. Slug:
`^[a-z0-9][a-z0-9._-]{0,63}$` (lowercase only — case-insensitive filesystems
would otherwise alias `Review`/`review`); `default`/`all`/`keys` are reserved.
`--no-key` forces the root queue even when `CCQ_KEY` is set.

Message JSON body (`protocol 2`; `<id>` is now 16 hex chars, not uuid8):
`{"id":"<id16>","ts":"<ISO8601 local TZ>","from":"<label>","msg":"<body>"}`

Owner identity (`pid`/`sig`/`claimed-at`) lives in the **`cur/` filename**, not the JSON body; read
commands synthesize those fields into `--json` output at read time.

### 1.3 The root **marker** (in-tree, distinct from the store)

There are two different things named `ccq`; do not confuse them:

| Thing | Location | Purpose | Committed to git? |
|---|---|---|---|
| **Root marker** `.ccq/` | **inside the project repo**, at a chosen root | declares "this dir is a queue root" (§2) | **yes** — shared by the team & all their agents |
| **Queue store** `$CCQ_HOME` | **per-machine**, outside any repo | holds the actual messages | no |

The marker is a directory `.ccq/` whose **presence** is the signal; it optionally contains
`.ccq/root.json` `{"label": "...", "created": "<ISO8601>"}`. `label` is display-only metadata that
**never** affects the queue key, so renaming a label cannot silently re-route a queue. A bare
`mkdir .ccq` (no `root.json`) is a valid root. *(Surfacing `label` in `status`/`--all` output is a
planned enhancement; the current build does not yet emit it.)*

### 1.4 Legacy migration

On startup, if the legacy store `~/.claude/inbox` exists and the new store is empty, `ccq doctor`
**reports it** and prints the one-time migration command (`mv ~/.claude/inbox/* "$CCQ_HOME"/`).
Implementations MAY auto-migrate when the target is empty. No silent dual-write.

---

## 2. Queue root resolution

Resolution turns a *starting directory* into a *queue root*, then encodes that root (§6) to get the
queue key. **This is the core behavioral change**: the queue is keyed at a *project root*, not the
exact working directory — so any subpath of a repo shares one queue, while monorepo sub-packages
can be isolated.

### 2.1 Precedence (first match wins)

1. **`--root <dir>` / `$CCQ_ROOT`** — force this exact dir as the root; **no upward walk**.
   `via=flag` / `via=env`. The escape hatch for exact addressing.
2. **Nearest `.ccq/` marker** — walk up from the start dir toward `/`, **stopping at `$HOME`**;
   first `.ccq/` found wins; root = the dir containing it. `via=marker`.
3. **Worktree → main working tree** *(default; `--worktree`/`CCQ_WORKTREE` opts out)* — a linked
   worktree resolves to the **main working tree** (the project's shared queue), so a message sent to
   the project is picked up whichever checkout the consuming session happens to run in. `via=git-main`.
   Resolved purely by reading git's `.git`-file → `gitdir` → `commondir` pointers (no fork). The main
   root is used **directly** (no second marker walk). A linked worktree is identified structurally —
   the `gitdir` admin dir's parent is `worktrees/` (a submodule's is `modules/`), not a path
   substring. **`--worktree`** keeps the worktree's *own* queue (`via=git`). Anything that is not a
   linked worktree with a real main working tree — a plain repo, a `--separate-git-dir`/symlinked
   `.git`, a submodule (a distinct repo), or a bare main (no working tree) — simply uses rule 4
   (`via=git`), no redirect, no note. A `.ccq/` marker (rule 2) still overrides the redirect.
   *Rationale:* worktrees are transparent for routing — intentional separation is done with a
   **sub-key** (§1.2), not by which checkout you launched in.
4. **git toplevel** — `git rev-parse --show-toplevel`. `via=git`. The zero-config default.
5. **Launch dir** — the canonicalized start dir itself. `via=launchdir`. Fallback when not in a
   repo and no marker exists.

`via` is a **closed set**: `flag | env | marker | git | git-main | launchdir`. It is surfaced
everywhere (§3 `root`/`status`, §4 JSON) so an agent always knows *which rule fired*.

### 2.2 `-d` vs `--root` (decisive)

- **`-d` / `--dir <dir>`** = the **starting directory for resolution**. It *walks up* through
  rules 2–4, exactly as if the session were launched there. So `ccq send -d ~/code/cortex/app/src`
  resolves to the `cortex` root and the message lands in cortex's queue — **this is the required
  "send to a subpath, arrives at the repo root" behavior.**
- **`--root <dir>`** = **force this exact dir** as the queue root, bypassing discovery (rule 1).

The starting directory, when `-d`/`--root` are absent, is: the agent session's launch dir if
recoverable (Claude: transcript reverse-lookup, immune to cwd drift), else `$PWD`. Either way,
rules 2–4 then walk up from it.

### 2.3 Scenario outcomes

| Standing in | `.ccq/` marker | `via` | Resolved root |
|---|---|---|---|
| `cortex/app/src` (cortex is a git repo) | none | `git` | `cortex` ✅ zero-config |
| `cortex/app/src` | at `cortex/app` | `marker` | `cortex/app` (intentional split) |
| monorepo `…/services/A/src` | `ccq init` at `services/A` | `marker` | `services/A` ✅ per-package |
| monorepo `…/services/A/src` | none | `git` | monorepo root ⚠️ **footgun → run `ccq init` per service** |
| `/tmp/scratch` (no git, no marker) | none | `launchdir` | `/tmp/scratch` |

### 2.4 Edge-case rulings

- **Nested markers** (`.ccq/` at `cortex` *and* `cortex/app`): nearest wins. `ccq init` warns when
  it detects a parent marker (legal but flagged).
- **git worktrees**: a linked worktree resolves to the **main working tree by default** (§2.1 rule
  3), so "send to the project" reaches the consuming session regardless of which checkout it is in —
  the common handoff case. The main root is derived from git's `.git`-file/`commondir` pointers (no
  fork). `--worktree`/`CCQ_WORKTREE` opts out and keeps the worktree's own distinct queue. Submodules
  are *not* redirected (a submodule is a distinct repo). *(This reverses the earlier "each worktree
  is its own queue" default: worktree isolation silently stranded project messages; intentional
  per-lane separation is done with a sub-key, not the worktree boundary.)*
- **Symlinks**: walk the *logical* path to find the marker/git boundary, then canonicalize the
  resulting root with `pwd -P` before encoding. One physical path = one queue.
- **`$HOME` ceiling**: the marker walk stops at `$HOME` so a stray `~/.ccq` cannot capture every
  repo under home. (`ccq init` at `$HOME` still works — start dir *equals* the marker dir.)
- **Fork cost**: the `.ccq/` walk is pure bash (0 forks). `git rev-parse` is forked **only** when
  no marker is found. Memoize the resolved root within the process (one resolution per invocation).
- **Start dir not recoverable as a real path** (e.g. Claude transcript without a `cwd`): skip the
  upward walk and key at the launch-dir encoding (graceful fallback — never error).
- **Bad `-d`/`--root`** (nonexistent dir): exit `2` (usage error), never a silent fallback.

---

## 3. Command set (flat-verb)

Grammar: `ccq [global-options] <verb> [args] [verb-options]`. **Flat verbs** (git-style), because
`ccq` manages essentially one resource (queue messages); the verb's argument shape makes the noun
obvious (`<id>...` ⇒ a message; none/`--state` ⇒ the queue). No `noun verb` nesting.

### 3.1 Verbs

```
# ── Producer ───────────────────────────────────────────────
ccq send [MESSAGE]            enqueue to the resolved-root queue
ccq send -                    enqueue, reading the body from stdin

# ── Query (read) ───────────────────────────────────────────
ccq list                      enumerate messages (default state: pending,processing)
ccq status                    resolved root + via + marker + queue path + counts
ccq root                      print the resolved root (one line); the scriptable resolver

# ── Block (the new wait primitive) ─────────────────────────
ccq wait                      block until a message is pending, then exit 0 (non-claiming)

# ── Process (mutate) ───────────────────────────────────────
ccq claim <id>...             atomically reserve; prints body; exactly one winner per race
ccq next                      atomically claim+print the oldest pending (autonomous dequeue)
ccq done <id>...              complete → archive to done/ (own claims; --force to override)
ccq release <id>...           return to pending (own claims; --force)
ccq rm <id>...                delete a pending message (alias: remove)
ccq clear --yes               drain the pending queue (--yes required)

# ── Setup / manage ─────────────────────────────────────────
ccq init [--label NAME]       designate the resolved start dir as a queue root (drops .ccq/)
ccq config                    print effective settings + their source (read-only)
ccq install                   install a stable ccq to ~/.local/bin (statusline/terminal/cron)
ccq doctor                    diagnose setup (jq, PATH, store, legacy migration)
ccq version                   print version
ccq help                      help
```

### 3.2 `wait` semantics (decisive)

- **Single-shot, non-claiming.** Event-driven via kqueue `EVFILT_VNODE` on `new/`
  (`NOTE_WRITE|NOTE_DELETE|NOTE_RENAME|NOTE_REVOKE`) — instant wake, zero CPU/zero tokens while
  blocked; a poll loop is the fallback when the watch can't be set up. Either way: check → register
  → re-check (close the register-after-arrival TOCTOU) → block; events are wake-ups, not truth, so
  it always re-`reap`+recounts on wake. Blocks until pending goes `0 → ≥1`, then prints and exits.
  It does **not** claim — `wait` returning is a *notification*, not a *reservation* (preserves the
  rename race model: many may wake, exactly one wins the subsequent `claim`/`next`). The woken
  session then runs the interactive flow (`list` → user selects → `claim`) or autonomous (`next`).
- **Returns 0 immediately if the queue is already non-empty** (no TOCTOU; "ensure there is work").
- **`--timeout <sec>`** → exit `124` if nothing arrives (default: block forever; the harness owns
  the lifecycle — process exit is the wake signal).
- **`--interval <sec>`** → poll period (default `2`).
- **`--json`** → on wake, print the available message(s) as JSONL so the agent can act in one call.

### 3.3 `next` vs `claim`

`claim <id>...` reserves *specific* ids (the interactive path: `list` → pick → claim). `next`
atomically claims+prints the *oldest* pending in one call (the autonomous path:
`while ccq wait; do ccq next --json; …; ccq done <id>; done`). `next` exits `4` when the queue is
empty so an autonomous loop can branch idle-vs-work.

### 3.4 Global options

```
-d, --dir <dir>      resolution start directory (walks up; default: session launch dir / $PWD)
    --root <dir>     force this exact dir as the queue root (no walk)
    --from <label>   sender label (send only; default: user@host)
    --json           machine output (JSONL); see §4
    --lang <en|ko>   human-text language only (never affects --json)
    --force          bypass the ownership check (done/release)
    --all            apply across all queues + channels (list/status; error where undefined — see §4)
    --key <slug>     target a sub-key channel within the root (CCQ_KEY; flag wins; §1.2)
    --no-key         force the root queue even when CCQ_KEY is set
    --worktree       keep a linked worktree's own queue (default: a worktree → the main repo; CCQ_WORKTREE; §2.1)
    --no-reap        skip the self-healing reap pass (rarely needed)
```

Long form is canonical. The only short alias kept is `-d` (high-frequency, universally understood).
`-l`/`-c`/`-V` and other short flags are dropped — an agent pays nothing for the long form and
short aliases only add ambiguity.

---

## 4. Machine output contract & exit codes

### 4.1 `--json`

- **One boolean flag, `--json`**, on every read command. Not a `--format`/`-o` enum (binary is
  unmissable for an LLM; nothing to misremember).
- **Output is JSON Lines** — one object per line, **never** a pretty array. Streamable, append-safe,
  trivially `jq`-able per line.
- **`stdout` carries data only.** All human chrome (the `📂 <root>` header, the `root/via` lines,
  warnings, progress) goes to **`stderr`**. An agent can `2>/dev/null` and trust stdout.
- **`--json` is byte-identical regardless of `--lang`.** Field values like `state` are stable enums
  (`new`/`claimed`), never localized.
- **No envelope.** Bare objects + exit codes for control flow (an `{ok,command,result}` wrapper
  wastes tokens on every call for a single-resource tool).

Message object (read commands — `list`/`wait`/`next`/`claim`):
```jsonc
{"id":"a1b2c3d4","ts":"2026-06-09T15:55:00+0900","from":"pie@mac","msg":"…","state":"new"}
{"id":"…","ts":"…","from":"…","msg":"…","state":"claimed","pid":12345,"claimed_at":1749000000,"age_s":320,"stale":false}
```

`status --json` (exactly one object; shape NEVER changes with flags). `key` is the active channel
(`null` for the root queue); `keys` lists the channel slugs that exist under the root:
```json
{"root":"/Users/pie/code/cortex","via":"git","marker":null,"encoded_key":"-Users-pie-code-cortex","queue":"/Users/pie/.local/state/ccq/-Users-pie-code-cortex","key":null,"keys":["review"],"pending":3,"processing":1,"archived":12}
```

`status --json --all` / `list --json --all`: one object per line (JSONL). `status --all` objects are
`{encoded_key, subkey, path, pending, processing, archived}`; `list --all` objects carry the normal
message fields (including a `done` object's `state:"done"`/`done_at`) plus `encoded_key` (the encoded
root, for joining) and `subkey`. `subkey` is `null` for the root queue, the slug for a channel (one
nesting level only). **`list --all` honors `--state`** (default = active: pending+processing) — it is
a *scope* modifier orthogonal to the *content* filter, so `list --all --state done|all` lists those
states across every queue. `--all` is an **error** (exit 2) on verbs where it is undefined, and
**cannot be combined with `--key`/`--no-key`/`--worktree`** (nor their `CCQ_KEY`/`CCQ_WORKTREE` env
equivalents) — a per-queue *target* is undefined across all queues, so it errors rather than silently
ignoring an opt-in the user set.

`root --json` (same `via`/`marker`/`encoded_key`/`queue`/`key` as `status`, minus the counts):
```json
{"root":"/…/cortex","via":"git","marker":null,"encoded_key":"-…-cortex","queue":"/…/ccq/-…-cortex","key":null}
```

### 4.2 Exit codes (fixed, documented, agent-branchable)

| Code | Meaning |
|---|---|
| `0` | success (wait: a message is available; batch claim/done/release/rm: **all** ids ok) |
| `1` | operational error (store unreadable, `jq` missing, publish conflict) |
| `2` | usage error (unknown verb, bad args, **bad `-d`/`--root` path**) — *your fault, fix the call* |
| `3` | partial failure / race (some ids in a batch failed; `claim` lost a race) — *re-query and retry* |
| `4` | empty (`next` had nothing to dequeue) |
| `124` | `wait --timeout` elapsed with nothing (GNU `timeout(1)` convention) |

The load-bearing distinction is **`2` (usage — fix the call) vs `3` (race/partial — retry)**:
the legacy bash CLI conflated both as `1`, leaving an agent unable to tell "I typed it wrong"
from "a peer claimed it first." Codes `126`/`127`/`128+` are shell-reserved and avoided.

---

## 5. Config

**No mutable `ccq config set`.** Persistent hidden state makes identical commands behave differently
across runs — the opposite of what an AI caller needs. Configuration is layered, lowest→highest:

```
built-in defaults  <  (optional) config file  <  env vars (CCQ_*)  <  per-invocation flags
```

`ccq config` is **read-only**: it prints the *effective* settings and **their source** so an agent
(or human) can verify what is in effect:

```
ccq config            # lang=ko (env CCQ_LANG) / from=pie@mac (default) / history_keep=200 (default) …
ccq config --json     # {"lang":"ko","lang_src":"env", "store":"/…/ccq","store_src":"default", …}
```

Settings inventory:

| Setting | Env | Flag | Default | Notes |
|---|---|---|---|---|
| language | `CCQ_LANG` | `--lang` | `en` | human output only |
| sender label | — | `--from` | `user@host` | |
| history cap | `CCQ_HISTORY_KEEP` | — | `200` | `done/` retention |
| stale-claim warn | `CCQ_STALE_WARN` | — | `43200` (12h) | `list` ⚠ threshold |
| poll interval | `CCQ_POLL_INTERVAL` | `--interval` | `2` | `wait` |
| store root | `CCQ_HOME` | — | `$XDG_STATE_HOME/ccq` | §1.1 |
| queue root | `CCQ_ROOT` | `--root` | (resolved §2) | force-exact |
| sub-key channel | `CCQ_KEY` | `--key` / `--no-key` | (none) | §1.2; flag wins |
| worktree own queue | `CCQ_WORKTREE` | `--worktree` | off (worktree → main) | §2.1; opt out of the default redirect |
| owner pid | `CCQ_OWNER_PID` | — | (auto) | process identity; ephemeral, never persisted |

A persistent config *file* is optional (only ~5 stable prefs exist); if added, place it below env in
precedence and use bash-sourceable `KEY=value` (no parser, no `jq`). Output format is **never**
configurable — `text` is always the default, `--json` always explicit.

---

## 6. Identity / encoding & the collision guard

The queue key is the **canonical absolute root path** (§2), encoded by replacing `/` and `.` with
`-` (same scheme as before, kept for human-readable store dirs).

This encoding is **lossy** — `/a/b.c`, `/a/b-c`, and `/a.b/c`-style paths can collide to the same
key. The legacy bash CLI wrote `path.txt` but **never checked it**, so a collision caused *silent
cross-project delivery*. The spec mandates a **collision guard**:

- On **materializing** a queue dir (any command that touches the queue — `send`/`list`/`status`/
  `wait`/`claim`/`next`/`done`/`release`/`rm`/`clear`), if `path.txt` exists and its content ≠ the
  current canonical root, **error loudly** (exit `1`: `queue key collision: <thisroot> encodes to an
  existing queue for <otherroot>`) instead of writing into the wrong project's queue. The pure
  resolvers `root`/`config` only *display* the computed path (no I/O, no guard) — any actual
  operation re-validates, so a collision can never cause a silent misdelivery.
- Always canonicalize via `pwd -P` before encoding (resolves symlinks, `/tmp`→`/private/tmp`).

Addressing model verdict (unchanged): **path-based addressing + maildir transport is correct** for
a local, daemon-free, restart-surviving, agent-neutral handoff tool. **Sub-key channels** (`--key`,
§1.2) extend this *within* a root — a channel is just a nested maildir, still pure path addressing,
no daemon or router. Peer/role *targeting* (`--to <name>`) and session-id targeting remain deferred
until a real need appears (a channel is a shared lane, not a directed message to a named peer).

---

## 7. Backward compatibility & migration

Pre-1.0; **break the surface, with a one-release shim.**

- This release ships the flat-verb grammar as canonical. A translation layer accepts every legacy
  bash-CLI spelling (`ccq "msg"`, `ccq -`, `-l`, `--json` as a command, `-c`, `--counts`, `--claim`,
  `--done`, `--release`, `--rm`, `clear`, `history`, `log`) executing the new code path and printing
  a **stderr** deprecation warning (never stdout — must not corrupt machine output). Removed before 1.0.
- The **in-repo skills** (`skills/send/SKILL.md`, `skills/listen/SKILL.md`) — the primary callers —
  use the flat-verb spelling. That is the real migration.
- **Storage move**: `~/.claude/inbox` → `$CCQ_HOME` (§1.4), surfaced by `ccq doctor`.
- `protocol` → `2` (the `cur/` owner-signature format changed). A clean cutover (the Rust binary
  replaces bash; no concurrent operation on shared queues) plus the conservative reap rule make the
  migration window safe; `ccq doctor` flags the legacy `~/.claude/inbox` store for a one-time move.

Old → new mapping:

```
ccq "msg"          → ccq send "msg"
ccq -              → ccq send -
ccq -l | list      → ccq list
ccq --json         → ccq list --json
ccq -c | --counts  → ccq status --json
ccq --claim ID     → ccq claim ID
ccq --done ID      → ccq done ID
ccq --release ID   → ccq release ID
ccq --rm ID        → ccq rm ID
ccq clear          → ccq clear --yes
ccq history | log  → ccq list --state all   (+ --json / --all)
```

---

## Appendix A — resolution pseudo-code

```bash
# Globals produced: ROOT (canonical abs), VIA (flag|env|marker|git|launchdir),
#                   MARKER (abs path to .ccq or ""), ENC, DIR.
# Pure function of: start dir, on-disk markers, git, flags/env. No clocks/PIDs/network.
# Fork budget: 0 forks if a marker is found; at most one `git rev-parse` otherwise.

canon()          { cd "$1" 2>/dev/null && pwd -P || printf ''; }   # physical abs path; "" on fail

find_marker_up() {                          # echo dir containing nearest .ccq/, ceiling at $HOME
  _d=$1
  while :; do
    [ -d "$_d/.ccq" ] && { printf '%s' "$_d"; return 0; }
    [ "$_d" = "$HOME" ] && return 1
    _p=${_d%/*}; [ -n "$_p" ] || _p=/
    [ "$_p" = "$_d" ] && return 1           # reached /
    _d=$_p
  done
}

resolve_root() {
  # 1) explicit exact root — wins, no walk
  if [ -n "${ROOT_FLAG:-}" ]; then ROOT=$(canon "$ROOT_FLAG"); [ -n "$ROOT" ] || die 2; VIA=flag; MARKER=""; return; fi
  if [ -n "${CCQ_ROOT:-}"  ]; then ROOT=$(canon "$CCQ_ROOT");  [ -n "$ROOT" ] || die 2; VIA=env;  MARKER=""; return; fi

  # start dir: session anchor (Claude transcript, cwd-drift-immune) else $PWD; -d overrides
  START=$(resolve_start_dir)                # may be "" if only an encoded key is recoverable
  if [ -z "$START" ]; then ENC=$(encoded_launch_key); return; fi   # graceful fallback (legacy behavior)
  START=$(canon "$START"); [ -n "$START" ] || die 2

  # 2) nearest .ccq/ marker (pure bash, 0 forks) — beats git
  if _m=$(find_marker_up "$START"); then ROOT=$(canon "$_m"); VIA=marker; MARKER="$_m/.ccq"; return; fi

  # 3) git toplevel (one fork, only when unmarked) — worktree-correct
  if _top=$(git -C "$START" rev-parse --show-toplevel 2>/dev/null) && [ -n "$_top" ]; then
    ROOT=$(canon "$_top"); VIA=git; MARKER=""; return
  fi

  # 4) launch dir fallback
  ROOT="$START"; VIA=launchdir; MARKER=""
}

# wiring (replaces the legacy encode_path-of-launch-dir block):
resolve_root
ENC=${ENC:-$(printf '%s' "${ROOT//[\/.]/-}")}
DIR="$CCQ_HOME/$ENC"
# collision guard (§6):
if [ -f "$DIR/path.txt" ] && [ "$(cat "$DIR/path.txt")" != "$ROOT" ]; then
  die 1 "queue key collision: $ROOT vs $(cat "$DIR/path.txt")"
fi
[ -d "$DIR/done" ] || mkdir -p "$DIR/tmp" "$DIR/new" "$DIR/cur" "$DIR/done"
[ -f "$DIR/path.txt" ] || printf '%s\n' "$ROOT" > "$DIR/path.txt"
```

## Appendix B — summary of rulings

| Area | Ruling |
|---|---|
| Store location | `$CCQ_HOME` > `$XDG_STATE_HOME/ccq` > `~/.local/state/ccq` (agent-neutral, off `~/.claude`) |
| Grammar | flat-verb (`ccq <verb>`); arg shape implies the noun |
| Root default | git toplevel (zero-config); `.ccq/` marker overrides; `-d` walks, `--root` is exact |
| Marker | `.ccq/` **directory** (presence = marker) + optional `root.json {label}`; committed to git |
| `wait` | single-shot, non-claiming, poll loop; `0` arrival / `124` timeout |
| `next` | autonomous atomic dequeue; `4` when empty |
| Output | one `--json` boolean → JSONL; stdout=data, stderr=chrome; `--json` ⟂ `--lang` |
| Exit codes | `0/1/2/3/4/124`; **`2` usage vs `3` race** is the key split |
| Config | read-only `ccq config`; no mutable set; env primary; format never configurable |
| Encoding | canonical (`pwd -P`) + **path.txt collision guard** (error, not silent misdelivery) |
| Compat | break + one-release stderr shim; migrate skills in lockstep; `protocol` → 2; clean cutover |
