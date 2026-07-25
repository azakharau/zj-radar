//! Deep runtime module: repo-owned events in, ordered host effects out.
//! No zellij-tile dependency.
//!
//! [`PluginRuntime`] is the pure state machine behind the wasm glue in
//! `lib.rs`. The glue translates each Zellij host event into one method call
//! here, and every mutating call returns an [`Outcome`] (a render flag plus an
//! ordered list of [`Effect`]s the glue replays against the host). Because the
//! mapping is total and side-effect-free, host tests drive these entry points
//! directly and assert on the returned `Outcome` — no Zellij needed.
//!
//! # Event entry points
//! - [`load`](PluginRuntime::load) — first run: seed config/snapshot, begin the
//!   permission flow.
//! - [`tabs_changed`](PluginRuntime::tabs_changed) /
//!   [`panes_changed`](PluginRuntime::panes_changed) /
//!   [`cwd_changed`](PluginRuntime::cwd_changed) — Zellij topology updates.
//! - [`command_changed`](PluginRuntime::command_changed) — a pane's foreground
//!   process changed (the *observed* information source).
//! - [`status_pipe`](PluginRuntime::status_pipe) — a `zj_radar.status.v1`
//!   broadcast from an agent hook (the *pushed* information source).
//! - [`config_pipe`](PluginRuntime::config_pipe) /
//!   [`control`](PluginRuntime::control) /
//!   [`control_pipe`](PluginRuntime::control_pipe) — runtime config + remote
//!   commands.
//! - [`session_name_changed`](PluginRuntime::session_name_changed) —
//!   `Event::ModeUpdate`'s `session_name`, the push-style source that
//!   replaced `SessionUpdate` for learning this session's own name
//!   (task-8b-brief.md: `SessionUpdate`'s peer list never populates without
//!   a plugin calling the blocking `get_session_list()`, which stock
//!   zj-radar never does — see `task-8-report.md`). Liveness itself no
//!   longer comes from a Zellij-reported peer list at all: it's implicit in
//!   which presence files `session_files::read_peer_presences` returns
//!   (its mtime gate IS the liveness signal).
//! - [`presences_changed`](PluginRuntime::presences_changed) — peer presence
//!   files, freshly read from disk.
//! - [`timer`](PluginRuntime::timer) — periodic tick (animation +
//!   permission-flow coordination + cross-session cycle commit).
//! - [`mouse_click`](PluginRuntime::mouse_click) — resolved against the cached
//!   [`RenderedRail`] for click-to-switch.
//! - [`permission_result`](PluginRuntime::permission_result) — Zellij's grant /
//!   deny verdict.

mod timer;

use crate::control::Verb;
use crate::config;
use crate::permission::{PermissionMarker, PermissionPolicy, PermissionProbe, PermissionState, Transition};
use crate::presence::Presence;
use crate::radar_state::{Direction, PaneUpdate, RadarChange, RadarState, RadarTab};
use crate::render::{self, RenderedRail};
use crate::rollup::TabRow;
use crate::sessions::{CommitTarget, Sessions};
use crate::status::Status;
use crate::tab_namer::TabRename;
use crate::theme;
use std::collections::BTreeMap;
pub(crate) use timer::{Cadence, FAST_FRAMES_PER_DOMAIN_TICK};
use timer::TimerChain;
/// Domain ticks between unconditional own-presence mtime heartbeats.
const PRESENCE_HEARTBEAT_TICKS: u64 = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    RequestPermission,
    SetSelectable(bool),
    SetTimeout(Cadence),
    PersistSnapshot,
    PersistPermissionMarker(PermissionMarker),
    RenameTab { position: usize, name: String },
    SwitchTab { position: usize },
    ShowPane { pane_id: u32 },
    /// Read these panes' working directories once (blocking `get_pane_cwd`) to
    /// bootstrap a name for a freshly-opened tab before it emits `CwdChanged`.
    ///
    /// Unlike the other (fire-and-forget) effects, this one is a *request*: the
    /// glue feeds each result back through `cwd_changed`, which re-enters the
    /// runtime and may itself emit `RenameTab`. The recursion is bounded —
    /// `cwd_changed` never emits another `ResolveCwd` — but note that this
    /// effect's full consequences are realized in that second pass, not in the
    /// `Outcome` that carried it.
    ResolveCwd { pane_ids: Vec<u32> },
    /// Close this plugin's own pane. Emitted by the onboarding floating pane
    /// after permission is granted — it has served its purpose. Needs no Zellij
    /// permission (`close_self` is always allowed).
    CloseSelf,
    /// Refresh the shared permission lock's mtime. Emitted each tick while this
    /// instance's own request is in-flight (`Requesting`), so waiting peers
    /// never see the lock go stale while a user is still looking at a live
    /// prompt — `reclaim_if_stale` (and with it the deferring rails' patience
    /// escalation) only ever fires once the prompt-owner is actually gone.
    HeartbeatPermissionLock,
    /// Show a desktop notification. `key` is the event's cross-instance
    /// identity (`notify_rules::claim_key`): every per-tab instance computes
    /// the same edge and emits this same effect, and the host layer uses the
    /// key to elect exactly one dispatcher (`SessionFiles::claim_notification`)
    /// so N visited tabs don't produce N identical toasts.
    Notify { key: String, title: String, body: String },
    /// Publish this session's own [`Presence`] for peer rails to read.
    /// Content-compared at the edge in `project` — lib.rs does
    /// `files.persist_presence(&runtime.presence_json())`.
    PersistPresence,
    /// Re-read every peer session's presence file and feed the result back
    /// through `presences_changed`. Both Fast domain edges and named-session
    /// Idle fires scan at 1 Hz, so peer staleness and recovery do not depend
    /// on unrelated agent activity.
    ReadPresences,
    /// Commit a cross-session cycle selection: switch to `name` and, once
    /// there, jump straight to the tab that needs attention (if any).
    /// Emitted by `timer` when `Sessions::tick` reports an idle commit.
    SwitchSession { name: String, tab_position: Option<usize> },
    /// Re-broadcast a `zj_radar.status.v1` payload over the shared pipe —
    /// `payload` is already wire-encoded (`payload::to_wire`), ready to hand
    /// to `zellij pipe`. Dismissing a left-clicked pending pane must be a SHARED
    /// signal (issue
    /// #5's design constraint — clearing it only in the clicking instance
    /// repeats the removed focus-clear mistake, where a background tab's
    /// rail never learns the card changed). So the click never mutates
    /// `radar` directly; it only ever produces this effect, and every
    /// instance — including the one that clicked, via the same fan-out a
    /// real producer's broadcast gets — converges through the normal
    /// `status_pipe` → `StatusStore::apply` intake once the pipe echoes it
    /// back. `apply`'s existing identical-rebroadcast no-op (same
    /// `(status, msg)` twice: no `completed_epoch_s` re-stamp, no second
    /// ledger edge) is what makes that convergence idempotent rather than a
    /// second race.
    BroadcastStatus { payload: String },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub render: bool,
    pub effects: Vec<Effect>,
}

