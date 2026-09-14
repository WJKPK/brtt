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

pub(crate) fn rtt_region_from_elf(path: &Path) -> Result<ScanRegion> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read ELF '{}'", path.display()))?;
    let address = probe_rs::rtt::find_rtt_control_block_in_raw_file(&bytes)
        .with_context(|| format!("failed to parse ELF '{}'", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!("ELF '{}' has no defined _SEGGER_RTT symbol", path.display())
        })?;
    Ok(ScanRegion::Exact(address))
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedFrame {
    pub(crate) message: String,
    pub(crate) timestamp: Option<String>,
    pub(crate) level: Option<Level>,
    pub(crate) module: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum DecodeOutput {
    Frame(DecodedFrame),
    Warning(String),
}

pub(crate) fn decode_frames(
    decoder: &mut dyn defmt_decoder::StreamDecoder,
    bytes: &[u8],
    locations: Option<&Locations>,
    can_recover: bool,
) -> Result<Vec<DecodeOutput>, defmt_decoder::DecodeError> {
    decoder.received(bytes);
    let mut frames = Vec::new();
    loop {
        match decoder.decode() {
            Ok(frame) => {
                let location = locations.and_then(|locations| locations.get(&frame.index()));
                frames.push(DecodeOutput::Frame(DecodedFrame {
                    message: frame.display_message().to_string(),
                    timestamp: frame
                        .display_timestamp()
                        .map(|timestamp| timestamp.to_string()),
                    level: frame.level(),
                    module: location.map(|location| location.module.clone()),
                }));
            }
            Err(defmt_decoder::DecodeError::UnexpectedEof) => return Ok(frames),
            Err(error) if can_recover => {
                frames.push(DecodeOutput::Warning(format!(
                    "defmt decode warning: {error}"
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

impl DefmtData {
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read ELF '{}'", path.display()))?;
        let table = Table::parse(&bytes)
            .with_context(|| format!("failed to parse defmt table in '{}'", path.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!("ELF '{}' contains no .defmt section", path.display())
            })?;
        let locations = table.get_locations(&bytes).ok();

        Ok(Self {
            path,
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

pub(crate) fn require_elf(path: Option<&Path>, has_defmt: bool) -> Result<Option<DefmtData>> {
    if !has_defmt {
        return Ok(None);
    }

    let path = path
        .ok_or_else(|| anyhow::anyhow!("--elf is required when using an up channel with :defmt"))?;
    Ok(Some(DefmtData::load(path)?))
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
    fn rank(level: defmt_parser::Level) -> u8 {
        match level {
            defmt_parser::Level::Trace => 0,
            defmt_parser::Level::Debug => 1,
            defmt_parser::Level::Info => 2,
            defmt_parser::Level::Warn => 3,
            defmt_parser::Level::Error => 4,
        }
    }

    rank(level) >= rank(minimum)
}

pub(crate) fn parse_filter_spec(spec: &str) -> Result<Vec<(String, defmt_parser::Level)>> {
    let mut result = Vec::new();
    for item in spec.split(',').filter(|item| !item.trim().is_empty()) {
        let (module, level) = item.split_once('=').unwrap_or(("", item));
        if module.contains(char::is_whitespace) {
            bail!(
                "invalid defmt filter module '{}': whitespace is not allowed",
                module
            );
        }
        result.push((module.to_string(), parse_filter_level(level.trim())?));
    }
    if result.is_empty() {
        bail!("defmt filter cannot be empty");
    }
    Ok(result)
}

pub(crate) fn filter_level(
    module: Option<&str>,
    filters: &[(String, defmt_parser::Level)],
) -> defmt_parser::Level {
    let mut selected = defmt_parser::Level::Trace;
    let mut best_len = 0;
    for (prefix, level) in filters {
        let matches = if prefix.is_empty() {
            true
        } else {
            module.is_some_and(|module| {
                module == prefix || module.starts_with(&format!("{prefix}::"))
            })
        };
        if matches && prefix.len() >= best_len {
            selected = *level;
            best_len = prefix.len();
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

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
            parse_filter_spec("warning,app=DEBUG").unwrap(),
            vec![
                (String::new(), Level::Warn),
                ("app".to_string(), Level::Debug)
            ]
        );
        assert!(parse_filter_spec("").is_err());
        assert!(parse_filter_spec("app=unknown").is_err());
        assert!(parse_filter_spec("app=warn,app=info").is_ok());
    }
}
