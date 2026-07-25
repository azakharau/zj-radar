//! Pure aggregation logic for the headless agent-status plugin.
//!
//! Agent hooks broadcast one `zj_radar.status.v1` payload per pane. This crate
//! folds those per-pane observations into the single short string that shows up
//! in the zjstatus bar as `{pipe_agents}` — an icon and a status glyph per agent
//! vendor, plus a count when a vendor has more than one pane busy.
//!
//! Nothing here touches `zellij-tile`, so it builds and tests on the host. The
//! wasm glue lives in `main.rs`.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use zj_radar_core::status::{GlyphSet, Role};
use zj_radar_core::payload::{sanitize, MAX_TAB_NAME_CHARS};
use zj_radar_core::{parse, to_wire, Kind, Status, StatusPayload};

/// The zjstatus widget key. It includes the `pipe_` prefix on purpose: zjstatus
/// derives its map key by stripping only the trailing `_format` / `_rendermode`
/// from the config key (its `PIPE_REGEX` is `_[a-zA-Z0-9]+$`), so
/// `pipe_agents_tabs_format` → `pipe_agents_tabs`, and the wire payload must
/// match.
pub const DEFAULT_PIPE_NAME: &str = "pipe_agents_tabs";

/// Ticks (≈ seconds) before an agent that has stopped reporting is dropped. A
/// producer that dies without sending `gone` — killed terminal, crashed hook —
/// would otherwise pin its glyph in the bar forever.
pub const DEFAULT_TTL_TICKS: u64 = 900;

/// `Done` is a transient "it just finished" marker, not a state worth holding
/// for the full TTL: without this it would sit in the bar until the pane is
/// reused. Short enough to read, long enough to notice.
pub const DEFAULT_DONE_TTL_TICKS: u64 = 30;

/// Frame interval for the working spinner, in milliseconds.
///
/// 100ms x 12 frames is a 1.2s cycle — close to the tempo of `cli-spinners`'
/// braille spinners (800ms), which ora/Ink and the agent harnesses animate, so the
/// bar reads as the same kind of motion as the harness's own.
///
/// It is not free: every frame is an IPC broadcast plus a full zjstatus repaint,
/// because zjstatus exempts pipe widgets from its widget cache. Raise
/// `animate_ms` to spend less, or set `animate false` for a static glyph.
pub const DEFAULT_ANIMATE_MS: u64 = 100;

/// Clamp: below this the bar is pure overhead, above it the animation stutters.
const MIN_ANIMATE_MS: u64 = 50;
const MAX_ANIMATE_MS: u64 = 1000;

/// Runtime configuration, read from the `load_plugins` node in `config.kdl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pipe_name: String,
    glyphs: GlyphSet,
    animate: bool,
    animate_ms: u64,
    ttl_ticks: u64,
    done_ttl_ticks: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            glyphs: GlyphSet::default(),
            animate: true,
            animate_ms: DEFAULT_ANIMATE_MS,
            ttl_ticks: DEFAULT_TTL_TICKS,
            done_ttl_ticks: DEFAULT_DONE_TTL_TICKS,
        }
    }
}

impl Config {
    /// Parse the plugin's KDL config block. Every key is optional; an
    /// unrecognized or unparseable value keeps the default rather than
    /// disabling the widget, so a typo degrades to "stock behaviour" instead of
    /// a silently blank bar.
    pub fn from_zellij_config(configuration: &BTreeMap<String, String>) -> Self {
        let defaults = Config::default();
        Config {
            pipe_name: non_empty(configuration, "pipe_name").unwrap_or(defaults.pipe_name),
            glyphs: configuration
                .get("glyphs")
                .and_then(|s| GlyphSet::from_config(s))
                .unwrap_or(defaults.glyphs),
            // Animating the working glyph costs ~5-11% server CPU, because each
            // frame is an IPC broadcast plus a full zjstatus repaint. Worth it
            // for the at-a-glance "it's alive" signal, but not everyone's
            // trade — `animate false` pins a static glyph and never leaves the
            // idle cadence (measured 0.0% CPU).
            animate: non_empty(configuration, "animate")
                .map(|v| !matches!(v.as_str(), "false" | "0" | "no" | "off"))
                .unwrap_or(defaults.animate),
            animate_ms: parse_ticks(configuration, "animate_ms")
                .map(|ms| ms.clamp(MIN_ANIMATE_MS, MAX_ANIMATE_MS))
                .unwrap_or(defaults.animate_ms),
            ttl_ticks: parse_ticks(configuration, "ttl_secs").unwrap_or(defaults.ttl_ticks),
            done_ttl_ticks: parse_ticks(configuration, "done_ttl_secs")
                .unwrap_or(defaults.done_ttl_ticks),
        }
    }

    pub fn glyphs(&self) -> GlyphSet {
        self.glyphs
    }

    pub fn animate(&self) -> bool {
        self.animate
    }

    /// Frame interval in seconds, for `set_timeout`.
    pub fn animate_secs(&self) -> f64 {
        self.animate_ms as f64 / 1000.0
    }

    /// The exact line zjstatus expects: it splits on `::`, requires the
    /// `zjstatus` sentinel and the `pipe` command, then stores the remainder
    /// under the widget key. Newlines are the protocol's line separator, so the
    /// caller must never put one in `output`.
    pub fn pipe_payload(&self, output: &str) -> String {
        format!("zjstatus::pipe::{}::{}", self.pipe_name, output)
    }

}

