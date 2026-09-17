use crate::channel::{channel_by_number, ChannelId, CoreChannel};
use crate::cli::{ChannelEncoding, ChannelSpec, SessionConfig};
use crate::defmt::{decode_frames, DecodeOutput, DefmtData, MAX_DECODE_BUFFERED_BYTES};
use crate::renderer::Renderer;
use crate::terminal::DecodedStream;
use anyhow::{bail, Context, Result};
use brtt::rtt::{Error as RttError, Rtt, RttDiscovery, ScanRegion};
use probe_rs::{Core, Session as ProbeSession};
use std::io::Write;
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
    fn new(spec: ChannelSpec, core: u32, defmt: Option<&'table DefmtData>) -> Result<Self> {
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
    fn process_defmt<W: Write>(
        &mut self,
        source: CoreChannel,
        bytes: &[u8],
        renderer: &mut Renderer<W>,
    ) -> Result<bool> {
        let Self::Defmt {
            data,
            stream,
            bytes_since_restart,
        } = self
        else {
            return Ok(false);
        };

        // Skip on empty input so the idle flush loop in `poll` doesn't repeatedly
        // re-trigger this reset once the threshold has already been crossed once.
        if !bytes.is_empty()
            && bytes_since_restart.saturating_add(bytes.len()) > MAX_DECODE_BUFFERED_BYTES
        {
            *stream = data.table.new_stream_decoder();
            *bytes_since_restart = 0;
            renderer.render_defmt_warning(
                source,
                "defmt decoder input exceeded 64 KiB; resetting decoder",
                Instant::now(),
            )?;
        }

        let decoded = decode_frames(
            stream.as_mut(),
            bytes,
            data.locations.as_ref(),
            data.table.encoding().can_recover(),
        );
        for item in decoded.frames {
            match item {
                DecodeOutput::Frame(frame) => {
                    renderer.render_defmt_frame(source, &frame, Instant::now())?
                }
                DecodeOutput::Warning(warning) => {
                    renderer.render_defmt_warning(source, &warning, Instant::now())?
                }
            }
        }

        if decoded.restart {
            *stream = data.table.new_stream_decoder();
            *bytes_since_restart = 0;
        } else {
            // The decoder does not expose its pending-byte count, so bound all
            // input received since it was created, including incomplete suffixes.
            *bytes_since_restart = bytes_since_restart.saturating_add(bytes.len());
        }
        Ok(decoded.hit_frame_limit)
    }
}

/// Outcome of one RTT attach attempt with the failure already classified:
/// retryable absence (disabled core, firmware not initialized yet) versus a
/// permanent configuration or compatibility failure.
pub(crate) enum AttachOutcome {
    Attached(Rtt),
    Retryable(String),
    Fatal(anyhow::Error),
}

/// Attaches to one core's RTT control block with `timeout`, bounds-checking
/// the index first (hard error for user mistakes). probe-rs performs a single
/// attempt for a zero timeout, so runtime retries stay nonblocking.
pub(crate) fn attach_rtt_classified(
    session: &mut ProbeSession,
    index: u32,
    discovery: &RttDiscovery,
    timeout: Duration,
) -> Result<AttachOutcome> {
    let core_count = session.target().cores.len();
    if (index as usize) >= core_count {
        bail!(
            "--elf index {index} out of range: target '{}' has {core_count} core(s)",
            session.target().name
        );
    }
    let mut core = match session.core(index as usize) {
        Ok(core) => core,
        Err(error) => {
            return Ok(AttachOutcome::Retryable(format!(
                "core {index} unavailable ({error:#})"
            )));
        }
    };
    if let Err(error) = ensure_rtt_compatible_target(core.is_64_bit()) {
        return Ok(AttachOutcome::Fatal(error));
    }
    match discovery.attach(&mut core, timeout) {
        Ok(rtt) => {
            log::info!("core {index}: found control block at {:#010x}", rtt.ptr());
            resume_if_halted(&mut core, index)?;
            Ok(AttachOutcome::Attached(rtt))
        }
        Err(error) => Ok(classify_attach_error(index, error)),
    }
}

