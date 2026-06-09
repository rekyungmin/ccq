# ccq CLI Reference

Per-project, agent-agnostic message queue. The queue is keyed at a **project root**
(a subpath resolves up to it). [`spec.md`](spec.md) is the full design contract.

```
ccq [global-options] <verb> [args]
```

Output language is English by default; `--lang ko` or `CCQ_LANG=ko` switches to Korean.
`--json` output is byte-identical regardless of language.

## Verbs

### Produce

| Command | Description |
|---|---|
| `ccq send "message"` | Enqueue. On success: `queued → <root> (N pending)` |
| `ccq send -` | Read the message from stdin (also auto-used when stdin is piped) |

### Query — auto-recovers dead claims (reap) on every run

| Command | Output |
|---|---|
| `ccq list` | Pending + processing (default). `--state pending\|processing\|done\|all` filters; `--all` spans projects |
| `ccq list --json` | Message JSON Lines, each with a `state` field (machine) |
| `ccq status` | `pending: N \| processing: M \| archived: K` + resolved root/via/marker. `--json` for a stable object; `--all` per-queue |
| `ccq root` | Print the resolved queue root (one line); `--json` adds `via`/`marker`/`encoded_key`/`queue` |

### Consume

| Command | Description |
|---|---|
| `ccq wait` | Block until a message is pending, then exit 0 (event-driven; `--timeout <s>`→124; `--interval <s>` poll). Non-claiming |
| `ccq claim <id>...` | Claim; prints the body JSON per success. Exactly one winner under contention |
| `ccq next` | Atomically claim+print the oldest pending (autonomous loop). Exit 4 when empty |
| `ccq done <id>...` | Complete → archive to `done/`. **Own claims only** (`--force` overrides) |
| `ccq release <id>...` | Give up → return to pending. Own claims only (`--force`) |
| `ccq rm <id>...` | Delete a pending message (alias `remove`) |
| `ccq clear --yes` | Drain the pending queue (processing/history kept). `--yes` is required |

### Set up / manage

| Command | Description |
|---|---|
| `ccq init [--label N]` | Mark the current dir a queue root (drops `.ccq/`) — for monorepo per-package isolation |
| `ccq config` | Read-only effective settings + their source. `--json` for a machine object |
| `ccq install` | Copy the binary to `~/.local/bin/ccq` for statusline/terminal/cron (in-session, the plugin auto-adds `bin/` to PATH) |
| `ccq doctor` | Diagnose: queue store, legacy `~/.claude/inbox` migration, stable copy, version/protocol |
| `ccq version` / `--version` / `-V` | Print the version (`--build-hash` adds the build id) |
| `ccq help` / `-h` | Help |

## Global options

| Option | Description |
|---|---|
| `-d, --dir <dir>` | Resolution **start** directory — walks up to the root (default: cwd) |
| `--root <dir>` | Force this exact dir as the queue root (no walk) |
| `--from <label>` | Sender label (`send`; default `user@host`; `-f` accepted) |
| `--json` | Machine output (JSON Lines) |
| `--lang <en\|ko>` | Human-text language only |
| `--force` | Bypass the ownership check (`done`/`release`) |
| `--all` | Span all queues, incl. sub-key channels (`list`/`status`) |
| `--key <slug>` | Target a sub-key **channel** within the resolved root (`CCQ_KEY` env; flag wins). Slug: `^[a-z0-9][a-z0-9._-]{0,63}$`; `default`/`all`/`keys` reserved |
| `--no-key` | Force the root queue even when `CCQ_KEY` is set |
| `--worktree` | Keep a linked worktree's **own** queue (`CCQ_WORKTREE` env). Default: a worktree resolves to the **main** repo's queue |
| `--state <s>` | `list` filter: `pending`\|`processing`\|`done`\|`all` |
| `--timeout <s>` / `--interval <s>` | `wait` ceiling / poll period |

Unknown options/verbs error with exit 2. A one-release shim accepts the legacy bash-CLI
spellings (`--claim`, `-l`, bare `ccq "msg"`, `--counts`, …) with a stderr deprecation note.

## Queue-root resolution

Precedence (a *start dir* — `-d` or cwd — resolves to a *root*):

1. `--root <dir>` / `CCQ_ROOT` — exact, no walk
2. nearest `.ccq/` marker walking up (ceiling at `$HOME`)
3. **worktree → main working tree** (default; `--worktree`/`CCQ_WORKTREE` opts out) — a linked
   worktree resolves to the main repo's working tree, derived from git's `.git`-file/`commondir`
   pointers; submodules and bare-main worktrees are not redirected
4. enclosing `.git` (dir or file)
5. the start dir itself

