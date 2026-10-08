//! RTT support and automatic control-block discovery.

use probe_rs::{config::MemoryRegion, Core, MemoryInterface};
use std::ops::Range;
use std::thread;
use std::time::{Duration, Instant};

use probe_rs::rtt::try_attach_to_rtt;
pub use probe_rs::rtt::{Error, Rtt, ScanRegion};

const SCAN_CHUNK_SIZE: usize = 32 * 1024;
const MIN_SCAN_CHUNK_SIZE: usize = 4 * 1024;
const RTT_MAGIC_OVERLAP: usize = Rtt::RTT_ID.len() - 1;
const RTT_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub enum RttDiscovery {
    Fixed(ScanRegion),
    Incremental(ScanRegion),
}

impl RttDiscovery {
    fn region(&self) -> &ScanRegion {
        match self {
            Self::Fixed(region) | Self::Incremental(region) => region,
        }
    }

    pub fn attach(&self, core: &mut Core<'_>, timeout: Duration) -> Result<Rtt, Error> {
        match self {
            Self::Fixed(_) => try_attach_to_rtt(core, timeout, self.region()),
            Self::Incremental(_) => try_attach_to_rtt_incremental(core, timeout, self.region()),
        }
    }

    /// Creates a resumable scan for a background attach attempt.
    /// Exact fixed locations use the direct attach path instead.
    pub fn incremental_scan(&self, core: &Core<'_>) -> Result<Option<IncrementalScan>, Error> {
        match self {
            Self::Fixed(ScanRegion::Exact(_)) => Ok(None),
            _ => IncrementalScan::new(core, self.region()).map(Some),
        }
    }
}

const RTT_CHANNEL_COUNTS_OFFSET: u64 = 16;
const RTT_CHANNELS_OFFSET: u64 = 24;
const RTT_CHANNEL_DESCRIPTOR_SIZE: u64 = 24;

/// Static 32-bit RTT metadata used to detect stale attachments.
/// Read/write offsets are deliberately excluded because they change during I/O.
pub struct RttMetadata {
    counts: [u32; 2],
    descriptors: Vec<(u64, [u32; 3])>,
}

impl RttMetadata {
    pub fn capture(
        core: &mut Core<'_>,
        rtt: &Rtt,
        up_channels: impl ExactSizeIterator<Item = usize>,
        down_channel: Option<usize>,
    ) -> Result<Self, Error> {
        let mut counts = [0; 2];
        core.read_32(rtt.ptr() + RTT_CHANNEL_COUNTS_OFFSET, &mut counts)?;
        let mut descriptors =
            Vec::with_capacity(up_channels.len() + usize::from(down_channel.is_some()));
        let down_descriptor = down_channel.map(|channel| u64::from(counts[0]) + channel as u64);
        for index in up_channels
            .map(|channel| channel as u64)
            .chain(down_descriptor)
        {
            let address = rtt.ptr() + RTT_CHANNELS_OFFSET + index * RTT_CHANNEL_DESCRIPTOR_SIZE;
            descriptors.push((address, read_static_metadata(core, address)?));
        }
        Ok(Self {
            counts,
            descriptors,
        })
    }

