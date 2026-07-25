# Contributing to zj-radar

Thanks for your interest! zj-radar is a host-side CLI plus a headless Zellij
plugin (Rust → `wasm32-wasip1`) that aggregates AI-agent status into a
zjstatus `{pipe_agents}` widget, and a Claude Code producer plugin. This guide
covers how to build, test, and propose changes.

## Project shape

zj-radar is a three-member Cargo workspace:

| Path | What it is |
|------|------------|
| `crates/core/` | Pure shared library (`zj_radar_core`): the versioned wire schema and status/command classification (`command`, `kind`, `observation`, `payload`, `status`, `wire`). No `clap`, no `zellij-tile`. |
| `crates/cli/` | The native `zj-radar` CLI (`notify`, `setup [claude\|codex]`). |
| `crates/agents/` | The headless Zellij **wasm plugin** (`zj-radar-agents`, artifact `zj_radar_agents.wasm`, Rust → `wasm32-wasip1`): aggregates per-pane status by vendor and republishes a `{pipe_agents}` string to every zjstatus instance. `lib.rs` is pure aggregation logic (host-testable, no `zellij-tile`); `main.rs` is the thin Zellij glue, gated behind `#[cfg(target_arch = "wasm32")]`. |
| `plugins/zj-radar-claude/` | The Claude Code producer plugin (hooks + bundled `notify.sh`). |
| `docs/` | Reference and install docs. Start with [`CONTEXT.md`](CONTEXT.md) (domain glossary). |

One idea is load-bearing — read [`CONTEXT.md`](CONTEXT.md) before changing the
core:

- **Push-driven, never poll-driven.** The aggregator plugin never issues
  blocking host queries. Status arrives via `zellij pipe` broadcasts, and the
  plugin only ever publishes back out to zjstatus from its `Timer` handler,
  never from `pipe()` — see *Aggregation* in `CONTEXT.md` for why fanning out
  mid-pipe would wedge the session.

## Prerequisites

- A stable Rust toolchain. `rust-toolchain.toml` requests the `wasm32-wasip1`
  target, which `rustup` auto-installs on first build. See
  [`docs/TOOLCHAIN.md`](docs/TOOLCHAIN.md).
- **MSRV is Rust 1.95** (`rust-version` in the root `Cargo.toml`). CI's `msrv`
  job builds with exactly that toolchain, so language/stdlib features newer
  than 1.95 will fail the PR. Dev otherwise tracks `stable`.
- For the full suite: `just`, plus `bats`, `shellcheck`, and `jq` (bash hook
  tests).
- Optional: Nix. `nix develop` drops you into a shell with everything pinned;
  `nix flake check` runs the same checks the `hermetic` CI job uses.

## Build

```sh
cargo build                                          # host library + CLI checks
cargo build --release --target wasm32-wasip1 -p zj-radar-agents   # the wasm plugin Zellij loads
```

## Test

`just` is the entry point:

```sh
just test        # deterministic host suite (crates/agents' aggregation logic is pure)
just test-bash   # bash hook tests (needs bats + shellcheck + jq)
just ci          # what every PR must pass locally: test + clippy + wasm build + test-bash
```

Run a single test with `cargo test <name>` (scope it with e.g.
`-p zj-radar-agents`).

- The shared core (`status`, `payload`, `command`, `kind`, `observation`,
  `wire`) lives in `crates/core`; the aggregator's own aggregation logic lives
  in `crates/agents/src/lib.rs`. Neither carries a `zellij-tile` dependency on
  the native target, so both run host-side — no wasm build needed for most
  work.

## Lint & formatting

```sh
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

> **This project does not use `rustfmt`.** The code is intentionally
> hand-formatted (e.g. aligned one-line multi-field structs). Please **do not**
> run `cargo fmt` / `cargo fmt --all` — it would reformat the whole codebase and
> the diff will be rejected. Match the formatting of the surrounding code.

`shellcheck` runs over `plugins/zj-radar-claude/scripts/notify.sh` in CI; run it
locally if you touch the script.

## Dev loop

```sh
just build-wasm     # cross-compile the aggregator to wasm32-wasip1
just install-wasm   # build-wasm + copy the artifact where load_plugins expects it
```

Zellij reads a `load_plugins` entry once, at session launch, and does not
hot-reload it — start a new session to pick up a freshly installed wasm. The
permission grant is cached per wasm path, so replacing the file in place (what
`just install-wasm` does) keeps it. See
[`docs/install.md`](docs/install.md) for the full manual setup, including the
one-time permission grant.

## Pull requests

1. Open an issue first for anything non-trivial, so we can agree on the approach.
2. Keep PRs focused; one logical change per PR.
3. `just ci` must pass — it runs the host suite, `cargo clippy ... -D warnings`,
   a wasm compile check, and the bash hook tests.
4. Add or update tests at the appropriate layer: new aggregation behavior → a
   unit test in `crates/agents/src/lib.rs`; new wire/parse behavior → a
   unit/proptest in `crates/core`.
5. Update docs (`README.md`, `docs/`, `CONTEXT.md`) when behavior or interfaces
   change.
6. Don't commit generated artifacts (`target/`) or editor/tool state.

## Adding a producer or an agent

The plugin's only real external interface is the versioned `zj_radar.status.v1`
pipe payload (see [`docs/producers.md#writing-your-own-producer`](docs/producers.md#writing-your-own-producer)). To add a new
instrumented agent to the CLI, add an `enum Agent` variant in
`crates/cli/src/agents/` and implement `Agent::derive`; the
`source_round_trips_through_kind` guard test will tell you what else to wire.
Observed (uninstrumented) commands like `cargo test` are classified in
`crates/core/src/command.rs`, not in `agents/` — though that classifier's
lifecycle store has no live consumer in the shipped aggregator today (see
`CONTEXT.md` → *Information source*).

## License

By contributing, you agree that your contributions are licensed under the
project's [MIT License](LICENSE).