`status` (or `root --json`) reports which rule fired (`via ∈ flag|env|marker|git|git-main|launchdir`);
plain `ccq root` stays a one-line scriptable path. A worktree redirected to its main shows `via=git-main`.

By **default a linked git worktree resolves to its main repo's queue**, so "send to the project"
reaches the consuming session whichever checkout it happens to be in — e.g. `ccq send -d ~/code/foo
"…"` is picked up by a session running in *any* foo worktree. Intentional separation is done with a
**sub-key** (`--key`), not by the worktree boundary. `--worktree` (or `CCQ_WORKTREE`) opts out and
keeps that worktree's own distinct queue. The redirect composes with `--key`.

## Channels (sub-keys)

Within one resolved root, `--key <slug>` (or `CCQ_KEY`) addresses an independent
**channel** — its own maildir nested under the root's queue. Use it when several
roles/consumers share one project (e.g. an `impl` lane and a `review` lane).
Channels are the deliberate way to split work — including across worktrees, which
otherwise share the main repo's queue by default (see Queue-root resolution).

```sh
ccq send --key review "look at PR #42"   # producer → the review channel
ccq wait --key review && ccq next --key review --json   # consumer of that channel
ccq status            # default channel + a keys: [...] list of the others
ccq status --all      # every root and every channel (subkey in --json)
ccq send --no-key …   # force the root queue even if CCQ_KEY is exported
```

The default (no `--key`) queue is unchanged and backward-compatible. The
collision-guard `path.txt` stays at the root; channels never get their own.

## Message states

```
send → pending(new) → claim/next → processing(cur) → done → archived(done/, 200)
                     ↖ release / reap (owner died · PID reused) ↙
```

- reap auto-recovers **definite death** (`kill→ESRCH`) or confirmed PID-reuse; never an
  uninspectable signature, never by elapsed time (protects long-running work)
- Completed messages are kept up to `CCQ_HISTORY_KEEP` (200), then trimmed oldest-first

## Output contract

- **Human** (`list`/`status`/`root` text): the `📂 <root>` header and warnings go to **stderr**;
  stdout carries data only
- **Machine** (`--json`): JSON Lines, no header, fixed schema, language-independent
- Exit codes: `0` ok / `1` op-error / `2` usage / `3` partial-or-race / `4` empty (`next`) /
  `124` `wait --timeout`

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `CCQ_HOME` | `$XDG_STATE_HOME/ccq` → `~/.local/state/ccq` | Queue store root |
| `CCQ_ROOT` | – | Force the exact queue root (like `--root`) |
| `CCQ_KEY` | – | Default sub-key channel (like `--key`; `--key`/`--no-key` override) |
| `CCQ_WORKTREE` | – | If set, a linked worktree keeps its own queue (opt out of the default worktree → main redirect; like `--worktree`) |
| `CCQ_OWNER_PID` | auto | Explicit claim-owner pid (recommended: the long-lived session pid) |
| `CCQ_LANG` | `en` | Output language (`en`\|`ko`); `--lang` overrides |
| `CCQ_HISTORY_KEEP` | 200 | `done/` retention cap |
| `CCQ_STALE_WARN` | 43200 (12h) | `list` long-claim ⚠ threshold |
| `CCQ_POLL_INTERVAL` | 2 | `wait` poll-fallback period (`--interval` overrides) |

## Storage layout

```
$CCQ_HOME/<encoded-root-path>/             # root's / and . replaced with -
├── tmp/                                    # being written (pre-publish)
├── new/<epoch>-<id>.json                   # pending
├── cur/<epoch>-<id>.<pid>.<sig>.<claimed-at>.json   # processing (owner in filename)
├── done/<done-at>-<id>.json                # completed history
├── path.txt                                # real root path (collision guard)
└── keys/<slug>/{tmp,new,cur,done}          # sub-key channels (--key); one level only
```

`<id>` is 16 hex chars; `<sig>` is the owner's microsecond start-time (protocol 2).
The in-repo `.ccq/` **root marker** (committed) is separate from this per-machine store.

Message body: `{"id":"<id16>","ts":"<ISO8601, local TZ>","from":"<label>","msg":"<body>"}`

## Examples

```sh
ccq send -d ~/code/foo "add auth tests"            # send to another project (subpath ok)
printf 'long instruction...' | ccq send -d ~/code/foo -
ccq list                                           # inspect my project's queue
ccq --lang ko list                                 # same, in Korean
ids=$(ccq list --json | jq -r .id) && ccq claim $ids
ccq next --json                                    # autonomous dequeue
ccq done <id>                                       # complete
ccq release --force <id>                           # recover a forgotten claim
ccq wait --json && ccq next --json                 # block, then take the arrival
ccq doctor                                         # check install + legacy migration
```

Tests: `cargo test` (unit + `assert_cmd` integration). Lint: `cargo clippy -- -D warnings`.
