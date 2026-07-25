//! Flat live-agent rail rendering.

use super::{
    pad_glyph, prefixed_line, spin_glyph, target_for_row, tc_fg, truncate, Line, LineBg,
    RenderOpts, RenderedRail, Seg,
};
use crate::rollup::{PaneDisplay, TabRow};
use crate::status::{Role, Status};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Lines a flat agent card occupies: one identity line, plus an attention
/// line only while the agent needs you AND sent an informative message.
/// The runtime's scroll clamp sums this per pane.
pub fn flat_card_lines(pane: &PaneDisplay) -> usize {
    if attention_extra(pane).is_some() {
        2
    } else {
        1
    }
}

/// The attention text for a card's second line: only while the agent needs
/// you, and only a real message (a question, not an echo of the state the
/// glyph already shows).
fn attention_extra(pane: &PaneDisplay) -> Option<&str> {
    const STATUS_ECHOES: [&str; 4] = ["working", "done", "needs you", "error"];
    if !pane.render_status().needs_you() {
        return None;
    }
    let msg = pane.msg().trim();
    if msg.is_empty() || msg == pane.kind().as_source() || STATUS_ECHOES.contains(&msg) {
        return None;
    }
    Some(msg)
}

/// One flat agent row (single line), pushed into `out`:
///
/// ```text
/// ⣶ 󰭆 radar             ← status glyph + BRIGHT vendor mark + session (dim)
///   approve deploy?     ← ONLY when needing attention with a real message
/// ```
///
/// The vendor mark identifies the agent kind; the dim text is the Zellij tab
/// name. Internal agent task/title strings change independently of the tab and
/// must not hide the stable name the user chose for it. The attention line
/// renders in the status role color. All card lines carry the pane's click
/// target.
fn push_flat_agent_card(out: &mut Vec<Line>, row: &TabRow, pane: &PaneDisplay, opts: &RenderOpts) {
    let status = pane.render_status();
    let glyph = if status == Status::Running {
        spin_glyph(opts.now_tick).to_string()
    } else {
        pad_glyph(status.glyph_for(opts.glyphs))
    };
    let glyph_w = UnicodeWidthStr::width(glyph.as_str());
    let mark = pane.kind().mark(opts.glyphs);
    let mark_w = UnicodeWidthChar::width(mark).unwrap_or(1);
    let dim = tc_fg(opts.theme.dim_strong);
    // The agent you're inside RIGHT NOW: focused pane of the active tab.
    // Session-wide facts (PaneManifest focus + TabUpdate active), so every
    // rail instance draws the same cue.
    let here = row.active && pane.is_focused();

    let tab = row.name.trim();
    let task = pane.task().trim();
    let place = if tab.is_empty() { task } else { tab };

    let mut target = target_for_row(row);
    target.pane_id = Some(pane.pane_id());

    // The one card line: `▌{glyph} {mark} {place}` — the mark is the
    // identity: bright (bold, default fg), attention-colored when the agent
    // needs you; the session name rides dim beside it. The focused agent
    // (active tab + focused pane) carries an accent spine in col 0 and a
    // bold, full-brightness name.
    let spine = if here { "▌" } else { " " };
    let title_text = prefixed_line(
        opts.width,
        1 + glyph_w + 1 + mark_w + 1,
        || format!("{spine}{glyph} {mark} {place}"),
        |avail| {
            let glyph_seg = Seg {
                color: status.role().ansi(),
                bold: status != Status::Idle,
                text: glyph.clone().into(),
            };
            let mark_seg = Seg {
                color: if status.needs_you() {
                    status.role().ansi()
                } else {
                    ""
                },
                bold: true,
                text: mark.to_string().into(),
            };
            let place_seg = Seg {
                color: if here { "" } else { &dim },
                bold: here,
                text: truncate(place, avail).into(),
            };
            let spine_seg = if here {
                Seg::new(Role::Accent.ansi(), "▌").to_string()
            } else {
                " ".to_string()
            };
            format!("{spine_seg}{glyph_seg} {mark_seg} {place_seg}")
        },
    );
    out.push(Line::new(title_text, Some(target.clone()), LineBg::None));

    // Attention line: `   {msg}` in the status role color — only when the
    // agent needs you and said something the glyph doesn't already convey.
    if let Some(msg) = attention_extra(pane) {
        let color = status.role().ansi();
        let attention_text = prefixed_line(
            opts.width,
            3,
            || format!("   {msg}"),
            |avail| format!("   {}", Seg::new(color, truncate(msg, avail))),
        );
        out.push(Line::new(attention_text, Some(target), LineBg::None));
    }
}

pub(super) fn render_agents_only(rows: &[TabRow], opts: &RenderOpts) -> RenderedRail {
    let mut lines: Vec<Line> = (0..opts.agents_pad_top)
        .map(|_| Line::new("\n".to_string(), None, LineBg::None))
        .collect();
    let mut first = true;
    for row in rows {
        for pane in row.display.panes.iter().filter(|pane| pane.is_agent()) {
            first = false;
            // Single-line rows need no breathing separator — density is the
            // point; an attention line is the only second row a card grows.
            push_flat_agent_card(&mut lines, row, pane, opts);
        }
    }
    if first {
        return RenderedRail::empty();
    }

    RenderedRail::from_lines(
        lines
            .into_iter()
            .skip(opts.agents_offset)
            .take(opts.height)
            .collect(),
    )
}
