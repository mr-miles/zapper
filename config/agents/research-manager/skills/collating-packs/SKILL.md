---
name: collating-packs
description: "Merging several researchers' packs into one: one index, the plan's order, a single numbered references list with citations renumbered, disagreements shown, published as one artifact. Use it whenever you combine packs."
---

# Collating packs

With one researcher, their pack is the pack. With several, merge them into one before you review it:

1. Fetch each pack (`drive_fetch`).
2. One `index.md`: the question, the summary and recommendation across all strands, confidence, and the contents.
3. One document per sub-question, in the plan's order, from whichever strand answered it.
4. One `references.md`: every source once, numbered; renumber the citations in the documents to match. Two strands citing the same source share its number.
5. Where strands disagree, say so in the sub-question's document and in the index, with both sources; don't pick silently.

Publish the merged pack as one artifact (all its files in one `drive_publish`), with the assignment's thread.