impl Outcome {
    fn none() -> Self {
        Self::default()
    }

    fn render() -> Self {
        Self::with_effects(true, vec![])
    }

    fn with_effects(render: bool, effects: Vec<Effect>) -> Self {
        Self { render, effects }
    }
}

#[derive(Default)]
pub(crate) struct PluginRuntime {
    pub(crate) radar: RadarState,
    pub(crate) tick: u64,
    /// Monotonic visual frame counter. Unlike `tick`, this advances on every
    /// live Fast fire and is used only by pure rendering animations.
    animation_frame: u64,
    /// Number of live Fast frames accumulated toward the next 1 Hz domain tick.
    fast_frames_since_domain_tick: u8,
    /// The one-shot `SetTimeout` chain — see [`TimerChain`]. Every arm goes
    /// through `arm_timer_if_needed` (`begin_permission_flow` and `project`
    /// both arm through it); every fire retires through `timer`.
    timer_chain: TimerChain,
    pub(crate) last_render_height: usize,
    agents_offset: usize,
    pub(crate) config: config::Config,
    pub(crate) permission: PermissionState,
    pub(crate) theme: theme::DerivedColors,
    last_rendered: RenderedRail,
    notify_prev: BTreeMap<u32, crate::status::Status>,
    /// Cross-session peer state + the Alt+[/] cycle selection machine.
    sessions: Sessions,
    /// This session's own name, learned from `session_name_changed`
    /// (`Event::ModeUpdate`'s `ModeInfo.session_name`). Empty until Zellij's
    /// first `ModeUpdate` lands — `project` withholds `Effect::PersistPresence`
    /// while empty, since an unnamed presence file is useless to peers.
    own_session_name: String,
    /// The last own-`Presence` actually published, canonicalized with
    /// `updated_epoch_s` zeroed before compare-and-cache — so `project` can
    /// content-compare and emit `Effect::PersistPresence` only on a real
    /// *content* edge, the same "write on edges only" rule `PersistSnapshot`
    /// follows. Comparing the raw JSON (epoch included) would fire on every
    /// Fast tick even with unchanged counts, since `last_now_epoch_s` moves
    /// every second — mirrors `sessions.rs::set_own`, whose badge-derived
    /// change check already excludes `updated_epoch_s` the same way. The
    /// real epoch still reaches peers: it's stamped fresh into whatever
    /// `presence_json()` returns when the host actually handles the effect,
    /// so it reads as "epoch of the last content edge", not "epoch of the
    /// last tick".
    last_presence: Option<Presence>,
    /// The most recent `now_epoch_s` any entry point has captured. Reused
    /// (never re-read from the clock) by call paths that have no epoch of
    /// their own to work with — `presence_json`, `session_name_changed`,
    /// `presences_changed` — so a single event's "now" never forks in two
    /// directions.
    last_now_epoch_s: u64,
}

impl PluginRuntime {
    pub(crate) fn load(
        &mut self,
        config: config::Config,
        snapshot: Option<&str>,
        permission: PermissionProbe,
    ) -> Outcome {
        self.config = config;
        // Load's single clock capture, stored eagerly (and reused by
        // `begin_permission_flow`'s arm below, keeping one "now" per event):
        // `session_name_changed` — often the very next event in — has no
        // epoch of its own and replays `last_now_epoch_s`, so without this
        // seed the first presence write would go out stamped
        // `updated_epoch_s: 0`.
        self.last_now_epoch_s = crate::clock::now_epoch_s();
        if let Some(raw) = snapshot {
            if let Some(tick) = self.radar.load_snapshot(raw) {
                self.tick = tick;
            }
        }
        // Seed the notification baseline from the restored snapshot so that
        // pre-existing completions never fire a spurious Notify effect.
        self.notify_prev = crate::notify_rules::status_map(&self.radar.notify_views());
        self.begin_permission_flow(permission)
    }

    pub(crate) fn build_rows(&self) -> Vec<TabRow> {
        self.radar.rows(self.tick)
    }

