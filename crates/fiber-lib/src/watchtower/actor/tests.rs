use std::sync::Mutex;

use fiber_types::SettlementKey;

use ckb_types::{core::ScriptHashType, packed::Byte32, prelude::*};
use fiber_types::TLCId;
use secp256k1::SecretKey;

use crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement;

use super::*;

#[derive(Default)]
struct TestWatchtowerStore {
    channels: Mutex<Vec<(NodeId, ChannelData)>>,
    settlements: Mutex<Vec<(Hash256, TLCId, OnChainTlcSettlement)>>,
    preimages: Mutex<Vec<(NodeId, Hash256, Hash256)>>,
    registered_features: Mutex<Vec<ChannelFeatures>>,
}

impl TestWatchtowerStore {
    fn settled_tlcs(&self) -> Vec<(Hash256, [u8; 32])> {
        self.settlements
            .lock()
            .expect("lock poisoned")
            .iter()
            .map(|(channel_id, _, settlement)| (*channel_id, settlement.payment_hash.into()))
            .collect()
    }

    fn settlements(&self) -> Vec<(Hash256, TLCId, OnChainTlcSettlement)> {
        self.settlements.lock().expect("lock poisoned").clone()
    }
}

impl WatchtowerStore for TestWatchtowerStore {
    fn get_watch_channels_with_nodes(&self) -> Vec<(NodeId, ChannelData)> {
        self.channels.lock().expect("lock poisoned").clone()
    }

    fn insert_watch_channel(
        &self,
        _node_id: NodeId,
        _channel_id: Hash256,
        _funding_udt_type_script: Option<Script>,
        _keys: WatchChannelKeys,
        _settlement_data: SettlementData,
        channel_features: ChannelFeatures,
    ) -> bool {
        self.registered_features
            .lock()
            .expect("lock poisoned")
            .push(channel_features);
        true
    }

    fn remove_watch_channel(&self, _node_id: NodeId, _channel_id: Hash256) -> bool {
        true
    }

    fn update_revocation(
        &self,
        _node_id: NodeId,
        _channel_id: Hash256,
        _revocation_data: RevocationData,
        _remote_settlement_data: SettlementData,
    ) {
    }

    fn update_pending_remote_settlement(
        &self,
        _node_id: NodeId,
        _channel_id: Hash256,
        _number: u64,
        _pending_remote_settlement_data: SettlementData,
    ) -> Result<(), String> {
        Ok(())
    }

    fn update_local_settlement(
        &self,
        _node_id: NodeId,
        _channel_id: Hash256,
        _local_settlement_data: SettlementData,
    ) {
    }

    fn insert_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256, preimage: Hash256) {
        self.preimages
            .lock()
            .expect("lock poisoned")
            .push((node_id, payment_hash, preimage));
    }

    fn remove_watch_preimage(&self, _node_id: NodeId, _payment_hash: Hash256) {}

    fn get_watch_preimage(&self, node_id: &NodeId, payment_hash: &Hash256) -> Option<Hash256> {
        self.preimages
            .lock()
            .expect("lock poisoned")
            .iter()
            .find_map(|(stored_node_id, stored_payment_hash, preimage)| {
                (stored_node_id == node_id && stored_payment_hash == payment_hash)
                    .then_some(*preimage)
            })
    }

    fn get_watchtower_signer(
        &self,
        _node_id: &NodeId,
        _channel_id: &Hash256,
    ) -> fiber_types::WatchtowerSignerState {
        fiber_types::WatchtowerSignerState::Internal
    }

    fn put_watchtower_signer(
        &self,
        _node_id: &NodeId,
        _channel_id: &Hash256,
        _state: fiber_types::WatchtowerSignerState,
    ) {
    }

    fn update_watchtower_signer<R, E>(
        &self,
        _node_id: &NodeId,
        _channel_id: &Hash256,
        update: impl FnOnce(&mut fiber_types::WatchtowerSignerState) -> Result<R, E>,
    ) -> Result<R, E> {
        update(&mut fiber_types::WatchtowerSignerState::Internal)
    }

    fn insert_onchain_tlc_settlement(
        &self,
        _node_id: &NodeId,
        channel_id: &Hash256,
        tlc_id: TLCId,
        settlement: OnChainTlcSettlement,
    ) {
        self.settlements
            .lock()
            .expect("lock poisoned")
            .push((*channel_id, tlc_id, settlement));
    }

    fn get_onchain_tlc_settlement(
        &self,
        _node_id: &NodeId,
        channel_id: &Hash256,
        tlc_id: TLCId,
        _payment_hash: &Hash256,
    ) -> Option<OnChainTlcSettlement> {
        self.settlements
            .lock()
            .expect("lock poisoned")
            .iter()
            .find_map(|(id, stored_tlc_id, settlement)| {
                (id == channel_id && *stored_tlc_id == tlc_id).then(|| settlement.clone())
            })
    }
}

