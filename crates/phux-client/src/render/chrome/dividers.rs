//! Pane dividers, pane titles, and the pane-grid rail — ratatui composer.
//!
//! Replaces `attach::multi_pane::paint_dividers` with a ratatui-based
//! composer that respects the **skip-cell carve-out** invariant from
//! ADR-0020: every cell inside a pane interior `Rect` is marked
//! `Cell::skip` so libghostty's direct VT output owns those cells
//! exclusively. The composer only emits VT bytes for the chrome cells
//! around panes — never for pane interiors.
//!
//! # The visual model
//!
//! Panes SHARE their rules. A split costs exactly one cell of chrome,
//! not two adjacent borders, so a 2x2 grid is one `\u{2502}` column and one
//! `\u{2500}` row crossing at a `\u{253c}`. The grid is closed at the top by a
//! **rail** — one row above the pane area — which is where each pane's
//! title lives.
//!
//! Two rules make it read as one object rather than a pile of glyphs:
//!
//! 1. **One stroke weight.** Every rule uses the LIGHT box-drawing set.
//!    Focus used to be drawn as a HEAVY rule, which forced the
//!    mixed-weight junction pictographs (`\u{2545}` `\u{2546}` `\u{2548}` `\u{2549}` ...) at
//!    every crossing; most terminal fonts either lack them or draw
//!    strokes that miss their light neighbours, so an emphasised grid
//!    looked like a broken one.
//! 2. **Focus is colour.** The rules on the focused pane's own frame
//!    (and its title) take `theme.divider_focus` + `BOLD`; everything
//!    else recedes to `theme.divider`. Bold is carried alongside the
//!    colour so the cue survives a terminal that ignores our fg.
//!
//! Emphasis is resolved HERE, from the `focused` pane the caller passes
//! and its rect, rather than in the rasterizer. The rasterizer only sees
//! the layout tree, whose remembered `focus` lags the client's — which
//! made the old heavy-rule emphasis point at the pane you had just left.
//!
//! # Architecture
//!
//! `compute_layout` (in `attach::multi_pane`) stays pure data: it walks
//! the layout tree and yields per-pane `Rect`s plus a list of
//! pre-resolved `DividerCell`s (one per box-drawing cell, junction shape
//! already resolved). This module consumes that data:
//!
//! 1. Allocate a ratatui `Buffer` covering the full viewport.
//! 2. Mark every cell inside a pane interior `Rect` with
//!    `set_diff_option(CellDiffOption::Skip)`.
//! 3. Write each `DividerCell` glyph + style into the buffer.
//! 4. Draw the rail across the top of the pane area, resolving `\u{252c}`
//!    where a vertical rule meets it.
//! 5. Inset each pane's title into the rule above it.
//! 6. Emit positioned VT bytes for non-skip cells only.
//!
//! Step 6 is hand-rolled (not via `CrosstermBackend`) because:
//! - There is no previous-frame buffer to diff against; we always paint
//!   from scratch (the orchestrator owns frame-level invalidation).
//! - We must not touch any pane interior cell — even a no-op write would
//!   stomp libghostty's SGR / cursor state.
//!
//! # Invariants
//!
//! - **Skip-cell**: VT bytes are emitted only for non-skip cells. A unit
//!   test (`skip_cells_never_emit_vt`) asserts no CUP target lands
//!   inside any pane interior `Rect`. THIS IS LOAD-BEARING — if it
//!   breaks, libghostty's pane output gets stomped.
//! - **SGR reset**: emits `\x1b[0m` before any chrome paint and again
//!   after the last cell, so leftover SGR from a prior pane render or
//!   from a rule's own styling never bleeds into the next paint.
//! - **No cursor positioning at exit**: the focused-pane render runs
//!   after dividers and is responsible for the final cursor placement.

use std::io::{self, Write};

use phux_protocol::TerminalId;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect as RataRect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::agent_meta::AgentMetaState;
use crate::attach::multi_pane::PaneLayout;
use crate::layout::Rect;
use crate::render::ELLIPSIS;
use crate::render::chrome::{AgentBadge, agent_badge, attention_badge};
use crate::render::overlay::HardcodedBinding;
use crate::render::theme::Theme;

/// The divider drag-resize table for handler-adjacency tests
/// section (phux-i0e8.10.3).
///
/// COLOCATED with the divider layer: the drag
/// itself is driven by the dispatcher's ADR-0048 grab, whose targets are
/// the `divider_hits` that `compute_layout` produces alongside the glyph
/// cells this module paints. The `help_table_targets_exist` adjacency
/// test asserts a split layout actually yields those grab targets.
pub static HELP_BINDINGS: &[HardcodedBinding] = &[HardcodedBinding {
    chord: "drag divider",
    action: "resize the panes either side",
}];

/// Cells of chrome a pane title needs before any of its label shows:
/// one lead-in `\u{2500}`, a space, a space, and the closing `\u{2500}` the run
/// continues into. A pane narrower than this simply gets no title.
const TITLE_CHROME_CELLS: u16 = 4;