    pub(crate) fn tabs_changed(&mut self, tabs: Vec<RadarTab>) -> Outcome {
        let change = self.radar.tabs_changed(tabs);
        self.project(vec![], change, crate::clock::now_epoch_s())
    }

    pub(crate) fn panes_changed(&mut self, update: PaneUpdate) -> Outcome {
        let now = crate::clock::now_epoch_s();
        if let Some(theme) = update.theme.clone() {
            self.theme = theme;
        }
        let change = self.radar.panes_changed(update, self.tick, now, self.config.naming);
        self.project(vec![], change, now)
    }

    /// Zellij reports the callback's actual elapsed time, which can be much
    /// larger than the requested timeout under load. Timer identity therefore
    /// comes from the single scheduled cadence stored in `TimerChain`, never
    /// from `elapsed_s`.
    pub(crate) fn timer(&mut self, permission: PermissionProbe, _elapsed_s: f64) -> Outcome {
        let Some(cadence) = self.timer_chain.on_fire() else {
            return Outcome::none();
        };
        // One clock capture per event: every consumer below (store timer,
        // cadence decision, re-arm via project) sees the same "now".
        let now = crate::clock::now_epoch_s();
        let is_fast_fire = cadence == Cadence::Fast;
        if is_fast_fire {
            self.animation_frame = self.animation_frame.wrapping_add(1);
            self.fast_frames_since_domain_tick += 1;
            if self.fast_frames_since_domain_tick < FAST_FRAMES_PER_DOMAIN_TICK {
                // Animation-only frame: repaint moving work, but deliberately
                // skip domain state and every host-I/O effect. `project`
                // re-arms the same single timer chain.
                let change = RadarChange {
                    render: self.radar.has_running_work(),
                    settle: false,
                    ..RadarChange::default()
                };
                return self.project(vec![], change, now);
            }
        }
        // Idle fires are domain ticks themselves. Every domain edge
        // resets the visual divider so a later Fast phase starts cleanly.
        self.fast_frames_since_domain_tick = 0;

        let mut effects = Vec::new();
        let permission_changed =
            self.check_deferred_permission_request(permission, &mut effects);
        // Our own request is in-flight (including one this very tick just
        // fired): keep the shared lock fresh so no waiting peer reclaims it
        // out from under a live prompt.
        if self.permission.selectable() {
            effects.push(Effect::HeartbeatPermissionLock);
        }
        self.tick += 1;
        // A domain tick can mutate the command store (debounced promotion to
        // Running, Running→Done confirm). Persist the snapshot when it does,
        // or a tab opened in that window would seed a rail missing the change.
        let store_changed = self.radar.timer(self.tick, now);
        // Cross-session peers stay on a 1 Hz domain clock whether the rail is
        // visually Fast or idle. Before the session name arrives there is no
        // presence identity to maintain or compare.
        if !self.own_session_name.is_empty() {
            effects.push(Effect::ReadPresences);
        }
        // Keep this session's presence mtime fresh once per minute regardless
        // of whether the preceding minute was visually active or idle.
        if !self.own_session_name.is_empty() && self.tick.is_multiple_of(PRESENCE_HEARTBEAT_TICKS) {
            effects.push(Effect::PersistPresence);
        }
        // Commit cross-session selection on the original 1 Hz domain clock.
        let session_commit = self.sessions.tick();
        if let Some(CommitTarget { name, attention_tab_position }) = session_commit {
            effects.push(Effect::SwitchSession { name, tab_position: attention_tab_position });
        }
        // An in-flight permission request repaints until the user answers.
        let awaiting_permission = self.sidebar_should_be_selectable();
        let minute_boundary = self.tick.is_multiple_of(PRESENCE_HEARTBEAT_TICKS);
        let render = permission_changed
            || awaiting_permission
            || store_changed
            || self.timer_should_continue()
            || (minute_boundary
                && (self.radar.ledger_any_unsaturated(now)
                    || self.radar.pending_wait_unsaturated(now)));
        let change = RadarChange {
            render,
            settle: true,
            persist_snapshot: store_changed,
            renames: vec![],
            cwd_bootstrap: vec![],
        };
        self.project(effects, change, now)
    }

    pub fn scroll_agents_up(&mut self, lines: usize) -> Outcome {
        if !self.config.agents_only || lines == 0 {
            return Outcome::none();
        }
        self.agents_offset = self.agents_offset.saturating_sub(lines);
        Outcome::render()
    }

    pub fn scroll_agents_down(&mut self, lines: usize) -> Outcome {
        if !self.config.agents_only || lines == 0 {
            return Outcome::none();
        }
        self.agents_offset = self.agents_offset.saturating_add(lines);
        Outcome::render()
    }

    pub(crate) fn mouse_click(&self, line: isize) -> Outcome {
        if !self.permission.granted() {
            return Outcome::none();
        }
        let Some(target) = self.last_rendered.target_at_line(line) else {
            return Outcome::none();
        };
        // A cross-session badge line (`session: Some(name)`) always wins the
        // match — those targets never carry a `pane_id`, but checking
        // `session` first (rather than falling into the `pane_id`/`tab_position`
        // arms below) keeps the three cases visibly mutually exclusive rather
        // than relying on that absence. `tab_position` is read through
        // `session_tab_position` (not the raw field), which undoes the
        // `RailTarget::for_session` sentinel encoding back into the
        // `Option<usize>` the effect needs — see both docs.
        let navigation = if let Some(name) = target.session.clone() {
            Effect::SwitchSession { name, tab_position: target.session_tab_position() }
        } else if let Some(pane_id) = target.pane_id {
            Effect::ShowPane { pane_id }
        } else {
            Effect::SwitchTab { position: target.tab_position }
        };
        let mut effects = vec![navigation];
        if target.session.is_none() {
            effects.extend(self.acknowledge_pending_targets(&target));
        }
        Outcome::with_effects(false, effects)
    }

