---
name: collating-packs
description: "Merging researchers' packs into one, reviewing it against the plan, and reporting: one index, the plan's order, a single numbered references list with citations renumbered, disagreements shown, versions after review. Use it whenever you combine, review or report a pack."
---

# Collating, reviewing and reporting the pack

## Collating

With one researcher, their pack is the pack. With several, merge them into one before you review it:

1. Fetch each pack (`drive_fetch`).
2. One `index.md`: the question, the summary and recommendation across all strands, confidence, and the contents.
3. One document per sub-question, in the plan's order, from whichever strand answered it.
4. One `references.md`: every source once, numbered; renumber the citations in the documents to match. Two strands citing the same source share its number.
5. Where strands disagree, say so in the sub-question's document and in the index, with both sources; don't pick silently.

Publish the merged pack as one artifact (all its files in one `drive_publish`), with the assignment's thread. Revisions after review are new versions of it (`artifact` and `changes`, office-publish), not new artifacts. Forward one pack, never several.

## Reviewing

Read the pack against the plan before anyone else does (reviewing-work):

- Every sub-question answered, or marked unknown with why.
- Every non-obvious claim cited to a reference that exists, and to the passage that supports it.
- Facts and judgement apart, judgement with its confidence.
- No padding; the index answers the question on its own.

Send fixes back to the researcher who owns the strand, specifically and once, with a due date. Collate and review again.

## Reporting

`mail_reply` to your assignment with `kind: report`, `close: true`: the answer in three sentences, the confidence, the main caveat, and the pack attached. Then let the researchers go, or keep them if more research is coming.