/// What to write into one pane's top rule.
///
/// Borrowed rather than owned: the caller already holds the pane's
/// cached OSC-2 title, and a per-frame `String` per pane is exactly the
/// allocation this layer exists without.
#[derive(Debug, Clone, Copy)]
pub struct PaneLabel<'a> {
    /// The pane's display name — its OSC-2 title in practice.
    pub text: &'a str,
    /// The pane's declared agent lifecycle state, when it runs an agent
    /// (ADR-0040). `None` for an ordinary shell pane.
    pub agent: Option<AgentMetaState>,
    /// `true` when the pane is waiting on a human (ADR-0035 asked).
    pub attention: bool,
    /// `true` once the user has visited the pane since its last state
    /// change; drives the "finished but unread" badge.
    pub seen: bool,
}

impl PaneLabel<'_> {
    /// The badge to draw ahead of the label, if any.
    fn badge(&self, theme: &Theme) -> Option<AgentBadge> {
        match (self.agent, self.attention) {
            (Some(state), _) => Some(agent_badge(theme, state, self.attention, self.seen)),
            (None, true) => Some(attention_badge(theme)),
            (None, false) => None,
        }
    }
}

/// Render the divider layer for `layout` to `out`.
///
/// `content` is the pane area `Rect` the layout was tiled into; the rail
/// is drawn on the row immediately above it (skipped when `content.y`
/// is 0, i.e. no row was reserved). `focused` is the CLIENT's focused
/// pane — the authority for which frame is emphasised. `label_of`
/// supplies each pane's title; return `None` for a pane that should stay
/// unlabelled.
///
/// # Behavior
///
/// - No-op when there is no chrome to draw at all (no dividers and no
///   rail row).
/// - Emits a leading `\x1b[0m` SGR reset, then the chrome cells as
///   style-coalesced runs, then a trailing `\x1b[0m`.
/// - **Does not** emit a final cursor position. The focused pane's
///   render runs after this and owns cursor placement, per the
///   chrome <-> pane handoff documented in ADR-0020.
/// - Pane interior cells are marked `Cell::skip` and never emitted —
///   libghostty owns those cells exclusively.
///
/// # Errors
///
/// Forwards any `io::Error` from `out`.
pub fn render_dividers<'p, W, F>(
    out: &mut W,
    layout: &PaneLayout,
    content: Rect,
    focused: Option<&TerminalId>,
    theme: &Theme,
    label_of: F,
) -> io::Result<()>
where
    W: Write,
    F: Fn(&TerminalId) -> Option<PaneLabel<'p>>,
{
    let (cols, rows) = layout.viewport;
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    if layout.dividers.is_empty() && rail_row(content).is_none_or(|y| y >= rows) {
        return Ok(());
    }

    let buffer = compose_buffer(layout, content, focused, theme, label_of);
    emit_buffer(out, &buffer)
}

/// The row the pane-grid rail occupies, or `None` when the pane area is
/// flush with the top of the viewport (nothing was reserved for it).
#[must_use]
pub const fn rail_row(content: Rect) -> Option<u16> {
    if content.y == 0 || content.w == 0 || content.h == 0 {
        None
    } else {
        Some(content.y - 1)
    }
}

/// Build the ratatui `Buffer` for the chrome layer.
///
/// Public-in-crate so the skip-cell invariant test can introspect the
/// buffer without re-emitting bytes, and so the rendered-frame compositor
/// (`phux snapshot --rendered`, phux-l5xa) can overlay the chrome cells
/// into its dense cell grid without re-parsing emitted VT.
pub(crate) fn compose_buffer<'p, F>(
    layout: &PaneLayout,
    content: Rect,
    focused: Option<&TerminalId>,
    theme: &Theme,
    label_of: F,
) -> Buffer
where
    F: Fn(&TerminalId) -> Option<PaneLabel<'p>>,
{
    let (cols, rows) = layout.viewport;
    // The focused pane's rect is the whole emphasis model: a rule is on
    // the focused frame exactly when it lies on that rect's perimeter.
    let frame = focused.and_then(|id| layout.rects.get(id)).copied();
    let area = RataRect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    // 1. Mark the WHOLE viewport skip. Everything this composer does not
    //    explicitly paint belongs to somebody else — a pane interior
    //    (libghostty owns those cells and ratatui must not emit into
    //    them), the sidebar strip, the status row, or an overlay. Only
    //    the cells written below clear the flag, so the layer can never
    //    emit a blank over another layer's content, and a pane interior
    //    is covered whether or not its `Rect` was well-formed.
    for y in 0..rows {
        for x in 0..cols {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }

    // 2. Paint each divider glyph. compute_layout has already resolved
    //    junction shapes per cell, so we just drop the symbol in and
    //    tint it by its focus flag. Setting the symbol does NOT clear
    //    skip on its own, so we explicitly unset skip here — without
    //    that, a degenerate layout where a divider cell overlapped a
    //    pane rect would silently drop the divider.
    let mut sbuf = [0u8; 4];
    for cell in &layout.dividers {
        if cell.x >= cols || cell.y >= rows {
            continue;
        }
        let symbol = cell.ch.encode_utf8(&mut sbuf);
        if let Some(c) = buf.cell_mut((cell.x, cell.y)) {
            c.set_symbol(symbol);
            c.set_style(rule_style(theme, on_frame(frame, cell.x, cell.y)));
            c.set_diff_option(CellDiffOption::None);
        }
    }

    // 3. Close the grid at the top. The rail runs the full width of the
    //    pane area and turns into a `\u{252c}` wherever a vertical rule drops
    //    out of it, so the grid reads as one frame rather than as loose
    //    lines that happen to line up.
    draw_rail(&mut buf, layout, content, frame, theme);

    // 4. Inset each pane's title into the rule above it.
    draw_titles(&mut buf, layout, content, focused, theme, label_of);

    buf
}

/// The style of one rule cell.
fn rule_style(theme: &Theme, focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme.divider_focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.divider)
    }
}

