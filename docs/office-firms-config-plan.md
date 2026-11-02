# E — firms are the office's own config (`config/office.yaml`)

Implementation plan for item **E** of [office-process-step4-plan.md](office-process-step4-plan.md) (architecture decision 2). Branch: `feat/office-firms-config` (off `feat/office-process-step4`). Resumable: each phase below keeps `make lint && make test` green and is its own commit.

## Goal

Firms are the office's domain, not the daemon's. Today the daemon's `zapper_policy::Config` carries `firms:` that the daemon never validates or uses, and the office fetches the whole daemon config (firms included) over `/v1/config` to read config that is conceptually its own. E moves `firms:` into an office-owned `config/office.yaml`, loaded and validated by the office against the daemon's profiles; the daemon stops carrying firm data.

## Current data flow (grounded)

- `zapper_policy::Config.firms: BTreeMap<String, serde_yaml::Value>` — `crates/zapper-policy/src/config.rs:29`, struct is `#[serde(deny_unknown_fields)]`.
- The office reads firms only through `OfficeClient::config().firms`:
  - `Local::config()` (`client.rs:172`) → the co-host `Core`'s running config — **test-only** now (`start` is `#[cfg(test)]`, `lib.rs:837`).
  - `Remote::config()` (`client.rs:331`) → `GET /v1/config?format=json` — the standalone (prod) path.
- Consumers of firm data (non-test):
  - `http.rs:147` `firm::all(&config)` (firm list for the page), `:166`/`:200` `firm::get(&config, f)`, `:249` `config().firms.contains_key(f)`.
  - `run.rs:558,567,623` `firm::get(&config, name)` (start/inspect a run).
  - `firm.rs:1185` `all`, `:1193` `get`, `:1202` `check` (`#[cfg(test)]`), `:1204,1210` read `c.firms`.
  - `lib.rs:843` `d.add_config_check(firm::check)` — wired only in the test-gated `start`.
  - `zapper/src/main.rs:1605` `config.firms.keys()` — `zapper check`'s "what the driver drives" line.
- Firm-config reload today (prod): the office calls `firm::get`/`all` fresh per request against the live `/v1/config`, so an operator edits `config/zapper.yaml` and runs `zapper config reload`; the daemon swaps its config and the office sees the new firms on the next read.
- Example configs mixing `profiles:`+`firms:`, loaded whole in `examples_tests` — all four carry a `firms:` block: `examples/mini-firm/zapper.yaml`, `examples/github-firm/zapper.yaml`, `examples/research-firm/zapper.yaml`, `examples/shopping-list/zapper.yaml`.

## Target design

- **`config/office.yaml`** — a new office-owned file with top-level `firms:` (same shape firms have today). Lives beside `config/zapper.yaml` in the checkout; outside a checkout, `zapper-office init` writes it to `~/.config/zapper/office.yaml` (mirror where `zapper init` writes `zapper.yaml`).
- **Office-owned firms holder.** The office struct (`OfficeOf`, behind `d.office()` / the standalone office) gains `firms: RwLock<BTreeMap<String, Firm>>`, loaded and validated at office boot and swapped atomically on reload. Firms are stored **parsed** (`Firm`), not raw `Value` — validation happens at load, not per request.
- **`OfficeClient::config()` keeps returning the daemon `Config`** (now firm-free); the office uses it only for `profiles`/`daemon` fields (host-room, profile resolution). No new trait method needed for firms — firms come from the office holder, not the client.
- **`firm::check` un-gated** and reshaped to a standalone validator `validate_firms(&BTreeMap<String, Firm>, profiles: &Config) -> Result<()>` (parse + `Firm::validate` against profiles + the driver-on check). The office calls it on load/reload.
- **`firms` removed from `zapper_policy::Config`.** Because `Config` is `deny_unknown_fields`, this also means `firms:` must leave `config/zapper.yaml`, the example configs, and whatever `zapper init` writes. `zapper check`'s firm line (`main.rs:1605`) loses its firm source — drop the firm clause (the office owns that knowledge now) or have it say "firms are configured in office.yaml".

## Design decisions

