use super::*;

#[test]
fn test_channel_open_record_samples_roundtrip() {
    ChannelOpenRecord::verify_samples_roundtrip(42);
}
