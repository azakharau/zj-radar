//! Tab Roll-Up: the per-pane → per-tab aggregation seam.
//!
//! Severity order `error > pending > running > done > idle`, with `done/total`
//! and `pending` counts and a highest-severity detail line. This is the domain
//! operation named "Tab Roll-Up" in `CONTEXT.md`: a deep, pure module that
//! turns a tab's panes plus a per-pane observation lookup into the `TabDisplay`
//! the rail renders. It owns the whole render-input vocabulary — `TabDisplay`,
//! `PaneDisplay`, `PrimaryDetail`, `ProgressCounts`, `Outcome`, plus the
//! rail-row types `TabRow`/`LedgerLine` and the topology record
//! `TerminalPane` — so the arrows run one way: `radar_state` builds these,
//! `render` consumes them, and neither imports the other.
//!
//! The "two sources, status wins" knowledge lives in the caller's `resolve`
//! closure — `roll_up` never learns there is more than one store, which keeps
//! the source seam (`StatusStore` / `CommandStore`) free to evolve.

use crate::kind::Kind;
use crate::observation::{ObservationOrigin, TrackedObservation};
use crate::status::Status;

/// One terminal pane of a tab's topology — the input record `roll_up`
/// aggregates and `RadarState` stores per tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalPane {
    pub id: u32,
    pub title: String,
    pub focused_in_tab: bool,
    /// The pane's root process reports lifecycle through the status pipe, so
    /// foreground child processes must not open command-origin observations.
    pub push_owned: bool,
}

