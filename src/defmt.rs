use anyhow::{Context, Result};
use brtt::rtt::ScanRegion;
use defmt_decoder::{Locations, Table};
use defmt_parser::Level;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct DefmtData {
    pub(crate) table: Table,
    locations: Option<Locations>,
}

fn read_elf(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read ELF '{}'", path.display()))
}

fn rtt_region_from_elf(path: &Path, bytes: &[u8]) -> Result<Option<ScanRegion>> {
    let address = probe_rs::rtt::find_rtt_control_block_in_raw_file(bytes)
        .with_context(|| format!("failed to parse ELF '{}'", path.display()))?;
    Ok(address.map(ScanRegion::Exact))
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedFrame {
    pub(crate) message: Box<str>,
    pub(crate) timestamp: Option<Box<str>>,
    pub(crate) level: Option<Level>,
}

#[derive(Debug, Clone)]
pub(crate) enum DecodeOutput {
    Frame(DecodedFrame),
    Warning(Box<str>),
}

/// Result of decoding the bytes received during one poll of a defmt channel.
#[derive(Debug, Default)]
pub(crate) struct DecodedFrames {
    pub(crate) frames: Vec<DecodeOutput>,
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
pub(crate) fn decode_frames(
    decoder: &mut dyn defmt_decoder::StreamDecoder,
    bytes: &[u8],
    can_recover: bool,
) -> DecodedFrames {
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
                result.frames.push(DecodeOutput::Frame(DecodedFrame {
                    message: frame.display_message().to_string().into_boxed_str(),
                    timestamp: frame
                        .display_timestamp()
                        .map(|timestamp| timestamp.to_string().into_boxed_str()),
                    level: frame.level(),
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
    /// Reads `path` once and prints its defmt table and DWARF availability.
    pub(crate) fn debug_from_elf(path: &Path, output: &mut impl std::io::Write) -> Result<()> {
        let bytes = read_elf(path)?;
        let mut data = Self::load(path, &bytes)?;
        data.locations = data.table.get_locations(&bytes).ok();
        data.debug_summary(path, output)?;
        Ok(())
    }

    fn load(path: &Path, bytes: &[u8]) -> Result<Self> {
        let table = Table::parse(bytes)
            .with_context(|| format!("failed to parse defmt table in '{}'", path.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!("ELF '{}' contains no .defmt section", path.display())
            })?;

        Ok(Self {
            table,
            locations: None,
        })
    }

    fn debug_summary(&self, path: &Path, output: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(output, "ELF: {}", path.display())?;
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

/// Everything derived from `--elf` for target modes. The file is read once,
/// so the RTT control block symbol and the defmt table stay consistent.
pub(crate) struct ElfContents {
    pub(crate) region: Option<ScanRegion>,
    pub(crate) defmt: Option<DefmtData>,
}

impl ElfContents {
    /// Parses the defmt table only when `with_defmt` is set.
    pub(crate) fn load(path: &Path, with_defmt: bool) -> Result<Self> {
        let bytes = read_elf(path)?;
        let region = rtt_region_from_elf(path, &bytes)?;
        let defmt = with_defmt
            .then(|| DefmtData::load(path, &bytes))
            .transpose()?;
        Ok(Self { region, defmt })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_elf_is_not_treated_as_missing_symbol() {
        let error = rtt_region_from_elf(Path::new("broken.elf"), b"not an ELF").unwrap_err();
        assert!(error.to_string().contains("failed to parse ELF"));
    }
}
