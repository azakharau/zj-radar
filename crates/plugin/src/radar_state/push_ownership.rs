use super::{RadarChange, RadarState};
use crate::command;
use crate::config;
use crate::kind::Kind;
use crate::observation::{ObservationOrigin, TrackedObservation};
use crate::payload;
use crate::rollup::TerminalPane;
use crate::status::Status;
use std::collections::{HashMap, HashSet};

/// Tracks panes whose status producer owns the pane lifecycle. Foreground
/// children in these panes are implementation details, not separate command
/// work for the radar.
#[derive(Default)]
pub(super) struct PushOwnership {
    /// Panes whose root terminal process is a push producer. Zellij only
    /// reports this for producers launched directly rather than from a shell.
    root: HashSet<u32>,
    /// Shell-launched producers learned from their CommandChanged edge, status
    /// payload, or a restored snapshot.
    learned: HashSet<u32>,
}

impl PushOwnership {
    /// Restore learned ownership before snapshot observations are routed. A
    /// restored remote OMP status must suppress a stale command observation
    /// for the same pane immediately.
    pub(super) fn restore(&mut self, observations: &[(u32, TrackedObservation)]) {
        self.learned = observations
            .iter()
            .filter_map(|(pane_id, observation)| {
                (observation.origin == ObservationOrigin::StatusPipe
                    && observation.kind == Kind::Omp)
                    .then_some(*pane_id)
            })
            .collect();
    }

    /// Replace direct-root ownership from the latest level-triggered manifest.
    pub(super) fn update_manifest(
        &mut self,
        tab_panes: &HashMap<usize, Vec<TerminalPane>>,
    ) {
        self.root = tab_panes
            .values()
            .flatten()
            .filter(|pane| pane.push_owned)
            .map(|pane| pane.id)
            .collect();
    }

    /// Apply one foreground-command edge and report whether the pane remains
    /// producer-owned after it.
    pub(super) fn on_command(
        &mut self,
        pane_id: u32,
        command: &[String],
        is_foreground: bool,
        at_shell: bool,
    ) -> bool {
        if command::is_push_producer_foreground(command, is_foreground) {
            self.learned.insert(pane_id);
        } else if at_shell {
            // A shell-launched producer returned to its actual prompt. Direct
            // root producers remain owned through `root`.
            self.learned.remove(&pane_id);
        }
        self.owns(pane_id)
    }

    /// Learn ownership from a producer's status payload. Returns true when the
    /// payload source is authoritative for the pane lifecycle.
    pub(super) fn on_status(&mut self, pane_id: u32, source: &str) -> bool {
        if !command::is_push_producer_program(source) {
            return false;
        }
        self.learned.insert(pane_id);
        true
    }

    pub(super) fn retain_live(&mut self, live: &HashSet<u32>) {
        self.learned.retain(|pane_id| live.contains(pane_id));
    }

    pub(super) fn owns(&self, pane_id: u32) -> bool {
        self.root.contains(&pane_id) || self.learned.contains(&pane_id)
    }

    pub(super) fn is_root(&self, pane_id: u32) -> bool {
        self.root.contains(&pane_id)
    }

    pub(super) fn root_panes(&self) -> &HashSet<u32> {
        &self.root
    }

    pub(super) fn learned_panes(&self) -> &HashSet<u32> {
        &self.learned
    }
}

