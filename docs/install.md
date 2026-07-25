# Installing the agent-status widget

This is the full install reference for the **aggregator** — the headless wasm
plugin that feeds the `{pipe_agents}` widget in your zjstatus bar. For the
**producer** (whatever broadcasts agent status to it), see
[`producers.md`](producers.md). For a copy-paste fast path, see
[Quick start](../README.md#quick-start).

**Requirements:** Zellij **0.44.3 or newer** (check with `zellij --version`),
and [zjstatus](https://github.com/dj95/zjstatus) already declared in your
`config.kdl` — the aggregator has no pane and no rendering of its own; it only
publishes a string for zjstatus's `{pipe_agents}` widget to display.

There are two jobs to get a working widget:

1. **Run the aggregator and wire it into zjstatus** — get the wasm onto disk,
   add a `load_plugins` entry for it, and add `{pipe_agents}` to your
   zjstatus `format_right`. *(This page.)*
2. **Send agent status to it** — install the Claude plugin or wire an agent to
   call `zj-radar notify`. *(See [`producers.md`](producers.md).)*

This is intentionally **not** something the `zj-radar` CLI installs, injects,
or manages for you: unlike the old per-tab sidebar, the aggregator is just
another Zellij plugin declared in *your* `config.kdl`, the same way you'd add
any other one. There is no `zj-radar setup zellij` — `setup` only wires
producers (`claude`, `codex`).

## 1. Get the wasm

### Download a release asset

Tagged releases publish `zj_radar_agents.wasm` (plus a `.sha256` checksum)
alongside the CLI tarballs:

```sh
mkdir -p ~/.config/zellij/plugins
cd ~/.config/zellij/plugins
curl -fsSLO https://github.com/marktoda/zj-radar/releases/latest/download/zj_radar_agents.wasm
curl -fsSLO https://github.com/marktoda/zj-radar/releases/latest/download/zj_radar_agents.wasm.sha256
sha256sum -c zj_radar_agents.wasm.sha256   # macOS: shasum -a 256 -c
```

### Build from source instead

```sh
git clone https://github.com/marktoda/zj-radar
cd zj-radar
just install-wasm   # = just build-wasm (cross-compiles to wasm32-wasip1) + copy
                     #   the artifact to ${ZELLIJ_CONFIG_DIR:-~/.config/zellij}/plugins/
```

`just build-wasm` needs the `wasm32-wasip1` target; `rust-toolchain.toml`
requests it, so `rustup` auto-installs it on first build (see
[`TOOLCHAIN.md`](TOOLCHAIN.md)).

### Nix / home-manager

The flake exposes the wasm as `packages.default` (alias `packages.zj-radar`)
and the CLI as `packages.zj-radar-cli`:

```nix
# flake.nix
inputs.zj-radar.url = "github:marktoda/zj-radar";
```

```nix
# home-manager module
home.packages = [inputs.zj-radar.packages.${pkgs.system}.zj-radar-cli];

# Symlink to a STABLE path rather than pointing `load_plugins` at the
# /nix/store path directly: Zellij keys permission grants by the configured
# location string, so a per-build store path re-prompts after every rebuild.
# Rebuilds swap the symlink target; the granted path never changes.
home.file.".config/zellij/plugins/zj_radar_agents.wasm".source =
  "${inputs.zj-radar.packages.${pkgs.system}.default}/bin/zj_radar_agents.wasm";
```

## 2. Wire it into `config.kdl`

Add a `load_plugins` entry for the aggregator (no pane — it's headless) and
point zjstatus's `format_right` at `{pipe_agents}`:

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

`pipe_name` must match between the `load_plugins` block and the
`pipe_<name>_format`/`pipe_<name>_rendermode` keys on your zjstatus config —
zjstatus derives its widget key by stripping the trailing `_format`/
`_rendermode` from the config key, so `pipe_agents_format` expects a payload
published under the name `pipe_agents`. See
[`configuration.md`](configuration.md) for every option.

The fixed wasm path matters for the next step: Zellij ties a plugin's
permission grant to its `load_plugins`/`plugins` location string.

## 3. Grant the permission (one time)

A `load_plugins` plugin has no pane, so Zellij's first-run `y`/`n` permission
prompt has nowhere to render. Launch it once as a floating pane instead:

```sh
zellij action launch-or-focus-plugin "file:$HOME/.config/zellij/plugins/zj_radar_agents.wasm" --floating
```

Press `y` in the floating pane, then close it. The grant is cached per wasm
path in Zellij's `permissions.kdl`, so **replacing the wasm in place** (an
upgrade) keeps the grant — only a path change re-prompts. This has to be done
once per machine, not once per session.

Zellij only reads `load_plugins` at session start, and the plugin's permission
set is `ReadApplicationState` + `MessageAndLaunchOtherPlugins` — nothing that
runs commands or mutates application state.

## 4. Restart Zellij

`load_plugins` entries are read once, at session launch — a new artifact on
disk never hot-swaps into a running session. Start a new session (or restart
Zellij) to pick up the widget. If nothing shows in the bar yet, that's
expected: an empty `{pipe_agents}` renders as nothing until a producer reports
something. See [Producers](producers.md).

## Verifying it's wired up

`zj-radar setup --check` reports on producers (Claude/Codex), not on the
aggregator itself — there is nothing for `setup` to check there, since it
never installed it. To sanity-check the aggregator directly, broadcast a fake
status from inside the session (see
[the smoke test in producers.md](producers.md#writing-your-own-producer)) and
confirm the bar picks it up within about a second.

## Uninstalling

Nothing here is installed *for* you, so nothing needs an uninstaller:

- Remove the `load_plugins` entry and the `pipe_agents*` keys from your
  `config.kdl` yourself.
- `rm ~/.config/zellij/plugins/zj_radar_agents.wasm`.
- To revoke the permission grant, edit the wasm's block out of Zellij's
  `permissions.kdl` (macOS: `~/Library/Caches/org.Zellij-Contributors.Zellij/`;
  Linux: `~/.cache/zellij/`, or `$XDG_CACHE_HOME/zellij`).

Producers are still uninstalled through the CLI: `zj-radar setup claude
--uninstall` / `zj-radar setup codex --uninstall`.
