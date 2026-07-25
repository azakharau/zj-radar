# Toolchain

Native `cargo` builds everything — both host tests and the WASM plugin. The
`wasm32-wasip1` target is requested by `rust-toolchain.toml`, so a `rustup`-managed
toolchain installs it automatically the first time you build. `cargo test` (host
target) covers the pure-logic modules and needs nothing extra.

Dev tracks `stable`; the workspace MSRV is **Rust 1.95** (declared as
`rust-version` in the root `Cargo.toml`, enforced by CI's `msrv` job, which
builds with exactly that toolchain).

```sh
cargo test                                          # host tests
cargo build --release --target wasm32-wasip1 -p zj-radar-agents   # the WASM plugin
# → target/wasm32-wasip1/release/zj_radar_agents.wasm
```

`just build-wasm` is the same wasm build; `just install-wasm` builds it and
copies the artifact to `${ZELLIJ_CONFIG_DIR:-~/.config/zellij}/plugins/`,
where the `load_plugins` entry in [`install.md`](install.md) expects it.

## If your `cargo` lacks the `wasm32-wasip1` target

If you use a non-`rustup` Rust that doesn't pick up the target from
`rust-toolchain.toml` (e.g. a bare Nix-profile toolchain), you'll see
`can't find crate for std … wasm32-wasip1 may not be installed`. Either add the
target to that toolchain, or use the repo's Nix dev shell, which pins a Rust with
the target:

```sh
nix develop -c cargo build --release --target wasm32-wasip1 -p zj-radar-agents
```

## Dev loop

```sh
just install-wasm   # build-wasm + copy the artifact into place
```

Uses the ambient Rust toolchain (`rust-toolchain.toml` auto-installs the wasm
target on first build). In the Nix shell, prefix with `nix develop -c`.

Zellij only reads a `load_plugins` entry at session launch, and does not
hot-reload it — start a new session to pick up a freshly installed wasm. The
permission grant is cached per wasm path, so replacing the file in place
(what `just install-wasm` does) keeps it; see
[Grant the permission](install.md#3-grant-the-permission-one-time).
