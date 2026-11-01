---
name: reviewing-work
description: "How to review someone's work and how to answer a review: check it against what was asked, with evidence, and send back one numbered list. Use it whenever you accept, reject or comment on a colleague's deliverable, or yours comes back with findings."
---

# Reviewing work

A review decides whether work is done: it either accepts the work or says exactly what to change. It costs the author a turn either way, so make one review count.

## Against what was asked

- Review against the assignment's criteria, the brief or spec, or the plan it follows, not your own taste. Name the version you reviewed: a commit, or the file and when.
- Check it yourself: run the software, follow the README from a fresh start, open the sources, walk the flows. A report that claims success without saying how it was verified: ask for the evidence.
- A numbered checklist of the criteria, each pass or fail, with what you saw.

## Reviewing code

Besides the criteria, go through the change in these passes. Each finding gets the review's severity: **blocks** acceptance, **should fix**, or **polish**.

1. **It builds and runs.** It builds from a clean checkout by the README, its tests pass, and nothing generated or secret is committed.
   - Blocks: it doesn't build, or a test fails. Should fix: a step the README leaves out. Polish: a warning nobody would miss.
2. **Types and interfaces.** Inputs are validated at the boundary, types say what the values are (no strings for everything, no casts that hide a mismatch), and public interfaces are what the spec says.
   - Blocks: an interface that differs from the spec, or input trusted without a check where it matters. Should fix: a loose type where a precise one was easy.
3. **Patterns and state.** It follows the patterns the codebase already uses; each part does one job behind its own interface; state is owned in one place, and side effects aren't hidden in functions that look pure; nothing is written twice.
   - Blocks: shared mutable state that can race or corrupt. Should fix: a second way of doing what the codebase already does one way, duplicated logic, a function mixing levels (parsing, policy and I/O in one). Polish: a name that misleads.
4. **Errors.** No failure is swallowed: each is handled, or passed up with what failed and on what; calls to other services handle their failures (timeouts, errors, the network); messages say enough to act on.
   - Blocks: an error ignored on a path that matters. Should fix: a message that says "error" and nothing more, a missing timeout.
5. **Dead code.** No commented-out blocks, debugging output, unreachable code, or unused exports and dependencies; no `TODO` without a reason.
   - Should fix: commented-out code, an unused dependency. Polish: a stray debug line in a path nobody uses.
6. **Tests.** Every new behaviour has a test that would fail without it; error paths and edge cases (empty, missing, the limits) are tested, not just the happy path; no test was deleted or weakened without a reason; each test's name says what it checks.
   - Blocks: new logic the criteria depend on with no test. Should fix: only the happy path tested. Polish: a test name that says "works".

**Examples**, by language: where there is a file for the language you review, `examples/<language>.md` beside this skill's `SKILL.md` (`examples/rust.md`, `examples/typescript.md`), read it first; each example names its pass and severity. The project's own conventions, where it keeps them, are in the repository you review.

## The findings

- **One numbered list**, most important first: what, where, expected, what you saw, and how to reproduce it. A severity on each: blocks acceptance, should fix, polish.
- Only what matters for the criteria; nits marked as such, or left out.
- Fix it yourself only when the team agreed you would, and say what you changed.
- **Accept** briefly: "Approved at <version>", and anything left for later.

## Answering a review

- Fix what it found, in order, and reply in its thread with the new version and what changed per item. An item you disagree with: say why, once, and let whoever asked for the work decide.
- A change after an approval needs the reviewer to look again.

## Do not

- Accept what you haven't checked.
- Send findings one at a time, or ones without a way to see them.
