//! Project-standard [`comfy_table::Table`] constructor.
//!
//! Every renderer that emits a table goes through here so column wrapping,
//! borders, and width clamping stay uniform. The width clamp respects the
//! detected terminal width but floors at 40 cols — narrower than that and
//! the table degrades to vertical noise; better to let the terminal scroll
//! horizontally on truly tiny windows.

use comfy_table::{ContentArrangement, Table};

use crate::term::TermPolicy;

/// Build the standard comfy-table base: horizontal-only border preset,
/// dynamic content arrangement, terminal-width clamped to the policy.
///
/// Why these choices:
/// - `UTF8_HORIZONTAL_ONLY` — vertical borders eat width on narrow terminals
///   and add visual noise on wide ones; horizontal separators carry enough
///   structure.
/// - `ContentArrangement::Dynamic` — comfy-table picks column widths from the
///   content + the table width budget, so callers don't hand-tune.
#[must_use]
pub fn build_table(policy: TermPolicy) -> Table {
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(u16::try_from(policy.width.max(40)).unwrap_or(u16::MAX));
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_table_honors_policy_width() {
        let mut policy = TermPolicy::plain();
        policy.width = 120;
        let table = build_table(policy);
        // comfy_table::Table::width() returns Option<u16>.
        assert_eq!(table.width(), Some(120));
    }

    #[test]
    fn build_table_floors_at_40() {
        let mut policy = TermPolicy::plain();
        policy.width = 10;
        let table = build_table(policy);
        assert_eq!(table.width(), Some(40));
    }

    #[test]
    fn build_table_caps_oversized_width_at_u16_max() {
        let mut policy = TermPolicy::plain();
        policy.width = usize::MAX;
        let table = build_table(policy);
        assert_eq!(table.width(), Some(u16::MAX));
    }
}
