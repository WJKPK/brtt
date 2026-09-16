use super::*;
use clap::Parser;

#[test]
fn channel_spec_parses_terminal_and_defmt_modes() {
    assert_eq!(
        "1:terminal".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: None,
            index: 1,
            mode: ChannelEncoding::Terminal,
        })
    );
    assert_eq!(
        "2:defmt".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: None,
            index: 2,
            mode: ChannelEncoding::Defmt,
        })
    );
    assert_eq!(
        "4294967295".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: None,
            index: u32::MAX,
            mode: ChannelEncoding::Terminal,
        })
    );
}

#[test]
fn channel_spec_parses_per_core_forms() {
    assert_eq!(
        "1:0".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: Some(1),
            index: 0,
            mode: ChannelEncoding::Terminal,
        })
    );
    assert_eq!(
        "0:0:terminal".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: Some(0),
            index: 0,
            mode: ChannelEncoding::Terminal,
        })
    );
    assert_eq!(
        "1:0:defmt".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            core: Some(1),
            index: 0,
            mode: ChannelEncoding::Defmt,
        })
    );
}

#[test]
fn channel_spec_rejects_malformed_per_core_values() {
    for value in ["1:0:defmt:x", "a:0", "0::terminal", ":0:terminal"] {
        assert!(value.parse::<ChannelSpec>().is_err(), "accepted {value:?}");
    }
}

#[test]
fn channel_spec_rejects_invalid_values() {
    for value in ["", ":terminal", "1:", "1:terminal:x", "-1", "not-a-channel"] {
        assert!(value.parse::<ChannelSpec>().is_err(), "accepted {value:?}");
    }

    assert!("1:binary".parse::<ChannelSpec>().is_err());
    assert!("4294967296".parse::<ChannelSpec>().is_err());
}

#[test]
fn opts_accept_repeated_channel_specs_in_order() {
    let opts = Opts::try_parse_from(["brtt", "-u", "3:terminal", "--up", "4", "-d", "2"]).unwrap();

    assert_eq!(
        opts.up,
        vec![
            ChannelSpec {
                core: None,
                index: 3,
                mode: ChannelEncoding::Terminal,
            },
            ChannelSpec {
                core: None,
                index: 4,
                mode: ChannelEncoding::Terminal,
            },
        ]
    );
    assert_eq!(opts.down, Some(2));
}

#[test]
fn opts_leave_channels_empty_when_unspecified() {
    let opts = Opts::try_parse_from(["brtt"]).unwrap();

    assert!(opts.up.is_empty());
    assert!(opts.down.is_none());
    assert!(opts.scan_region.is_none());
    assert!(!opts.timestamps);
    assert!(opts.probe.is_none());
}

#[test]
fn opts_preserve_explicit_scan_region() {
    let opts = Opts::try_parse_from(["brtt", "--scan-region", "0x20000000"]).unwrap();

    assert!(matches!(
        opts.scan_region,
        Some(ScanRegion::Exact(0x20000000))
    ));
}

#[test]
fn scan_region_rejects_empty_and_reversed_ranges() {
    assert!(parse_scan_region("0x2000..0x2000").is_err());
    assert!(parse_scan_region("0x3000..0x2000").is_err());
}

#[test]
fn opts_preserve_explicit_probe_zero() {
    let opts = Opts::try_parse_from(["brtt", "--probe", "0"]).unwrap();

    assert_eq!(opts.probe, Some(ProbeInfo::Number(0)));
}

fn validate_args(args: &[&str]) -> std::result::Result<(), String> {
    let opts = Opts::try_parse_from(args).map_err(|error| error.to_string())?;
    let specs = configured_up_specs(&opts.up);
    opts.validate(&specs).map_err(|error| error.to_string())
}

fn assert_error_contains(args: &[&str], expected: &str) {
    let error = validate_args(args).expect_err("arguments unexpectedly accepted");
    assert!(
        error.contains(expected),
        "{error:?} does not contain {expected:?}"
    );
}

#[test]
fn validation_rejects_unsupported_channel_combinations() {
    assert_error_contains(
        &["brtt", "--up", "0", "--up", "0"],
        "specified more than once",
    );
    assert_error_contains(&["brtt", "--poll-interval", "0"], "not in 1..");
    assert_error_contains(&["brtt", "--up", "1:defmt"], "--elf is required");
    assert_error_contains(
        &["brtt", "--defmt-filter", "warn"],
        "requires at least one up channel",
    );
    assert!(validate_args(&["brtt", "--elf", "firmware.elf"]).is_ok());
}

#[test]
fn validation_rejects_log_modifiers_without_a_log() {
    assert_error_contains(&["brtt", "--log-per-channel"], "--log <PATH>");
    assert_error_contains(&["brtt", "--log-format", "raw"], "--log <PATH>");
}

