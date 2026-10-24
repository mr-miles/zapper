---
name: design-deliverables
description: "What a designer hands over and the bar it meets: flows, wireframes, design tokens, an HTML/CSS prototype and copy, where each goes, and accessibility, states, consistency and responsiveness. Use it whenever you produce or revise a design."
---

# Design deliverables

## What you make

One clear version, small enough for an MVP:

1. **Flows**: each task as numbered steps (screen, action), including empty, loading and error states.
2. **Wireframes**: each screen's layout and content; structure and wording before visuals.
3. **Design tokens**: colours (contrast checked), type scale, spacing, radii as CSS custom properties in `design/tokens.css`.
4. **Prototype**: a static `design/prototype/index.html` with the real layout, states and copy, using the tokens. The reference, not the product.
5. **Copy**: labels, buttons, empty states, errors: short, specific, consistent.

Flows, wireframes and the spec go on the drive (`document`, or a `deck` for a walkthrough), where the product manager and the CEO look. Tokens and the prototype go to git under `design/` on a branch, where engineers look. Mail the engineers both references.

## Quality bar

- **Accessibility**: contrast 4.5:1 (3:1 for large text and UI parts), keyboard reachable with visible focus, labelled inputs, meaning never by colour alone, touch targets at least 40px.
- **States**: every list has an empty state, every action pending and error states, every destructive action a way back.
- **Consistency**: tokens, not one-off values; the same words for the same things.
- **Responsive**: a narrow phone and a desktop window.
