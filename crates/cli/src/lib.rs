//! Native CLI (`zj-radar`): the host front door for the sidebar.
//!
//! Three subcommands, one per module:
//! - `notify <agent>` ([`notify`]) — the *pushed* information source. Reads an
//!   agent's hook payload and broadcasts a `zj_radar.status.v1` update. Each
//!   agent is a peer adapter behind the [`agents::Agent::derive`] seam, so
//!   `notify` stays agent-agnostic.
//! - `setup [codex|zellij]` ([`setup`]) — idempotent wiring: manage Codex
//!   notify/`hooks.json`, install the wasm plugin, and inject the rail into a
//!   Zellij layout ([`layout`]).
//! - `run` ([`run`]) — turnkey launch of a Zellij session that owns its own
//!   config with the rail preinstalled.

// Re-export the shared core so the CLI submodules keep addressing these as
// `crate::status`, `crate::payload`, … with no per-reference churn.
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use zj_radar_core::{command, kind, payload, pipe, status};

use clap::{Parser, Subcommand};

mod agents;
mod fsutil;
mod notify;
mod setup;

/// Process-wide failure flag. The setup/run orchestrators report refusals and
/// write failures by printing a diagnostic and returning early through several
/// layers; they mark the invocation failed here instead of threading a Result
/// through every signature. [`run`] maps it to the process exit code, so
/// `zj-radar setup … && next` composes correctly in scripts and installers.
/// A user *declining* a confirmation prompt is not a failure.
pub(crate) mod exit {
    use std::sync::atomic::{AtomicBool, Ordering};

    static FAILED: AtomicBool = AtomicBool::new(false);

    /// Report a failure as `{label}: {msg}` on stderr AND mark the invocation
    /// failed. This is the only way to set the flag, so a failure diagnostic
    /// can never print without the exit code following it. Raw `eprintln!` is
    /// reserved for warnings, guidance, and continuation lines — anything that
    /// reports a *failure* goes through here.
    pub(crate) fn fail_report(label: &str, msg: impl std::fmt::Display) {
        FAILED.store(true, Ordering::Relaxed);
        eprintln!("{label}: {msg}");
    }

    pub(crate) fn failed() -> bool {
        FAILED.load(Ordering::Relaxed)
    }
}

#[derive(Parser)]
#[command(
    name = "zj-radar",
    version,
    about = "Launch a Zellij session with the zj-radar sidebar, broadcast agent status to it, and wire agents up."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Broadcast one agent's status to the sidebar (called from an agent hook).
    Notify {
        /// Which agent is reporting: `claude`, `codex`, or `generic` (any
        /// script — pass explicit `--status`/`--msg`/`--task` flags, no hook
        /// payload needed).
        agent: String,
        /// Hook payload as a trailing argument (codex). Claude passes it on stdin instead.
        input: Option<String>,
        /// Explicit status (claude hooks pass this; required for `generic`):
        /// running | pending | done | error | idle.
        #[arg(long)]
        status: Option<String>,
        /// Activity line for `generic` (e.g. "deploying site"); shown on the
        /// pane's rail line. Running with no msg gets a "working" baseline.
        #[arg(long)]
        msg: Option<String>,
        /// Sticky task label for `generic`; empty keeps the stored label.
        #[arg(long)]
        task: Option<String>,
        /// Kind mark for `generic`: test | build | deploy | server | command …
        /// (default `generic` → the neutral ⦿ mark).
        #[arg(long)]
        source: Option<String>,
        /// Print the payload instead of broadcasting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Idempotently wire installed agents to use zj-radar.
    Setup {
        /// Targets to set up (default: detected agents only). Supported: claude, codex.
        targets: Vec<String>,
        /// Remove our entries instead of adding them.
        #[arg(long)]
        uninstall: bool,
        /// Show what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Check setup status without writing files. Conflicts with
        /// `--uninstall`: silently running the doctor instead of uninstalling
        /// would read as "uninstalled".
        #[arg(long, conflicts_with = "uninstall")]
        check: bool,
        /// Use Codex's legacy single-slot notify config instead of hooks.json.
        #[arg(long)]
        legacy_notify: bool,
        /// Overwrite conflicting entries where supported.
        #[arg(long)]
        force: bool,
    },
}

/// CLI entry point (called by `src/main.rs`). Returns the process exit code:
/// failure when any orchestrator flagged a refusal/error via [`exit::fail_report`].
pub fn run() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Notify {
            agent,
            input,
            status,
            msg,
            task,
            source,
            dry_run,
        } => {
            if agent == "generic" {
                notify::run_generic(
                    status.as_deref(),
                    msg.as_deref(),
                    task.as_deref(),
                    source.as_deref(),
                    dry_run,
                );
            } else {
                notify::run(&agent, input.as_deref(), status.as_deref(), dry_run);
            }
        }
        Command::Setup {
            targets,
            uninstall,
            dry_run,
            yes,
            check,
            legacy_notify,
            force,
        } => {
            setup::run(setup::SetupOptions {
                targets: &targets,
                uninstall,
                dry_run,
                yes,
                check,
                legacy_notify,
                force,
            });
        }
    }
    if exit::failed() {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
