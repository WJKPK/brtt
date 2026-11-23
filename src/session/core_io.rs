use super::channel_decode::{ChannelDecoders, PolledChunk, UpChannelConfig};
use super::SessionPolicy;
use crate::channel::{channel_by_number, ChannelId, CoreChannel, CoreId};
use crate::defmt::DefmtData;
use crate::elf::ElfContents;
use crate::rtt::{Error as RttError, IncrementalScan, Rtt, RttDiscovery, RttMetadata, ScanRegion};
use anyhow::{anyhow, bail, Context, Result};
use probe_rs::{Core, Session as ProbeSession};
use std::ops::Range;
use std::time::{Duration, Instant};

pub(super) const RTT_TIMEOUT: Duration = Duration::from_secs(3);
/// Single-attempt reattach timeout: probe-rs tries once, then returns instead
/// of retrying until the timeout expires.
const RTT_RETRY_TIMEOUT: Duration = Duration::ZERO;
/// Minimum gap between reattach attempts so a pending core never stalls the
/// healthy cores' polling loop.
const RTT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const TARGET_HALT_TIMEOUT: Duration = Duration::from_millis(100);
const UP_CHANNEL_BUFFER_SIZE: usize = 4 * 1024;
const SCAN_CHUNKS_PER_TICK: usize = 1;
const MAX_RTT_READS_PER_POLL: usize = 16;
const RTT_READ_BUDGET_PER_POLL: usize = UP_CHANNEL_BUFFER_SIZE * MAX_RTT_READS_PER_POLL;

/// Per-core RTT discovery and defmt metadata derived from `--elf` and target config.
///
/// Stored outside `CoreSlot` so each slot's defmt stream decoder can borrow
/// `DefmtData` across reattachments.
struct CoreSetup {
    discovery: RttDiscovery,
    defmt: Option<DefmtData>,
    up_channels: Vec<UpChannelConfig>,
    down_channel: Option<ChannelId>,
}

impl CoreSetup {
    fn is_relevant(&self) -> bool {
        !self.up_channels.is_empty() || self.down_channel.is_some()
    }
}

/// Owns setup metadata borrowed by core slots throughout the session.
pub(super) struct Startup {
    setups: Vec<(CoreId, CoreSetup)>,
}

impl Startup {
    pub(super) fn new(
        elves: Vec<(CoreId, ElfContents)>,
        requested: Option<&ScanRegion>,
        target_default: &ScanRegion,
        policy: &SessionPolicy,
    ) -> Self {
        let setups = resolve_core_setups(
            elves,
            requested,
            target_default,
            |contents| contents.rtt_symbol,
            |core, discovery, contents| CoreSetup {
                discovery,
                defmt: contents.and_then(|contents| contents.defmt),
                up_channels: policy
                    .up_specs
                    .iter()
                    .filter(|spec| spec.applies_to(core))
                    .map(|spec| UpChannelConfig {
                        channel: spec.channel,
                        mode: spec.mode,
                    })
                    .collect(),
                down_channel: policy.down_channel,
            },
        );
        Self { setups }
    }

    pub(super) fn attach(&self, session: ProbeSession, reset: bool) -> Result<CoreSlots<'_>> {
        let mut slots = CoreSlots::build(session, &self.setups)?;
        if reset {
            slots.chip_reset()?;
        }
        let summary = slots.attach_all(RTT_TIMEOUT)?;
        if summary.attached == 0 {
            return Err(summary.error());
        }
        if !summary.pending.is_empty() {
            log::info!(
                "continuing with {} core(s); pending cores retry in the background",
                summary.attached
            );
        }
        Ok(slots)
    }

    pub(super) fn decoders(&self) -> Result<ChannelDecoders<'_>> {
        ChannelDecoders::new(
            self.setups
                .iter()
                .filter(|(_, setup)| setup.is_relevant())
                .map(|(id, setup)| (*id, setup.up_channels.as_slice(), setup.defmt.as_ref())),
        )
    }
}

pub(super) fn resolve_rtt_discoveries(
    rtt_symbols: Vec<(CoreId, Option<u64>)>,
    requested: Option<&ScanRegion>,
    target_default: &ScanRegion,
) -> Vec<(CoreId, RttDiscovery)> {
    resolve_core_setups(
        rtt_symbols,
        requested,
        target_default,
        |symbol| *symbol,
        |_, discovery, _| discovery,
    )
}

