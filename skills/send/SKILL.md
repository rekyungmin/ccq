---
name: send
description: Send a message into another project's ccq queue. Use when the user wants to send, hand off, forward, or queue work to another project, folder, or session — even without saying "ccq". (한국어 — 다른 프로젝트·폴더·세션에 작업 보내기/넘기기/큐에 넣기)
argument-hint: "[project-path] [message]"
allowed-tools: Bash(ccq *)
---
<!-- ccq-package: ccq -->
<!-- ccq-skill-version: 0.0.1 -->
<!-- ccq-min-cli: 0.0.1 -->
<!-- ccq-protocol: 2 -->

# ccq message send

Use `ccq send -d <project-path> "message"` to append a message to the target project's queue.
The receiver is a session (any agent) running now or later in that project, which processes it via `/ccq:listen`. A subpath resolves to the project root, so `-d` may point anywhere inside the target.

## Workflow

1. Determine the target project path. If ambiguous, confirm with the user (sending to the wrong place makes another session act on it).
2. Compose the message as a **single self-contained instruction**:
   - The receiving session has none of this conversation's context — include the background it needs inside the message.
   - Use absolute file paths.
   - One task = one message (call ccq multiple times for multiple tasks).
3. Run: `ccq send -d <path> "message"`
   - Add `--from <label>` if the sender should be distinguishable (default: user@host).
   - For lots of newlines/special characters, pipe via stdin: `printf '%s' "..." | ccq send -d <path> -`
4. Report the result (pending count) and tell the user the receiving side processes it with `/ccq:listen`.

If ccq is not installed (`command -v ccq` fails), point the user to the plugin's `install.sh`.