/// Draw the rail across the top of the pane area.
///
/// A rail cell is a `\u{252c}` where a pane boundary drops out of it (which
/// is exactly where a divider cell sits on the pane area's first row)
/// and a plain `\u{2500}` everywhere else. The rail carries the same focus
/// tint as the rules below it, so the focused pane's frame is
/// continuous around its whole perimeter.
///
/// Two linear passes — lay the full rule, then walk the divider cells
/// once and tee the columns they occupy. Probing every column against
/// the divider list instead would be `O(columns x dividers)` per frame.
fn draw_rail(
    buf: &mut Buffer,
    layout: &PaneLayout,
    content: Rect,
    frame: Option<Rect>,
    theme: &Theme,
) {
    let Some(y) = rail_row(content) else {
        return;
    };
    let (cols, rows) = layout.viewport;
    if y >= rows {
        return;
    }
    let x1 = content.x.saturating_add(content.w).min(cols);
    for x in content.x..x1 {
        put(
            buf,
            x,
            y,
            "\u{2500}",
            rule_style(theme, on_frame(frame, x, y)),
        );
    }
    for cell in &layout.dividers {
        if cell.y == content.y
            && cell.x >= content.x
            && cell.x < x1
            && matches!(
                cell.ch,
                '\u{2502}' | '\u{251c}' | '\u{2524}' | '\u{253c}' | '\u{252c}'
            )
        {
            let style = rule_style(theme, on_frame(frame, cell.x, y));
            put(buf, cell.x, y, "\u{252c}", style);
        }
    }
}

/// `true` when `(x, y)` lies on the perimeter of the focused pane's
/// rect — the ring of chrome cells one step outside it, corners
/// included.
///
/// This is the whole focus model, and it is deliberately geometric: any
/// rule cell touching the focused pane is part of its frame, whichever
/// split produced it, so a `┼` where the focused pane's left rule
/// crosses an unrelated horizontal rule is emphasised rather than left
/// recessive in the middle of the frame.
fn on_frame(frame: Option<Rect>, x: u16, y: u16) -> bool {
    frame.is_some_and(|r| {
        let left = r.x.saturating_sub(1);
        let right = r.x.saturating_add(r.w);
        let top = r.y.saturating_sub(1);
        let bottom = r.y.saturating_add(r.h);
        let in_rows = y >= top && y <= bottom;
        let in_cols = x >= left && x <= right;
        in_rows && in_cols && (x == left || x == right || y == top || y == bottom)
    })
}

/// Inset each pane's title into the rule above it.
fn draw_titles<'p, F>(
    buf: &mut Buffer,
    layout: &PaneLayout,
    content: Rect,
    focused: Option<&TerminalId>,
    theme: &Theme,
    label_of: F,
) where
    F: Fn(&TerminalId) -> Option<PaneLabel<'p>>,
{
    for (id, rect) in &layout.rects {
        // The rule above the pane: the rail for a top-row pane, the
        // interior horizontal divider otherwise. A pane whose top row is
        // 0 has no rule to write into.
        let Some(y) = rect.y.checked_sub(1) else {
            continue;
        };
        if y >= layout.viewport.1 || rect.w <= TITLE_CHROME_CELLS {
            continue;
        }
        let Some(label) = label_of(id) else {
            continue;
        };
        draw_one_title(buf, *rect, y, &label, theme, focused == Some(id));
    }
    // `content` is only consulted for the rail's extent, which
    // `draw_rail` already handled; naming it here keeps the two passes'
    // signatures symmetric for the caller.
    let _ = content;
}

/// Write ` <badge> <label> ` into the rule at row `y`, starting one cell
/// inside the pane's left edge.
fn draw_one_title(
    buf: &mut Buffer,
    rect: Rect,
    y: u16,
    label: &PaneLabel<'_>,
    theme: &Theme,
    focused: bool,
) {
    let text = label.text.trim();
    if text.is_empty() {
        return;
    }
    let badge = label.badge(theme);
    // Budget: the pane's width, less the lead-in rule cell, the two
    // padding spaces, and one closing rule cell.
    let mut budget = usize::from(rect.w - TITLE_CHROME_CELLS);
    let badge_cells = usize::from(u8::from(badge.is_some())) * 2;
    if budget <= badge_cells {
        return;
    }
    budget -= badge_cells;

    let title_style = if focused {
        Style::default()
            .fg(theme.pane_title_focus)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.pane_title)
    };
    let pad = Style::default().fg(if focused {
        theme.divider_focus
    } else {
        theme.divider
    });

    let mut x = rect.x + 1;
    x = put(buf, x, y, " ", pad);
    if let Some(b) = badge {
        let mut style = Style::default().fg(b.color);
        if b.emphatic {
            style = style.add_modifier(Modifier::BOLD);
        }
        x = put(buf, x, y, b.glyph, style);
        x = put(buf, x, y, " ", pad);
    }
    x = put_clipped(buf, x, y, text, budget, title_style);
    put(buf, x, y, " ", pad);
}

/// Write one already-1-cell symbol and return the next column.
fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) -> u16 {
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_symbol(symbol);
        cell.set_style(style);
        cell.set_diff_option(CellDiffOption::None);
    }
    x.saturating_add(1)
}

