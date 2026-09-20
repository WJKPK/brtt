use crate::channel::{channel_by_number, ChannelId, CoreChannel, CoreId};
use crate::cli::{ChannelEncoding, ChannelSpec, SessionPolicy};
use crate::defmt::{
    decode_frames, DecodeOutput, DecodedFrame, DefmtData, MAX_DECODE_BUFFERED_BYTES,
};
use crate::terminal::{DecodedStream, TerminalChunk};
use anyhow::{anyhow, bail, Context, Result};
use brtt::rtt::{Error as RttError, Rtt, RttDiscovery, ScanRegion};
use probe_rs::{Core, Session as ProbeSession};
use std::time::{Duration, Instant};

pub(crate) const RTT_TIMEOUT: Duration = Duration::from_secs(3);
/// Single-attempt reattach timeout: probe-rs tries once, then returns instead
/// of retrying until the timeout expires.
pub(crate) const RTT_RETRY_TIMEOUT: Duration = Duration::ZERO;
/// Minimum gap between reattach attempts so a pending core never stalls the
/// healthy cores' polling loop.
pub(crate) const RTT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const TARGET_HALT_TIMEOUT: Duration = Duration::from_millis(100);
const UP_CHANNEL_BUFFER_SIZE: usize = 4 * 1024;
const MAX_RTT_READS_PER_POLL: usize = 16;
const RTT_READ_BUDGET_PER_POLL: usize = UP_CHANNEL_BUFFER_SIZE * MAX_RTT_READS_PER_POLL;

/// Per-core information derived from its ELF and target configuration.
///
/// It remains outside `CoreSlot`: a defmt stream decoder borrows the table,
/// and making the slot own both would create a self-referential structure.
pub(crate) struct CoreSetup {
    pub(crate) discovery: RttDiscovery,
    pub(crate) defmt: Option<DefmtData>,
}

/// Output port used by per-core RTT I/O. Borrowed frames keep the hot poll path
/// allocation-free and prevent target I/O from depending on the renderer.
pub(crate) trait UpSink {
    fn raw_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()>;
    fn terminal(&mut self, source: CoreChannel, chunk: TerminalChunk, at: Instant) -> Result<()>;
    fn defmt_frame(&mut self, source: CoreChannel, frame: &DecodedFrame, at: Instant)
        -> Result<()>;
    fn defmt_warning(&mut self, source: CoreChannel, warning: &str, at: Instant) -> Result<()>;
    fn is_interactive(&self) -> bool;
}

struct UpChannelReader<'table> {
    source: CoreChannel,
    buffer: Vec<u8>,
    decoder: ChannelDecoder<'table>,
}

enum ChannelDecoder<'table> {
    Terminal(Box<DecodedStream>),
    Defmt {
        data: &'table DefmtData,
        stream: Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>,
        bytes_since_restart: usize,
    },
}

impl<'table> UpChannelReader<'table> {
    fn new(spec: ChannelSpec, core: CoreId, defmt: Option<&'table DefmtData>) -> Result<Self> {
        let channel = ChannelId::from_cli(spec.index, "up")?;
        let decoder = match (spec.mode, defmt) {
            (ChannelEncoding::Terminal, _) => {
                ChannelDecoder::Terminal(Box::new(DecodedStream::new()))
            }
            (ChannelEncoding::Defmt, Some(data)) => ChannelDecoder::Defmt {
                data,
                stream: data.table.new_stream_decoder(),
                bytes_since_restart: 0,
            },
            (ChannelEncoding::Defmt, None) => {
                bail!("missing defmt table for channel {}", spec.index)
            }
        };
        Ok(Self {
            source: CoreChannel { core, channel },
            buffer: vec![0; UP_CHANNEL_BUFFER_SIZE],
            decoder,
        })
    }
}

