use super::*;

#[test]
fn test_channel_data_samples_roundtrip() {
    ChannelData::verify_samples_roundtrip(42);
}