/// Write `text` clipped to `budget` DISPLAY CELLS, marking a cut with
/// [`ELLIPSIS`], and return the next column.
///
/// Display cells, not chars: a pane title is an arbitrary OSC-2 string,
/// so a CJK or emoji title measured by `chars().count()` overflows its
/// budget and writes over the rule that was supposed to close it. Wide
/// characters occupy their leading cell and blank the trailing one, the
/// way every terminal lays them out.
///
/// Allocation-free by construction: cells are written in place, so a
/// per-frame `String` per pane never exists.
fn put_clipped(buf: &mut Buffer, x: u16, y: u16, text: &str, budget: usize, style: Style) -> u16 {
    if budget == 0 {
        return x;
    }
    let total: usize = text.chars().filter_map(UnicodeWidthChar::width).sum();
    let fits = total <= budget;
    // When it does not fit, the last cell is spent on the ellipsis.
    let text_budget = if fits { budget } else { budget - 1 };

    let mut cur = x;
    let mut used = 0usize;
    let mut sym = [0u8; 4];
    for ch in text.chars() {
        let Some(w) = UnicodeWidthChar::width(ch) else {
            continue;
        };
        if w == 0 || used + w > text_budget {
            break;
        }
        cur = put(buf, cur, y, ch.encode_utf8(&mut sym), style);
        // A wide glyph owns its trailing cell; blank it so the rule does
        // not show through the right half of the character.
        for _ in 1..w {
            cur = put(buf, cur, y, " ", style);
        }
        used += w;
    }
    if !fits {
        cur = put(buf, cur, y, ELLIPSIS.encode_utf8(&mut sym), style);
    }
    cur
}

/// Emit non-skip cells in `buf` as positioned VT paints.
///
/// Iteration is row-major. Consecutive cells in a row that share a style
/// are emitted as ONE `CUP` plus one SGR plus their symbols, so a rail
/// spanning 160 columns costs one escape sequence rather than 160 — the
/// styling added by phux-l96p.8 must not multiply the frame's byte cost.
/// A gap (a skip cell, or a blank) closes the run.
///
/// Skip is the only content filter — cells with an empty symbol but
/// skip=false are also skipped so we don't paint stray spaces over
/// future overlay layers.
fn emit_buffer<W: Write>(out: &mut W, buf: &Buffer) -> io::Result<()> {
    out.write_all(b"\x1b[0m")?;
    let area = buf.area;
    let mut style: Option<Style> = None;
    for y in area.y..area.y.saturating_add(area.height) {
        // `run` is the column the current run of same-styled cells would
        // continue at; `None` means no run is open.
        let mut run: Option<u16> = None;
        for x in area.x..area.x.saturating_add(area.width) {
            // `(x, y)` is in-bounds by construction (we iterate `area`),
            // but `cell` is `Option`-returning; treat any miss as a
            // silent skip rather than panic — keeps the chrome layer
            // resilient if a future ratatui upgrade changes bounds
            // semantics.
            let Some(cell) = buf.cell((x, y)) else {
                run = None;
                continue;
            };
            if cell.diff_option == CellDiffOption::Skip || cell.symbol().is_empty() {
                run = None;
                continue;
            }
            let cell_style = cell.style();
            if style != Some(cell_style) {
                write_style(out, cell_style)?;
                style = Some(cell_style);
                run = None;
            }
            if run != Some(x) {
                // CUP is 1-based.
                write!(out, "\x1b[{};{}H", y.saturating_add(1), x.saturating_add(1))?;
            }
            out.write_all(cell.symbol().as_bytes())?;
            run = Some(x.saturating_add(1));
        }
    }
    // Trailing reset so the next layer (status bar, focused pane render)
    // doesn't inherit any chrome SGR.
    out.write_all(b"\x1b[0m")?;
    out.flush()
}

