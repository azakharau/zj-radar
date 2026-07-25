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
use zj_radar_core::{parse, to_wire, Kind, Status, StatusPayload};

/// The zjstatus widget key. It includes the `pipe_` prefix on purpose: zjstatus
/// derives its map key by stripping only the trailing `_format` / `_rendermode`
/// from the config key (its `PIPE_REGEX` is `_[a-zA-Z0-9]+$`), so
/// `pipe_agents_format` → `pipe_agents`, and the wire payload must match.
pub const DEFAULT_PIPE_NAME: &str = "pipe_agents";

/// Ticks (≈ seconds) before an agent that has stopped reporting is dropped. A
/// producer that dies without sending `gone` — killed terminal, crashed hook —
/// would otherwise pin its glyph in the bar forever.
pub const DEFAULT_TTL_TICKS: u64 = 900;

/// `Done` is a transient "it just finished" marker, not a state worth holding
/// for the full TTL: without this it would sit in the bar until the pane is
/// reused. Short enough to read, long enough to notice.
pub const DEFAULT_DONE_TTL_TICKS: u64 = 30;

/// Runtime configuration, read from the `load_plugins` node in `config.kdl`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pipe_name: String,
    glyphs: GlyphSet,
    ttl_ticks: u64,
    done_ttl_ticks: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            glyphs: GlyphSet::default(),
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
            ttl_ticks: parse_ticks(configuration, "ttl_secs").unwrap_or(defaults.ttl_ticks),
            done_ttl_ticks: parse_ticks(configuration, "done_ttl_secs")
                .unwrap_or(defaults.done_ttl_ticks),
        }
    }

    pub fn glyphs(&self) -> GlyphSet {
        self.glyphs
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

/// Live agent observations, keyed by Zellij terminal pane id.
#[derive(Default, Debug)]
pub struct Agents {
    entries: HashMap<u32, Entry>,
}

impl Agents {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fold one status payload in. Returns whether the aggregate could have
    /// changed — the caller uses that only to skip work; the published-string
    /// comparison is what actually suppresses redundant repaints.
    ///
    /// `gone` and `Idle` both mean "stop showing this pane": `gone` is the
    /// producer saying the agent is finished with the pane, `Idle` is the
    /// status vocabulary's own "nothing to report", and neither earns a glyph.
    pub fn apply(&mut self, payload: &StatusPayload, now: u64) -> bool {
        if payload.gone || payload.status == Status::Idle {
            return self.entries.remove(&payload.pane_id).is_some();
        }
        let entry = Entry {
            kind: Kind::from_source(&payload.source),
            status: payload.status,
            seen: now,
        };
        self.entries.insert(payload.pane_id, entry) != Some(entry)
    }

    /// Drop observations whose pane no longer exists. A closed terminal takes
    /// its agent with it, and no producer gets to run a hook on the way out.
    pub fn retain_panes(&mut self, live: &HashSet<u32>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|pane_id, _| live.contains(pane_id));
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
            .map(|(pane_id, entry)| {
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

    /// The `{pipe_agents}` string: one `icon + glyph` group per agent vendor,
    /// most urgent first, with a count appended when a vendor holds more than
    /// one pane. Empty when nothing is live, so the widget disappears entirely.
    ///
    /// Grouping walks `Kind::ALL` rather than hashing `Kind` (which is neither
    /// `Hash` nor `Ord`); the list is short and its order is the tiebreak for
    /// vendors sharing a status.
    pub fn render(&self, glyphs: GlyphSet) -> String {
        let mut groups: Vec<(Kind, Status, usize)> = Kind::ALL
            .iter()
            .filter_map(|&kind| {
                let mut worst: Option<Status> = None;
                let mut count = 0usize;
                for entry in self.entries.values().filter(|e| e.kind == kind) {
                    // `Status` is declared in ascending-severity order and derives
                    // `Ord`, so `max` IS the documented aggregation rule
                    // (error > pending > running > done).
                    worst = Some(worst.map_or(entry.status, |w: Status| w.max(entry.status)));
                    count += 1;
                }
                worst.map(|status| (kind, status, count))
            })
            .collect();
        if groups.is_empty() {
            return String::new();
        }
        // Stable sort, so vendors sharing a status keep `Kind::ALL` order.
        groups.sort_by_key(|group| std::cmp::Reverse(group.1));

        let mut out = String::new();
        for (kind, status, count) in groups {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str("#[fg=");
            out.push_str(role_hex(status.role()));
            out.push(']');
            out.push(kind.mark(glyphs));
            out.push(status.glyph_for(glyphs));
            if count > 1 {
                out.push_str(&count.to_string());
            }
        }
        // The widget sits directly against `{pipe_resources}` in `format_right`;
        // the trailing space is the separator, and it must not exist when the
        // widget is empty or the bar gains a phantom gap.
        out.push(' ');
        out
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

    impl Agents {
        fn default_with(payload: &StatusPayload) -> Agents {
            let mut agents = Agents::default();
            agents.apply(payload, 0);
            agents
        }
    }

    #[test]
    fn empty_registry_renders_nothing() {
        assert_eq!(Agents::default().render(nerd()), "");
    }

    #[test]
    fn a_single_agent_renders_its_mark_and_glyph_without_a_count() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        let out = agents.render(nerd());
        // Compared whole, not by `contains`: the `#[fg=#7AA2F7]` colour prefix
        // has digits of its own, so a substring check for "no count" would pass
        // vacuously.
        assert_eq!(
            out,
            format!(
                "#[fg=#7AA2F7]{}{} ",
                Kind::Claude.mark(nerd()),
                Status::Running.glyph_for(nerd())
            )
        );
    }

    #[test]
    fn same_vendor_on_two_panes_collapses_to_one_group_with_a_count() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Running), 0);
        let out = agents.render(nerd());
        assert_eq!(out.matches(Kind::Claude.mark(nerd())).count(), 1, "{out:?}");
        assert!(out.contains('2'), "{out:?}");
    }

    #[test]
    fn a_vendors_group_takes_its_most_urgent_pane_status() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "claude", Status::Error), 0);
        let out = agents.render(nerd());
        assert!(out.contains(Status::Error.glyph_for(nerd())), "{out:?}");
        assert!(!out.contains(Status::Running.glyph_for(nerd())), "{out:?}");
    }

    #[test]
    fn groups_are_ordered_most_urgent_first() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(2, "codex", Status::Error), 0);
        let out = agents.render(nerd());
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
        assert!(agents.render(nerd()).contains(Kind::Omp.mark(nerd())));
    }

    #[test]
    fn gone_drops_the_pane_regardless_of_status() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        let mut gone = payload(1, "claude", Status::Running);
        gone.gone = true;
        assert!(agents.apply(&gone, 1));
        assert_eq!(agents.render(nerd()), "");
    }

    #[test]
    fn idle_drops_the_pane_rather_than_showing_a_muted_glyph() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "claude", Status::Running), 0);
        agents.apply(&payload(1, "claude", Status::Idle), 1);
        assert_eq!(agents.render(nerd()), "");
    }

    #[test]
    fn ack_does_not_suppress_the_indicator() {
        // `ack` means "don't notify me again", not "hide the state" — the pane
        // is still pending and the bar must still say so.
        let mut agents = Agents::default();
        let mut acked = payload(1, "claude", Status::Pending);
        acked.ack = true;
        agents.apply(&acked, 0);
        assert!(agents.render(nerd()).contains(Status::Pending.glyph_for(nerd())));
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
        let out = agents.render(nerd());
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
    fn payload_is_the_exact_zjstatus_pipe_line() {
        let config = Config::default();
        assert_eq!(
            config.pipe_payload("#[fg=#7AA2F7]x "),
            "zjstatus::pipe::pipe_agents::#[fg=#7AA2F7]x "
        );
    }

    #[test]
    fn rendered_output_never_contains_a_newline() {
        // zjstatus treats `\n` as its protocol line separator, so one in the
        // content would truncate or corrupt the widget.
        let mut agents = Agents::default();
        for (i, source) in ["claude", "codex", "omp", "gemini"].iter().enumerate() {
            agents.apply(&payload(i as u32, source, Status::Pending), 0);
        }
        assert!(!agents.render(nerd()).contains('\n'));
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
            after.render(nerd()),
            before.render(nerd()),
            "a reload must reproduce the same bar"
        );
    }

    #[test]
    fn snapshot_of_an_empty_registry_restores_to_empty() {
        let mut after = Agents::default();
        after.restore(&Agents::default().snapshot(), 0);
        assert!(after.is_empty());
        assert_eq!(after.render(nerd()), "");
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
        assert!(agents.render(nerd()).contains(Kind::Claude.mark(nerd())));
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

    #[test]
    fn an_unknown_source_still_renders_as_other() {
        let mut agents = Agents::default();
        agents.apply(&payload(1, "some-new-agent", Status::Running), 0);
        assert!(agents.render(nerd()).contains(Kind::Other.mark(nerd())));
    }
}
