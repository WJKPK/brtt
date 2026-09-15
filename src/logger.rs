use crate::cli::{LogDestination, LogFormat};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub(crate) struct Logger {
    path: PathBuf,
    sink: LogSink,
    format: LogFormat,
    include_channel: bool,
    channels: HashMap<usize, ChannelLog>,
}

/// Where log output goes. A single source of truth: either one merged file
/// or one lazily-created file per channel.
enum LogSink {
    Merged(BufWriter<File>),
    PerChannel,
}

/// Log sink and decoding state for one RTT up channel.
struct ChannelLog {
    /// Per-channel file from `--log-per-channel`; `None` while merged.
    file: Option<BufWriter<File>>,
    /// Line assembly state; `None` for raw channels, which need no decoding.
    line_assembly: Option<LineAssembly>,
}

/// How one channel assembles log lines.
///
/// VT-decoded terminal output arrives with line boundaries already resolved
/// upstream (target.rs decodes once and hands over complete lines plus the
/// trailing partial), so Logger only caches the partial. Defmt frame text
/// has no such guarantee and must be re-split here on `\n`.
enum LineAssembly {
    PreSplit { partial: Vec<u8> },
    Buffered { pending: Vec<u8> },
}

impl LineAssembly {
    fn pre_split() -> Self {
        Self::PreSplit {
            partial: Vec::new(),
        }
    }

    fn buffered() -> Self {
        Self::Buffered {
            pending: Vec::new(),
        }
    }

    fn set_partial(&mut self, partial: &[u8]) {
        match self {
            Self::PreSplit { partial: slot } => *slot = partial.to_vec(),
            Self::Buffered { .. } => {
                unreachable!("set_partial is only valid on pre-split line assembly")
            }
        }
    }

    /// Appends a raw fragment and returns newly-completed lines, each
    /// including its trailing `\n`. The remainder stays buffered.
    fn ingest_fragment(&mut self, fragment: &[u8]) -> Vec<Vec<u8>> {
        let Self::Buffered { pending } = self else {
            unreachable!("ingest_fragment is only valid on buffered line assembly")
        };
        pending.extend_from_slice(fragment);

        let mut lines = Vec::new();
        let mut start = 0;
        while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
            let end = start + offset + 1;
            lines.push(pending[start..end].to_vec());
            start = end;
        }
        pending.drain(..start);
        lines
    }

    fn partial_line(&self) -> Vec<u8> {
        match self {
            Self::PreSplit { partial } => partial.clone(),
            Self::Buffered { pending } => pending.clone(),
        }
    }

    fn clear_partial(&mut self) {
        match self {
            Self::PreSplit { partial } => partial.clear(),
            Self::Buffered { pending } => pending.clear(),
        }
    }
}

impl Logger {
    pub(crate) fn new(
        destination: Option<&LogDestination>,
        format: LogFormat,
        include_channel: bool,
    ) -> Result<Option<Self>> {
        let Some(destination) = destination else {
            return Ok(None);
        };
        let per_channel = matches!(destination, LogDestination::PerChannel(_));
        if format == LogFormat::Raw && !per_channel && include_channel {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let (path, sink) = match destination {
            LogDestination::Merged(path) => {
                (path, LogSink::Merged(BufWriter::new(open_log(path)?)))
            }
            LogDestination::PerChannel(path) => (path, LogSink::PerChannel),
        };
        Ok(Some(Self {
            path: path.to_path_buf(),
            sink,
            format,
            include_channel: include_channel && !per_channel,
            channels: HashMap::new(),
        }))
    }

    fn channel_log(
        &mut self,
        channel: usize,
        line_assembly: Option<LineAssembly>,
    ) -> Result<&mut ChannelLog> {
        use std::collections::hash_map::Entry;
        match self.channels.entry(channel) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let file = if matches!(self.sink, LogSink::PerChannel) {
                    Some(BufWriter::new(open_log(&channel_path(
                        &self.path, channel,
                    ))?))
                } else {
                    None
                };
                Ok(entry.insert(ChannelLog {
                    file,
                    line_assembly,
                }))
            }
        }
    }

    fn file_for_channel(&mut self, channel: usize) -> Result<&mut BufWriter<File>> {
        let Self { sink, channels, .. } = self;
        match sink {
            LogSink::Merged(file) => Ok(file),
            LogSink::PerChannel => Ok(channels
                .get_mut(&channel)
                .expect("channel log initialized")
                .file
                .as_mut()
                .expect("per-channel log file initialized")),
        }
    }

    /// Append exact RTT bytes; raw log mode only.
    pub(crate) fn write_bytes(&mut self, channel: usize, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(channel, None)?;
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
        self.channel_log(channel, Some(LineAssembly::pre_split()))?
            .assembly()
            .set_partial(partial);
        self.write_tagged_lines(channel, complete)?;
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
        let lines = self
            .channel_log(channel, Some(LineAssembly::buffered()))?
            .assembly()
            .ingest_fragment(line);
        if !lines.is_empty() {
            self.write_tagged_lines(channel, lines)?;
        }
        Ok(())
    }

    fn partial_lines(&self) -> Vec<(usize, Vec<u8>)> {
        let mut lines: Vec<_> = self
            .channels
            .iter()
            .filter_map(|(&channel, log)| {
                let line = log
                    .line_assembly
                    .as_ref()
                    .map(LineAssembly::partial_line)
                    .unwrap_or_default();
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
        if let LogSink::Merged(file) = &mut self.sink {
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
            if let Some(assembly) = &mut log.line_assembly {
                assembly.clear_partial();
            }
        }
        self.flush_files()
    }
}

impl ChannelLog {
    fn assembly(&mut self) -> &mut LineAssembly {
        self.line_assembly
            .as_mut()
            .expect("channel line assembly initialized")
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
