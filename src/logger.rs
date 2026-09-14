use crate::cli::LogFormat;
use crate::terminal::DecodedStream;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub(crate) struct Logger {
    path: PathBuf,
    per_channel: bool,
    format: LogFormat,
    include_channel: bool,
    merged: Option<BufWriter<File>>,
    channels: HashMap<usize, ChannelLog>,
}

/// Log sink and decoding state for one RTT up channel.
struct ChannelLog {
    /// Per-channel file from `--log-per-channel`; `None` while merged.
    file: Option<BufWriter<File>>,
    state: ChannelState,
}

/// Decoding state selected once per channel from its RTT channel mode.
enum ChannelState {
    /// Raw RTT bytes, written without decoding.
    Raw,
    /// Shell output, decoded through a VT terminal model.
    Terminal(Box<DecodedStream>),
    /// Formatted defmt text, buffered until a newline.
    Text(Vec<u8>),
}

impl Logger {
    pub(crate) fn new(
        path: Option<&Path>,
        per_channel: bool,
        format: LogFormat,
        include_channel: bool,
    ) -> Result<Option<Self>> {
        let Some(path) = path else { return Ok(None) };
        if format == LogFormat::Raw && !per_channel && include_channel {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let mut logger = Self {
            path: path.to_path_buf(),
            per_channel,
            format,
            include_channel: include_channel && !per_channel,
            merged: None,
            channels: HashMap::new(),
        };
        if !per_channel {
            logger.merged = Some(BufWriter::new(open_log(path)?));
        }
        Ok(Some(logger))
    }

    fn channel_log(&mut self, channel: usize, state: impl FnOnce() -> ChannelState) -> Result<()> {
        use std::collections::hash_map::Entry;
        if let Entry::Vacant(entry) = self.channels.entry(channel) {
            let file = if self.per_channel {
                Some(BufWriter::new(open_log(&channel_path(
                    &self.path, channel,
                ))?))
            } else {
                None
            };
            entry.insert(ChannelLog {
                file,
                state: state(),
            });
        }
        Ok(())
    }

    fn channel_mut(&mut self, channel: usize) -> &mut ChannelLog {
        self.channels
            .get_mut(&channel)
            .expect("channel log initialized")
    }

    fn file_for_channel(&mut self, channel: usize) -> Result<&mut BufWriter<File>> {
        if self.per_channel {
            let log = self.channel_mut(channel);
            Ok(log.file.as_mut().expect("per-channel log file initialized"))
        } else {
            Ok(self.merged.as_mut().expect("merged file initialized"))
        }
    }

    /// Append exact RTT bytes; raw log mode only.
    pub(crate) fn write_bytes(&mut self, channel: usize, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || ChannelState::Raw)?;
        self.file_for_channel(channel)?
            .write_all(bytes)
            .with_context(|| format!("writing raw log for channel {channel}"))?;
        Ok(())
    }

    /// Append terminal-channel bytes after VT decoding; decoded mode only.
    pub(crate) fn write_chars(&mut self, channel: usize, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || {
            ChannelState::Terminal(Box::new(DecodedStream::new()))
        })?;
        let complete = self.channel_mut(channel).terminal_stream().consume(bytes);
        if complete.is_empty() {
            return Ok(());
        }
        let include_channel = self.include_channel;
        {
            let file = self.file_for_channel(channel)?;
            for line in &complete {
                if include_channel {
                    write!(file, "[ch{channel}] ")?;
                }
                file.write_all(line)?;
            }
        }
        self.flush_files()
    }

    /// Append an already-decoded defmt line; decoded mode only.
    pub(crate) fn write_defmt_decoded(&mut self, channel: usize, line: &[u8]) -> Result<()> {
        if self.format != LogFormat::Decoded || line.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, || ChannelState::Text(Vec::new()))?;
        let mut pending = std::mem::take(self.channel_mut(channel).text_slot());
        pending.extend_from_slice(line);

        let mut start = 0;
        let has_complete_line = memchr::memchr(b'\n', &pending).is_some();
        if has_complete_line {
            let include_channel = self.include_channel;
            {
                let file = self.file_for_channel(channel)?;
                while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
                    let end = start + offset + 1;
                    if include_channel {
                        write!(file, "[ch{channel}] ")?;
                    }
                    file.write_all(&pending[start..end])?;
                    start = end;
                }
            }
            self.flush_files()?;
        }
        if start > 0 {
            pending.drain(..start);
        }
        *self.channel_mut(channel).text_slot() = pending;
        Ok(())
    }

    fn partial_lines(&self) -> Vec<(usize, Vec<u8>)> {
        let mut lines: Vec<_> = self
            .channels
            .iter()
            .filter_map(|(&channel, log)| {
                let line = match &log.state {
                    ChannelState::Terminal(stream) => stream.visible_line(),
                    ChannelState::Text(pending) => pending.clone(),
                    ChannelState::Raw => Vec::new(),
                };
                (!line.is_empty()).then_some((channel, line))
            })
            .collect();
        lines.sort_unstable_by_key(|(channel, _)| *channel);
        lines
    }

    fn write_tails(&mut self, tails: &[(usize, Vec<u8>)], newline: bool) -> Result<()> {
        let include_channel = self.include_channel;
        for (channel, line) in tails {
            let file = self.file_for_channel(*channel)?;
            if include_channel {
                write!(file, "[ch{channel}] ")?;
            }
            file.write_all(line)?;
            if newline {
                file.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    /// Writes any buffered partial lines and flushes the log files.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, false)?;
        self.flush_files()
    }

    /// Flushes buffered log data without emitting partial lines.
    pub(crate) fn flush_files(&mut self) -> Result<()> {
        if let Some(file) = &mut self.merged {
            file.flush().context("flushing log file")?;
        }
        for log in self.channels.values_mut() {
            if let Some(file) = &mut log.file {
                file.flush().context("flushing per-channel log file")?;
            }
        }
        Ok(())
    }

    /// Finalizes partial lines and clears all per-target terminal state.
    ///
    /// Used when the target restarts so output from a new boot is not merged
    /// with the previous session's partial line.
    pub(crate) fn reset(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, true)?;
        for log in self.channels.values_mut() {
            match &mut log.state {
                ChannelState::Terminal(stream) => stream.reset(),
                ChannelState::Text(pending) => pending.clear(),
                ChannelState::Raw => {}
            }
        }
        self.flush_files()
    }
}

impl ChannelLog {
    fn terminal_stream(&mut self) -> &mut DecodedStream {
        match &mut self.state {
            ChannelState::Terminal(stream) => stream,
            _ => unreachable!("channel log is not a terminal stream"),
        }
    }

    fn text_slot(&mut self) -> &mut Vec<u8> {
        match &mut self.state {
            ChannelState::Text(pending) => pending,
            _ => unreachable!("channel log is not defmt text"),
        }
    }
}

fn open_log(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening log file '{}'", path.display()))
}

fn channel_path(path: &Path, channel: usize) -> PathBuf {
    let mut name = path
        .file_stem()
        .map(|stem| stem.to_os_string())
        .unwrap_or_else(|| OsString::from("log"));
    name.push(format!(".ch{channel}"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}

#[cfg(test)]
#[path = "../tests/logger.rs"]
mod tests;
