use super::*;
use clap::Parser;

fn resolve_args(args: &[&str]) -> std::result::Result<ResolvedOpts, String> {
    let opts = Opts::try_parse_from(args).map_err(|error| error.to_string())?;
    opts.resolve().map_err(|error| error.to_string())
}

fn assert_resolve_error(args: &[&str], expected: &str) {
    let error = resolve_args(args).expect_err("arguments unexpectedly accepted");
    assert!(
        error.contains(expected),
        "{error:?} does not contain {expected:?}"
    );
}

#[test]
fn channel_spec_parses_all_forms() {
    for (value, core, index, mode) in [
        ("1:terminal", None, 1, ChannelEncoding::Terminal),
        ("2:defmt", None, 2, ChannelEncoding::Defmt),
        ("4294967295", None, u32::MAX, ChannelEncoding::Terminal),
        ("1:0", Some(1), 0, ChannelEncoding::Terminal),
        ("0:0:terminal", Some(0), 0, ChannelEncoding::Terminal),
        ("1:0:defmt", Some(1), 0, ChannelEncoding::Defmt),
    ] {
        assert_eq!(
            value.parse::<ChannelSpec>(),
            Ok(ChannelSpec { core, index, mode }),
            "parsing {value:?}"
        );
    }
    for value in [
        "",
        ":terminal",
        "1:",
        "1:terminal:x",
        "1:0:defmt:x",
        "0::terminal",
        ":0:terminal",
        "a:0",
        "-1",
        "1:binary",
        "not-a-channel",
        "4294967296",
    ] {
        assert!(value.parse::<ChannelSpec>().is_err(), "accepted {value:?}");
    }
}

#[test]
fn elf_spec_parses_bare_and_indexed_paths() {
    assert_eq!(
        "firmware.elf".parse::<ElfSpec>(),
        Ok(ElfSpec {
            index: None,
            path: PathBuf::from("firmware.elf"),
        })
    );
    assert_eq!(
        "1=m4.elf".parse::<ElfSpec>(),
        Ok(ElfSpec {
            index: Some(1),
            path: PathBuf::from("m4.elf"),
        })
    );
    for value in ["", "0=", "=m7.elf", "firmware=v2.elf", "4294967296=m7.elf"] {
        assert!(value.parse::<ElfSpec>().is_err(), "accepted {value:?}");
    }
}

#[test]
fn resolve_assigns_bare_paths_around_explicit_indices() {
    for (args, expected) in [
        (
            vec!["brtt", "--elf", "1=m4.elf", "--elf", "m7.elf"],
            vec![(1, "m4.elf"), (0, "m7.elf")],
        ),
        (
            vec!["brtt", "--elf", "first.elf", "--elf", "0=second.elf"],
            vec![(1, "first.elf"), (0, "second.elf")],
        ),
        (
            vec![
                "brtt", "--elf", "a.elf", "--elf", "b.elf", "--elf", "1=c.elf",
            ],
            vec![(0, "a.elf"), (2, "b.elf"), (1, "c.elf")],
        ),
        (vec!["brtt", "--elf", "1=m4.elf"], vec![(1, "m4.elf")]),
    ] {
        let resolved = resolve_args(&args).unwrap();
        let expected = expected
            .into_iter()
            .map(|(index, path)| (index, PathBuf::from(path)))
            .collect::<Vec<_>>();
        assert_eq!(resolved.elf_specs(), &expected, "args {args:?}");
    }
    assert_resolve_error(
        &["brtt", "--elf", "0=a.elf", "--elf", "0=b.elf"],
        "specified more than once",
    );
}

#[test]
fn resolve_applies_channel_default() {
    let resolved = resolve_args(&["brtt", "--elf", "1=core.elf"]).unwrap();

    assert_eq!(resolved.elf_specs(), &[(1, PathBuf::from("core.elf"))],);
    assert_eq!(
        resolved.up_specs(),
        &[ChannelSpec {
            core: None,
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    );
}

#[test]
fn resolve_rejects_bad_channel_and_defmt_combinations() {
    assert_resolve_error(
        &["brtt", "--up", "0", "--up", "0"],
        "specified more than once",
    );
    assert_resolve_error(
        &["brtt", "--up", "0", "--up", "1:0"],
        "specified more than once",
    );
    assert_resolve_error(
        &["brtt", "--up", "0:0", "--up", "0:0:defmt"],
        "conflicting modes",
    );
    assert_resolve_error(&["brtt", "--poll-interval", "0"], "not in 1..");
    assert_resolve_error(&["brtt", "--up", "1:defmt"], "--elf is required");
    assert_resolve_error(
        &["brtt", "--defmt-filter", "warn"],
        "requires at least one up channel",
    );
    assert_resolve_error(&["brtt", "--log-per-channel"], "--log <PATH>");
    assert_resolve_error(&["brtt", "--log-format", "raw"], "--log <PATH>");
    assert_resolve_error(
        &["brtt", "--up", "1:0", "--elf", "0=a.elf"],
        "selects no configured core",
    );
}

#[test]
fn resolve_rejects_raw_merged_log_fanout() {
    assert!(
        resolve_args(&[
            "brtt",
            "--log",
            "out.log",
            "--log-format",
            "raw",
            "--up",
            "0"
        ])
        .is_ok(),
        "single-core raw merged log rejected"
    );
    assert_resolve_error(
        &[
            "brtt",
            "--elf",
            "0=a.elf",
            "--elf",
            "1=b.elf",
            "--log",
            "out.log",
            "--log-format",
            "raw",
            "--up",
            "0",
        ],
        "requires --log-per-channel",
    );
    assert!(
        resolve_args(&["brtt", "--log", "out.log", "--log-format", "raw"]).is_ok(),
        "single-source raw merged log rejected"
    );
}

#[test]
fn resolve_rejects_conflicting_exit_modes() {
    assert_resolve_error(
        &["brtt", "--list", "--up", "0"],
        "--list cannot be combined",
    );
    assert_resolve_error(
        &["brtt", "--probe", "list", "--reset"],
        "--probe list cannot be combined",
    );
    assert_resolve_error(&["brtt", "--debug-defmt-table"], "--elf <[INDEX=]PATH>");
    assert_resolve_error(
        &[
            "brtt",
            "--debug-defmt-table",
            "--elf",
            "firmware.elf",
            "--list",
        ],
        "cannot be combined",
    );
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
        assert_resolve_error(&full, "cannot be combined with session options");
    }
}

#[test]
fn resolve_accepts_supported_session_and_list_combinations() {
    assert!(resolve_args(&[
        "brtt", "--elf", "0=a.elf", "--elf", "1=b.elf", "--up", "0:0", "--up", "1:0"
    ])
    .is_ok());
    assert!(resolve_args(&[
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
    for args in [
        vec!["brtt", "--list", "--chip", "nRF54L15"],
        vec!["brtt", "--list", "--scan-region", "0x20002e68"],
        vec!["brtt", "--list", "--elf", "firmware.elf"],
    ] {
        assert!(resolve_args(&args).is_ok(), "rejected {args:?}");
    }
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
