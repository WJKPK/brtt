use crate::channel::{expand_sources, ChannelEncoding, ChannelId, ChannelSpec};
use crate::elf::{resolve_elf_specs, ElfSpec};
use crate::probe::{Chip, ProbeSelection, TargetRequest};
use crate::rtt::ScanRegion;
use crate::session::{ColorMode, LogConfig, LogDestination, LogFormat, SessionPolicy};
use anyhow::{bail, Result};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq, Clone)]
enum ProbeInfo {
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

impl std::str::FromStr for ChannelSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // Format is `[CORE:]CHANNEL[:MODE]`. Without `CORE:`, the channel applies
        // to all configured cores. A two-part spec is `CHANNEL:MODE` when the second
        // segment is a known mode (`terminal` or `defmt`), and `CORE:CHANNEL` otherwise.
        let parts: Vec<&str> = value.split(':').collect();
        let (core, index, mode) = match parts.as_slice() {
            [index] => (None, *index, "terminal"),
            [first, second] if ["terminal", "defmt"].contains(second) => (None, *first, *second),
            [core, index] => (Some(*core), *index, "terminal"),
            [core, index, mode] => (Some(*core), *index, *mode),
            _ => {
                return Err(format!(
                    "invalid channel specification '{value}', expected [CORE:]CHANNEL[:MODE]"
                ));
            }
        };

        if index.is_empty() {
            return Err("channel index cannot be empty".to_string());
        }
        let index = index
            .parse::<u32>()
            .map_err(|_| format!("invalid channel index '{index}', expected a u32"))?;

        let core = match core {
            None => None,
            Some("") => {
                return Err("core index cannot be empty".to_string());
            }
            Some(core) => Some(
                core.parse::<u32>()
                    .map_err(|_| format!("invalid core index '{core}', expected a u32"))?,
            ),
        };
        let mode = mode.parse()?;

        Ok(ChannelSpec { core, index, mode })
    }
}

impl std::str::FromStr for ElfSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if let Some((index, path)) = value.split_once('=') {
            if index.is_empty() {
                return Err(format!("invalid ELF index in '{value}', expected a u32"));
            }
            let index = index
                .parse::<u32>()
                .map_err(|_| format!("invalid ELF index '{index}', expected a u32"))?;
            if path.is_empty() {
                return Err(format!(
                    "invalid ELF specification '{value}', expected [INDEX=]PATH"
                ));
            }
            return Ok(ElfSpec {
                index: Some(index),
                path: PathBuf::from(path),
            });
        }
        if value.is_empty() {
            return Err(format!(
                "invalid ELF specification '{value}', expected [INDEX=]PATH"
            ));
        }
        Ok(ElfSpec {
            index: None,
            path: PathBuf::from(value),
        })
    }
}

