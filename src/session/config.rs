use crate::channel::{ChannelId, ChannelSpec};
use crate::probe::ProbeIdentity;
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

/// Validated, normalized display and input policy shared by every configured
/// core. Per-core discovery and defmt data belong to the target setup.
pub(crate) struct SessionPolicy {
    pub(crate) identity: ProbeIdentity,
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

/// Validated session settings, before the selected probe's identity is known.
pub(crate) struct SessionOptions {
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) down_channel: Option<ChannelId>,
    pub(crate) down_explicit: bool,
    pub(crate) poll_interval: Duration,
    pub(crate) timestamps: bool,
    pub(crate) color: ColorMode,
    pub(crate) log: Option<LogConfig>,
}

impl SessionOptions {
    pub(crate) fn with_identity(self, identity: ProbeIdentity) -> SessionPolicy {
        SessionPolicy {
            identity,
            up_specs: self.up_specs,
            down_channel: self.down_channel,
            down_explicit: self.down_explicit,
            poll_interval: self.poll_interval,
            timestamps: self.timestamps,
            color: self.color,
            log: self.log,
        }
    }
}
