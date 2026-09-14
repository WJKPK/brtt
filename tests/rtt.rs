use super::*;

#[test]
fn magic_is_found_when_split_at_each_chunk_boundary() {
    for split in 1..Rtt::RTT_ID.len() {
        let mut first = vec![0u8; SCAN_CHUNK_SIZE - (Rtt::RTT_ID.len() - split)];
        first.extend_from_slice(&Rtt::RTT_ID[..split]);
        let mut second = Rtt::RTT_ID[split..].to_vec();
        second.extend_from_slice(&[0u8; 4]);

        let mut combined = first[first.len() - RTT_MAGIC_OVERLAP..].to_vec();
        combined.extend_from_slice(&second);
        assert_eq!(find_magic(&combined), Some(RTT_MAGIC_OVERLAP - split));
    }
}

#[test]
fn magic_is_not_found_without_a_complete_identifier() {
    assert_eq!(find_magic(&Rtt::RTT_ID[..Rtt::RTT_ID.len() - 1]), None);
}