fn non_empty(configuration: &BTreeMap<String, String>, key: &str) -> Option<String> {
    configuration
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_ticks(configuration: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    non_empty(configuration, key)?.parse().ok()
}

/// One pane's most recent observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    kind: Kind,
    status: Status,
    seen: u64,
}

/// One tab as the status bar needs to draw it. Mirrors the fields of Zellij's
/// `TabInfo` that matter here, so the pure renderer never sees `zellij-tile`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabView {
    pub position: usize,
    pub name: String,
    pub active: bool,
    /// `TabInfo::has_bell_notification` — a pane in this tab rang and has not
    /// been looked at yet. Rendered here rather than by zjstatus because
    /// zjstatus 0.23 has no bell support at all, and the data comes from Zellij.
    pub bell: bool,
    /// `TabInfo::is_flashing_bell` — the transient 400ms flash right after the
    /// bell. Only visible if the bar repaints fast enough, which the animation
    /// cadence already guarantees.
    pub flashing_bell: bool,
    /// `TabInfo::is_sync_panes_active` — input is broadcast to every pane here.
    /// Dangerous to forget, so it stays visible; zjstatus rendered it as
    /// `[sync]` and dropping it when the plugin took over the strip would be a
    /// regression.
    pub sync: bool,
    /// `TabInfo::is_fullscreen_active` — a pane is zoomed, so the tab is hiding
    /// its siblings.
    pub fullscreen: bool,
}

/// Tokyo Night palette, matching the hand-written zjstatus config this replaces
/// so the strip looks native rather than bolted on.
const BG: &str = "#222436";
const TAB_ACTIVE_BG: &str = "#7AA2F7";
const TAB_NAME: &str = "#A9B1D6";
const TAB_DIM: &str = "#565F89";
/// Powerline thin separator (U+E0B1). A full-height `│` in the page background
/// colour reads as a HOLE punched through the block; this chevron is what
/// powerline-style bars use to divide segments *within* one block, and the user's
/// font already renders nerd glyphs (see `U+E795` in their `format_right`).
const DIVIDER: char = '\u{e0b1}';
const INK: &str = "#222436";

/// Live agent observations, keyed by `(terminal pane id, vendor)`.
///
/// Not by pane alone: a pane can host several agents at once — most importantly
/// when a relay funnels a whole remote session through ONE local pane — and
/// keying by pane would let each new vendor evict the last. Same vendor twice in
/// one pane still merges, which is the right granularity for a display that shows
/// one icon per vendor.
#[derive(Default, Debug)]
pub struct Agents {
    entries: HashMap<(u32, Kind), Entry>,
}

impl Agents {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Terminal pane ids with a live agent. Used by callers that need to relate
    /// agents back to panes without reaching into the entry map.
    pub fn pane_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.entries.keys().map(|(pane_id, _)| *pane_id).collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Fold one status payload in. Returns whether the aggregate could have
    /// changed — the caller uses that only to skip work; the published-string
    /// comparison is what actually suppresses redundant repaints.
    ///
    /// `gone` and `Idle` both mean "stop showing this pane": `gone` is the
    /// producer saying the agent is finished with the pane, `Idle` is the
    /// status vocabulary's own "nothing to report", and neither earns a glyph.
    pub fn apply(&mut self, payload: &StatusPayload, now: u64) -> bool {
        let kind = Kind::from_source(&payload.source);
        if payload.gone || payload.status == Status::Idle {
            return self.entries.remove(&(payload.pane_id, kind)).is_some();
        }
        let entry = Entry {
            kind,
            status: payload.status,
            seen: now,
        };
        self.entries.insert((payload.pane_id, kind), entry) != Some(entry)
    }

