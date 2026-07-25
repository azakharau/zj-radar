//! Integration tests for `zj-radar setup codex` — default hooks.json path.
//!
//! main's `tests/cli.rs` covers: one real run → writes hooks.json with the
//! ZJ_RADAR_CODEX_HOOK=v1 marker (without touching a foreign notify slot).
//!
//! NEW coverage added here:
//!   1. dry-run does NOT write hooks.json; positive control: real run DOES write.
//!   2. idempotency: two real runs → identical hooks.json; first run is non-vacuous.
//!
//! All tests isolate via CODEX_HOME pointing to a tempdir. The `codex_installed()`
//! guard inside setup.rs accepts a pre-existing hooks.json, so we seed the
//! tempdir with an empty `{}` to satisfy it without needing a fake binary on PATH.

mod support;

use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

const HOOK_MARKER: &str = "ZJ_RADAR_CODEX_HOOK=v1 zj-radar notify codex";

/// Returns a fresh tempdir with an empty hooks.json pre-created so that
/// `codex_installed()` returns true (it accepts an existing hooks.json).
fn isolated_codex_home() -> TempDir {
    let dir = TempDir::new().unwrap();
    // seed an empty JSON object — codex_installed() checks hooks_path().exists()
    fs::write(dir.path().join("hooks.json"), "{}\n").unwrap();
    dir
}

// ── Test 1: dry-run does not write; positive control confirms it would have ─

#[test]
fn setup_dry_run_does_not_write_hooks_json() {
    let codex_home = isolated_codex_home();
    let hooks_path = codex_home.path().join("hooks.json");

    // dry-run must leave hooks.json unchanged (still the empty `{}` seed)
    Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "codex", "--dry-run", "--yes"])
        .env("CODEX_HOME", codex_home.path())
        .assert()
        .success();

    let after_dry_run = fs::read_to_string(&hooks_path).unwrap();
    assert_eq!(
        after_dry_run.trim(),
        "{}",
        "dry-run must not modify hooks.json; got: {after_dry_run:?}"
    );

    // Positive control: the same CODEX_HOME without --dry-run MUST install our hooks.
    Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "codex", "--yes"])
        .env("CODEX_HOME", codex_home.path())
        .assert()
        .success();

    let after_real = fs::read_to_string(&hooks_path).unwrap();
    assert!(
        after_real.contains(HOOK_MARKER),
        "real run must have written our hook command; got: {after_real:?}"
    );
    // Verify the file has the expected shape: our marker appears for multiple events
    assert!(
        after_real.contains("\"Stop\""),
        "hooks.json must contain the Stop event"
    );
    assert!(
        after_real.contains("\"PermissionRequest\""),
        "hooks.json must contain the PermissionRequest event"
    );
}

// ── Test 2: idempotency ─────────────────────────────────────────────────────

#[test]
fn setup_codex_hooks_is_idempotent() {
    let codex_home = isolated_codex_home();
    let hooks_path = codex_home.path().join("hooks.json");

    let run = || {
        Command::cargo_bin("zj-radar")
            .unwrap()
            .args(["setup", "codex", "--yes"])
            .env("CODEX_HOME", codex_home.path())
            .assert()
            .success();
    };

    // First run installs
    run();
    let after_first = fs::read_to_string(&hooks_path).unwrap();

    // Non-vacuous: first run actually wrote our hook
    assert!(
        after_first.contains(HOOK_MARKER),
        "first run must have written our hook command; got: {after_first:?}"
    );

    // Second run must not change the file
    run();
    let after_second = fs::read_to_string(&hooks_path).unwrap();

    assert_eq!(
        after_first, after_second,
        "second setup must be a no-op (idempotent)"
    );
}

// ── Test 2b: codex hook guidance mentions disabled hooks when config says so ──
//
// `print_codex_hook_guidance` writes the `hooks appear disabled` warning to
// STDERR (it's a warning) and the `run \`/hooks\`` line to STDOUT — always,
// disabled or not. Both cases reach it via the same `--yes` install (a fresh
// hooks.json install still lands on the guidance-printing tail).

#[test]
fn setup_codex_guidance_warns_when_hooks_feature_disabled() {
    let codex_home = isolated_codex_home();
    fs::write(
        codex_home.path().join("config.toml"),
        "[features]\nhooks = false\n",
    )
    .unwrap();

    let output = Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "codex", "--yes"])
        .env("CODEX_HOME", codex_home.path())
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stderr.contains("hooks appear disabled"),
        "config.toml with [features]\\nhooks = false must warn on stderr; got stderr: {stderr:?}"
    );
    assert!(
        stdout.contains("run `/hooks`"),
        "guidance must still print the /hooks reminder on stdout; got stdout: {stdout:?}"
    );
}

#[test]
fn setup_codex_guidance_silent_on_disabled_warning_when_hooks_enabled() {
    // No config.toml at all: `[features].hooks` is unset, so hooks are
    // enabled-or-unset — the disabled warning must not appear, but the
    // `/hooks` reminder still must.
    let codex_home = isolated_codex_home();

    let output = Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "codex", "--yes"])
        .env("CODEX_HOME", codex_home.path())
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        !stderr.contains("hooks appear disabled"),
        "no config.toml means hooks are not disabled; got stderr: {stderr:?}"
    );
    assert!(
        stdout.contains("run `/hooks`"),
        "guidance must print the /hooks reminder on stdout; got stdout: {stdout:?}"
    );
}

// ── Test 3: `--wasm` and `--download` are mutually exclusive ─────────────────
// The guard must short-circuit before any download or config write.


// ── Test: an interrupted download never leaves a partial wasm behind ─────────
// curl/wget write the destination incrementally, so a killed transfer used to
// leave a partial file that the exists()/up-to-date gates then treated as a
// valid wasm forever after (and Zellij would load it with permissions). The
// download must stage to a `.part` sibling and clean it up on failure.


// ── Test: setup/check operate on the layout Zellij actually loads ────────────
// The layout name resolves --layout → config's `default_layout` → "default".
// Before this, both hardcoded default.kdl: a `default_layout "main"` user got
// the rail injected into a file Zellij never reads, and --check contradicted a
// successful `--inject --layout my` install.


// ── Test: the doctor is scriptable ───────────────────────────────────────────
// Missing items set the exit code so a caller can gate on the doctor. There is
// no longer a `zellij:` half to report: the indicator is a `{pipe_agents}`
// widget the user declares in their own config.kdl, so setup neither installs
// nor inspects it.

#[test]
fn bare_check_reports_the_agent_producers_and_gates_the_exit_code() {
    let home = TempDir::new().unwrap(); // empty: the codex half is all Missing
    let output = Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "--check"])
        .env("CODEX_HOME", home.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("codex:"),
        "bare --check must report the codex half; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("zellij:"),
        "the zellij target is gone; --check must not claim to inspect it; got:\n{stdout}"
    );
    assert!(
        !output.status.success(),
        "missing items must exit non-zero so scripts can gate on the doctor"
    );
}

#[test]
fn setup_refuses_the_removed_zellij_target() {
    let output = Command::cargo_bin("zj-radar")
        .unwrap()
        .args(["setup", "zellij"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("supported: claude, codex"),
        "refusal must name the surviving targets; got:\n{stderr}"
    );
    assert!(!output.status.success(), "an unsupported target must exit non-zero");
}

