use anyhow::{bail, Context, Result};
use brtt::rtt::ScanRegion;
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) enum ProbeInfo {
    Number(usize),
    List,
}

impl std::str::FromStr for ProbeInfo {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<ProbeInfo, &'static str> {
        if s == "list" {
            Ok(ProbeInfo::List)
        } else if let Ok(n) = s.parse::<usize>() {
            Ok(ProbeInfo::Number(n))
        } else {
            Err("Invalid probe number.")
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ChannelMode {
    Ascii,
    Defmt,
}

impl ChannelMode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ChannelMode::Ascii => "ascii",

            ChannelMode::Defmt => "defmt",
        }
    }
}

impl std::str::FromStr for ChannelMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ascii" => Ok(ChannelMode::Ascii),

            "defmt" => Ok(ChannelMode::Defmt),
            _ => Err(format!(
                "invalid channel mode '{value}', expected ascii or defmt"
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct ChannelSpec {
    pub(crate) index: u32,
    pub(crate) mode: ChannelMode,
}

impl std::str::FromStr for ChannelSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        let index = parts.next().unwrap_or_default();
        let mode = parts.next().unwrap_or("ascii");

        if parts.next().is_some() {
            return Err(format!(
                "invalid channel specification '{value}', expected INDEX[:MODE]"
            ));
        }

        if index.is_empty() {
            return Err("channel index cannot be empty".to_string());
        }

        let index = index
            .parse::<u32>()
            .map_err(|_| format!("invalid channel index '{index}', expected a u32"))?;
        let mode = mode.parse()?;

        Ok(ChannelSpec { index, mode })
    }
}

pub(crate) fn parse_scan_region(
    mut src: &str,
) -> Result<ScanRegion, Box<dyn std::error::Error + Send + Sync + 'static>> {
    src = src.trim();
    if src.is_empty() {
        return Ok(ScanRegion::Ram);
    }

    let parts = src
        .split("..")
        .map(|p| {
            if p.starts_with("0x") || p.starts_with("0X") {
                u64::from_str_radix(&p[2..], 16)
            } else {
                p.parse()
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    match *parts.as_slice() {
        [addr] => Ok(ScanRegion::Exact(addr)),
        [start, end] => Ok(ScanRegion::range(start..end)),
        _ => Err("Invalid range: multiple '..'s".into()),
    }
}

#[derive(Debug, clap::Parser)]
#[clap(
    name = "brtt",
    about = "Better RTT (Real-Time Transfer) client",
    version = clap::crate_version!(),
)]
pub(crate) struct Opts {
    #[clap(
        short,
        long,
        default_value = "0",
        help = "Specify probe number or 'list' to list probes."
    )]
    pub(crate) probe: ProbeInfo,

    #[clap(
        short,
        long,
        help = "Target chip type. Leave unspecified to auto-detect."
    )]
    pub(crate) chip: Option<String>,

    #[clap(short, long, help = "List RTT channels and exit.")]
    pub(crate) list: bool,

    #[clap(
        short,
        long,
        action = clap::ArgAction::Append,
        value_name = "CHANNEL[:MODE]",
        help = "Up channel specification. MODE is ascii or defmt; defaults to ascii. May be repeated."
    )]
    pub(crate) up: Vec<ChannelSpec>,

    #[clap(
        short,
        long,
        action = clap::ArgAction::Append,
        value_name = "CHANNEL[:MODE]",
        help = "Down channel specification. Only one channel is supported; defaults to channel 0."
    )]
    pub(crate) down: Vec<ChannelSpec>,

    #[clap(short, long, help = "Reset the target after RTT session was opened")]
    pub(crate) reset: bool,

    #[clap(
        long,
        default_value = "10",
        value_name = "MILLISECONDS",
        help = "Polling interval for RTT and keyboard input."
    )]
    pub(crate) poll_interval: u64,

    #[clap(
        long,
        default_value = "",
        value_parser = parse_scan_region,
        help = "Memory region to scan for control block. You can specify either an exact starting address '0x1000' or a range such as '0x0000..0x1000'. Both decimal and hex are accepted."
    )]
    pub(crate) scan_region: ScanRegion,

    #[clap(long, value_name = "PATH", help = "ELF containing the defmt table.")]
    pub(crate) elf: Option<PathBuf>,

    #[clap(long, help = "Print the loaded defmt table and exit.")]
    pub(crate) debug_defmt_table: bool,

    #[clap(
        long,
        value_name = "SPEC",
        help = "Filter defmt output, e.g. warn or app=debug,warn."
    )]
    pub(crate) defmt_filter: Option<String>,

    #[clap(long, value_enum, default_value_t = ColorMode::Auto, help = "Defmt level color mode.")]
    pub(crate) color: ColorMode,

    #[clap(
        short = 'L',
        long,
        value_name = "PATH",
        help = "Write session output to a log file."
    )]
    pub(crate) log: Option<PathBuf>,

    #[clap(long, help = "Write one log file per up channel.")]
    pub(crate) log_per_channel: bool,

    #[clap(
        long,
        value_enum,
        help = "Log raw bytes or decoded text. Defaults to decoded."
    )]
    pub(crate) log_format: Option<LogFormat>,

    #[clap(long, help = "Disable the default down channel and keyboard input.")]
    pub(crate) no_down: bool,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormat {
    Raw,
    Decoded,
}

