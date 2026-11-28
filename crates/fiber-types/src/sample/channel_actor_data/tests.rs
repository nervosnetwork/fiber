use super::*;
use crate::channel::{ChannelImmutableData, ChannelMutableData};

#[test]
fn test_channel_actor_data_samples_roundtrip() {
    ChannelActorData::verify_samples_roundtrip(42);
}

#[test]
fn test_split_records_preserve_the_merged_view() {
    let channel = sample_v2(42);
    let (immutable, mutable) = channel.clone().into_parts();
    let immutable: ChannelImmutableData =
        bincode::deserialize(&bincode::serialize(&immutable).unwrap()).unwrap();
    let mutable: ChannelMutableData =
        bincode::deserialize(&bincode::serialize(&mutable).unwrap()).unwrap();
    let merged = ChannelActorData::from_parts(immutable, mutable);
    assert_eq!(
        bincode::serialize(&merged).unwrap(),
        bincode::serialize(&channel).unwrap()
    );
}