fn resolve_core_setups<T, Out>(
    items: Vec<(CoreId, T)>,
    requested: Option<&ScanRegion>,
    target_default: &ScanRegion,
    rtt_symbol: impl Fn(&T) -> Option<u64>,
    build: impl Fn(CoreId, RttDiscovery, Option<T>) -> Out,
) -> Vec<(CoreId, Out)> {
    let mut setups = if items.is_empty() {
        let core = CoreId::new(0);
        vec![(
            core,
            build(
                core,
                resolve_scan_region(None, requested, target_default),
                None,
            ),
        )]
    } else {
        if requested.is_some() && items.iter().any(|(_, item)| rtt_symbol(item).is_some()) {
            log::info!("ignoring --scan-region because --elf provides _SEGGER_RTT.");
        }
        items
            .into_iter()
            .map(|(core, item)| {
                let discovery = resolve_scan_region(rtt_symbol(&item), requested, target_default);
                (core, build(core, discovery, Some(item)))
            })
            .collect()
    };
    setups.sort_by_key(|(id, _)| *id);
    setups
}

/// Precedence: ELF-provided `_SEGGER_RTT` > explicit `--scan-region` > target default.
/// Automatic scanning is only used when neither the ELF nor the user pins a region.
fn resolve_scan_region(
    rtt_symbol: Option<u64>,
    requested: Option<&ScanRegion>,
    target_default: &ScanRegion,
) -> RttDiscovery {
    match (rtt_symbol, requested) {
        (Some(address), _) => RttDiscovery::Fixed(ScanRegion::Exact(address)),
        (None, Some(region)) => RttDiscovery::Fixed(region.clone()),
        (None, None) => RttDiscovery::Incremental(target_default.clone()),
    }
}

pub(super) enum AttachOutcome {
    Attached(Rtt),
    Retryable(String),
}

pub(super) fn attach_rtt_classified(
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
    ensure_rtt_compatible_target(core.is_64_bit())?;
    match discovery.attach(&mut core, timeout) {
        Ok(rtt) => {
            log::info!("core {id}: found control block at {:#010x}", rtt.ptr());
            resume_if_halted(&mut core, id)?;
            Ok(AttachOutcome::Attached(rtt))
        }
        Err(error) => classify_attach_error(id, error),
    }
}

fn classify_attach_error(id: CoreId, error: RttError) -> Result<AttachOutcome> {
    match error {
        RttError::ControlBlockNotFound | RttError::ControlBlockCorrupted(_) => Ok(
            AttachOutcome::Retryable(format!("core {id} RTT unavailable ({error:#})")),
        ),
        RttError::Probe(_) | RttError::MemoryRead(_) | RttError::Other(_) => Ok(
            AttachOutcome::Retryable(format!("core {id} attach transport error ({error:#})")),
        ),
        permanent => Err(anyhow!("core {id}: error attaching to RTT ({permanent:#})")),
    }
}