fn commitment_lock_prefix() -> Script {
    Script::new_builder()
        .code_hash(Byte32::from([1u8; 32]))
        .hash_type(ScriptHashType::Type)
        .args([2u8; 36].to_vec().pack())
        .build()
}

fn settlement_lock(prefix: &Script) -> Script {
    let mut args = prefix.args().raw_data().to_vec();
    args.extend_from_slice(&[3u8; 20]);
    args.push(1);
    args.push(ChannelFeatures::V2.bits());
    prefix.clone().as_builder().args(args.pack()).build()
}

fn settlement_witness(payment_hash: [u8; 32]) -> Vec<u8> {
    settlement_witness_with_unlock(
        payment_hash,
        Unlock {
            unlock_type: 0,
            with_preimage: false,
            signature: [0u8; 65],
            preimage: None,
        },
    )
}

fn test_htlc(payment_hash: [u8; 32]) -> Htlc {
    Htlc {
        htlc_type: 0,
        payment_amount: 1_000,
        payment_hash: payment_hash.to_vec(),
        remote_htlc_pubkey_hash: [4u8; 20],
        local_htlc_pubkey_hash: [5u8; 20],
        htlc_expiry: 0,
    }
}

fn tracked_tlc(payment_hash: [u8; 32], tlc_id: u64) -> TrackedSettlementTlc {
    TrackedSettlementTlc {
        tlc_id: TLCId::Offered(tlc_id),
        payment_hash: payment_hash.into(),
        hash_algorithm: HashAlgorithm::CkbHash,
        witness: test_htlc(payment_hash).to_witness(),
        channel_features: ChannelFeatures::V2,
    }
}

fn watched_outpoints(
    outpoint: OutPoint,
    payment_hashes: &[[u8; 32]],
) -> HashMap<OutPoint, Vec<TrackedSettlementTlc>> {
    HashMap::from([(
        outpoint,
        payment_hashes
            .iter()
            .enumerate()
            .map(|(index, payment_hash)| tracked_tlc(*payment_hash, index as u64))
            .collect(),
    )])
}

