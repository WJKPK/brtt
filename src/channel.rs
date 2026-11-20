use anyhow::{Context, Result};
use probe_rs::rtt::RttChannel;

/// Index of a configured target core (`u32` in CLI/ELF, `usize` in probe-rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CoreId(u32);

impl CoreId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u32 {
        self.0
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for CoreId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Logical RTT channel number (distinct from slice position in probe-rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ChannelId(usize);

impl ChannelId {
    pub(crate) fn from_cli(value: u32, direction: &str) -> Result<Self> {
        usize::try_from(value).map(Self).with_context(|| {
            format!("{direction} channel index {value} cannot be represented on this host")
        })
    }

    pub(crate) fn value(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CoreChannel {
    pub(crate) core: CoreId,
    pub(crate) channel: ChannelId,
}

impl std::fmt::Display for CoreChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[c{}:ch{}]", self.core, self.channel.value())
    }
}

pub(crate) fn channel_by_number<T: RttChannel>(
    channels: &mut [T],
    channel: ChannelId,
) -> Option<&mut T> {
    channels
        .iter_mut()
        .find(|candidate| candidate.number() == channel.value())
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum ChannelEncoding {
    Terminal,
    Defmt,
}

impl ChannelEncoding {
    pub(crate) fn name(self) -> &'static str {
        match self {
            ChannelEncoding::Terminal => "terminal",
            ChannelEncoding::Defmt => "defmt",
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct ChannelSpec {
    /// `None` selects the channel on every configured core.
    pub(crate) core: Option<u32>,
    pub(crate) index: u32,
    pub(crate) mode: ChannelEncoding,
}

impl ChannelSpec {
    pub(crate) fn applies_to(&self, core: u32) -> bool {
        self.core.is_none_or(|c| c == core)
    }
}

pub(crate) struct SelectedSource {
    pub(crate) core: u32,
    pub(crate) channel: u32,
    pub(crate) mode: ChannelEncoding,
}

pub(crate) fn expand_sources(up_specs: &[ChannelSpec], cores: &[u32]) -> Vec<SelectedSource> {
    let mut sources = Vec::new();
    for spec in up_specs {
        for core in cores {
            if spec.applies_to(*core) {
                sources.push(SelectedSource {
                    core: *core,
                    channel: spec.index,
                    mode: spec.mode,
                });
            }
        }
    }
    sources
}
