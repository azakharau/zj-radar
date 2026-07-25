# zj-radar — domain glossary

Names for the good seams in zj-radar. This file defines the domain concepts in
zj-radar's architecture, focusing on the key interfaces and state flows.

> **The per-tab sidebar plugin (`crates/plugin`) — the rail renderer,
> `RadarState`, the ledger, tab naming, click-target lockstep, and the
> cross-session presence badge — was removed entirely in commit `137f3d5` and
> replaced by the headless aggregator described below. If you're looking for
> any of those concepts, they no longer exist in this repo; the commit message
> and `crates/agents/src/lib.rs`'s doc comments carry the design reasoning that
> replaced them.**

## Status contract

The real external seam between producers and zj-radar: the versioned
`zj_radar.status.v1` pipe payload (`{v, source, pane, status, repo, branch,
msg, task, ack, gone}`). Producers (the Claude plugin, the Codex CLI, `zj-radar
notify generic`) are adapters that broadcast it; `zj-radar-core::parse`
defends against malformed input at parse time (sanitize, truncate, drop
oversized/malformed) regardless of who calls it. Ordering is latest-wins — the
pipe delivers in order and no producer stamps a sequence, so there is nothing
to reorder. Unknown fields are tolerated and ignored, so older producers still
parse.

The wire schema carries more than the shipped aggregator currently reads:
`repo`/`branch`/`msg`/`task`/`ack` are parsed, sanitized, and capped like every
other field, but `crates/agents` (the only shipped consumer today) keys its
whole aggregate on `pane_id` + `status` + `source` only (see *Aggregation*
below) — the rest exists for a future or custom consumer of the same pipe, not
for today's `{pipe_agents}` widget.

## Information source

Anything that produces a per-pane observation. Two modalities were designed
into `zj-radar-core`, but only one has a live consumer today:

- **Pushed** — instrumented agents report status by broadcasting the *status
  contract* through the host CLI (`zj-radar notify <agent>`). Each agent is a
  peer adapter behind the **agent intake** seam — `Agent::derive(&Intake) ->
  Option<AgentUpdate>` in `crates/cli/src/agents/` — so `notify::run` is a
  thin, agent-agnostic shell (read input → derive → broadcast). Adding an
  agent is a compiler-guided `enum Agent` variant; its `source()` string is
  the single vocabulary shared across the CLI argument, the wire `source`, and
  `Kind::from_source`, pinned by the `source_round_trips_through_kind` guard
  test. This is the only modality `crates/agents` actually aggregates.
- **Observed** — uninstrumented commands (e.g. `cargo test`), classified by
  argv in `crates/core/src/command.rs::command_kind` with a full lifecycle
  store (`CommandStore`, debounce, TTL recede, `is_shell_prompt`-driven
  exit-clear). This modality's store has **no live consumer** post-`137f3d5`:
  it lived inside the now-deleted sidebar plugin, which subscribed to
  Zellij's `CommandChanged` event to drive it. `crates/agents` never
  subscribes to `CommandChanged`. Only a few pure helpers from this module
  (`contains_word`, `AGENT_NAMES`) are still used, by the CLI's `agents.rs`,
  for unrelated string-matching — the observation/lifecycle machinery itself
  (`CommandStore`, `Pending`, `DEBOUNCE_TICKS`, `is_shell_prompt`) is
  compiled and tested but otherwise dead code from the shipped binaries'
  point of view.

Both modalities emit a `source` string that must be a subset of `Kind`
(`Kind::from_source`). Both halves are guarded: the agent half by
`source_round_trips_through_kind` (in `crates/cli/src/agents`), the command
half by `command_source_round_trips_through_kind` (in
`crates/core/src/command.rs`) — each pins that its classifier's `source` token
round-trips back to the same `Kind`, never the `Other` sentinel.

## Aggregation

The per-pane → per-vendor fold that produces the `{pipe_agents}` string. The
**aggregation seam** is `Agents::apply`/`Agents::render` in
`crates/agents/src/lib.rs`: a pure, host-testable module (no `zellij-tile`
dependency) that owns one `Entry { kind, status, seen }` per live pane,
keyed by Zellij pane id.

- **Grouping.** Panes are grouped by `Kind` (the vendor — Claude, Codex,
  Gemini, OpenCode, Omp, …), walking `Kind::ALL` rather than hashing `Kind`
  (which is neither `Hash` nor `Ord`) so the table order is the tiebreak for
  vendors sharing a status.
- **Severity.** Each group's displayed status is `.max()` over that group's
  panes. `Status` is declared in ascending-severity order and derives `Ord`,
  so `.max()` *is* the documented priority `error > pending > running > done`
  — there is no separate priority table to keep in sync.
- **Rendering.** One `icon + status glyph` per vendor group, plus a count
  when the group holds more than one busy pane. Empty string when nothing is
  live, so the widget disappears entirely rather than showing a blank shell.
- **Lifecycle.** `gone` and `Idle` both drop a pane's entry outright (not just
  recolor it) — the semantic is "stop showing this pane," not "show it as
  quiet." `retain_panes` drops entries for panes Zellij no longer reports
  (via `PaneUpdate`). `expire` drops an entry that has gone silent past its
  TTL (`ttl_secs`, default 15 min; `done` uses the shorter `done_ttl_secs`,
  default 30s) — the guard against a producer that dies without sending
  `gone`.
- **Publish discipline.** The plugin (`crates/agents/src/main.rs`) publishes
  the rendered string to every zjstatus instance via `pipe_message_to_plugin`
  — with neither a plugin url nor a destination id, which is what makes
  Zellij broadcast to every loaded zjstatus instance rather than one — but
  **only from its `Timer` handler, never from `pipe()`**. Publishing while a
  CLI pipe is still outstanding would fan a new message out while
  `zellij pipe`'s caller is still blocked waiting on this same plugin
  instance, wedging Zellij's wasm thread session-wide under repeated hook
  traffic. `pipe()` therefore only records the observation and marks a dirty
  flag; the next `Timer` tick (every 250ms) does the actual publish, deduped
  against the last string sent (zjstatus re-renders on every pipe message
  without comparing values, and its pipe widgets bypass its widget cache, so
  sender-side dedup is what keeps an idle bar from repainting forever).

## Setup analysis

How `zj-radar setup` learns the current state of the world for the **Codex**
target. The **setup-analysis seam** is `analyze_codex(&CodexEnv) ->
CodexFacts` in `crates/cli/src/setup/analyze.rs`: a pure derivation fed a thin
`CodexEnv` of already-read values (file contents, PATH lookups) by the IO
shell. `CodexFacts` is the single home for every derived fact — the Codex
hooks-feature state, notify state, and whether zj-radar's own hooks are
already installed (and how completely).

`check_codex`/`setup_codex` project from `CodexFacts` for their `--check`
output and install gating; the pure mutator (`edit_codex_hooks` → `Outcome`)
is NOT driven by `Facts` — it shares only the low-level primitive detectors in
`crates/cli/src/setup/detect.rs`, a neutral module both `analyze` and `edit`
depend on. **Claude has no equivalent seam**: `check_claude` only has two
facts to verify (the `claude` binary on `PATH`, and whether the
`zj-radar-claude` plugin is installed via Claude's own marketplace), so it
reads them directly rather than going through an `analyze`/`Facts` step.
There is no `analyze_zellij` — the aggregator is not something `setup`
installs, so there is nothing for it to analyze.
