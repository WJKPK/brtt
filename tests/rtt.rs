use super::*;

#[test]
fn magic_is_found_when_split_at_each_chunk_boundary() {
    for split in 1..Rtt::RTT_ID.len() {
        let mut scan = IncrementalScan {
            ranges: Vec::new(),
            range: 0,
            address: 0,
            overlap: 0,
            buffer: vec![0; SCAN_CHUNK_SIZE + RTT_MAGIC_OVERLAP],
        };
        scan.buffer[SCAN_CHUNK_SIZE - split..SCAN_CHUNK_SIZE]
            .copy_from_slice(&Rtt::RTT_ID[..split]);
        scan.retain_overlap(SCAN_CHUNK_SIZE);
        let end = scan.overlap + Rtt::RTT_ID.len() - split;
        scan.buffer[scan.overlap..end].copy_from_slice(&Rtt::RTT_ID[split..]);
        assert_eq!(
            find_magic(&scan.buffer[..end]),
            Some(RTT_MAGIC_OVERLAP - split)
        );
    }
}

#[test]
fn magic_is_not_found_without_a_complete_identifier() {
    assert_eq!(find_magic(&Rtt::RTT_ID[..Rtt::RTT_ID.len() - 1]), None);
}