fn ensure_rtt_compatible_target(is_64_bit: bool) -> Result<()> {
    if is_64_bit {
        bail!("64-bit targets are not supported until probe-rs fixes 32-bit RTT offset writes on 64-bit targets");
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum AttachmentLoss {
    MetadataChanged,
    TransportUnavailable,
}

#[derive(Clone, Copy)]
pub(super) enum CoreEvent {
    Attached { id: CoreId, recovered: bool },
    AttachmentLost { id: CoreId, reason: AttachmentLoss },
}

pub(super) struct DownWrite {
    pub(super) count: usize,
    pub(super) event: Option<CoreEvent>,
}

#[derive(Clone, Copy)]
pub(super) struct PollOutcome {
    pub(super) had_data: bool,
    pub(super) had_events: bool,
}

struct ChunkRange {
    source: CoreChannel,
    range: Range<usize>,
}

/// Pre-allocated per-tick buffer reused across polls without reallocation.
struct PollBuffer {
    data: Vec<u8>,
    cursor: usize,
    chunks: Vec<ChunkRange>,
    events: Vec<CoreEvent>,
}

impl PollBuffer {
    fn new(slot_count: usize) -> Self {
        Self {
            data: vec![0; slot_count * RTT_READ_BUDGET_PER_POLL],
            cursor: 0,
            chunks: Vec::with_capacity(slot_count * MAX_RTT_READS_PER_POLL),
            events: Vec::with_capacity(slot_count),
        }
    }

    fn clear(&mut self) {
        self.cursor = 0;
        self.chunks.clear();
        self.events.clear();
    }

    fn write_slice(&mut self, max: usize) -> &mut [u8] {
        &mut self.data[self.cursor..self.cursor + max]
    }

    fn commit_read(&mut self, source: CoreChannel, count: usize) {
        let start = self.cursor;
        let end = start + count;
        self.cursor = end;
        self.chunks.push(ChunkRange {
            source,
            range: start..end,
        });
    }

    fn push_event(&mut self, event: CoreEvent) {
        self.events.push(event);
    }

    fn outcome(&self) -> PollOutcome {
        PollOutcome {
            had_data: !self.chunks.is_empty(),
            had_events: !self.events.is_empty(),
        }
    }

    fn chunks(&self) -> impl Iterator<Item = PolledChunk<'_>> + '_ {
        self.chunks.iter().map(|chunk| PolledChunk {
            source: chunk.source,
            bytes: &self.data[chunk.range.clone()],
        })
    }
}

struct AttachedCore {
    rtt: Rtt,
    down_channel: Option<ChannelId>,
    next_channel: usize,
    metadata: RttMetadata,
}

struct CoreSlot<'setup> {
    id: CoreId,
    setup: &'setup CoreSetup,
    state: TargetState,
}

struct PendingState {
    next_retry: Instant,
    ever_attached: bool,
    scan: Option<IncrementalScan>,
}

impl PendingState {
    fn try_attach(
        &mut self,
        session: &mut ProbeSession,
        id: CoreId,
        discovery: &RttDiscovery,
        timeout: Duration,
    ) -> Result<Option<AttachOutcome>> {
        if !timeout.is_zero() {
            return attach_rtt_classified(session, id, discovery, timeout).map(Some);
        }
        let mut core = match session.core(id.as_usize()) {
            Ok(core) => core,
            Err(error) => {
                return Ok(Some(AttachOutcome::Retryable(format!(
                    "core {id} unavailable ({error:#})"
                ))));
            }
        };
        ensure_rtt_compatible_target(core.is_64_bit())?;
        if self.scan.is_none() {
            self.scan = discovery.incremental_scan(&core)?;
        }
        let Some(scan) = &mut self.scan else {
            drop(core);
            return attach_rtt_classified(session, id, discovery, timeout).map(Some);
        };
        match scan.step(&mut core, SCAN_CHUNKS_PER_TICK) {
            Ok(Some(rtt)) => {
                self.scan = None;
                resume_if_halted(&mut core, id)?;
                Ok(Some(AttachOutcome::Attached(rtt)))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.scan = None;
                classify_attach_error(id, error).map(Some)
            }
        }
    }
}

enum TargetState {
    Pending(PendingState),
    Attached(AttachedCore),
}

pub(super) struct CoreSlots<'setup> {
    session: ProbeSession,
    slots: Vec<CoreSlot<'setup>>,
    polled: PollBuffer,
}

struct AttachSummary {
    attached: usize,
    pending: Vec<String>,
}

impl AttachSummary {
    fn error(&self) -> anyhow::Error {
        let detail = self.pending.join("; ");
        anyhow!("no RTT targets could be opened ({detail}); ensure the cores are running and the ELF addresses match the firmware")
    }
}

impl<'setup> CoreSlot<'setup> {
    fn new(id: CoreId, setup: &'setup CoreSetup) -> Self {
        Self {
            id,
            setup,
            state: TargetState::Pending(PendingState {
                next_retry: Instant::now(),
                ever_attached: false,
                scan: None,
            }),
        }
    }

    fn id(&self) -> CoreId {
        self.id
    }

    fn is_attached(&self) -> bool {
        matches!(self.state, TargetState::Attached(_))
    }

    fn is_due(&self, now: Instant) -> bool {
        matches!(&self.state, TargetState::Pending(pending) if now >= pending.next_retry)
    }

    fn defer_retry(&mut self, now: Instant) {
        let ever_attached = match &self.state {
            TargetState::Pending(pending) => pending.ever_attached,
            TargetState::Attached(_) => true,
        };
        self.state = TargetState::Pending(PendingState {
            next_retry: now + RTT_RETRY_INTERVAL,
            ever_attached,
            scan: None,
        });
    }

