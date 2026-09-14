use anyhow::{bail, Context, Result};
use brtt::rtt::ScanRegion;
use defmt_decoder::{Locations, Table};
use defmt_parser::Level;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct DefmtData {
    pub(crate) path: PathBuf,
    pub(crate) table: Table,
    pub(crate) locations: Option<Locations>,
}

/// A parsed defmt filter entry: a module prefix with a minimum level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filter {
    pub(crate) module: Box<str>,
    pub(crate) level: Level,
}

impl Filter {
    fn matches(&self, module: Option<&str>) -> bool {
        if self.module.is_empty() {
            return true;
        }
        module.is_some_and(|module| {
            module == &*self.module
                || module
                    .strip_prefix(&*self.module)
                    .is_some_and(|rest| rest.starts_with("::"))
        })
    }
}

/// Parsed `--defmt-filter` value, kept as a newtype so clap does not treat the
/// inner `Vec` as a repeated argument.
#[derive(Debug, Clone)]
pub(crate) struct FilterSpec(pub(crate) Vec<Filter>);

pub(crate) fn read_elf(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read ELF '{}'", path.display()))
}

pub(crate) fn rtt_region_from_elf(path: &Path, bytes: &[u8]) -> Result<ScanRegion> {
    let address = probe_rs::rtt::find_rtt_control_block_in_raw_file(bytes)
        .with_context(|| format!("failed to parse ELF '{}'", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!("ELF '{}' has no defined _SEGGER_RTT symbol", path.display())
        })?;
    Ok(ScanRegion::Exact(address))
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedFrame<'a> {
    pub(crate) message: Box<str>,
    pub(crate) timestamp: Option<Box<str>>,
    pub(crate) level: Option<Level>,
    /// Borrowed from the ELF locations, which outlive the session.
    pub(crate) module: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) enum DecodeOutput<'a> {
    Frame(DecodedFrame<'a>),
    Warning(Box<str>),
}

/// Result of decoding the bytes received during one poll of a defmt channel.
#[derive(Debug, Default)]
pub(crate) struct DecodedFrames<'a> {
    pub(crate) frames: Vec<DecodeOutput<'a>>,
    pub(crate) warnings: u64,
    pub(crate) suppressed_warnings: u64,
    pub(crate) hit_frame_limit: bool,
    /// Set when the decoder cannot make progress and must be recreated.
    pub(crate) restart: bool,
}

const MAX_DECODED_FRAMES: usize = 1024;
const MAX_DECODE_ITERATIONS: usize = 4096;
const MAX_DECODE_WARNINGS: usize = 8;
pub(crate) const MAX_DECODE_BUFFERED_BYTES: usize = 64 * 1024;

/// Decodes all complete frames from the channel decoder.
///
/// Decode errors are reported as warnings and never fail the session. Framed
/// encodings resynchronize after a consumed frame; raw encodings cannot, so
/// they request a decoder restart after reporting the error.
pub(crate) fn decode_frames<'a>(
    decoder: &mut dyn defmt_decoder::StreamDecoder,
    bytes: &[u8],
    locations: Option<&'a Locations>,
    can_recover: bool,
) -> DecodedFrames<'a> {
    decoder.received(bytes);
    let mut result = DecodedFrames::default();
    let mut iterations = 0usize;

    loop {
        if result.frames.len() >= MAX_DECODED_FRAMES || iterations >= MAX_DECODE_ITERATIONS {
            if result.frames.len() >= MAX_DECODED_FRAMES {
                result.hit_frame_limit = true;
                break;
            }
            result.suppressed_warnings += 1;
            result.restart = true;
            break;
        }
        iterations += 1;

        match decoder.decode() {
            Ok(frame) => {
                let location = locations.and_then(|locations| locations.get(&frame.index()));
                result.frames.push(DecodeOutput::Frame(DecodedFrame {
                    message: frame.display_message().to_string().into_boxed_str(),
                    timestamp: frame
                        .display_timestamp()
                        .map(|timestamp| timestamp.to_string().into_boxed_str()),
                    level: frame.level(),
                    module: location.map(|location| location.module.as_str()),
                }));
            }
            Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
            Err(error) if can_recover => {
                result.warnings += 1;
                if result.warnings as usize <= MAX_DECODE_WARNINGS {
                    result.frames.push(DecodeOutput::Warning(
                        format!("defmt decode warning: {error}").into_boxed_str(),
                    ));
                } else {
                    result.suppressed_warnings += 1;
                }
            }
            Err(error) => {
                result.warnings += 1;
                result.frames.push(DecodeOutput::Warning(
                    format!("defmt decode warning: {error}; resetting decoder").into_boxed_str(),
                ));
                result.restart = true;
                break;
            }
        }
    }

    if result.suppressed_warnings > 0 {
        result.frames.push(DecodeOutput::Warning(
            format!(
                "{} further defmt decode warnings suppressed",
                result.suppressed_warnings
            )
            .into_boxed_str(),
        ));
    }
    result
}

