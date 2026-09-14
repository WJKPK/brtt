use crate::cli::LogFormat;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

pub(crate) struct Logger {
    path: PathBuf,
    per_channel: bool,
    format: LogFormat,
    merged: Option<File>,
    channels: HashMap<u32, File>,
    streams: HashMap<u32, DecodedStream>,
    text_pending: HashMap<u32, (Vec<u8>, bool)>,
}

struct DecodedStream {
    line: Vec<Option<Cell>>,
    cursor: usize,
    parser: vte::Parser,
    complete: Vec<Vec<u8>>,
    include_channel: bool,
}

#[derive(Clone)]
struct Cell {
    character: char,
    combining: Option<String>,
}

const MAX_TERMINAL_COLUMNS: usize = 4096;

impl Logger {
    pub(crate) fn new(
        path: Option<&Path>,
        per_channel: bool,
        format: LogFormat,
        channel_count: usize,
    ) -> Result<Option<Self>> {
        let Some(path) = path else { return Ok(None) };
        if format == LogFormat::Raw && !per_channel && channel_count > 1 {
            bail!("--log-format raw with multiple up channels requires --log-per-channel");
        }
        let mut logger = Self {
            path: path.to_path_buf(),
            per_channel,
            format,
            merged: None,
            channels: HashMap::new(),
            streams: HashMap::new(),
            text_pending: HashMap::new(),
        };
        if !per_channel {
            logger.merged = Some(open_log(path)?);
        }
        Ok(Some(logger))
    }

    fn file_for_channel(&mut self, channel: u32) -> Result<&mut File> {
        if self.per_channel && !self.channels.contains_key(&channel) {
            let path = channel_path(&self.path, channel);
            self.channels.insert(channel, open_log(&path)?);
        }
        if self.per_channel {
            Ok(self.channels.get_mut(&channel).expect("file inserted"))
        } else {
            Ok(self.merged.as_mut().expect("merged file initialized"))
        }
    }

    pub(crate) fn write_raw(&mut self, channel: u32, bytes: &[u8]) -> Result<()> {
        if self.format != LogFormat::Raw || bytes.is_empty() {
            return Ok(());
        }
        self.file_for_channel(channel)?
            .write_all(bytes)
            .with_context(|| format!("writing raw log for channel {channel}"))?;
        Ok(())
    }

    pub(crate) fn write_terminal(
        &mut self,
        channel: u32,
        bytes: &[u8],
        include_channel: bool,
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        let stream = self
            .streams
            .entry(channel)
            .or_insert_with(|| DecodedStream {
                line: Vec::new(),
                cursor: 0,
                parser: vte::Parser::new(),
                complete: Vec::new(),
                include_channel,
            });
        let complete = stream.consume(bytes);
        let mut tagged = Vec::new();
        for line in complete {
            if stream.include_channel {
                tagged.extend_from_slice(format!("[ch{channel}] ").as_bytes());
            }
            tagged.extend_from_slice(&line);
        }
        if tagged.is_empty() {
            return Ok(());
        }
        self.file_for_channel(channel)?
            .write_all(&tagged)
            .with_context(|| format!("writing decoded log for channel {channel}"))?;
        Ok(())
    }

