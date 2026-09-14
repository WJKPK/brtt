use super::*;

fn levels(filters: &[Filter]) -> Vec<(&str, Level)> {
    filters
        .iter()
        .map(|filter| (&*filter.module, filter.level))
        .collect()
}

#[test]
fn filter_uses_longest_module_prefix() {
    let filters = parse_filter_spec("warn,app=info,app::net=debug").unwrap();
    assert_eq!(
        filter_level(Some("app::net::tcp"), &filters),
        defmt_parser::Level::Debug
    );
    assert_eq!(
        filter_level(Some("app::ui"), &filters),
        defmt_parser::Level::Info
    );
    assert_eq!(
        filter_level(Some("other"), &filters),
        defmt_parser::Level::Warn
    );
}

#[test]
fn level_enabled_uses_an_inclusive_minimum() {
    assert!(!level_enabled(Level::Debug, Level::Info));
    assert!(level_enabled(Level::Info, Level::Info));
    assert!(level_enabled(Level::Error, Level::Warn));
}

#[test]
fn filter_defaults_to_trace_without_a_matching_rule() {
    let filters = parse_filter_spec("app=warn").unwrap();

    assert_eq!(filter_level(Some("other"), &filters), Level::Trace);
    assert_eq!(filter_level(None, &filters), Level::Trace);
}

#[test]
fn filter_parser_accepts_aliases_and_rejects_invalid_specs() {
    assert_eq!(
        levels(&parse_filter_spec("warning,app=DEBUG").unwrap()),
        vec![("", Level::Warn), ("app", Level::Debug)]
    );
    assert!(parse_filter_spec("").is_err());
    assert!(parse_filter_spec("app=unknown").is_err());
    assert!(parse_filter_spec("app=warn,app=info").is_ok());
}

#[test]
fn filter_parser_trims_whitespace_around_entries() {
    let filters = parse_filter_spec("warn, app=debug").unwrap();

    assert_eq!(
        levels(&filters),
        vec![("", Level::Warn), ("app", Level::Debug)]
    );
    assert!(parse_filter_spec("app net=debug").is_err());
}

#[test]
fn filter_matches_module_boundaries_only() {
    let filters = parse_filter_spec("app=info").unwrap();

    assert_eq!(filter_level(Some("app"), &filters), Level::Info);
    assert_eq!(filter_level(Some("app::net"), &filters), Level::Info);
    assert_eq!(filter_level(Some("application"), &filters), Level::Trace);
}
