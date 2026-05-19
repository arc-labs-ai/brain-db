//! `OutputFormatArg::Auto` resolves to `Table` on a TTY and `Ndjson` when
//! piped. The resolver is a pure function so we test without process
//! state.

use brain_shell::output::{resolve_auto, OutputFormatArg};

#[test]
fn auto_resolves_to_table_on_tty() {
    assert_eq!(
        resolve_auto(OutputFormatArg::Auto, true),
        OutputFormatArg::Table
    );
}

#[test]
fn auto_resolves_to_ndjson_when_piped() {
    assert_eq!(
        resolve_auto(OutputFormatArg::Auto, false),
        OutputFormatArg::Ndjson
    );
}

#[test]
fn non_auto_passes_through() {
    for fmt in [
        OutputFormatArg::Table,
        OutputFormatArg::Wide,
        OutputFormatArg::Json,
        OutputFormatArg::Ndjson,
        OutputFormatArg::Yaml,
        OutputFormatArg::JsonPath(".name".into()),
    ] {
        let resolved_tty = resolve_auto(fmt.clone(), true);
        let resolved_pipe = resolve_auto(fmt.clone(), false);
        assert_eq!(resolved_tty, fmt);
        assert_eq!(resolved_pipe, fmt);
    }
}

#[test]
fn jsonpath_payload_round_trips() {
    let f = OutputFormatArg::JsonPath(".items[0].id".into());
    let resolved = resolve_auto(f.clone(), true);
    assert_eq!(resolved, f);
    assert_eq!(f.short_name(), "jsonpath");
}
