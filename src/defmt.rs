use crate::elf::read_elf;
use anyhow::{Context, Result};
use defmt_decoder::Table;
use defmt_parser::Level;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct DefmtData {
    pub(crate) table: Table,
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

#[derive(Debug, Default)]
pub(crate) struct DecodedFrames {
    pub(crate) frames: Vec<DecodeOutput>,
    pub(crate) hit_frame_limit: bool,
    /// Set when the decoder cannot make progress and must be recreated.
    pub(crate) restart: bool,
}

const MAX_DECODED_FRAMES: usize = 1024;
const MAX_DECODE_ITERATIONS: usize = 4096;
const MAX_DECODE_WARNINGS: usize = 8;
pub(crate) const MAX_DECODE_BUFFERED_BYTES: usize = 64 * 1024;

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
    let mut warnings = 0u64;
    let mut iterations = 0usize;
    let mut suppressed_warnings = 0u64;

    loop {
        if result.frames.len() >= MAX_DECODED_FRAMES || iterations >= MAX_DECODE_ITERATIONS {
            if result.frames.len() >= MAX_DECODED_FRAMES {
                result.hit_frame_limit = true;
                break;
            }
            suppressed_warnings += 1;
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
                warnings += 1;
                if warnings as usize <= MAX_DECODE_WARNINGS {
                    result.frames.push(DecodeOutput::Warning(
                        format!("defmt decode warning: {error}").into_boxed_str(),
                    ));
                } else {
                    suppressed_warnings += 1;
                }
            }
            Err(error) => {
                result.frames.push(DecodeOutput::Warning(
                    format!("defmt decode warning: {error}; resetting decoder").into_boxed_str(),
                ));
                result.restart = true;
                break;
            }
        }
    }

    if suppressed_warnings > 0 {
        result.frames.push(DecodeOutput::Warning(
            format!("{suppressed_warnings} further defmt decode warnings suppressed")
                .into_boxed_str(),
        ));
    }
    result
}

impl DefmtData {
    pub(crate) fn debug_from_elf(path: &Path, output: &mut impl std::io::Write) -> Result<()> {
        let bytes = read_elf(path)?;
        let data = Self::load(path, &bytes)?;
        let has_locations = data.table.get_locations(&bytes).is_ok();
        data.debug_summary(path, has_locations, output)?;
        Ok(())
    }

    pub(crate) fn load(path: &Path, bytes: &[u8]) -> Result<Self> {
        let table = Table::parse(bytes)
            .with_context(|| format!("failed to parse defmt table in '{}'", path.display()))?
            .ok_or_else(|| {
                anyhow::anyhow!("ELF '{}' contains no .defmt section", path.display())
            })?;

        Ok(Self { table })
    }

    fn debug_summary(
        &self,
        path: &Path,
        has_locations: bool,
        output: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        writeln!(output, "ELF: {}", path.display())?;
        writeln!(output, "Encoding: {:?}", self.table.encoding())?;
        writeln!(output, "Has timestamp: {}", self.table.has_timestamp())?;
        writeln!(output, "Locations: {has_locations}")?;
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

pub(crate) fn level_name(level: defmt_parser::Level) -> &'static str {
    match level {
        defmt_parser::Level::Trace => "trace",
        defmt_parser::Level::Debug => "debug",
        defmt_parser::Level::Info => "info",
        defmt_parser::Level::Warn => "warn",
        defmt_parser::Level::Error => "error",
    }
}
