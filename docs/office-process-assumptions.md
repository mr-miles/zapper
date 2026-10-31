# office-process build: assumptions to review

Working notes from the unattended build of [proposal 13](proposals/13-office-process.md)
on `feat/office-process`. Each is a choice made where the proposal or the code left
something open; review and correct as needed. Delete this file before the PR merges.

## §4 — decider socket

- **Timeout 10s**, on no answer or an error the request is refused as a denial (per the
  proposal). Constant `ASK_TIMEOUT` in `deciders/socket.rs`.
- **`allowed` notes are fire-and-forget**: `SocketDecider::allowed` spawns a task to send
  the note and does not wait, so a slow/absent decider never holds a request its rule
  already allowed. Cost: if the send fails (socket gone mid-run), a *once-only* standing
  approval might be used more than once. Low severity, only on socket failure.
- `MAX_MCP_LINE` (128 KiB) reused as the decider reply bound (it is the generic line cap
  in `zapper-proto`).

## §5 — named layers attached by labels

- **One active run per firm at a time.** A role's layer is the core named layer
  `<firm>/<role>`, reset to empty at run start. Two concurrent runs of the *same* firm
  would share the layer. If concurrent same-firm runs are possible, key the layer (and the
  `firm-role` label value) by run id instead.
- **`firm-role` label value = `<firm>/<role>` = the layer's name.** Attachment is a single
  key lookup per the fixed precedence slot (session own → firm-role → profile → global).
- **Only the `firm-role` slot is implemented.** The proposal also notes a firm-wide layer
  (`firm` key) falls out "for free"; not added (the office does not use one today). Adding
  it needs a scope for the firm slot (`Scope::Firm`, or a generic `Scope::Layer`).
- **`RuleKeeper` is kept**, now reading the same Arcs the core vends, so `zapper rules`
  still lists role layers. The proposal has it "go" in favour of the core listing named
  layers natively (with §2 rule events); left as a later cleanup to keep the migration
  behaviour-preserving.

## §9 — record check through the interface

- The foreman gates on **`Core::record_writable()` = `record::healthy()`** (the cheap
  atomic), not `record_status()["healthy"]` (which, with a keeper, also does a `hello()`
  round-trip). Chosen to avoid a keeper round-trip on every turn-gate / idle loop; matches
  the prior office behaviour exactly. If the stricter "keeper answers now" check is wanted
  at the gate, switch to parsing `record_status()` (accepting the per-gate round-trip).