#[test]
fn tracked_settlement_tlcs_accepts_private_key_only_channel() {
    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let settlement_data = SettlementData {
        local_amount: 3_000,
        remote_amount: 2_000,
        tlcs: Vec::new(),
    };
    let channel_data = ChannelData {
        channel_id: [9u8; 32].into(),
        funding_udt_type_script: None,
        local_settlement_key: Some(local_settlement_key.clone()),
        local_settlement_key_pubkey: None,
        remote_settlement_key,
        local_funding_pubkey: Privkey::from(&[3; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from(&[4; 32]).pubkey(),
        remote_settlement_data: settlement_data.clone(),
        pending_remote_settlement_data: settlement_data.clone(),
        local_settlement_data: settlement_data.clone(),
        revocation_data: None,
        channel_features: ChannelFeatures::V2,
    };
    let witness = settlement_data_to_witness(
        &settlement_data,
        false,
        local_settlement_key.pubkey(),
        remote_settlement_key,
    );
    let mut lock_args =
        ckb_hash::blake2b_256(channel_data_x_only_aggregated_pubkey(&channel_data, true))[..20]
            .to_vec();
    lock_args.extend_from_slice(&0u64.to_le_bytes());
    lock_args.extend_from_slice(&0u64.to_be_bytes());
    lock_args.extend_from_slice(blake160(&witness).as_ref());
    lock_args.extend_from_slice(&[0, ChannelFeatures::V2.bits()]);
    let commitment_lock = get_script_by_contract(Contract::CommitmentLock, &lock_args);

    assert_eq!(
        crate::fiber::onchain_tlc_reconcile::verified_tracked_settlement_tlcs(
            &commitment_lock,
            &channel_data,
            false
        ),
        Some(Vec::new())
    );
}

fn settlement_witness_with_unlock(payment_hash: [u8; 32], unlock: Unlock) -> Vec<u8> {
    settlement_witness_with_unlocks(&[payment_hash], vec![unlock])
}

fn settlement_witness_with_unlocks(payment_hashes: &[[u8; 32]], unlocks: Vec<Unlock>) -> Vec<u8> {
    let settlement_witness = SettlementWitness {
        pending_htlc_count: payment_hashes.len(),
        pending_htlcs: payment_hashes
            .iter()
            .enumerate()
            .map(|(index, prefix)| {
                test_htlc(tracked_tlc(*prefix, index as u64).payment_hash.into())
            })
            .collect(),
        settlement_remote_pubkey_hash: [6u8; 20],
        settlement_remote_amount: 2_000,
        settlement_local_pubkey_hash: [7u8; 20],
        settlement_local_amount: 3_000,
        unlocks: vec![],
    };

    let mut witness = [
        XUDT_COMPATIBLE_WITNESS.as_slice(),
        &[unlocks.len() as u8],
        settlement_witness.to_witness().as_slice(),
    ]
    .concat();
    for unlock in unlocks {
        witness.extend_from_slice(&unlock.to_witness());
    }
    witness
}

fn settlement_witness_final_party_unlock(
    payment_hashes: [[u8; 32]; 2],
    unlock_type: u8,
) -> Vec<u8> {
    let settlement_witness = SettlementWitness {
        pending_htlc_count: 2,
        pending_htlcs: payment_hashes
            .into_iter()
            .enumerate()
            .map(|(index, prefix)| test_htlc(tracked_tlc(prefix, index as u64).payment_hash.into()))
            .collect(),
        settlement_remote_pubkey_hash: [6u8; 20],
        settlement_remote_amount: 2_000,
        settlement_local_pubkey_hash: [7u8; 20],
        settlement_local_amount: 3_000,
        unlocks: vec![],
    };
    let unlock = Unlock {
        unlock_type,
        with_preimage: false,
        signature: [0u8; 65],
        preimage: None,
    };

    [
        XUDT_COMPATIBLE_WITNESS.as_slice(),
        &[0x01],
        settlement_witness.to_witness().as_slice(),
        unlock.to_witness().as_slice(),
    ]
    .concat()
}

fn tx_with_input_output_and_witness(
    input_out_point: OutPoint,
    output_lock: Script,
    witness: Option<Vec<u8>>,
) -> Transaction {
    tx_with_inputs_outputs_and_witnesses(
        vec![input_out_point],
        vec![output_lock],
        witness.into_iter().collect(),
    )
}

fn tx_with_inputs_outputs_and_witnesses(
    input_out_points: Vec<OutPoint>,
    output_locks: Vec<Script>,
    witnesses: Vec<Vec<u8>>,
) -> Transaction {
    let mut tx_builder = Transaction::default().as_advanced_builder();
    for input_out_point in input_out_points {
        tx_builder = tx_builder.input(
            CellInput::new_builder()
                .previous_output(input_out_point)
                .build(),
        );
    }
    for output_lock in output_locks {
        tx_builder = tx_builder
            .output(CellOutput::new_builder().lock(output_lock).build())
            .output_data(Bytes::default());
    }
    for witness in witnesses {
        tx_builder = tx_builder.witness(witness.pack());
    }
    tx_builder.build().data()
}

#[test]
fn settlement_builder_rejects_preimage_for_different_full_hash() {
    let self_node_id = NodeId::local();
    let preimage: Hash256 = [11u8; 32].into();
    let stored_payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage).into();
    let mut queried_payment_hash_bytes: [u8; 32] = stored_payment_hash
        .as_ref()
        .try_into()
        .expect("payment hash");
    queried_payment_hash_bytes[31] ^= 1;
    let queried_payment_hash: Hash256 = queried_payment_hash_bytes.into();
    let local_settlement_key = Privkey::from(&[1; 32]);
    let remote_settlement_key = Privkey::from(&[2; 32]).pubkey();
    let settlement_tlc = fiber_types::SettlementTlc {
        tlc_id: TLCId::Offered(0),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1_000,
        payment_hash: queried_payment_hash,
        expiry: 60_000,
        local_key: Some(Privkey::from(&[3; 32])),
        local_key_pubkey: None,
        local_key_commitment_number: None,
        remote_key: Privkey::from(&[4; 32]).pubkey(),
    };
    let tracked_tlcs = vec![TrackedSettlementTlc {
        tlc_id: settlement_tlc.tlc_id.flip(),
        payment_hash: settlement_tlc.payment_hash,
        hash_algorithm: settlement_tlc.hash_algorithm,
        witness: settlement_tlc_to_witness(&settlement_tlc, false),
        channel_features: ChannelFeatures::V2,
    }];
    let settlement_data = SettlementData {
        local_amount: 100_000_000_000,
        remote_amount: 100_000_000_000,
        tlcs: vec![settlement_tlc],
    };
    let channel_data = ChannelData {
        channel_id: [9u8; 32].into(),
        funding_udt_type_script: None,
        local_settlement_key: Some(local_settlement_key.clone()),
        local_settlement_key_pubkey: Some(local_settlement_key.pubkey()),
        remote_settlement_key,
        local_funding_pubkey: Privkey::from(&[5; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from(&[6; 32]).pubkey(),
        remote_settlement_data: settlement_data.clone(),
        pending_remote_settlement_data: settlement_data.clone(),
        local_settlement_data: settlement_data.clone(),
        revocation_data: None,
        channel_features: ChannelFeatures::V2,
    };
    let settlement_witness = SettlementWitness::build_from_witness(
        &[
            &[0u8],
            settlement_data_to_witness(
                &settlement_data,
                false,
                local_settlement_key.pubkey(),
                remote_settlement_key,
            )
            .as_slice(),
        ]
        .concat(),
    )
    .expect("settlement witness");

    let delay_epoch = EpochNumberWithFraction::new(3, 0, 1);
    let since = Since::new(
        SinceType::EpochNumberWithFraction,
        delay_epoch.full_value(),
        true,
    )
    .value();
    let mut lock_args = vec![0u8; 20];
    lock_args.extend_from_slice(&since.to_le_bytes());
    lock_args.extend_from_slice(&0u64.to_be_bytes());
    lock_args.extend_from_slice(
        blake160(&settlement_data_to_witness(
            &settlement_data,
            false,
            local_settlement_key.pubkey(),
            remote_settlement_key,
        ))
        .as_ref(),
    );
    lock_args.extend_from_slice(&[1, ChannelFeatures::V2.bits()]);
    let lock = Script::new_builder()
        .code_hash(Byte32::from([1u8; 32]))
        .hash_type(ScriptHashType::Type)
        .args(lock_args.pack())
        .build();
    let type_script = Script::new_builder()
        .code_hash(Byte32::from([2u8; 32]))
        .hash_type(ScriptHashType::Type)
        .build();
    let commitment_cell = Cell {
        output: CellOutput::new_builder()
            .capacity(200_000_000_000u64)
            .lock(lock)
            .type_(Some(type_script).pack())
            .build()
            .into(),
        // A safe builder returns before inspecting output data because no exact preimage
        // exists for the queried full hash. A prefix-only regression would reach this
        // deliberately invalid sentinel.
        output_data: Some(ckb_jsonrpc_types::JsonBytes::from_vec(Vec::new())),
        out_point: OutPoint::new(Byte32::from([3u8; 32]), 0).into(),
        block_number: 0u64.into(),
        tx_index: 0u32.into(),
    };
    let store = TestWatchtowerStore::default();
    store.insert_watch_preimage(self_node_id.clone(), stored_payment_hash, preimage);
    let signer = LocalSigner::new(SecretKey::from_slice(&[7u8; 32]).expect("secret key"));
    let mut cell_collector = new_default_cell_collector("http://127.0.0.1:8114");

    let result = build_settlement_tx(
        &Default::default(),
        commitment_cell,
        EpochNumberWithFraction::new(10, 0, 1),
        EpochNumberWithFraction::new(10, 0, 1),
        0,
        &self_node_id,
        false,
        channel_data,
        &settlement_data,
        Some(settlement_witness),
        &tracked_tlcs,
        &signer,
        &mut cell_collector,
        &store,
    );

    assert!(
        matches!(result, Ok(None)),
        "a preimage for a different full hash must not start settlement construction: {result:?}"
    );
}

#[test]
fn settlement_witness_rejects_short_payment_hash() {
    let payment_hash: Vec<u8> = (0..32).collect();
    let mut v2_htlc = vec![0u8];
    v2_htlc.extend_from_slice(&1_000u128.to_le_bytes());
    v2_htlc.extend_from_slice(&payment_hash);
    v2_htlc.extend_from_slice(&[4u8; 20]);
    v2_htlc.extend_from_slice(&[5u8; 20]);
    v2_htlc.extend_from_slice(&0u64.to_le_bytes());
    let v2_witness = [
        &[0u8, 1u8],
        v2_htlc.as_slice(),
        &[6u8; 20],
        &2_000u128.to_le_bytes(),
        &[7u8; 20],
        &3_000u128.to_le_bytes(),
        &[1u8, 1u8],
        &[0u8; 65],
        &payment_hash,
    ]
    .concat();
    let parsed = SettlementWitness::build_from_witness(&v2_witness).expect("V2 settlement witness");
    assert_eq!(parsed.pending_htlcs[0].payment_hash, payment_hash);
    assert_eq!(parsed.unlocks[0].witness_len(), 99);

    let mut old_witness = v2_witness;
    old_witness.drain(2 + 17 + 20..2 + 17 + 32);
    assert!(SettlementWitness::build_from_witness(&old_witness).is_none());
}

#[test]
fn first_settlement_witness_uses_v2_layout() {
    let payment_hash_bytes: [u8; 32] = (0..32)
        .map(|value| value as u8)
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    let payment_hash: Hash256 = payment_hash_bytes.into();
    let settlement_data = SettlementData {
        local_amount: 2_000,
        remote_amount: 3_000,
        tlcs: vec![fiber_types::SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1_000,
            payment_hash,
            expiry: 0,
            local_key: Some(Privkey::from(&[1u8; 32])),
            local_key_pubkey: None,
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[2u8; 32]).pubkey(),
        }],
    };

    let witness = settlement_data_to_witness(
        &settlement_data,
        false,
        Privkey::from(&[3u8; 32]).pubkey(),
        Privkey::from(&[4u8; 32]).pubkey(),
    );
    let parsed = SettlementWitness::build_from_witness(&[&[0u8], witness.as_slice()].concat())
        .expect("V2 first settlement witness");

    assert_eq!(parsed.pending_htlcs[0].payment_hash, payment_hash.as_ref());
}

#[test]
fn feature_bitmap_watch_channel_registration_reaches_store() {
    let store = TestWatchtowerStore::default();
    let settlement_data = SettlementData {
        local_amount: 1,
        remote_amount: 2,
        tlcs: vec![],
    };
    store.insert_watch_channel(
        NodeId::local(),
        [1u8; 32].into(),
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::local(Privkey::from(&[1u8; 32])),
            remote_settlement: Privkey::from(&[2u8; 32]).pubkey(),
            local_funding: Privkey::from(&[3u8; 32]).pubkey(),
            remote_funding: Privkey::from(&[4u8; 32]).pubkey(),
        },
        settlement_data,
        ChannelFeatures::V2,
    );

    assert_eq!(
        *store.registered_features.lock().expect("lock poisoned"),
        vec![ChannelFeatures::V2]
    );
}