/// Emit the SGR for one chrome cell: a full reset, then bold, then the
/// foreground. The reset is what makes a run boundary cheap to reason
/// about — no attribute can survive from the previous run.
fn write_style<W: Write>(out: &mut W, style: Style) -> io::Result<()> {
    out.write_all(b"\x1b[0m")?;
    if style.add_modifier.contains(Modifier::BOLD) {
        out.write_all(b"\x1b[1m")?;
    }
    match style.fg {
        None | Some(Color::Reset) => Ok(()),
        Some(color) => crate::render::sgr::write_sgr_color(out, color, true),
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::attach::multi_pane::{DividerCell, compute_layout, compute_layout_in};
    use crate::layout::{LayoutNode, LayoutState, SplitDir, split_at};

    fn t(id: u32) -> TerminalId {
        TerminalId::local(id)
    }

    fn leaf(id: u32) -> LayoutNode {
        LayoutNode::Leaf(t(id))
    }

    /// The whole viewport, with no rail row reserved. Used by the tests
    /// that predate the rail and only care about the interior grid.
    fn full(cols: u16, rows: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            w: cols,
            h: rows,
        }
    }

    /// A pane area with one rail row above it, the shipped shape.
    fn railed(cols: u16, rows: u16) -> Rect {
        Rect {
            x: 0,
            y: 1,
            w: cols,
            h: rows - 1,
        }
    }

    fn theme() -> Theme {
        Theme::default()
    }

    /// No labels at all — the pre-title behaviour.
    fn unlabelled(_: &TerminalId) -> Option<PaneLabel<'static>> {
        None
    }

    /// Render to a string with a fixed label on every pane.
    fn render(layout: &PaneLayout, content: Rect, label: Option<&'static str>) -> String {
        render_with_focus(layout, content, Some(&t(1)), label)
    }

    /// Render with an explicit focused pane.
    fn render_with_focus(
        layout: &PaneLayout,
        content: Rect,
        focus: Option<&TerminalId>,
        label: Option<&'static str>,
    ) -> String {
        let mut bytes: Vec<u8> = Vec::new();
        render_dividers(&mut bytes, layout, content, focus, &theme(), |_| {
            label.map(|text| PaneLabel {
                text,
                agent: None,
                attention: false,
                seen: true,
            })
        })
        .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    /// Empty layout with no rail: no bytes emitted.
    #[test]
    fn empty_layout_is_noop() {
        let layout = PaneLayout {
            viewport: (80, 24),
            rects: HashMap::new(),
            dividers: Vec::new(),
            divider_hits: Vec::new(),
        };
        assert!(render(&layout, full(80, 24), None).is_empty());
    }

    /// Zero-axis viewport: no-op.
    #[test]
    fn zero_viewport_is_noop() {
        let layout = PaneLayout {
            viewport: (0, 24),
            rects: HashMap::new(),
            dividers: vec![DividerCell {
                x: 0,
                y: 0,
                ch: '\u{2502}',
            }],
            divider_hits: Vec::new(),
        };
        assert!(render(&layout, full(0, 24), None).is_empty());
    }

    /// Two-pane horizontal split: every emitted byte targets a chrome
    /// cell; no CUP lands inside either pane's interior.
    /// THIS IS THE LOAD-BEARING SKIP-CELL INVARIANT TEST.
    #[test]
    fn skip_cells_never_emit_vt() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        assert!(
            !layout.dividers.is_empty(),
            "test precondition: have dividers"
        );

        let s = render(&layout, content, Some("shell"));
        let pane_rects: Vec<Rect> = layout.rects.values().copied().collect();
        let cups = extract_cups(&s);
        assert!(!cups.is_empty(), "expected at least one CUP");
        for (row_1b, col_1b) in cups {
            // Convert to 0-based outer-viewport coords.
            let y = row_1b.saturating_sub(1);
            let x = col_1b.saturating_sub(1);
            for r in &pane_rects {
                assert!(
                    !rect_contains(*r, x, y),
                    "CUP at ({x}, {y}) landed inside pane interior rect {r:?} — \
                     skip-cell invariant violated"
                );
            }
        }
    }

    /// A CUP only opens a run; the cells that FOLLOW it in the same run
    /// carry no CUP of their own, so this walks the decoded cell stream
    /// rather than the CUP list. Every painted cell must be chrome.
    #[test]
    fn no_painted_cell_lands_inside_a_pane() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let s = render(&layout, content, Some("shell"));
        for (x, y, sym) in painted_cells(&s) {
            for r in layout.rects.values() {
                assert!(
                    !rect_contains(*r, x, y),
                    "painted {sym:?} at ({x}, {y}) inside pane rect {r:?}"
                );
            }
        }
    }

    /// Three-pane cross split: same skip invariant holds across T-piece
    /// junctions and inner dividers.
    #[test]
    fn skip_cells_invariant_holds_for_cross_split() {
        let content = railed(80, 24);
        let layout = cross_layout(content, t(2));
        let s = render(&layout, content, Some("shell"));
        let pane_rects: Vec<Rect> = layout.rects.values().copied().collect();
        for (row_1b, col_1b) in extract_cups(&s) {
            let y = row_1b.saturating_sub(1);
            let x = col_1b.saturating_sub(1);
            for r in &pane_rects {
                assert!(
                    !rect_contains(*r, x, y),
                    "CUP at ({x}, {y}) landed inside pane interior rect {r:?}"
                );
            }
        }
    }

    /// Emitted bytes start with an SGR reset and end with one.
    #[test]
    fn emits_leading_and_trailing_sgr_reset() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let s = render(&layout, content, None);
        assert!(s.starts_with("\x1b[0m"), "expected leading SGR reset");
        assert!(s.ends_with("\x1b[0m"), "expected trailing SGR reset");
    }

    /// phux-l96p.8: focus is a COLOUR, not a stroke weight. The grid is
    /// uniformly light and the focused pane's own rules are tinted with
    /// `divider_focus` + bold. (This replaces the old
    /// `heavy_glyph_present_when_focus_adjacent`: heavy glyphs forced
    /// mixed-weight junctions that most terminal fonts cannot draw.)
    #[test]
    fn focus_is_painted_not_thickened() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let s = render(&layout, content, None);
        assert!(
            !s.contains('\u{2503}') && !s.contains('\u{2501}'),
            "the grid must be uniformly light"
        );
        assert!(s.contains('\u{2502}'), "expected the light │");
        assert!(
            s.contains(&sgr_fg(theme().divider_focus)),
            "expected the focus tint in {s:?}"
        );
        assert!(
            s.contains("\x1b[1m"),
            "expected the focused rule to be bold"
        );
    }

    /// An unfocused rule recedes to `theme.divider` and is not bold.
    #[test]
    fn an_unfocused_rule_uses_the_recessive_tone() {
        let content = railed(80, 24);
        // Focus sits in pane 1, so the divider between 2 and 3 is off
        // the focused frame.
        let tree = split_at(&leaf(1), &t(1), &t(2), SplitDir::Horizontal, 0.5).unwrap();
        let tree = split_at(&tree, &t(2), &t(3), SplitDir::Horizontal, 0.5).unwrap();
        let state = LayoutState {
            tree: Some(tree),
            focus: Some(t(1)),
        };
        let layout = compute_layout_in(&state, content, (80, 24));
        let s = render(&layout, content, None);
        assert!(
            s.contains(&sgr_fg(theme().divider)),
            "expected the recessive rule tone in {s:?}"
        );
    }

    /// phux-l96p.8 regression: emphasis follows the CLIENT's focused
    /// pane, not the layout tree's remembered focus. The two diverge
    /// routinely — a focus move updates the client immediately and the
    /// tree only when the layout is next persisted — and while the
    /// rasterizer owned emphasis the accent sat on the pane you had just
    /// left.
    #[test]
    fn emphasis_follows_the_passed_focus_not_the_layout_tree() {
        let content = railed(80, 24);
        // The tree remembers pane 1; the client is on pane 2.
        let tree = split_at(&leaf(1), &t(1), &t(2), SplitDir::Horizontal, 0.5).unwrap();
        let state = LayoutState {
            tree: Some(tree),
            focus: Some(t(1)),
        };
        let layout = compute_layout_in(&state, content, (80, 24));
        let one = layout.rects.get(&t(1)).copied().expect("pane 1");
        let two = layout.rects.get(&t(2)).copied().expect("pane 2");
        let rail_y = rail_row(content).expect("a rail");

        let focus_two = styled_cells(&render_with_focus(&layout, content, Some(&t(2)), None));
        // Pane 2's own top rail is accented; pane 1's is not.
        assert!(
            focus_two[&(two.x + 2, rail_y)].1,
            "the focused pane's rail must be accented"
        );
        assert!(
            !focus_two[&(one.x + 2, rail_y)].1,
            "an unfocused pane's rail must recede"
        );

        // Flip the client's focus and the emphasis flips with it, even
        // though the layout tree never changed.
        let focus_one = styled_cells(&render_with_focus(&layout, content, Some(&t(1)), None));
        assert!(focus_one[&(one.x + 2, rail_y)].1);
        assert!(!focus_one[&(two.x + 2, rail_y)].1);
    }

    /// The rail closes the grid across the top of the pane area and
    /// turns into a `┬` wherever a vertical rule drops out of it.
    #[test]
    fn the_rail_tees_where_a_vertical_rule_meets_it() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let cells: HashMap<(u16, u16), String> = painted_cells(&render(&layout, content, None))
            .into_iter()
            .map(|(x, y, sym)| ((x, y), sym))
            .collect();
        let rail_y = rail_row(content).expect("a rail row");
        assert_eq!(rail_y, 0);
        // The divider column, read off the layout itself.
        let col = layout.dividers.first().expect("a divider").x;
        assert_eq!(cells.get(&(col, rail_y)).map(String::as_str), Some("┬"));
        // Its neighbours are plain rule.
        assert_eq!(cells.get(&(0, rail_y)).map(String::as_str), Some("─"));
        assert_eq!(cells.get(&(79, rail_y)).map(String::as_str), Some("─"));
    }

    /// With no row reserved above the pane area there is no rail — the
    /// composer must not invent one over pane content.
    #[test]
    fn no_reserved_row_means_no_rail() {
        let content = full(80, 24);
        assert_eq!(rail_row(content), None);
        let layout = split_layout(content);
        for (_, y, _) in painted_cells(&render(&layout, content, None)) {
            assert!(y > 0 || layout.dividers.iter().any(|d| d.y == 0));
        }
    }

    /// A pane's title is inset one cell into the rule above it, wrapped
    /// in single spaces so the rule reads as broken FOR the label.
    #[test]
    fn a_title_is_inset_into_the_rule_above_its_pane() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let s = render(&layout, content, Some("editor"));
        assert!(s.contains("editor"), "expected the label in {s:?}");
        let cells: HashMap<(u16, u16), String> = painted_cells(&s)
            .into_iter()
            .map(|(x, y, sym)| ((x, y), sym))
            .collect();
        let rail_y = rail_row(content).unwrap();
        // Pane at x = 0: rule cell, space, then the label.
        assert_eq!(cells.get(&(0, rail_y)).map(String::as_str), Some("─"));
        assert_eq!(cells.get(&(1, rail_y)).map(String::as_str), Some(" "));
        assert_eq!(cells.get(&(2, rail_y)).map(String::as_str), Some("e"));
        assert_eq!(cells.get(&(8, rail_y)).map(String::as_str), Some(" "));
    }

    /// The focused pane's title rides `pane_title_focus`; an unfocused
    /// one recedes. Both are drawn from the same one focus fact the
    /// rules use, so a title can never disagree with its own frame.
    #[test]
    fn the_focused_title_is_accented() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let s = render(&layout, content, Some("editor"));
        assert!(s.contains(&sgr_fg(theme().pane_title_focus)));
        assert!(s.contains(&sgr_fg(theme().pane_title)));
    }

    /// A title too long for its pane is cut with the shared ellipsis and
    /// never runs past the rule that closes it.
    #[test]
    fn a_long_title_is_clipped_to_the_pane() {
        let content = railed(24, 6);
        let state = LayoutState {
            tree: Some(leaf(1)),
            focus: Some(t(1)),
        };
        let layout = compute_layout_in(&state, content, (24, 6));
        let s = render(&layout, content, Some("a-very-long-pane-title-indeed"));
        assert!(s.contains(ELLIPSIS), "expected an ellipsis in {s:?}");
        let rail_y = rail_row(content).unwrap();
        let width: u16 = painted_cells(&s)
            .into_iter()
            .filter(|(_, y, _)| *y == rail_y)
            .map(|(x, _, _)| x)
            .max()
            .unwrap();
        assert!(width < 24, "the rail overran the viewport: {width}");
    }

    /// Display cells, not chars. A wide (CJK) title must be measured by
    /// the cells it occupies or it overwrites the rule closing it.
    #[test]
    fn a_wide_title_is_clipped_by_display_width() {
        let content = railed(24, 6);
        let state = LayoutState {
            tree: Some(leaf(1)),
            focus: Some(t(1)),
        };
        let layout = compute_layout_in(&state, content, (24, 6));
        let s = render(&layout, content, Some("編集器編集器編集器編集器編集器"));
        let rail_y = rail_row(content).unwrap();
        let max_x: u16 = painted_cells(&s)
            .into_iter()
            .filter(|(_, y, _)| *y == rail_y)
            .map(|(x, _, _)| x)
            .max()
            .unwrap();
        assert!(
            max_x < 24,
            "a double-width title escaped its pane (max_x = {max_x})"
        );
    }

    /// A pane whose program never set a title gets no label — phux does
    /// not invent a name for a pane it did not name.
    #[test]
    fn an_empty_title_draws_nothing() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let with = render(&layout, content, Some(""));
        let without = render(&layout, content, None);
        assert_eq!(with, without);
    }

    /// A pane that has asked for a human badges on its own title, in the
    /// same tone the sidebar's row uses.
    #[test]
    fn an_asking_pane_badges_on_its_title() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let mut bytes: Vec<u8> = Vec::new();
        render_dividers(&mut bytes, &layout, content, Some(&t(1)), &theme(), |_| {
            Some(PaneLabel {
                text: "claude",
                agent: None,
                attention: true,
                seen: true,
            })
        })
        .unwrap();
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains('●'), "expected the attention badge in {s:?}");
        assert!(s.contains(&sgr_fg(theme().attention)));
    }

    /// The badge vocabulary is shared with the sidebar: a `working` pane
    /// draws the same glyph in both places or the two surfaces are
    /// describing different machines.
    #[test]
    fn the_pane_badge_matches_the_sidebar_vocabulary() {
        let th = theme();
        for state in [
            AgentMetaState::Working,
            AgentMetaState::Blocked,
            AgentMetaState::Done,
            AgentMetaState::Idle,
        ] {
            let label = PaneLabel {
                text: "claude",
                agent: Some(state),
                attention: false,
                seen: true,
            };
            let badge = label.badge(&th).expect("an agent pane badges");
            assert_eq!(badge, agent_badge(&th, state, false, true));
        }
    }

    /// A run of same-styled cells costs ONE CUP, not one per cell: the
    /// rail is 80 cells wide and must not cost 80 escape sequences.
    #[test]
    fn a_styled_run_emits_one_cup() {
        let content = railed(80, 6);
        let state = LayoutState {
            tree: Some(leaf(1)),
            focus: Some(t(1)),
        };
        let layout = compute_layout_in(&state, content, (80, 6));
        let s = render(&layout, content, None);
        let cups = extract_cups(&s);
        assert_eq!(
            cups.len(),
            1,
            "an unbroken 80-cell rail should cost one CUP, got {}",
            cups.len()
        );
        assert!(
            s.len() < 80 * 4,
            "per-cell escapes crept back in: {} bytes",
            s.len()
        );
    }

    /// Buffer-level introspection: every cell inside a pane Rect has
    /// `skip = true` after compose; every divider cell has `skip =
    /// false` and the right symbol. This is the structural twin of
    /// `skip_cells_never_emit_vt` — if compose breaks, this catches it
    /// before emit even runs.
    #[test]
    fn compose_buffer_marks_pane_interiors_skip() {
        let content = railed(80, 24);
        let layout = split_layout(content);
        let buf = compose_buffer(&layout, content, Some(&t(1)), &theme(), unlabelled);
        for r in layout.rects.values() {
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    let cell = buf.cell((x, y)).expect("in-bounds");
                    assert!(
                        cell.diff_option == CellDiffOption::Skip,
                        "pane interior cell ({x}, {y}) in {r:?} not marked skip"
                    );
                }
            }
        }
        for d in &layout.dividers {
            let cell = buf.cell((d.x, d.y)).expect("in-bounds");
            assert!(
                cell.diff_option != CellDiffOption::Skip,
                "divider cell ({}, {}) marked skip",
                d.x,
                d.y
            );
            assert_eq!(
                cell.symbol().chars().next(),
                Some(d.ch),
                "divider cell at ({}, {}) symbol mismatch",
                d.x,
                d.y
            );
        }
    }

    /// phux-i0e8.10.3: the help table advertises drag-to-resize, so the
    /// drag targets must actually exist — a split layout yields the
    /// `divider_hits` the dispatcher's ADR-0048 grab resolves against
    /// (the drag behavior itself is held by `input_dispatch`'s ADR-0048
    /// tests). If dividers stop producing hit targets, the advertised
    /// gesture would be dead and this fails.
    #[test]
    fn help_table_targets_exist() {
        assert!(
            HELP_BINDINGS.iter().any(|b| b.chord.contains("drag")),
            "the table must advertise the drag gesture"
        );
        let layout = split_layout(railed(80, 24));
        assert!(
            !layout.divider_hits.is_empty(),
            "a split layout must yield divider grab targets"
        );
    }

    // -- helpers ---------------------------------------------------------------

    fn split_layout(content: Rect) -> PaneLayout {
        let tree = split_at(&leaf(1), &t(1), &t(2), SplitDir::Horizontal, 0.5).unwrap();
        let state = LayoutState {
            tree: Some(tree),
            focus: Some(t(1)),
        };
        compute_layout_in(&state, content, (content.w, content.y + content.h))
    }

    fn cross_layout(content: Rect, focus: TerminalId) -> PaneLayout {
        let t1 = split_at(&leaf(1), &t(1), &t(2), SplitDir::Horizontal, 0.5).unwrap();
        let t2 = split_at(&t1, &t(1), &t(3), SplitDir::Vertical, 0.5).unwrap();
        let state = LayoutState {
            tree: Some(t2),
            focus: Some(focus),
        };
        compute_layout_in(&state, content, (content.w, content.y + content.h))
    }

    /// `compute_layout` is still the whole-viewport convenience wrapper.
    #[test]
    fn compute_layout_covers_the_whole_viewport() {
        let tree = split_at(&leaf(1), &t(1), &t(2), SplitDir::Horizontal, 0.5).unwrap();
        let state = LayoutState {
            tree: Some(tree),
            focus: Some(t(1)),
        };
        let layout = compute_layout(&state, (80, 24));
        assert_eq!(layout.viewport, (80, 24));
    }

    /// Half-open rect-contains, matching `multi_pane::rect_contains`.
    fn rect_contains(r: Rect, x: u16, y: u16) -> bool {
        x >= r.x && y >= r.y && x < r.x.saturating_add(r.w) && y < r.y.saturating_add(r.h)
    }

    /// Map each painted cell to `(symbol, accented)`, where `accented`
    /// means the cell carried the focus style (bold + `divider_focus`).
    fn styled_cells(s: &str) -> HashMap<(u16, u16), (String, bool)> {
        let focus_sgr = format!("\x1b[1m{}", sgr_fg(theme().divider_focus));
        let mut out = HashMap::new();
        let (mut x, mut y) = (0u16, 0u16);
        let mut accented = false;
        let mut rest = s;
        while let Some(i) = rest.find('\x1b') {
            for c in rest[..i].chars() {
                out.insert((x, y), (c.to_string(), accented));
                x = x.saturating_add(1);
            }
            let tail = &rest[i..];
            if tail.starts_with(&focus_sgr) {
                accented = true;
                rest = &tail[focus_sgr.len()..];
                continue;
            }
            let end = tail[1..]
                .find(|c: char| c.is_ascii_alphabetic())
                .map_or(tail.len(), |j| j + 2);
            let seq = &tail[..end];
            if seq.ends_with('H')
                && let Some((rr, cc)) = seq[2..seq.len() - 1].split_once(';')
                && let (Ok(rn), Ok(cn)) = (rr.parse::<u16>(), cc.parse::<u16>())
            {
                y = rn.saturating_sub(1);
                x = cn.saturating_sub(1);
            } else if seq.ends_with('m') {
                accented = false;
            }
            rest = &tail[end..];
        }
        for c in rest.chars() {
            out.insert((x, y), (c.to_string(), accented));
            x = x.saturating_add(1);
        }
        out
    }

    /// The foreground SGR this layer emits for `color`.
    fn sgr_fg(color: Color) -> String {
        let mut out: Vec<u8> = Vec::new();
        crate::render::sgr::write_sgr_color(&mut out, color, true).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Decode the emitted stream into `(x, y, symbol)` per painted cell,
    /// tracking the cursor across a run so cells written without their
    /// own CUP are still located.
    fn painted_cells(s: &str) -> Vec<(u16, u16, String)> {
        let mut out = Vec::new();
        let (mut x, mut y) = (0u16, 0u16);
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Consume `[`, then the parameter bytes, then the final.
                let mut body = String::new();
                if chars.peek() == Some(&'[') {
                    chars.next();
                }
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        if c == 'H'
                            && let Some((r, col)) = body.split_once(';')
                            && let (Ok(rn), Ok(cn)) = (r.parse::<u16>(), col.parse::<u16>())
                        {
                            y = rn.saturating_sub(1);
                            x = cn.saturating_sub(1);
                        }
                        break;
                    }
                    body.push(c);
                }
                continue;
            }
            out.push((x, y, c.to_string()));
            x = x.saturating_add(1);
        }
        out
    }

    /// Extract every CUP target `(row_1b, col_1b)` from a VT byte stream.
    fn extract_cups(s: &str) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i + 1 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                // Find the terminator letter.
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && !bytes[j].is_ascii_alphabetic() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'H' {
                    let body = std::str::from_utf8(&bytes[start..j]).unwrap_or("");
                    if let Some((r, c)) = body.split_once(';')
                        && let (Ok(rn), Ok(cn)) = (r.parse::<u16>(), c.parse::<u16>())
                    {
                        out.push((rn, cn));
                    }
                }
                i = j.saturating_add(1);
            } else {
                i += 1;
            }
        }
        out
    }
}
