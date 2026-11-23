use probe_rs::rtt::RttChannel;

/// Index of a configured target core (`u32` in CLI/ELF, `usize` in probe-rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CoreId(u32);

impl CoreId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

impl std::fmt::Display for CoreId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for CoreId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u32>()
            .map(Self::new)
            .map_err(|_| format!("invalid core index '{value}', expected a u32"))
    }
}

/// Logical RTT channel number (distinct from slice position in probe-rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ChannelId(u32);

impl ChannelId {
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    pub(crate) const fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub(crate) const fn as_u64(self) -> u64 {
        self.0 as u64
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for ChannelId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<u32>()
            .map(Self::new)
            .map_err(|_| format!("invalid channel index '{value}', expected a u32"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CoreChannel {
    pub(crate) core: CoreId,
    pub(crate) channel: ChannelId,
}

impl std::fmt::Display for CoreChannel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "[c{}:ch{}]", self.core, self.channel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceLabel {
    None,
    ChannelOnly,
    CoreAndChannel,
}

impl SourceLabel {
    pub(crate) const fn for_session(source_count: usize, core_count: usize) -> Self {
        if source_count <= 1 {
            Self::None
        } else if core_count > 1 {
            Self::CoreAndChannel
        } else {
            Self::ChannelOnly
        }
    }

    pub(crate) const fn is_enabled(self) -> bool {
        !matches!(self, Self::None)
    }

    pub(crate) const fn without_per_channel(self, per_channel: bool) -> Self {
        if per_channel {
            Self::None
        } else {
            self
        }
    }

    pub(crate) const fn display(self, source: CoreChannel) -> SourceLabelDisplay {
        SourceLabelDisplay {
            label: self,
            source,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceLabelDisplay {
    label: SourceLabel,
    source: CoreChannel,
}

impl std::fmt::Display for SourceLabelDisplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.label {
            SourceLabel::None => Ok(()),
            SourceLabel::ChannelOnly => write!(formatter, "[ch{}] ", self.source.channel),
            SourceLabel::CoreAndChannel => write!(formatter, "{} ", self.source),
        }
    }
}

pub(crate) fn channel_by_number<T: RttChannel>(
    channels: &mut [T],
    channel: ChannelId,
) -> Option<&mut T> {
    channels
        .iter_mut()
        .find(|candidate| candidate.number() == channel.as_usize())
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
    pub(crate) core: Option<CoreId>,
    pub(crate) channel: ChannelId,
    pub(crate) mode: ChannelEncoding,
}

impl ChannelSpec {
    pub(crate) fn applies_to(&self, core: CoreId) -> bool {
        self.core.is_none_or(|c| c == core)
    }
}

pub(crate) struct SelectedSource {
    pub(crate) core: CoreId,
    pub(crate) channel: ChannelId,
    pub(crate) mode: ChannelEncoding,
}

pub(crate) fn expand_sources(up_specs: &[ChannelSpec], cores: &[CoreId]) -> Vec<SelectedSource> {
    let mut sources = Vec::new();
    for spec in up_specs {
        for core in cores {
            if spec.applies_to(*core) {
                sources.push(SelectedSource {
                    core: *core,
                    channel: spec.channel,
                    mode: spec.mode,
                });
            }
        }
    }
    sources
}