impl ChannelDecoder<'_> {
    fn process_defmt<S: UpSink>(
        &mut self,
        source: CoreChannel,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<bool> {
        let Self::Defmt {
            data,
            stream,
            bytes_since_restart,
        } = self
        else {
            return Ok(false);
        };

        if !bytes.is_empty()
            && bytes_since_restart.saturating_add(bytes.len()) > MAX_DECODE_BUFFERED_BYTES
        {
            *stream = data.table.new_stream_decoder();
            *bytes_since_restart = 0;
            sink.defmt_warning(
                source,
                "defmt decoder input exceeded 64 KiB; resetting decoder",
                Instant::now(),
            )?;
        }

        let decoded = decode_frames(stream.as_mut(), bytes, data.table.encoding().can_recover());
        for item in decoded.frames {
            match item {
                DecodeOutput::Frame(frame) => sink.defmt_frame(source, &frame, Instant::now())?,
                DecodeOutput::Warning(warning) => {
                    sink.defmt_warning(source, &warning, Instant::now())?
                }
            }
        }

        if decoded.restart {
            *stream = data.table.new_stream_decoder();
            *bytes_since_restart = 0;
        } else {
            *bytes_since_restart = bytes_since_restart.saturating_add(bytes.len());
        }
        Ok(decoded.hit_frame_limit)
    }
}

/// Outcome of one RTT attach attempt with the failure already classified.
pub(crate) enum AttachOutcome {
    Attached(Rtt),
    Retryable(String),
    Fatal(anyhow::Error),
}

pub(crate) fn attach_rtt_classified(
    session: &mut ProbeSession,
    id: CoreId,
    discovery: &RttDiscovery,
    timeout: Duration,
) -> Result<AttachOutcome> {
    let core_count = session.target().cores.len();
    if id.as_usize() >= core_count {
        bail!(
            "--elf index {id} out of range: target '{}' has {core_count} core(s)",
            session.target().name
        );
    }
    let mut core = match session.core(id.as_usize()) {
        Ok(core) => core,
        Err(error) => {
            return Ok(AttachOutcome::Retryable(format!(
                "core {id} unavailable ({error:#})"
            )));
        }
    };
    if let Err(error) = ensure_rtt_compatible_target(core.is_64_bit()) {
        return Ok(AttachOutcome::Fatal(error));
    }
    match discovery.attach(&mut core, timeout) {
        Ok(rtt) => {
            log::info!("core {id}: found control block at {:#010x}", rtt.ptr());
            resume_if_halted(&mut core, id)?;
            Ok(AttachOutcome::Attached(rtt))
        }
        Err(error) => Ok(classify_attach_error(id, error)),
    }
}

fn classify_attach_error(id: CoreId, error: RttError) -> AttachOutcome {
    match error {
        RttError::ControlBlockNotFound | RttError::ControlBlockCorrupted(_) => {
            AttachOutcome::Retryable(format!("core {id} RTT unavailable ({error:#})"))
        }
        RttError::Probe(_) | RttError::MemoryRead(_) | RttError::Other(_) => {
            AttachOutcome::Retryable(format!("core {id} attach transport error ({error:#})"))
        }
        permanent => {
            AttachOutcome::Fatal(anyhow!("core {id}: error attaching to RTT ({permanent:#})"))
        }
    }
}

fn ensure_rtt_compatible_target(is_64_bit: bool) -> Result<()> {
    if is_64_bit {
        bail!("64-bit targets are not supported until probe-rs fixes 32-bit RTT offset writes on 64-bit targets");
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct PollStats {
    pub(crate) bytes: usize,
    pub(crate) messages: usize,
}

/// State changes which must be applied to the renderer before any later core
/// can render during the same tick.
pub(crate) enum CoreEvent {
    Attached { id: CoreId, recovered: bool },
    RttBlockChanged { id: CoreId },
}

/// Bidirectional RTT I/O for one attached core, excluding the transient
/// probe-rs `Core` handle.
pub(crate) struct TargetIo<'setup> {
    rtt: Rtt,
    id: CoreId,
    down_channel: Option<ChannelId>,
    down_present: bool,
    readers: Vec<UpChannelReader<'setup>>,
}

struct CoreSlot<'setup> {
    id: CoreId,
    setup: &'setup CoreSetup,
    state: TargetState<'setup>,
}

