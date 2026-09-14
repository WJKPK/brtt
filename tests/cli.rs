use super::*;
use clap::Parser;

#[test]
fn channel_spec_defaults_to_terminal() {
    assert_eq!(
        "7".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            index: 7,
            mode: ChannelEncoding::Terminal,
        })
    );
}

#[test]
fn channel_spec_parses_terminal_and_defmt_modes() {
    assert_eq!(
        "1:terminal".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            index: 1,
            mode: ChannelEncoding::Terminal,
        })
    );
    assert_eq!(
        "2:defmt".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            index: 2,
            mode: ChannelEncoding::Defmt,
        })
    );
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
fn channel_spec_accepts_u32_max() {
    assert_eq!(
        "4294967295".parse::<ChannelSpec>(),
        Ok(ChannelSpec {
            index: u32::MAX,
            mode: ChannelEncoding::Terminal,
        })
    );
}

#[test]
fn opts_accept_repeated_channel_specs_in_order() {
    let opts = Opts::try_parse_from(["brtt", "-u", "3:terminal", "--up", "4", "-d", "2"]).unwrap();

    assert_eq!(
        opts.up,
        vec![
            ChannelSpec {
                index: 3,
                mode: ChannelEncoding::Terminal,
            },
            ChannelSpec {
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
fn opts_accept_startup_timestamps() {
    let opts = Opts::try_parse_from(["brtt", "--timestamp"]).unwrap();

    assert!(opts.timestamps);
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
    assert_error_contains(&["brtt", "--debug-defmt-table"], "--elf <PATH>");
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
fn configured_up_specs_default_to_channel_zero() {
    assert_eq!(
        configured_up_specs(&[]),
        vec![ChannelSpec {
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    );
}

#[test]
fn configured_up_specs_preserve_channel_order_and_modes() {
    let specs = vec![
        ChannelSpec {
            index: 2,
            mode: ChannelEncoding::Terminal,
        },
        ChannelSpec {
            index: 5,
            mode: ChannelEncoding::Defmt,
        },
    ];

    assert_eq!(configured_up_specs(&specs), specs);
}