    /// Acknowledgement half of `mouse_click`: every pane a click on
    /// `target` should check for a dismissible `Pending` — one pane for a
    /// pane row (`target.pane_id: Some(p)`), every pane in the tab for a tab
    /// row (`target.pane_id: None` — a tab's header line, or a plain tab with
    /// no per-pane detail line at all) — turned into one `Effect::BroadcastStatus`
    /// per pane whose STATUS-PIPE observation (`RadarState::status_observation`;
    /// never command-origin, which is untouched by design) currently reads
    /// `Pending`. A target with nothing Pending on it — including every
    /// other status, and a tab with no tracked panes at all — produces no
    /// effects: `Outcome::with_effects(false, vec![])` equals `Outcome::none()`,
    /// so this is a strict no-op, not a render with nothing to show for it.
    /// Deliberately returns `render: false` from the caller regardless: this
    /// gesture never mutates `radar` — see `Effect::BroadcastStatus`'s doc for
    /// why the convergence has to ride the pipe instead.
    fn acknowledge_pending_targets(&self, target: &render::RailTarget) -> Vec<Effect> {
        let pane_ids = match target.pane_id {
            Some(pane_id) => vec![pane_id],
            None => self.radar.pane_ids_for_tab(target.tab_position),
        };
        pane_ids
            .into_iter()
            .filter_map(|pane_id| self.acknowledge_pending_payload(pane_id))
            .map(|payload| Effect::BroadcastStatus { payload })
            .collect()
    }

    /// The synthetic `zj_radar.status.v1` payload that acknowledges `pane_id`
    /// — `None` unless the status store currently reads it as `Pending`
    /// (anything else, including a command-origin observation the status
    /// store never held in the first place, is left alone). Carries the
    /// observation's own repo/branch/msg forward — only `status` moves,
    /// Pending → Done — and re-derives `source` from its already-classified
    /// `kind` via `Kind::as_source`, the exact inverse of the intake
    /// classification (`Kind::from_source`), so the pane's `Kind` survives
    /// the round trip unchanged. `task` is left absent (empty): `StatusStore::apply`
    /// treats an empty `task` on a non-Idle status as "leave the sticky label
    /// alone", so the turn's task stays put rather than this dismiss quietly
    /// erasing it. Rides with `ack: true`: this Done means "the user has seen
    /// it", so the notifier must stay silent in EVERY instance the echo
    /// reaches — a gesture that means "stop flagging this" must not flag it
    /// one more time (`notify_rules::diff` skips acknowledged observations).
    fn acknowledge_pending_payload(&self, pane_id: u32) -> Option<String> {
        let obs = self.radar.status_observation(pane_id)?;
        if obs.status != Status::Pending {
            return None;
        }
        Some(crate::payload::to_wire(&crate::payload::StatusPayload {
            pane_id,
            status: Status::Done,
            repo: obs.repo.clone(),
            branch: obs.branch.clone(),
            msg: obs.msg.clone(),
            task: String::new(),
            source: obs.kind.as_source().to_string(),
            ack: true,
            gone: false,
        }))
    }

    /// Run an imperative command verb. `AttentionNext/Prev` resolve a
    /// deterministic target tab and emit `SwitchTab`; `SessionNext/Prev`
    /// advance the cross-session cycle selection (`Sessions::cycle`) and
    /// render the highlight move — the actual switch is a *later* idle-commit
    /// (see `timer`), not an immediate effect here. Inert until permission is
    /// granted, mirroring `mouse_click`: session switching is
    /// `ChangeApplicationState` territory, same as `SwitchTab`.
    pub(crate) fn control(&mut self, verb: Verb) -> Outcome {
        if !self.permission.granted() {
            return Outcome::none();
        }
        match verb {
            Verb::AttentionNext | Verb::AttentionPrev => {
                let dir = if verb == Verb::AttentionNext { Direction::Next } else { Direction::Prev };
                match self.radar.next_attention_tab(dir) {
                    Some(position) => Outcome::with_effects(false, vec![Effect::SwitchTab { position }]),
                    None => Outcome::none(),
                }
            }
            Verb::SessionNext | Verb::SessionPrev => {
                let dir = if verb == Verb::SessionNext { Direction::Next } else { Direction::Prev };
                let render = self.sessions.cycle(dir);
                // A fresh tap must arm Fast immediately (not wait for the next
                // domain change to pass through `project`), so the idle-commit
                // in `timer` fires promptly rather than waiting on a fully
                // disarmed chain.
                let mut effects = Vec::new();
                self.arm_timer_if_needed(self.last_now_epoch_s, &mut effects);
                Outcome::with_effects(render, effects)
            }
        }
    }

    /// Parse a `cmd.v1` payload and dispatch it. Unknown verbs are a no-op.
    pub(crate) fn control_pipe(&mut self, payload: &str) -> Outcome {
        match crate::control::parse(payload) {
            Some(verb) => self.control(verb),
            None => Outcome::none(),
        }
    }