    pub fn is_valid(&self, core: &mut Core<'_>, rtt: &Rtt) -> Result<bool, Error> {
        let mut id = [0; Rtt::RTT_ID.len()];
        core.read(rtt.ptr(), &mut id)?;
        if id != Rtt::RTT_ID {
            return Ok(false);
        }
        let mut counts = [0; 2];
        core.read_32(rtt.ptr() + RTT_CHANNEL_COUNTS_OFFSET, &mut counts)?;
        if counts != self.counts {
            return Ok(false);
        }
        for &(address, expected) in &self.descriptors {
            if read_static_metadata(core, address)? != expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn read_static_metadata(core: &mut Core<'_>, address: u64) -> Result<[u32; 3], Error> {
    let mut fields = [0; 3];
    core.read_32(address, &mut fields)?;
    Ok(fields)
}

/// A resumable automatic scan. Owns its scratch buffers so each pending retry
/// performs bounded work without allocating or restarting at the beginning.
pub struct IncrementalScan {
    ranges: Vec<Range<u64>>,
    range: usize,
    address: u64,
    overlap: usize,
    buffer: Vec<u8>,
}

impl IncrementalScan {
    pub fn new(core: &Core<'_>, region: &ScanRegion) -> Result<Self, Error> {
        let ranges: Vec<Range<u64>> = match region {
            ScanRegion::Ram => core
                .memory_regions()
                .filter_map(MemoryRegion::as_ram_region)
                .filter(|region| !region.is_alias)
                .map(|region| region.range.clone())
                .collect(),
            ScanRegion::Ranges(ranges) => ranges.clone(),
            ScanRegion::Exact(_) => return Err(Error::NoControlBlockLocation),
        };
        if ranges.is_empty() {
            return Err(Error::NoControlBlockLocation);
        }
        let address = ranges[0].start;
        Ok(Self {
            ranges,
            range: 0,
            address,
            overlap: 0,
            buffer: vec![0; SCAN_CHUNK_SIZE + RTT_MAGIC_OVERLAP],
        })
    }

    /// Returns `None` while more memory remains; a completed sweep returns
    /// `ControlBlockNotFound`. The caller can then start a fresh sweep later.
    pub fn step(&mut self, core: &mut Core<'_>, max_chunks: usize) -> Result<Option<Rtt>, Error> {
        let mut chunks = 0;
        while self.range < self.ranges.len() {
            let end = self.ranges[self.range].end;
            if self.address >= end {
                self.next_range();
                continue;
            }
            if chunks >= max_chunks {
                return Ok(None);
            }
            let remaining = usize::try_from(end - self.address).unwrap_or(usize::MAX);
            let requested_len = remaining.min(SCAN_CHUNK_SIZE);
            let read_len = read_scan_chunk(
                core,
                self.address,
                &mut self.buffer[self.overlap..],
                requested_len,
            );
            chunks += 1;
            let Some(read_len) = read_len else {
                // Only this minimum-sized page is unreadable; later pages of
                // the same RAM range may still contain a valid control block.
                self.address = skip_unreadable(self.address, requested_len);
                self.overlap = 0;
                continue;
            };
            let end = self.overlap + read_len;
            let first_address = self.address - self.overlap as u64;
            let mut search_from = 0;
            while let Some(relative) = find_magic(&self.buffer[search_from..end]) {
                let offset = search_from + relative;
                let candidate = first_address + offset as u64;
                search_from = offset + 1;
                match Rtt::attach_at(core, candidate) {
                    Ok(rtt) => return Ok(Some(rtt)),
                    Err(Error::ControlBlockNotFound | Error::ControlBlockCorrupted(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            self.retain_overlap(end);
            self.address += read_len as u64;
        }
        Err(Error::ControlBlockNotFound)
    }

    fn retain_overlap(&mut self, end: usize) {
        self.overlap = end.min(RTT_MAGIC_OVERLAP);
        self.buffer.copy_within(end - self.overlap..end, 0);
    }

    fn next_range(&mut self) {
        self.range += 1;
        self.overlap = 0;
        if let Some(range) = self.ranges.get(self.range) {
            self.address = range.start;
        }
    }
}

fn skip_unreadable(address: u64, requested_len: usize) -> u64 {
    address + requested_len.min(MIN_SCAN_CHUNK_SIZE) as u64
}

fn attach_region_incremental(core: &mut Core<'_>, region: &ScanRegion) -> Result<Rtt, Error> {
    if let ScanRegion::Exact(address) = region {
        return Rtt::attach_region(core, &ScanRegion::Exact(*address));
    }
    let mut scan = IncrementalScan::new(core, region)?;
    loop {
        if let Some(rtt) = scan.step(core, usize::MAX)? {
            return Ok(rtt);
        }
    }
}

fn read_scan_chunk(
    core: &mut Core<'_>,
    address: u64,
    chunk: &mut [u8],
    requested_len: usize,
) -> Option<usize> {
    if requested_len == 0 {
        return None;
    }
    let mut read_len = requested_len;
    loop {
        match core.read(address, &mut chunk[..read_len]) {
            Ok(()) => return Some(read_len),
            Err(error) if read_len > MIN_SCAN_CHUNK_SIZE => {
                read_len = (read_len / 2).max(MIN_SCAN_CHUNK_SIZE);
                log::debug!("automatic RTT scan read at {address:#010x} failed; retrying with {read_len} bytes: {error}");
            }
            Err(error) => {
                log::debug!("automatic RTT scan could not read chunk at {address:#010x}: {error}");
                return None;
            }
        }
    }
}

fn find_magic(data: &[u8]) -> Option<usize> {
    memchr::memmem::find(data, &Rtt::RTT_ID)
}

fn try_attach_to_rtt_incremental(
    core: &mut Core<'_>,
    timeout: Duration,
    region: &ScanRegion,
) -> Result<Rtt, Error> {
    let started = Instant::now();
    loop {
        match attach_region_incremental(core, region) {
            Err(Error::NoControlBlockLocation) => return Err(Error::NoControlBlockLocation),
            Err(error) if started.elapsed() < timeout => {
                log::debug!(
                    "failed to initialize RTT automatically: {error}. Retrying until timeout."
                );
                thread::sleep(RTT_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

#[cfg(test)]
#[path = "../tests/rtt.rs"]
mod tests;
