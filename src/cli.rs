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

/// Up channel selection, optionally restricted to one core.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct ChannelSpec {
    /// `None` selects the channel on every configured core.
    pub(crate) core: Option<u32>,
    pub(crate) index: u32,
    pub(crate) mode: ChannelEncoding,
}

impl ChannelSpec {
    /// Whether this spec selects channels on `core` (`None` means every core).
    pub(crate) fn applies_to(&self, core: u32) -> bool {
        self.core.is_none_or(|c| c == core)
    }
}

impl std::str::FromStr for ChannelSpec {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // `[CORE:]CHANNEL[:MODE]`: a bare channel or `CHANNEL:MODE` keeps
        // today's meaning (all cores); `1:0` can only be CORE:CHANNEL since
        // `0` is not a mode, and `1:0:defmt` is fully explicit.
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

/// One `--elf` value: either a bare path (assigned the lowest free core
/// index) or an explicit `INDEX=PATH` mapping. Any `=` requires the indexed
/// form; a bare path must not contain `=`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(crate) struct ElfSpec {
    pub(crate) index: Option<u32>,
    pub(crate) path: PathBuf,
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

/// Default `--poll-interval` in milliseconds: the single source of truth for
/// the clap default below and the session-options check.
pub(crate) const DEFAULT_POLL_INTERVAL_MS: u64 = 10;

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
        "    Filters hide only, target still sends.\n",
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
    pub(crate) probe: Option<ProbeInfo>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "Target chip type. Leave unspecified to auto-detect."
    )]
    pub(crate) chip: Option<String>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "List RTT channels and exit."
    )]
    pub(crate) list: bool,

    #[clap(
        short,
        long,
        help_heading = "Channels",
        action = clap::ArgAction::Append,
        value_name = "[CORE:]CHANNEL[:MODE]",
        help = "Up channel specification, as CHANNEL, CHANNEL:MODE or CORE:CHANNEL[:MODE]. MODE is terminal or defmt; defaults to terminal. Without CORE: the channel is selected on every configured core. May be repeated."
    )]
    pub(crate) up: Vec<ChannelSpec>,

    #[clap(
        short,
        long,
        help_heading = "Channels",
        conflicts_with = "no_down",
        value_name = "CHANNEL",
        help = "Down channel specification. Only one channel is supported; defaults to channel 0. Applies to every configured core; keyboard input routes to the lowest core exposing it."
    )]
    pub(crate) down: Option<u32>,

    #[clap(
        short,
        long,
        help_heading = "Target",
        help = "Reset the target after RTT session was opened"
    )]
    pub(crate) reset: bool,

    #[clap(
        short = 't',
        long = "timestamp",
        help_heading = "Display",
        help = "Enable local date and time timestamps with millisecond precision."
    )]
    pub(crate) timestamps: bool,

    #[clap(
        long,
        help_heading = "Display",
        default_value_t = DEFAULT_POLL_INTERVAL_MS,
        value_parser = clap::value_parser!(u64).range(1..),
        value_name = "MILLISECONDS",
        help = "Polling interval for RTT and keyboard input."
    )]
    pub(crate) poll_interval: u64,

    #[clap(
        long,
        help_heading = "Target",
        value_parser = parse_scan_region,
        help = "Memory region to scan for control block. You can specify either an exact starting address '0x1000' or a range such as '0x0000..0x1000'. Both decimal and hex are accepted."
    )]
    pub(crate) scan_region: Option<ScanRegion>,

    #[clap(
        long,
        help_heading = "Defmt",
        action = clap::ArgAction::Append,
        value_name = "[INDEX=]PATH",
        help = "ELF file for a target core, as PATH or INDEX=PATH. A bare PATH targets the lowest free core index from 0; INDEX selects the core explicitly and may be sparse. May be repeated to attach several cores."
    )]
    pub(crate) elf: Vec<ElfSpec>,

    #[clap(
        long,
        help_heading = "Defmt",
        requires = "elf",
        help = "Print the loaded defmt table and exit."
    )]
    pub(crate) debug_defmt_table: bool,

    #[clap(
        long = "defmt-filter",
        help_heading = "Defmt",
        value_parser = crate::defmt::parse_filter_spec_value,
        value_name = "SPEC",
        help = "Filter defmt output, e.g. warn or app=debug,warn."
    )]
    pub(crate) defmt_filters: Option<FilterSpec>,

    #[clap(long, value_enum, default_value_t = ColorMode::Auto, help_heading = "Display", help = "Terminal color mode for channel labels and defmt levels.")]
    pub(crate) color: ColorMode,

    #[clap(
        short = 'L',
        long,
        help_heading = "Logging",
        value_name = "PATH",
        help = "Write session output to a log file."
    )]
    pub(crate) log: Option<PathBuf>,

    #[clap(
        long,
        requires = "log",
        help_heading = "Logging",
        help = "Write one log file per up channel."
    )]
    pub(crate) log_per_channel: bool,

    #[clap(
        long,
        value_enum,
        help_heading = "Logging",
        requires = "log",
        help = "Log raw bytes or cleaned decoded text. Defaults to decoded."
    )]
    pub(crate) log_format: Option<LogFormat>,

    #[clap(
        long,
        help_heading = "Channels",
        help = "Disable the default down channel and keyboard input."
    )]
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

