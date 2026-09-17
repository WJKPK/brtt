use anyhow::{Context, Result};
use brtt::RttChannel;

/// Index of one configured target core.
///
/// The CLI and ELF mapping use `u32`, while probe-rs indexes cores with
/// `usize`. Keeping the logical identity typed prevents either representation
/// from leaking through the session model.
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

/// Index of an RTT up or down channel.
///
/// RTT channels carry their own numbers, which do not necessarily match their
/// position in the channel slice returned by probe-rs. Keeping the CLI value in
/// a typed wrapper ensures lookups use the channel number, never a slice index.
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

#[cfg(test)]
impl ChannelId {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ChannelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// One RTT channel on one target core. Display form `[c0:ch0]` names the
/// source in merged multi-core output; single-source sessions keep the
/// legacy `[ch0]` shape (see the `show_cores` flag at each tag site).
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

#[cfg(test)]
#[path = "../tests/channel.rs"]
mod tests;
