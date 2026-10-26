# Interactive agent dialog

Give an agent a chat-like way to talk to the operator for exploratory work, instead of the current async mail round-trips.

## Why mail is clunky for exploration

Office mail is threaded and supports back-and-forth, but it is **turn-granular and asynchronous** (`crates/zapper-office/src/mail.rs`, `foreman.rs`): a worker mails `BOARD` a question, its turn ends (it is parked), the board replies via UI/CLI, and the worker only sees the reply when it wakes on its next turn. That is right for assignments and reports, but poor for "let's explore this idea together", where you want many quick rounds.

## What exists today

- **Mail** — async, turn-granular; the real operator↔agent channel today.
- **Approvals / asks** — one-shot allow/deny (`Core::approvals` / `Core::answer`, `interface.rs:431`); not a conversation.
- **Attach-to-session terminal** — a fully interactive, real-time bidirectional PTY into the VM (`GET /sessions/{id}/attach` → `Session::open_shell`, `crates/zapper-core/src/daemon/session.rs:317`), recorded as an asciicast and broadcast live. But it is a shell *alongside* the agent, **not** an injection point into the agent's Claude conversation.
- **Mid-turn input** — `Driver::prompt` queues for the *next* turn (`crates/zapper-core/src/daemon/driver.rs:266`), so it cannot interject mid-turn. `Conversation::steer` does hand a message to a running turn, but **only pi/opencode support it — Claude Code does not** (`harness.rs:106`).

**Gap:** there is no tool for an agent to open a synchronous, chat-like dialog with the operator. The agent-facing MCP tool set (`crates/zapper-office/src/ops.rs`) is only async mail plus one-shot asks.

## Options

- **A — Live question (fast ping-pong on mail).** Add an MCP tool `ask_operator(message)` that posts a board question flagged "live"; the UI shows it in a chat thread; and the foreman fires the worker's next wake *immediately* on reply instead of parking it. Reuses mail + the record + the existing UI. Because Claude Code can't steer mid-turn, "live" means end-turn-then-rewake-on-reply — a fast ping-pong, not token streaming. Good enough for a chat feel when reply latency is low.
- **B — Dedicated chat channel over the control plumbing.** Carry an "operator chat" text stream over the proven vsock shell/terminal mechanism, surfaced as a UI chat pane, read/written by the agent as a tool. Real-time, but it lives outside the agent's Claude conversation, so the agent must treat it as a tool rather than as its own dialogue.
- **C — Harness-level steer for Claude Code.** Implement `Conversation::steer` for the Claude Code harness so operator messages inject mid-turn (as for pi/opencode). The most genuinely chat-like, but it depends on `claude -p` supporting mid-turn input — its stream-json `-p` mode is batch-per-turn, so this is likely **blocked on harness capability** without a different invocation mode.
- **D — Interactive agent session the operator shares.** For exploration, launch Claude Code **interactively** in the VM (not the headless `-p` driven loop) and let the operator attach to the same live terminal via the existing attach channel. Operator and agent then share one live Claude Code TUI — a genuine chat. Egress is still behind the proxy, and the terminal is already recorded. This fits "explore an idea with an agent" exactly, and needs essentially no new engine mechanism — it is the non-driven run path plus the existing attach.

## Recommendation

Use the two that need the least new mechanism, for the two different initiators:

- **Operator-initiated exploration → Option D.** Add a thin convenience (a `zapper` command and a UI "chat"/"explore" button) that starts an *interactive* (non-driven) agent session and attaches the operator to its live terminal. No engine changes; reuses the attach channel and recording. This is the direct answer to "I want to interact directly with an agent in a chat-like interface."
- **Agent-initiated questions inside a headless/driven run → Option A.** Add `ask_operator` / `discuss` as an office MCP tool that posts a live-flagged board question and tightens the foreman loop to re-wake the worker immediately on reply. Accept that it is turn-granular ping-pong, not streaming (the Claude Code limitation).

Keep Option C on the roadmap as the "true mid-turn chat" endpoint if/when a Claude Code invocation mode supports live input; B only if a non-conversation side-channel is specifically wanted.

## Build sketch

- **D:** reuse the interactive run path + `GET /sessions/{id}/attach`; add a UI entry point and a convenience command. Nothing new in the engine.
- **A:** a new `ops.rs` MCP tool `ask_operator`/`discuss` that drafts a board question tagged "live"; the UI renders the thread as a chat; the foreman loop re-wakes the worker on a reply to a live thread rather than waiting for the normal schedule.

## A note on persistence

A dialog's content is journaled for audit, and an interactive exploration's terminal is recorded as an asciicast — but a run itself is not resumable, and (per [proposal 01](01-persistent-outputs.md)) a firm's live state is per-run ephemeral. If you want the *conversation itself* to persist and resume across runs, that is the broader per-run-ephemerality question, not something the dialog feature solves on its own.