    /// Learn (or relearn) this session's own name — the push-style source
    /// that replaced `SessionUpdate` (task-8b-brief.md): `Event::ModeUpdate`'s
    /// `ModeInfo.session_name`. `None` is a true no-op: Zellij can in
    /// principle fire `ModeUpdate` before the session has a name, and there
    /// is nothing to do with that yet — re-projecting on every such event
    /// would waste a `project()` pass for nothing new to report. A `Some`
    /// always re-projects, even a repeat of the already-known name:
    /// `ModeUpdate` fires on far more than name changes (any mode/keybind/
    /// pane-group change too), and letting `project`'s own idempotent
    /// content-compares (`PersistPresence`'s cache, `Sessions::set_own`'s
    /// badge diff) absorb the repeats is simpler, and no less correct, than
    /// hand-rolling an equality check here.
    pub(crate) fn session_name_changed(&mut self, name: Option<String>) -> Outcome {
        let Some(name) = name else { return Outcome::none() };
        self.own_session_name = name;
        self.project(vec![], RadarChange::default(), self.last_now_epoch_s)
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn own_session_name(&self) -> &str {
        &self.own_session_name
    }

    /// A fresh read of every peer session's presence file, each paired with
    /// its file's mtime age in seconds (`Effect::ReadPresences`'s
    /// read-back — `session_files::read_peer_presences`'s `age_secs`).
    /// Renders only when the derived badge actually changed.
    pub(crate) fn presences_changed(&mut self, raw: Vec<(String, u64)>) -> Outcome {
        let render = self.sessions.update_presences(raw);
        let change = RadarChange { render, settle: false, persist_snapshot: false, renames: vec![], cwd_bootstrap: vec![] };
        self.project(vec![], change, self.last_now_epoch_s)
    }

    /// This session's own `Presence`, derived from the same rows the rail
    /// renders (`radar.rows`) — never a separately-tracked count that could
    /// drift from what's on screen. `attention_tab_position` is the first
    /// (lowest-position) row that `needs_you()`, matching the position
    /// `Sessions`/`BadgeEntry` expect for a same-repo jump-on-arrival.
    fn own_presence(&self) -> Presence {
        let rows = self.radar.rows(self.tick);
        let running = rows.iter().filter(|r| r.display.status == Status::Running).count();
        let mut attention = 0usize;
        let mut attention_tab_position = None;
        for r in &rows {
            if r.display.status.needs_you() {
                attention += 1;
                if attention_tab_position.is_none() {
                    attention_tab_position = Some(r.number as usize - 1);
                }
            }
        }
        Presence {
            session_name: self.own_session_name.clone(),
            running,
            attention,
            attention_tab_position,
            updated_epoch_s: self.last_now_epoch_s,
        }
    }

    /// JSON the host actually writes to disk on `Effect::PersistPresence` —
    /// see `own_presence` for the field derivation. Only the wasm glue
    /// (Task 6) calls this in production; on a host build nothing here
    /// invokes it (that wiring is still `project`'s `own_presence`
    /// comparison), so it would otherwise read as dead code.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn presence_json(&self) -> String {
        self.own_presence().to_json()
    }

    /// True while [`timer`](Self::timer) still consumes a fresh
    /// [`PermissionProbe`] — i.e. the permission machine is waiting on a peer's
    /// marker/lock. Once a request is in-flight or the state is resolved,
    /// `on_timer` ignores the probe entirely, so the host glue can skip the
    /// per-tick marker read + lock attempt (N tabs were stat-reading N files
    /// per second forever) and pass `PermissionProbe::default()` instead.
    pub(crate) fn wants_permission_probe(&self) -> bool {
        self.permission.is_waiting()
    }

    pub(crate) fn permission_result(&mut self, granted: bool) -> Outcome {
        self.record_permission_result(granted);
        let mut effects = vec![
            Effect::PersistPermissionMarker(if granted {
                PermissionMarker::Granted
            } else {
                PermissionMarker::Denied
            }),
            Effect::SetSelectable(self.sidebar_should_be_selectable()),
        ];
        // The onboarding pane exists only to host the grant prompt. Once granted
        // — and the grant is cached by plugin URL, so the rail inherits it — it
        // removes itself, leaving the user with just the rail.
        if granted && self.config.role == config::Role::Onboarding {
            effects.push(Effect::CloseSelf);
        }
        Outcome::with_effects(true, effects)
    }

    pub(crate) fn cwd_changed(&mut self, pane_id: u32, path: String) -> Outcome {
        let change = self.radar.cwd_changed(pane_id, path, self.config.naming);
        self.project(vec![], change, crate::clock::now_epoch_s())
    }

    pub(crate) fn command_changed(
        &mut self,
        pane_id: u32,
        command: &[String],
        is_foreground: bool,
    ) -> Outcome {
        let change = self.radar.command_changed(pane_id, command, is_foreground, self.tick);
        self.project(vec![], change, crate::clock::now_epoch_s())
    }

    pub(crate) fn status_pipe(&mut self, raw: &str) -> Outcome {
        let now = crate::clock::now_epoch_s();
        let Some(change) = self.radar.status_pipe(raw, self.tick, now, self.config.naming) else {
            return Outcome::none();
        };
        self.project(vec![], change, now)
    }

    pub(crate) fn snapshot_json(&self, existing: Option<&str>) -> String {
        self.radar.snapshot_json(existing, self.tick)
    }

    pub(crate) fn config_pipe(&mut self, raw: &str) -> Outcome {
        let Some(kv) = crate::config::overrides_from_json(raw) else {
            return Outcome::none();
        };
        self.config.apply_overrides(&kv);
        let renames = self.radar.recompute_renames(self.config.naming);
        let change = RadarChange {
            render: true,
            renames,
            settle: false,
            persist_snapshot: false,
            cwd_bootstrap: vec![],
        };
        self.project(vec![], change, crate::clock::now_epoch_s())
    }

