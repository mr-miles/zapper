---
name: office-publish
description: "How to publish finished work to the shared drive and build on others' (spaces, kinds, titles, summaries, versions, attachments, Markdown, decks). Use it whenever you deliver a document, report, dataset or deck, or use one."
---

# The shared drive

The drive holds the firm's finished work: documents, reports, datasets, decks. What you publish can't be changed afterwards; revisions are new versions.

## What to publish

- Deliverables: what an assignment asked for, what others will build on, what your manager or the board will read. Not scratch, logs or drafts nobody asked for.
- Publish before you report; the report attaches it.
- Never code: code goes to git as a branch (the drive refuses `kind: "code"`).

## Publishing

`drive_publish(paths, title, kind, summary, space?, artifact?, thread?, changes?)`

- `paths`: files under `/work`, your home or `/tmp`; about 50 MiB per artifact. Images in the same artifact can be shown in its Markdown.
- `space`: where it goes (default `general`); you need write there. Put it where its readers can read it.
- `kind`: `document` (briefs, specs, findings), `report` (results of a piece of work), `dataset` (CSV with a header row, or JSON), `deck` (slides), `board-pack` (the CEO's report to the board).
- `title`: specific ("Product brief v1: scope and acceptance criteria"). `summary`: one paragraph that tells a reader whether to open it.
- `thread`: the assignment it answers (`t-12`).
- **Revising**: publish again with `artifact: "a-3"` and `changes` (one or two lines on what changed). It stays in its space. A new artifact only for different work.

## Using others' work

- `drive_list(kind?, author?, query?, thread?)` finds artifacts in spaces you can read; check before producing something again. With `thread` (your assignment's thread) it lists what was published for that task, the work delegated from it included.
- `drive_fetch(artifact: "a-3@v2", dest: "/work/in/a-3")` copies a version into your machine. Read it before building on it; fetch exactly the version you were sent.
- Attach and cite by version: `drive:a-3@v2`.

## Markdown

The UI renders headings, emphasis, code, lists, quotes, tables, rules, `http`/`https`/`drive:` links, and images that are files of the same artifact (`![Home](home.png)`). Raw HTML shows as text.

- A `#` title, a short summary, then `##` sections; the conclusion first.
- Tables for numbers and comparisons; code blocks for commands.
- One line per paragraph or list item.

## Decks and board packs

Markdown slides separated by a line with only `---`; a heading on each; one idea per slide; at most about six bullets or one table. Put detail in a companion document and link it.
