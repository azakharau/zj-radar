//! `zj-radar setup [claude|codex]` — idempotent, conflict-aware local wiring for
//! the agent producers. Claude is wired through Claude Code's own plugin
//! marketplace (we drive the `claude plugin` CLI, never its files); Codex gets
//! hooks.json entries.
//!
//! There is deliberately no `zellij` target. The status indicator is a
//! `{pipe_agents}` widget in the user's own zjstatus bar, fed by the headless
//! `zj-radar-agents` plugin — both declared in the user's `config.kdl` like any
//! other plugin. Nothing about that is ours to install, inject, or own, so the
//! wasm-installing / layout-injecting / session-launching layer is gone.

mod analyze;
mod check;
mod claude;
mod codex;
mod detect;
mod edit;
pub(crate) use analyze::*;
pub(crate) use check::*;
pub(crate) use claude::*;
pub(crate) use codex::*;
pub(crate) use edit::*;

use std::path::{Path, PathBuf};

/// Our legacy Codex notify invocation — also the idempotency/uninstall marker.
pub(crate) const CODEX_NOTIFY_MARKER: [&str; 3] = ["zj-radar", "notify", "codex"];
// Also used by `run`'s producer detection so the two agree on what marks a wired
// Codex producer (shared single source of truth).
pub(crate) const CODEX_HOOK_MARKER: &str = "ZJ_RADAR_CODEX_HOOK=v1";
pub(crate) const CODEX_HOOK_COMMAND: &str = "ZJ_RADAR_CODEX_HOOK=v1 zj-radar notify codex";
pub(crate) const CODEX_HOOK_COMMAND_WINDOWS: &str =
    "cmd /C \"set ZJ_RADAR_CODEX_HOOK=v1&& zj-radar notify codex\"";
pub(crate) const CODEX_HOOK_EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];

pub struct SetupOptions<'a> {
    pub targets: &'a [String],
    pub uninstall: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub check: bool,
    pub legacy_notify: bool,
    pub force: bool,
}

pub(crate) struct CodexSetupOpts {
    legacy_notify: bool,
    force:         bool,
    dry_run:       bool,
    yes:           bool,
}

/// The single operation a `setup` invocation performs. Resolving this once makes
/// the precedence (grant > check > uninstall > install) explicit instead of
/// implicit in the order of `if` blocks.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Check,
    Uninstall,
    Install,
}

/// Clap already hard-errors on `--check --uninstall`, so the
/// check-beats-uninstall rung is defensive, not a CLI surface.
pub(crate) fn mode_from_flags(check: bool, uninstall: bool) -> Mode {
    if check {
        Mode::Check
    } else if uninstall {
        Mode::Uninstall
    } else {
        Mode::Install
    }
}

pub(crate) fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
        .unwrap_or(false)
}

/// Entry point for `zj-radar setup`.
pub fn run(options: SetupOptions<'_>) {
    let mode = mode_from_flags(options.check, options.uninstall);

    let bare = options.targets.is_empty();
    let want_codex = bare || options.targets.iter().any(|a| a == "codex");
    // Bare `setup` = detected agents only: claude joins codex there, and
    // `setup_claude` itself skips gracefully when the binary is absent.
    let want_claude = bare || options.targets.iter().any(|a| a == "claude");
    for a in options
        .targets
        .iter()
        .filter(|a| !matches!(a.as_str(), "claude" | "codex"))
    {
        crate::exit::fail_report(
            "zj-radar",
            format!("setup does not support '{a}' (supported: claude, codex). Skipping."),
        );
    }

    if mode == Mode::Check {
        let both = bare;
        let mut missing = false;
        if want_codex || both {
            missing |= check_codex(options.legacy_notify);
        }
        // Explicit `setup claude --check` always reports; the bare doctor
        // includes claude only when the binary is present (detected agents),
        // so a claude-less machine's doctor isn't failed by an agent it
        // doesn't have.
        if options.targets.iter().any(|a| a == "claude") || (both && which("claude")) {
            missing |= check_claude();
        }
        if missing {
            // The items above are the diagnostic; this sets the exit code so a
            // caller can gate on the doctor.
            crate::exit::fail_report("zj-radar", "check found missing items (listed above)");
        }
        return;
    }

    let uninstall = mode == Mode::Uninstall;
    if want_codex {
        setup_codex(
            uninstall,
            CodexSetupOpts {
                legacy_notify: options.legacy_notify,
                force:         options.force,
                dry_run:       options.dry_run,
                yes:           options.yes,
            },
        );
    }
    if want_claude {
        setup_claude(uninstall, options.dry_run, options.yes);
    }
}