/// Retryable absence keeps the slot pending; anything structural is fatal so
/// a bad address or ambiguous scan never degrades into silent polling.
fn classify_attach_error(index: u32, error: RttError) -> AttachOutcome {
    match error {
        RttError::ControlBlockNotFound | RttError::ControlBlockCorrupted(_) => {
            AttachOutcome::Retryable(format!("core {index} RTT unavailable ({error:#})"))
        }
        RttError::Probe(_) | RttError::MemoryRead(_) | RttError::Other(_) => {
            AttachOutcome::Retryable(format!("core {index} attach transport error ({error:#})"))
        }
        permanent => AttachOutcome::Fatal(anyhow::anyhow!(
            "core {index}: error attaching to RTT ({permanent:#})"
        )),
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

pub(crate) enum PollOutcome {
    Data(PollStats),
    Reattach,
}

/// Bidirectional RTT channel I/O for one core of an attached target.
///
/// Holds everything owned per core except the live [`Core`] handle, which
/// probe-rs only lends out transiently: callers acquire it from the session
/// for each operation and drop it immediately after. `session.rs` sequences
/// lifecycle operations (reset, reattach, renderer and input restart); this
/// type only performs target-side I/O and discovery. Only the up specs
/// selecting `index` are opened; the rest belong to other cores and are
/// ignored here.
pub(crate) struct TargetIo<'config> {
    rtt: Rtt,
    config: &'config SessionConfig,
    index: u32,
    down_present: bool,
    readers: Vec<UpChannelReader<'config>>,
}

/// One configured core with persistent lifecycle state. A core that is
/// disabled or whose firmware has not initialized RTT yet stays `Pending`
/// and keeps retrying instead of vanishing from the session.
pub(crate) struct TargetSlot<'config> {
    index: u32,
    config: &'config SessionConfig,
    state: TargetState<'config>,
}

pub(crate) enum TargetState<'config> {
    Pending {
        next_retry: Instant,
        ever_attached: bool,
    },
    Attached(TargetIo<'config>),
}

/// Outcome of [`TargetSlot::try_attach`]: permanent failures surface as
/// `Err`, retryable absence as `Retryable` with a diagnostic reason.
/// `Attached.recovered` tells a first attach apart from a recovery so the
/// runner logs the right notice.
pub(crate) enum AttachResult {
    Attached { recovered: bool },
    Retryable(String),
}

impl<'config> TargetSlot<'config> {
    pub(crate) fn new(index: u32, config: &'config SessionConfig) -> Self {
        Self {
            index,
            config,
            state: TargetState::Pending {
                next_retry: Instant::now(),
                ever_attached: false,
            },
        }
    }

    /// Builds every relevant slot: cores with no applicable up channel are
    /// still relevant while down input is enabled, since any of them may
    /// expose the routable down channel.
    pub(crate) fn relevant(index: u32, config: &'config SessionConfig) -> bool {
        config.up_specs.iter().any(|spec| spec.applies_to(index)) || config.down_channel.is_some()
    }

    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn is_attached(&self) -> bool {
        matches!(self.state, TargetState::Attached(_))
    }

    pub(crate) fn attached(&self) -> Option<&TargetIo<'config>> {
        match &self.state {
            TargetState::Attached(target) => Some(target),
            TargetState::Pending { .. } => None,
        }
    }

    pub(crate) fn attached_mut(&mut self) -> Option<&mut TargetIo<'config>> {
        match &mut self.state {
            TargetState::Attached(target) => Some(target),
            TargetState::Pending { .. } => None,
        }
    }

    /// Whether a reattach attempt is due. Attached slots are always due.
    pub(crate) fn due(&self, now: Instant) -> bool {
        match &self.state {
            TargetState::Attached(_) => true,
            TargetState::Pending { next_retry, .. } => now >= *next_retry,
        }
    }

    /// Moves the slot back to pending after a lost core or moved RTT block,
    /// throttling the next attempt so healthy cores keep streaming.
    pub(crate) fn mark_pending(&mut self) {
        self.state = TargetState::Pending {
            next_retry: Instant::now() + RTT_RETRY_INTERVAL,
            ever_attached: true,
        };
    }

    /// Single attach attempt for a pending slot. Attached slots are a no-op.
    /// Returns the retry reason for aggregate startup diagnostics.
    pub(crate) fn try_attach(
        &mut self,
        session: &mut ProbeSession,
        timeout: Duration,
    ) -> Result<AttachResult> {
        let ever_attached = match &self.state {
            TargetState::Attached(_) => return Ok(AttachResult::Attached { recovered: true }),
            TargetState::Pending { ever_attached, .. } => *ever_attached,
        };
        match attach_rtt_classified(session, self.index, &self.config.discovery, timeout)? {
            AttachOutcome::Attached(rtt) => {
                self.state = TargetState::Attached(TargetIo::new(rtt, self.index, self.config)?);
                Ok(AttachResult::Attached {
                    recovered: ever_attached,
                })
            }
            AttachOutcome::Retryable(reason) => Ok(AttachResult::Retryable(reason)),
            AttachOutcome::Fatal(error) => Err(error),
        }
    }

    /// Throttles the next attempt after a retryable failure.
    pub(crate) fn defer_retry(&mut self) {
        if let TargetState::Pending { ever_attached, .. } = &self.state {
            let ever_attached = *ever_attached;
            self.state = TargetState::Pending {
                next_retry: Instant::now() + RTT_RETRY_INTERVAL,
                ever_attached,
            };
        }
    }
}