#[test]
fn validation_rejects_conflicting_exit_modes() {
    assert_error_contains(
        &["brtt", "--list", "--up", "0"],
        "--list cannot be combined",
    );
    assert_error_contains(
        &["brtt", "--probe", "list", "--reset"],
        "--probe list cannot be combined",
    );
    assert_error_contains(&["brtt", "--debug-defmt-table"], "--elf <[INDEX=]PATH>");
    assert_error_contains(
        &[
            "brtt",
            "--debug-defmt-table",
            "--elf",
            "firmware.elf",
            "--list",
        ],
        "cannot be combined",
    );
}

#[test]
fn each_session_option_is_rejected_with_list() {
    // Every session-only flag must trip the mode guard on its own. Each case
    // below reaches validate_operation_modes: the log modifiers ride along
    // with --log and the defmt filter rides with a defmt channel, so they
    // are not rejected by an earlier rule instead. chip, scan region, ELF
    // and the probe selector stay allowed (see the next test).
    for args in [
        vec!["--up", "0"],
        vec!["--down", "1"],
        vec!["--no-down"],
        vec!["--reset"],
        vec!["--timestamp"],
        vec!["--poll-interval", "5"],
        vec!["--log", "capture.log"],
        vec!["--log", "capture.log", "--log-per-channel"],
        vec!["--log", "capture.log", "--log-format", "raw"],
        vec![
            "--up",
            "1:defmt",
            "--elf",
            "firmware.elf",
            "--defmt-filter",
            "warn",
        ],
        vec!["--color", "always"],
    ] {
        let mut full = vec!["brtt", "--list"];
        full.extend(args);
        assert_error_contains(&full, "cannot be combined with session options");
    }
}

#[test]
fn list_accepts_target_discovery_options() {
    assert!(validate_args(&["brtt", "--list", "--chip", "nRF54L15"]).is_ok());
    assert!(validate_args(&["brtt", "--list", "--scan-region", "0x20002e68"]).is_ok());
    assert!(validate_args(&["brtt", "--list", "--elf", "firmware.elf"]).is_ok());
}

#[test]
fn validation_accepts_supported_defmt_and_logging_options() {
    assert!(validate_args(&[
        "brtt",
        "--up",
        "1:defmt",
        "--elf",
        "firmware.elf",
        "--defmt-filter",
        "warn",
        "--log",
        "capture.log",
        "--log-format",
        "decoded"
    ])
    .is_ok());
}

#[test]
fn elf_spec_accepts_bare_and_indexed_paths() {
    assert_eq!(
        "firmware.elf".parse::<ElfSpec>(),
        Ok(ElfSpec {
            index: None,
            path: PathBuf::from("firmware.elf"),
        })
    );
    assert_eq!(
        "0=m7.elf".parse::<ElfSpec>(),
        Ok(ElfSpec {
            index: Some(0),
            path: PathBuf::from("m7.elf"),
        })
    );
    assert_eq!(
        "1=m4.elf".parse::<ElfSpec>(),
        Ok(ElfSpec {
            index: Some(1),
            path: PathBuf::from("m4.elf"),
        })
    );
}

#[test]
fn elf_spec_rejects_malformed_values() {
    for value in ["", "0=", "=m7.elf", "firmware=v2.elf", "4294967296=m7.elf"] {
        assert!(value.parse::<ElfSpec>().is_err(), "accepted {value:?}");
    }
}

#[test]
fn resolve_elf_specs_assigns_bare_paths_to_free_slots() {
    let specs = ["1=m4.elf", "m7.elf"]
        .iter()
        .map(|value| value.parse::<ElfSpec>().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        resolve_elf_specs(&specs).unwrap(),
        vec![(1, PathBuf::from("m4.elf")), (0, PathBuf::from("m7.elf")),]
    );
}

#[test]
fn resolve_elf_specs_rejects_duplicate_indices() {
    let specs = ["0=a.elf", "0=b.elf"]
        .iter()
        .map(|value| value.parse::<ElfSpec>().unwrap())
        .collect::<Vec<_>>();

    let error = resolve_elf_specs(&specs).expect_err("duplicate index accepted");
    assert!(error.to_string().contains("specified more than once"));
}

#[test]
fn resolve_elf_specs_allows_sparse_indices() {
    let specs = ["1=m4.elf".parse::<ElfSpec>().unwrap()];

    assert_eq!(
        resolve_elf_specs(&specs).unwrap(),
        vec![(1, PathBuf::from("m4.elf"))]
    );
}

#[test]
fn channel_spec_applies_to_selected_cores() {
    let bare = "0".parse::<ChannelSpec>().unwrap();
    assert!(bare.applies_to(0));
    assert!(bare.applies_to(1));

    let pinned = "1:0".parse::<ChannelSpec>().unwrap();
    assert!(!pinned.applies_to(0));
    assert!(pinned.applies_to(1));
}