    /// Drop observations whose pane no longer exists. A closed terminal takes
    /// its agent with it, and no producer gets to run a hook on the way out.
    pub fn retain_panes(&mut self, live: &HashSet<u32>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(pane_id, _), _| live.contains(pane_id));
        self.entries.len() != before
    }

    /// Serialize live observations so they survive a plugin reload.
    ///
    /// Zellij reloads a plugin (and wipes its memory) without telling producers,
    /// and producers only emit on a status *change* — so an agent that has been
    /// "running" for ten minutes would leave the bar blank until it next changed
    /// state. That is the whole reason this exists.
    ///
    /// The format is one `zj_radar.status.v1` line per pane, i.e. exactly the
    /// wire contract, so `restore` is just the read path already used for live
    /// payloads and no second schema can drift from it.
    pub fn snapshot(&self) -> String {
        let mut lines: Vec<(u32, String)> = self
            .entries
            .iter()
            .map(|((pane_id, _), entry)| {
                (
                    *pane_id,
                    to_wire(&StatusPayload {
                        pane_id: *pane_id,
                        status: entry.status,
                        source: entry.kind.as_source().to_string(),
                        ..Default::default()
                    }),
                )
            })
            .collect();
        // Sorted so an unchanged set serializes byte-identically and the
        // caller can skip the write.
        lines.sort_by_key(|(pane_id, _)| *pane_id);
        lines
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rebuild from `snapshot`. Unparseable lines are skipped rather than
    /// failing the whole restore: a partially-written or older-format file must
    /// degrade to "fewer agents shown", never to a plugin that cannot start.
    ///
    /// `now` stamps every restored entry, so TTLs restart from the reload. The
    /// alternative — persisting timestamps — would expire entries against a
    /// tick counter that resets on reload, which is worse than restarting them.
    pub fn restore(&mut self, snapshot: &str, now: u64) {
        for line in snapshot.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(payload) = parse(line) {
                self.apply(&payload, now);
            }
        }
    }

    /// Drop observations that have gone quiet for longer than their TTL.
    pub fn expire(&mut self, now: u64, config: &Config) -> bool {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            let ttl = if entry.status == Status::Done {
                config.done_ttl_ticks
            } else {
                config.ttl_ticks
            };
            now.saturating_sub(entry.seen) < ttl
        });
        self.entries.len() != before
    }


    /// True while any agent is working, i.e. while the spinner must animate. The
    /// caller uses this to pick a fast frame cadence and drop back to an idle one
    /// the moment nothing is moving.
    pub fn any_running(&self) -> bool {
        self.entries.values().any(|e| e.status == Status::Running)
    }

    /// Every vendor at work in one tab, worst-status-first — not just the single
    /// worst one. A tab running Claude *and* waiting on Codex has two things to
    /// say, and collapsing them to the louder one hides the other outright.
    ///
    /// Grouping walks `Kind::ALL` rather than hashing `Kind`; the list is short and
    /// its order is the tiebreak for vendors sharing a status.
    fn tab_clusters(
        &self,
        position: usize,
        pane_tab: &HashMap<u32, usize>,
    ) -> Vec<(Kind, Status, usize)> {
        let mut clusters: Vec<(Kind, Status, usize)> = Kind::ALL
            .iter()
            .filter_map(|&kind| {
                let mut worst: Option<Status> = None;
                let mut count = 0usize;
                for ((pane_id, k), entry) in &self.entries {
                    if *k != kind || pane_tab.get(pane_id) != Some(&position) {
                        continue;
                    }
                    count += 1;
                    // `Status` is ascending-severity and derives `Ord`, so `max`
                    // IS the documented priority: error > pending > running > done.
                    worst = Some(worst.map_or(entry.status, |w: Status| w.max(entry.status)));
                }
                worst.map(|status| (kind, status, count))
            })
            .collect();
        clusters.sort_by_key(|c| std::cmp::Reverse(c.1));
        clusters
    }


    /// Render the whole tab strip, replacing zjstatus's own `{tabs}`.
    ///
    /// zjstatus's per-tab tokens are a closed set (`{index}`, `{name}`, sync /
    /// fullscreen / floating) with no way to inject agent state. So the only way
    /// to show status *per tab* — short of renaming the user's tabs, which needs
    /// `ChangeApplicationState` and overwrites names the user owns — is to draw
    /// the strip here and hand it over as one pipe value.
    ///
    /// Layout, and why each part earns its cells:
    ///
    /// ```text
    ///   1 recall      2 π⣻ build      3 notes󰂚 [sync]
    ///   │ │           │ │  │          │       │
    ///   │ │           │ │  └ name     │       └ zellij's own indicators
    ///   │ │           │ └ vendor+status, animated only while working
    ///   │ └ name      └ index is ALWAYS shown: navigation is `GoToTab <n>`,
    ///   └ index         so hiding it on the tab you are on is hostile
    /// ```
    ///
    /// The colour hierarchy is the point: a tab that *needs you* (pending/error)
    /// puts the status colour on the NAME as well, so it reads as urgent from the
    /// corner of your eye. A tab that is merely working keeps a normal name and
    /// says so only through the small animated glyph — busy is not urgent, and
    /// making it shout would train you to ignore the strip.
    pub fn render_tabs(
        &self,
        tabs: &[TabView],
        pane_tab: &HashMap<u32, usize>,
        glyphs: GlyphSet,
        frame: usize,
    ) -> String {
        let mut out = String::new();
        for tab in tabs {
            // Tab names are user-controlled and reach us verbatim from Zellij. A
            // newline would be read as a zjstatus directive separator (this
            // string is published alongside a second directive), and an escape
            // sequence could repaint the bar arbitrarily. Core's sanitizer strips
            // control chars, bidi overrides and ANSI/OSC, and caps the width.
            let name = sanitize(&tab.name, MAX_TAB_NAME_CHARS);
            let clusters = self.tab_clusters(tab.position, pane_tab);
            let peak = clusters.first().map(|c| c.1);
            let index = tab.position + 1;

            if tab.active {
                // The block itself says "you are here", so the accent colour is
                // free to carry the status instead of being spent on selection.
                let accent = peak.map_or(TAB_ACTIVE_BG, |s| role_hex(s.role()));
                // No index on the active tab: it is a jump target, and you cannot
                // jump to where you already are.
                out.push_str(&format!("#[fg={INK},bg={accent},bold] "));
                for (kind, status, count) in &clusters {
                    out.push(kind.mark(glyphs));
                    out.push(' ');
                    out.push_str(&status_glyph(*status, glyphs, frame));
                    push_count(&mut out, *count);
                    out.push(' ');
                }
                if !clusters.is_empty() {
                    out.push(DIVIDER);
                    out.push(' ');
                }
                out.push_str(&name);
                out.push_str(&indicators(tab, INK));
                out.push_str(&bell_glyph(tab));
                out.push_str(&format!(" #[fg={accent},bg={BG}] "));
            } else {
                out.push_str(&format!("#[fg={TAB_DIM},bg={BG}] {index} "));
                let name_colour = match peak {
                    // Needs you: paint the name too, so it is findable without
                    // reading glyphs.
                    Some(s) if s.needs_you() => role_hex(s.role()),
                    _ => TAB_NAME,
                };
                for (kind, status, count) in &clusters {
                    // Each vendor keeps its OWN colour, so a tab that is working
                    // and also waiting shows both facts rather than one.
                    out.push_str(&format!("#[fg={}]", role_hex(status.role())));
                    out.push(kind.mark(glyphs));
                    out.push(' ');
                    out.push_str(&status_glyph(*status, glyphs, frame));
                    push_count(&mut out, *count);
                    out.push(' ');
                }
                if !clusters.is_empty() {
                    out.push_str(&format!("#[fg={TAB_DIM}]{DIVIDER} "));
                }
                out.push_str(&format!("#[fg={name_colour}]{name}"));
                out.push_str(&indicators(tab, TAB_DIM));
                out.push_str(&bell_glyph(tab));
                out.push(' ');
            }
        }
        out
    }
}

