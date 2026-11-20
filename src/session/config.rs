use crate::channel::{ChannelId, ChannelSpec};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogFormat {
    Raw,
    Decoded,
}

pub(crate) struct LogConfig {
    pub(crate) destination: LogDestination,
    pub(crate) format: LogFormat,
}

pub(crate) enum LogDestination {
    Merged(PathBuf),
    PerChannel(PathBuf),
}

pub(crate) struct SessionPolicy {
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) down_channel: Option<ChannelId>,
    /// Whether `--down` was passed explicitly (as opposed to the default).
    /// Decides if a missing down channel is an error or just disables input.
    pub(crate) down_explicit: bool,
    pub(crate) poll_interval: Duration,
    pub(crate) timestamps: bool,
    pub(crate) color: ColorMode,
    pub(crate) log: Option<LogConfig>,
}
