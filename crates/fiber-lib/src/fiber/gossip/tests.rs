use super::*;
use crate::{
    fiber::{
        gossip_policy::{ByteRateLimitConfig, GossipBanConfig},
        types::ChannelUpdateChannelFlags,
    },
    tests::gen_utils::gen_rand_fiber_public_key,
    ChannelTestContext,
};

fn chargeable_response(id: u64) -> GossipMessage {
    GossipMessage::QueryBroadcastMessagesResult(QueryBroadcastMessagesResult {
        id,
        messages: Vec::new(),
        missing_queries: vec![0, 1, 2],
    })
}

fn outbound_policy(global_burst_bytes: u64, peer_burst_bytes: u64) -> GossipPolicyState {
    GossipPolicyState::new(GossipPolicyConfig {
        ban: GossipBanConfig::default(),
        outbound_global: ByteRateLimitConfig {
            rate_bytes_per_sec: 1,
            burst_bytes: global_burst_bytes,
        },
        outbound_peer: ByteRateLimitConfig {
            rate_bytes_per_sec: 1,
            burst_bytes: peer_burst_bytes,
        },
        outbound_delay_queue_capacity: 16,
        inbound_channel_update: ChannelUpdateRateLimitConfig::default(),
    })
}

#[test]
fn test_broadcast_result_len_validation_enforces_request_and_protocol_limit() {
    assert!(is_valid_broadcast_message_result_len(100, 100));
    assert!(!is_valid_broadcast_message_result_len(101, 100));
    assert!(!is_valid_broadcast_message_result_len(
        MAX_NUM_OF_BROADCAST_MESSAGES as usize + 1,
        MAX_NUM_OF_BROADCAST_MESSAGES as usize + 1
    ));
}

#[test]
fn test_query_result_validation_enforces_query_count_and_missing_indexes() {
    assert!(is_valid_query_broadcast_messages_result(
        &QueryBroadcastMessagesResult {
            id: 0,
            messages: vec![],
            missing_queries: vec![0, 1],
        },
        2,
    ));
    assert!(!is_valid_query_broadcast_messages_result(
        &QueryBroadcastMessagesResult {
            id: 0,
            messages: vec![],
            missing_queries: vec![0, 1],
        },
        1,
    ));
    assert!(!is_valid_query_broadcast_messages_result(
        &QueryBroadcastMessagesResult {
            id: 0,
            messages: vec![],
            missing_queries: vec![2],
        },
        2,
    ));
}

#[tokio::test]
async fn test_dependency_filter_replays_only_messages_with_returned_dependencies() {
    let channel_context_1 = ChannelTestContext::gen().await;
    let channel_context_2 = ChannelTestContext::gen().await;
    let update_with_dependency =
        BroadcastMessage::ChannelUpdate(channel_context_1.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            Some(1_000),
        ));
    let update_without_dependency =
        BroadcastMessage::ChannelUpdate(channel_context_2.create_channel_update_of_node1(
            ChannelUpdateChannelFlags::empty(),
            42,
            42,
            42,
            Some(1_000),
        ));

    let replayed = filter_messages_with_returned_dependencies(
        &[BroadcastMessage::ChannelAnnouncement(
            channel_context_1.channel_announcement.clone(),
        )],
        &[update_with_dependency.clone(), update_without_dependency],
    );

    assert_eq!(replayed, vec![update_with_dependency]);
}

#[test]
fn test_delayed_outbound_queue_rejects_peer_byte_budget_overflow() {
    let peer = gen_rand_fiber_public_key();
    let message = chargeable_response(0);
    let bytes = outbound_gossip_message_cost(&message).expect("chargeable response");
    assert!(bytes > 0);

    let mut policy = outbound_policy(bytes * 10, bytes);
    let mut delayed_messages = DelayedGossipMessageQueue::new(16);

    assert!(reserve_or_delay_outbound_message(
        &mut policy,
        &mut delayed_messages,
        &peer,
        message.clone(),
        0,
    )
    .expect("immediate send")
    .is_some());
    assert!(reserve_or_delay_outbound_message(
        &mut policy,
        &mut delayed_messages,
        &peer,
        message.clone(),
        0,
    )
    .expect("first delayed payload")
    .is_none());
    assert!(
        reserve_or_delay_outbound_message(&mut policy, &mut delayed_messages, &peer, message, 0,)
            .is_err(),
        "queued bytes for one peer must be capped, not only queued item count"
    );
}

#[test]
fn test_delayed_outbound_queue_rejects_global_byte_budget_overflow() {
    let peer1 = gen_rand_fiber_public_key();
    let peer2 = gen_rand_fiber_public_key();
    let message = chargeable_response(0);
    let bytes = outbound_gossip_message_cost(&message).expect("chargeable response");
    assert!(bytes > 0);

    let mut policy = outbound_policy(bytes, bytes * 10);
    let mut delayed_messages = DelayedGossipMessageQueue::new(16);

    assert!(reserve_or_delay_outbound_message(
        &mut policy,
        &mut delayed_messages,
        &peer1,
        message.clone(),
        0,
    )
    .expect("immediate send")
    .is_some());
    assert!(reserve_or_delay_outbound_message(
        &mut policy,
        &mut delayed_messages,
        &peer1,
        message.clone(),
        0,
    )
    .expect("first delayed payload")
    .is_none());
    assert!(
        reserve_or_delay_outbound_message(&mut policy, &mut delayed_messages, &peer2, message, 0,)
            .is_err(),
        "queued bytes across peers must be globally capped"
    );
}

impl<S, C> GossipService<S, C>
where
    S: GossipMessageStore + Clone + Send + Sync + 'static,
    C: CkbChainClient + Clone + Send + Sync + 'static,
{
    pub(crate) fn get_extended_actor(&self) -> &ActorRef<ExtendedGossipMessageStoreMessage> {
        &self.extended_store.actor
    }
}
