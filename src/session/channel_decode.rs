use super::terminal_decode::{DecodedStream, TerminalChunk};
use crate::channel::{ChannelEncoding, ChannelId, CoreChannel, CoreId};
use crate::defmt::{
    decode_frames, DecodeOutput, DecodedFrame, DefmtData, MAX_DECODE_BUFFERED_BYTES,
};
use anyhow::{bail, Result};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UpChannelConfig {
    pub(super) channel: ChannelId,
    pub(super) mode: ChannelEncoding,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PolledChunk<'a> {
    pub(super) source: CoreChannel,
    pub(super) bytes: &'a [u8],
}

/// Output port used by per-core RTT channel decoders. Borrowed frames keep
/// the hot poll path allocation-free and decouple decoding from the renderer.
pub(super) trait UpSink {
    fn raw_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()>;
    fn terminal(&mut self, source: CoreChannel, chunk: TerminalChunk, at: Instant) -> Result<()>;
    fn defmt_frame(&mut self, source: CoreChannel, frame: &DecodedFrame, at: Instant)
        -> Result<()>;
    fn defmt_warning(&mut self, source: CoreChannel, warning: &str, at: Instant) -> Result<()>;
    fn needs_styled_terminal_output(&self) -> bool;
}

pub(super) struct ChannelDecoders<'table> {
    readers: Vec<UpChannelReader<'table>>,
}

impl<'table> ChannelDecoders<'table> {
    pub(super) fn new(
        cores: impl IntoIterator<
            Item = (
                CoreId,
                &'table [UpChannelConfig],
                Option<&'table DefmtData>,
            ),
        >,
    ) -> Result<Self> {
        let mut readers = Vec::new();
        for (core, configs, defmt) in cores {
            for &config in configs {
                readers.push(UpChannelReader::new(config, core, defmt)?);
            }
        }
        Ok(Self { readers })
    }

    pub(super) fn process_chunks<'a, S: UpSink>(
        &mut self,
        chunks: impl IntoIterator<Item = PolledChunk<'a>>,
        sink: &mut S,
    ) -> Result<()> {
        for chunk in chunks {
            self.process_bytes(chunk.source, chunk.bytes, sink)?;
        }
        Ok(())
    }

    pub(super) fn process_bytes<S: UpSink>(
        &mut self,
        source: CoreChannel,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<()> {
        let reader = self
            .readers
            .iter_mut()
            .find(|reader| reader.source == source)
            .expect("polled chunk source must match a configured up channel");
        reader.process_bytes(bytes, sink)
    }

    pub(super) fn reset_core(&mut self, core: CoreId) {
        for reader in &mut self.readers {
            if reader.source.core == core {
                reader.reset();
            }
        }
    }

    pub(super) fn reset_all(&mut self) {
        for reader in &mut self.readers {
            reader.reset();
        }
    }
}

struct UpChannelReader<'table> {
    source: CoreChannel,
    decoder: ChannelDecoder<'table>,
}

enum ChannelDecoder<'table> {
    Terminal(Box<DecodedStream>),
    Defmt {
        data: &'table DefmtData,
        stream: Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>,
        incomplete_bytes: usize,
    },
}

impl<'table> UpChannelReader<'table> {
    fn new(
        config: UpChannelConfig,
        core: CoreId,
        defmt: Option<&'table DefmtData>,
    ) -> Result<Self> {
        let channel = config.channel;
        let decoder = match (config.mode, defmt) {
            (ChannelEncoding::Terminal, _) => {
                ChannelDecoder::Terminal(Box::new(DecodedStream::new()))
            }
            (ChannelEncoding::Defmt, Some(data)) => ChannelDecoder::Defmt {
                data,
                stream: data.table.new_stream_decoder(),
                incomplete_bytes: 0,
            },
            (ChannelEncoding::Defmt, None) => bail!("missing defmt table for channel {channel}"),
        };
        Ok(Self {
            source: CoreChannel { core, channel },
            decoder,
        })
    }

    fn reset(&mut self) {
        match &mut self.decoder {
            ChannelDecoder::Terminal(stream) => **stream = DecodedStream::new(),
            ChannelDecoder::Defmt {
                data,
                stream,
                incomplete_bytes,
            } => {
                *stream = data.table.new_stream_decoder();
                *incomplete_bytes = 0;
            }
        }
    }

    fn process_bytes<S: UpSink>(&mut self, bytes: &[u8], sink: &mut S) -> Result<()> {
        sink.raw_bytes(self.source, bytes)?;
        match &mut self.decoder {
            ChannelDecoder::Terminal(stream) => {
                let chunk = stream.consume_chunk(bytes, sink.needs_styled_terminal_output());
                sink.terminal(self.source, chunk, Instant::now())
            }
            decoder @ ChannelDecoder::Defmt { .. } => {
                decoder.process_defmt(self.source, bytes, sink)
            }
        }
    }
}

impl ChannelDecoder<'_> {
    fn process_defmt<S: UpSink>(
        &mut self,
        source: CoreChannel,
        bytes: &[u8],
        sink: &mut S,
    ) -> Result<()> {
        let Self::Defmt {
            data,
            stream,
            incomplete_bytes,
        } = self
        else {
            return Ok(());
        };
        if !bytes.is_empty()
            && incomplete_bytes.saturating_add(bytes.len()) > MAX_DECODE_BUFFERED_BYTES
        {
            *stream = data.table.new_stream_decoder();
            *incomplete_bytes = 0;
            sink.defmt_warning(
                source,
                "defmt decoder incomplete input exceeded 64 KiB; resetting decoder",
                Instant::now(),
            )?;
        }
        let mut input = bytes;
        let mut made_progress = false;
        let mut restart = false;
        loop {
            let decoded =
                decode_frames(stream.as_mut(), input, data.table.encoding().can_recover());
            input = &[];
            made_progress |= decoded
                .frames
                .iter()
                .any(|item| matches!(item, DecodeOutput::Frame(_)));
            let hit_frame_limit = decoded.hit_frame_limit;
            restart |= decoded.restart;
            for item in decoded.frames {
                match item {
                    DecodeOutput::Frame(frame) => {
                        sink.defmt_frame(source, &frame, Instant::now())?
                    }
                    DecodeOutput::Warning(warning) => {
                        sink.defmt_warning(source, &warning, Instant::now())?
                    }
                }
            }
            if restart || !hit_frame_limit {
                break;
            }
        }
        if restart {
            *stream = data.table.new_stream_decoder();
            *incomplete_bytes = 0;
        } else if made_progress {
            *incomplete_bytes = bytes.len();
        } else {
            *incomplete_bytes = incomplete_bytes.saturating_add(bytes.len());
        }
        Ok(())
    }
}