impl RadarState {
    /// Unlike the other mutating entry points, this one takes no `now_epoch_s`:
    /// the displaced observation `clear_on_prompt_return` hands back already
    /// carries its own `completed_epoch_s` stamp from when it first completed,
    /// so `LedgerEntry::from_observation` needs no fresh epoch here — and
    /// `CommandChanged` is the chattiest event in the system, so the caller
    /// shouldn't pay a clock read for a value nothing consumes.
    pub(crate) fn command_changed(
        &mut self,
        pane_id: u32,
        command: &[String],
        is_foreground: bool,
        tick: u64,
    ) -> RadarChange {
        let at_shell = crate::command::is_shell_prompt(command, is_foreground);
        let push_owned =
            self.push_ownership.on_command(pane_id, command, is_foreground, at_shell);
        let cwd = self.pane_cwd.get(&pane_id).map(String::as_str);
        let command_cleared = if push_owned {
            self.command.clear_push_owned(pane_id)
        } else {
            self.command
                .on_command_changed(pane_id, command, is_foreground, cwd, tick)
        };
        // A pane back at its shell prompt means the agent that was pushing status
        // has exited (no producer hook fires on quit), so clear the now-stale
        // pushed status → idle. This rides the shared `CommandChanged` signal, so
        // every tab's instance clears in lockstep. A Running status is not
        // cleared immediately — `clear_on_prompt_return` starts a grace clock
        // instead, so a mid-turn foreground flicker to a shell can't be
        // mistaken for the agent exiting, while an agent killed mid-turn still
        // expires to idle on the timer (`expire_stale_running`).
        let status_cleared = if push_owned {
            // Child commands cannot prove that the root producer exited. A
            // pane close is carried by PaneUpdate and the producer's own hook
            // owns status transitions while the root process remains live.
            self.status.cancel_running_suspect(pane_id);
            false
        } else if at_shell {
            match self.status.clear_on_prompt_return(pane_id, tick) {
                Some(receded) => {
                    // Status-origin recede: the shadow filter never suppresses
                    // it — it only ever applies to Command-origin observations.
                    self.ledger_recede_now(vec![(pane_id, receded)]);
                    true
                }
                None => false,
            }
        } else {
            // The agent's exe back in the foreground resolves a mid-turn
            // flicker: cancel any stale-Running grace clock the shell blip
            // started. Other foregrounds don't vouch — a command run in the
            // shell an agent died in must not keep its ghost alive.
            if crate::command::is_agent_foreground(command, is_foreground) {
                self.status.cancel_running_suspect(pane_id);
            }
            false
        };
        RadarChange {
            render: true,
            settle: false,
            // Persist only when we actually cleared, so a newly-opened tab
            // rehydrates the idle from the snapshot rather than the stale status.
            persist_snapshot: command_cleared || status_cleared,
            ..RadarChange::default()
        }
    }

    pub(crate) fn status_pipe(
        &mut self,
        raw: &str,
        tick: u64,
        now_epoch_s: u64,
        naming: config::NamingMode,
    ) -> Option<RadarChange> {
        let p = payload::parse(raw)?;
        let pane_id = p.pane_id;
        // `gone: true` — the producer says the agent behind the pane exited.
        // Drop the observation outright (the row vanishes; no idle residue),
        // receding a departing completion to the ledger exactly like the
        // return-to-shell clear. A gone for an untracked pane is a no-op.
        if p.gone {
            let removed = self.status.remove(pane_id)?;
            self.ledger_recede_now(vec![(pane_id, removed)]);
            return Some(RadarChange {
                render: true,
                settle: false,
                persist_snapshot: true,
                ..RadarChange::default()
            });
        }
        if self.push_ownership.on_status(pane_id, &p.source) {
            self.command.clear_push_owned(pane_id);
        }
        // Captured BEFORE `apply` overwrites the store: the ping flash fires
        // only on a LIVE not-Pending → Pending edge, never on a re-broadcast of
        // an already-Pending status (spec's "flip", not "is"). Snapshot load
        // never touches this map at all, so a restored Pending never flashes.
        let was_pending = self.status.get(pane_id).map(|o| o.status) == Some(Status::Pending);
        let flips_to_pending = p.status == Status::Pending && !was_pending;
        // A Done/Error that recedes on overwrite (a new broadcast for the same
        // pane, INCLUDING the `/clear` idle-overwrite edge) hands off here.
        if let Some(displaced) = self.status.apply(p, tick, now_epoch_s) {
            // Status-origin recede: never suppressed (see `command_changed`'s
            // matching call site).
            self.ledger_recede_now(vec![(pane_id, displaced)]);
        }
        if flips_to_pending {
            if let Some((tab_id, _)) = self.pane_tab_index().get(&pane_id) {
                self.flash_until.insert(*tab_id, tick + 2);
            }
        }
        // NOTE: we deliberately do NOT settle here. A pushed status is shown as-is;
        // focus no longer recedes or clears it. A completion clears only via a new
        // broadcast for the pane, the return-to-shell exit-clear
        // (`command_changed` → `clear_on_prompt_return`), or a prune.
        Some(RadarChange {
            render: true,
            persist_snapshot: true,
            renames: self.rename_tabs(naming),
            cwd_bootstrap: Vec::new(),
            settle: false,
        })
    }
}