/// Working spinner: a HOLLOW 4x4 braille square whose perimeter has one gap
/// chasing clockwise. Static form `⣏⣹`.
///
/// Two braille cells are exactly a 4x4 dot grid, so a 4x4 ring fills the cell's
/// full height and sits on the text baseline. A 3x3 ring was tried first and
/// leaves the bottom dot-row dark, which makes the glyph float high in the line —
/// the same reason core's `working_spin` reads badly here: it lights only a
/// FOUR-dot snake, so most frames look like specks near one edge. Fine in the old
/// sidebar's wide rail; dirt on the screen in a one-line bar.
///
/// Eleven of the twelve perimeter dots are lit at every frame, so it reads as a
/// solid ring with a gap travelling round it rather than as scattered dots. Two
/// cells wide, which also balances the one-cell vendor mark beside it.
const SPINNER: [&str; 12] = [
    "⣎⣹", "⣇⣹", "⣏⣸", "⣏⣱", "⣏⣩", "⣏⣙", "⣏⡹", "⣏⢹", "⡏⣹", "⢏⣹", "⣋⣹", "⣍⣹",
];

/// The status glyph, animated for `Running` only. Every other state is a settled
/// fact and a moving glyph would imply otherwise.
fn status_glyph(status: Status, glyphs: GlyphSet, frame: usize) -> String {
    if status == Status::Running {
        SPINNER[frame % SPINNER.len()].to_string()
    } else {
        status.glyph_for(glyphs).to_string()
    }
}

/// Zellij's own per-tab flags, kept because the plugin took over the strip and
/// silently dropping them would be a regression. Rendered in the caller's dim
/// colour so they never compete with agent status.
fn indicators(tab: &TabView, colour: &str) -> String {
    let mut out = String::new();
    if tab.sync {
        out.push_str(&format!("#[fg={colour}] [sync]"));
    }
    if tab.fullscreen {
        out.push_str(&format!("#[fg={colour}] [full]"));
    }
    out
}

/// A bell marker for the tab. Flashing (the transient 400ms state) is louder
/// than a settled unacknowledged bell, so it gets the error colour.
fn bell_glyph(tab: &TabView) -> String {
    if tab.flashing_bell {
        format!("#[fg={}]󰂚", role_hex(Role::Error))
    } else if tab.bell {
        format!("#[fg={}]󰂚", role_hex(Role::Attention))
    } else {
        String::new()
    }
}

fn push_count(out: &mut String, count: usize) {
    if count > 1 {
        out.push_str(&count.to_string());
    }
}

