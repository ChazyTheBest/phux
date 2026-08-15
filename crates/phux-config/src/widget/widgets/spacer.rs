//! `spacer` widget — elastic blank space that pushes its neighbours apart.
//!
//! Every other widget is content-sized: it renders what it has and the
//! composer decides whether the row can afford it. A spacer inverts that.
//! It has no content, contributes nothing to the row's natural width, and
//! is then handed the columns nothing else claimed — so
//! `left = ["windows", { kind = "spacer" }, "session-name"]` puts the tab
//! strip hard left and the session name hard against whatever comes next,
//! at every terminal width, without a second slot.
//!
//! The contract in one line: **a spacer is paid out of slack, and slack is
//! the first thing a narrow row loses.** On a row that is already full it
//! renders zero cells, which is what makes it safe to leave in a config you
//! also use on a phone — see [`crate::widget::StatusBar::render`] for the
//! exact arithmetic and the consequences for the center slot.

use std::collections::BTreeMap;

use crate::widget::{
    Cell, StatusWidget, WidgetCells, WidgetContext, WidgetError, WidgetKindSpec,
    reject_unknown_opts,
};

/// Widget kind, used in error messages.
const KIND: &str = "spacer";

/// Doc spec — the factory validates against this same const, so the
/// documented option surface is the enforced one (phux-i0e8.11.3).
pub(in crate::widget) const SPEC: WidgetKindSpec = WidgetKindSpec {
    kind: KIND,
    summary: "Elastic blank space. Takes no width of its own and then \
              absorbs an even share of whatever columns the row has left \
              over, so the widgets on either side of it are pushed apart. \
              Renders nothing on a row with no room to spare, which makes \
              it the first thing to yield on a narrow terminal rather than \
              something that has to be configured away. Style it (`style = \
              { bg = ... }`) to paint the gap rather than leave it blank.",
    options: &[],
};

/// `spacer` widget: zero natural width, elastic under the composer.
#[derive(Debug, Clone, Copy, Default)]
pub struct SpacerWidget;

impl StatusWidget for SpacerWidget {
    /// Zero cells. This is the *natural* width the composer fits the row
    /// against, and a spacer's whole point is to want nothing until the
    /// fitting is done.
    fn render(&self, _ctx: &WidgetContext<'_>) -> WidgetCells {
        WidgetCells { cells: Vec::new() }
    }

    /// Exactly `budget` blank cells.
    ///
    /// This is the elastic payout: the composer calls it with the share of
    /// the row's slack this spacer was allotted. It is also what the
    /// narrowing path calls with a budget of `0` (a zero-natural-width
    /// widget is charged nothing and given nothing), which is why a spacer
    /// vanishes rather than misbehaving on a crowded row.
    ///
    /// The cells are left unstyled, so a `style` table on the widget wins
    /// through the registry's decorator and a background colour fills the
    /// gap instead of merely reserving it.
    fn render_within(&self, _ctx: &WidgetContext<'_>, budget: usize) -> WidgetCells {
        WidgetCells {
            cells: vec![Cell::default(); budget],
        }
    }

    fn elastic(&self, _ctx: &WidgetContext<'_>) -> bool {
        true
    }

    // No `poll_interval`: blank space never needs a repaint of its own.
}

/// Factory: builds a [`SpacerWidget`].
///
/// Takes no options (per [`SPEC`]). A width-limited pad is deliberately not
/// offered: `min-cols` / `max-cols` already express "only on a wide enough
/// bar", and a fixed-width gap is a `text` widget full of spaces, which
/// says what it is.
///
/// # Errors
///
/// Returns [`WidgetError::InvalidOption`] on any option at all.
pub(in crate::widget) fn factory(
    opts: &BTreeMap<String, toml::Value>,
) -> Result<Box<dyn StatusWidget>, WidgetError> {
    reject_unknown_opts(&SPEC, opts)?;
    Ok(Box::new(SpacerWidget))
}

#[cfg(test)]
mod tests {
    use super::{SpacerWidget, factory};
    use crate::widget::{StatusWidget, WidgetContext, WidgetError};
    use std::collections::BTreeMap;

    fn opts(pairs: &[(&str, toml::Value)]) -> BTreeMap<String, toml::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn ctx() -> WidgetContext<'static> {
        WidgetContext {
            cols: 80,
            ..WidgetContext::new(std::time::UNIX_EPOCH, "", "C-a", &[])
        }
    }

    #[test]
    fn a_spacer_takes_no_natural_width() {
        assert!(SpacerWidget.render(&ctx()).cells.is_empty());
    }

    #[test]
    fn a_spacer_fills_exactly_the_budget_it_is_given() {
        let cells = SpacerWidget.render_within(&ctx(), 7).cells;
        assert_eq!(cells.len(), 7);
        assert!(
            cells.iter().all(|c| c.text.is_empty() || c.text[0] == ' '),
            "a spacer paints blanks, not glyphs",
        );
        assert!(
            cells.iter().all(|c| c.style.is_none()),
            "cells stay unstyled so a `style` table can fill the gap",
        );
    }

    #[test]
    fn a_zero_budget_renders_nothing() {
        assert!(SpacerWidget.render_within(&ctx(), 0).cells.is_empty());
    }

    #[test]
    fn a_spacer_declares_itself_elastic() {
        assert!(SpacerWidget.elastic(&ctx()));
    }

    #[test]
    fn the_factory_takes_no_options() {
        factory(&opts(&[])).expect("a bare spacer builds");
        let err = factory(&opts(&[("width", toml::Value::Integer(4))]))
            .expect_err("spacer has no options");
        let WidgetError::InvalidOption { kind, message } = err else {
            panic!("expected InvalidOption");
        };
        assert_eq!(kind, "spacer");
        assert!(message.contains("unknown option"), "got {message}");
    }
}
