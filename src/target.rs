use crate::channel::{channel_by_number, ChannelId};
use crate::cli::{ChannelEncoding, ChannelSpec, SessionConfig};
use crate::defmt::{decode_frames, DecodeOutput, DefmtData, MAX_DECODE_BUFFERED_BYTES};
use crate::renderer::Renderer;
use crate::terminal::DecodedStream;
use anyhow::{bail, Context, Result};
use brtt::rtt::{Error as RttError, Rtt, RttDiscovery, ScanRegion};
use probe_rs::Core;
use std::io::Write;
use std::time::{Duration, Instant};

const RTT_TIMEOUT: Duration = Duration::from_secs(3);
const TARGET_HALT_TIMEOUT: Duration = Duration::from_millis(100);
const UP_CHANNEL_BUFFER_SIZE: usize = 4 * 1024;
const MAX_RTT_READS_PER_POLL: usize = 16;
const RTT_READ_BUDGET_PER_POLL: usize = UP_CHANNEL_BUFFER_SIZE * MAX_RTT_READS_PER_POLL;

struct UpChannelReader<'table> {
    channel: ChannelId,
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
    fn new(spec: ChannelSpec, defmt: Option<&'table DefmtData>) -> Result<Self> {
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
            channel,
            buffer: vec![0; UP_CHANNEL_BUFFER_SIZE],
            decoder,
        })
    }

    fn restart(&mut self) {
        self.decoder.restart();
    }
}