/// The shared preamble for every `setup_*` step: turn an editor's
/// `Result<Outcome, String>` into an `Option<Outcome>`, reporting a refusal as the
/// standard `{label}: refused — {e}` line and yielding `None` so the caller bails.
/// Centralizes the one diagnostic all three orchestrators printed by hand.
pub(crate) fn edit_or_report(label: &str, edit: Result<Outcome, String>) -> Option<Outcome> {
    match edit {
        Ok(outcome) => Some(outcome),
        Err(e) => {
            crate::exit::fail_report(label, format!("refused — {e}"));
            None
        }
    }
}

pub(crate) fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The shared "commit an edit" tail for every `setup_*` step: prompt (unless
/// `--yes`), run any `pre_write` side effects (e.g. copying the wasm), then write
/// `new` to `path` atomically — emitting the standard `skipped`/`failed`
/// diagnostics under `label`. Returns whether the file was written, so the caller
/// can print its success epilogue. Callers keep `--dry-run` handling and prompt
/// wording, which differ per target. A `pre_write` error is reported as
/// `{label}: {e}`, so its message should read as a sentence without the prefix.
pub(crate) fn confirm_and_write(
    label: &str,
    path: &Path,
    new: &str,
    yes: bool,
    prompt: &str,
    pre_write: impl FnOnce() -> Result<(), String>,
) -> bool {
    if !yes && !confirm(prompt) {
        println!("{label}: skipped (declined)");
        return false;
    }
    if let Err(e) = pre_write() {
        crate::exit::fail_report(label, e);
        return false;
    }
    if let Err(e) = write_atomic(path, new) {
        crate::exit::fail_report(label, format!("write failed — {e}"));
        return false;
    }
    true
}

/// Back up the existing file, then write atomically (temp file + rename via the
/// shared `fsutil::atomic_write`). The `.bak` is specific to `setup` editing the
/// user's own files; `run` writes its owned dir without one. A failed backup
/// aborts the write: the success epilogues advertise the `.bak` as the restore
/// point, so the user's file must never be replaced without it existing.
pub(crate) fn write_atomic(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if path.exists() {
        std::fs::copy(path, path_with_suffix(path, ".zj-radar.bak")).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("backup copy failed ({e}); {} left untouched", path.display()),
            )
        })?;
    }
    crate::fsutil::atomic_write(path, contents.as_bytes())
}

pub(crate) fn path_with_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| format!("{}{}", name.to_string_lossy(), suffix))
        .unwrap_or_else(|| format!("config{suffix}"));
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;



    #[test]
    fn write_atomic_aborts_when_backup_cannot_be_written() {
        // The success epilogues advertise the .bak as the restore point, so a
        // failed backup must abort the write, not overwrite-and-lie. Force the
        // copy to fail by occupying the .bak path with a directory.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.kdl");
        std::fs::write(&target, "original").unwrap();
        std::fs::create_dir(path_with_suffix(&target, ".zj-radar.bak")).unwrap();

        let err = write_atomic(&target, "replacement").unwrap_err();
        assert!(err.to_string().contains("backup copy failed"), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original",
            "target must be untouched when the backup fails"
        );
    }
}