#[test]
fn validate_up_coverage_accepts_reachable_selections() {
    let specs = ["0", "1:0"]
        .iter()
        .map(|value| value.parse::<ChannelSpec>().unwrap())
        .collect::<Vec<_>>();

    assert!(validate_up_coverage(&specs, &[0, 1]).is_ok());
    assert!(validate_up_coverage(&specs, &[0]).is_err());
}

#[test]
fn validate_up_coverage_rejects_dangling_selections() {
    let specs = ["1:0".parse::<ChannelSpec>().unwrap()];

    let error = validate_up_coverage(&specs, &[0]).expect_err("dangling selection accepted");
    assert!(error.to_string().contains("selects no configured core"));
}

#[test]
fn resolve_elf_specs_reserves_explicit_indices_before_bare_paths() {
    // Bare-first order resolves identically to explicit-first: the bare path
    // fills the lowest index not claimed by any explicit mapping.
    let specs = ["first.elf", "0=second.elf"]
        .iter()
        .map(|value| value.parse::<ElfSpec>().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        resolve_elf_specs(&specs).unwrap(),
        vec![
            (1, PathBuf::from("first.elf")),
            (0, PathBuf::from("second.elf")),
        ]
    );
}

#[test]
fn resolve_elf_specs_rejects_bare_path_colliding_with_later_explicit() {
    // Two bare paths plus an explicit claim on the second bare slot: the
    // second bare path must skip the reserved index instead of colliding.
    let specs = ["a.elf", "b.elf", "1=c.elf"]
        .iter()
        .map(|value| value.parse::<ElfSpec>().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        resolve_elf_specs(&specs).unwrap(),
        vec![
            (0, PathBuf::from("a.elf")),
            (2, PathBuf::from("b.elf")),
            (1, PathBuf::from("c.elf")),
        ]
    );
}

#[test]
fn validate_up_coverage_names_the_dangling_core() {
    let specs = ["3:0".parse::<ChannelSpec>().unwrap()];

    let error = validate_up_coverage(&specs, &[0]).expect_err("dangling selection accepted");
    assert!(error.to_string().contains("3:0"), "{error}");
}

#[test]
fn expand_sources_counts_bare_specs_once_per_core() {
    let specs = ["0".parse::<ChannelSpec>().unwrap()];

    let sources = expand_sources(&specs, &[0, 1]);

    assert_eq!(sources.len(), 2);
    assert!(sources.iter().any(|source| source.core == 0));
    assert!(sources.iter().any(|source| source.core == 1));
}

#[test]
fn validate_expanded_rejects_raw_merged_log_over_two_cores() {
    use clap::Parser;
    let opts = Opts::try_parse_from([
        "brtt",
        "--log",
        "out.log",
        "--log-format",
        "raw",
        "--up",
        "0",
    ])
    .unwrap();
    let up_specs = configured_up_specs(&opts.up);

    let error = opts
        .validate_expanded(&up_specs, &[0, 1])
        .expect_err("raw merged log over two cores accepted");
    assert!(error.to_string().contains("requires --log-per-channel"));
}

#[test]
fn validate_expanded_accepts_raw_merged_log_for_one_source() {
    use clap::Parser;
    let opts = Opts::try_parse_from(["brtt", "--log", "out.log", "--log-format", "raw"]).unwrap();
    let up_specs = configured_up_specs(&opts.up);

    assert!(opts.validate_expanded(&up_specs, &[0]).is_ok());
}

#[test]
fn configured_up_specs_default_to_channel_zero() {
    assert_eq!(
        configured_up_specs(&[]),
        vec![ChannelSpec {
            core: None,
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    );
}

#[test]
fn validation_accepts_disjoint_per_core_channels() {
    assert!(validate_args(&["brtt", "--up", "0:0", "--up", "1:0"]).is_ok());
}

#[test]
fn validation_rejects_overlapping_up_specs() {
    assert_error_contains(
        &["brtt", "--up", "0", "--up", "1:0"],
        "specified more than once",
    );
    assert_error_contains(
        &["brtt", "--up", "0:0", "--up", "0:0:defmt"],
        "conflicting modes",
    );
}

#[test]
fn mode_picks_the_single_requested_action() {
    let mode = |args: &[&str]| Opts::try_parse_from(args).unwrap().mode();

    assert_eq!(mode(&["brtt"]), Mode::Session);
    assert_eq!(mode(&["brtt", "--list"]), Mode::ListChannels);
    assert_eq!(mode(&["brtt", "--probe", "list"]), Mode::ListProbes);
    assert_eq!(
        mode(&["brtt", "--debug-defmt-table", "--elf", "firmware.elf"]),
        Mode::DebugDefmtTable
    );
}
