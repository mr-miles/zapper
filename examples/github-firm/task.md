A shopping list app for one person, on GitHub

Ada, you start alone. We want a simple shopping list web app for a single user: add an item (with an optional quantity), tick it off as bought, remove it, and clear the bought ones. A Node.js backend with a small JSON API, and a React frontend. No database yet: the backend keeps the list in memory, and it's fine that a restart empties it. No accounts, no deployment.

It goes in the firm's GitHub repository, the one your preamble lists. Its `main` already has a workflow that runs `npm test` on every pull request.

Please:

1. Call budget() and ask me for a budget that fits this plan: one product manager, then two engineers, so that each one's work is reviewed by the other.
2. Hire a product manager to write the specification: the user's needs, the features above as user stories with acceptance criteria, the API (routes and JSON shapes) and the screens. It goes in the repository as SPEC.md, through a pull request like everything else, so the engineers build from it.
3. When I've merged the spec, hire the engineers and have them build it from SPEC.md: the backend, the frontend, and a README that says how to install, build, run and test it.
4. Every change is a pull request, reviewed by another engineer: for repetition, poor encapsulation and code that doesn't follow the spec, as well as bugs. Its checks must pass before it comes to me, and I merge it. Nobody pushes to main, and nobody changes the workflows: if one needs changing, send me the change.
5. Report back to me when it's done, with a short summary: what was built, how to run it, and what it cost.

Done means all of these, checked by you, not taken on trust:

- All the code is merged to main through pull requests whose checks passed.
- A fresh clone builds with the README's steps.
- All the tests pass.
- The tests exercise the spec: each acceptance criterion in SPEC.md has a test that checks it, the API's and the frontend's.
- The code has been reviewed: no repetition, each part's job behind its own interface.

Everything the team needs is mail, the firm's git repository and npm. Grant git access to those who need it, at the least level that does the work.