impl ChannelDecoder<'_> {
    fn restart(&mut self) {
        match self {
            Self::Terminal(stream) => stream.reset(),
            Self::Defmt {
                data,
                stream,
                bytes_since_restart,
            } => {
                *stream = data.table.new_stream_decoder();
                *bytes_since_restart = 0;
            }
        }
    }

    fn process_defmt<W: Write>(
        &mut self,
        channel: ChannelId,
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
                channel,
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
                    renderer.render_defmt_frame(channel, &frame, Instant::now())?
                }
                DecodeOutput::Warning(warning) => {
                    renderer.render_defmt_warning(channel, &warning, Instant::now())?
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

/// Attaches to the target's RTT control block during session startup.
pub(crate) fn attach_initial_rtt(core: &mut Core, discovery: &RttDiscovery) -> Result<Rtt> {
    ensure_rtt_compatible_target(core.is_64_bit())?;
    log::info!("attaching to RTT...");
    let rtt = discovery
        .attach(core, RTT_TIMEOUT)
        .context("Error attaching to RTT")?;
    log::info!("found control block at {:#010x}", rtt.ptr());
    Ok(rtt)
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

/// Bidirectional RTT channel I/O for one attached target session.
///
/// `session.rs` sequences lifecycle operations (reset, reattach, renderer and
/// input restart); this type only performs target-side I/O and discovery.
pub(crate) struct TargetIo<'probe, 'defmt> {
    core: Core<'probe>,
    rtt: Rtt,
    discovery: RttDiscovery,
    up_specs: Vec<ChannelSpec>,
    down_channel: Option<ChannelId>,
    readers: Vec<UpChannelReader<'defmt>>,
}

impl<'probe, 'defmt> TargetIo<'probe, 'defmt> {
    pub(crate) fn new(core: Core<'probe>, rtt: Rtt, config: &'defmt SessionConfig) -> Result<Self> {
        let readers = config
            .up_specs
            .iter()
            .copied()
            .map(|spec| UpChannelReader::new(spec, config.defmt.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            core,
            rtt,
            discovery: config.discovery.clone(),
            up_specs: config.up_specs.clone(),
            down_channel: config.down_channel,
            readers,
        })
    }

    pub(crate) fn reset_and_reattach(&mut self) -> Result<()> {
        self.core
            .halt(TARGET_HALT_TIMEOUT)
            .context("Error halting target before reset")?;
        Rtt::clear_control_block(&mut self.core, &ScanRegion::Exact(self.rtt.ptr()))
            .context("Error clearing stale RTT control block before reset")?;
        self.core.reset().context("Error resetting target")?;
        self.rtt = self
            .discovery
            .attach(&mut self.core, RTT_TIMEOUT)
            .context("Error reattaching to RTT after target reset")?;
        Ok(())
    }

    /// Reattaches after the target restarted and RTT state changed under us.
    pub(crate) fn reattach(&mut self) -> Result<()> {
        self.rtt = self
            .discovery
            .attach(&mut self.core, RTT_TIMEOUT)
            .context("Error reattaching to RTT after target restart")?;
        Ok(())
    }

    /// Validates configured channels and restarts decoding for a new epoch.
    pub(crate) fn reset_epoch(&mut self) -> Result<()> {
        self.validate_channels()?;
        for reader in &mut self.readers {
            reader.restart();
        }
        Ok(())
    }

    pub(crate) fn validate_channels(&mut self) -> Result<()> {
        validate_up_specs(&mut self.rtt, &self.up_specs)?;
        if let Some(down_channel) = self.down_channel {
            if channel_by_number(self.rtt.down_channels(), down_channel).is_none() {
                bail!("down channel {down_channel} does not exist.");
            }
        }
        Ok(())
    }

    /// Writes as much of `data` as the target down channel accepts and returns
    /// the number of bytes consumed.
    pub(crate) fn write_down(&mut self, data: &[u8]) -> Result<usize> {
        let Some(channel_id) = self.down_channel else {
            return Ok(0);
        };
        if data.is_empty() {
            return Ok(0);
        }
        let Some(channel) = channel_by_number(self.rtt.down_channels(), channel_id) else {
            return Ok(0);
        };
        channel
            .write(&mut self.core, data)
            .context("Error writing to RTT")
    }

    pub(crate) fn poll<W: Write>(&mut self, renderer: &mut Renderer<W>) -> Result<PollOutcome> {
        let mut stats = PollStats::default();
        let mut budget = RTT_READ_BUDGET_PER_POLL;

        while budget > 0 {
            let mut made_progress = false;

            for reader in self.readers.iter_mut() {
                if budget == 0 {
                    break;
                }
                let max = reader.buffer.len().min(budget);
                let count = match channel_by_number(self.rtt.up_channels(), reader.channel) {
                    Some(channel) => {
                        match channel.read(&mut self.core, &mut reader.buffer[..max]) {
                            Ok(count) => count,
                            Err(RttError::ReadPointerChanged) => return Ok(PollOutcome::Reattach),
                            Err(error) => {
                                return Err(error).with_context(|| {
                                    format!("Error reading from RTT up channel {}", reader.channel)
                                });
                            }
                        }
                    }
                    None => 0,
                };
                if count == 0 {
                    // No new bytes, but a defmt decoder may still be holding a
                    // complete frame from a previous partial read — drain it.
                    while reader
                        .decoder
                        .process_defmt(reader.channel, &[], renderer)?
                    {}
                    continue;
                }

                made_progress = true;
                budget -= count;
                stats.bytes += count;
                renderer.log_raw_bytes(reader.channel, &reader.buffer[..count])?;

                match &mut reader.decoder {
                    ChannelDecoder::Terminal(stream) => {
                        let styled = renderer.is_interactive();
                        let chunk = stream.consume_chunk(&reader.buffer[..count], styled);
                        renderer.render_terminal_event(reader.channel, chunk, Instant::now())?;
                        stats.messages += 1;
                    }
                    decoder @ ChannelDecoder::Defmt { .. } => {
                        let hit_frame_limit = decoder.process_defmt(
                            reader.channel,
                            &reader.buffer[..count],
                            renderer,
                        )?;
                        if hit_frame_limit {
                            while decoder.process_defmt(reader.channel, &[], renderer)? {}
                        }
                    }
                }
            }

            if !made_progress {
                break;
            }
        }

        Ok(PollOutcome::Data(stats))
    }
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec]) -> Result<()> {
    for spec in specs {
        let channel = ChannelId::from_cli(spec.index, "up")?;

        if channel_by_number(rtt.up_channels(), channel).is_none() {
            bail!("up channel {} does not exist.", spec.index);
        }
    }

    Ok(())
}