#[test]
fn unrelated_same_prefix_tx_does_not_pollute_store() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hash = [42u8; 32];
    let attacker_tx = tx_with_input_output_and_witness(
        OutPoint::new([8u8; 32].pack(), 0),
        settlement_lock(&lock_prefix),
        Some(settlement_witness(payment_hash)),
    );
    let mut watched_outpoints =
        watched_outpoints(OutPoint::new([7u8; 32].pack(), 0), &[[43u8; 32]]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    let processed = process_watched_settlement_tx(
        &attacker_tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &self_node_id,
    );

    assert!(processed.is_none());
    assert!(store.settled_tlcs().is_empty());
    assert_eq!(watched_outpoints.len(), 1);
}

#[test]
fn watched_non_first_input_does_not_parse_unrelated_witness_zero() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let malicious_payment_hash = [42u8; 32];
    let watched_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let fee_out_point = OutPoint::new([8u8; 32].pack(), 0);
    let tx = tx_with_inputs_outputs_and_witnesses(
        vec![fee_out_point, watched_out_point.clone()],
        vec![settlement_lock(&lock_prefix)],
        vec![settlement_witness(malicious_payment_hash), vec![]],
    );
    let mut watched_outpoints = watched_outpoints(watched_out_point, &[malicious_payment_hash]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    let processed = process_watched_settlement_tx(
        &tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &self_node_id,
    );

    assert_eq!(processed, None);
    assert!(store.settled_tlcs().is_empty());
}