    fn lose_attachment(&mut self, now: Instant, reason: AttachmentLoss) -> CoreEvent {
        debug_assert!(self.is_attached());
        self.state = TargetState::Pending(PendingState {
            next_retry: now + RTT_RETRY_INTERVAL,
            ever_attached: true,
            scan: None,
        });
        CoreEvent::AttachmentLost {
            id: self.id,
            reason,
        }
    }

    fn try_attach(&mut self, session: &mut ProbeSession, timeout: Duration) -> Result<SlotAttach> {
        let TargetState::Pending(pending) = &mut self.state else {
            return Ok(SlotAttach::AlreadyAttached);
        };
        let recovered = pending.ever_attached;
        let Some(outcome) = pending.try_attach(session, self.id, &self.setup.discovery, timeout)?
        else {
            return Ok(SlotAttach::ScanInProgress);
        };
        match outcome {
            AttachOutcome::Attached(rtt) => {
                let mut core = match session.core(self.id.as_usize()) {
                    Ok(core) => core,
                    Err(error) => {
                        return Ok(SlotAttach::Retryable(format!(
                            "core {} unavailable after RTT discovery ({error:#})",
                            self.id
                        )))
                    }
                };
                let target = match AttachedCore::new(rtt, self.id, self.setup, &mut core) {
                    Ok(target) => target,
                    Err(TargetIoInitError::Snapshot(error)) => {
                        return Ok(SlotAttach::Retryable(format!(
                            "core {} RTT metadata snapshot unavailable ({error:#})",
                            self.id
                        )))
                    }
                    Err(TargetIoInitError::Configuration(error)) => return Err(error),
                };
                self.state = TargetState::Attached(target);
                Ok(SlotAttach::Attached { recovered })
            }
            AttachOutcome::Retryable(reason) => Ok(SlotAttach::Retryable(reason)),
        }
    }

    fn poll(
        &mut self,
        session: &mut ProbeSession,
        polled: &mut PollBuffer,
        now: Instant,
    ) -> Option<AttachmentLoss> {
        let mut core = match session.core(self.id.as_usize()) {
            Ok(core) => core,
            Err(error) => {
                log::warn!(
                    "core {} unavailable ({error:#}); continuing with remaining cores",
                    self.id
                );
                self.lose_attachment(now, AttachmentLoss::TransportUnavailable);
                return Some(AttachmentLoss::TransportUnavailable);
            }
        };
        let result = match &mut self.state {
            TargetState::Attached(target) => {
                target.poll(self.id, &self.setup.up_channels, &mut core, polled)
            }
            TargetState::Pending(_) => return None,
        };
        match result {
            Ok(TargetPoll::Ok) => None,
            Ok(TargetPoll::Reattach) => {
                self.lose_attachment(now, AttachmentLoss::MetadataChanged);
                Some(AttachmentLoss::MetadataChanged)
            }
            Err(error) => {
                let reason = if matches!(&error, RttError::ReadPointerChanged) {
                    AttachmentLoss::MetadataChanged
                } else {
                    AttachmentLoss::TransportUnavailable
                };
                log::warn!(
                    "core {} RTT read error ({error:#}); retrying attachment",
                    self.id
                );
                self.lose_attachment(now, reason);
                Some(reason)
            }
        }
    }

    fn down_present(&self) -> bool {
        matches!(&self.state, TargetState::Attached(target) if target.down_present())
    }

    fn rtt_ptr(&self) -> Option<u64> {
        match &self.state {
            TargetState::Attached(target) => Some(target.rtt_ptr()),
            TargetState::Pending(_) => None,
        }
    }