/// Tokyo Night hexes matching the palette already used across the user's
/// zjstatus config. `Role::ansi` is the 16-colour SGR form, which zjstatus's
/// format directives cannot consume — these go through `#[fg=…]` instead.
fn role_hex(role: Role) -> &'static str {
    match role {
        Role::Error => "#F7768E",
        Role::Attention => "#E0AF68",
        Role::Working => "#7AA2F7",
        Role::Success => "#9ECE6A",
        Role::Muted => "#565F89",
        Role::Accent => "#BB9AF7",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(pane_id: u32, source: &str, status: Status) -> StatusPayload {
        StatusPayload {
            pane_id,
            status,
            source: source.into(),
            ..Default::default()
        }
    }

    fn nerd() -> GlyphSet {
        GlyphSet::Nerd
    }

    /// Render every live pane as if it sat in one inactive tab. The aggregation
    /// tests care about which glyphs/counts/colours come out, not about tab
    /// topology, so this keeps them focused.
    fn no_agent_shown(out: &str) -> bool {
        !Kind::ALL.iter().any(|k| out.contains(k.mark(nerd())))
    }

    fn render_all(agents: &Agents) -> String {
        let map: HashMap<u32, usize> = agents.pane_ids().into_iter().map(|p| (p, 0)).collect();
        agents.render_tabs(&[tab(0, "t", false)], &map, nerd(), 0)
    }

    impl Agents {
        fn default_with(payload: &StatusPayload) -> Agents {
            let mut agents = Agents::default();
            agents.apply(payload, 0);
            agents
        }
    }

    #[test]
    fn empty_registry_renders_nothing() {
        assert!(no_agent_shown(&render_all(&Agents::default())));
    }

    #[test]
    fn a_single_agent_renders_its_mark_and_glyph_without_a_count() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        let out = render_all(&agents);
        // A lone agent shows vendor + spinner and NO trailing count digit.
        let cluster = format!("{} ", Kind::Claude.mark(nerd()));
        assert!(out.contains(&cluster), "{out:?}");
        // Look only at the cluster itself: from the vendor mark up to the next
        // colour directive. Beyond that, the palette hexes are full of digits.
        let from_mark = &out[out.find(Kind::Claude.mark(nerd())).unwrap()..];
        let cluster_only = &from_mark[..from_mark.find("#[").unwrap_or(from_mark.len())];
        assert!(
            !cluster_only.chars().any(|c| c.is_ascii_digit()),
            "no count for a single agent, got cluster {cluster_only:?} in {out:?}"
        );
    }

    #[test]
    fn same_vendor_on_two_panes_collapses_to_one_group_with_a_count() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Running), 0);
        let out = render_all(&agents);
        assert_eq!(out.matches(Kind::Claude.mark(nerd())).count(), 1, "{out:?}");
        assert!(out.contains('2'), "{out:?}");
    }

    #[test]
    fn a_vendors_group_takes_its_most_urgent_pane_status() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Error), 0);
        let out = render_all(&agents);
        assert!(out.contains(Status::Error.glyph_for(nerd())), "{out:?}");
        assert!(!out.contains(Status::Running.glyph_for(nerd())), "{out:?}");
    }

    #[test]
    fn groups_are_ordered_most_urgent_first() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "codex", Status::Error), 0);
        let out = render_all(&agents);
        let codex = out.find(Kind::Codex.mark(nerd())).expect("codex group");
        let claude = out.find(Kind::Claude.mark(nerd())).expect("claude group");
        assert!(codex < claude, "error must sort before running: {out:?}");
    }

    #[test]
    fn severity_order_is_error_pending_running_done() {
        // Pins the ordering this crate relies on. If `statuses!` is ever
        // reordered, aggregation silently changes meaning — this fails first.
        assert!(Status::Error > Status::Pending);
        assert!(Status::Pending > Status::Running);
        assert!(Status::Running > Status::Done);
        assert!(Status::Done > Status::Idle);
    }

    #[test]
    fn omp_is_a_first_class_vendor() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "omp", Status::Pending), 0);
        assert!(render_all(&agents).contains(Kind::Omp.mark(nerd())));
    }

    #[test]
    fn gone_drops_the_pane_regardless_of_status() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        let mut gone = payload(1, "claude", Status::Running);
        gone.gone = true;
        assert!(agents.apply(&gone, 1));
        assert!(no_agent_shown(&render_all(&agents)));
    }

    #[test]
    fn idle_drops_the_pane_rather_than_showing_a_muted_glyph() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(1, "claude", Status::Idle), 1);
        assert!(no_agent_shown(&render_all(&agents)));
    }

    #[test]
    fn ack_does_not_suppress_the_indicator() {
        // `ack` means "don't notify me again", not "hide the state" — the pane
        // is still pending and the bar must still say so.
        let mut agents = Agents::default();
        let mut acked = payload(1, "claude", Status::Pending);
        acked.ack = true;
        agents.apply(&acked, 0);
        assert!(render_all(&agents).contains(Status::Pending.glyph_for(nerd())));
    }

    #[test]
    fn re_applying_an_identical_observation_reports_no_change() {
        let mut agents = Agents::default();
        assert!(agents.apply(&payload(1, "claude", Status::Running), 0));
        assert!(!agents.apply(&payload(1, "claude", Status::Running), 0));
    }

    #[test]
    fn a_closed_pane_loses_its_entry() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "codex", Status::Running), 0);
        assert!(agents.retain_panes(&HashSet::from([2])));
        let out = render_all(&agents);
        assert!(!out.contains(Kind::Claude.mark(nerd())), "{out:?}");
        assert!(out.contains(Kind::Codex.mark(nerd())), "{out:?}");
    }

    #[test]
    fn a_silent_agent_expires_at_the_ttl() {
        let config = Config::default();
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        assert!(!agents.expire(DEFAULT_TTL_TICKS - 1, &config));
        assert!(agents.expire(DEFAULT_TTL_TICKS, &config));
        assert!(agents.is_empty());
    }

    #[test]
    fn done_expires_on_the_short_ttl_not_the_long_one() {
        let config = Config::default();
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Done), 0);
        assert!(agents.expire(DEFAULT_DONE_TTL_TICKS, &config));
        assert!(agents.is_empty());
    }

    #[test]
    fn rendered_output_never_contains_a_newline() {
        // zjstatus treats `\n` as its protocol line separator, so one in the
        // content would truncate or corrupt the widget.
        let mut agents = Agents::default();
        for (i, source) in ["claude", "codex", "omp", "gemini"].iter().enumerate() {
            agents.apply(&payload(i as u32, source, Status::Pending), 0);
        }
        assert!(!render_all(&agents).contains('\n'));
    }

    #[test]
    fn animation_is_on_by_default_and_switchable_off() {
        assert!(Config::default().animate());
        for off in ["false", "0", "no", "off"] {
            let c = Config::from_zellij_config(&BTreeMap::from([(
                "animate".to_string(),
                off.to_string(),
            )]));
            assert!(!c.animate(), "{off:?} must disable animation");
        }
        let c = Config::from_zellij_config(&BTreeMap::from([(
            "animate".to_string(),
            "true".to_string(),
        )]));
        assert!(c.animate());
    }

    #[test]
    fn config_reads_every_key_and_falls_back_on_junk() {
        let config = Config::from_zellij_config(&BTreeMap::from([
            ("pipe_name".to_string(), "pipe_custom".to_string()),
            ("glyphs".to_string(), "nerd".to_string()),
            ("ttl_secs".to_string(), "42".to_string()),
            ("done_ttl_secs".to_string(), "not-a-number".to_string()),
        ]));
        assert_eq!(config.pipe_payload("x"), "zjstatus::pipe::pipe_custom::x");
        assert_eq!(config.glyphs(), GlyphSet::Nerd);
        assert_eq!(config.ttl_ticks, 42);
        assert_eq!(config.done_ttl_ticks, DEFAULT_DONE_TTL_TICKS);
    }

    #[test]
    fn snapshot_round_trips_every_vendor_and_status() {
        let mut before = Agents::default();
        before.apply(&payload(0, "omp", Status::Running), 5);
        before.apply(&payload(1, "codex", Status::Pending), 5);
        before.apply(&payload(2, "claude", Status::Error), 5);

        let mut after = Agents::default();
        after.restore(&before.snapshot(), 0);

        assert_eq!(
            render_all(&after),
            render_all(&before),
            "a reload must reproduce the same bar"
        );
    }

    #[test]
    fn snapshot_of_an_empty_registry_restores_to_empty() {
        let mut after = Agents::default();
        after.restore(&Agents::default().snapshot(), 0);
        assert!(after.is_empty());
        assert!(no_agent_shown(&render_all(&after)));
    }

    #[test]
    fn snapshot_is_stable_for_an_unchanged_set() {
        // The writer skips unchanged content, so serialization must not depend
        // on HashMap iteration order or it would rewrite the file constantly.
        let mut agents = Agents::default();
        agents.apply(&payload(7, "codex", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Pending), 0);
        agents.apply(&payload(5, "omp", Status::Error), 0);
        let first = agents.snapshot();
        for _ in 0..20 {
            assert_eq!(agents.snapshot(), first);
        }
    }

    #[test]
    fn restore_skips_junk_lines_instead_of_failing() {
        let mut agents = Agents::default();
        let good = Agents::default_with(&payload(1, "claude", Status::Running));
        let mixed = format!("not json\n\n{}\n{{\"v\":1}}\n", good.snapshot());
        agents.restore(&mixed, 0);
        assert!(render_all(&agents).contains(Kind::Claude.mark(nerd())));
    }

    #[test]
    fn restored_entries_expire_from_the_reload_not_from_never() {
        let config = Config::default();
        let mut before = Agents::default();
        before.apply(&payload(1, "claude", Status::Running), 0);
        let mut after = Agents::default();
        after.restore(&before.snapshot(), 0);
        assert!(!after.expire(DEFAULT_TTL_TICKS - 1, &config));
        assert!(after.expire(DEFAULT_TTL_TICKS, &config));
    }

    fn tab(position: usize, name: &str, active: bool) -> TabView {
        TabView {
            position,
            name: name.into(),
            active,
            bell: false,
            flashing_bell: false,
            sync: false,
            fullscreen: false,
        }
    }

    fn in_tab(pairs: &[(u32, usize)]) -> HashMap<u32, usize> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_tab_without_an_agent_keeps_the_stock_look() {
        let out = Agents::default().render_tabs(&[tab(0, "notes", false)], &HashMap::new(), nerd(), 0);
        assert!(out.contains("notes"), "{out:?}");
        assert!(out.contains(" 1 "), "index is always shown \u{2014} navigation is GoToTab <n>: {out:?}");
        for kind in Kind::ALL {
            assert!(!out.contains(kind.mark(nerd())), "no vendor glyph on a plain tab: {out:?}");
        }
    }

    #[test]
    fn an_agents_tab_shows_vendor_status_separator_and_name() {
        let mut agents = Agents::default();
        agents.apply(&payload(4, "codex", Status::Pending), 0);
        let out = agents.render_tabs(&[tab(0, "build", false)], &in_tab(&[(4, 0)]), nerd(), 0);
        assert!(out.contains(Kind::Codex.mark(nerd())), "vendor icon: {out:?}");
        assert!(out.contains(Status::Pending.glyph_for(nerd())), "status: {out:?}");
        assert!(out.contains(" 1 "), "index still shown alongside the agent: {out:?}");
        assert!(out.contains("build"), "tab name: {out:?}");
        // Pending "needs you", so the NAME takes the status colour too, making the
        // tab findable without reading glyphs.
        assert!(
            out.contains(&format!("#[fg={}]build", role_hex(Status::Pending.role()))),
            "an attention state must colour the name: {out:?}"
        );
    }

    #[test]
    fn a_running_agent_animates_and_other_states_do_not() {
        let mut running = Agents::default();
        running.apply(&payload(1, "claude", Status::Running), 0);
        let map = in_tab(&[(1, 0)]);
        let frames: Vec<String> = (0..4)
            .map(|f| running.render_tabs(&[tab(0, "t", false)], &map, nerd(), f))
            .collect();
        assert!(
            frames.iter().collect::<std::collections::HashSet<_>>().len() > 1,
            "the working glyph must change between frames: {frames:?}"
        );

        let mut pending = Agents::default();
        pending.apply(&payload(1, "claude", Status::Pending), 0);
        let a = pending.render_tabs(&[tab(0, "t", false)], &map, nerd(), 0);
        let b = pending.render_tabs(&[tab(0, "t", false)], &map, nerd(), 9);
        assert_eq!(a, b, "a settled state must not animate");
    }

    #[test]
    fn any_running_drives_the_animation_cadence() {
        let mut agents = Agents::default();
        assert!(!agents.any_running());
        agents.apply(&payload(1, "claude", Status::Pending), 0);
        assert!(!agents.any_running(), "pending is settled, not working");
        agents.apply(&payload(2, "codex", Status::Running), 0);
        assert!(agents.any_running());
    }

    #[test]
    fn the_active_tab_takes_the_status_colour_as_its_background() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Error), 0);
        let out = agents.render_tabs(&[tab(0, "t", true)], &in_tab(&[(1, 0)]), nerd(), 0);
        assert!(out.contains("bg=#F7768E"), "error bg on the active tab: {out:?}");
    }

    #[test]
    fn agents_are_attributed_to_their_own_tab_only() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Error), 0);
        let tabs = [tab(0, "left", false), tab(1, "right", false)];
        // pane 1 lives in tab 1, so tab 0 must stay clean.
        let out = agents.render_tabs(&tabs, &in_tab(&[(1, 1)]), nerd(), 0);
        let mark = Kind::Claude.mark(nerd());
        assert_eq!(out.matches(mark).count(), 1, "exactly one tab decorated: {out:?}");
        // The decoration must sit in tab 1's segment, i.e. after tab 0's name.
        let after_left = out.find("left").unwrap();
        assert!(out.find(mark).unwrap() > after_left, "leaked into tab 0: {out:?}");
        assert!(out.contains(" 1 "), "tab 0 keeps its plain index: {out:?}");
    }

    #[test]
    fn a_tab_with_two_agents_shows_the_worst_status_and_a_count() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Error), 0);
        let out = agents.render_tabs(&[tab(0, "t", false)], &in_tab(&[(1, 0), (2, 0)]), nerd(), 0);
        assert!(out.contains(Status::Error.glyph_for(nerd())), "worst status: {out:?}");
        assert!(out.contains('2'), "count: {out:?}");
    }

    #[test]
    fn bells_render_and_a_flash_is_louder_than_a_settled_bell() {
        let mut t = tab(0, "t", false);
        t.bell = true;
        let settled = Agents::default().render_tabs(&[t.clone()], &HashMap::new(), nerd(), 0);
        assert!(settled.contains("#E0AF68"), "settled bell in attention: {settled:?}");

        t.flashing_bell = true;
        let flashing = Agents::default().render_tabs(&[t], &HashMap::new(), nerd(), 0);
        assert!(flashing.contains("#F7768E"), "flash in error: {flashing:?}");
    }

    #[test]
    fn a_tab_shows_every_vendor_at_work_in_it_not_just_the_worst() {
        // The old behaviour collapsed a tab to its single loudest vendor, which
        // hid the others outright.
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "codex", Status::Pending), 0);
        let map = in_tab(&[(1, 0), (2, 0)]);
        for active in [true, false] {
            let out = agents.render_tabs(&[tab(0, "t", active)], &map, nerd(), 0);
            assert!(out.contains(Kind::Claude.mark(nerd())), "claude missing (active={active}): {out:?}");
            assert!(out.contains(Kind::Codex.mark(nerd())), "codex missing (active={active}): {out:?}");
            // Most urgent first: pending outranks running.
            assert!(
                out.find(Kind::Codex.mark(nerd())) < out.find(Kind::Claude.mark(nerd())),
                "urgent vendor must lead: {out:?}"
            );
        }
    }

    #[test]
    fn one_pane_can_host_several_agents() {
        // A relay funnels a whole remote session through ONE local pane. Keying
        // observations by pane alone made each vendor evict the last, so only the
        // most recent remote agent was ever visible.
        let mut agents = Agents::default();
        agents.apply(&payload(7, "claude", Status::Running), 0);
        agents.apply(&payload(7, "codex", Status::Error), 0);
        agents.apply(&payload(7, "omp", Status::Pending), 0);
        let out = agents.render_tabs(&[tab(0, "relay", false)], &in_tab(&[(7, 0)]), nerd(), 0);
        for k in [Kind::Claude, Kind::Codex, Kind::Omp] {
            assert!(out.contains(k.mark(nerd())), "{k:?} evicted: {out:?}");
        }
    }

    #[test]
    fn the_same_vendor_twice_in_one_pane_still_merges() {
        // Per-vendor is the display granularity, so a repeat report updates rather
        // than accumulating.
        let mut agents = Agents::default();
        agents.apply(&payload(3, "claude", Status::Running), 0);
        agents.apply(&payload(3, "claude", Status::Error), 1);
        let out = agents.render_tabs(&[tab(0, "t", false)], &in_tab(&[(3, 0)]), nerd(), 0);
        assert_eq!(out.matches(Kind::Claude.mark(nerd())).count(), 1, "{out:?}");
        assert!(out.contains(Status::Error.glyph_for(nerd())), "latest status wins: {out:?}");
    }

    #[test]
    fn closing_a_pane_drops_all_of_its_agents() {
        let mut agents = Agents::default();
        agents.apply(&payload(4, "claude", Status::Running), 0);
        agents.apply(&payload(4, "codex", Status::Pending), 0);
        assert!(agents.retain_panes(&HashSet::new()));
        assert!(agents.is_empty(), "both vendors on the dead pane must go");
    }

    #[test]
    fn snapshot_round_trips_several_agents_sharing_one_pane() {
        let mut before = Agents::default();
        before.apply(&payload(9, "claude", Status::Running), 0);
        before.apply(&payload(9, "omp", Status::Error), 0);
        let mut after = Agents::default();
        after.restore(&before.snapshot(), 0);
        let map = in_tab(&[(9, 0)]);
        assert_eq!(
            after.render_tabs(&[tab(0, "t", false)], &map, nerd(), 0),
            before.render_tabs(&[tab(0, "t", false)], &map, nerd(), 0)
        );
    }

    #[test]
    fn the_active_tab_shows_no_index_but_inactive_ones_do() {
        // The index is a jump target for `GoToTab <n>`; you cannot jump to the tab
        // you are already on, so showing it there is pure noise.
        let tabs = [tab(0, "here", true), tab(1, "there", false)];
        let out = Agents::default().render_tabs(&tabs, &HashMap::new(), nerd(), 0);
        assert!(!out.contains(" 1 "), "active tab must not show its index: {out:?}");
        assert!(out.contains(" 2 "), "inactive tab keeps its index: {out:?}");
    }

    #[test]
    fn a_divider_separates_the_agent_cluster_from_the_name() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        let map = in_tab(&[(1, 0)]);
        for active in [true, false] {
            let out = agents.render_tabs(&[tab(0, "name", active)], &map, nerd(), 0);
            let d = out.find(DIVIDER).expect("divider present");
            assert!(
                d < out.find("name").unwrap(),
                "divider must sit between glyphs and name (active={active}): {out:?}"
            );
        }
        // A tab with no agent has nothing to divide.
        let plain = Agents::default().render_tabs(&[tab(0, "name", false)], &map, nerd(), 0);
        assert!(!plain.contains(DIVIDER), "no divider without an agent: {plain:?}");
    }

    #[test]
    fn zellijs_own_indicators_survive_the_takeover() {
        // The plugin owns the strip now, so if it does not draw these they vanish
        // silently — and forgetting that input is broadcast to every pane is
        // exactly the kind of surprise a status bar exists to prevent.
        let mut t = tab(0, "t", false);
        t.sync = true;
        let out = Agents::default().render_tabs(&[t.clone()], &HashMap::new(), nerd(), 0);
        assert!(out.contains("[sync]"), "{out:?}");
        assert!(!out.contains("[full]"), "{out:?}");

        t.fullscreen = true;
        let both = Agents::default().render_tabs(&[t], &HashMap::new(), nerd(), 0);
        assert!(both.contains("[sync]") && both.contains("[full]"), "{both:?}");
    }

    #[test]
    fn a_working_tab_keeps_a_calm_name_while_an_urgent_one_does_not() {
        // Busy is not urgent. If "running" shouted as loudly as "needs you" the
        // strip would train you to ignore it.
        let mut running = Agents::default();
        running.apply(&payload(1, "claude", Status::Running), 0);
        let calm = running.render_tabs(&[tab(0, "t", false)], &in_tab(&[(1, 0)]), nerd(), 0);
        assert!(calm.contains(&format!("#[fg={TAB_NAME}]t")), "{calm:?}");

        let mut urgent = Agents::default();
        urgent.apply(&payload(1, "claude", Status::Error), 0);
        let loud = urgent.render_tabs(&[tab(0, "t", false)], &in_tab(&[(1, 0)]), nerd(), 0);
        assert!(loud.contains(&format!("#[fg={}]t", role_hex(Role::Error))), "{loud:?}");
    }

    #[test]
    fn the_spinner_is_a_hollow_square_that_never_goes_sparse() {
        // Every frame must keep the ring readable: 11 of 12 perimeter dots lit.
        // A frame that dropped to a few dots is the bug this replaced.
        for f in &SPINNER {
            let dots: u32 = f
                .chars()
                .map(|c| (c as u32 - 0x2800).count_ones())
                .sum();
            assert_eq!(dots, 11, "frame {f:?} should light 11 dots");
        }
        assert_eq!(SPINNER.len(), 12, "one frame per perimeter dot");
    }

    #[test]
    fn a_tab_with_no_bell_gets_no_bell_glyph() {
        let out = Agents::default().render_tabs(&[tab(0, "t", false)], &HashMap::new(), nerd(), 0);
        assert!(!out.contains('\u{f009a}'), "{out:?}");
    }

    #[test]
    fn the_tab_strip_never_contains_a_newline() {
        // zjstatus reads '\n' as a directive separator, so one here would corrupt
        // the widget — and this string is published alongside a second directive.
        let mut agents = Agents::default();
        agents.apply(&payload(1, "omp", Status::Running), 0);
        let mut t = tab(0, "a\nb", true);
        t.bell = true;
        let out = agents.render_tabs(&[t, tab(1, "x", false)], &in_tab(&[(1, 0)]), nerd(), 3);
        assert!(!out.contains('\n'), "{out:?}");
    }

    #[test]
    fn payload_is_the_exact_zjstatus_pipe_line_for_the_configured_widget() {
        // The widget key must include the `pipe_` prefix: zjstatus strips only the
        // trailing `_format` / `_rendermode` from its config key, so
        // `pipe_agents_tabs_format` -> `pipe_agents_tabs`.
        assert_eq!(
            Config::default().pipe_payload("X"),
            "zjstatus::pipe::pipe_agents_tabs::X"
        );
    }

    #[test]
    fn an_unknown_source_still_renders_as_other() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "some-new-agent", Status::Running), 0);
        assert!(render_all(&agents).contains(Kind::Other.mark(nerd())));
    }
}