#[test]
fn watched_settlement_tx_updates_store_and_tracks_next_outpoint() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hash = [42u8; 32];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness(payment_hash)),
    );
    let tracked = tracked_tlc(payment_hash, 0);
    let mut watched_outpoints =
        HashMap::from([(first_commitment_out_point, vec![tracked.clone()])]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    let processed = process_watched_settlement_tx(
        &tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &self_node_id,
    );

    assert_eq!(processed, Some(0));
    assert_eq!(store.settled_tlcs(), vec![(channel_id, payment_hash)]);
    assert_eq!(
        store.settlements(),
        vec![(
            channel_id,
            TLCId::Offered(0),
            OnChainTlcSettlement {
                payment_hash: tracked.payment_hash,
                hash_algorithm: HashAlgorithm::CkbHash,
                preimage: None,
                tx_hash: tx.calc_tx_hash().into(),
                tlc_index: 0,
            },
        )]
    );
    assert!(watched_outpoints.contains_key(&OutPoint::new(tx.calc_tx_hash(), 0)));
}

#[test]
fn shared_prefix_unlock_index_maps_to_exact_tlc() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let first_hash = [42u8; 32];
    let mut second_hash = first_hash;
    second_hash[31] = 43;
    assert_eq!(&first_hash[..20], &second_hash[..20]);
    let tracked = [tracked_tlc(first_hash, 0), tracked_tlc(second_hash, 1)];
    assert_ne!(tracked[0].payment_hash, tracked[1].payment_hash);

    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness_with_unlocks(
            &[first_hash, second_hash],
            vec![Unlock {
                unlock_type: 1,
                with_preimage: false,
                signature: [0u8; 65],
                preimage: None,
            }],
        )),
    );
    let mut watched_outpoints = HashMap::from([(first_commitment_out_point, tracked.to_vec())]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();

    assert_eq!(
        process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &NodeId::local(),
        ),
        Some(0)
    );

    let settlements = store.settlements();
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].1, TLCId::Offered(1));
    assert_eq!(settlements[0].2.payment_hash, tracked[1].payment_hash);
    let remaining = watched_outpoints
        .get(&OutPoint::new(tx.calc_tx_hash(), 0))
        .expect("next settlement outpoint tracked");
    assert_eq!(remaining, &tracked[..1]);
}