enum TargetState<'setup> {
    Pending {
        next_retry: Instant,
        ever_attached: bool,
    },
    Attached(TargetIo<'setup>),
}

pub(crate) struct CoreSlots<'setup> {
    slots: Vec<CoreSlot<'setup>>,
}

pub(crate) struct Startup {
    attached: usize,
    pending: Vec<(CoreId, String)>,
}

impl Startup {
    pub(crate) fn attached_count(&self) -> usize {
        self.attached
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(crate) fn error(&self) -> anyhow::Error {
        let detail = self
            .pending
            .iter()
            .map(|(_, reason)| reason.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        anyhow!("no RTT targets could be opened ({detail}); ensure the cores are running and the ELF addresses match the firmware")
    }
}

impl<'setup> CoreSlot<'setup> {
    fn new(id: CoreId, setup: &'setup CoreSetup) -> Self {
        Self {
            id,
            setup,
            state: TargetState::Pending {
                next_retry: Instant::now(),
                ever_attached: false,
            },
        }
    }

    fn is_relevant(id: CoreId, policy: &SessionPolicy) -> bool {
        policy
            .up_specs
            .iter()
            .any(|spec| spec.applies_to(id.value()))
            || policy.down_channel.is_some()
    }

    fn id(&self) -> CoreId {
        self.id
    }

    fn is_attached(&self) -> bool {
        matches!(self.state, TargetState::Attached(_))
    }

    fn is_due(&self, now: Instant) -> bool {
        matches!(&self.state, TargetState::Pending { next_retry, .. } if now >= *next_retry)
    }

    fn defer_retry(&mut self, now: Instant) {
        let ever_attached = match self.state {
            TargetState::Pending { ever_attached, .. } => ever_attached,
            TargetState::Attached(_) => true,
        };
        self.state = TargetState::Pending {
            next_retry: now + RTT_RETRY_INTERVAL,
            ever_attached,
        };
    }

    fn lose_attachment(&mut self, now: Instant) {
        self.state = TargetState::Pending {
            next_retry: now + RTT_RETRY_INTERVAL,
            ever_attached: true,
        };
    }

    fn try_attach(
        &mut self,
        session: &mut ProbeSession,
        policy: &SessionPolicy,
        timeout: Duration,
    ) -> Result<SlotAttach> {
        let recovered = match self.state {
            TargetState::Attached(_) => return Ok(SlotAttach::AlreadyAttached),
            TargetState::Pending { ever_attached, .. } => ever_attached,
        };
        match attach_rtt_classified(session, self.id, &self.setup.discovery, timeout)? {
            AttachOutcome::Attached(rtt) => {
                self.state =
                    TargetState::Attached(TargetIo::new(rtt, self.id, self.setup, policy)?);
                Ok(SlotAttach::Attached { recovered })
            }
            AttachOutcome::Retryable(reason) => Ok(SlotAttach::Retryable(reason)),
            AttachOutcome::Fatal(error) => Err(error),
        }
    }

    fn poll<S: UpSink>(
        &mut self,
        session: &mut ProbeSession,
        sink: &mut S,
        now: Instant,
    ) -> Result<SlotPoll> {
        let mut core = match session.core(self.id.as_usize()) {
            Ok(core) => core,
            Err(error) => {
                log::warn!(
                    "core {} unavailable ({error:#}); continuing with remaining cores",
                    self.id
                );
                self.lose_attachment(now);
                return Ok(SlotPoll::Idle);
            }
        };
        let result = match &mut self.state {
            TargetState::Attached(target) => target.poll(&mut core, sink),
            TargetState::Pending { .. } => return Ok(SlotPoll::Idle),
        }?;
        match result {
            TargetPoll::Data(stats) => Ok(SlotPoll::Data(stats)),
            TargetPoll::Reattach => {
                self.lose_attachment(now);
                Ok(SlotPoll::Reattach)
            }
        }
    }

    fn down_present(&self) -> bool {
        matches!(&self.state, TargetState::Attached(target) if target.down_present())
    }

    fn rtt_ptr(&self) -> Option<u64> {
        match &self.state {
            TargetState::Attached(target) => Some(target.rtt_ptr()),
            TargetState::Pending { .. } => None,
        }
    }

    fn write_down(&mut self, core: &mut Core, bytes: &[u8]) -> Result<usize> {
        match &mut self.state {
            TargetState::Attached(target) => target.write_down(core, bytes),
            TargetState::Pending { .. } => Ok(0),
        }
    }
}

enum SlotAttach {
    AlreadyAttached,
    Attached { recovered: bool },
    Retryable(String),
}

enum SlotPoll {
    Idle,
    Data(PollStats),
    Reattach,
}

impl<'setup> CoreSlots<'setup> {
    pub(crate) fn build(
        setups: &'setup [(CoreId, CoreSetup)],
        policy: &SessionPolicy,
    ) -> Result<Self> {
        let slots: Vec<_> = setups
            .iter()
            .filter(|(id, _)| CoreSlot::is_relevant(*id, policy))
            .map(|(id, setup)| CoreSlot::new(*id, setup))
            .collect();
        if slots.is_empty() {
            bail!("no configured core selects any up channel and down input is disabled");
        }
        Ok(Self { slots })
    }

    pub(crate) fn attach_all(
        &mut self,
        session: &mut ProbeSession,
        policy: &SessionPolicy,
        timeout: Duration,
    ) -> Result<Startup> {
        let mut attached = 0;
        let mut pending = Vec::new();
        for slot in &mut self.slots {
            match slot.try_attach(session, policy, timeout) {
                Ok(SlotAttach::AlreadyAttached | SlotAttach::Attached { .. }) => attached += 1,
                Ok(SlotAttach::Retryable(reason)) => {
                    log::warn!("{reason}; continuing without it");
                    slot.defer_retry(Instant::now());
                    pending.push((slot.id(), reason));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("core {} failed to attach", slot.id()))
                }
            }
        }
        Ok(Startup { attached, pending })
    }

    /// Resets through the lowest accessible configured core. RTT attachment is
    /// not required: stale blocks attached so far are cleared best-effort,
    /// otherwise the target reset itself invalidates RAM.
    pub(crate) fn chip_reset(&mut self, session: &mut ProbeSession) -> Result<()> {
        let mut ids: Vec<CoreId> = self.slots.iter().map(CoreSlot::id).collect();
        ids.sort_unstable();
        ids.dedup();
        let mut first_error = None;
        let mut initiator = None;
        for id in ids {
            match session.core(id.as_usize()) {
                Ok(core) => {
                    drop(core);
                    initiator = Some(id);
                    break;
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        let Some(id) = initiator else {
            match first_error {
                Some(error) => {
                    return Err(error).context(
                        "cannot reset target: no configured core is currently accessible",
                    );
                }
                None => bail!("cannot reset target: no configured core is currently accessible"),
            }
        };
        let rtt_ptrs: Vec<u64> = self.slots.iter().filter_map(CoreSlot::rtt_ptr).collect();
        {
            let mut core = session.core(id.as_usize())?;
            reset_device(&mut core, id, &rtt_ptrs)?;
        }
        for slot in self.slots.iter().filter(|slot| slot.id() != id) {
            let other = slot.id();
            let Ok(mut core) = session.core(other.as_usize()) else {
                continue;
            };
            if let Err(error) = resume_if_halted(&mut core, other) {
                log::warn!("core {other}: could not resume after reset ({error:#})");
            }
        }
        let now = Instant::now();
        for slot in &mut self.slots {
            slot.defer_retry(now);
        }
        log::info!("target reset issued via core {id}; reattaching cores");
        Ok(())
    }

    pub(crate) fn maintain<S, F>(
        &mut self,
        session: &mut ProbeSession,
        now: Instant,
        policy: &SessionPolicy,
        sink: &mut S,
        mut on_event: F,
    ) -> Result<PollStats>
    where
        S: UpSink,
        F: FnMut(&mut S, CoreEvent) -> Result<()>,
    {
        for slot in &mut self.slots {
            if slot.is_attached() || !slot.is_due(now) {
                continue;
            }
            match slot.try_attach(session, policy, RTT_RETRY_TIMEOUT)? {
                SlotAttach::AlreadyAttached => {
                    unreachable!("pending slot cannot already be attached")
                }
                SlotAttach::Attached { recovered } => {
                    let id = slot.id();
                    on_event(sink, CoreEvent::Attached { id, recovered })?;
                }
                SlotAttach::Retryable(reason) => {
                    log::trace!("{reason}; retrying");
                    slot.defer_retry(now);
                }
            }
        }

        let mut stats = PollStats::default();
        for slot in &mut self.slots {
            if !slot.is_attached() {
                continue;
            }
            let id = slot.id();
            match slot.poll(session, sink, now)? {
                SlotPoll::Idle => {}
                SlotPoll::Data(data) => {
                    stats.bytes += data.bytes;
                    stats.messages += data.messages;
                }
                SlotPoll::Reattach => {
                    log::info!("core {id} RTT block changed; reattaching");
                    on_event(sink, CoreEvent::RttBlockChanged { id })?;
                }
            }
        }
        Ok(stats)
    }

    pub(crate) fn ids(&self) -> Vec<CoreId> {
        self.slots.iter().map(CoreSlot::id).collect()
    }

    pub(crate) fn all_attached(&self) -> bool {
        self.slots.iter().all(CoreSlot::is_attached)
    }

    pub(crate) fn is_attached(&self, id: CoreId) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.id() == id && slot.is_attached())
    }

    pub(crate) fn routable_down(&self) -> Vec<CoreId> {
        self.slots
            .iter()
            .filter(|slot| slot.down_present())
            .map(CoreSlot::id)
            .collect()
    }

    pub(crate) fn missing_down(&self) -> Vec<CoreId> {
        self.slots
            .iter()
            .filter(|slot| slot.is_attached() && !slot.down_present())
            .map(CoreSlot::id)
            .collect()
    }

    pub(crate) fn write_down(
        &mut self,
        session: &mut ProbeSession,
        id: CoreId,
        bytes: &[u8],
    ) -> Result<usize> {
        let mut core = match session.core(id.as_usize()) {
            Ok(core) => core,
            Err(_) => return Ok(0),
        };
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id() == id) else {
            return Ok(0);
        };
        slot.write_down(&mut core, bytes)
    }
}

impl<'setup> TargetIo<'setup> {
    fn new(rtt: Rtt, id: CoreId, setup: &'setup CoreSetup, policy: &SessionPolicy) -> Result<Self> {
        let readers = policy
            .up_specs
            .iter()
            .copied()
            .filter(|spec| spec.applies_to(id.value()))
            .map(|spec| UpChannelReader::new(spec, id, setup.defmt.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let mut target = Self {
            rtt,
            id,
            down_channel: policy.down_channel,
            down_present: false,
            readers,
        };
        target.validate_channels(&policy.up_specs)?;
        target.refresh_down();
        Ok(target)
    }

    fn down_present(&self) -> bool {
        self.down_present
    }

    fn refresh_down(&mut self) {
        self.down_present = self
            .down_channel
            .is_some_and(|channel| channel_by_number(self.rtt.down_channels(), channel).is_some());
    }

    fn rtt_ptr(&self) -> u64 {
        self.rtt.ptr()
    }

    fn validate_channels(&mut self, specs: &[ChannelSpec]) -> Result<()> {
        validate_up_specs(&mut self.rtt, specs, self.id)
    }

    fn write_down(&mut self, core: &mut Core, data: &[u8]) -> Result<usize> {
        let Some(channel_id) = self.down_channel else {
            return Ok(0);
        };
        if data.is_empty() {
            return Ok(0);
        }
        let Some(channel) = channel_by_number(self.rtt.down_channels(), channel_id) else {
            return Ok(0);
        };
        channel.write(core, data).context("Error writing to RTT")
    }

    fn poll<S: UpSink>(&mut self, core: &mut Core, sink: &mut S) -> Result<TargetPoll> {
        let mut stats = PollStats::default();
        let mut budget = RTT_READ_BUDGET_PER_POLL;

        while budget > 0 {
            let mut made_progress = false;
            for reader in &mut self.readers {
                if budget == 0 {
                    break;
                }
                let max = reader.buffer.len().min(budget);
                let count = match channel_by_number(self.rtt.up_channels(), reader.source.channel) {
                    Some(channel) => match channel.read(core, &mut reader.buffer[..max]) {
                        Ok(count) => count,
                        Err(RttError::ReadPointerChanged) => return Ok(TargetPoll::Reattach),
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!(
                                    "Error reading from RTT up channel {}",
                                    reader.source.channel
                                )
                            });
                        }
                    },
                    None => 0,
                };
                if count == 0 {
                    while reader.decoder.process_defmt(reader.source, &[], sink)? {}
                    continue;
                }

                made_progress = true;
                budget -= count;
                stats.bytes += count;
                sink.raw_bytes(reader.source, &reader.buffer[..count])?;
                match &mut reader.decoder {
                    ChannelDecoder::Terminal(stream) => {
                        let chunk =
                            stream.consume_chunk(&reader.buffer[..count], sink.is_interactive());
                        sink.terminal(reader.source, chunk, Instant::now())?;
                        stats.messages += 1;
                    }
                    decoder @ ChannelDecoder::Defmt { .. } => {
                        let hit_frame_limit =
                            decoder.process_defmt(reader.source, &reader.buffer[..count], sink)?;
                        if hit_frame_limit {
                            while decoder.process_defmt(reader.source, &[], sink)? {}
                        }
                    }
                }
            }
            if !made_progress {
                break;
            }
        }

