# zj-radar

Live AI-agent status — *working*, *waiting for you*, *done*, or *error* — as a
compact `{pipe_agents}` widget inside the [zjstatus](https://github.com/dj95/zjstatus)
bar you already run in [Zellij](https://zellij.dev). One icon + status glyph per
agent vendor, a count when more than one pane of that vendor is busy, nothing
shown when nothing is live.

<p align="center">
  <a href="https://github.com/marktoda/zj-radar/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/marktoda/zj-radar/ci.yml?branch=main&label=ci">
  </a>
  <a href="https://crates.io/crates/zj-radar">
    <img alt="crates.io" src="https://img.shields.io/crates/v/zj-radar">
  </a>
  <a href="https://github.com/marktoda/zj-radar/blob/main/LICENSE">
    <img alt="License" src="https://img.shields.io/github/license/marktoda/zj-radar">
  </a>
  <img alt="Zellij plugin" src="https://img.shields.io/badge/zellij-plugin-8A2BE2">
  <img alt="Claude Code" src="https://img.shields.io/badge/Claude%20Code-supported-orange">
  <img alt="Codex" src="https://img.shields.io/badge/Codex-supported-black">
  <img alt="Status" src="https://img.shields.io/badge/status-alpha-yellow">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#how-is-this-different">How is this different?</a> ·
  <a href="#configuration">Configuration</a> ·
  <a href="#producers">Producers</a>
</p>

## What is it?

Agents like Claude Code spend long stretches working, then quietly block on a
permission prompt or finish. In a many-tab Zellij session it's easy to lose
track of which agent needs you. zj-radar surfaces that at a glance, inside the
status bar you already have — without launching, owning, or wrapping your
agents, and without a dedicated pane of its own.

## Highlights

- See which Claude Code / Codex panes are **working, done, errored, or waiting for you**,
  aggregated per vendor across every pane running that vendor.
- Lives in your **existing zjstatus bar** — no dedicated pane, no per-tab
  plugin instance, no layout edits.
- **Push-driven** updates via `zellij pipe`; the widget never polls.
- Works with **Claude Code** today, **Codex** via the native CLI, and any
  [custom producer](https://github.com/marktoda/zj-radar/blob/main/docs/producers.md#writing-your-own-producer) that can send JSON.

## Quick start

> **Requires Zellij 0.44.3 or newer** (don't have Zellij? [install it](https://zellij.dev/documentation/installation)
> first — `zellij --version` to check).

```sh
# 1. Install the zj-radar CLI (prebuilt: Linux x86_64/aarch64, Apple Silicon macOS;
#    Intel macOS installs from source — see docs/install.md)
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/marktoda/zj-radar/releases/latest/download/install.sh | sh

# 2. Get the aggregator wasm onto disk (from a release tag, or build it
#    yourself — see docs/install.md):
mkdir -p ~/.config/zellij/plugins
curl -fsSL -o ~/.config/zellij/plugins/zj_radar_agents.wasm \
  https://github.com/marktoda/zj-radar/releases/latest/download/zj_radar_agents.wasm
```

Then wire it into your own `~/.config/zellij/config.kdl` — this is deliberately
**not** something zj-radar installs for you (it's your bar, your layout):

```kdl
load_plugins {
    "file:~/.config/zellij/plugins/zj_radar_agents.wasm" {
        pipe_name "pipe_agents"
        glyphs "nerd"          // or "plain" without a Nerd Font
    }
}
plugins {
    zjstatus location="file:~/.config/zellij/plugins/zjstatus-v0.23.0.wasm" {
        format_right "... {pipe_agents}{pipe_resources} ..."
        pipe_agents_format     "{output}"
        pipe_agents_rendermode "dynamic"
    }
}
```

A `load_plugins` entry has no pane, so Zellij's first-run permission prompt has
nowhere to render. Grant it once, from a floating pane:

```sh
zellij action launch-or-focus-plugin "file:$HOME/.config/zellij/plugins/zj_radar_agents.wasm" --floating
# press `y` in the floating pane, then close it
```

The grant is cached per wasm path in Zellij's `permissions.kdl`, so replacing
the wasm in place (an upgrade) keeps it — only a path change re-prompts.
Restart Zellij (or start a new session) to pick up the `load_plugins` entry.

Then add a **producer** so the widget has something to show — without one, it
stays blank (it deliberately doesn't guess at agents it can't hear from). One
command per agent:

```sh
zj-radar setup claude   # installs the zj-radar-claude plugin via Claude Code's marketplace
zj-radar setup codex    # wires Codex hooks — then run `/hooks` inside Codex to trust them
```

(Prefer Claude Code's own UI? `/plugin install zj-radar-claude@zj-radar` inside
Claude Code does the same thing — `setup claude` drives that same marketplace.)

Full details (building the wasm yourself, Nix, checksums) are in
**[`docs/install.md`](https://github.com/marktoda/zj-radar/blob/main/docs/install.md)**.
Custom producers are in **[`docs/producers.md`](https://github.com/marktoda/zj-radar/blob/main/docs/producers.md)**.

## How it works

```
agent hooks -> `zj-radar notify` -> one CLI pipe (zj_radar.status.v1)
            -> ONE headless `zj-radar-agents` plugin (loaded once per session,
               no pane)
            -> pipe_message_to_plugin broadcast -> every zjstatus instance
            -> {pipe_agents} widget
```

Each agent hook broadcasts a `zj_radar.status.v1` pipe payload, same as
before. The one headless `zj-radar-agents` plugin — loaded once via
`load_plugins`, not once per tab — folds those per-pane observations into a
short string (an icon + status glyph per vendor, `.max()` over that vendor's
panes by severity: `error > pending > running > done`) and republishes it to
every zjstatus instance via `pipe_message_to_plugin`, with neither a plugin
URL nor a destination id — that combination is what makes Zellij broadcast to
every zjstatus instance rather than one. The republish only ever happens from
the plugin's `Timer` handler, never from `pipe()`: fanning a broadcast out
while a CLI pipe is still outstanding would wedge Zellij's wasm thread
session-wide (`zellij pipe` blocks its caller until every recipient plugin
instance has returned).

This replaced a per-tab sidebar plugin: `zellij pipe` blocks until *every*
loaded plugin instance consumes the message, so one instance per tab meant
every agent hook paid for all of them. A single headless consumer that fans
out in-process removes that cost, at the price of the sidebar's own UI (tab
list, click-to-switch, per-tab breakdown) — none of which survived the
refactor. See the `zj-radar-agents` crate (`crates/agents/src/lib.rs`) for the
aggregation logic and its rationale in more detail.

## How is this different?

| Tool | Best for | How `zj-radar` differs |
|---|---|---|
| [Claude Squad](https://github.com/smtg-ai/claude-squad) | Running multiple agents in isolated git worktrees from one TUI. | `zj-radar` does not launch or own agents; it shows status inside the Zellij session you already use. |
| [cmux](https://github.com/manaflow-ai/cmux) | A macOS terminal with vertical tabs, notifications, browser panes, and agent-aware UI. | `zj-radar` is a Zellij plugin, not a new terminal app. |
| [zjstatus](https://github.com/dj95/zjstatus) | Replacing / customizing the Zellij status bar. | `zj-radar` doesn't replace it — it feeds one widget (`{pipe_agents}`) into the zjstatus bar you already have. |
| Plain Zellij tabs | Manual multiplexing. | `zj-radar` adds agent state at a glance, aggregated per vendor. |

The short version: **inside your existing zjstatus bar, push-driven, not an
orchestrator, not a new terminal.**

## Configuration

Options live on the `load_plugins` entry in `~/.config/zellij/config.kdl` (see
[Quick start](#quick-start) above for the full block). The full option table
is in **[`docs/configuration.md`](https://github.com/marktoda/zj-radar/blob/main/docs/configuration.md)**.

## Producers

A producer broadcasts agent status to the pipe the aggregator consumes.
zj-radar ships two and documents the wire format so you can write your own:

- **Claude Code** — a Claude plugin that auto-registers status hooks (no
  `settings.json` editing).
- **Codex / native CLI** — `zj-radar notify` + `zj-radar setup codex`.
- **Custom** — broadcast a `zj_radar.status.v1` JSON payload from anything.

See **[`docs/producers.md`](https://github.com/marktoda/zj-radar/blob/main/docs/producers.md)** for install steps, the payload
schema, and a copy-paste smoke test.

## Documentation

| Doc | What's in it |
|-----|--------------|
| [`docs/install.md`](https://github.com/marktoda/zj-radar/blob/main/docs/install.md) | Building/installing the aggregator wasm, wiring `config.kdl`, the one-time permission grant, Nix / home-manager. |
| [`docs/producers.md`](https://github.com/marktoda/zj-radar/blob/main/docs/producers.md) | Claude Code, Codex, and writing your own producer (payload schema + smoke test). |
| [`docs/configuration.md`](https://github.com/marktoda/zj-radar/blob/main/docs/configuration.md) | The aggregator's `load_plugins` options (`pipe_name`, `glyphs`, TTLs). |
| [`docs/troubleshooting.md`](https://github.com/marktoda/zj-radar/blob/main/docs/troubleshooting.md) | Blank widget, permission grant, version skew. |
| [`docs/distribution.md`](https://github.com/marktoda/zj-radar/blob/main/docs/distribution.md) | The rationale behind the producer install story (historical memo). |

## Status & roadmap

- ✅ **`{pipe_agents}` zjstatus widget** — one headless plugin, loaded once per
  session, feeding every zjstatus instance; see [How it works](#how-it-works).
- ✅ **Claude Code producer** — ships as a Claude plugin (`plugins/zj-radar-claude`).
- ✅ **`zj-radar` CLI** — native, jq-free `notify` (Claude + Codex) and
  conflict-aware `setup [claude|codex]`; see [`docs/producers.md`](https://github.com/marktoda/zj-radar/blob/main/docs/producers.md#codex-and-the-native-cli).
- ✅ **Prebuilt releases** — a tagged release ships static Linux + macOS CLI
  binaries, a one-line `curl | sh` installer, and the aggregator wasm as a
  separate release asset (`zj_radar_agents.wasm`). See
  [`docs/install.md`](https://github.com/marktoda/zj-radar/blob/main/docs/install.md).
- ✅ **crates.io / `cargo binstall`** — `cargo install zj-radar` (or
  `cargo binstall zj-radar` for the prebuilt binary) works today. The CLI and
  its `zj-radar-core` dependency publish to crates.io; the wasm plugin is not
  a crates.io crate — it ships as a release artifact you fetch or build
  yourself (see [`docs/install.md`](https://github.com/marktoda/zj-radar/blob/main/docs/install.md)).

The changelog is the [GitHub Releases page](https://github.com/marktoda/zj-radar/releases) —
each tag's notes cover what changed.

## Development

```sh
just test        # host tests, no wasm needed (crates/agents' aggregation logic is pure)
just clippy       # workspace lint, warnings are errors
just build-wasm   # cross-compile the aggregator to wasm32-wasip1
just install-wasm # build-wasm + copy the artifact where load_plugins expects it
just test-bash    # bash hook + installer tests (needs bats + shellcheck + jq)
just ci           # everything a PR must pass: test + clippy + build-wasm + test-bash
```

### Repo layout

| Path | What it is |
|------|------------|
| `crates/core/` | Pure shared library (`zj_radar_core`): the versioned wire schema + status/command classification (`command`, `kind`, `observation`, `payload`, `status`, `wire`). No `clap`, no `zellij-tile` — fully host-testable. |
| `crates/cli/` | Host-side `zj-radar` CLI (package `zj-radar`): `notify` and `setup [claude\|codex]`. Built with `-p zj-radar`. |
| `crates/agents/` | The headless Zellij **wasm plugin** (`zj-radar-agents`, artifact `zj_radar_agents.wasm`, Rust → `wasm32-wasip1`): aggregates per-pane status by vendor and republishes a `{pipe_agents}` string to every zjstatus instance. Pure aggregation logic lives in `lib.rs` (host-testable); the Zellij glue is in `main.rs`, gated behind `#[cfg(target_arch = "wasm32")]`. Built with `-p zj-radar-agents`. |
| `plugins/zj-radar-claude/` | A **Claude Code plugin** that broadcasts agent status via hooks — no `settings.json` editing. |
| `docs/` | Reference and install docs. |

## Contributing

Issues and PRs welcome. See [`CONTRIBUTING.md`](https://github.com/marktoda/zj-radar/blob/main/CONTRIBUTING.md) for build/test
layers, the no-`rustfmt` rule, and the push-driven invariant.
[`CONTEXT.md`](https://github.com/marktoda/zj-radar/blob/main/CONTEXT.md) is the domain glossary —
the fastest way to orient before touching the core.

## License

MIT — see [`LICENSE`](https://github.com/marktoda/zj-radar/blob/main/LICENSE).
