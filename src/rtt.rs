//! RTT support and automatic control-block discovery.

use probe_rs::{config::MemoryRegion, Core, MemoryInterface};
use std::ops::Range;
use std::thread;
use std::time::{Duration, Instant};

pub use probe_rs::rtt::{try_attach_to_rtt, try_attach_to_rtt_shared, Error, Rtt, ScanRegion};

const SCAN_CHUNK_SIZE: usize = 32 * 1024;
const MIN_SCAN_CHUNK_SIZE: usize = 4 * 1024;
const RTT_MAGIC_OVERLAP: usize = Rtt::RTT_ID.len() - 1;
const RTT_RETRY_DELAY: Duration = Duration::from_millis(50);
/// Limit pending-core scanning to one memory transfer per retry, not one RAM sweep.
pub const SCAN_CHUNKS_PER_TICK: usize = 1;

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
}

/// A resumable automatic scan. Owns its scratch buffers so each pending retry
/// performs bounded work without allocating or restarting at the beginning.
pub struct IncrementalScan {
    ranges: Vec<Range<u64>>,
    range: usize,
    address: u64,
    overlap: Vec<u8>,
    chunk: Vec<u8>,
    combined: Vec<u8>,
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
            overlap: Vec::with_capacity(RTT_MAGIC_OVERLAP),
            chunk: vec![0; SCAN_CHUNK_SIZE],
            combined: Vec::with_capacity(SCAN_CHUNK_SIZE + RTT_MAGIC_OVERLAP),
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
            let read_len = read_scan_chunk(core, self.address, &mut self.chunk, requested_len);
            chunks += 1;
            let Some(read_len) = read_len else {
                // Only this minimum-sized page is unreadable; later pages of
                // the same RAM range may still contain a valid control block.
                self.address = skip_unreadable(self.address, requested_len);
                self.overlap.clear();
                continue;
            };
            self.combined.clear();
            self.combined.extend_from_slice(&self.overlap);
            self.combined.extend_from_slice(&self.chunk[..read_len]);
            let first_address = self.address - self.overlap.len() as u64;
            let mut search_from = 0;
            while let Some(relative) = find_magic(&self.combined[search_from..]) {
                let offset = search_from + relative;
                let candidate = first_address + offset as u64;
                search_from = offset + 1;
                match Rtt::attach_at(core, candidate) {
                    Ok(rtt) => return Ok(Some(rtt)),
                    Err(Error::ControlBlockNotFound | Error::ControlBlockCorrupted(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            self.overlap.clear();
            let tail_start = self.combined.len().saturating_sub(RTT_MAGIC_OVERLAP);
            self.overlap.extend_from_slice(&self.combined[tail_start..]);
            self.address += read_len as u64;
        }
        Err(Error::ControlBlockNotFound)
    }

    fn next_range(&mut self) {
        self.range += 1;
        self.overlap.clear();
        if let Some(range) = self.ranges.get(self.range) {
            self.address = range.start;
        }
    }
}

fn skip_unreadable(address: u64, requested_len: usize) -> u64 {
    address + requested_len.min(MIN_SCAN_CHUNK_SIZE) as u64
}

pub fn attach_region_incremental(core: &mut Core<'_>, region: &ScanRegion) -> Result<Rtt, Error> {
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

pub fn try_attach_to_rtt_incremental(
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