impl<'config> TargetIo<'config> {
    /// Builds channel readers for the specs selecting `index` and validates
    /// them against the attached block. Selection mistakes (unknown channel,
    /// missing defmt table) are hard errors.
    pub(crate) fn new(rtt: Rtt, index: u32, config: &'config SessionConfig) -> Result<Self> {
        let readers = config
            .up_specs
            .iter()
            .copied()
            .filter(|spec| spec.applies_to(index))
            .map(|spec| UpChannelReader::new(spec, index, config.defmt.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        let mut target = Self {
            rtt,
            config,
            index,
            down_present: false,
            readers,
        };
        target.validate_channels()?;
        target.refresh_down();
        Ok(target)
    }

    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    /// Whether the configured down channel currently exists in this core's
    /// RTT table. Recomputed after every attach and poll so firmware restarts
    /// refresh keyboard routing.
    pub(crate) fn down_present(&self) -> bool {
        self.down_present
    }

    fn refresh_down(&mut self) {
        self.down_present = match self.config.down_channel {
            Some(channel) => channel_by_number(self.rtt.down_channels(), channel).is_some(),
            None => false,
        };
    }

    /// Issues one chip-wide device reset through this core's handle. All slots
    /// reattach independently afterwards; only one physical reset happens.
    pub(crate) fn reset_device(core: &mut Core, index: u32, rtt_ptrs: &[u64]) -> Result<()> {
        core.halt(TARGET_HALT_TIMEOUT)
            .context("Error halting target before reset")?;
        for &rtt_ptr in rtt_ptrs {
            Rtt::clear_control_block(core, &ScanRegion::Exact(rtt_ptr)).with_context(|| {
                format!("Error clearing stale RTT control block at {rtt_ptr:#010x} before reset")
            })?;
        }
        core.reset().context("Error resetting target")?;
        resume_if_halted(core, index)?;
        Ok(())
    }

    pub(crate) fn rtt_ptr(&self) -> u64 {
        self.rtt.ptr()
    }

    fn validate_channels(&mut self) -> Result<()> {
        validate_up_specs(&mut self.rtt, &self.config.up_specs, self.index)?;
        Ok(())
    }

    /// Writes as much of `data` as the target down channel accepts and returns
    /// the number of bytes consumed.
    pub(crate) fn write_down(&mut self, core: &mut Core, data: &[u8]) -> Result<usize> {
        let Some(channel_id) = self.config.down_channel else {
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

    pub(crate) fn poll<W: Write>(
        &mut self,
        core: &mut Core,
        renderer: &mut Renderer<W>,
    ) -> Result<PollOutcome> {
        let mut stats = PollStats::default();
        let mut budget = RTT_READ_BUDGET_PER_POLL;

        while budget > 0 {
            let mut made_progress = false;

            for reader in self.readers.iter_mut() {
                if budget == 0 {
                    break;
                }
                let max = reader.buffer.len().min(budget);
                let count = match channel_by_number(self.rtt.up_channels(), reader.source.channel) {
                    Some(channel) => match channel.read(core, &mut reader.buffer[..max]) {
                        Ok(count) => count,
                        Err(RttError::ReadPointerChanged) => return Ok(PollOutcome::Reattach),
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
                    // No new bytes, but a defmt decoder may still be holding a
                    // complete frame from a previous partial read — drain it.
                    while reader.decoder.process_defmt(reader.source, &[], renderer)? {}
                    continue;
                }

                made_progress = true;
                budget -= count;
                stats.bytes += count;
                renderer.log_raw_bytes(reader.source, &reader.buffer[..count])?;

                match &mut reader.decoder {
                    ChannelDecoder::Terminal(stream) => {
                        let styled = renderer.is_interactive();
                        let chunk = stream.consume_chunk(&reader.buffer[..count], styled);
                        renderer.render_terminal_event(reader.source, chunk, Instant::now())?;
                        stats.messages += 1;
                    }
                    decoder @ ChannelDecoder::Defmt { .. } => {
                        let hit_frame_limit = decoder.process_defmt(
                            reader.source,
                            &reader.buffer[..count],
                            renderer,
                        )?;
                        if hit_frame_limit {
                            while decoder.process_defmt(reader.source, &[], renderer)? {}
                        }
                    }
                }
            }

            if !made_progress {
                break;
            }
        }

        // The firmware may have restarted its channel table without moving
        // the control block; keep keyboard routing accurate every poll.
        self.refresh_down();
        Ok(PollOutcome::Data(stats))
    }
}

/// RTT clients should not leave a target stopped merely because the probe
/// connected while it was halted. This is also required after a reset: the
/// reset sequence may preserve the debug halt request.
pub(crate) fn resume_if_halted(core: &mut Core, index: u32) -> Result<()> {
    let status = core
        .status()
        .with_context(|| format!("Error reading core {index} status before resume"))?;
    if status.is_halted() {
        core.run()
            .with_context(|| format!("Error resuming core {index}"))?;
        log::debug!("core {index}: resumed after attach/reset (was {status:?})");
    }
    Ok(())
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec], core: u32) -> Result<()> {
    for spec in specs.iter().filter(|spec| spec.applies_to(core)) {
        let channel = ChannelId::from_cli(spec.index, "up")?;

        if channel_by_number(rtt.up_channels(), channel).is_none() {
            bail!("up channel {} does not exist on core {core}.", spec.index);
        }
    }

    Ok(())
}
