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

## Recommendation

Do **(A) first**: it is a small, enforceable change that gives the firm Legion's main strength — evidence-backed, risk-tiered gating with explicit approval — while keeping the roles, mail and containment the Office already has. Reuse the **keeper** for attestation (it is Legion's hash-pinning, already built), make the **oracle** an approved drive artifact so review has something typed to check against, and route Legion's reflection commands through the existing **process-improvement** role. Treat **(B)** as a later option for a firm whose whole purpose is governed delivery.

## Open questions

- **Where does `risk` come from** — stated by the board/CEO per phase, or derived from the surface and protected paths an assignment touches (Legion derives it)?
- **Who owns the oracle.** The product-manager writes acceptance criteria today; an R2+ oracle wants an owner distinct from the implementer, as Legion insists.
- **Double approval.** The firm already has asks/grants and the board; adding Legion-style approvers must not create two overlapping approval systems — decide which decision each owns.
- **Typed artifacts.** How far to go towards Legion's typed spec/oracle/evidence schemas versus the firm's current prose-plus-attachment artifacts.