fn parse_scan_region(
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

const DEFAULT_POLL_INTERVAL_MS: u64 = 10;

#[derive(Debug, clap::Parser)]
#[clap(
    name = "brtt",
    about = "Better RTT (Real-Time Transfer) client",
    version = clap::crate_version!(),
    after_help = concat!(
        "Behavior:\n",
        "  Output: --up can repeat, terminal and defmt can mix. Shared output is tagged [chN],\n",
        "    or [cN:chM] when several cores stream. Cross-core order is poll order, never\n",
        "    temporal; host timestamps correlate instead.\n",
        "  Terminal model: shell output repaints lines. Display and decoded log share one decode,\n",
        "    so they agree. --log-format raw stores exact RTT bytes instead.\n",
        "  Defmt frames: messages carry level and timestamp; needs --elf.\n",
        "  Session: tio-like Ctrl-T commands; host-side timestamps. Data to stdout, diagnostics to\n",
        "    stderr. Logs have no ANSI escapes. On restart brtt reattaches and finishes partial lines.",
    ),
)]
pub(crate) struct Opts {
    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "Specify probe number or 'list' to list probes. Prompts when multiple probes are available."
    )]
    probe: Option<ProbeInfo>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "Target chip type. Leave unspecified to auto-detect."
    )]
    chip: Option<Chip>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "List RTT channels and exit."
    )]
    list: bool,

    #[clap(
        short,
        long,
        help_heading = "Channels",
        action = clap::ArgAction::Append,
        value_name = "[CORE:]CHANNEL[:MODE]",
        help = "Up channel specification, as CHANNEL, CHANNEL:MODE or CORE:CHANNEL[:MODE]. MODE is terminal or defmt; defaults to terminal. Without CORE: the channel is selected on every configured core. May be repeated."
    )]
    up: Vec<ChannelSpec>,

    #[clap(
        short,
        long,
        help_heading = "Channels",
        conflicts_with = "no_down",
        value_name = "CHANNEL",
        help = "Down channel specification. Only one channel is supported; defaults to channel 0. Applies to every configured core; keyboard input routes to the lowest core exposing it."
    )]
    down: Option<u32>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "Reset the target before opening the RTT session"
    )]
    reset: bool,

    #[clap(
        short = 't',
        long = "timestamp",
        help_heading = "Display",
        help = "Enable local date and time timestamps with millisecond precision."
    )]
    timestamps: bool,

    #[clap(
        long,
        help_heading = "Display",
        default_value_t = DEFAULT_POLL_INTERVAL_MS,
        value_parser = clap::value_parser!(u64).range(1..),
        value_name = "MILLISECONDS",
        help = "Polling interval for RTT and keyboard input."
    )]
    poll_interval: u64,

    #[clap(
        long,
        help_heading = "Target",
        value_parser = parse_scan_region,
        help = "Memory region to scan for control block. You can specify either an exact starting address '0x1000' or a range such as '0x0000..0x1000'. Both decimal and hex are accepted."
    )]
    scan_region: Option<ScanRegion>,

    #[clap(
        long,
        help_heading = "Defmt",
        action = clap::ArgAction::Append,
        value_name = "[INDEX=]PATH",
        help = "ELF file for a target core, as PATH or INDEX=PATH. A bare PATH targets the lowest free core index from 0; INDEX selects the core explicitly and may be sparse. May be repeated to attach several cores."
    )]
    elf: Vec<ElfSpec>,

    #[clap(
        long,
        help_heading = "Defmt",
        requires = "elf",
        help = "Print the loaded defmt table and exit."
    )]
    debug_defmt_table: bool,

    #[clap(long, value_enum, default_value_t = ColorMode::Auto, help_heading = "Display", help = "Terminal color mode for channel labels and defmt levels.")]
    color: ColorMode,

    #[clap(
        short = 'L',
        long,
        help_heading = "Logging",
        value_name = "PATH",
        help = "Write session output to a log file."
    )]
    log: Option<PathBuf>,

    #[clap(
        long,
        requires = "log",
        help_heading = "Logging",
        help = "Write one log file per up channel."
    )]
    log_per_channel: bool,

    #[clap(
        long,
        value_enum,
        help_heading = "Logging",
        requires = "log",
        help = "Log raw bytes or cleaned decoded text. Defaults to decoded."
    )]
    log_format: Option<LogFormat>,

    #[clap(
        long,
        help_heading = "Channels",
        help = "Disable the default down channel and keyboard input."
    )]
    no_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    DebugDefmtTable,
    ListProbes,
    ListChannels,
    Session,
}

impl Opts {
    pub(crate) fn resolve(mut self) -> Result<ResolvedCommand> {
        let up_specs = configured_up_specs(&self.up);
        let elf_specs = resolve_elf_specs(&self.elf)?;
        let mut cores: Vec<u32> = if elf_specs.is_empty() {
            vec![0]
        } else {
            elf_specs.iter().map(|(index, _)| *index).collect()
        };
        cores.sort_unstable();

        self.validate(&up_specs)?;
        if self.mode() == Mode::Session {
            self.validate_expanded(&up_specs, &cores)?;
        }

        match self.mode() {
            Mode::ListProbes => return Ok(ResolvedCommand::ListProbes),
            Mode::DebugDefmtTable => {
                return Ok(ResolvedCommand::DebugDefmtTable(elf_specs));
            }
            Mode::ListChannels | Mode::Session => {}
        }

        let probe = match self.probe {
            Some(ProbeInfo::Number(index)) => ProbeSelection::Index(index),
            None if io::stdin().is_terminal() => ProbeSelection::Prompt,
            None => ProbeSelection::OnlyAvailable,
            Some(ProbeInfo::List) => unreachable!("probe listing returned above"),
        };
        let target = TargetConfig {
            probe,
            chip: self
                .chip
                .take()
                .map_or(TargetRequest::AutoDetect, TargetRequest::Named),
            elf_specs,
            scan_region: self.scan_region.take(),
        };
        if self.list {
            return Ok(ResolvedCommand::ListChannels(target));
        }
        let reset = self.reset;
        let policy = self.into_policy(up_specs)?;
        Ok(ResolvedCommand::Session {
            target,
            policy,
            reset,
        })
    }

    fn mode(&self) -> Mode {
        if self.debug_defmt_table {
            Mode::DebugDefmtTable
        } else if matches!(self.probe, Some(ProbeInfo::List)) {
            Mode::ListProbes
        } else if self.list {
            Mode::ListChannels
        } else {
            Mode::Session
        }
    }

    fn has_defmt_up_channel(up_specs: &[ChannelSpec]) -> bool {
        up_specs
            .iter()
            .any(|spec| spec.mode == ChannelEncoding::Defmt)
    }

    fn validate(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        self.validate_channels(up_specs)?;
        self.validate_defmt(up_specs)?;
        self.validate_logging(up_specs)?;
        self.validate_operation_modes()?;
        Ok(())
    }

