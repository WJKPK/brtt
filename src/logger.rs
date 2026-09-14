use crate::cli::LogFormat;
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
    /// Already-decoded terminal lines from the single VT decode in `target.rs`.
    ///
    /// Holds the latest plain decoded partial line. Completed lines are
    /// written immediately; the partial is cached so `flush`/`reset` can
    /// finalize it without owning a second terminal parser.
    Terminal { partial: Vec<u8> },
    /// Formatted defmt text, buffered until a newline.
    Text(Vec<u8>),
}

impl ChannelState {
    fn partial_line(&self) -> Vec<u8> {
        match self {
            Self::Terminal { partial } => partial.clone(),
            Self::Text(pending) => pending.clone(),
            Self::Raw => Vec::new(),
        }
    }

    fn clear_partial(&mut self) {
        match self {
            Self::Terminal { partial } => partial.clear(),
            Self::Text(pending) => pending.clear(),
            Self::Raw => {}
        }
    }
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

    /// Append already-decoded terminal lines; decoded mode only.
    ///
    /// `complete` holds plain decoded lines including their trailing `\n`,
    /// produced by the single VT decode of the channel bytes.
    /// `partial` is the current plain decoded partial line, cached so a later
    /// `flush`/`reset` can finalize it.
    pub(crate) fn write_terminal_decoded<I, B>(
        &mut self,
        channel: usize,
        complete: I,
        partial: &[u8],
    ) -> Result<()>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        if self.format != LogFormat::Decoded {
            return Ok(());
        }
        let mut complete = complete.into_iter().peekable();
        if complete.peek().is_none() && partial.is_empty() && !self.channels.contains_key(&channel)
        {
            return Ok(());
        }
        self.channel_log(channel, || ChannelState::Terminal {
            partial: Vec::new(),
        })?;
        self.write_tagged_lines(channel, complete)?;
        *self.channel_mut(channel).terminal_partial() = partial.to_vec();
        Ok(())
    }

    /// Writes decoded lines with optional channel tags and flushes.
    ///
    /// Shared by the terminal and defmt decoded paths; both hand over
    /// already-split lines including their trailing `\n`.
    fn write_tagged_lines<I, B>(&mut self, channel: usize, lines: I) -> Result<()>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut lines = lines.into_iter().peekable();
        if lines.peek().is_none() {
            return Ok(());
        }
        let include_channel = self.include_channel;
        {
            let file = self.file_for_channel(channel)?;
            for line in lines {
                if include_channel {
                    write!(file, "[ch{channel}] ")?;
                }
                file.write_all(line.as_ref())?;
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

        let mut ranges = Vec::new();
        let mut start = 0;
        while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
            let end = start + offset + 1;
            ranges.push(start..end);
            start = end;
        }
        if !ranges.is_empty() {
            self.write_tagged_lines(channel, ranges.iter().map(|range| &pending[range.clone()]))?;
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
                let line = log.state.partial_line();
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
            log.state.clear_partial();
        }
        self.flush_files()
    }
}

impl ChannelLog {
    fn terminal_partial(&mut self) -> &mut Vec<u8> {
        match &mut self.state {
            ChannelState::Terminal { partial } => partial,
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