        self.refresh_down();
        Ok(TargetPoll::Data(stats))
    }
}

enum TargetPoll {
    Data(PollStats),
    Reattach,
}

fn reset_device(core: &mut Core, id: CoreId, rtt_ptrs: &[u64]) -> Result<()> {
    core.halt(TARGET_HALT_TIMEOUT)
        .context("Error halting target before reset")?;
    for &rtt_ptr in rtt_ptrs {
        Rtt::clear_control_block(core, &ScanRegion::Exact(rtt_ptr)).with_context(|| {
            format!("Error clearing stale RTT control block at {rtt_ptr:#010x} before reset")
        })?;
    }
    core.reset().context("Error resetting target")?;
    resume_if_halted(core, id)
}

pub(crate) fn resume_if_halted(core: &mut Core, id: CoreId) -> Result<()> {
    let status = core
        .status()
        .with_context(|| format!("Error reading core {id} status before resume"))?;
    if status.is_halted() {
        core.run()
            .with_context(|| format!("Error resuming core {id}"))?;
        log::debug!("core {id}: resumed after attach/reset (was {status:?})");
    }
    Ok(())
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec], core: CoreId) -> Result<()> {
    for spec in specs.iter().filter(|spec| spec.applies_to(core.value())) {
        let channel = ChannelId::from_cli(spec.index, "up")?;
        if channel_by_number(rtt.up_channels(), channel).is_none() {
            bail!("up channel {} does not exist on core {core}.", spec.index);
        }
    }
    Ok(())
}
