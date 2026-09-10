use tracing::warn;

use crate::fiber::channel::ChannelActorStateStore;
use crate::fiber_types::{Hash256, SettlementData};
use crate::store::{NodeNamespace, Store};
use crate::watchtower::WatchtowerStore;

use super::{tenant_watchtower_node_id, HostedTenantRecord};

/// Create the host watchtower row for a hosted private channel if it is missing.
///
/// Existing rows are left unchanged so later revocation and signer state survive
/// a repeated ChannelOnline. Evicting the tenant runtime must not call this with
/// a remove.
pub fn ensure_hosted_watch_channel(
    store: &Store,
    tenant: &HostedTenantRecord,
    channel_id: Hash256,
) -> Result<(), String> {
    let node_id = tenant_watchtower_node_id(&tenant.tenant_pubkey);
    if store.get_watch_channel(&node_id, &channel_id).is_some() {
        return Ok(());
    }

    let tenant_store = store.namespaced(NodeNamespace::hosted_tenant(tenant.tenant_id.as_str()));
    if let Some(state) = tenant_store.get_channel_actor_state(&channel_id) {
        let params = state.hosted_watch_channel_params()?;
        store.insert_watch_channel(
            node_id,
            channel_id,
            params.funding_udt_type_script,
            params.local_settlement_key,
            params.local_settlement_key_pubkey,
            params.remote_settlement_key,
            params.local_funding_pubkey,
            params.remote_funding_pubkey,
            params.settlement_data,
        );
        return Ok(());
    }

    let Some(state) = store.get_channel_actor_state(&channel_id) else {
        warn!(
            tenant_id = %tenant.tenant_id,
            %channel_id,
            "Cannot ensure hosted watch: channel state is not in the host store"
        );
        return Ok(());
    };
    let params = state.hosted_watch_channel_params()?;
    if !params.settlement_data.tlcs.is_empty() {
        warn!(
            tenant_id = %tenant.tenant_id,
            %channel_id,
            "Cannot invert a Public T settlement that still has pending TLCs"
        );
        return Ok(());
    }
    store.insert_watch_channel(
        node_id,
        channel_id,
        params.funding_udt_type_script,
        None,
        params.remote_settlement_key,
        params.local_settlement_key_pubkey,
        params.remote_funding_pubkey,
        params.local_funding_pubkey,
        invert_empty_settlement(params.settlement_data),
    );
    Ok(())
}

fn invert_empty_settlement(data: SettlementData) -> SettlementData {
    SettlementData {
        local_amount: data.remote_amount,
        remote_amount: data.local_amount,
        tlcs: Vec::new(),
    }
}