    pub(crate) fn render(&mut self, rows: usize, cols: usize) -> String {
        self.last_render_height = rows;
        let tabrows = self.build_rows();
        let tabrows = visible_rows(tabrows, self.config.agents_only);
        if !self.config.agents_only {
            self.agents_offset = 0;
        } else {
            // Cards are single-line, growing a second line only while an
            // agent needs attention with a real message — sum the actual
            // per-pane heights.
            let flat_lines: usize = tabrows
                .iter()
                .flat_map(|row| &row.display.panes)
                .filter(|pane| pane.is_agent())
                .map(render::flat_card_lines)
                .sum();
            self.agents_offset = self
                .agents_offset
                .min((flat_lines + self.config.agents_pad_top).saturating_sub(rows));
        }
        let opts = render::RenderOpts {
            width: cols.max(1),
            height: rows,
            now_tick: self.animation_frame,
            glyphs: self.config.glyphs,
            header: self.config.header,
            density: self.config.density,
            theme: self.theme.clone(),
            now_epoch_s: crate::clock::now_epoch_s(),
            jump_hint: self.config.jump_hint.shows(),
            badge: self.sessions.badge(),
            agents_only: self.config.agents_only,
            agents_offset: self.agents_offset,
            agents_pad_top: self.config.agents_pad_top,
        };
        let ledger = self.radar.ledger_lines();
        let rail = if !self.permission.granted() {
            render::needs_permission(&opts, self.config.grant_hint)
        } else if !self.config.agents_only && tabrows.is_empty() && self.radar.ledger_is_empty() {
            render::onboarding(&opts)
        } else {
            render::render_rail(&tabrows, &ledger, &opts)
        };
        let ansi = rail.ansi.clone();
        self.last_rendered = rail;
        ansi
    }

    /// Test-only: this session's natural content height at `cols` — enough to
    /// show every row with no overflow folding and no bottom-region padding
    /// (spec §9). Click-mapping tests want a "big enough, no overflow" height
    /// without hard-coding one; passing a merely-large sentinel (the old
    /// `usize::MAX / 2` convention) now lands in the bottom region's
    /// unbounded-filler branch, so this asks `render::body_line_count` for the
    /// real number instead.
    #[cfg(test)]
    pub(crate) fn natural_height(&mut self, cols: usize) -> usize {
        let tabrows = self.build_rows();
        let tabrows = visible_rows(tabrows, self.config.agents_only);
        let opts = render::RenderOpts {
            width: cols.max(1),
            height: usize::MAX / 2,
            now_tick: self.animation_frame,
            glyphs: self.config.glyphs,
            header: self.config.header,
            density: self.config.density,
            theme: self.theme.clone(),
            now_epoch_s: crate::clock::now_epoch_s(),
            jump_hint: self.config.jump_hint.shows(),
            badge: self.sessions.badge(),
            agents_only: self.config.agents_only,
            agents_offset: 0,
            agents_pad_top: self.config.agents_pad_top,
        };
        render::body_line_count(&tabrows, &self.radar.ledger_lines(), &opts)
    }

    pub(crate) fn sidebar_should_be_selectable(&self) -> bool {
        self.permission.selectable()
    }

    /// Test-only: force the in-flight `Requesting` state without driving the
    /// full probe flow. Production reaches `Requesting` via `on_load`/`on_timer`.
    #[cfg(test)]
    pub(crate) fn record_permission_request_started(&mut self) {
        self.permission = PermissionState::Requesting;
    }

    pub(crate) fn record_permission_result(&mut self, granted: bool) {
        self.permission.on_result(granted);
    }

    #[cfg(test)]
    pub(crate) fn target_at_line(&self, line: isize) -> Option<(usize, Option<u32>)> {
        let t = self.last_rendered.target_at_line(line)?;
        Some((t.tab_position, t.pane_id))
    }

    #[cfg(test)]
    pub(crate) fn tab_position_at_line(&self, line: isize) -> Option<usize> {
        self.target_at_line(line).map(|(pos, _)| pos)
    }

    /// Collapse the two config flags into the permission module's policy. The
    /// precedence (onboarding wins, then defer, else the lock dance) lives here,
    /// so `permission.rs` never imports `Config` and the dead
    /// `Onboarding && defer` combination is unrepresentable downstream.
    fn permission_policy(&self) -> PermissionPolicy {
        if self.config.role == config::Role::Onboarding {
            PermissionPolicy::OnboardingPane
        } else if self.config.defer_permission {
            PermissionPolicy::Deferring
        } else {
            PermissionPolicy::LockCoordinated
        }
    }

    fn begin_permission_flow(&mut self, probe: PermissionProbe) -> Outcome {
        let policy = self.permission_policy();
        let mut effects = Vec::new();
        // Only a fresh request needs the host's permission prompt; a marker-driven
        // resolution or a wait emit no permission effect here.
        if self.permission.on_load(&probe, policy) == Transition::Requested {
            effects.push(Effect::RequestPermission);
        }
        // Arm a timer whenever a decision is still outstanding — either we're
        // waiting on a peer's marker, or our own request is in-flight. Pre-grant
        // Zellij withholds the state events that would otherwise trigger a paint
        // (they need ReadApplicationState), so this timer is the only thing that
        // gets the needs_permission screen onto the rail. `last_now_epoch_s`
        // is the capture `load` (this fn's sole caller) took at entry — not a
        // second clock read, so the whole load event sees a single "now".
        self.arm_timer_if_needed(self.last_now_epoch_s, &mut effects);
        // Load always initializes the sidebar's selectability, every arm.
        effects.push(Effect::SetSelectable(self.permission.selectable()));
        Outcome::with_effects(false, effects)
    }