    pub(crate) fn write_text(
        &mut self,
        channel: u32,
        bytes: &[u8],
        include_channel: bool,
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        let (pending, include_channel) = self
            .text_pending
            .entry(channel)
            .or_insert_with(|| (Vec::new(), include_channel));
        pending.extend_from_slice(bytes);
        let mut complete = Vec::new();
        while let Some(position) = pending.iter().position(|&byte| byte == b'\n') {
            complete.push(pending.drain(..=position).collect::<Vec<_>>());
        }
        let mut tagged = Vec::new();
        for line in complete {
            if *include_channel {
                tagged.extend_from_slice(format!("[ch{channel}] ").as_bytes());
            }
            tagged.extend_from_slice(&line);
        }
        if !tagged.is_empty() {
            self.file_for_channel(channel)?
                .write_all(&tagged)
                .with_context(|| format!("writing decoded log for channel {channel}"))?;
        }
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> Result<()> {
        let stream_tails: Vec<_> = self
            .streams
            .iter()
            .filter(|&(&_channel, stream)| !stream.visible_line().is_empty())
            .map(|(&channel, stream)| (channel, stream.include_channel, stream.visible_line()))
            .collect();
        for (channel, include_channel, line) in stream_tails {
            let mut tail = Vec::new();
            if include_channel {
                tail.extend_from_slice(format!("[ch{channel}] ").as_bytes());
            }
            tail.extend(line);
            self.file_for_channel(channel)?.write_all(&tail)?;
        }
        let text_tails: Vec<_> = self
            .text_pending
            .iter()
            .filter(|&(&_channel, (bytes, _))| !bytes.is_empty())
            .map(|(&channel, (bytes, include_channel))| (channel, *include_channel, bytes.clone()))
            .collect();
        for (channel, include_channel, bytes) in text_tails {
            let mut tail = Vec::new();
            if include_channel {
                tail.extend_from_slice(format!("[ch{channel}] ").as_bytes());
            }
            tail.extend_from_slice(&bytes);
            self.file_for_channel(channel)?.write_all(&tail)?;
        }
        if let Some(file) = &mut self.merged {
            file.flush().context("flushing log file")?;
        }
        for file in self.channels.values_mut() {
            file.flush().context("flushing per-channel log file")?;
        }
        Ok(())
    }
}

impl DecodedStream {
    fn consume(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
        std::mem::take(&mut self.complete)
    }

    fn finish_line(&mut self) -> Vec<u8> {
        let mut line = self.visible_line();
        line.push(b'\n');
        self.line.clear();
        self.cursor = 0;
        line
    }

    fn visible_line(&self) -> Vec<u8> {
        let end = self
            .line
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |index| index + 1);
        let mut output = Vec::new();
        for cell in &self.line[..end] {
            match cell {
                Some(cell) => {
                    let mut buffer = [0; 4];
                    output.extend_from_slice(cell.character.encode_utf8(&mut buffer).as_bytes());
                    if let Some(combining) = &cell.combining {
                        output.extend_from_slice(combining.as_bytes());
                    }
                }
                None => output.push(b' '),
            }
        }
        output
    }
}

