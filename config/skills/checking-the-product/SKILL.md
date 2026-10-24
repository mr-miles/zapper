---
name: checking-the-product
description: "How to check what is built through its interface, as a user would: the paths and pages the criteria name, at desktop and phone widths, with the browser and curl. Use it whenever you test, review or verify the product or a running service."
---

# Checking the product

Check through the product's interface, not its code: what a user or a client sees is what counts.

## Tools

- `curl -i URL` for each path and API the criteria name: status, headers, body.
- `browse shot URL /work/shot.png` screenshots a page (`--width 390` for a phone, `--full` for the whole page); read the PNG to see it.
- `browse text URL` prints the visible text after scripts ran; `browse console URL` prints script errors and failed requests.
- `browse` loads only the product's own site: outside images and scripts are skipped and listed, not requested. Don't add `--all` for routine checks: every outside host it loads is an access request the officers must decide.

## What to check

- Each acceptance criterion, as a user would do it, with a numbered pass or fail and what you saw.
- Desktop and phone widths; empty, loading and error states; the console clean.
- Several clients at once for anything shared between users.
- After a restart: what should persist does. The logs show no errors.
- Where it runs: your own workloads in `dev` while building; the release at `http://apps.<domain>` once released. Never test against the release while building.

## Report

What you checked, at which version, how, and what passed and failed, with screenshots or output for each failure.