    fn check_deferred_permission_request(
        &mut self,
        probe: PermissionProbe,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let policy = self.permission_policy();
        // `on_timer` is inert unless we're waiting on a peer; a decision landing
        // (marker arrived, or we reclaimed a stale lock — see session_files)
        // refreshes selectability. `Requested` additionally fires the prompt.
        match self.permission.on_timer(&probe, policy) {
            Transition::Requested => effects.push(Effect::RequestPermission),
            Transition::Resolved { .. } => {}
            Transition::NoChange | Transition::StillWaiting => return false,
        }
        effects.push(Effect::SetSelectable(self.permission.selectable()));
        true
    }

    /// Cadence policy: Fast while visual or tick-windowed work is live; Idle
    /// once a named session only needs 1 Hz peer liveness; None when even that
    /// work is absent. Denied
    /// rails always disarm because the events that clear restored domain work
    /// cannot arrive without `ReadApplicationState`.
    /// `now_epoch_s` is the event's single clock capture (the stores already
    /// take epochs as arguments; this extends that discipline up through the
    /// runtime so one event never sees two different "now"s).
    fn desired_cadence(&self, _now_epoch_s: u64) -> Option<Cadence> {
        // The early return outranks the name check below by construction: a
        // denied rail must fully disarm even though its name is known.
        if self.permission.denied() {
            return None;
        }
        if self.permission.is_waiting()
            || self.permission.selectable()
            || self.timer_should_continue()
            // A pending cross-session cycle selection needs the idle-commit
            // in `timer` to fire promptly instead of waiting on a fully
            // disarmed chain.
            || self.sessions.wants_fast_cadence()
        {
            Some(Cadence::Fast)
        } else if !self.own_session_name.is_empty() {
            // A named session has published a presence file. Keep peer reads
            // on the 1 Hz domain clock and heartbeat our own mtime every 60
            // domain ticks without paying for Fast's visual frames.
            Some(Cadence::Idle)
        } else {
            None
        }
    }

    /// Arm (or re-arm) the one-shot timer at `desired_cadence()` — a thin
    /// bridge from [`TimerChain::arm`]'s decision to the effect vec, so the
    /// "arm returned ⇒ SetTimeout emitted" pairing has exactly one home.
    fn arm_timer_if_needed(&mut self, now_epoch_s: u64, effects: &mut Vec<Effect>) {
        if let Some(cadence) = self.timer_chain.arm(self.desired_cadence(now_epoch_s)) {
            effects.push(Effect::SetTimeout(cadence));
        }
    }

    /// Whether the one-shot timer needs the Fast cadence for domain work.
    /// A named rail without these triggers still uses the 1 Hz Idle cadence
    /// for peer liveness. Four triggers require Fast:
    ///
    /// - **animating work** — a `Running` agent/command whose glyph spins each
    ///   tick (`RadarState::has_running_work`);
    /// - **an un-carried completion edge** — a `status_pipe` payload defers its
    ///   recede + notification to the timer (it can't trust its own focus, see
    ///   `RadarState::status_pipe`), so we must keep ticking until the settle has
    ///   run. [`has_unsettled_notifications`] goes false the moment that settle
    ///   advances the baseline, so a *backgrounded* `Done`/`Error`/`Pending` stops
    ///   pinning the timer awake once notified — a later focus change or broadcast
    ///   re-arms it. (The pre-settle baseline read costs at most one extra tick.)
    /// - **a command `Done` awaiting its TTL recede** (`RadarState::command_awaiting_recede`)
    ///   — the row itself is static (it doesn't animate and its notify already
    ///   fired), but the ledger handoff at `DONE_TTL_TICKS` still needs a tick to
    ///   land on schedule, so it keeps this armed even though `has_running_work`
    ///   stays narrow (see that method's doc for the arming split).
    /// - **an active ping flash** (`RadarState::has_active_flash`) — the
    ///   flip-to-pending glance-catcher is a two-tick visual, not a card fact,
    ///   so nothing else here would otherwise keep the timer awake for it.
    ///
    /// [`has_unsettled_notifications`]: Self::has_unsettled_notifications
    fn timer_should_continue(&self) -> bool {
        self.radar.has_running_work()
            || self.has_unsettled_notifications()
            || self.radar.command_awaiting_recede()
            || self.radar.has_active_flash(self.tick)
    }

    /// True while the notification baseline lags the live per-pane statuses — i.e.
    /// an attention edge has landed that a settle tick hasn't carried yet. Reads
    /// the exact baseline [`notify_effects`](Self::notify_effects) advances, so it
    /// goes quiet precisely when the deferred recede + notify are done.
    fn has_unsettled_notifications(&self) -> bool {
        crate::notify_rules::status_map(&self.radar.notify_views()) != self.notify_prev
    }