impl DefmtData {
    pub(crate) fn load(path: &Path, bytes: &[u8]) -> Result<Self> {
        let table = Table::parse(bytes)
            .with_context(|| format!("failed to parse defmt table in '{}'", path.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!("ELF '{}' contains no .defmt section", path.display())
            })?;
        let locations = table.get_locations(bytes).ok();

        Ok(Self {
            path: path.to_path_buf(),
            table,
            locations,
        })
    }

    pub(crate) fn debug_summary(&self, output: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(output, "ELF: {}", self.path.display())?;
        writeln!(output, "Encoding: {:?}", self.table.encoding())?;
        writeln!(output, "Has timestamp: {}", self.table.has_timestamp())?;
        writeln!(output, "Locations: {}", self.locations.is_some())?;
        writeln!(output, "Indices:")?;
        for index in self.table.indices() {
            writeln!(output, "  {index:#x}")?;
        }
        writeln!(output, "Raw symbols:")?;
        for symbol in self.table.raw_symbols() {
            writeln!(output, "  {symbol}")?;
        }
        Ok(())
    }
}

pub(crate) fn parse_filter_level(value: &str) -> Result<defmt_parser::Level> {
    match value.to_ascii_lowercase().as_str() {
        "trace" => Ok(defmt_parser::Level::Trace),
        "debug" => Ok(defmt_parser::Level::Debug),
        "info" => Ok(defmt_parser::Level::Info),
        "warn" | "warning" => Ok(defmt_parser::Level::Warn),
        "error" => Ok(defmt_parser::Level::Error),
        _ => Err(anyhow::anyhow!(
            "invalid defmt level '{}', expected trace, debug, info, warn, or error",
            value
        )),
    }
}

pub(crate) fn level_name(level: defmt_parser::Level) -> &'static str {
    match level {
        defmt_parser::Level::Trace => "trace",
        defmt_parser::Level::Debug => "debug",
        defmt_parser::Level::Info => "info",
        defmt_parser::Level::Warn => "warn",
        defmt_parser::Level::Error => "error",
    }
}

pub(crate) fn level_enabled(level: defmt_parser::Level, minimum: defmt_parser::Level) -> bool {
    level >= minimum
}

pub(crate) fn parse_filter_spec(spec: &str) -> Result<Vec<Filter>> {
    let mut result = Vec::new();
    for item in spec.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (module, level) = item.split_once('=').unwrap_or(("", item));
        let module = module.trim();
        if module.contains(char::is_whitespace) {
            bail!("invalid defmt filter module '{module}': whitespace is not allowed");
        }
        result.push(Filter {
            module: module.into(),
            level: parse_filter_level(level.trim())?,
        });
    }
    if result.is_empty() {
        bail!("defmt filter cannot be empty");
    }
    Ok(result)
}

pub(crate) fn parse_filter_spec_value(spec: &str) -> std::result::Result<FilterSpec, String> {
    parse_filter_spec(spec)
        .map(FilterSpec)
        .map_err(|error| error.to_string())
}

pub(crate) fn filter_level(module: Option<&str>, filters: &[Filter]) -> defmt_parser::Level {
    let mut selected = defmt_parser::Level::Trace;
    let mut best_len = 0;
    for filter in filters {
        if filter.matches(module) && filter.module.len() >= best_len {
            selected = filter.level;
            best_len = filter.module.len();
        }
    }
    selected
}

#[cfg(test)]
#[path = "../tests/defmt.rs"]
mod tests;
