use crate::channel::ChannelId;
use crate::defmt::{DefmtData, Filter, FilterSpec};
use anyhow::{bail, Result};
use brtt::rtt::{RttDiscovery, ScanRegion};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

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
pub(crate) enum ChannelEncoding {
    Terminal,
    Defmt,
}

impl ChannelEncoding {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ChannelEncoding::Terminal => "terminal",
            ChannelEncoding::Defmt => "defmt",
        }
    }
}

impl std::str::FromStr for ChannelEncoding {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "terminal" => Ok(ChannelEncoding::Terminal),
            "defmt" => Ok(ChannelEncoding::Defmt),
            _ => Err(format!(
                "invalid channel mode '{value}', expected terminal or defmt"
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct ChannelSpec {
    pub(crate) index: u32,
    pub(crate) mode: ChannelEncoding,
}

impl std::str::FromStr for ChannelSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split(':');
        let index = parts.next().unwrap_or_default();
        let mode = parts.next().unwrap_or("terminal");

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
        [start, end] if start < end => Ok(ScanRegion::range(start..end)),
        [start, end] => Err(format!(
            "invalid scan range '{src}': start {start:#x} must be less than end {end:#x}"
        )
        .into()),
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
        help = "Specify probe number or 'list' to list probes. Prompts when multiple probes are available."
    )]
    pub(crate) probe: Option<ProbeInfo>,

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
        help = "Up channel specification. MODE is terminal or defmt; defaults to terminal. May be repeated."
    )]
    pub(crate) up: Vec<ChannelSpec>,

    #[clap(
        short,
        long,
        conflicts_with = "no_down",
        value_name = "CHANNEL",
        help = "Down channel specification. Only one channel is supported; defaults to channel 0."
    )]
    pub(crate) down: Option<u32>,

    #[clap(short, long, help = "Reset the target after RTT session was opened")]
    pub(crate) reset: bool,

    #[clap(
        short = 't',
        long = "timestamp",
        help = "Enable local date and time timestamps with millisecond precision."
    )]
    pub(crate) timestamps: bool,

    #[clap(
        long,
        default_value = "10",
        value_parser = clap::value_parser!(u64).range(1..),
        value_name = "MILLISECONDS",
        help = "Polling interval for RTT and keyboard input."
    )]
    pub(crate) poll_interval: u64,

    #[clap(
        long,
        value_parser = parse_scan_region,
        help = "Memory region to scan for control block. You can specify either an exact starting address '0x1000' or a range such as '0x0000..0x1000'. Both decimal and hex are accepted."
    )]
    pub(crate) scan_region: Option<ScanRegion>,

    #[clap(
        long,
        value_name = "PATH",
        help = "ELF containing the RTT control block symbol and, optionally, a defmt table."
    )]
    pub(crate) elf: Option<PathBuf>,

    #[clap(
        long,
        requires = "elf",
        help = "Print the loaded defmt table and exit."
    )]
    pub(crate) debug_defmt_table: bool,

    #[clap(
        long = "defmt-filter",
        value_parser = crate::defmt::parse_filter_spec_value,
        value_name = "SPEC",
        help = "Filter defmt output, e.g. warn or app=debug,warn."
    )]
    pub(crate) defmt_filters: Option<FilterSpec>,

    #[clap(long, value_enum, default_value_t = ColorMode::Auto, help = "Terminal color mode for channel labels and defmt levels.")]
    pub(crate) color: ColorMode,

    #[clap(
        short = 'L',
        long,
        value_name = "PATH",
        help = "Write session output to a log file."
    )]
    pub(crate) log: Option<PathBuf>,

    #[clap(long, requires = "log", help = "Write one log file per up channel.")]
    pub(crate) log_per_channel: bool,

    #[clap(
        long,
        value_enum,
        requires = "log",
        help = "Log raw bytes or cleaned decoded text. Defaults to decoded."
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
    fn has_defmt_up_channel(up_specs: &[ChannelSpec]) -> bool {
        up_specs
            .iter()
            .any(|spec| spec.mode == ChannelEncoding::Defmt)
    }

    pub(crate) fn needs_defmt_data(&self, up_specs: &[ChannelSpec]) -> bool {
        self.debug_defmt_table || Self::has_defmt_up_channel(up_specs)
    }

    pub(crate) fn validate(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        self.validate_channels(up_specs)?;
        self.validate_defmt(up_specs)?;
        self.validate_logging(up_specs)?;
        self.validate_operation_modes()?;
        self.validate_filter()?;
        Ok(())
    }

    fn validate_channels(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        let mut channels = HashSet::new();
        for spec in up_specs {
            if !channels.insert(spec.index) {
                bail!("up channel {} was specified more than once", spec.index);
            }
        }
        Ok(())
    }

    fn validate_defmt(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        let has_defmt = Self::has_defmt_up_channel(up_specs);
        if self.defmt_filters.is_some() && !has_defmt {
            bail!("--defmt-filter requires at least one up channel using :defmt");
        }
        if has_defmt && self.elf.is_none() {
            bail!("--elf is required when using an up channel with :defmt");
        }
        if self.debug_defmt_table && self.elf.is_none() {
            bail!("--debug-defmt-table requires --elf");
        }
        Ok(())
    }

    fn validate_logging(&self, up_specs: &[ChannelSpec]) -> Result<()> {
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
        Ok(())
    }

    fn has_session_options(&self) -> bool {
        !self.up.is_empty()
            || self.down.is_some()
            || self.no_down
            || self.reset
            || self.timestamps
            || self.log.is_some()
            || self.log_per_channel
            || self.log_format.is_some()
            || self.defmt_filters.is_some()
            || self.poll_interval != 10
    }

    fn validate_operation_modes(&self) -> Result<()> {
        if self.debug_defmt_table {
            if self.list || matches!(self.probe, Some(ProbeInfo::List)) {
                bail!("--debug-defmt-table cannot be combined with --list or --probe list");
            }
            if self.has_session_options() || self.color != ColorMode::Auto {
                bail!("--debug-defmt-table cannot be combined with session options");
            }
        }
        if self.list {
            if matches!(self.probe, Some(ProbeInfo::List)) {
                bail!("--list cannot be combined with --probe list");
            }
            if self.has_session_options() || self.color != ColorMode::Auto {
                bail!("--list cannot be combined with session options");
            }
        }
        if matches!(self.probe, Some(ProbeInfo::List))
            && (self.list || self.has_session_options() || self.color != ColorMode::Auto)
        {
            bail!("--probe list cannot be combined with session options");
        }
        Ok(())
    }

    fn validate_filter(&self) -> Result<()> {
        if let Some(spec) = &self.defmt_filters {
            let mut prefixes = HashSet::new();
            for filter in &spec.0 {
                if !prefixes.insert(filter.module.clone()) {
                    bail!(
                        "defmt filter prefix '{}' was specified more than once",
                        filter.module
                    );
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn configured_up_specs(specs: &[ChannelSpec]) -> Vec<ChannelSpec> {
    if specs.is_empty() {
        vec![ChannelSpec {
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    } else {
        specs.to_vec()
    }
}

pub(crate) struct LogConfig {
    pub(crate) destination: LogDestination,
    pub(crate) format: LogFormat,
}

pub(crate) enum LogDestination {
    Merged(PathBuf),
    PerChannel(PathBuf),
}

/// Validated, normalized session configuration derived from [`Opts`].
///
/// All startup policy (default channel specs, log merging, scan discovery) is
/// resolved here so the session loop only performs I/O.
pub(crate) struct SessionConfig {
    pub(crate) probe: String,
    pub(crate) chip: String,
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) down_channel: Option<ChannelId>,
    pub(crate) poll_interval: Duration,
    pub(crate) reset: bool,
    pub(crate) timestamps: bool,
    pub(crate) defmt: Option<DefmtData>,
    pub(crate) defmt_filters: Option<Vec<Filter>>,
    pub(crate) color: ColorMode,
    pub(crate) log: Option<LogConfig>,
    pub(crate) discovery: RttDiscovery,
}

impl SessionConfig {
    pub(crate) fn from_opts(
        opts: Opts,
        probe: String,
        chip: String,
        defmt: Option<DefmtData>,
        discovery: RttDiscovery,
    ) -> Result<Self> {
        let down_channel = if opts.no_down {
            None
        } else {
            Some(ChannelId::from_cli(opts.down.unwrap_or(0), "down")?)
        };
        let log = opts.log.map(|path| LogConfig {
            destination: if opts.log_per_channel {
                LogDestination::PerChannel(path)
            } else {
                LogDestination::Merged(path)
            },
            format: opts.log_format.unwrap_or(LogFormat::Decoded),
        });

        Ok(Self {
            probe,
            chip,
            up_specs: configured_up_specs(&opts.up),
            down_channel,
            poll_interval: Duration::from_millis(opts.poll_interval),
            reset: opts.reset,
            timestamps: opts.timestamps,
            defmt,
            defmt_filters: opts.defmt_filters.map(|spec| spec.0),
            color: opts.color,
            log,
            discovery,
        })
    }
}

#[cfg(test)]
#[path = "../tests/cli.rs"]
mod tests;
