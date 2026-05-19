//! Common cell builders for [`comfy_table`].
//!
//! Centralising these means a renderer never decides how a confidence value
//! or a memory id "looks" — it asks for the right cell and the theme +
//! formatting policy stays consistent across every table in the project.

use comfy_table::Cell;

use crate::term::TermPolicy;
use crate::theme::{Theme, Token};

/// Format a confidence value (0.0–1.0) as `0.91` and paint it with
/// [`Token::Confidence`]. NaN renders as `n/a` in the muted token to
/// avoid emitting `NaN` to scriptable output.
#[must_use]
pub fn confidence_cell(theme: &Theme, policy: TermPolicy, score: f32) -> Cell {
    if score.is_nan() {
        let text = theme.paint(Token::Muted, "n/a", policy);
        return Cell::new(text);
    }
    let text = format!("{score:.2}");
    Cell::new(theme.paint(Token::Confidence, &text, policy))
}

/// Format a similarity / fused score as `0.9134` (one more digit than a
/// confidence — scores are usually compared against neighbours) and paint
/// it with [`Token::Score`].
#[must_use]
pub fn score_cell(theme: &Theme, policy: TermPolicy, score: f32) -> Cell {
    if score.is_nan() {
        let text = theme.paint(Token::Muted, "n/a", policy);
        return Cell::new(text);
    }
    let text = format!("{score:.4}");
    Cell::new(theme.paint(Token::Score, &text, policy))
}

/// Paint an already-formatted short id string under [`Token::MemoryId`].
///
/// The caller produces the short form (e.g. `s7/m42/v3`) — this helper
/// only adds the theming so the column stays visually distinct.
#[must_use]
pub fn short_id_cell(theme: &Theme, policy: TermPolicy, id: &str) -> Cell {
    Cell::new(theme.paint(Token::MemoryId, id, policy))
}

/// Paint an entity id (any surface form) under [`Token::EntityId`].
#[must_use]
pub fn entity_id_cell(theme: &Theme, policy: TermPolicy, id: &str) -> Cell {
    Cell::new(theme.paint(Token::EntityId, id, policy))
}

/// Paint a relation predicate under [`Token::Predicate`].
#[must_use]
pub fn predicate_cell(theme: &Theme, policy: TermPolicy, predicate: &str) -> Cell {
    Cell::new(theme.paint(Token::Predicate, predicate, policy))
}

/// Build the two cells (label + value) for a property-card row.
///
/// Returned as a `[Cell; 2]` rather than wrapped in a `Row` so callers can
/// pass the cells through [`comfy_table::Table::add_row`] alongside other
/// rows of different shapes.
#[must_use]
pub fn kv_row(theme: &Theme, policy: TermPolicy, label: &str, value: &str) -> [Cell; 2] {
    [
        Cell::new(theme.paint(Token::Label, label, policy)),
        Cell::new(theme.paint(Token::Value, value, policy)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_cell_formats_two_digits() {
        let cell = confidence_cell(&Theme::default(), TermPolicy::plain(), 0.9123);
        assert_eq!(cell.content(), "0.91");
    }

    #[test]
    fn confidence_cell_handles_nan() {
        let cell = confidence_cell(&Theme::default(), TermPolicy::plain(), f32::NAN);
        assert_eq!(cell.content(), "n/a");
    }

    #[test]
    fn score_cell_formats_four_digits() {
        let cell = score_cell(&Theme::default(), TermPolicy::plain(), 0.91237);
        assert_eq!(cell.content(), "0.9124");
    }

    #[test]
    fn kv_row_yields_two_cells() {
        let row = kv_row(&Theme::default(), TermPolicy::plain(), "key", "value");
        assert_eq!(row[0].content(), "key");
        assert_eq!(row[1].content(), "value");
    }

    #[test]
    fn short_id_cell_preserves_text_under_no_color() {
        let cell = short_id_cell(&Theme::default(), TermPolicy::plain(), "s7/m42/v3");
        assert_eq!(cell.content(), "s7/m42/v3");
    }
}
