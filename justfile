# Deterministic suite: host target. The aggregator's logic lives in
# crates/agents/src/lib.rs and is pure, so it runs here natively.
test:
    cargo test --all-features

# Bash hook + installer tests (requires bats + shellcheck + jq on PATH).
# Builds the CLI first: parity.bats compares the bash producer against
# target/debug/zj-radar. Covers every shipped shell script: notify.sh (the
# Claude producer hook), install.sh (the curl|sh release asset — see
# scripts/tests/install.bats), and funnel.sh (the fresh-machine CI check).
test-bash:
    cargo build -p zj-radar
    shellcheck plugins/zj-radar-claude/scripts/notify.sh scripts/install.sh
    bats plugins/zj-radar-claude/tests scripts/tests

# Lint the whole workspace; warnings are errors (matches CI).
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Compile the headless aggregator to wasm (matches CI's wasm step, so a
# wasm-glue-only breakage fails locally too).
build-wasm:
    cargo build --release --target wasm32-wasip1 -p zj-radar-agents

# Install the freshly built aggregator where `load_plugins` in the user's
# config.kdl expects it.
#
# Zellij loads a plugin once per session AT LAUNCH; a new artifact on disk never
# hot-swaps into a running session. Start a NEW session to pick this up — and
# note the plugin's permissions are cached per wasm path in Zellij's
# permissions.kdl, so replacing the file in place keeps the existing grant.
install-wasm: build-wasm
    cp target/wasm32-wasip1/release/zj_radar_agents.wasm \
       "${ZELLIJ_CONFIG_DIR:-$HOME/.config/zellij}/plugins/zj_radar_agents.wasm"
    @echo "installed — start a NEW zellij session to load it"

# Everything a PR must pass locally.
ci: test clippy build-wasm test-bash