/// What this run does. `validate_operation_modes` rejects combining these,
/// so choosing the first matching flag is enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `--debug-defmt-table`: print the table and exit. ELF only, no probe.
    DebugDefmtTable,
    /// `--probe list`: print probes and exit. No ELF and no target.
    ListProbes,
    /// `--list`: attach, find RTT, print channels and exit.
    ListChannels,
    /// Normal operation.
    Session,
}

impl Opts {
    pub(crate) fn mode(&self) -> Mode {
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

    pub(crate) fn validate(&self, up_specs: &[ChannelSpec]) -> Result<()> {
        self.validate_channels(up_specs)?;
        self.validate_defmt(up_specs)?;
        self.validate_logging(up_specs)?;
        self.validate_operation_modes()?;
        self.validate_filter()?;
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
        if self.defmt_filters.is_some() && !has_defmt {
            bail!("--defmt-filter requires at least one up channel using :defmt");
        }
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

    /// Validates the specs after ELF indices resolve to cores: coverage plus
    /// raw merged-log fan-out over expanded sources (one bare `--up` over two
    /// cores is two sources). Runs before probe attachment so failures never
    /// touch hardware.
    pub(crate) fn validate_expanded(&self, up_specs: &[ChannelSpec], cores: &[u32]) -> Result<()> {
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

    /// True when any flag that only affects a live session was set.
    ///
    /// `--list`, `--debug-defmt-table` and `--probe list` reject these (see
    /// `validate_operation_modes`). Target-discovery options (`chip`,
    /// `scan_region`, `elf`) and the `--probe` selector are intentionally NOT
    /// listed: the info modes need them to attach and find RTT.
    ///
    /// When adding a session-only flag to `Opts`, add it to the destructure
    /// below (deliberately exhaustive: omitting a field is a compile error)
    /// and to the `||` chain when it is session-only.
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
            defmt_filters,
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
            || defmt_filters.is_some()
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
            core: None,
            index: 0,
            mode: ChannelEncoding::Terminal,
        }]
    } else {
        specs.to_vec()
    }
}

/// One resolved up channel on one core after bare specs expand over every
/// configured core. Used for source-count validation before touching hardware.
pub(crate) struct SelectedSource {
    pub(crate) core: u32,
    pub(crate) channel: u32,
    pub(crate) mode: ChannelEncoding,
}

/// Expands every spec over the configured cores: a bare spec yields one
/// source per core, an explicit `CORE:` spec yields one source on its core
/// (which the caller guarantees is configured).
pub(crate) fn expand_sources(up_specs: &[ChannelSpec], cores: &[u32]) -> Vec<SelectedSource> {
    let mut sources = Vec::new();
    for spec in up_specs {
        for core in cores {
            if spec.applies_to(*core) {
                sources.push(SelectedSource {
                    core: *core,
                    channel: spec.index,
                    mode: spec.mode,
                });
            }
        }
    }
    sources
}

/// Every selected channel must reach at least one configured core.
/// Bare specs always qualify; only explicit `CORE:` selections can dangle.
pub(crate) fn validate_up_coverage(up_specs: &[ChannelSpec], cores: &[u32]) -> Result<()> {
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

/// Assigns each `--elf` value a core index. Explicit `INDEX=` mappings reserve
/// their slots first regardless of argument order, then bare paths fill the
/// lowest free slots in CLI order. Gaps are allowed so a single `--elf 1=…`
/// can target a non-zero core.
pub(crate) fn resolve_elf_specs(specs: &[ElfSpec]) -> Result<Vec<(u32, PathBuf)>> {
    let mut reserved = HashSet::new();
    for spec in specs {
        if let Some(index) = spec.index {
            if !reserved.insert(index) {
                bail!("--elf index {index} was specified more than once");
            }
        }
    }
    let mut used = HashSet::new();
    let mut resolved = Vec::with_capacity(specs.len());
    for spec in specs {
        let index = match spec.index {
            Some(index) => index,
            None => (0..)
                .find(|index| !reserved.contains(index) && !used.contains(index))
                .expect("u32 core indices exhausted"),
        };
        if !used.insert(index) {
            bail!("--elf index {index} was specified more than once");
        }
        resolved.push((index, spec.path.clone()));
    }
    Ok(resolved)
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
    /// Whether `--down` was passed explicitly (as opposed to the default).
    /// Decides if a missing down channel is an error or just disables input.
    pub(crate) down_explicit: bool,
    pub(crate) poll_interval: Duration,
    pub(crate) timestamps: bool,
    pub(crate) defmt: Option<DefmtData>,
    pub(crate) defmt_filters: Option<Vec<Filter>>,
    pub(crate) color: ColorMode,
    pub(crate) log: Option<LogConfig>,
    pub(crate) discovery: RttDiscovery,
}

impl SessionConfig {
    pub(crate) fn from_opts(
        opts: &Opts,
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
        let down_explicit = opts.down.is_some();
        let log = opts.log.as_ref().map(|path| LogConfig {
            destination: if opts.log_per_channel {
                LogDestination::PerChannel(path.clone())
            } else {
                LogDestination::Merged(path.clone())
            },
            format: opts.log_format.unwrap_or(LogFormat::Decoded),
        });

        Ok(Self {
            probe,
            chip,
            up_specs: configured_up_specs(&opts.up),
            down_channel,
            down_explicit,
            poll_interval: Duration::from_millis(opts.poll_interval),
            timestamps: opts.timestamps,
            defmt,
            defmt_filters: opts.defmt_filters.clone().map(|spec| spec.0),
            color: opts.color,
            log,
            discovery,
        })
    }
}

#[cfg(test)]
#[path = "../tests/cli.rs"]
mod tests;