    fn validate_channels(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        for (position, spec) in up_specs.iter().enumerate() {
            for other in &up_specs[..position] {
                let same_channel = spec.index == other.index;
                let shared_core =
                    spec.core.is_none() || other.core.is_none() || spec.core == other.core;
                if same_channel && shared_core {
                    if spec.mode == other.mode {
                        bail!("up channel {} was specified more than once", spec.index);
                    }
                    bail!(
                        "up channel {} was specified with conflicting modes",
                        spec.index
                    );
                }
            }
        }
        Ok(())
    }

    fn validate_defmt(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        let has_defmt = Self::has_defmt_up_channel(up_specs);
        if has_defmt && self.elf.is_empty() {
            bail!("--elf is required when using an up channel with :defmt");
        }
        if self.debug_defmt_table && self.elf.is_empty() {
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

    /// Validates channel specs against the resolved core set, including core
    /// coverage and raw merged-log fan-out after bare `--up` specs expand over
    /// every configured core.
    fn validate_expanded(&self, up_specs: &[ChannelSpec], cores: &[u32]) -> Result<()> {
        validate_up_coverage(up_specs, cores)?;
        if self.log.is_some()
            && self.log_format == Some(LogFormat::Raw)
            && !self.log_per_channel
            && expand_sources(up_specs, cores).len() > 1
        {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        Ok(())
    }

    /// True when any flag that only applies to a live session was set.
    ///
    /// The destructure is exhaustive so new `Opts` fields must be classified.
    fn has_session_options(&self) -> bool {
        let Self {
            probe: _,
            chip: _,
            list: _,
            up,
            down,
            no_down,
            reset,
            timestamps,
            poll_interval,
            scan_region: _,
            elf: _,
            debug_defmt_table: _,
            color,
            log,
            log_per_channel,
            log_format,
        } = self;
        !up.is_empty()
            || down.is_some()
            || *no_down
            || *reset
            || *timestamps
            || *poll_interval != DEFAULT_POLL_INTERVAL_MS
            || log.is_some()
            || *log_per_channel
            || log_format.is_some()
            || *color != ColorMode::Auto
    }

    fn validate_operation_modes(&self) -> Result<()> {
        if self.debug_defmt_table {
            if self.list || matches!(self.probe, Some(ProbeInfo::List)) {
                bail!("--debug-defmt-table cannot be combined with --list or --probe list");
            }
            if self.has_session_options() {
                bail!("--debug-defmt-table cannot be combined with session options");
            }
        }
        if self.list {
            if matches!(self.probe, Some(ProbeInfo::List)) {
                bail!("--list cannot be combined with --probe list");
            }
            if self.has_session_options() {
                bail!("--list cannot be combined with session options");
            }
        }
        if matches!(self.probe, Some(ProbeInfo::List)) && (self.list || self.has_session_options())
        {
            bail!("--probe list cannot be combined with session options");
        }
        Ok(())
    }
}

pub(crate) enum ResolvedCommand {
    ListProbes,
    DebugDefmtTable(Vec<(u32, PathBuf)>),
    ListChannels(TargetConfig),
    Session {
        target: TargetConfig,
        policy: SessionPolicy,
        reset: bool,
    },
}

pub(crate) struct TargetConfig {
    pub(crate) probe: ProbeSelection,
    pub(crate) chip: TargetRequest,
    pub(crate) elf_specs: Vec<(u32, PathBuf)>,
    pub(crate) scan_region: Option<ScanRegion>,
}

fn configured_up_specs(specs: &[ChannelSpec]) -> Vec<ChannelSpec> {
    if specs.is_empty() {
        vec![ChannelSpec {
            core: None,
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    } else {
        specs.to_vec()
    }
}

/// Every selected channel must reach at least one configured core.
/// Bare specs always qualify; only explicit `CORE:` selections can dangle.
fn validate_up_coverage(up_specs: &[ChannelSpec], cores: &[u32]) -> Result<()> {
    for spec in up_specs {
        if !cores.iter().any(|core| spec.applies_to(*core)) {
            match spec.core {
                Some(core) => {
                    bail!(
                        "up channel {core}:{index} selects no configured core",
                        index = spec.index
                    )
                }
                None => bail!("up channel {} selects no configured core", spec.index),
            }
        }
    }
    Ok(())
}

impl Opts {
    fn into_policy(self, up_specs: Vec<ChannelSpec>) -> Result<SessionPolicy> {
        let down_channel = if self.no_down {
            None
        } else {
            Some(ChannelId::from_cli(self.down.unwrap_or(0), "down")?)
        };
        let down_explicit = self.down.is_some();
        let log = self.log.map(|path| LogConfig {
            destination: if self.log_per_channel {
                LogDestination::PerChannel(path)
            } else {
                LogDestination::Merged(path)
            },
            format: self.log_format.unwrap_or(LogFormat::Decoded),
        });

        Ok(SessionPolicy {
            up_specs,
            down_channel,
            down_explicit,
            poll_interval: Duration::from_millis(self.poll_interval),
            timestamps: self.timestamps,
            color: self.color,
            log,
        })
    }
}