impl Opts {
    pub(crate) fn validate(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        if self.poll_interval == 0 {
            bail!("--poll-interval must be greater than zero");
        }

        let mut channels = HashSet::new();
        for spec in up_specs {
            if !channels.insert(spec.index) {
                bail!("up channel {} was specified more than once", spec.index);
            }
        }

        if self.down.len() > 1 {
            bail!("multiple down channels are not supported; specify only one");
        }
        if let Some(spec) = self.down.first() {
            if spec.mode != ChannelMode::Ascii {
                bail!("down channel {} only supports the ascii mode", spec.index);
            }
        }
        if self.no_down && !self.down.is_empty() {
            bail!("--no-down cannot be combined with --down");
        }

        let has_defmt = up_specs.iter().any(|spec| spec.mode == ChannelMode::Defmt);
        if self.defmt_filter.is_some() && !has_defmt {
            bail!("--defmt-filter requires at least one up channel using :defmt");
        }
        if self.elf.is_some() && !has_defmt && !self.debug_defmt_table {
            bail!("--elf requires an up channel using :defmt or --debug-defmt-table");
        }
        if has_defmt && self.elf.is_none() {
            bail!("--elf is required when using an up channel with :defmt");
        }
        if self.debug_defmt_table && self.elf.is_none() {
            bail!("--debug-defmt-table requires --elf");
        }

        if self.log.is_none() {
            if self.log_per_channel {
                bail!("--log-per-channel requires --log");
            }
            if self.log_format.is_some() {
                bail!("--log-format requires --log");
            }
        } else if self.log_format == Some(LogFormat::Raw)
            && !self.log_per_channel
            && up_specs.len() > 1
        {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }

        let session_options = || {
            !self.up.is_empty()
                || !self.down.is_empty()
                || self.no_down
                || self.reset
                || self.log.is_some()
                || self.log_per_channel
                || self.log_format.is_some()
                || self.defmt_filter.is_some()
                || self.elf.is_some()
                || self.poll_interval != 10
                || !matches!(&self.scan_region, ScanRegion::Ram)
                || self.chip.is_some()
        };
        if self.debug_defmt_table {
            if self.list || matches!(self.probe, ProbeInfo::List) {
                bail!("--debug-defmt-table cannot be combined with --list or --probe list");
            }
            if session_options()
                && (!self.up.is_empty()
                    || !self.down.is_empty()
                    || self.no_down
                    || self.reset
                    || self.log.is_some()
                    || self.log_per_channel
                    || self.log_format.is_some()
                    || self.defmt_filter.is_some()
                    || self.poll_interval != 10
                    || !matches!(&self.scan_region, ScanRegion::Ram)
                    || self.chip.is_some()
                    || self.color != ColorMode::Auto)
            {
                bail!("--debug-defmt-table cannot be combined with session options");
            }
        }
        if self.list {
            if matches!(self.probe, ProbeInfo::List) {
                bail!("--list cannot be combined with --probe list");
            }
            if !self.up.is_empty()
                || !self.down.is_empty()
                || self.no_down
                || self.reset
                || self.log.is_some()
                || self.log_per_channel
                || self.log_format.is_some()
                || self.defmt_filter.is_some()
                || self.elf.is_some()
                || self.poll_interval != 10
                || !matches!(&self.scan_region, ScanRegion::Ram)
                || self.chip.is_some()
                || self.color != ColorMode::Auto
            {
                bail!("--list cannot be combined with session options");
            }
        }
        if matches!(self.probe, ProbeInfo::List)
            && (self.list
                || !self.up.is_empty()
                || !self.down.is_empty()
                || self.no_down
                || self.reset
                || self.log.is_some()
                || self.log_per_channel
                || self.log_format.is_some()
                || self.defmt_filter.is_some()
                || self.elf.is_some()
                || self.poll_interval != 10
                || !matches!(&self.scan_region, ScanRegion::Ram)
                || self.chip.is_some()
                || self.color != ColorMode::Auto)
        {
            bail!("--probe list cannot be combined with session options");
        }

        if let Some(filter) = self.defmt_filter.as_deref() {
            let parsed = crate_filter_spec(filter)?;
            let mut prefixes = HashSet::new();
            for (prefix, _) in parsed {
                if !prefixes.insert(prefix.clone()) {
                    bail!(
                        "defmt filter prefix '{}' was specified more than once",
                        prefix
                    );
                }
            }
        }
        Ok(())
    }
}