impl Perform for DecodedStream {
    fn print(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            let end = self.cursor.min(self.line.len());
            if let Some(cell) = self.line[..end].iter_mut().rev().flatten().next() {
                cell.combining
                    .get_or_insert_with(String::new)
                    .push(character);
            }
            return;
        }
        self.cursor = self.cursor.min(MAX_TERMINAL_COLUMNS);
        let end = self.cursor.saturating_add(width).min(MAX_TERMINAL_COLUMNS);
        if end <= self.cursor {
            return;
        }
        self.line.resize(end, None);
        self.line[self.cursor] = Some(Cell {
            character,
            combining: None,
        });
        self.line[self.cursor + 1..end].fill(None);
        self.cursor += width;
        self.cursor = self.cursor.min(MAX_TERMINAL_COLUMNS);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor = 0,
            b'\n' => {
                let line = self.finish_line();
                self.complete.push(line);
            }
            8 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => {
                self.cursor = self
                    .cursor
                    .saturating_add(8 - self.cursor % 8)
                    .min(MAX_TERMINAL_COLUMNS);
                self.line.resize(self.cursor, None);
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _: &[u8], _: bool, action: char) {
        let value = || {
            params
                .iter()
                .next()
                .and_then(|values| values.first().copied())
                .unwrap_or(1) as usize
        };
        match action {
            'K' => match value() {
                0 => self.line.truncate(self.cursor.min(self.line.len())),
                1 => {
                    let end = self.cursor.saturating_add(1).min(self.line.len());
                    self.line[..end].fill(None);
                }
                2 => self.line.clear(),
                _ => {}
            },
            'G' | '`' => self.cursor = value().saturating_sub(1).min(MAX_TERMINAL_COLUMNS),
            'C' => {
                self.cursor = self
                    .cursor
                    .saturating_add(value())
                    .min(MAX_TERMINAL_COLUMNS)
            }
            'D' => self.cursor = self.cursor.saturating_sub(value()),
            _ => {}
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

fn channel_path(path: &Path, channel: u32) -> PathBuf {
    let suffix = format!(".ch{channel}");
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("log");
        return path.with_file_name(format!("{name}{suffix}"));
    };
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("log");
    path.with_file_name(format!("{stem}{suffix}.{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brtt-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn channel_paths_insert_suffix_before_extension() {
        assert_eq!(
            channel_path(Path::new("capture.log"), 2),
            PathBuf::from("capture.ch2.log")
        );
        assert_eq!(
            channel_path(Path::new("capture"), 2),
            PathBuf::from("capture.ch2")
        );
        assert_eq!(
            channel_path(Path::new("logs/capture"), 2),
            PathBuf::from("logs/capture.ch2")
        );
    }

    #[test]
    fn merged_decoded_logs_are_channel_tagged() {
        let path = test_path("merged");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.write_text(0, b"one\n", true).unwrap();
        logger.write_text(1, b"two\n", true).unwrap();
        logger.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"[ch0] one\n[ch1] two\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn raw_merged_logs_reject_multiple_channels() {
        let path = test_path("raw");
        assert!(Logger::new(Some(&path), false, LogFormat::Raw, 2).is_err());
    }

    #[test]
    fn merged_decoded_logs_keep_partial_channels_separate() {
        let path = test_path("merged-partial");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.write_text(0, b"foo", true).unwrap();
        logger.write_text(1, b"bar\n", true).unwrap();
        logger.write_text(0, b"\n", true).unwrap();
        logger.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"[ch1] bar\n[ch0] foo\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn per_channel_raw_logs_preserve_bytes() {
        let path = test_path("raw-per-channel.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Raw, 2)
            .unwrap()
            .unwrap();
        logger.write_raw(1, &[0, 1, 0xff]).unwrap();
        logger.flush().unwrap();
        let channel_path = channel_path(&path, 1);
        assert_eq!(fs::read(&channel_path).unwrap(), &[0, 1, 0xff]);
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn per_channel_decoded_logs_do_not_need_channel_tags() {
        let path = test_path("decoded-per-channel.log");
        let mut logger = Logger::new(Some(&path), true, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.write_text(1, b"message\n", false).unwrap();
        logger.flush().unwrap();

        let channel_path = channel_path(&path, 1);
        assert_eq!(fs::read(&channel_path).unwrap(), b"message\n");
        fs::remove_file(channel_path).unwrap();
    }

    #[test]
    fn decoded_logger_flushes_an_unfinished_line() {
        let path = test_path("decoded-tail");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_text(0, b"unfinished", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"unfinished");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_collapses_terminal_redraws() {
        let path = test_path("decoded-redraw");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, b"\r\x1b[2K> help\r\x1b[2K> \r\x1b[2K> help\r\n", false)
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"> help\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_handles_escape_sequences_split_between_reads() {
        let path = test_path("decoded-split-escape");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_terminal(0, b"old\r\x1b[", false).unwrap();
        logger.write_terminal(0, b"2Knew\n", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decoded_logger_keeps_echoed_commands() {
        let path = test_path("decoded-command");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_terminal(0, b"> pwd\r\n", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"> pwd\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn plain_decoded_text_does_not_interpret_terminal_controls() {
        let path = test_path("decoded-plain-text");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_text(0, b"value: \x1b[2K\n", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"value: \x1b[2K\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_preserves_utf8_and_display_width() {
        let path = test_path("terminal-utf8");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_terminal(0, "ż界\n".as_bytes(), false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), "ż界\n".as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_keeps_combining_marks_with_their_base_character() {
        let path = test_path("terminal-combining");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger
            .write_terminal(0, "e\u{301}\n".as_bytes(), false)
            .unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), "e\u{301}\n".as_bytes());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_erases_the_cursor_cell_with_csi_one_k() {
        let path = test_path("terminal-erase-before");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_terminal(0, b"abc\x1b[1K\n", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"\n");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_logger_flush_preserves_parser_state() {
        let path = test_path("terminal-flush-state");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 1)
            .unwrap()
            .unwrap();
        logger.write_terminal(0, b"old\r\x1b[", false).unwrap();
        logger.flush().unwrap();
        logger.write_terminal(0, b"2Knew\n", false).unwrap();
        logger.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"oldnew\n");
        fs::remove_file(path).unwrap();
    }
}
