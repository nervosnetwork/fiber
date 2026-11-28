use super::*;
use crate::fiber_types::{HashAlgorithm, PrevTlcInfo};
use crate::gen_rand_sha256_hash;
use crate::tests::gen_utils::gen_rand_fiber_public_key;
use crate::tests::test_utils::generate_store;

#[test]
fn get_by_payment_hash_prefers_succeeded_on_same_ms_tie() {
    let (store, _dir) = generate_store();
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let payment_hash = gen_rand_sha256_hash();
    let channel_a = Hash256::from([1u8; 32]);
    let channel_b = Hash256::from([2u8; 32]);

    let make_delivery =
        |channel_id: Hash256, tlc_id: u64, status: LspPaymentDeliveryStatus| LspPaymentDelivery {
            payment_hash,
            tenant_id: TenantId::new("tenant-tie").unwrap(),
            private_channel_id: gen_rand_sha256_hash(),
            status,
            request: TrampolineForwardingRequest {
                next_node_id: gen_rand_fiber_public_key(),
                amount_to_forward: 100,
                payment_hash,
                tlc_expiry_delta: 3600,
                tlc_expiry_limit: 7200,
                build_max_fee_amount: 10,
                max_parts: None,
                udt_type_script: None,
                remaining_trampoline_onion: vec![],
                previous_tlc: PrevTlcInfo::new_with_shared_secret(
                    channel_id, tlc_id, 10, [0u8; 32],
                ),
                hash_algorithm: HashAlgorithm::CkbHash,
                max_outgoing_tlc_expiry: 100_000,
            },
            buffer_deadline: 10_000,
            created_at: 5_000,
            updated_at: 5_000,
            attempt_count: 1,
            last_error: None,
            last_error_code: None,
        };

    // Even though channel_b has a larger key, channel_a is Succeeded and must win on equal timestamps.
    store
        .put_lsp_payment_delivery(&make_delivery(
            channel_a,
            0,
            LspPaymentDeliveryStatus::Succeeded,
        ))
        .unwrap();
    store
        .put_lsp_payment_delivery(&make_delivery(
            channel_b,
            9,
            LspPaymentDeliveryStatus::Failed {
                reason: "downstream failed".to_string(),
            },
        ))
        .unwrap();

    let selected = manager
        .get_by_payment_hash(&payment_hash)
        .unwrap()
        .expect("selected delivery");
    assert_eq!(selected.status, LspPaymentDeliveryStatus::Succeeded);
    assert_eq!(selected.key().incoming_channel_id, channel_a);
}

#[test]
fn list_pending_uses_pending_index_without_decoding_final_deliveries() {
    let (store, _dir) = generate_store();
    let manager = LspPaymentDeliveryManager::new(store.clone());
    let channel = Hash256::from([7u8; 32]);

    let make_delivery = |tlc_id: u64, status: LspPaymentDeliveryStatus| {
        let payment_hash = gen_rand_sha256_hash();
        LspPaymentDelivery {
            payment_hash,
            tenant_id: TenantId::new("tenant-pending-index").unwrap(),
            private_channel_id: gen_rand_sha256_hash(),
            status,
            request: TrampolineForwardingRequest {
                next_node_id: gen_rand_fiber_public_key(),
                amount_to_forward: 100,
                payment_hash,
                tlc_expiry_delta: 3600,
                tlc_expiry_limit: 7200,
                build_max_fee_amount: 10,
                max_parts: None,
                udt_type_script: None,
                remaining_trampoline_onion: vec![],
                previous_tlc: PrevTlcInfo::new_with_shared_secret(channel, tlc_id, 10, [0u8; 32]),
                hash_algorithm: HashAlgorithm::CkbHash,
                max_outgoing_tlc_expiry: 100_000,
            },
            buffer_deadline: 10_000,
            created_at: 5_000,
            updated_at: 5_000,
            attempt_count: 0,
            last_error: None,
            last_error_code: None,
        }
    };

    let succeeded = make_delivery(1, LspPaymentDeliveryStatus::Deferred);
    let failed = make_delivery(2, LspPaymentDeliveryStatus::Deferred);
    let deferred = make_delivery(3, LspPaymentDeliveryStatus::Deferred);
    let in_flight = make_delivery(4, LspPaymentDeliveryStatus::Deferred);

    for delivery in [&succeeded, &failed, &deferred, &in_flight] {
        store.put_lsp_payment_delivery(delivery).unwrap();
    }
    assert_eq!(manager.list_pending().unwrap().len(), 4);

    // Transition delivery 1 through full lifecycle to Succeeded, and delivery 2 to Failed.
    let key_1 = succeeded.key();
    manager.begin_dispatch(&key_1, 5_001).unwrap();
    manager
        .transition(&key_1, LspPaymentDeliveryStatus::InFlight, 5_002)
        .unwrap();
    manager
        .transition(
            &key_1,
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Success,
                failure: None,
            },
            5_003,
        )
        .unwrap();
    assert_eq!(manager.list_pending().unwrap().len(), 4);
    manager
        .transition(&key_1, LspPaymentDeliveryStatus::Succeeded, 5_004)
        .unwrap();

    let key_2 = failed.key();
    manager
        .transition(
            &key_2,
            LspPaymentDeliveryStatus::Failed {
                reason: "expired".to_string(),
            },
            5_005,
        )
        .unwrap();

    let key_4 = in_flight.key();
    manager.begin_dispatch(&key_4, 5_006).unwrap();
    let in_flight = manager
        .transition(&key_4, LspPaymentDeliveryStatus::InFlight, 5_007)
        .unwrap();

    // Corrupt the primary records of finalized deliveries. A full-table scan
    // fails to decode them, while the pending-only index must ignore them.
    store.put(delivery_key(&key_1), b"corrupt-historical-delivery");
    store.put(delivery_key(&key_2), b"corrupt-historical-delivery");
    assert!(store.list_lsp_payment_deliveries().is_err());

    let pending = manager.list_pending().unwrap();
    assert_eq!(pending, vec![deferred.clone(), in_flight.clone()]);

    // Pending index persists across reopening the delivery manager.
    let reopened = LspPaymentDeliveryManager::new(store.clone());
    assert_eq!(reopened.list_pending().unwrap(), vec![deferred, in_flight]);

    // Completing the remaining pending deliveries empties the pending index.
    let key_3 = pending[0].key();
    reopened
        .transition(
            &key_3,
            LspPaymentDeliveryStatus::Failed {
                reason: "removed".to_string(),
            },
            5_008,
        )
        .unwrap();
    reopened
        .transition(
            &key_4,
            LspPaymentDeliveryStatus::SettlingUpstream {
                payment_status: PaymentStatus::Success,
                failure: None,
            },
            5_009,
        )
        .unwrap();
    reopened
        .transition(&key_4, LspPaymentDeliveryStatus::Succeeded, 5_010)
        .unwrap();
    store.put(delivery_key(&key_3), b"corrupt-historical-delivery");
    store.put(delivery_key(&key_4), b"corrupt-historical-delivery");

    assert!(reopened.list_pending().unwrap().is_empty());
}
