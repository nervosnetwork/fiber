use super::*;

pub(crate) fn verified_tracked_settlement_tlcs(
    lock: &Script,
    channel: &ChannelData,
    expected_remote: bool,
) -> Option<Vec<TrackedSettlementTlc>> {
    let (for_remote, _, snapshot) = verify_and_select_settlement_data(channel, lock)?;
    if for_remote != expected_remote {
        return None;
    }
    Some(tracked_settlement_tlcs(
        snapshot,
        for_remote,
        channel.channel_features,
    ))
}