fn crate_filter_spec(spec: &str) -> Result<Vec<(String, defmt_parser::Level)>> {
    let mut result = Vec::new();
    for item in spec.split(',').filter(|item| !item.trim().is_empty()) {
        let (module, level) = item.split_once('=').unwrap_or(("", item));
        if module.contains(char::is_whitespace) {
            bail!(
                "invalid defmt filter module '{}': whitespace is not allowed",
                module
            );
        }
        let level = match level.trim().to_ascii_lowercase().as_str() {
            "trace" => defmt_parser::Level::Trace,
            "debug" => defmt_parser::Level::Debug,
            "info" => defmt_parser::Level::Info,
            "warn" | "warning" => defmt_parser::Level::Warn,
            "error" => defmt_parser::Level::Error,
            value => bail!(
                "invalid defmt level '{}', expected trace, debug, info, warn, or error",
                value
            ),
        };
        result.push((module.to_string(), level));
    }
    if result.is_empty() {
        bail!("defmt filter cannot be empty");
    }
    Ok(result)
}

pub(crate) fn selected_channel(specs: &[ChannelSpec], direction: &str) -> Result<usize> {
    let spec = match specs {
        [] => return Ok(0),
        [spec] => spec,
        _ => bail!(
            "Multiple {direction} channels are not supported yet; use only one specification."
        ),
    };

    usize::try_from(spec.index).with_context(|| {
        format!(
            "{direction} channel index {} cannot be represented on this host",
            spec.index
        )
    })
}

