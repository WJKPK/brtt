//! RTT support and automatic control-block discovery.

use probe_rs::{config::MemoryRegion, Core, MemoryInterface};
use std::thread;
use std::time::{Duration, Instant};

pub use probe_rs::rtt::{try_attach_to_rtt, try_attach_to_rtt_shared, Error, Rtt, ScanRegion};

const SCAN_CHUNK_SIZE: usize = 32 * 1024;
const MIN_SCAN_CHUNK_SIZE: usize = 4 * 1024;
const RTT_MAGIC_OVERLAP: usize = Rtt::RTT_ID.len() - 1;

/// How brtt discovers the RTT control block for a session.
///
/// `Fixed` attaches to a known region, while `Incremental` scans a region in
/// chunks and retries until the attach timeout expires.
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

    /// Attaches to the target's RTT control block using this discovery mode.
    pub fn attach(&self, core: &mut Core<'_>, timeout: Duration) -> Result<Rtt, Error> {
        match self {
            Self::Fixed(_) => try_attach_to_rtt(core, timeout, self.region()),
            Self::Incremental(_) => try_attach_to_rtt_incremental(core, timeout, self.region()),
        }
    }
}

/// Attaches to the first valid RTT block found by scanning memory incrementally.
pub fn attach_region_incremental(core: &mut Core<'_>, region: &ScanRegion) -> Result<Rtt, Error> {
    let started = Instant::now();
    let ranges = match region {
        ScanRegion::Ram => core
            .memory_regions()
            .filter_map(MemoryRegion::as_ram_region)
            .filter(|region| !region.is_alias)
            .map(|region| region.range.clone())
            .collect(),
        ScanRegion::Ranges(ranges) => ranges.clone(),
        ScanRegion::Exact(address) => {
            return Rtt::attach_region(core, &ScanRegion::Exact(*address))
        }
    };
    if ranges.is_empty() {
        return Err(Error::NoControlBlockLocation);
    }

    let mut bytes_read = 0usize;
    let mut chunks_read = 0usize;
    let mut chunk = vec![0u8; SCAN_CHUNK_SIZE];
    let mut combined = Vec::with_capacity(SCAN_CHUNK_SIZE + RTT_MAGIC_OVERLAP);
    let mut overlap = Vec::with_capacity(RTT_MAGIC_OVERLAP);

    for range in ranges {
        if range.end < range.start {
            continue;
        }
        overlap.clear();

        let mut address = range.start;
        while address < range.end {
            let remaining = usize::try_from(range.end - address).unwrap_or(usize::MAX);
            let requested_len = remaining.min(SCAN_CHUNK_SIZE);
            let Some(read_len) = read_scan_chunk(core, address, &mut chunk, requested_len) else {
                break;
            };
            bytes_read += read_len;
            chunks_read += 1;

            let overlap_len = overlap.len();
            combined.clear();
            combined.extend_from_slice(&overlap);
            combined.extend_from_slice(&chunk[..read_len]);
            let first_new_address = address.saturating_sub(overlap_len as u64);
            let mut search_from = 0;
            while let Some(relative) = find_magic(&combined[search_from..]) {
                let offset = search_from + relative;
                let candidate = first_new_address + offset as u64;
                search_from = offset + 1;

                match Rtt::attach_at(core, candidate) {
                    Ok(rtt) => {
                        log::debug!(
                            "automatic RTT scan found control block at {candidate:#010x} after reading {bytes_read} bytes in {chunks_read} chunks ({:?})",
                            started.elapsed()
                        );
                        return Ok(rtt);
                    }
                    Err(Error::ControlBlockNotFound | Error::ControlBlockCorrupted(_)) => {}
                    Err(error) => return Err(error),
                }
            }

            overlap.clear();
            let tail_start = combined.len().saturating_sub(RTT_MAGIC_OVERLAP);
            overlap.extend_from_slice(&combined[tail_start..]);
            address += read_len as u64;
        }
    }

    log::debug!(
        "automatic RTT scan found no control block after reading {bytes_read} bytes in {chunks_read} chunks ({:?})",
        started.elapsed()
    );
    Err(Error::ControlBlockNotFound)
}

/// Reads one scan chunk, shrinking the read on failure.
///
/// Returns the number of bytes read, or `None` when the chunk is unreadable
/// (which stops scanning only the current range).
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
                log::debug!(
                    "automatic RTT scan read at {address:#010x} failed; retrying with {read_len} bytes: {error}"
                );
            }
            Err(error) => {
                log::debug!(
                    "automatic RTT scan could not read range starting at {address:#010x}: {error}"
                );
                return None;
            }
        }
    }
}

fn find_magic(data: &[u8]) -> Option<usize> {
    memchr::memmem::find(data, &Rtt::RTT_ID)
}

/// Retries incremental automatic RTT attachment until `timeout` expires.
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
                thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
#[path = "../tests/rtt.rs"]
mod tests;