#[test]
fn final_party_unlock_keeps_pending_tlcs_tracked() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hashes = [[41u8; 32], [42u8; 32]];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness_final_party_unlock(payment_hashes, 0xFE)),
    );
    let tracked = [
        tracked_tlc(payment_hashes[0], 0),
        tracked_tlc(payment_hashes[1], 1),
    ];
    let mut watched_outpoints = HashMap::from([(first_commitment_out_point, tracked.to_vec())]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    let processed = process_watched_settlement_tx(
        &tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &self_node_id,
    );

    assert_eq!(processed, Some(0));
    assert!(
        store.settlements().is_empty(),
        "a party-balance unlock is not a settlement proof for any pending TLC"
    );
    assert_eq!(
        watched_outpoints.get(&OutPoint::new(tx.calc_tx_hash(), 0)),
        Some(&tracked.to_vec()),
        "the successor witness still contains every pending TLC"
    );
}

#[test]
fn terminal_final_party_unlock_records_settled_without_preimage() {
    // Issue #1611: after #1583 the watchtower skips every final-party unlock. When the
    // unlock consumes the last commitment cell (no successor commitment output), the
    // terminal sweep carries every still-pending TLC off-chain. B (the offering party of
    // the expired TLC) may legally sweep it, but without a settlement record the channel
    // reconciliation stays incomplete forever and `RemoveTlc` is never propagated upstream.
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hashes = [[41u8; 32], [42u8; 32]];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tracked = [
        tracked_tlc(payment_hashes[0], 0),
        tracked_tlc(payment_hashes[1], 1),
    ];
    // A terminal settlement consumes the last commitment cell: the output is a plain
    // party-balance cell, not a successor commitment cell carrying the pending TLCs.
    let party_balance_lock = Script::new_builder().args([1u8; 8].to_vec().pack()).build();
    for unlock_type in [0xFE, 0xFF] {
        let tx = tx_with_input_output_and_witness(
            first_commitment_out_point.clone(),
            party_balance_lock.clone(),
            Some(settlement_witness_with_unlocks(
                &payment_hashes,
                vec![Unlock {
                    unlock_type,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                }],
            )),
        );
        let mut watched_outpoints =
            HashMap::from([(first_commitment_out_point.clone(), tracked.to_vec())]);
        let mut processed_tx_hashes = HashSet::new();
        let store = TestWatchtowerStore::default();
        let self_node_id = NodeId::local();

        let processed = process_watched_settlement_tx(
            &tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        );

        assert_eq!(processed, Some(0), "unlock 0x{:02x}", unlock_type);
        let settlements = store.settlements();
        assert_eq!(
            settlements.len(),
            2,
            "unlock 0x{:02x}: every terminal-swept TLC needs a settlement record",
            unlock_type
        );
        for (index, tlc) in tracked.iter().enumerate() {
            let (stored_channel_id, stored_tlc_id, settlement) = &settlements[index];
            assert_eq!(stored_channel_id, &channel_id);
            assert_eq!(stored_tlc_id, &tlc.tlc_id);
            assert_eq!(settlement.payment_hash, tlc.payment_hash);
            assert_eq!(settlement.hash_algorithm, tlc.hash_algorithm);
            assert!(
                settlement.preimage.is_none(),
                "unlock 0x{:02x}: terminal sweep settles the TLC without a preimage",
                unlock_type
            );
        }
        assert!(
            watched_outpoints.is_empty(),
            "unlock 0x{:02x}: no successor commitment cell remains after a terminal sweep",
            unlock_type
        );
    }
}

#[test]
fn final_party_unlock_only_removes_explicitly_unlocked_tlc() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hashes = [[41u8; 32], [42u8; 32]];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness_with_unlocks(
            &payment_hashes,
            vec![
                Unlock {
                    unlock_type: 0xFE,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                },
                Unlock {
                    unlock_type: 1,
                    with_preimage: false,
                    signature: [0u8; 65],
                    preimage: None,
                },
            ],
        )),
    );
    let tracked = [
        tracked_tlc(payment_hashes[0], 0),
        tracked_tlc(payment_hashes[1], 1),
    ];
    let mut watched_outpoints = HashMap::from([(first_commitment_out_point, tracked.to_vec())]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();

    let processed = process_watched_settlement_tx(
        &tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &NodeId::local(),
    );

    assert_eq!(processed, Some(0));
    let settlements = store.settlements();
    assert_eq!(settlements.len(), 1);
    assert_eq!(settlements[0].1, TLCId::Offered(1));
    assert_eq!(settlements[0].2.payment_hash, tracked[1].payment_hash);
    assert_eq!(
        watched_outpoints.get(&OutPoint::new(tx.calc_tx_hash(), 0)),
        Some(&tracked[..1].to_vec()),
        "only the explicitly index-unlocked TLC leaves the successor witness"
    );
}

#[test]
fn watched_tx_does_not_track_extra_same_prefix_output() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let payment_hash = [42u8; 32];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let tx = tx_with_inputs_outputs_and_witnesses(
        vec![first_commitment_out_point.clone()],
        vec![settlement_lock(&lock_prefix), settlement_lock(&lock_prefix)],
        vec![settlement_witness(payment_hash)],
    );
    let extra_same_prefix_out_point = OutPoint::new(tx.calc_tx_hash(), 1);
    let mut watched_outpoints = watched_outpoints(first_commitment_out_point, &[payment_hash]);
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    let processed = process_watched_settlement_tx(
        &tx,
        &mut watched_outpoints,
        &mut processed_tx_hashes,
        &lock_prefix,
        &channel_id,
        &store,
        &self_node_id,
    );

    assert_eq!(processed, Some(0));
    assert!(!watched_outpoints.contains_key(&extra_same_prefix_out_point));
}

