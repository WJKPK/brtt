use super::*;

struct TestChannel(usize);

impl RttChannel for TestChannel {
    fn number(&self) -> usize {
        self.0
    }

    fn name(&self) -> Option<&str> {
        None
    }

    fn buffer_size(&self) -> usize {
        0
    }
}

#[test]
fn channel_lookup_uses_rtt_number_not_slice_index() {
    let mut channels = [TestChannel(1), TestChannel(3)];

    assert_eq!(
        channel_by_number(&mut channels, ChannelId(1))
            .unwrap()
            .number(),
        1
    );
    assert_eq!(
        channel_by_number(&mut channels, ChannelId(3))
            .unwrap()
            .number(),
        3
    );
    assert!(channel_by_number(&mut channels, ChannelId(0)).is_none());
    assert!(channel_by_number(&mut channels, ChannelId(2)).is_none());
}