    fn write_down(&mut self, core: &mut Core, bytes: &[u8]) -> DownWrite {
        let result = match &mut self.state {
            TargetState::Attached(target) => match target.metadata_valid(core) {
                Ok(true) => target.write_down(core, bytes).map(Some),
                Ok(false) => Ok(None),
                Err(error) => Err(error),
            },
            TargetState::Pending(_) => {
                return DownWrite {
                    count: 0,
                    event: None,
                }
            }
        };
        match result {
            Ok(Some(count)) => DownWrite { count, event: None },
            Ok(None) => DownWrite {
                count: 0,
                event: Some(self.lose_attachment(Instant::now(), AttachmentLoss::MetadataChanged)),
            },
            Err(error) => {
                let reason = if matches!(&error, RttError::ReadPointerChanged) {
                    AttachmentLoss::MetadataChanged
                } else {
                    AttachmentLoss::TransportUnavailable
                };
                log::warn!(
                    "core {} RTT down error ({error:#}); retrying attachment",
                    self.id
                );
                DownWrite {
                    count: 0,
                    event: Some(self.lose_attachment(Instant::now(), reason)),
                }
            }
        }
    }
}

enum SlotAttach {
    AlreadyAttached,
    Attached { recovered: bool },
    ScanInProgress,
    Retryable(String),
}

impl<'setup> CoreSlots<'setup> {
    fn build(session: ProbeSession, setups: &'setup [(CoreId, CoreSetup)]) -> Result<Self> {
        let slots: Vec<_> = setups
            .iter()
            .filter(|(_, setup)| setup.is_relevant())
            .map(|(id, setup)| CoreSlot::new(*id, setup))
            .collect();
        if slots.is_empty() {
            bail!("no configured core selects any up channel and down input is disabled");
        }
        let polled = PollBuffer::new(slots.len());
        Ok(Self {
            session,
            slots,
            polled,
        })
    }