#[test]
fn asc_processing_handles_parent_before_child_without_candidate_cache() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let parent_payment_hash = [42u8; 32];
    let child_payment_hash = [43u8; 32];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let parent_tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness_with_unlocks(
            &[parent_payment_hash, child_payment_hash],
            vec![Unlock {
                unlock_type: 0,
                with_preimage: false,
                signature: [0u8; 65],
                preimage: None,
            }],
        )),
    );
    let child_tx = tx_with_input_output_and_witness(
        OutPoint::new(parent_tx.calc_tx_hash(), 0),
        settlement_lock(&lock_prefix),
        Some(settlement_witness(child_payment_hash)),
    );
    let mut watched_outpoints = watched_outpoints(
        first_commitment_out_point,
        &[parent_payment_hash, child_payment_hash],
    );
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    assert_eq!(
        process_watched_settlement_tx(
            &parent_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        ),
        Some(0)
    );
    assert_eq!(
        process_watched_settlement_tx(
            &child_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        ),
        Some(0)
    );

    assert_eq!(
        store.settled_tlcs(),
        vec![
            (channel_id, parent_payment_hash),
            (channel_id, child_payment_hash)
        ]
    );
    let settlements = store.settlements();
    assert_eq!(settlements[0].1, TLCId::Offered(0));
    assert_eq!(settlements[1].1, TLCId::Offered(1));
    assert_eq!(settlements[1].2.tlc_index, 0);
}

#[test]
fn one_pass_processing_misses_child_when_indexer_returns_child_before_parent() {
    let lock_prefix = commitment_lock_prefix();
    let channel_id: Hash256 = [9u8; 32].into();
    let parent_payment_hash = [42u8; 32];
    let child_payment_hash = [43u8; 32];
    let first_commitment_out_point = OutPoint::new([7u8; 32].pack(), 0);
    let parent_tx = tx_with_input_output_and_witness(
        first_commitment_out_point.clone(),
        settlement_lock(&lock_prefix),
        Some(settlement_witness_with_unlocks(
            &[parent_payment_hash, child_payment_hash],
            vec![Unlock {
                unlock_type: 0,
                with_preimage: false,
                signature: [0u8; 65],
                preimage: None,
            }],
        )),
    );
    let child_tx = tx_with_input_output_and_witness(
        OutPoint::new(parent_tx.calc_tx_hash(), 0),
        settlement_lock(&lock_prefix),
        Some(settlement_witness(child_payment_hash)),
    );
    let mut watched_outpoints = watched_outpoints(
        first_commitment_out_point,
        &[parent_payment_hash, child_payment_hash],
    );
    let mut processed_tx_hashes = HashSet::new();
    let store = TestWatchtowerStore::default();
    let self_node_id = NodeId::local();

    // Simulate child-before-parent order: child is skipped on first pass
    assert_eq!(
        process_watched_settlement_tx(
            &child_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        ),
        None,
        "child processed before parent should return None"
    );

    // Parent is processed, adding its output to watched_outpoints
    assert_eq!(
        process_watched_settlement_tx(
            &parent_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        ),
        Some(0)
    );

    // With multi-pass retry, child is retried and now succeeds
    assert_eq!(
        process_watched_settlement_tx(
            &child_tx,
            &mut watched_outpoints,
            &mut processed_tx_hashes,
            &lock_prefix,
            &channel_id,
            &store,
            &self_node_id,
        ),
        Some(0),
        "child should succeed on retry after parent is processed"
    );

    assert_eq!(
        store.settled_tlcs(),
        vec![
            (channel_id, parent_payment_hash),
            (channel_id, child_payment_hash)
        ]
    );
    let settlements = store.settlements();
    assert_eq!(settlements[0].1, TLCId::Offered(0));
    assert_eq!(settlements[1].1, TLCId::Offered(1));
}
#[derive(Clone)]
struct SettlementFeeCollector(ckb_sdk::traits::LiveCell);

#[async_trait::async_trait]
impl CellCollector for SettlementFeeCollector {
    async fn collect_live_cells_async(
        &mut self,
        _query: &CellQueryOptions,
        _apply_changes: bool,
    ) -> Result<(Vec<ckb_sdk::traits::LiveCell>, u64), ckb_sdk::traits::CellCollectorError> {
        Ok((vec![self.0.clone()], self.0.output.capacity().unpack()))
    }
    fn lock_cell(
        &mut self,
        _: OutPoint,
        _: u64,
    ) -> Result<(), ckb_sdk::traits::CellCollectorError> {
        Ok(())
    }
    fn apply_tx(
        &mut self,
        _: Transaction,
        _: u64,
    ) -> Result<(), ckb_sdk::traits::CellCollectorError> {
        Ok(())
    }
    fn reset(&mut self) {}
}

