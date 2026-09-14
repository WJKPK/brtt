use crate::cli::LogFormat;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) struct Logger {
    path: PathBuf,
    per_channel: bool,
    format: LogFormat,
    merged: Option<File>,
    channels: HashMap<u32, File>,
    line_start: HashMap<u32, bool>,
}

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
            line_start: HashMap::new(),
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

    pub(crate) fn write_decoded(
        &mut self,
        channel: u32,
        bytes: &[u8],
        include_channel: bool,
    ) -> Result<()> {
        if self.format != LogFormat::Decoded || bytes.is_empty() {
            return Ok(());
        }
        let key = if self.per_channel { channel } else { u32::MAX };
        let mut line_start = *self.line_start.get(&key).unwrap_or(&true);
        let mut tagged = Vec::with_capacity(bytes.len() + 12);
        for &byte in bytes {
            if line_start && include_channel {
                tagged.extend_from_slice(format!("[ch{channel}] ").as_bytes());
            }
            tagged.push(byte);
            line_start = byte == b'\n';
        }
        self.line_start.insert(key, line_start);
        self.file_for_channel(channel)?
            .write_all(&tagged)
            .with_context(|| format!("writing decoded log for channel {channel}"))?;
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> Result<()> {
        if let Some(file) = &mut self.merged {
            file.flush().context("flushing log file")?;
        }
        for file in self.channels.values_mut() {
            file.flush().context("flushing per-channel log file")?;
        }
        Ok(())
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
        return path.with_file_name(format!("{}{}", path.display(), suffix));
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
    }

    #[test]
    fn merged_decoded_logs_are_channel_tagged() {
        let path = test_path("merged");
        let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, 2)
            .unwrap()
            .unwrap();
        logger.write_decoded(0, b"one\n", true).unwrap();
        logger.write_decoded(1, b"two\n", true).unwrap();
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
}