/// The end-result of a finished *command* pane, shown as a tag after the
/// activity (`cargo build exit 1`; `Ok` renders no tag — the line-1 status
/// glyph is the one done signal). Built in `rollup::roll_up`; agents never
/// carry one. Kept structured (not baked into
/// `msg`) so the renderer can reserve its width — the outcome survives
/// truncation while the command absorbs the squeeze — and color it
/// independently of the (dim) command text. The display methods
/// (`full`/`minimal`/`role`) live in `render`, since they encode glyphs and a
/// width-driven form; the enum here is pure semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0 / returned to the shell with no failure evidence.
    Ok,
    /// Nonzero exit; `Some(code)` when known, `None` for a signal/no-code exit.
    Failed(Option<i32>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryDetail {
    pub repo: String,
    pub branch: String,
    pub msg: String,
    pub task: String,
    pub since_tick: u64,
    pub status: Status,
    pub kind: Kind,
    /// End-result tag for a finished command pane (None for agents/active).
    pub outcome: Option<Outcome>,
    /// Wall-clock stamp of the waiting-on-you edge (Pending only) — the
    /// renderer turns it into the `· 12m` wait tag against its own
    /// `now_epoch_s`, so no epoch threads through the roll-up itself.
    pub pending_epoch_s: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneDisplay {
    Tracked {
        pane_id: u32,
        kind: Kind,
        status: Status,
        msg: String,
        task: String,
        since_tick: u64,
        outcome: Option<Outcome>,
        /// Waiting-on-you stamp (Pending only) — see `PrimaryDetail`.
        pending_epoch_s: Option<u64>,
        /// This row came from an explicit status-pipe producer (not the
        /// command heuristic). An announced row with an unrecognized source
        /// (`Kind::Other`, e.g. a generic status producer) still counts
        /// as an agent for the agents-only rail — the producer vouched for it.
        announced: bool,
        /// The observation has ever been active. An Idle row that HAS worked
        /// is a leftover (agent exited or was cleared) — the agents-only rail
        /// hides it, while a never-active Idle row is a fresh identity
        /// announcement from a live agent and stays visible.
        ever_active: bool,
        /// This pane is the focused pane of its tab (session-wide fact from
        /// the PaneManifest, identical for every rail instance). The renderer
        /// combines it with `TabRow.active` to bold the agent you're in.
        focused: bool,
    },
    Untracked {
        pane_id: u32,
        title: String,
    },
}

impl PaneDisplay {
    pub(crate) fn untracked(pane_id: u32, title: &str) -> Self {
        let title = if title.trim().is_empty() {
            "terminal".to_string()
        } else {
            title.to_string()
        };
        Self::Untracked { pane_id, title }
    }

    pub(crate) fn is_tracked(&self) -> bool {
        matches!(self, Self::Tracked { .. })
    }

    /// Whether this pane belongs on the agents-only rail: a recognized
    /// agent kind from either origin, or any explicitly announced
    /// (status-pipe) row whose source didn't classify — `Kind::Other`
    /// covers generic status producers; command-heuristic rows with non-agent kinds
    /// stay hidden. Idle agents STAY on the rail (the sidebar's contract is
    /// "every open agent in the session, with status"); rows leave only
    /// when the producer says `gone` or the pane closes.
    pub(crate) fn is_agent(&self) -> bool {
        match self {
            Self::Tracked {
                kind, announced, ..
            } => kind.is_agent() || (*announced && *kind == Kind::Other),
            Self::Untracked { .. } => false,
        }
    }

    pub(crate) fn is_focused(&self) -> bool {
        matches!(self, Self::Tracked { focused: true, .. })
    }

    pub(crate) fn pane_id(&self) -> u32 {
        match self {
            Self::Tracked { pane_id, .. } | Self::Untracked { pane_id, .. } => *pane_id,
        }
    }

    pub(crate) fn status(&self) -> Option<Status> {
        match self {
            Self::Tracked { status, .. } => Some(*status),
            Self::Untracked { .. } => None,
        }
    }

    pub(crate) fn render_status(&self) -> Status {
        self.status().unwrap_or(Status::Idle)
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Tracked { kind, .. } => *kind,
            Self::Untracked { .. } => Kind::Other,
        }
    }

    pub(crate) fn msg(&self) -> &str {
        match self {
            Self::Tracked { msg, .. } => msg,
            Self::Untracked { title, .. } => title,
        }
    }

    pub(crate) fn task(&self) -> &str {
        match self {
            Self::Tracked { task, .. } => task,
            Self::Untracked { .. } => "",
        }
    }

    pub(crate) fn outcome(&self) -> Option<Outcome> {
        match self {
            Self::Tracked { outcome, .. } => *outcome,
            Self::Untracked { .. } => None,
        }
    }

    /// Waiting-on-you stamp (Pending only) — feeds `render::wait_tag`.
    pub(crate) fn pending_epoch_s(&self) -> Option<u64> {
        match self {
            Self::Tracked { pending_epoch_s, .. } => *pending_epoch_s,
            Self::Untracked { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabDisplay {
    pub status: Status,
    pub progress: ProgressCounts,
    pub detail: Option<PrimaryDetail>,
    pub panes: Vec<PaneDisplay>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressCounts {
    pub done: usize,
    pub total: usize,
    pub pending: usize,
}

/// Roll a tab's panes up into a single `TabDisplay`.
///
/// `resolve` maps a pane id to its resolved observation, if any. The caller owns
/// the precedence across observation sources (status pipe vs command); this
/// function only sees "is there an observation for this pane?".
///
/// A pane with no observation renders as untracked. A never-active observation
/// does too, unless it is an agent identity announced through the status pipe.
/// Only ever-active observations count toward `done/total/pending`.
pub fn roll_up<'a>(
    panes: &[TerminalPane],
    resolve: impl Fn(u32) -> Option<&'a TrackedObservation>,
) -> TabDisplay {
    let mut best: Option<PrimaryDetail> = None;
    let mut done = 0usize;
    let mut total = 0usize;
    let mut pending = 0usize;
    let mut pane_displays = Vec::with_capacity(panes.len());

    for pane in panes {
        let Some(s) = resolve(pane.id) else {
            pane_displays.push(PaneDisplay::untracked(pane.id, &pane.title));
            continue;
        };

        if s.ever_active {
            total += 1;
            if s.status == Status::Done {
                done += 1;
            }
            // Counted with `total`/`done`, not outside the gate: a pane excluded
            // from `total` (never ever_active, e.g. a snapshot-loaded row) must
            // not inflate `pending`, or progress reads inconsistent (pending > total).
            if s.status == Status::Pending {
                pending += 1;
            }
        }
        let announced = s.origin == ObservationOrigin::StatusPipe;
        if s.ever_active || (announced && (s.kind.is_agent() || s.kind == Kind::Other)) {
            // The sticky task label prefers what the producer sent; when it
            // never did (resumed omp sessions never surface their name
            // through the extension API), fall back to the pane title omp
            // itself maintains — `π: <session name>` — which is exactly the
            // name the user sees in omp's own UI.
            let task = if s.task.is_empty() {
                pane.title
                    .strip_prefix("π: ")
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string()
            } else {
                s.task.clone()
            };
            pane_displays.push(PaneDisplay::Tracked {
                pane_id: pane.id,
                kind: s.kind,
                status: s.status,
                msg: s.msg.clone(),
                task,
                since_tick: s.last_change_tick,
                outcome: pane_outcome(s),
                pending_epoch_s: s.pending_epoch_s,
                announced,
                ever_active: s.ever_active,
                focused: pane.focused_in_tab,
            });
        } else {
            pane_displays.push(PaneDisplay::untracked(pane.id, &pane.title));
        }
        // Most-urgent active pane wins, ties broken by most-recent change.
        // `Status: Ord` ranks severity, so this is a single lexicographic
        // `(status, tick)` compare — `>=` keeps the last pane on a full tie.
        if s.status.is_active() {
            let key = (s.status, s.last_change_tick);
            let wins = best
                .as_ref()
                .is_none_or(|d| key >= (d.status, d.since_tick));
            if wins {
                best = Some(PrimaryDetail {
                    repo: s.repo.clone(),
                    branch: s.branch.clone(),
                    msg: s.msg.clone(),
                    task: s.task.clone(),
                    since_tick: s.last_change_tick,
                    status: s.status,
                    kind: s.kind,
                    outcome: pane_outcome(s),
                    pending_epoch_s: s.pending_epoch_s,
                });
            }
        }
    }

    TabDisplay {
        status: best.as_ref().map_or(Status::Idle, |d| d.status),
        progress: ProgressCounts {
            done,
            total,
            pending,
        },
        detail: best,
        panes: pane_displays,
    }
}

/// Derive the end-result outcome tag for a pane, scoped to *command-origin*
/// panes — agents (status pipe) keep their hook msg with no tag. Done → `Ok`
/// (no tag; the line-1 status glyph is the one done signal); Error →
/// `Failed(exit_code)` (`exit N`, or `✗` when the code is unknown). Returns
/// `None` for active/idle panes and all agents.
fn pane_outcome(s: &TrackedObservation) -> Option<Outcome> {
    if s.origin != ObservationOrigin::Command {
        return None;
    }
    match s.status {
        Status::Done => Some(Outcome::Ok),
        Status::Error => Some(Outcome::Failed(s.exit_code)),
        _ => None,
    }
}

/// One rail row as the renderer consumes it: the tab's identity bits plus its
/// rolled-up [`TabDisplay`]. Built by `RadarState::rows`; `render_rail` never
/// reaches past it into state.
#[derive(Debug)]
pub struct TabRow {
    pub number: u32,
    pub name: String,
    pub active: bool,
    pub has_bell: bool,
    /// True for the two ticks after this tab's pane flipped from not-Pending
    /// to Pending (`RadarState::flash_until`) — the one-shot "ping" that
    /// outranks the active tint in `card_tint` in the renderer.
    pub flash: bool,
    pub display: TabDisplay,
}

/// A ledger entry, resolved for rendering: the live tab position (or `None`
/// once that tab is gone, making the row click-inert) looked up fresh on every
/// call, rather than cached — the ledger itself only ever remembers the
/// `TabId` it happened in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LedgerLine {
    pub at_epoch_s: u64,
    pub error: bool,
    pub tab_name: String,
    pub label: String,
    pub tab_position: Option<usize>,
}

#[cfg(test)]
mod tests;
