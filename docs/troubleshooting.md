# Troubleshooting

## `{pipe_agents}` never shows anything

The aggregator has no pane and no rendering of its own — it only publishes a
string that zjstatus displays — so "nothing shows up" can be any of a few
independent pieces. Diagnose in order:

1. **The widget hasn't been granted permission yet.** Because a `load_plugins`
   entry has no pane, Zellij's first-run `y`/`n` prompt has nowhere to render
   by default. See [Grant the permission](install.md#3-grant-the-permission-one-time) —
   launch the wasm once as a floating pane and press `y` there. This is a
   one-time, per-machine step (cached per wasm path in `permissions.kdl`); it
   has to happen again only if the wasm's path changes.
2. **`pipe_name` mismatch.** The aggregator's `pipe_name` config key
   (default `pipe_agents`) must match the `pipe_<name>_format` /
   `pipe_<name>_rendermode` keys on your zjstatus plugin block. A mismatch
   means zjstatus is listening for a widget name the aggregator never
   publishes under. See [`configuration.md`](configuration.md).
3. **`pipe_agents_rendermode` isn't `dynamic`.** zjstatus's pipe widgets
   bypass its normal cache; without `rendermode "dynamic"` the widget may not
   repaint when a new value arrives.
4. **No producer is wired.** An empty widget with everything else configured
   correctly usually just means no agent has broadcast a status yet — the
   widget deliberately doesn't guess at agents it can't hear from. Bypass
   producers with the smoke test in
   [Writing your own producer](producers.md#writing-your-own-producer): if a
   fake broadcast makes the widget light up, the aggregator/zjstatus wiring is
   fine and the producer is the problem.
5. **You edited `config.kdl` but didn't start a new session.** `load_plugins`
   is read once, at session launch — editing the file never hot-reloads a
   running session. Start a new one.

## Zellij too old / wrong `zellij-tile` API

The aggregator is pinned to `zellij-tile = "=0.44.3"`
(`crates/agents/Cargo.toml`) and requires Zellij **0.44.3 or newer**
(`zellij --version` to check). Zellij keeps compiled plugins working across
newer releases, so later minors are fine; older ones predate the plugin API
this build targets and the wasm may fail to load outright — no permission
grant will wake it in that case.

## Producer prerequisites

The agent must run *inside* the Zellij session — the hooks no-op without
`$ZELLIJ_PANE_ID` (e.g. a plain terminal, or ssh without Zellij). The Claude
plugin's bash fallback additionally needs `jq`; installing the `zj-radar` CLI
removes that dependency (the hook script prefers it automatically when it's
on `PATH`). See [`producers.md`](producers.md).

## `zellij pipe` calls hang or time out

`zellij pipe` is not fire-and-forget: Zellij holds the client process until
the aggregator's `pipe()` handler returns. That handler does no host I/O and
returns immediately by design (it only republishes from its `Timer` handler,
never from `pipe()` — see the aggregator's own doc comments in
`crates/agents/src/main.rs` for why), so a hang here points elsewhere: an
un-granted permission can still leave the plugin unable to process its
subscriptions correctly. Bundled producers bound every send with a timeout
regardless (`ZJ_RADAR_PIPE_TIMEOUT`, default 5s) — see
[Bound your sends](producers.md#writing-your-own-producer). Third-party
producers should apply the same bound.