    fn attach_all(&mut self, timeout: Duration) -> Result<AttachSummary> {
        let mut attached = 0;
        let mut pending = Vec::new();
        for slot in &mut self.slots {
            match slot.try_attach(&mut self.session, timeout) {
                Ok(SlotAttach::AlreadyAttached | SlotAttach::Attached { .. }) => attached += 1,
                Ok(SlotAttach::ScanInProgress) => {
                    pending.push(format!("core {} RTT scan in progress", slot.id()));
                }
                Ok(SlotAttach::Retryable(reason)) => {
                    log::warn!("{reason}; continuing without it");
                    slot.defer_retry(Instant::now());
                    pending.push(reason);
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("core {} failed to attach", slot.id()))
                }
            }
        }
        Ok(AttachSummary { attached, pending })
    }

    /// Resets through the lowest accessible configured core. RTT attachment is
    /// not required: stale blocks attached so far are cleared best-effort,
    /// otherwise the target reset itself invalidates RAM.
    pub(super) fn chip_reset(&mut self) -> Result<()> {
        let mut ids: Vec<CoreId> = self.slots.iter().map(CoreSlot::id).collect();
        ids.sort_unstable();
        ids.dedup();
        let mut first_error = None;
        let mut initiator = None;
        for &id in &ids {
            match self.session.core(id.as_usize()) {
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
        let initiator_ptr = self
            .slots
            .iter()
            .find(|slot| slot.id() == id)
            .and_then(CoreSlot::rtt_ptr);
        for slot in &self.slots {
            let owner = slot.id();
            if owner == id {
                continue;
            }
            let Some(rtt_ptr) = slot.rtt_ptr() else {
                continue;
            };
            match self.session.core(owner.as_usize()) {
                Ok(mut core) => halt_and_clear(&mut core, owner, Some(rtt_ptr)),
                Err(error) => log::warn!(
                    "core {owner}: unavailable to clear stale RTT block before reset ({error:#}); resetting anyway"
                ),
            }
        }
        let reset_result = match self.session.core(id.as_usize()) {
            Ok(mut core) => reset_device(&mut core, id, initiator_ptr),
            Err(error) => Err(error).context("cannot access reset initiator"),
        };
        // A system reset can leave peer cores halted, including pending cores
        // that had no RTT block to clear.
        for &other in &ids {
            if other == id {
                continue;
            }
            let mut core = match self.session.core(other.as_usize()) {
                Ok(core) => core,
                Err(error) => {
                    log::warn!("core {other}: unavailable to resume after reset ({error:#})");
                    continue;
                }
            };
            if let Err(error) = resume_after_operation(&mut core, other) {
                log::warn!("core {other}: could not resume after reset ({error:#})");
            }
        }
        reset_result?;
        let now = Instant::now();
        for slot in &mut self.slots {
            slot.defer_retry(now);
        }
        log::info!("target reset issued via core {id}; reattaching cores");
        Ok(())
    }

    pub(super) fn poll(&mut self, now: Instant) -> Result<PollOutcome> {
        self.polled.clear();
        // Poll healthy cores before touching any pending core's potentially slow RAM.
        for slot in &mut self.slots {
            if !slot.is_attached() {
                continue;
            }
            let id = slot.id();
            if let Some(reason) = slot.poll(&mut self.session, &mut self.polled, now) {
                log::info!("core {id} RTT attachment lost; reattaching");
                self.polled
                    .push_event(CoreEvent::AttachmentLost { id, reason });
            }
        }
        for slot in &mut self.slots {
            if slot.is_attached() || !slot.is_due(now) {
                continue;
            }
            match slot.try_attach(&mut self.session, RTT_RETRY_TIMEOUT)? {
                SlotAttach::AlreadyAttached => {
                    unreachable!("pending slot cannot already be attached")
                }
                SlotAttach::Attached { recovered } => {
                    let id = slot.id();
                    self.polled.push_event(CoreEvent::Attached { id, recovered });
                }
                SlotAttach::ScanInProgress => {}
                SlotAttach::Retryable(reason) => {
                    log::trace!("{reason}; retrying");
                    slot.defer_retry(now);
                }
            }
        }
        Ok(self.polled.outcome())
    }

    pub(super) fn polled_chunks(&self) -> impl Iterator<Item = PolledChunk<'_>> + '_ {
        self.polled.chunks()
    }

    pub(super) fn polled_events(&self) -> &[CoreEvent] {
        &self.polled.events
    }

    pub(super) fn ids(&self) -> Vec<CoreId> {
        self.slots.iter().map(CoreSlot::id).collect()
    }

    pub(super) fn all_attached(&self) -> bool {
        self.slots.iter().all(CoreSlot::is_attached)
    }

    pub(super) fn is_attached(&self, id: CoreId) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.id() == id && slot.is_attached())
    }

    pub(super) fn attached_ids(&self) -> impl Iterator<Item = CoreId> + '_ {
        self.slots
            .iter()
            .filter(|slot| slot.is_attached())
            .map(CoreSlot::id)
    }
    pub(super) fn routable_down(&self) -> Vec<CoreId> {
        self.slots
            .iter()
            .filter(|slot| slot.down_present())
            .map(CoreSlot::id)
            .collect()
    }

    pub(super) fn missing_down(&self) -> Vec<CoreId> {
        self.slots
            .iter()
            .filter(|slot| slot.is_attached() && !slot.down_present())
            .map(CoreSlot::id)
            .collect()
    }

    pub(super) fn write_down(&mut self, id: CoreId, bytes: &[u8]) -> Result<DownWrite> {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.id() == id) else {
            return Ok(DownWrite {
                count: 0,
                event: None,
            });
        };
        if !slot.is_attached() {
            return Ok(DownWrite {
                count: 0,
                event: None,
            });
        }
        let mut core = match self.session.core(id.as_usize()) {
            Ok(core) => core,
            Err(error) => {
                log::warn!("core {id} unavailable during RTT down write ({error:#})");
                return Ok(DownWrite {
                    count: 0,
                    event: Some(
                        slot.lose_attachment(Instant::now(), AttachmentLoss::TransportUnavailable),
                    ),
                });
            }
        };
        Ok(slot.write_down(&mut core, bytes))
    }
}

enum TargetIoInitError {
    Snapshot(RttError),
    Configuration(anyhow::Error),
}

impl AttachedCore {
    fn new(
        mut rtt: Rtt,
        id: CoreId,
        setup: &CoreSetup,
        core: &mut Core,
    ) -> Result<Self, TargetIoInitError> {
        validate_up_specs(&mut rtt, &setup.up_channels, id)
            .map_err(TargetIoInitError::Configuration)?;
        let down_channel = setup
            .down_channel
            .filter(|channel| channel_by_number(rtt.down_channels(), *channel).is_some());
        let metadata = RttMetadata::capture(
            core,
            &rtt,
            setup.up_channels.iter().map(|config| config.channel),
            down_channel,
        )
        .map_err(TargetIoInitError::Snapshot)?;
        Ok(Self {
            rtt,
            down_channel,
            next_channel: 0,
            metadata,
        })
    }

    fn down_present(&self) -> bool {
        self.down_channel.is_some()
    }

