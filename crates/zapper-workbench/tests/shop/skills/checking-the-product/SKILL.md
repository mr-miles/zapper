---
name: checking-the-product
description: "How to check what is built through its interface, as a user would: the paths and pages the criteria name, at desktop and phone widths, with the browser and curl. Use it whenever you test, review or verify the product or a running service."
---

# Checking the product

Check through the product's interface, not its code: what a user or a client sees is what counts.

## Tools

- `curl -i URL` for each path and API the criteria name: status, headers, body.
- `browse shot URL`, `browse text URL` and `browse console URL` (the image's headless browser) for what a page shows and what it logs. Don't add `--all` for routine checks: every outside host it loads is a request the user has to approve.

## What to check

- Each acceptance criterion, as a user would do it, with a numbered pass or fail and what you saw.
- Desktop and phone widths; empty, loading and error states; the console clean.
- Several clients at once for anything shared between users.
- With the teammates' pieces yours depends on, not stubs of them.
- After a restart: what should persist does. The logs show no errors.
- Where it runs: in your own sandbox, as the brief's instructions start it.

## Report

What you checked, at which version, how, and what passed and failed, with screenshots or output for each failure, to the user.
