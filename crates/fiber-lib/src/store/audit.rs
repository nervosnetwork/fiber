use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber_types::{ChannelAuditInfo, ChannelState, RestoreAuditMap};

pub fn create_restore_audit_map<S: ChannelActorStateStore>(store: &S) -> RestoreAuditMap {
    let mut audit_map = RestoreAuditMap::new();

    for channel in store.get_all_channel_states() {
        if is_risk_of_penalty(&channel.state) {
            audit_map.add_channel(
                channel.id,
                ChannelAuditInfo {
                    local_commitment_number: channel.commitment_numbers.get_local(),
                },
            );
        }
    }
    audit_map
}

fn is_risk_of_penalty(state: &ChannelState) -> bool {
    !matches!(
        state,
        ChannelState::NegotiatingFunding(_)
            | ChannelState::CollaboratingFundingTx(_)
            | ChannelState::Closed(_)
    )
}