1. **Firms stored parsed in the office, loaded once + explicit reload** (not re-read per request). Mirrors the daemon's running/startup-config model: validated as a whole, applied atomically. A running firm already snapshots its `Firm` at run start (`run.rs` `firm::get` at launch), so in-flight runs are unaffected by a reload — unchanged from today.
2. **Reload ownership** — the one genuinely operator-facing open question (see below). Recommended: a `zapper-office config reload` that re-reads `office.yaml`, re-validates against the daemon's current profiles, and swaps the holder atomically; parallels `zapper config reload`. Firm-config is no longer touched by the daemon's `zapper config reload`.
3. **Contain test churn by splitting YAML inside `test_core`, not by rewriting test literals.** `test_core(yaml)` (`lib.rs:917`) parses the combined YAML to a `serde_yaml::Value`, removes the `firms` key, hands the rest to `TestCore::new`, and loads the extracted `firms` into the office holder. Existing test YAML literals (which keep `firms:`) are untouched. Only ~7 sites that build a `Config` directly and call `firm::get(&config, …)` need a new `test_firm(yaml, name) -> Firm` helper (`access.rs:1078`, `mail.rs:1150`, `run.rs:994,1050,1094`, `workforce.rs:1708`). The original "~18 files" estimate assumed rewriting every literal; splitting in `test_core` avoids that.
4. **Examples are split, not stripped.** Each firm example becomes `examples/<name>/zapper.yaml` (profiles/daemon/images) + `examples/<name>/office.yaml` (firms). `examples_tests` loads both. This is the operator-visible layout change; document it in the examples' and `AGENTS.md`'s descriptions.

## Phased implementation

Each phase is a commit; `make lint && make test` green at each.

- **E1 — office-owned loader + holder, firms still mirrored from the daemon.** Add `office.yaml` loader (`firm::load(path) -> Result<BTreeMap<String, Firm>>`), the `firms` holder on the office, and `validate_firms` (un-gate `check`, reshape to take parsed firms + profiles). Wire the holder in both boots: `standalone::serve` reads `office.yaml` (path from config dir); test `start`/`test_core` loads the firms split out of the test YAML. Route `http.rs`/`run.rs` firm reads through the holder (`office.firm(name)`, `office.firms()`) instead of `config().firms`. **Do not remove `firms` from `Config` yet** — keep it parsing so nothing else breaks mid-phase. This is the big, behaviour-preserving phase; firms now come from the office even though the field still exists on `Config`.
- **E2 — cut the daemon edge.** Remove `firms` from `zapper_policy::Config` (`config.rs:29`). Fix fallout: `test_core` YAML-split (already in E1), `zapper/src/main.rs:1605` + the `c.firms.is_empty()` assertion at `:1928`, and `Config::from_yaml` call sites. Strip `firms:` from `config/zapper.yaml` and move it to `config/office.yaml`. Split all four firm example configs (`mini-firm`, `github-firm`, `research-firm`, `shopping-list`) into `zapper.yaml`+`office.yaml` and update `examples_tests`.
- **E3 — `zapper-office init` writes `office.yaml`; reload.** `zapper-office init` writes `~/.config/zapper/office.yaml` (and the skills, as today). Implement the chosen reload path (decision 2). Update `zapper check` wording.
- **E4 — docs.** `AGENTS.md` (the `config/` map: add `office.yaml`, note firms moved), `docs/configuration.md`, `docs/the-office.md`/`office.md`, the examples' READMEs, and the step4 plan's E item → done.

## Test-rework strategy

- `test_core`: parse → strip `firms` → `TestCore::new(rest)` → `office.load_firms(extracted)`. One function.
- New `#[cfg(test)]` helper `test_firm(yaml: &str, name: &str) -> Firm` for the ~7 direct-Config call sites.
- `examples_tests`: load `zapper.yaml` into `Config` and `office.yaml` into the firms holder; assert the firm validates against the profiles.
- `firm.rs` existing `check`/validate unit tests: retarget to `validate_firms`.

## Risks & rollback

- **Blast radius:** `zapper_policy::Config` is shared by the daemon, CLI, workbench and office; removing a field touches serialization round-trips (startup/running config, `config save`, `config diff`). E2 is the risky phase — do it alone, lean on `make test` (the config round-trip tests) and a manual `zapper config save`/`diff` smoke. E1 is behaviour-preserving and low-risk.
- **Operator-visible:** the config file splits in two and the examples' layout changes. This is intended (decision 2) but must be called out in docs and the changelog/PR description.
- **Rollback:** each phase is a standalone commit on `feat/office-firms-config`; revert E2 to restore the single-file config if the round-trip story needs more work, keeping E1's office-owned reads.

## Reload (decision 2 — settled)

**`zapper-office config reload`** (operator's choice): re-reads `office.yaml`, re-validates the firms against the daemon's current profiles, and swaps the office's firms holder atomically — or fails as a whole and changes nothing, parallel to `zapper config reload`. Firm-config is no longer touched by the daemon's `zapper config reload`. Running firms are unaffected (a run snapshots its `Firm` at start). Implemented in E3.

## Progress log

- 2026-11-02 — Plan written on branch `feat/office-firms-config`. No code yet. Pending operator's reload-ownership choice before E3; E1/E2 can start regardless.