#[test]
fn test_fresh_channel_pre_tlc_commitment_builds_settlement() {
    let preceding = SettlementData {
        local_amount: 100_000_000_000,
        remote_amount: 100_000_000_000,
        tlcs: vec![],
    };
    let mut pending = preceding.clone();
    pending.local_amount -= 1_000;
    pending.tlcs.push(fiber_types::SettlementTlc {
        tlc_id: TLCId::Offered(0),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1_000,
        payment_hash: [11u8; 32].into(),
        expiry: 60_000,
        local_key: Some(Privkey::from(&[3; 32])),
        local_key_pubkey: None,
        local_key_commitment_number: None,
        remote_key: Privkey::from(&[4; 32]).pubkey(),
    });
    let channel_data = ChannelData {
        channel_id: [9u8; 32].into(),
        funding_udt_type_script: None,
        local_settlement_key: Some(Privkey::from(&[1; 32])),
        local_settlement_key_pubkey: None,
        remote_settlement_key: Privkey::from(&[2; 32]).pubkey(),
        local_funding_pubkey: Privkey::from(&[5; 32]).pubkey(),
        remote_funding_pubkey: Privkey::from(&[6; 32]).pubkey(),
        remote_settlement_data: preceding.clone(),
        pending_remote_settlement_data: pending,
        local_settlement_data: preceding.clone(),
        revocation_data: None,
        channel_features: ChannelFeatures::V2,
    };
    let witness = settlement_data_to_witness(
        &preceding,
        true,
        channel_data.local_settlement_pubkey(),
        channel_data.remote_settlement_key,
    );
    let since = Since::new(
        SinceType::EpochNumberWithFraction,
        EpochNumberWithFraction::new(3, 0, 1).full_value(),
        true,
    )
    .value();
    let mut args =
        ckb_hash::blake2b_256(channel_data_x_only_aggregated_pubkey(&channel_data, false))[..20]
            .to_vec();
    args.extend_from_slice(&since.to_le_bytes());
    args.extend_from_slice(&1u64.to_be_bytes());
    args.extend_from_slice(&blake160(&witness).0);
    args.push(0);
    args.push(ChannelFeatures::V2.bits());
    let commitment_lock = get_script_by_contract(Contract::CommitmentLock, &args);
    assert_eq!(
        crate::fiber::onchain_tlc_reconcile::verified_tracked_settlement_tlcs(
            &commitment_lock,
            &channel_data,
            true
        ),
        Some(Vec::new()),
        "the initial commitment must use the preceding snapshot, not the pending first TLC"
    );
    let commitment_cell = Cell {
        output: CellOutput::new_builder()
            .capacity(200_000_000_000u64)
            .lock(commitment_lock)
            .build()
            .into(),
        output_data: Some(ckb_jsonrpc_types::JsonBytes::from_vec(vec![])),
        out_point: OutPoint::new(Byte32::from([8u8; 32]), 0).into(),
        block_number: 1u64.into(),
        tx_index: 1u32.into(),
    };
    let signer = LocalSigner::new(SecretKey::from_slice(&[7u8; 32]).unwrap());
    let mut collector = SettlementFeeCollector(ckb_sdk::traits::LiveCell {
        output: CellOutput::new_builder()
            .capacity(200_000_000_000u64)
            .lock(get_script_by_contract(
                Contract::Secp256k1Lock,
                signer.pubkey_hash(),
            ))
            .build(),
        output_data: Default::default(),
        out_point: OutPoint::new(Byte32::from([7u8; 32]), 0),
        block_number: 1,
        tx_index: 1,
    });
    let store = TestWatchtowerStore::default();
    store
        .channels
        .lock()
        .unwrap()
        .push((NodeId::local(), channel_data.clone()));
    // Supply the correct empty TLC mapping independently so this catches the builder
    // reselecting the pending snapshot even after snapshot verification is repaired.
    let tx = build_settlement_tx(
        &Default::default(),
        commitment_cell,
        EpochNumberWithFraction::new(10, 0, 1),
        EpochNumberWithFraction::new(13, 0, 1),
        0,
        &NodeId::local(),
        true,
        channel_data,
        &preceding,
        None,
        &[],
        &signer,
        &mut collector,
        &store,
    )
    .expect("build settlement")
    .expect("mature pre-TLC commitment must settle");
    let actual = tx.witnesses().get(0).unwrap().raw_data();
    let offset = XUDT_COMPATIBLE_WITNESS.len() + 1;
    assert_eq!(&actual[offset..offset + witness.len()], witness.as_slice());
    let remaining_capacity: u64 = tx.outputs().get(0).unwrap().capacity().unpack();
    assert_eq!(remaining_capacity, 100_000_000_000);
}
