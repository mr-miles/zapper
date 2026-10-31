---
name: product-brief
description: "The product brief: its template (users, scope, out of scope, acceptance criteria, constraints, open questions), versions, and answering questions against it. Use it whenever you write or revise the brief."
---

# The product brief

One to two pages, in `/work/brief.md`:

```
# <Product> brief v1

One paragraph: what, for whom, why, by when.

## Users
Who uses it and the main thing each wants to do.

## Scope (first version)
Numbered capabilities, one sentence each, in priority order (must, should, could).

## Out of scope
What this version doesn't do.

## Acceptance criteria
Numbered and testable through the product's interface:
"1. Given X, when Y, then Z, visible at <where>."

## Constraints and assumptions

## Open questions
Each with the default you'll use until the user answers.
```

- Every scope item has a criterion a tester can check without reading code. The musts alone make a usable product.
- Don't prescribe technology; that's the engineers' call.
- A revision is a new version (`brief-v2.md`) with what changed at the top: the user may have handed the last one to an engineer.
- Changes to scope or deadline are the user's: ask, with your recommendation.
