# Legion Ascended's governance mapped onto The Office

Map the concepts of [Legion Ascended](https://github.com/9thLevelSoftware/legion-ascended) — a governed, evidence-based, human-in-the-loop workflow for AI software work (`start → plan → build → review → ship`) — onto the roles and mechanisms of a zapper firm (The Office), and say what a firm run under that discipline would look like.

> **Read from the README.** This is built from Legion Ascended's README summary, not its full source. Confirm its oracle / taskgraph / evidence schemas and the exact per-tier gate sets against the repo before acting on anything typed below.

## The structural match

Legion and The Office sit at **different layers, and they compose** rather than compete:

- **The Office supplies the actors and containment** — named roles, each a coding agent (Claude Code or pi) in its own VM, talking by mail, publishing to a shared drive, with host-enforced network / exec / secret boundaries and a tamper-evident record ([`crates/zapper-office/`](../../crates/zapper-office/), [office.md](../office.md)).
- **Legion supplies the governance spine** — a disciplined flow where nothing is "done" without **evidence**, **risk-tiered gates**, and **human approval**.

The Office today is strong on *who does the work and how they are contained*, and comparatively loose on *what makes work provably done*. Legion is the reverse. So this proposal is not "rebuild Legion as a firm"; it is "run a zapper firm that follows Legion's discipline", mapping each Legion primitive onto a role's action, a charter rule, a drive artifact, or the record. The point of contact is that both already treat **a hash-verified record as the source of truth** — Legion's `.legion/project` state, and the firm's run under `state/firms/.../runs/<id>/` with the keeper's hash chains ([security.md](../security.md#the-record)).

## Concept mapping

| Legion concept | The Office equivalent | Notes / gap |
|---|---|---|
| The `legion` CLI / the `start→ship` spine | The **charter + the shared skills** (`office-mail`, `reviewing-work`, `releases`, `checking-the-product` under [`config/skills/`](../../config/skills/)) — the process every role follows | Legion is a *process*, not a role: it becomes the firm's operating discipline, encoded in skills and the charter. |
| **Constitution** | **The charter** — fixed articles plus the board's own, read by every role in its preamble ([`prompts.rs`](../../crates/zapper-office/src/prompts.rs), [office.md](../office.md#the-charter)) | Near-exact match. |
| **Intake / draft → spec + ROADMAP** | **Board kickoff task** (the goal) → **product-manager** (`product-brief`) writes the spec → **CEO** turns it into workstreams (the roadmap) | The Office already does this; Legion adds "drafts must be explicitly accepted" before work starts. |
| **Phase** | A **workstream / assignment thread** with a deliverable and a due date | 1:1. |
| **Change bundle** | A **parent assignment and its delegated threads** — the office already groups a task by thread context ([`mail.rs`](../../crates/zapper-office/src/mail.rs), `task_threads`) | 1:1 conceptually. |
| **Taskgraph / task contract** | How a **manager splits work** (`manager` skill): each assignment is an outcome, constraints, "what done looks like", a due date | The Office's assignments *are* task contracts, just untyped. |
| **Oracle** (the check that defines correct behaviour) | The **acceptance criteria in the brief** plus the reviewer's pass/fail (`reviewing-work`, `checking-the-product`) | **Gap worth closing:** make the oracle an explicit, approved drive artifact *before* build, not just prose in the assignment. |
| **Executor** (`claude` / `codex` / `manual` / `fake`) | A **worker's agent in its VM** — Claude Code or pi, chosen per role; `fake` ≈ the fake-Claude test harness; `manual` ≈ the board doing it by hand | The Office *is* "one executor per worker"; a role's `model:` ≈ `--executor`. |
| **Evidence** | **Published drive artifacts** (versioned, content-addressed by sha256, [`drive.rs`](../../crates/zapper-office/src/drive.rs)) + git commits + the run's record (journal, transcripts, audit) | Strong match; the Office already collects this. |
| **Attestation** (a named person asserts hash-pinned files are evidence) | A **signed `report` with attached drive artifacts**, over the **keeper's per-line hash chains** ([security.md](../security.md#the-record)) | Strong match: the keeper gives Legion-style hash-pinning and re-verification; the "named person" is the worker whose VM sent the report. |
| **Approval / Approver** | **The board** (goal, budget, access an officer cannot grant) plus **role-owned sign-offs** (the PM accepts the work, the CTO/CEO commands a release, a security officer decides access) | "Approver listed in `project.json`" ≈ **who in the org chart owns that decision**, set by the charter and the chart. |
| **Risk tier (R0–R3) → gate set** | *(new)* a **per-assignment risk level** that sets which reviews and approvals are mandatory | **The biggest idea the Office lacks and should borrow — see below.** |
| **Gate** (satisfied / unsatisfied / **unevaluable**) | A **required review, clearance or approval before "done"**: a `reviewing-work` accept, a security clearance, the board's `close`, the release-to-prod command | "Absence of evidence is not satisfaction" ≈ the office refusing to `close` while work is open ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs)). |
| **Waiver** (audited "not applicable" + reason) | A **recorded decision with a reason** (the CEO or an officer logs "this gate does not apply, because …") in the journal | The Office records a reason on every decision already; a "waiver" is just a named one. |
| **Release plan** (observe + rollback, **executes nothing**) | The **`releases` skill** — ask → command → deploy → verify → rollback, incidents | **Divergence:** Legion only *records* readiness; the Office actually *deploys* to its k3s prod. See below. |
| **Semantic map** (read-only repo view; "context, not proof") | A **researcher / analyst read-only report**, or the **`process-review`** operating skill's record scan | "Verify the cited source before acting" ≈ the firm's existing rule to check claims against source. |
| **State under `.legion/project`, hash-pinned, re-verified** | **`state/firms/.../runs/<id>/`** (journal, transcripts, drive, `config.yaml`) plus the keeper's hash chains | Strong match — both treat the record as source of truth, verified on read. |
| **`retro` / `learn` / `milestone` / `council`** | The **`process-improvement` role and its `retrospective`** ([`config/agents/process-improvement/`](../../config/agents/process-improvement/)), and the **`process-review`** operating skill | Direct fit: Legion's reflection commands are exactly that role's remit. |

## The workflow as a firm run

`start → plan → build → review → ship` becomes one run:

1. **start** — The board sends the kickoff goal; the **CEO** scopes it and the **product-manager** writes the spec/brief (the *constitution* — the charter — is already in place). The spec is **published and accepted** before work begins.
2. **plan** — The CEO or a manager breaks the roadmap into **phases = assignments**, each with an explicit **oracle** (an acceptance artifact) published to the drive and approved by the owning role.
3. **build** — **Engineers (executors)** do the work in their VMs, commit to the firm's git, and **publish evidence** (results, how to check, test output) to the drive.
4. **review** — The **oracle owner** (the PM, designer or CTO, by `reviewing-work` and `checking-the-product`) evaluates the evidence against the oracle; security officers clear any access or traffic. The sign-off is Legion's `--accept --approver`.
5. **ship** — Gated by risk tier: the **CTO or CEO** commands the release (`releases`), a **security officer** confirms the gate, and the **board** gives the R3-level approval. Unlike Legion, this *deploys to prod*, so the gates are load-bearing, not advisory.

## The one real addition: risk tiers as gate policy

The Office's biggest missing piece is Legion's **risk tier → required gate set**. The proposal is to add a `risk` to an assignment (stated, or derived from the surface it touches) that the charter maps to mandatory sign-offs:

| Tier | Firm gates (who must sign off before "done") |
|---|---|
| **R0** | The engineer's own check, with evidence published. No independent review. |
| **R1** | + one independent review (`reviewing-work`). |
| **R2** (default) | + an approved oracle up front, a PM/designer review, an integration check (`checking-the-product`), and the CEO accepting the whole change. |
| **R3** | + a **security-officer** architecture/security review, **board** approval before build, the release *observed*, and **rollback evidence** on the drive. |

This is enforceable the way the Office already enforces authority: the host and charter can **refuse to let the CEO `close`** an R3 thread until the required officer clearance, board approval, and release-plus-rollback evidence exist on the record — which is exactly the shape of the existing "cannot close while assignments are open" rule ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs)), and of Legion's "a gate is unsatisfied until its evidence exists."

## Where they do not map

- **Ship executes.** Legion publishes and deploys nothing; the Office deploys to its own k3s prod. "Ship" is therefore a stronger, real action in the firm — a reason to make the R3 gates mandatory rather than advisory.
- **Human-in-the-loop versus autonomy.** Legion keeps a human on every acceptance; the Office is built to run agents autonomously with the board as an occasional approver. Adopting Legion means **deliberately inserting the board (or a designated approver role) as a hard gate** at R2/R3 — a policy choice, not free.
- **Typed, hash-pinned rigour.** Legion's oracles, typed taskgraphs, and hash-pinned attestation are more formal than the Office's prose assignments and reviews. The keeper already gives the hashing; the *typing* (schemas for specs, oracles and evidence) would be new work.
- **One repository versus a company.** Legion governs one repository's change; the Office is an organisation that can run several workstreams and products at once. Legion maps to *one firm's one product line*, not the whole org.

## What it would look like

Two shapes, not mutually exclusive:

- **(A) Legion-as-discipline (recommended first).** Keep the existing roles. Add a `risk` field to assignments, a charter article encoding the R0–R3 gate sets, an **oracle artifact kind** on the drive, and a **gate/shipping skill** for the CTO and CEO that refuses to close until the gates are evidenced. Minimal new structure, large rigour gain. Change points: the firm config and charter ([`prompts.rs`](../../crates/zapper-office/src/prompts.rs)), the mail/close path ([`mail.rs`](../../crates/zapper-office/src/mail.rs)), a new skill under [`config/skills/`](../../config/skills/), and the drive's artifact kinds ([`drive.rs`](../../crates/zapper-office/src/drive.rs)).
- **(B) A role-per-stage firm.** A dedicated chart that mirrors the workflow: *Intake/Spec* (product-manager), *Planner* (CTO/manager), *Implementers* (engineers = executors), *Oracle/Reviewer* (a QA or designer role), and *Gatekeeper/Release* (the CTO plus a security officer), with the **board as Approver** and the **process-improvement** role running Legion's `retro`/`council`. The risk tier decides how many of these must sign off.

The repo's legacy "49 personas / 33 skills" surface is optional and CLI-ignored, so this proposal maps the **core model** rather than importing personas 1:1; cherry-pick a persona only where a gate (for example an independent security reviewer) wants a role the firm does not already have.

## Artifact store and typing

Legion bundles specs, oracles, taskgraphs and evidence into the code repository (`.legion/project`), as [OpenSpec](https://github.com/Fission-AI/OpenSpec) bundles specs and changes into `openspec/`. For a firm that spans repositories this is the wrong shape: cross-repository changes have no single home, evidence bloats git history, the governance lifecycle entangles with branch/merge mechanics, and repository write implies governance write. Keep the format, invert the storage. (OpenSpec detail here is from secondary sources — confirm against its repo.)

- **Specs and oracles** — an **OpenSpec-compatible** Markdown dialect (RFC 2119 `MUST/SHOULD/MAY`, `GIVEN/WHEN/THEN` scenarios, the delta / `propose → apply → archive` shape), so agents already fluent in it work and `openspec validate` can lint the artifacts — but stored as `spec` and `oracle` artifact kinds in the firm's **shared drive**, not in the code repository.
- **Evidence and attestation** — new typed artifact kinds the drive and keeper already have the machinery for: content-addressed by sha256 ([`drive.rs`](../../crates/zapper-office/src/drive.rs)), hash-chained and re-verifiable ([the record](../security.md#the-record)), attested by a worker/approver identity under grant-gated, officer-owned authority ([`access.rs`](../../crates/zapper-office/src/access.rs)) — so "nobody attests their own evidence" falls out of the same rule as "nobody decides their own access request".
- **Reference out, not bundle in** — a store artifact names the repositories and commits it governs by `repo@sha` (and the tickets it came from), rather than living inside one repo. One store per firm/portfolio points at many repositories: this is [proposal 01](01-persistent-outputs.md)'s durable, central drive used as the governance store.

This leaves a three-layer split, with the code repository holding none of the governance:

| Layer | Lives in |
|---|---|
| Code | the git repository (or repositories) |
| Workplans / taskgraph / task contracts | zapper **assignments + threads + desk**, where a "task" is a delegation tree ([`mail.rs`](../../crates/zapper-office/src/mail.rs) `task_threads`, [`drive.js`](../../crates/zapper-office/src/ui/drive.js)) — it plays the taskgraph's *role* (ownership, decomposition, due dates, open/closed lifecycle) but not its *shape* (see note). Legion writes these as repo files; the firm does not |
| Spec + oracle + evidence + attestation + verdicts | the **central store** (drive + keeper): OpenSpec-format for spec/oracle, the firm's extension for the rest |

Store entries bind the three by referencing `repo@sha` and ticket ids. So **workplans stay tickets, code stays in the repo, and only the durable governance artifacts are typed and stored** — cleaner than bundling, and mostly machinery the firm already has. The residual OpenSpec question is not *whether* to reuse its format (yes) but whether the server-side validator embeds the Node tool or ports its checks into the daemon (see below — the validator must be shared with the agents' self-lint).

One caveat on the middle row: zapper's assignments are not a taskgraph, only its role. A "task" is a *tree* of delegated assignments (parent → children by thread `context`), prose rather than typed, with ownership, due dates and an open/closed lifecycle but **no first-class dependency edges, no typed task contract, and no typed link to an oracle** — ordering between sibling tasks is coordinated through mail and `waiting`, not modelled. For most firm work that is enough and is the right place to leave it. If a phase needs real taskgraph fidelity (a DAG with dependencies and per-task oracle links and verdicts), that is a deliberate addition — either type the assignments into task contracts, or keep tasks light and only type the durable governance (spec/oracle/evidence/attestation), letting the existing coordination layer carry decomposition. The proposal assumes the latter unless a tier demands otherwise.

## An MCP interface to the store

Agents reach the store the way they reach mail and the drive today: office MCP tools, served per-VM over the gateway and recorded as `mcp` events ([`ops.rs`](../../crates/zapper-office/src/ops.rs), [`gateway.rs`](../../crates/zapper-core/src/daemon/gateway.rs)). The rule, as everywhere in zapper, is that **the guard is on the host, not in the agent**: an agent proposes; the daemon validates and decides.

| Tool | Does | Enforcement |
|---|---|---|
| `store_validate(kind, doc)` | Lint/validate a document against its schema — read-only, no write | Convenience only; runs the *same* validator the write path uses |
| `store_put(kind, doc)` | Create or revise a `spec`/`oracle`/`evidence` draft | **Re-validates server-side** (schema + referential integrity: refs resolve, ids stable, hashes re-verify) and rejects the whole write with a structured error list if invalid — a document the agent *thinks* is valid cannot land unless it is |
| `attest(change, requirement, evidence, note)` | Assert that hash-pinned evidence satisfies a requirement | Checks the attester is allowed and **is not the author**, that the evidence resolves and re-verifies, and records it |
| `gate_status(change)` | Read the derived verdicts (satisfied / unsatisfied / unevaluable) and what each gate still needs | Verdicts are **derived server-side** from evidence + attestations + risk tier; an agent cannot assert "done" |
| `archive(change)` | The controlled `applied → archived` transition that folds the spec deltas into the live spec | A single **atomic, idempotent, recorded** transition the daemon performs under preconditions (state is approved/applied, every required gate satisfied or waived); the agent never writes files or moves state itself, so it cannot archive early or corrupt the step |
| `waive(gate, reason)` | Record an audited "not applicable" | Approver/officer only; a reason is required; recorded |

Three properties do the work the question asks for:

- **Self-lint that cannot drift from enforcement.** `store_validate` and `store_put` call **one validator**; the lint is advisory, the write re-runs it authoritatively. Agents can check themselves freely, and an invalid document is still refused on write — the agent's judgement is never the gate. (This is why the validator must be shared: a separate linter would drift and give a false pass.)
- **A host-enforced state machine.** Documents move `draft → proposed → approved → applied → archived`; each tool is a transition with preconditions checked on the host, exactly as the office already refuses to `close` while work is open ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs)). The agent can request transitions, not skip or forge them.
- **Atomic, immutable, recorded writes.** Artifacts are content-addressed and immutable — revisions are new versions, as the drive already works — so `archive` is all-or-nothing and idempotent (re-archiving is a no-op, not corruption) and lands on the keeper's chain. A half-written transition can't exist because the record appends atomically.

Concretely this is a new set of office tools plus a shared validator and a small state machine — new logic, not new transport: it reuses the MCP gateway, the drive for content, the keeper for hashing and attestation, and grants for authority.

### Read as files, write as MCP

Repo-local OpenSpec earns its keep one way the proposal must not lose: the agent can `grep`/`rg` the docs, read them, and pipe them into its own scripts — workflows it is well trained in. That is a *read* concern, and reads cannot corrupt the store or the archive step, so serve them as files even though writes go through MCP:

- **A read-only, grant-scoped mirror of the store inside each VM** — e.g. `/store`, materialised by the daemon and kept fresh, holding only the spaces this worker may read. The agent greps and scripts against it exactly like repo-local OpenSpec. This is the mechanism the firm already uses for the read-only skills mount (`~/.claude/skills`, "so a worker can't edit its instructions", [office.md](../office.md#agents-and-skills)) and the read-only host source (`/run/sandbox/source`); a store mount is the same pattern.
- **All writes and transitions go through the MCP tools.** The agent authors a spec/oracle as a file in its own workspace — grep and edit it freely — then submits it; it never writes into `/store`, which is read-only and enforced as such.

A central mirror is in fact *better* for grep than repo-local bundling in the multi-repo case: all repositories' governance sits in one tree, so a cross-repository `rg` just works, which scattered per-repo files cannot give. If git ergonomics are wanted specifically, the same content can be served as a read-only git mirror at `git.<firm>.internal/store.git` (the firm already serves git); it is a projection, never authoritative.

**No in-VM CLI**, to keep the sandbox surface minimal. A `store` binary is not shipped; three things already cover what it would have done:

- **Reads and search** — standard `grep`/`rg` over the read-only mount; no new binary.
- **Writes** — the MCP tools, which the model calls natively, so a governance write is a deliberate, recorded, validated tool call rather than a side effect in a shell script.
- **Lint and validate** — the read-only `store_validate` MCP tool is the linter; the agent iterates against it, and `store_put` re-validates server-side regardless, so skipping the self-lint still cannot push an invalid document.

A **skill** carries the rest: the OpenSpec-compatible format and the workflow (author a file → `store_validate` → `store_put` → `attest` → `archive`), pointing the agent at `/store` to read. So **skill + MCP validation + server-side enforcement replace the CLI**, and the only thing given up — writing to the store from inside an agent-authored shell script — is something governance wants to forbid anyway.

## Per-project and per-workflow governance

Legion is built for software, so its schema, spec and ticket formats are rigorous throughout. A firm may do anything — research, analysis, design, operations — and may not want that formality, particularly around tickets. So rigor must be a **dial, not a firm-wide law**, set the way zapper already layers everything. Legion's own R0–R3 tiers are the precedent (R0 is already "minimal"); this extends the dial down to "no typed spec at all" and makes the schemas themselves swap by project.

Bundle the stack into a named **workflow profile** — which artifact kinds exist and their schemas (as data, a `schemas_dir` read like `skills_dir` / `agents_dir`), which risk tiers and gates apply, and which skills attach:

```yaml
workflow_profile:
  software:                        # Legion-style
    artifacts: { spec: openspec, oracle: openspec, evidence: typed, attestation: typed }
    risk_tiers: [R0, R1, R2, R3]   # R2 default
    tickets: prose                 # assignments stay prose even here
    skills: [spec-authoring, shipping-gates]
  research:                        # light
    artifacts: { findings: pack }  # a brief and cited findings; no oracle
    risk_tiers: [none, light]
    tickets: prose
    skills: [research-sources]
  freeform: {}                     # today's behaviour: untyped drive, prose assignments
```

**No profile means exactly today's behaviour** — free-prose assignments, an untyped drive, review on request — so the rigor is strictly additive and opt-in.

Select it at the three layers the firm already has:

- **Per firm (project):** `firms.<name>.workflow: software` in the config is the default; a research firm sets `research` or omits it.
- **At kickoff (per run):** the board picks a profile on the start form as it picks a scenario today; the chosen profile's rules fold into the run's **charter** ([office.md](../office.md#the-charter)), so every role reads the governance level in its preamble.
- **Per phase (per assignment):** the CEO or product manager tags a workstream with a profile (or just a risk tier), so a software firm's one-off research spike drops to `research`, and a research firm's ship-critical component rises to `software`.

Enforcement stays uniform: the store's MCP tools validate against **whatever schema the active profile declares, or pass through untyped if none** — the validator is generic, the profile is the data it checks against, so the same tools serve a rigid and a loose firm alike.

Keep the axes independent, especially tickets:

- **Governance axis** (spec / oracle / evidence / attestation / gates): off → full, per profile.
- **Ticket-typing axis** (assignments: prose → typed task contracts): **default off, rarely on.** Even the `software` profile above keeps `tickets: prose`, because zapper's assignments are a prose delegation tree and forcing them into typed task contracts was the weakest part of the mapping. A workflow turns ticket-typing on only when a phase genuinely needs a dependency DAG. So adopting the governance rigor never imposes rigor on tickets: a firm can run fully typed specs with entirely free-prose assignments.

So whether a layer applies is config / kickoff / a per-phase tag; what the schemas are is `schemas_dir` data the profile names; how to work under it is the skills the profile attaches; and enforcement is the generic store validator plus the risk-tier gate map, both parameterised by the active profile — config, data and skills, not a new engine.

## Tickets, stages and the taskgraph

The taskgraph is the one place to resist over-typing: zapper's tickets should keep prose content and gain only the *structure* a taskgraph needs, all optional (default absent = today's free delegation). Two pieces:

- **Hierarchy — already exists as `context`.** A thread carries `context` (a single parent thread), set automatically to the thread its sender was working in, and `task_threads` ([`mail.rs`](../../crates/zapper-office/src/mail.rs)) already walks it to roll a task up for grouping and the objective's close. So the decomposition tree and its roll-up are built; the only addition worth considering is making `context` *declarable* (not only auto-derived) for up-front planning and re-parenting. A separate `child_of` field would duplicate it.
- **Ordering — an ordered list of stages on the parent (the one new field).** The parent ticket carries `stages: [[id, …], …]` — ordered, each stage a set that runs in parallel. It is readable (the whole plan sits in one place), it matches authority (ordering is a planning decision owned by the parent's owner, not declared piecemeal by children), it is naturally scoped (it references only its own children), and list-of-stages expresses sequence plus fan-out, which covers most plans — Legion's phase flow included. The office releases stage N+1 only when every ticket in stage N is *accepted* (the review close, not a bare report).

Cross-subtree dependencies — a ticket depending on one outside its parent's stage list, a true DAG diamond — are deliberately **out of scope for now**: the stage list covers sequence and parallelism within a decomposition, which is the common case, and a general dependency-edge mechanism (an optional `not_before` by id) can be added later if a workflow needs it. Note the stage list is only the *authoring* surface: stored, it lowers to Paperclip-style `blocks` relations (see *Shaping the plan on Paperclip and Legion* below), so the ordering is edge-shaped underneath even though the planner writes only stages, and a later cross-subtree dependency is just another `blocks` row.

This reproduces the common taskgraph shape — decomposition, ordering, parallelism — with content left prose, so typed contracts and oracle links stay in the governance layer, separate from the ticket graph.

### Authoring a plan

An agent lays out the whole plan in one atomic call rather than drip-sending assignments, because a ticket must be able to exist *before* it is delivered — later stages are created blocked, not pushed to their workers yet. A **`plan` MCP tool** takes the parent (its current task by default) and the ordered stages, each stage a list of assignment specs — a `slot` naming which team member does the ticket (in place of `to`; see below) and the fields `mail_send` already takes — `subject`, `body`, `due`, `cc`, and `attachments` (shared-drive references like `drive:a-3@v2`, up to 16, each validated to resolve and be readable by the recipient). The office validates the whole plan (recipients assignable under the chart, each child in one stage, attachments resolve and are readable, within limits), creates every child with `context` set, activates stage 0 (delivers and wakes those recipients), and holds the rest blocked.

Attachments are references to already-published drive artifacts, so a plan can attach only documents that exist when it is authored. A later stage whose input is an earlier stage's not-yet-produced output does not pre-attach it: that output is published to the task's drive and read at run time when the stage's worker wakes (the drive is already grouped by the task through `context`). Carrying an accepted stage's deliverables forward as attachments on release is a possible enhancement, out of scope for the base design.

A plan is a schedule of *work*, not a fixed roster — the office boots with only its CEO and hires or contracts during the run, and a worker is a 4–6 GiB VM, so a plan must not force every worker to exist from the start and idle until needed. So the plan declares a small **team of slots** and each ticket names a slot rather than a worker. A slot is a role to staff (`dev#1: {role: dev}`), optionally pinned to a name (`dev#2: {name: bertie}`), given a budget, and flagged **`release_when_done`** (below); the office binds it to one worker the first stage that uses it — an existing worker for a name, a hire for a role, or, for bounded late work, a **contract** — reuses that worker for the slot's later tickets, and releases it after its last. A slot referenced only in the final stage is thus staffed only then, and a contractor's VM stops when it delivers, so nothing idles.

Two properties fall straight out of slots, where role-only staffing gives neither. **Same slot, same worker:** `dev#1` building A in stage 1 and testing B in stage 2 is one person, so knowledge carries across the plan instead of a stranger being re-bound each time. **Distinct slots, distinct workers:** the office binds different slots to different people, so separation of duties is structural — no exclusion constraint to state. A build-and-cross-test plan is just:

```
team:    dev#1: {role: dev}    dev#2: {role: dev}
stage 1:   Build A (dev#1)       Test A (dev#2)
stage 2:   Build B (dev#2)       Test B (dev#1)
```

each feature tested by the other's developer. The costs to weigh: N distinct slots of a role commit the firm to N distinct workers, and a slot whose uses are non-contiguous keeps its worker alive through the gap or is re-staffed. That choice is explicit, a per-slot **`release_when_done`** flag rather than the planner's unstated habit: set, the office frees the worker's VM whenever the slot has no open ticket and re-staffs it when next needed (reclaims RAM, at the cost of a fresh worker and another background check); unset, the worker is held from first use to last, idle through any gap, keeping its context. Contiguous slots are unaffected either way.

A slot may carry a **budget**, so provisioning is funded from the plan and the planner need not be woken for the hand-off at all: within the board-approved envelope the office hires/contracts and funds at release (this is the CEO's existing `budget()` plan — role, title, budget each — made executable). The residuals wake someone other than the planner: over the envelope asks the board; a new worker still wakes a *security officer* for the background check (the invariant holds); a contract offers at the slot's budget and escalates only on an overrunning quote. The planner is then woken only for work it personally owns — a review it must do, a board report. Make auto-provisioning opt-in per slot, since auto-spend is an autonomy choice an operator may want to gate.

A general **`distinct_from: [ticket-ids]`** exclusion — an assignee that must differ from those tickets' assignees, resolved at staffing — remains the fallback for the rare constraint that crosses the slot scheme or crosses plans; with slots covering same-and-different within a plan it is a deferred escape hatch, like `not_before`. As each stage is accepted the office releases the next and wakes its workers; the planner is not in the loop for the hand-offs. `mail_send(assignment)` stays for one-off delegation — the degenerate one-ticket, one-stage plan.

The engine pieces this needs: a **deferred/blocked assignment state** (created, not delivered, not waking its worker until released), a **staged-release scheduler** (release stage N+1 when stage N is accepted, reusing the existing accept signal and wake), **slot binding** (bind a slot to a worker at first use, reuse it, release after its last stage, distinct slots to distinct workers), **atomic batch validation**, and desk/UI visibility of the plan. Everything else — creating assignments, `context`, obligations, acceptance, waking — already exists.

A **planning skill** teaches the manager / CEO / product manager to decompose the task into a team of slots and ordered stages with per-ticket acceptance criteria and call `plan` once, to reuse a slot where knowledge should carry and use distinct slots where duties must be separated, to keep stages minimal, and that later stages auto-release on acceptance so they must not be chased by hand.

## Shaping the plan on Paperclip and Legion

Two forward constraints make the small `plan` changes worth modelling deliberately: the result should be **swappable for Paperclip** (so a firm can later run Paperclip's orchestrator over zapper's sandbox, [proposal 02](02-paperclip-integration.md)), and it should **cover what Legion requires** in these areas (so Legion's skills and roles can be reused unchanged). Both are met by shaping the Office's plan/ticket model on Paperclip's schema (read at `df5a78c`), which already persists the Legion-shaped pieces. The agent's experience barely changes; what changes is the *stored shape*.

| Office element | Model it as (Paperclip table) | Covers (Legion) |
|---|---|---|
| Ticket | an `issue`: `parent_id` (= `context`), `assignee_agent_id` (= the slot's bound worker), `status`, `priority`, `review_policy` | a task |
| Ordering | `issue_relations` rows, `type: "blocks"` — the stage-list is authoring sugar that compiles to blocks-edges | the taskgraph's edges |
| A plan | an `issue_plan_decomposition`: a source ticket + `requested_children` specs → `child_issue_ids`, with a status | a phase / change bundle |
| Staged release | an `issue_tree_hold` on the plan's subtree with a `release_policy` (free the next blocks-frontier as its blockers are accepted) | gated progression |
| Per-ticket governance | an optional `completion_contract`: `risk`, `acceptance` (the check), `completion_authority`, `incomplete_criteria_policy`, hash-pinned | oracle + risk tier + gate + attestation |
| Review outcome | a `work_assessment`: the verdict against the contract, with an input digest | review / evidence |

The small deltas to the Office, then:

1. **Store ordering as `blocks` relations, not only a stage-list.** The planner still writes stages (readable, parent-owned); `plan` lowers them to blocks-edges between child tickets. The stored ordering is then Paperclip's shape, and Legion's DAG maps onto it directly.
2. **Make `plan` emit a decomposition.** The call records a decomposition (source ticket, the child specs requested, the child ids created) and creates the children under the source — the shape of `issue_plan_decompositions`.
3. **Model staged/blocked release as a tree-hold**, not an ad-hoc per-ticket flag: a hold on the plan's subtree whose release policy frees each blocks-frontier as its blockers are accepted — `issue_tree_holds`.
4. **Allow an optional `completion_contract` per ticket** — `{risk, acceptance, completion_authority, incomplete_criteria_policy}`, hash-pinned — carried only when the workflow profile turns governance on (off by default). This one object gives Legion its oracle/risk/gate and matches Paperclip's contract field-for-field.
5. **A review records an assessment** against that contract (accept/reject, reason, digest) — the Office's reviewing-work close in `work_assessments` shape.

Slots stay an Office **authoring** convenience with no Paperclip counterpart: they resolve to a concrete `assignee_agent_id` per ticket, so on a swap they simply dissolve into assignments (continuity = the same agent id, separation = different ids), and `release_when_done` maps to Paperclip's agent `lifecycle_state`/holds. The skills — a `planning` skill and an `acceptance`/oracle skill — are written in Legion's vocabulary (spec, oracle, risk, task) so Legion's own skills and roles drop in later. So "replace the Office's plan engine with Paperclip" becomes a field remap, not a redesign, and "use Legion's planner/reviewer skills against the Office" works because the artifacts they expect are the ones the Office now stores.

## How far config-only gets you

Much of this proposal needs code (the store, the MCP tools, typed validation, risk-tier verdicts). But a useful subset is reachable **today with config alone** — a firm definition, roles ([`config/agents/`](../../config/agents/)) and skills ([`config/skills/`](../../config/skills/)) — because the dividing line is *soft* versus *hard* enforcement: skills and the charter can instruct the workflow, but only the office can refuse. The verdict: config-only buys a convincing Legion-*shaped* firm and most of the *process*; code buys the *guarantees*.

What config-only gives you:

- **The choreography** — product-manager (spec), CTO/manager (plan), engineers (build), a reviewer, security-officer (the R3 gate) and process-improvement (retro) are all roles and briefs; `reviewing-work`, `checking-the-product`, `releases` and `retrospective` already exist as skills, and a `spec-authoring` / `oracle` / `shipping-gates` skill is more of the same.
- **Spec, oracle and evidence as drive artifacts** — published, versioned, immutable, attached to the assignment thread. Not typed or validated, but present and on the record.
- **Human approval** — the board, plus the role sign-offs the charter names as owners.
- **A tamper-evident record** — the keeper (config plus a one-time install) gives hash chains and re-verification, so a signed `report` with attached artifacts is attestation in all but the typed form.
- **The gates the office already enforces**, which happen to match Legion's philosophy: no closing the objective while work is open ([`mail.rs:577`](../../crates/zapper-office/src/mail.rs); ≈ "absence of evidence is not satisfaction"), nobody decides their own (≈ independent review), officer-first and security review (≈ the R3 security gate), and prod only on command (≈ the ship gate).

Two gates can even be made *hard* with no code, by structuring them from existing host-enforced mechanisms:

- **An un-skippable review** — make the review its own **assignment** in the task tree; the office refuses to close the phase while a sub-assignment is open, so "no ship without review" holds mechanically.
- **Approved-before-build** — put approved specs in a drive **space** whose publish grant only the approver holds; engineers can read but cannot fabricate the approval, and grants are enforced on the host.

What still needs code (the rest of this proposal): server-side **validation** ("can't push an invalid document"), the **typed store / MCP tools / read-only mount / archive state machine**, **risk-tier derivation and typed verdicts**, and **typed hash-pinned attestation** (the keeper is the substrate, the typed object is not). And the natural-language **articles the board adds to the charter are instructional, not mechanically enforced** — so a new gate expressed only as an article is soft until it is code, or structured as an assignment or a grant.

So a "Legion-lite" firm is definable today: the roles and skills above, charter articles for the discipline, the keeper on, and the two hard gates built from sub-assignments and space grants. That is most of the value for a team whose agents follow their skills; the guarantees — valid typed artifacts, un-skippable typed gates at every tier, the cross-repo store — are the code.

## Recommendation

Do **(A) first**: it is a small, enforceable change that gives the firm Legion's main strength — evidence-backed, risk-tiered gating with explicit approval — while keeping the roles, mail and containment the Office already has. Reuse the **keeper** for attestation (it is Legion's hash-pinning, already built), make the **oracle** an approved drive artifact so review has something typed to check against, store the governance centrally (one store across repositories, not bundled into each repo), serve it as a **read-only mount for grep and scripts with writes through MCP tools** (host-side validation, a controlled archive transition, and no in-VM CLI — skill plus validation carry authoring) so agents can self-lint but can neither push invalid documents nor corrupt archiving, and route Legion's reflection commands through the existing **process-improvement** role. Make the whole stack a selectable **workflow profile** that defaults to off (today's behaviour), so software firms get the rigor and other firms opt in per project or per phase, with ticket-typing left off even when governance is on. A config-only **"Legion-lite"** firm (the roles and skills above, charter articles, the keeper on, and review-as-assignment and space-grant gates) is available as **step 0, before any code** — it is most of the process and some real gates; the code then adds the guarantees. Treat **(B)** as a later option for a firm whose whole purpose is governed delivery.

## Open questions

- **Where does `risk` come from** — stated by the board/CEO per phase, or derived from the surface and protected paths an assignment touches (Legion derives it)?
- **Who owns the oracle.** The product-manager writes acceptance criteria today; an R2+ oracle wants an owner distinct from the implementer, as Legion insists.
- **Double approval.** The firm already has asks/grants and the board; adding Legion-style approvers must not create two overlapping approval systems — decide which decision each owns.
- **Typed artifacts and the store.** Addressed above (OpenSpec-compatible spec/oracle, zapper-store evidence/attestation, served via MCP). The residual is the shared validator: embed the OpenSpec Node tool, or port its checks into the daemon so self-lint and the write path cannot drift.
- **Taskgraph fidelity.** Addressed above ("Tickets, stages and the taskgraph"): `context` for the tree, an ordered stage-list on the parent for ordering (released on acceptance), authored by a `plan` tool, content left prose. Residual: whether to make `context` declarable; the cost of the deferred/blocked assignment state in the foreman; and whether cross-subtree dependency edges (`not_before`) are ever needed — out of scope for now.
- **Profile definition and precedence.** Where workflow profiles live (a `workflows_dir` read like `skills_dir`), and the precedence when a per-phase tag, a kickoff choice and a firm default disagree (most-specific wins, presumably) — and whether a phase may freely mix axes (typed spec, prose tickets, light gates).
