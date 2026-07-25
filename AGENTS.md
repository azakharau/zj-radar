# AGENTS.md

Entry point for AI agents (and humans skimming) working in zj-radar. Keep this
thin — it points at the real docs rather than duplicating them.

zj-radar is a host-side `zj-radar` CLI, a headless Zellij plugin (Rust →
`wasm32-wasip1`) that aggregates agent status into a zjstatus `{pipe_agents}`
widget, and a Claude Code producer plugin.

## Read first

- [`CONTEXT.md`](CONTEXT.md) — domain glossary and the load-bearing seams (the
  status contract, the aggregation seam, setup analysis). **Read before
  changing the core.**
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — project shape, full build/test/lint
  details, PR expectations.

## Commands

```sh
cargo build                                          # host library + CLI checks
cargo build --release --target wasm32-wasip1 -p zj-radar-agents   # the wasm plugin Zellij loads
just test        # deterministic host suite (unit tests; crates/agents' aggregation is pure)
just test-bash   # bash hook tests (needs bats + shellcheck + jq)
just build-wasm  # cross-compile the wasm plugin
just install-wasm # build-wasm + copy the artifact where load_plugins expects it
just ci          # what every PR must pass: test + clippy + build-wasm + test-bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

The `wasm32-wasip1` target is requested by `rust-toolchain.toml` and
auto-installs on first build (see [`docs/TOOLCHAIN.md`](docs/TOOLCHAIN.md)). Most
of the core lives in `crates/core` and is host-testable — no wasm build needed
for typical work.

## Non-negotiable rules

- **Do not run `rustfmt` / `cargo fmt`.** The code is intentionally hand-formatted
  (e.g. aligned one-line multi-field structs). A `cargo fmt` diff will be rejected.
  Match the surrounding code.
- **Push-driven, never poll-driven.** The aggregator plugin must not issue
  blocking host queries; status arrives via `zellij pipe` broadcasts.
- **Never publish from `pipe()`.** The aggregator plugin publishes to
  zjstatus only from its `Timer` handler. Publishing from inside `pipe()`
  would fan a message out while the triggering `zellij pipe` client is still
  blocked on this same plugin instance, wedging Zellij's wasm thread
  session-wide under repeated hook traffic (`CONTEXT.md` → *Aggregation*, and
  the doc comments on `pipe()` in `crates/agents/src/main.rs`).

## Adding a producer or agent

The only external interface is the versioned `zj_radar.status.v1` pipe payload.
New instrumented agent → `enum Agent` variant in `crates/cli/src/agents/` +
`Agent::derive`; the `source_round_trips_through_kind` guard test tells you what
else to wire. Observed (uninstrumented) commands like `cargo test` are classified
in `crates/core/src/command.rs`, not in `agents/` — but note that module's
lifecycle store has no live consumer today (see `CONTEXT.md` → *Information
source*); don't assume it's wired into anything without checking.