    /// Diff observable pane statuses against `notify_prev` and emit `Effect::Notify`
    /// for each attention-status transition.
    ///
    /// Intentionally runs regardless of `permission_granted`. Without the
    /// `RunCommands` grant, `run_command` is a silent host no-op, so notifications
    /// are harmlessly dropped. More importantly, gating this on `permission_granted`
    /// would skip advancing `notify_prev` during the ungranted window, which risks a
    /// burst of stale notifications the moment the grant arrives. The ungranted window
    /// is startup-only and brief, so the no-op cost is negligible.
    fn notify_effects(&mut self) -> Vec<Effect> {
        let views = self.radar.notify_views();
        let focused = self.radar.last_focused();
        let notes = crate::notify_rules::diff(&self.notify_prev, &views, focused, &self.config);
        self.notify_prev = crate::notify_rules::status_map(&views);
        notes
            .into_iter()
            .map(|n| Effect::Notify { key: crate::notify_rules::claim_key(&n), title: n.title, body: n.body })
            .collect()
    }

    fn effects_from_renames(&self, renames: Vec<TabRename>) -> Vec<Effect> {
        renames
            .into_iter()
            .map(|TabRename { position, name }| Effect::RenameTab { position, name })
            .collect()
    }

    /// The sole projection from a domain [`RadarChange`] to host [`Effect`]s.
    /// `fx` is a caller-supplied seed so the `timer` handler's permission
    /// effects come first, without a post-hoc splice. Canonical order: renames
    /// → snapshot → presence → cwd → `SetTimeout` → notify — identical to
    /// today's `panes_changed` apart from the presence edge, so that handler
    /// is otherwise byte-for-byte unchanged. `settle` is the per-handler stamp
    /// described in `## Settle` (`CONTEXT.md`): this is the sole caller of
    /// `notify_effects`, and the only *domain-change* path that arms the timer
    /// (the permission flow arms it separately in `begin_permission_flow`).
    /// `TimerChain::arm` self-guards on the armed cadence and on whether
    /// there's anything to arm for, so calling it unconditionally here is a
    /// no-op wherever a handler has no pending work to arm for.
    ///
    /// Also the single point deciding `Effect::PersistPresence`: every
    /// domain-change entry point funnels through here with its `now_epoch_s`,
    /// which this stores (`last_now_epoch_s`) for the call paths that have no
    /// epoch of their own — `presence_json`, `session_name_changed`,
    /// `presences_changed`. The freshly computed own-`Presence` is
    /// content-compared against the last one published, EXCLUDING
    /// `updated_epoch_s` (zeroed on both sides before the compare/cache) —
    /// see `last_presence`'s doc for why a raw compare would defeat the edge
    /// gate on Fast cadence. A real content edge pushes the effect and
    /// updates the cache, mirroring `PersistSnapshot`'s "write on edges only"
    /// rule. Withheld while `own_session_name` is empty — a presence file
    /// with no name is useless to peers — so it stays quiet until
    /// `session_name_changed` (also routed through here) learns it.
    ///
    /// Same gate also feeds `Sessions::set_own` — the single path for own
    /// counts into the OWN badge row (task-8b-brief.md un-deads it: nothing
    /// called it before this). Unlike `PersistPresence`, this is NOT
    /// edge-gated by `last_presence`'s cache — `Sessions::set_own` already
    /// does its own badge-derived content-compare and reports whether the
    /// badge actually changed, so calling it every `project` pass (once the
    /// name is known) is correct AND is what closes the gap where the own
    /// row never updated as running/attention moved.
    ///
    /// Also the single point that de-dupes `Effect::PersistPresence` when the
    /// seeded `fx` (e.g. `timer`'s minute heartbeat) and this pass's own
    /// edge-gated push both land — see the `retain` near the bottom.
    fn project(&mut self, mut fx: Vec<Effect>, c: RadarChange, now_epoch_s: u64) -> Outcome {
        self.last_now_epoch_s = now_epoch_s;
        fx.extend(self.effects_from_renames(c.renames));
        if c.persist_snapshot {
            fx.push(Effect::PersistSnapshot);
        }
        let mut render = c.render;
        if !self.own_session_name.is_empty() {
            let fresh = self.own_presence();
            render |= self.sessions.set_own(fresh.clone());
            let mut compare = fresh;
            compare.updated_epoch_s = 0;
            if self.last_presence.as_ref() != Some(&compare) {
                self.last_presence = Some(compare);
                fx.push(Effect::PersistPresence);
            }
        }
        if !c.cwd_bootstrap.is_empty() {
            fx.push(Effect::ResolveCwd { pane_ids: c.cwd_bootstrap });
        }
        self.arm_timer_if_needed(now_epoch_s, &mut fx);
        if c.settle {
            fx.extend(self.notify_effects());
        }
        // `fx` can carry TWO `PersistPresence`s by the time we get here: the
        // minute heartbeat `timer` seeds unconditionally and the edge-gated
        // push above can both land on the same domain tick (for example, a
        // debounce promotion crossing the heartbeat boundary).
        // `project` is the single assembly point for every entry path, so
        // it's the one place that can see both pushes at once and collapse
        // them; keep the earliest (whichever reason got there first) rather
        // than narrowing either push's own semantics.
        let mut persist_presence_seen = false;
        fx.retain(|effect| {
            if matches!(effect, Effect::PersistPresence) {
                let first = !persist_presence_seen;
                persist_presence_seen = true;
                first
            } else {
                true
            }
        });
        Outcome::with_effects(render, fx)
    }
}

fn visible_rows(
    mut tabrows: Vec<crate::rollup::TabRow>,
    agents_only: bool,
) -> Vec<crate::rollup::TabRow> {
    if agents_only {
        tabrows.retain(|row| {
            row.display
                .panes
                .iter()
                .any(crate::rollup::PaneDisplay::is_agent)
        });
    }
    tabrows
}

#[cfg(test)]
mod tests;
