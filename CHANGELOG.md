# Changelog

All notable changes to ccq are documented here. Versioning is [SemVer](https://semver.org/).

## [0.0.1] — 2026-06-09

Initial public release. A per-project, agent-agnostic message queue for handing work
to AI coding-agent sessions — maildir-style, daemonless, lockless. Written in Rust
(edition 2024) and shipped as a single committed universal macOS binary (`bin/ccq`),
so the Claude Code plugin runs it with no build step.

### Added
- **Core queue** — `send` enqueues; `list`/`status`/`root` query; `wait` blocks at
  zero CPU until a message arrives (event-driven via kqueue `EVFILT_VNODE`, poll
  fallback, `--timeout`→124); `claim`/`next` reserve atomically; `done`/`release`/`rm`/
  `clear` finish or drop. Every state transition is an atomic no-clobber rename
  (`renameatx_np(RENAME_EXCL)`); a dead consumer's claim is auto-reaped back to pending.
- **Flat-verb grammar** with lenient flag placement. `--json` on every read command
  emits JSON Lines (stdout = data only, stderr = human chrome) and is byte-identical
  across `--lang`. Fixed exit codes: `0` ok / `1` op-error / `2` usage / `3` partial-race
  / `4` empty (`next`) / `124` `wait --timeout`.
- **Agent-neutral store** — queues live under `$CCQ_HOME` (default `$XDG_STATE_HOME/ccq`
  → `~/.local/state/ccq`), keyed by the canonical project-root path. A `path.txt`
  collision guard errors loudly rather than misdelivering when two paths encode alike.
- **Queue-root resolution** — `--root`/`CCQ_ROOT` (exact) → nearest `.ccq/` marker
  (ceiling `$HOME`) → **a git worktree resolves to its main repo's queue** (`--worktree`/
  `CCQ_WORKTREE` opts out) → enclosing `.git` → launch dir. `status`/`root` report which
  rule fired (`via`). `ccq init` drops a `.ccq/` marker for monorepo per-package queues.
- **Sub-key channels** — `--key <slug>` (or `CCQ_KEY`) addresses an independent maildir
  nested under the root (`<store>/<encoded-root>/keys/<slug>/`), for several roles/lanes
  sharing one checkout. Default (no key) is unchanged; `--no-key` forces the root queue;
  slugs are `^[a-z0-9][a-z0-9._-]{0,63}$` (lowercase-only for case-insensitive FS),
  `default`/`all`/`keys` reserved. `status` lists channels (`keys: […]`); `--all` spans
  queues and channels and honors `--state`.
- **Worktrees are transparent** — "send to the project" reaches the consuming session
  whichever checkout it runs in, because a linked worktree resolves to the main working
  tree by default (derived from git's `.git`-file/`commondir` pointers, no `git` fork;
  `via=git-main`). Intentional separation is done with a sub-key, not the worktree
  boundary. Submodules and bare-repo worktrees keep their own queue.
- **`ccq config`** — read-only effective settings with provenance.
- **`ccq install`** — atomic copy of the running binary to `~/.local/bin` for statusline/
  terminal/cron use (in-session the plugin already puts `ccq` on PATH).
- **`ccq doctor`** — diagnoses the store, a legacy `~/.claude/inbox` migration, the stable
  copy, and version/protocol.
- **Bilingual output** — English by default; Korean via `--lang ko` or `CCQ_LANG=ko`
  (`--lang` > `CCQ_LANG` > `en`; no locale auto-detection). Never affects `--json`.
- A one-release **compatibility shim** translates the legacy bash CLI's command
  spellings to the flat-verb grammar with a stderr deprecation note (removed before 1.0).

### Notes
- **macOS only** by design (`proc_pidinfo`, `kqueue`, `renameatx_np`, `arc4random_buf`);
  the build fails to compile elsewhere. On-disk `protocol` is `2`.
- No subprocess forks: in-process JSON (serde), time (jiff), process identity
  (`proc_pidinfo`), atomic moves (`renameatx_np`). Sub-10ms per command.