    fn rtt_ptr(&self) -> u64 {
        self.rtt.ptr()
    }

    fn metadata_valid(&self, core: &mut Core) -> Result<bool, RttError> {
        self.metadata.is_valid(core, &self.rtt)
    }

    fn write_down(&mut self, core: &mut Core, data: &[u8]) -> Result<usize, RttError> {
        if data.is_empty() {
            return Ok(0);
        }
        match self.down_channel {
            Some(channel_id) => channel_by_number(self.rtt.down_channels(), channel_id)
                .map_or(Ok(0), |channel| channel.write(core, data)),
            None => Ok(0),
        }
    }

    fn poll(
        &mut self,
        id: CoreId,
        up_channels: &[UpChannelConfig],
        core: &mut Core,
        polled: &mut PollBuffer,
    ) -> Result<TargetPoll, RttError> {
        match self.metadata_valid(core) {
            Ok(true) => {}
            Ok(false) | Err(RttError::ReadPointerChanged) => return Ok(TargetPoll::Reattach),
            Err(error) => return Err(error),
        }
        let mut budget = RTT_READ_BUDGET_PER_POLL;
        let len = up_channels.len();
        if len == 0 {
            return Ok(TargetPoll::Ok);
        }
        let start = self.next_channel;
        self.next_channel = (start + 1) % len;
        while budget > 0 {
            let mut made_progress = false;
            for offset in 0..len {
                if budget == 0 {
                    break;
                }
                let channel_id = up_channels[(start + offset) % len].channel;
                let max = UP_CHANNEL_BUFFER_SIZE.min(budget);
                let count = match channel_by_number(self.rtt.up_channels(), channel_id) {
                    Some(channel) => match channel.read(core, polled.write_slice(max)) {
                        Ok(count) => count,
                        Err(RttError::ReadPointerChanged) => return Ok(TargetPoll::Reattach),
                        Err(error) => return Err(error),
                    },
                    None => 0,
                };
                if count == 0 {
                    continue;
                }
                made_progress = true;
                budget -= count;
                polled.commit_read(
                    CoreChannel {
                        core: id,
                        channel: channel_id,
                    },
                    count,
                );
            }
            if !made_progress {
                break;
            }
        }
        Ok(TargetPoll::Ok)
    }
}

enum TargetPoll {
    Ok,
    Reattach,
}

fn halt_and_clear(core: &mut Core, id: CoreId, rtt_ptr: Option<u64>) {
    match core.halt(TARGET_HALT_TIMEOUT) {
        Ok(_) => {
            if let Some(rtt_ptr) = rtt_ptr {
                if let Err(error) = Rtt::clear_control_block(core, &ScanRegion::Exact(rtt_ptr)) {
                    log::warn!("core {id}: could not clear stale RTT block at {rtt_ptr:#010x} before reset ({error:#}); resetting anyway");
                }
            }
        }
        Err(error) => log::warn!(
            "core {id}: could not halt to clear stale RTT block ({error:#}); resetting anyway"
        ),
    }
}

fn reset_device(core: &mut Core, id: CoreId, rtt_ptr: Option<u64>) -> Result<()> {
    halt_and_clear(core, id, rtt_ptr);
    let reset = core.reset().context("Error resetting target");
    let resume = resume_after_operation(core, id);
    match (reset, resume) {
        (Err(reset), Err(resume)) => {
            Err(reset.context(format!("also failed to resume core {id}: {resume:#}")))
        }
        (Err(reset), _) => Err(reset),
        (_, Err(resume)) => Err(resume),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn resume_after_operation(core: &mut Core, id: CoreId) -> Result<()> {
    resume_if_halted(core, id).or_else(|error| {
        // Even if status could not be queried, do not leave a halted core stranded.
        core.run()
            .with_context(|| format!("{error:#}; fallback run of core {id} also failed"))
    })
}

fn resume_if_halted(core: &mut Core, id: CoreId) -> Result<()> {
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

fn validate_up_specs(rtt: &mut Rtt, specs: &[UpChannelConfig], core: CoreId) -> Result<()> {
    for spec in specs {
        if channel_by_number(rtt.up_channels(), spec.channel).is_none() {
            bail!("up channel {} does not exist on core {core}.", spec.channel);
        }
    }
    Ok(())
}
