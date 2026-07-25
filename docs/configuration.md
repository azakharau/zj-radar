# Configuration

The aggregator (`zj-radar-agents`) reads its options from its own
`load_plugins` entry in `~/.config/zellij/config.kdl`. For a minimal example,
see [Quick start in the README](../README.md#quick-start).

## Options

Every key is optional; an unrecognized or unparseable value keeps the default
rather than disabling the widget, so a typo degrades to stock behavior instead
of a silently blank bar.

```kdl
load_plugins {
    "file:~/.config/zellij/plugins/zj_radar_agents.wasm" {
        pipe_name "pipe_agents"   // default shown
        glyphs "nerd"             // default is "plain"
        ttl_secs 900              // default shown (15 minutes)
        done_ttl_secs 30          // default shown
    }
}
```

| Key | Values | Default | Effect |
|-----|--------|---------|--------|
| `pipe_name` | any non-empty string | `pipe_agents` | The zjstatus pipe/widget name. Must match the `pipe_<name>_format` / `pipe_<name>_rendermode` keys on your zjstatus plugin block (see below) — zjstatus derives the widget key by stripping the trailing `_format`/`_rendermode` from the config key. |
| `glyphs` | `plain` · `nerd` | `plain` | Status glyph set. `nerd` needs a Nerd Font; it also upgrades the per-vendor mark to a real vendor logo (Claude, Codex, Gemini, OpenCode, …) from Simple Icons. |
| `ttl_secs` | integer seconds | `900` | How long a pane's last observation is shown after it stops reporting, before being dropped. Guards against a producer that dies without sending `gone` (a killed terminal, a crashed hook) pinning its glyph forever. |
| `done_ttl_secs` | integer seconds | `30` | Same idea, but specifically for `done`: a transient "it just finished" marker, not worth holding for the full `ttl_secs` — short enough to notice, too short to become stale furniture. |

## Wiring the widget into zjstatus

The aggregator only publishes a string; zjstatus is what actually renders it.
Both sides must agree on the pipe name:

```kdl
plugins {
    zjstatus location="file:~/.config/zellij/plugins/zjstatus-v0.23.0.wasm" {
        format_right "... {pipe_agents}{pipe_resources} ..."
        pipe_agents_format     "{output}"
        pipe_agents_rendermode "dynamic"
    }
}
```

`pipe_agents_rendermode "dynamic"` is required — zjstatus's pipe widgets
bypass its normal widget cache, and `dynamic` is what makes it re-render on
every incoming pipe message rather than only on its own tick.

## What the widget shows

One `icon + status glyph` group per agent vendor with at least one busy pane,
most urgent first (`error > pending > running > done`), with a count appended
when more than one pane of that vendor is busy (e.g. two Claude panes render
as one grouped entry, not two). The widget renders as an empty string —
disappearing entirely — when nothing is live.

There is no runtime config pipe and no keybinding surface: unlike the old
per-tab sidebar, the aggregator has nothing to toggle at runtime beyond what's
in the `load_plugins` block above. Changing an option means editing
`config.kdl` and starting a new Zellij session (`load_plugins` is read once,
at launch).
