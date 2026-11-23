use super::{LogDestination, LogFormat};
use crate::channel::{CoreChannel, CoreId};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub(super) struct Logger {
    sink: LogSink,
    format: LogFormat,
    include_channel: bool,
    show_cores: bool,
    channels: HashMap<CoreChannel, ChannelLog>,
}

enum LogSink {
    Merged(BufWriter<File>),
    PerChannel(PathBuf),
}

struct ChannelLog {
    file: Option<BufWriter<File>>,
    line_assembly: Option<LineAssembly>,
}

/// Terminal output arrives pre-split from the VT decoder as complete lines
/// plus a trailing partial; defmt fragments are buffered until `\n`.
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
            Self::PreSplit { partial: slot } => {
                slot.clear();
                slot.extend_from_slice(partial);
            }
            Self::Buffered { .. } => {
                unreachable!("set_partial is only valid on pre-split line assembly")
            }
        }
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
    pub(super) fn new(
        destination: Option<&LogDestination>,
        format: LogFormat,
        include_channel: bool,
        show_cores: bool,
    ) -> Result<Option<Self>> {
        let Some(destination) = destination else {
            return Ok(None);
        };
        let per_channel = matches!(destination, LogDestination::PerChannel(_));
        if format == LogFormat::Raw && !per_channel && include_channel {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let sink = match destination {
            LogDestination::Merged(path) => LogSink::Merged(BufWriter::new(open_log(path)?)),
            LogDestination::PerChannel(path) => LogSink::PerChannel(path.clone()),
        };
        Ok(Some(Self {
            sink,
            format,
            include_channel: include_channel && !per_channel,
            show_cores,
            channels: HashMap::new(),
        }))
    }

    fn channel_log(
        &mut self,
        source: CoreChannel,
        line_assembly: Option<LineAssembly>,
    ) -> Result<&mut ChannelLog> {
        use std::collections::hash_map::Entry;
        match self.channels.entry(source) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let file = match &self.sink {
                    LogSink::PerChannel(path) => {
                        Some(BufWriter::new(open_log(&channel_path(path, source))?))
                    }
                    LogSink::Merged(_) => None,
                };
                Ok(entry.insert(ChannelLog {
                    file,
                    line_assembly,
                }))
            }
        }
    }

    fn file_for_channel(&mut self, source: CoreChannel) -> Result<&mut BufWriter<File>> {
        let Self { sink, channels, .. } = self;
        match sink {
            LogSink::Merged(file) => Ok(file),
            LogSink::PerChannel(_) => Ok(channels
                .get_mut(&source)
                .expect("channel log initialized")
                .file
                .as_mut()
                .expect("per-channel log file initialized")),
        }
    }

    pub(super) fn write_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.channel_log(source, None)?;
        self.file_for_channel(source)?
            .write_all(bytes)
            .with_context(|| format!("writing raw log for {source}"))?;
        Ok(())
    }

    /// Appends VT-decoded terminal lines and caches `partial` for `flush`/`reset`.
    pub(super) fn write_terminal_decoded<I, B>(
        &mut self,
        source: CoreChannel,
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
        if complete.peek().is_none() && partial.is_empty() && !self.channels.contains_key(&source) {
            return Ok(());
        }
        self.channel_log(source, Some(LineAssembly::pre_split()))?
            .assembly()
            .set_partial(partial);
        self.write_tagged_lines(source, complete)?;
        Ok(())
    }

    /// Writes decoded lines with optional channel tags without flushing; files
    /// are flushed once per tick.
    fn write_tagged_lines<I, B>(&mut self, source: CoreChannel, lines: I) -> Result<()>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut lines = lines.into_iter().peekable();
        if lines.peek().is_none() {
            return Ok(());
        }
        let include_channel = self.include_channel;
        let show_cores = self.show_cores;
        let file = self.file_for_channel(source)?;
        for line in lines {
            if include_channel {
                if show_cores {
                    write!(file, "{source} ")?;
                } else {
                    write!(file, "[ch{}] ", source.channel)?;
                }
            }
            file.write_all(line.as_ref())?;
        }
        Ok(())
    }

    pub(super) fn write_defmt_decoded(
        &mut self,
        source: CoreChannel,
        fragment: &[u8],
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || fragment.is_empty() {
            return Ok(());
        }
        self.channel_log(source, Some(LineAssembly::buffered()))?;
        let include_channel = self.include_channel;
        let show_cores = self.show_cores;
        let Self { sink, channels, .. } = self;
        let channel_log = channels.get_mut(&source).expect("channel log initialized");
        let ChannelLog {
            file,
            line_assembly,
        } = channel_log;
        let Some(LineAssembly::Buffered { pending }) = line_assembly else {
            unreachable!("defmt decoded channel uses buffered line assembly")
        };
        pending.extend_from_slice(fragment);
        let mut start = 0;
        while let Some(offset) = memchr::memchr(b'\n', &pending[start..]) {
            let end = start + offset + 1;
            write_defmt_line(
                sink,
                file,
                source,
                include_channel,
                show_cores,
                &pending[start..end],
            )?;
            start = end;
        }
        pending.drain(..start);
        Ok(())
    }

    fn partial_lines(&self) -> Vec<(CoreChannel, Vec<u8>)> {
        let mut lines: Vec<_> = self
            .channels
            .iter()
            .filter_map(|(&source, log)| {
                let line = log
                    .line_assembly
                    .as_ref()
                    .map(LineAssembly::partial_line)
                    .unwrap_or_default();
                (!line.is_empty()).then_some((source, line))
            })
            .collect();
        lines.sort_unstable_by_key(|(source, _)| *source);
        lines
    }

    fn write_tails(&mut self, tails: &[(CoreChannel, Vec<u8>)], newline: bool) -> Result<()> {
        let include_channel = self.include_channel;
        let show_cores = self.show_cores;
        for (source, line) in tails {
            let file = self.file_for_channel(*source)?;
            if include_channel {
                if show_cores {
                    write!(file, "{source} ")?;
                } else {
                    write!(file, "[ch{}] ", source.channel)?;
                }
            }
            file.write_all(line)?;
            if newline {
                file.write_all(b"\n")?;
            }
        }
        Ok(())
    }

    pub(super) fn flush(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, false)?;
        self.flush_files()
    }

    pub(super) fn flush_files(&mut self) -> Result<()> {
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

    /// Finalizes partial lines with a newline and clears line-assembly state
    /// across a target restart.
    pub(super) fn reset(&mut self) -> Result<()> {
        let tails = self.partial_lines();
        self.write_tails(&tails, true)?;
        for log in self.channels.values_mut() {
            if let Some(assembly) = &mut log.line_assembly {
                assembly.clear_partial();
            }
        }
        self.flush_files()
    }

    pub(super) fn reset_core(&mut self, core: CoreId) -> Result<()> {
        let tails: Vec<_> = self
            .partial_lines()
            .into_iter()
            .filter(|(source, _)| source.core == core)
            .collect();
        self.write_tails(&tails, true)?;
        for (source, log) in self.channels.iter_mut() {
            if source.core == core {
                if let Some(assembly) = &mut log.line_assembly {
                    assembly.clear_partial();
                }
            }
        }
        self.flush_files()
    }
}

fn write_defmt_line(
    sink: &mut LogSink,
    channel_file: &mut Option<BufWriter<File>>,
    source: CoreChannel,
    include_channel: bool,
    show_cores: bool,
    line: &[u8],
) -> Result<()> {
    let file = match sink {
        LogSink::Merged(file) => file,
        LogSink::PerChannel(_) => channel_file
            .as_mut()
            .expect("per-channel log file initialized"),
    };
    if include_channel {
        if show_cores {
            write!(file, "{source} ")?;
        } else {
            write!(file, "[ch{}] ", source.channel)?;
        }
    }
    file.write_all(line)?;
    Ok(())
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

fn channel_path(path: &Path, source: CoreChannel) -> PathBuf {
    let mut name = path
        .file_stem()
        .map(|stem| stem.to_os_string())
        .unwrap_or_else(|| OsString::from("log"));
    name.push(format!(".c{}.ch{}", source.core, source.channel));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}