pub(crate) fn configured_up_specs(specs: &[ChannelSpec]) -> Vec<ChannelSpec> {
    if specs.is_empty() {
        vec![ChannelSpec {
            index: 0,
            mode: ChannelMode::Ascii,
        }]
    } else {
        specs.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn channel_spec_defaults_to_raw() {
        assert_eq!(
            "7".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 7,
                mode: ChannelMode::Ascii,
            })
        );
    }

    #[test]
    fn channel_spec_parses_all_modes() {
        assert_eq!(
            "1:ascii".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 1,
                mode: ChannelMode::Ascii,
            })
        );
        assert_eq!(
            "2:ascii".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 2,
                mode: ChannelMode::Ascii,
            })
        );
        assert_eq!(
            "3:defmt".parse::<ChannelSpec>(),
            Ok(ChannelSpec {
                index: 3,
                mode: ChannelMode::Defmt,
            })
        );
    }

    #[test]
    fn channel_spec_rejects_invalid_values() {
        for value in ["", ":ascii", "1:", "1:ascii:x", "-1", "not-a-channel"] {
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
                mode: ChannelMode::Ascii,
            })
        );
    }

    #[test]
    fn opts_accept_repeated_channel_specs_in_order() {
        let opts = Opts::try_parse_from([
            "brtt", "-u", "3:ascii", "--up", "4", "-d", "1:defmt", "--down", "2:ascii",
        ])
        .unwrap();

        assert_eq!(
            opts.up,
            vec![
                ChannelSpec {
                    index: 3,
                    mode: ChannelMode::Ascii,
                },
                ChannelSpec {
                    index: 4,
                    mode: ChannelMode::Ascii,
                },
            ]
        );
        assert_eq!(
            opts.down,
            vec![
                ChannelSpec {
                    index: 1,
                    mode: ChannelMode::Defmt,
                },
                ChannelSpec {
                    index: 2,
                    mode: ChannelMode::Ascii,
                },
            ]
        );
    }

    #[test]
    fn opts_leave_channels_empty_when_unspecified() {
        let opts = Opts::try_parse_from(["brtt"]).unwrap();

        assert!(opts.up.is_empty());
        assert!(opts.down.is_empty());
    }

    fn validate_args(args: &[&str]) -> std::result::Result<(), String> {
        let opts = Opts::try_parse_from(args).map_err(|error| error.to_string())?;
        let specs = configured_up_specs(&opts.up);
        opts.validate(&specs).map_err(|error| error.to_string())
    }

    #[test]
    fn validation_rejects_unsupported_channel_combinations() {
        assert!(validate_args(&["brtt", "--up", "0", "--up", "0"]).is_err());
        assert!(validate_args(&["brtt", "--down", "0:defmt"]).is_err());
        assert!(validate_args(&["brtt", "--poll-interval", "0"]).is_err());
        assert!(validate_args(&["brtt", "--up", "1:defmt"]).is_err());
        assert!(validate_args(&["brtt", "--defmt-filter", "warn"]).is_err());
        assert!(validate_args(&["brtt", "--elf", "firmware.elf"]).is_err());
    }

    #[test]
    fn validation_rejects_log_modifiers_without_a_log() {
        assert!(validate_args(&["brtt", "--log-per-channel"]).is_err());
        assert!(validate_args(&["brtt", "--log-format", "raw"]).is_err());
    }

    #[test]
    fn validation_rejects_conflicting_exit_modes() {
        assert!(validate_args(&["brtt", "--list", "--up", "0"]).is_err());
        assert!(validate_args(&["brtt", "--probe", "list", "--reset"]).is_err());
        assert!(validate_args(&["brtt", "--debug-defmt-table"]).is_err());
        assert!(validate_args(&[
            "brtt",
            "--debug-defmt-table",
            "--elf",
            "firmware.elf",
            "--list"
        ])
        .is_err());
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
                mode: ChannelMode::Ascii,
            }]
        );
    }

    #[test]
    fn configured_up_specs_preserve_channel_order_and_modes() {
        let specs = vec![
            ChannelSpec {
                index: 2,
                mode: ChannelMode::Ascii,
            },
            ChannelSpec {
                index: 5,
                mode: ChannelMode::Ascii,
            },
        ];

        assert_eq!(configured_up_specs(&specs), specs);
    }

    #[test]
    fn selected_channel_preserves_default_and_rejects_multiple_channels() {
        assert_eq!(selected_channel(&[], "up").unwrap(), 0);
        assert_eq!(
            selected_channel(
                &[ChannelSpec {
                    index: 4,
                    mode: ChannelMode::Ascii,
                }],
                "up",
            )
            .unwrap(),
            4
        );
        assert!(selected_channel(
            &[
                ChannelSpec {
                    index: 1,
                    mode: ChannelMode::Ascii,
                },
                ChannelSpec {
                    index: 2,
                    mode: ChannelMode::Ascii,
                },
            ],
            "down",
        )
        .is_err());
    }
}
