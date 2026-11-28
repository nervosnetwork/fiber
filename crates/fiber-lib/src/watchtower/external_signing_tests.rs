use ckb_sdk::traits::{CellCollectorError, LiveCell};
use ckb_types::packed::Byte32;
use fiber_types::{
    OnchainKeyPurpose, OnchainSigningContent, SettlementKey, SettlementTlc, TLCId,
    WatchChannelKeys, WatchtowerSignerState,
};
use tempfile::TempDir;

use crate::{
    rpc::watchtower::{
        RpcContext, SubmitWatchtowerSignatureParams, SubmitWatchtowerSignatureResult,
        WatchtowerRpcServer, WatchtowerRpcServerImpl,
    },
    store::{open_store, Store},
    watchtower::{sign_onchain_request, verify_onchain_signature},
};

use super::*;

// Only the fee cell source is mocked. Settlement construction, persistent signer
// state, RPC verification and signature application use the production paths.
#[derive(Clone)]
struct FeeCells;

#[async_trait::async_trait]
impl CellCollector for FeeCells {
    async fn collect_live_cells_async(
        &mut self,
        query: &CellQueryOptions,
        _apply_changes: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        let capacity = 100_000_000_000;
        Ok((
            vec![LiveCell {
                output: CellOutput::new_builder()
                    .capacity(capacity)
                    .lock(query.primary_script.clone())
                    .build(),
                output_data: Default::default(),
                out_point: OutPoint::new(Byte32::from([90; 32]), 0),
                block_number: 0,
                tx_index: 0,
            }],
            capacity,
        ))
    }

    fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
        unreachable!("settlement only queries fee cells")
    }
    fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
        unreachable!("settlement only queries fee cells")
    }
    fn reset(&mut self) {}
}

struct Fixture {
    events: crate::signing::SigningEvents,
    store: Store,
    _dir: TempDir,
    node_id: NodeId,
    channel: ChannelData,
    cell: Cell,
    tracked: Vec<TrackedSettlementTlc>,
    witness: Option<SettlementWitness>,
    base_key: Privkey,
    tlc_key: Privkey,
    for_remote: bool,
}

impl Fixture {
    fn new(for_remote: bool, subsequent: bool, with_preimage: bool, local_keys: bool) -> Self {
        Self::with_features(
            for_remote,
            subsequent,
            with_preimage,
            local_keys,
            ChannelFeatures::V2,
        )
    }

    fn with_features(
        for_remote: bool,
        subsequent: bool,
        with_preimage: bool,
        local_keys: bool,
        features: ChannelFeatures,
    ) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = open_store(dir.path()).expect("open store");
        let node_id = NodeId::local();
        let base_key = Privkey::from([42; 32]);
        let tlc_key =
            fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
        let preimage = Hash256::from([13; 32]);
        let offered = for_remote != with_preimage;
        let tlc = SettlementTlc {
            tlc_id: if offered {
                TLCId::Offered(1)
            } else {
                TLCId::Received(1)
            },
            hash_algorithm: HashAlgorithm::Sha256,
            payment_amount: 20_000_000_000,
            payment_hash: HashAlgorithm::Sha256.hash(preimage.as_ref()).into(),
            expiry: 100_000,
            local_key: local_keys.then(|| tlc_key.clone()),
            local_key_pubkey: Some(tlc_key.pubkey()),
            local_key_commitment_number: (!local_keys).then_some(7),
            remote_key: Privkey::from([12; 32]).pubkey(),
        };
        let mut earlier = tlc.clone();
        earlier.tlc_id = if offered {
            TLCId::Offered(0)
        } else {
            TLCId::Received(0)
        };
        earlier.payment_hash = Hash256::from([55; 32]);
        earlier.expiry = 1_000_000;
        earlier.local_key_commitment_number = (!local_keys).then_some(6);
        let earlier_key = Privkey::from([6; 32]);
        earlier.local_key = local_keys.then(|| earlier_key.clone());
        earlier.local_key_pubkey = Some(earlier_key.pubkey());
        let settlement = SettlementData {
            // Subsequent-TLC fixtures isolate TLC signing after the local balance has no
            // self-fundable CKB left; initial fixtures keep a 1,000-CKB balance (with an
            // unexpired `earlier` TLC keeping balance unlock inactive until tests clear TLCs).
            local_amount: if subsequent { 0 } else { 100_000_000_000 },
            remote_amount: 100_000_000_000,
            tlcs: vec![earlier, tlc],
        };
        let channel_id = Hash256::from([9; 32]);
        store.insert_watch_channel(
            node_id.clone(),
            channel_id,
            None,
            WatchChannelKeys {
                local_settlement: if local_keys {
                    SettlementKey::local(base_key.clone())
                } else {
                    SettlementKey::external(base_key.pubkey())
                },
                remote_settlement: Privkey::from([10; 32]).pubkey(),
                local_funding: Privkey::from([11; 32]).pubkey(),
                remote_funding: Privkey::from([12; 32]).pubkey(),
            },
            settlement,
            features,
        );
        store.insert_watch_preimage(
            node_id.clone(),
            HashAlgorithm::Sha256.hash(preimage.as_ref()).into(),
            preimage,
        );
        let channel = store
            .get_watch_channel(&node_id, &channel_id)
            .expect("channel");
        let snapshot = &channel.local_settlement_data;
        let witness_bytes = settlement_data_to_witness(
            snapshot,
            for_remote,
            base_key.pubkey(),
            channel.remote_settlement_key,
        );
        let since = Since::new(
            SinceType::EpochNumberWithFraction,
            EpochNumberWithFraction::new(3, 0, 1).full_value(),
            true,
        )
        .value();
        let mut args =
            ckb_hash::blake2b_256(channel_data_x_only_aggregated_pubkey(&channel, !for_remote))
                [..20]
                .to_vec();
        args.extend_from_slice(&since.to_le_bytes());
        args.extend_from_slice(&10u64.to_be_bytes());
        args.extend_from_slice(blake160(&witness_bytes).as_ref());
        args.push(0);
        args.push(features.bits());
        let mut lock = get_script_by_contract(Contract::CommitmentLock, &args);
        let mut tracked =
            crate::tests::verified_tracked_settlement_tlcs(&lock, &channel, for_remote)
                .expect("verified TLC identities");
        let witness = subsequent.then(|| {
            let mut witness =
                SettlementWitness::build_from_witness(&[&[0], witness_bytes.as_slice()].concat())
                    .expect("witness");
            // The first TLC was already unlocked: the remaining TLC has witness
            // index 0, snapshot index 1, derivation index 7, closing number 10.
            witness.unlocks.push(Unlock {
                unlock_type: 0,
                with_preimage: false,
                signature: [0; 65],
                preimage: None,
            });
            tracked.remove(0);
            witness
        });
        if let Some(previous) = &witness {
            let mut remaining = previous.clone();
            assert!(remaining.update());
            let mut args = lock.args().raw_data()[..36].to_vec();
            args.extend_from_slice(blake160(&remaining.to_witness()).as_ref());
            args.push(1);
            args.push(features.bits());
            lock = lock.as_builder().args(args.pack()).build();
        }
        let cell = Cell {
            output: CellOutput::new_builder()
                .capacity(if subsequent {
                    220_000_000_000u64
                } else {
                    240_000_000_000u64
                })
                .lock(lock)
                .build()
                .into(),
            output_data: Some(ckb_jsonrpc_types::JsonBytes::default()),
            out_point: OutPoint::new(Byte32::from([3; 32]), 0).into(),
            block_number: 0u64.into(),
            tx_index: 0u32.into(),
        };
        Self {
            events: Default::default(),
            store,
            _dir: dir,
            node_id,
            channel,
            cell,
            tracked,
            witness,
            base_key,
            tlc_key,
            for_remote,
        }
    }

    fn build(&self) -> Result<Option<TransactionView>, Box<dyn std::error::Error>> {
        self.build_at(200_000)
    }

    fn build_at(
        &self,
        current_time: u64,
    ) -> Result<Option<TransactionView>, Box<dyn std::error::Error>> {
        self.build_with_collector(current_time, &mut FeeCells)
    }

    fn build_with_collector(
        &self,
        current_time: u64,
        collector: &mut dyn CellCollector,
    ) -> Result<Option<TransactionView>, Box<dyn std::error::Error>> {
        let signer =
            LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
        // Keep the original commitment's snapshot even after a successor cell
        // changes its witness hash or another direction's snapshot is updated.
        let settlement_data = if self.for_remote {
            &self.channel.remote_settlement_data
        } else {
            &self.channel.local_settlement_data
        };
        build_settlement_tx(
            &self.events,
            self.cell.clone(),
            EpochNumberWithFraction::new(10, 0, 1),
            EpochNumberWithFraction::new(14, 0, 1),
            current_time,
            &self.node_id,
            self.for_remote,
            self.channel.clone(),
            settlement_data,
            self.witness.clone(),
            &self.tracked,
            &signer,
            collector,
            &self.store,
        )
    }

    fn pending(&self) -> (Hash256, OnchainSigningContent) {
        let (request_id, req) = self.pending_request();
        (request_id, req.content)
    }

    fn pending_request(&self) -> (Hash256, fiber_types::PendingWatchtowerRequest) {
        let WatchtowerSignerState::External(state) = self
            .store
            .get_watchtower_signer(&self.node_id, &self.channel.channel_id)
        else {
            panic!("expected external signer state");
        };
        let (request_id, req) = state.first_pending().expect("pending request");
        (request_id, req.clone())
    }

    fn submit_with_tx(
        &self,
        request_id: Hash256,
        signature: [u8; 65],
        signed_transaction: Transaction,
    ) -> Result<SubmitWatchtowerSignatureResult, jsonrpsee::types::ErrorObjectOwned> {
        let rpc = WatchtowerRpcServerImpl::new(self.store.clone())
            .with_signing_events(self.events.clone());
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(rpc.submit_watchtower_signature(
                RpcContext {
                    node_id: self.node_id.to_string(),
                    tenant_scoped: false,
                },
                SubmitWatchtowerSignatureParams {
                    channel_id: self.channel.channel_id.into(),
                    request_id: request_id.into(),
                    signature: signature.to_vec(),
                    signed_transaction: signed_transaction.into(),
                },
            ))
    }
}

fn fund_pending(req: &fiber_types::PendingWatchtowerRequest) -> OnchainSigningContent {
    let template = &req.content.transaction;
    let mut outputs: Vec<_> = template.raw().outputs().into_iter().collect();
    let payout_idx = outputs.len() - 1;
    let payout_cap: u64 = outputs[payout_idx].capacity().unpack();
    let payout_lock = test_custom_payout_lock();
    let payout_data_len = template
        .raw()
        .outputs_data()
        .get(payout_idx)
        .map(|d| d.raw_data().len())
        .unwrap_or(0);
    let payout_with_lock = outputs[payout_idx]
        .clone()
        .as_builder()
        .lock(payout_lock.clone())
        .build();
    let payout_min_cap = payout_with_lock
        .occupied_capacity(ckb_types::core::Capacity::bytes(payout_data_len).unwrap())
        .unwrap()
        .as_u64();
    let tx = if let Some(since) = req.required_expiry_since {
        let payout_out = if payout_cap < payout_min_cap {
            payout_with_lock
                .as_builder()
                .capacity(payout_min_cap)
                .build()
        } else {
            payout_with_lock
        };
        outputs[payout_idx] = payout_out;
        let updated = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(outputs)
            .build()
            .data();
        fund_wallet_tx(
            &updated,
            OutPoint::new(Byte32::from([0xaa; 32]), 0),
            since,
            payout_lock,
            &[0xde, 0xad],
        )
    } else if payout_cap >= payout_min_cap + 1_000 {
        outputs[payout_idx] = payout_with_lock
            .as_builder()
            .capacity(payout_cap - 1_000)
            .build();
        template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(outputs)
            .build()
            .data()
    } else {
        outputs[payout_idx] = payout_with_lock
            .as_builder()
            .capacity(payout_min_cap.max(14_200_000_000u64))
            .build();
        let updated = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(outputs)
            .build()
            .data();
        fund_wallet_tx(
            &updated,
            OutPoint::new(Byte32::from([0xaa; 32]), 0),
            0,
            payout_lock,
            &[0xde, 0xad],
        )
    };
    OnchainSigningContent {
        key_purpose: req.content.key_purpose,
        transaction: tx,
    }
}

fn assert_applied_signature(tx: &TransactionView, content: &OnchainSigningContent, key: &Privkey) {
    let bytes = tx
        .witnesses()
        .get(0)
        .expect("settlement witness")
        .raw_data();
    let witness = SettlementWitness::build_from_witness(&bytes[XUDT_COMPATIBLE_WITNESS.len()..])
        .expect("signed settlement witness");
    let unlock = witness.unlocks.last().expect("unlock");
    verify_onchain_signature(&key.pubkey(), content, &unlock.signature)
        .expect("correct unlock signature");
    assert_eq!(tx.hash(), content.transaction.calc_tx_hash());
}

#[test]
fn external_request_reuses_pending_template_without_querying_collector() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let fixture = Fixture::new(for_remote, subsequent, false, false);
            assert!(fixture
                .build_with_collector(200_000, &mut PanicCollector)
                .unwrap()
                .is_none());
            let (id, pending) = fixture.pending_request();
            assert!(fixture
                .build_with_collector(200_001, &mut PanicCollector)
                .unwrap()
                .is_none());
            assert_eq!(fixture.pending(), (id, pending.content.clone()));
            let funded = fund_pending(&pending);
            let signature = sign_onchain_request(&fixture.tlc_key, &funded).unwrap();
            fixture
                .submit_with_tx(id, signature, funded.transaction.clone())
                .unwrap();
            let tx = fixture
                .build_with_collector(200_002, &mut PanicCollector)
                .unwrap()
                .unwrap();
            assert_applied_signature(&tx, &funded, &fixture.tlc_key);
        }
    }
}

#[test]
fn external_timeout_signing_waits_for_tlc_expiry() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let fixture = Fixture::new(for_remote, subsequent, false, false);
            assert!(fixture.build_at(99_999).expect("before expiry").is_none());
            let WatchtowerSignerState::External(state) = fixture
                .store
                .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
            else {
                panic!("expected external signer state");
            };
            assert!(state.pending_requests.is_empty());
            assert!(fixture.build_at(100_001).expect("after expiry").is_none());
            let (_, pending) = fixture.pending_request();
            assert_eq!(pending.content.transaction.raw().inputs().len(), 1);
            assert_eq!(
                pending.required_expiry_since,
                Some(fiber_types::watchtower::settlement_timestamp_since(100_000))
            );
        }
    }
}

#[test]
fn external_tlc_signature_survives_advancing_chain_time() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            for with_preimage in [false, true] {
                for with_udt in [false, true] {
                    let mut fixture = Fixture::new(for_remote, subsequent, with_preimage, false);
                    if with_udt {
                        let type_script = get_script_by_contract(Contract::SimpleUDT, &[31; 32]);
                        let output: CellOutput = fixture.cell.output.clone().into();
                        fixture.cell.output = output
                            .as_builder()
                            .type_(Some(type_script).pack())
                            .build()
                            .into();
                        fixture.cell.output_data = Some(ckb_jsonrpc_types::JsonBytes::from_vec(
                            240_000_000_000u128.to_le_bytes().to_vec(),
                        ));
                    }
                    assert!(fixture
                        .build_with_collector(200_000, &mut PanicCollector)
                        .expect("queue request")
                        .is_none());
                    let (request_id, pending) = fixture.pending_request();
                    let funded = fund_pending(&pending);
                    let signature =
                        sign_onchain_request(&fixture.tlc_key, &funded).expect("TLC signature");
                    assert_eq!(
                        fixture
                            .submit_with_tx(request_id, signature, funded.transaction.clone())
                            .expect("submit"),
                        SubmitWatchtowerSignatureResult::Applied
                    );

                    // The next scan observes a later chain median time, but the
                    // same commitment and TLC must reuse this signed transaction.
                    let tx = fixture
                        .build_with_collector(260_000, &mut PanicCollector)
                        .expect("next scan")
                        .expect("accepted signature must produce a signed transaction");
                    assert_applied_signature(&tx, &funded, &fixture.tlc_key);
                    if !with_preimage {
                        let fee_since: u64 = tx
                            .inputs()
                            .get(1)
                            .expect("wallet funding input")
                            .since()
                            .unpack();
                        assert_eq!(
                            fee_since,
                            fiber_types::watchtower::settlement_timestamp_since(100_000)
                        );
                    }
                    let WatchtowerSignerState::External(state) = fixture
                        .store
                        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
                    else {
                        panic!("expected external signer state");
                    };
                    assert!(state.pending_requests.is_empty());
                    assert_eq!(state.signed_signatures.len(), 1);
                }
            }
        }
    }
}

#[test]
fn external_tlc_signing_uses_creation_index_through_rpc_and_transaction_builder() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            for with_preimage in [false, true] {
                let fixture = Fixture::new(for_remote, subsequent, with_preimage, false);
                assert!(fixture.build().expect("queue request").is_none());
                let (request_id, pending) = fixture.pending_request();
                assert_eq!(
                    pending.content.key_purpose,
                    OnchainKeyPurpose::Tlc {
                        commitment_number: 7
                    }
                );
                assert!(fixture.build().expect("still pending").is_none());
                assert_eq!(fixture.pending(), (request_id, pending.content.clone()));
                let funded = fund_pending(&pending);
                let wrong =
                    sign_onchain_request(&fixture.base_key, &funded).expect("wrong-key signature");
                assert!(fixture
                    .submit_with_tx(request_id, wrong, funded.transaction.clone())
                    .is_err());
                assert_eq!(fixture.pending(), (request_id, pending.content.clone()));
                let signature =
                    sign_onchain_request(&fixture.tlc_key, &funded).expect("TLC signature");
                assert_eq!(
                    fixture
                        .submit_with_tx(request_id, signature, funded.transaction.clone())
                        .expect("submit"),
                    SubmitWatchtowerSignatureResult::Applied
                );
                assert_eq!(
                    fixture
                        .submit_with_tx(request_id, signature, funded.transaction.clone())
                        .expect("retry"),
                    SubmitWatchtowerSignatureResult::AlreadyApplied
                );
                let tx = fixture
                    .build()
                    .expect("apply signature")
                    .expect("signed tx");
                assert_applied_signature(&tx, &funded, &fixture.tlc_key);
            }
        }
    }
}

#[test]
fn external_tlc_signing_rejects_missing_derivation_index_and_public_key() {
    for missing_pubkey in [false, true] {
        let mut fixture = Fixture::new(true, true, false, false);
        for data in [
            &mut fixture.channel.local_settlement_data,
            &mut fixture.channel.remote_settlement_data,
            &mut fixture.channel.pending_remote_settlement_data,
        ] {
            if missing_pubkey {
                data.tlcs[1].local_key_pubkey = None;
            } else {
                data.tlcs[1].local_key_commitment_number = None;
            }
        }
        let error = fixture
            .build()
            .expect_err("must fail before queuing")
            .to_string();
        assert!(
            error.contains(if missing_pubkey {
                "public key is missing"
            } else {
                "derivation index is missing"
            }),
            "{error}"
        );
        let WatchtowerSignerState::External(state) = fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        else {
            panic!("expected external signer state");
        };
        assert!(state.pending_requests.is_empty());
    }
}

#[test]
fn external_tlc_rpc_rejects_unknown_derivation_index_without_settlement_key_fallback() {
    let fixture = Fixture::new(true, false, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, mut pending) = fixture.pending_request();
    pending.content.key_purpose = OnchainKeyPurpose::Tlc {
        commitment_number: 99,
    };
    let funded = fund_pending(&pending);
    let signature = sign_onchain_request(&fixture.base_key, &funded).expect("base-key signature");
    let mut state = fiber_types::WatchtowerExternalSignerState::default();
    state.pending_requests.insert(request_id, pending);
    fixture.store.put_watchtower_signer(
        &fixture.node_id,
        &fixture.channel.channel_id,
        WatchtowerSignerState::External(state),
    );
    let err = fixture
        .submit_with_tx(request_id, signature, funded.transaction)
        .expect_err("unknown index");
    assert!(err.message().contains("TLC signing key not found"));
}

#[test]
fn legacy_local_tlc_key_still_signs_without_derivation_metadata() {
    for subsequent in [false, true] {
        let fixture = Fixture::new(true, subsequent, false, true);
        let tx = fixture.build().expect("local signing").expect("signed tx");
        // The signature digest depends on the raw transaction, not its purpose.
        let content = OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Tlc {
                commitment_number: 7,
            },
            transaction: tx.data(),
        };
        assert_applied_signature(&tx, &content, &fixture.tlc_key);
    }
}

#[test]
fn spent_inputs_prune_watchtower_requests_and_publish_successor() {
    use crate::signing::notifications::{SigningChange, SigningScope};
    use fiber_json_types::WatchtowerSigningStatus;

    let fixture = Fixture::new(false, false, false, false);
    let Fixture {
        events,
        store,
        node_id,
        channel,
        base_key,
        ..
    } = &fixture;
    let channel_id = &channel.channel_id;
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut changes = events
        .hub
        .subscribe(SigningScope::Watchtower(node_id.clone()));
    let mut requests: Vec<_> = [3u8, 99]
        .into_iter()
        .map(|seed| {
            let content = OnchainSigningContent {
                key_purpose: OnchainKeyPurpose::Settlement,
                transaction: TransactionBuilder::default()
                    .inputs([0, 1].map(|index| {
                        CellInput::new(OutPoint::new(Byte32::from([seed; 32]), index), 0)
                    }))
                    .build()
                    .data(),
            };
            let request_id = Hash256::from(ckb_hash::blake2b_256(content.transaction.as_slice()));
            (request_id, content)
        })
        .collect();
    // Make the stale request sort first, reproducing the RPC/notification blockage.
    requests.sort_by_key(|(id, _)| *id);
    let (stale_id, stale) = &requests[0];
    let (live_id, live) = &requests[1];
    // Either a commitment input or a fee input can invalidate a request.
    let now_ms = crate::now_timestamp_as_millis_u64();
    for input_index in [0, 1] {
        for already_signed in [false, true] {
            let mut external = fiber_types::WatchtowerExternalSignerState::default();
            if already_signed {
                external.signed_signatures = requests
                    .iter()
                    .map(|(id, content)| {
                        (
                            *id,
                            fiber_types::SignedWatchtowerRequest {
                                content: content.clone(),
                                signature: sign_onchain_request(base_key, content).unwrap(),
                                created_at_ms: now_ms,
                            },
                        )
                    })
                    .collect();
                let stale_sig = external.signed_signatures[stale_id].signature;
                external.last_applied = Some(fiber_types::LastAppliedWatchtowerSignature {
                    request_id: *stale_id,
                    signature: stale_sig,
                    signed_transaction_hash: fiber_types::external_funding_watchtower_tx_hash(
                        &stale.transaction,
                    ),
                });
            } else {
                external.pending_requests = requests
                    .iter()
                    .map(|(id, content)| {
                        (
                            *id,
                            fiber_types::PendingWatchtowerRequest {
                                content: content.clone(),
                                created_at_ms: now_ms,
                                required_expiry_since: None,
                            },
                        )
                    })
                    .collect();
            }
            let state = WatchtowerSignerState::External(external);
            store.put_watchtower_signer(node_id, channel_id, state.clone());
            // Unrelated chain spends must leave both valid requests untouched.
            let unrelated = HashSet::from([OutPoint::new(Byte32::from([200; 32]), 0)]);
            prune_spent_signing_requests(events, store, node_id, channel_id, &unrelated);
            assert_eq!(store.get_watchtower_signer(node_id, channel_id), state);
            let spent = HashSet::from([stale
                .transaction
                .raw()
                .inputs()
                .get(input_index)
                .unwrap()
                .previous_output()]);
            prune_spent_signing_requests(events, store, node_id, channel_id, &spent);
            let WatchtowerSignerState::External(state) =
                store.get_watchtower_signer(node_id, channel_id)
            else {
                panic!("external signer");
            };
            assert!(!state.pending_requests.contains_key(stale_id));
            assert!(!state.signed_signatures.contains_key(stale_id));
            if already_signed {
                assert_eq!(state.signed_signatures.len(), 1);
                assert_eq!(&state.signed_signatures[live_id].content, live);
                if input_index == 0 {
                    // Commitment cell spent on-chain -> preserve last_applied receipt.
                    assert_eq!(
                        state.last_applied.as_ref().map(|a| a.request_id),
                        Some(*stale_id)
                    );
                } else {
                    // Only wallet fee input spent while commitment cell remained live -> clear receipt.
                    assert!(
                        state.last_applied.is_none(),
                        "fee-only spend must clear last_applied"
                    );
                }
            } else {
                assert_eq!(state.pending_requests.len(), 1);
                assert_eq!(&state.pending_requests[live_id].content, live);
            }
            let expected = crate::rpc::watchtower::to_rpc_watchtower_signing_status(
                WatchtowerSignerState::External(state),
            );
            match &expected {
                WatchtowerSigningStatus::ExternalFundingRequired { request_id, .. } => {
                    assert_eq!(*request_id, (*live_id).into())
                }
                WatchtowerSigningStatus::NoSignatureRequired => assert!(already_signed),
                WatchtowerSigningStatus::Backoff { .. } => {
                    panic!("unexpected backoff during live signing test")
                }
            }
            let event = runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(1), changes.recv())
                    .await
                    .expect("published change")
                    .expect("receiver")
            });
            let SigningChange::WatchtowerUpdated { status, .. } = event.as_ref() else {
                panic!("watchtower update");
            };
            assert_eq!(status, &expected);
        }
    }
}

#[test]
fn pruned_signed_request_can_be_regenerated() {
    let fixture = Fixture::new(false, false, false, false);
    assert!(fixture.build().unwrap().is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let signature = sign_onchain_request(&fixture.tlc_key, &funded).unwrap();
    fixture
        .submit_with_tx(request_id, signature, funded.transaction.clone())
        .unwrap();
    let spent = HashSet::from([pending
        .content
        .transaction
        .raw()
        .inputs()
        .get(0)
        .unwrap()
        .previous_output()]);
    prune_spent_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &spent,
    );
    let WatchtowerSignerState::External(state) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(state.pending_requests.is_empty() && state.signed_signatures.is_empty());
    assert_eq!(state.last_applied.unwrap().request_id, request_id);

    // Model the original cell becoming live again after a reorg. Rebuilding
    // issues a fresh request ID for the same template so a new submission can proceed.
    assert!(fixture.build().unwrap().is_none());
    let (regenerated_id, regenerated_content) = fixture.pending();
    assert_ne!(regenerated_id, request_id);
    assert_eq!(regenerated_content, pending.content);
    assert_eq!(
        fixture
            .submit_with_tx(regenerated_id, signature, funded.transaction)
            .unwrap(),
        SubmitWatchtowerSignatureResult::Applied
    );
    assert!(fixture.build().unwrap().is_some());
}

#[test]
fn external_final_settlement_uses_base_key_through_transaction_builder() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let mut fixture = Fixture::new(for_remote, false, false, false);
            for data in [
                &mut fixture.channel.local_settlement_data,
                &mut fixture.channel.remote_settlement_data,
                &mut fixture.channel.pending_remote_settlement_data,
            ] {
                data.tlcs.clear();
            }
            let snapshot = &fixture.channel.local_settlement_data;
            let witness_bytes = settlement_data_to_witness(
                snapshot,
                for_remote,
                fixture.base_key.pubkey(),
                fixture.channel.remote_settlement_key,
            );
            let output: CellOutput = fixture.cell.output.clone().into();
            let mut args = output.lock().args().raw_data()[..36].to_vec();
            args.extend_from_slice(blake160(&witness_bytes).as_ref());
            args.extend_from_slice(&[0, ChannelFeatures::V2.bits()]);
            let lock = output.lock().as_builder().args(args.pack()).build();
            fixture.tracked =
                crate::tests::verified_tracked_settlement_tlcs(&lock, &fixture.channel, for_remote)
                    .expect("empty snapshot is verified");
            fixture.cell.output = output.as_builder().lock(lock).build().into();
            fixture.witness = subsequent.then(|| {
                SettlementWitness::build_from_witness(&[&[0], witness_bytes.as_slice()].concat())
                    .expect("settlement witness")
            });
            use crate::signing::notifications::{SigningChange, SigningScope};
            let mut events = fixture
                .events
                .hub
                .subscribe(SigningScope::Watchtower(fixture.node_id.clone()));
            let runtime = tokio::runtime::Runtime::new().expect("event runtime");
            let receive = |events: &mut crate::signing::notifications::SigningChanges| {
                runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                        .await
                        .expect("business event")
                        .expect("active receiver")
                })
            };
            assert!(fixture.build().expect("queue settlement").is_none());
            assert!(matches!(
                receive(&mut events).as_ref(),
                SigningChange::WatchtowerUpdated {
                    status: fiber_json_types::WatchtowerSigningStatus::ExternalFundingRequired { .. },
                    ..
                }
            ));
            let (request_id, pending) = fixture.pending_request();
            assert_eq!(pending.content.key_purpose, OnchainKeyPurpose::Settlement);
            let funded = fund_pending(&pending);
            let wrong =
                sign_onchain_request(&fixture.tlc_key, &funded).expect("wrong-key signature");
            assert!(fixture
                .submit_with_tx(request_id, wrong, funded.transaction.clone())
                .is_err());
            let signature =
                sign_onchain_request(&fixture.base_key, &funded).expect("settlement signature");
            fixture
                .submit_with_tx(request_id, signature, funded.transaction.clone())
                .expect("submit settlement");
            // The rejected signature above emitted nothing; the next event is
            // the successfully committed signature's cleared pending status.
            assert!(matches!(
                receive(&mut events).as_ref(),
                SigningChange::WatchtowerUpdated {
                    status: fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired,
                    ..
                }
            ));
            let tx = fixture
                .build()
                .expect("apply settlement")
                .expect("signed tx");
            assert_applied_signature(&tx, &funded, &fixture.base_key);
        }
    }
}

#[test]
fn external_tlc_signing_replaces_stale_request_purpose_for_same_transaction() {
    for already_signed in [false, true] {
        let fixture = Fixture::new(true, true, false, false);
        assert!(fixture.build().expect("queue").is_none());
        let (request_id, expected_pending) = fixture.pending_request();
        let expected = expected_pending.content.clone();
        let mut stale = expected.clone();
        stale.key_purpose = OnchainKeyPurpose::Tlc {
            commitment_number: 10,
        };
        let wrong_signature =
            sign_onchain_request(&fixture.base_key, &stale).expect("old signature");
        let mut state = fiber_types::WatchtowerExternalSignerState::default();
        let now_ms = crate::now_timestamp_as_millis_u64();
        if already_signed {
            state.signed_signatures.insert(
                request_id,
                fiber_types::SignedWatchtowerRequest {
                    content: stale,
                    signature: wrong_signature,
                    created_at_ms: now_ms,
                },
            );
            state.last_applied = Some(fiber_types::LastAppliedWatchtowerSignature {
                request_id,
                signature: wrong_signature,
                signed_transaction_hash: Hash256::default(),
            });
        } else {
            state.pending_requests.insert(
                request_id,
                fiber_types::PendingWatchtowerRequest {
                    content: stale,
                    created_at_ms: now_ms,
                    required_expiry_since: expected_pending.required_expiry_since,
                },
            );
        }
        fixture.store.put_watchtower_signer(
            &fixture.node_id,
            &fixture.channel.channel_id,
            WatchtowerSignerState::External(state),
        );
        assert!(fixture.build().expect("replace stale request").is_none());
        let (new_request_id, replaced_pending) = fixture.pending_request();
        assert_ne!(new_request_id, request_id);
        assert_eq!(replaced_pending.content, expected);
        let funded = fund_pending(&expected_pending);
        let signature =
            sign_onchain_request(&fixture.tlc_key, &funded).expect("corrected signature");
        assert_eq!(
            fixture
                .submit_with_tx(new_request_id, signature, funded.transaction.clone())
                .expect("submit corrected signature"),
            SubmitWatchtowerSignatureResult::Applied
        );
        let tx = fixture
            .build()
            .expect("apply corrected signature")
            .expect("signed tx");
        assert_applied_signature(&tx, &funded, &fixture.tlc_key);
    }
}

#[test]
fn external_tlc_signing_rejects_conflicting_public_keys_for_same_index() {
    let mut fixture = Fixture::new(true, false, false, false);
    fixture.channel.local_settlement_data.tlcs[1].local_key_pubkey =
        Some(fixture.base_key.pubkey());
    let error = fixture.build().expect_err("conflicting keys").to_string();
    assert!(
        error.contains("conflicting TLC signing public keys"),
        "{error}"
    );
}

#[test]
fn concurrent_watchtower_rpc_submissions_preserve_both_signatures() {
    let fixture = Fixture::new(false, false, false, false);
    assert!(fixture.build().expect("first request").is_none());
    let (first_id, first_pending) = fixture.pending_request();
    let mut second = Fixture::new(false, false, false, false);
    second.store = fixture.store.clone();
    second.cell.out_point = OutPoint::new(Byte32::from([99; 32]), 1).into();
    assert!(second.build().expect("second request").is_none());
    let WatchtowerSignerState::External(state) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    let (second_id, second_pending) = state
        .pending_requests
        .iter()
        .find(|(id, _)| **id != first_id)
        .map(|(id, req)| (*id, req.clone()))
        .expect("second request");
    let first_funded = fund_pending(&first_pending);
    let second_funded = fund_pending(&second_pending);
    let first_sig = sign_onchain_request(&fixture.tlc_key, &first_funded).expect("first signature");
    let second_sig =
        sign_onchain_request(&fixture.tlc_key, &second_funded).expect("second signature");
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first_submit = scope.spawn(|| {
            start.wait();
            fixture.submit_with_tx(first_id, first_sig, first_funded.transaction.clone())
        });
        let second_submit = scope.spawn(|| {
            start.wait();
            second.submit_with_tx(second_id, second_sig, second_funded.transaction.clone())
        });
        for submit in [first_submit, second_submit] {
            assert_eq!(
                submit
                    .join()
                    .expect("submit thread")
                    .expect("submit signature"),
                SubmitWatchtowerSignatureResult::Applied
            );
        }
    });
    let first_tx = fixture
        .build()
        .expect("first scan")
        .expect("first signed tx");
    let second_tx = second
        .build()
        .expect("second scan")
        .expect("second signed tx");
    assert_applied_signature(&first_tx, &first_funded, &fixture.tlc_key);
    assert_applied_signature(&second_tx, &second_funded, &fixture.tlc_key);
    let WatchtowerSignerState::External(state) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(state.pending_requests.is_empty());
    assert_eq!(state.signed_signatures.len(), 2);
}

#[test]
fn test_watchtower_signature_request_overwrite_repro() {
    let fixture = Fixture::new(false, false, false, false);

    // 1. Initial build creates request 1 (req_1)
    assert!(fixture.build().expect("build 1").is_none());
    let (req_1, pending_1) = fixture.pending_request();

    // 2. Simulate a second transaction/cell being built for the SAME channel
    // before req_1 is signed.
    let mut modified_fixture = Fixture::new(false, false, false, false);
    modified_fixture.store = fixture.store.clone();
    // Point to a distinct outpoint so that the constructed transaction differs
    modified_fixture.cell.out_point = OutPoint::new(Byte32::from([99; 32]), 1).into();

    assert!(modified_fixture.build().expect("build 2").is_none());
    let WatchtowerSignerState::External(signer_state) =
        modified_fixture.store.get_watchtower_signer(
            &modified_fixture.node_id,
            &modified_fixture.channel.channel_id,
        )
    else {
        panic!("expected external signer state");
    };
    assert_eq!(
        signer_state.pending_requests.len(),
        2,
        "both requests must be pending"
    );
    let (req_2, pending_2) = signer_state
        .pending_requests
        .iter()
        .find(|(k, _)| **k != req_1)
        .map(|(k, v)| (*k, v.clone()))
        .expect("second pending request");
    assert_ne!(
        req_1, req_2,
        "two distinct transactions must produce distinct request_ids"
    );

    // 3. Client funds & signs req_1 and submits via RPC
    let funded_1 = fund_pending(&pending_1);
    let sig_1 = sign_onchain_request(&fixture.tlc_key, &funded_1).expect("TLC signature 1");
    let submit_1_res = fixture.submit_with_tx(req_1, sig_1, funded_1.transaction.clone());
    assert_eq!(
        submit_1_res.expect("submit req_1 must succeed without being overwritten"),
        SubmitWatchtowerSignatureResult::Applied
    );

    // 4. Client also funds & signs req_2 and submits via RPC
    let funded_2 = fund_pending(&pending_2);
    let sig_2 = sign_onchain_request(&fixture.tlc_key, &funded_2).expect("TLC signature 2");
    let submit_2_res = fixture.submit_with_tx(req_2, sig_2, funded_2.transaction.clone());
    assert_eq!(
        submit_2_res.expect("submit req_2 must succeed"),
        SubmitWatchtowerSignatureResult::Applied
    );

    // 5. Watchtower loop can now consume signatures for both transactions successfully
    let tx_1 = fixture
        .build()
        .expect("build signed tx 1")
        .expect("tx 1 signed");
    assert_applied_signature(&tx_1, &funded_1, &fixture.tlc_key);

    let tx_2 = modified_fixture
        .build()
        .expect("build signed tx 2")
        .expect("tx 2 signed");
    assert_applied_signature(&tx_2, &funded_2, &fixture.tlc_key);

    // 6. Test idempotency & persistence across scan cycles before on-chain confirmation:
    // Retrying submission for req_1 must return AlreadyApplied
    assert_eq!(
        fixture
            .submit_with_tx(req_1, sig_1, funded_1.transaction.clone())
            .expect("retry submit req_1 after build"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );

    // Re-building tx_1 before onchain confirmation must still return the signed tx and not re-ask
    let tx_1_second = fixture
        .build()
        .expect("build signed tx 1 again")
        .expect("tx 1 still signed");
    assert_applied_signature(&tx_1_second, &funded_1, &fixture.tlc_key);
}

#[test]
fn external_full_hash_tlc_signature_survives_later_scan() {
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            for with_preimage in [false, true] {
                let fixture = Fixture::with_features(
                    for_remote,
                    subsequent,
                    with_preimage,
                    false,
                    ChannelFeatures::V2,
                );
                assert!(fixture.build_at(200_000).unwrap().is_none());
                let (id, pending) = fixture.pending_request();
                let funded = fund_pending(&pending);
                let signature = sign_onchain_request(&fixture.tlc_key, &funded).unwrap();
                assert_eq!(
                    fixture
                        .submit_with_tx(id, signature, funded.transaction.clone())
                        .unwrap(),
                    SubmitWatchtowerSignatureResult::Applied
                );
                let tx = fixture
                    .build_at(260_000)
                    .unwrap()
                    .expect("signed transaction");
                assert_applied_signature(&tx, &funded, &fixture.tlc_key);
            }
        }
    }
}

#[test]
fn external_pending_request_expires_after_ttl_and_enforces_backoff() {
    let fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);

    let first = fixture.build().expect("build settlement");
    assert!(first.is_none(), "external signer pauses for signature");

    let channels = fixture.store.get_watch_channels_with_nodes();
    let (request_id, created_at_ms) = match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert_eq!(ext.pending_requests.len(), 1);
            let (&id, req) = ext.pending_requests.iter().next().expect("pending request");
            (id, req.created_at_ms)
        }
        other => panic!("expected external signer state, got {other:?}"),
    };

    // Mid-TTL sync must NOT renew created_at_ms or drop the request.
    let mid_ttl = created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS / 2;
    crate::watchtower::actor::tests::expire_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &channels,
        mid_ttl,
    );
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert_eq!(
                ext.pending_requests
                    .get(&request_id)
                    .map(|r| r.created_at_ms),
                Some(created_at_ms)
            );
            assert_eq!(ext.consecutive_expired_requests, 0);
        }
        other => panic!("expected external signer state, got {other:?}"),
    }

    // Past TTL: unsigned request must expire and arm backoff.
    let expired_at = created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS;
    crate::watchtower::actor::tests::expire_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &channels,
        expired_at,
    );
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert!(ext.pending_requests.is_empty());
            assert_eq!(ext.consecutive_expired_requests, 1);
            assert!(ext.retry_after_ms > expired_at);
        }
        other => panic!("expected external signer state, got {other:?}"),
    }

    // While in retry backoff (set into the future relative to real clock), build_settlement_tx
    // must refuse to create a new request.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let fiber_types::WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = crate::now_timestamp_as_millis_u64() + 60_000;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture.build().expect("build during backoff").is_none());
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert!(
                ext.pending_requests.is_empty(),
                "channel in backoff must not create a new pending request"
            );
        }
        other => panic!("expected external signer state, got {other:?}"),
    }

    // Signed-but-unbroadcast signatures must also expire after WATCHTOWER_HOLD_MS.
    let signed_id = Hash256::from([0xab; 32]);
    let signed_created_at = expired_at + 10_000;
    let signed_tx = TransactionBuilder::default()
        .input(CellInput::new(OutPoint::new(Byte32::from([1; 32]), 0), 0))
        .build()
        .data();
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let fiber_types::WatchtowerSignerState::External(ext) = state {
                ext.signed_signatures.insert(
                    signed_id,
                    fiber_types::SignedWatchtowerRequest {
                        content: fiber_types::OnchainSigningContent {
                            key_purpose: fiber_types::OnchainKeyPurpose::Settlement,
                            transaction: signed_tx,
                        },
                        signature: [0u8; 65],
                        created_at_ms: signed_created_at,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    crate::watchtower::actor::tests::expire_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &channels,
        signed_created_at + crate::ckb::held_cells::WATCHTOWER_HOLD_MS,
    );
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert!(
                ext.signed_signatures.is_empty(),
                "expired signed_signature must be evicted"
            );
        }
        other => panic!("expected external signer state, got {other:?}"),
    }
}

#[test]
fn external_signing_request_caps_and_cross_channel_per_node_backoff() {
    let fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
    let now_ms = crate::now_timestamp_as_millis_u64();

    let dummy_content = |seed: u8| OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([seed; 32]), 0),
                0,
            ))
            .input(CellInput::new(
                OutPoint::new(Byte32::from([seed.wrapping_add(1); 32]), 0),
                0,
            ))
            .build()
            .data(),
    };

    let insert_extra_external_channel = |store: &Store, node_id: &NodeId, channel_seed: u8| {
        let channel_id = Hash256::from([channel_seed; 32]);
        let base_key = Privkey::from([42; 32]);
        store.insert_watch_channel(
            node_id.clone(),
            channel_id,
            None,
            WatchChannelKeys {
                local_settlement: SettlementKey::external(base_key.pubkey()),
                remote_settlement: Privkey::from([10; 32]).pubkey(),
                local_funding: Privkey::from([11; 32]).pubkey(),
                remote_funding: Privkey::from([12; 32]).pubkey(),
            },
            SettlementData {
                local_amount: 1,
                remote_amount: 1,
                tlcs: Vec::new(),
            },
            ChannelFeatures::V2,
        );
        channel_id
    };

    let pending_len = |store: &Store, node_id: &NodeId, channel_id: &Hash256| -> usize {
        match store.get_watchtower_signer(node_id, channel_id) {
            WatchtowerSignerState::External(ext) => ext.pending_requests.len(),
            other => panic!("expected external signer state, got {other:?}"),
        }
    };

    // 1. Per-channel cap (4): 2 pending + 2 signed on the same channel blocks new requests.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for i in 0..2u8 {
                    let id = Hash256::from([10 + i; 32]);
                    ext.pending_requests.insert(
                        id,
                        fiber_types::PendingWatchtowerRequest {
                            content: dummy_content(10 + i),
                            created_at_ms: now_ms,
                            required_expiry_since: None,
                        },
                    );
                }
                for i in 0..2u8 {
                    let id = Hash256::from([20 + i; 32]);
                    ext.signed_signatures.insert(
                        id,
                        fiber_types::SignedWatchtowerRequest {
                            content: dummy_content(20 + i),
                            signature: [0u8; 65],
                            created_at_ms: now_ms,
                        },
                    );
                }
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture.build().expect("build at per-channel cap").is_none());
    assert_eq!(
        pending_len(
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id
        ),
        2,
        "per-channel cap (pending + signed == 4) must block new external signing requests"
    );

    // Clear fixture channel state for subsequent checks.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.clear();
                ext.signed_signatures.clear();
                ext.retry_after_ms = 0;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // 2. Cross-channel per-node backoff: backoff armed on sibling channel blocks fixture channel.
    let sibling_channel = insert_extra_external_channel(&fixture.store, &fixture.node_id, 0x70);
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &sibling_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = now_ms + 60_000;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture
        .build()
        .expect("build during sibling backoff")
        .is_none());
    assert_eq!(
        pending_len(
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id
        ),
        0,
        "backoff on sibling channel of same node_id must block new requests"
    );

    // Clear sibling backoff; backoff on an unrelated node_id must NOT block fixture channel.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &sibling_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = 0;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    let other_node = NodeId::from_bytes(Privkey::from([88; 32]).pubkey().serialize().to_vec());
    let other_channel = insert_extra_external_channel(&fixture.store, &other_node, 0x71);
    fixture
        .store
        .update_watchtower_signer(&other_node, &other_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = now_ms + 60_000;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // 3. Per-node cap (16): 16 active requests on sibling channel blocks fixture channel.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &sibling_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for i in 0..8u8 {
                    let id = Hash256::from([40 + i; 32]);
                    ext.pending_requests.insert(
                        id,
                        fiber_types::PendingWatchtowerRequest {
                            content: dummy_content(40 + i),
                            created_at_ms: now_ms,
                            required_expiry_since: None,
                        },
                    );
                }
                for i in 0..8u8 {
                    let id = Hash256::from([50 + i; 32]);
                    ext.signed_signatures.insert(
                        id,
                        fiber_types::SignedWatchtowerRequest {
                            content: dummy_content(50 + i),
                            signature: [0u8; 65],
                            created_at_ms: now_ms,
                        },
                    );
                }
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture.build().expect("build at per-node cap").is_none());
    assert_eq!(
        pending_len(
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id
        ),
        0,
        "per-node cap (16) must block new requests across channels of the same node"
    );

    // Clear sibling holds for global cap test.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &sibling_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.clear();
                ext.signed_signatures.clear();
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // 4. Global cap (64): 64 requests on another node blocks fixture channel; 63 allows it.
    fixture
        .store
        .update_watchtower_signer(&other_node, &other_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for i in 0..64u8 {
                    let id = Hash256::from([100 + i; 32]);
                    ext.pending_requests.insert(
                        id,
                        fiber_types::PendingWatchtowerRequest {
                            content: dummy_content(100 + i),
                            created_at_ms: now_ms,
                            required_expiry_since: None,
                        },
                    );
                }
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture.build().expect("build at global cap").is_none());
    assert_eq!(
        pending_len(
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id
        ),
        0,
        "global cap (64) must block new requests from any node"
    );

    // Free 1 global slot (63 active) -> fixture channel can now create a pending request
    // (also confirming other_node's backoff does not affect fixture.node_id).
    fixture
        .store
        .update_watchtower_signer(&other_node, &other_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                let first_id = Hash256::from([100; 32]);
                ext.pending_requests.remove(&first_id);
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture.build().expect("build below global cap").is_none());
    assert_eq!(
        pending_len(
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id
        ),
        1,
        "once below global cap and outside own-node backoff, new external signing request succeeds"
    );
}

#[allow(clippy::too_many_arguments)]
fn build_custom_ckb_fixture(
    for_remote: bool,
    subsequent: bool,
    local_amount: u128,
    remote_amount: u128,
    tlcs: Vec<SettlementTlc>,
    preimages: Vec<(Hash256, Hash256)>,
    local_keys: bool,
) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = open_store(dir.path()).expect("open store");
    let node_id = NodeId::local();
    let base_key = Privkey::from([42; 32]);
    let tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());

    // For subsequent settlement tests, prepend a dummy TLC at index 0 that was
    // already unlocked in the preceding witness so `tlcs` remain at indices 0..N.
    let mut full_tlcs = Vec::new();
    if subsequent {
        let unlocked_key = Privkey::from([5; 32]);
        full_tlcs.push(SettlementTlc {
            tlc_id: TLCId::Offered(99),
            hash_algorithm: HashAlgorithm::Sha256,
            payment_amount: 10_000_000_000,
            payment_hash: Hash256::from([99; 32]),
            expiry: 50_000,
            local_key: local_keys.then(|| unlocked_key.clone()),
            local_key_pubkey: Some(unlocked_key.pubkey()),
            local_key_commitment_number: (!local_keys).then_some(5),
            remote_key: Privkey::from([12; 32]).pubkey(),
        });
    }
    full_tlcs.extend(tlcs);

    let settlement = SettlementData {
        local_amount,
        remote_amount,
        tlcs: full_tlcs,
    };
    let channel_id = Hash256::from([9; 32]);
    store.insert_watch_channel(
        node_id.clone(),
        channel_id,
        None,
        WatchChannelKeys {
            local_settlement: if local_keys {
                SettlementKey::local(base_key.clone())
            } else {
                SettlementKey::external(base_key.pubkey())
            },
            remote_settlement: Privkey::from([10; 32]).pubkey(),
            local_funding: Privkey::from([11; 32]).pubkey(),
            remote_funding: Privkey::from([12; 32]).pubkey(),
        },
        settlement,
        ChannelFeatures::V2,
    );
    for (payment_hash, preimage) in preimages {
        store.insert_watch_preimage(node_id.clone(), payment_hash, preimage);
    }
    let channel = store
        .get_watch_channel(&node_id, &channel_id)
        .expect("channel");
    let snapshot = &channel.local_settlement_data;
    let witness_bytes = settlement_data_to_witness(
        snapshot,
        for_remote,
        base_key.pubkey(),
        channel.remote_settlement_key,
    );
    let since = Since::new(
        SinceType::EpochNumberWithFraction,
        EpochNumberWithFraction::new(3, 0, 1).full_value(),
        true,
    )
    .value();
    let mut args =
        ckb_hash::blake2b_256(channel_data_x_only_aggregated_pubkey(&channel, !for_remote))[..20]
            .to_vec();
    args.extend_from_slice(&since.to_le_bytes());
    args.extend_from_slice(&10u64.to_be_bytes());
    args.extend_from_slice(blake160(&witness_bytes).as_ref());
    args.push(0);
    args.push(ChannelFeatures::V2.bits());
    let mut lock = get_script_by_contract(Contract::CommitmentLock, &args);
    let mut tracked = crate::tests::verified_tracked_settlement_tlcs(&lock, &channel, for_remote)
        .expect("verified TLC identities");
    let witness = subsequent.then(|| {
        let mut witness =
            SettlementWitness::build_from_witness(&[&[0], witness_bytes.as_slice()].concat())
                .expect("witness");
        witness.unlocks.push(Unlock {
            unlock_type: 0,
            with_preimage: false,
            signature: [0; 65],
            preimage: None,
        });
        tracked.remove(0);
        witness
    });
    if let Some(previous) = &witness {
        let mut remaining = previous.clone();
        assert!(remaining.update());
        let mut args = lock.args().raw_data()[..36].to_vec();
        args.extend_from_slice(blake160(&remaining.to_witness()).as_ref());
        args.push(1);
        args.push(ChannelFeatures::V2.bits());
        lock = lock.as_builder().args(args.pack()).build();
    }
    let cell = Cell {
        output: CellOutput::new_builder()
            .capacity(250_000_000_000u64)
            .lock(lock)
            .build()
            .into(),
        output_data: Some(ckb_jsonrpc_types::JsonBytes::default()),
        out_point: OutPoint::new(Byte32::from([3; 32]), 0).into(),
        block_number: 0u64.into(),
        tx_index: 0u32.into(),
    };
    Fixture {
        events: Default::default(),
        store,
        _dir: dir,
        node_id,
        channel,
        cell,
        tracked,
        witness,
        base_key,
        tlc_key,
        for_remote,
    }
}

#[derive(Clone)]
struct PanicCollector;

#[async_trait::async_trait]
impl CellCollector for PanicCollector {
    async fn collect_live_cells_async(
        &mut self,
        _: &CellQueryOptions,
        _: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        panic!("external-funding watchtower template tx must never query cell_collector");
    }
    fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
        unreachable!()
    }
    fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
        unreachable!()
    }
    fn reset(&mut self) {}
}

#[derive(Clone, Default)]
struct LockTracker {
    locked: HashSet<OutPoint>,
}

#[async_trait::async_trait]
impl CellCollector for LockTracker {
    async fn collect_live_cells_async(
        &mut self,
        _: &CellQueryOptions,
        _: bool,
    ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
        Ok((vec![], 0))
    }
    fn lock_cell(&mut self, out_point: OutPoint, _: u64) -> Result<(), CellCollectorError> {
        self.locked.insert(out_point);
        Ok(())
    }
    fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
        Ok(())
    }
    fn reset(&mut self) {}
}

fn test_udt_script() -> Script {
    Script::new_builder()
        .code_hash(Byte32::from([0x77; 32]))
        .args([0x01u8; 32].as_slice().pack())
        .build()
}

fn test_custom_payout_lock() -> Script {
    Script::new_builder()
        .code_hash(Byte32::from([0x99; 32]))
        .args([0xabu8; 20].as_slice().pack())
        .build()
}

fn udt_external_funding_fixture(with_preimage: bool, clear_tlcs: bool) -> Fixture {
    let udt_script = test_udt_script();
    let mut fixture =
        Fixture::with_features(false, false, with_preimage, false, ChannelFeatures::V2);
    let mut settlement_data = fixture.channel.local_settlement_data.clone();
    if clear_tlcs {
        settlement_data.tlcs.clear();
    }
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    fixture.store.insert_watch_channel(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        Some(udt_script.clone()),
        WatchChannelKeys {
            local_settlement: SettlementKey::external(fixture.base_key.pubkey()),
            remote_settlement: fixture.channel.remote_settlement_key,
            local_funding: fixture.channel.local_funding_pubkey,
            remote_funding: fixture.channel.remote_funding_pubkey,
        },
        settlement_data,
        ChannelFeatures::V2,
    );
    if with_preimage {
        let preimage = Hash256::from([13; 32]);
        fixture.store.insert_watch_preimage(
            fixture.node_id.clone(),
            HashAlgorithm::Sha256.hash(preimage.as_ref()).into(),
            preimage,
        );
    }
    fixture.channel = fixture
        .store
        .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
        .expect("channel");
    let witness_bytes = settlement_data_to_witness(
        &fixture.channel.local_settlement_data,
        fixture.for_remote,
        fixture.base_key.pubkey(),
        fixture.channel.remote_settlement_key,
    );
    let output: CellOutput = fixture.cell.output.clone().into();
    let mut args = output.lock().args().raw_data().to_vec();
    args[36..56].copy_from_slice(blake160(&witness_bytes).as_ref());
    let lock = output.lock().as_builder().args(args.pack()).build();
    let output = output
        .as_builder()
        .lock(lock)
        .type_(Some(udt_script).pack())
        .capacity(25_000_000_000u64) // < 180 + 142 CKB so partial balance also needs external funding
        .build();
    fixture.cell.output = output.into();
    fixture.cell.output_data = Some(ckb_jsonrpc_types::JsonBytes::from_vec(
        240_000_000_000u128.to_le_bytes().to_vec(),
    ));
    if clear_tlcs {
        fixture.tracked.clear();
    }
    fixture
}

fn fund_wallet_tx(
    template: &Transaction,
    outpoint: OutPoint,
    since: u64,
    change_lock: Script,
    witness: &[u8],
) -> Transaction {
    let mut outputs: Vec<_> = template.raw().outputs().into_iter().collect();
    let last_idx = outputs.len().saturating_sub(1);
    if let Some(last) = outputs.last_mut() {
        if last.lock() == Script::default() {
            let mut updated = last.clone().as_builder().lock(change_lock.clone()).build();
            let data_len = template
                .raw()
                .outputs_data()
                .get(last_idx)
                .map(|d| d.raw_data().len())
                .unwrap_or(0);
            let min_cap = updated
                .occupied_capacity(ckb_types::core::Capacity::bytes(data_len).unwrap())
                .unwrap()
                .as_u64();
            let cur_cap: u64 = updated.capacity().unpack();
            if cur_cap < min_cap {
                updated = updated.as_builder().capacity(min_cap).build();
            }
            *last = updated;
        }
    }
    outputs.push(
        CellOutput::new_builder()
            .capacity(10_000_000_000u64)
            .lock(change_lock)
            .build(),
    );
    template
        .clone()
        .into_view()
        .as_advanced_builder()
        .input(CellInput::new(outpoint, since))
        .set_outputs(outputs)
        .output_data(ckb_types::packed::Bytes::default())
        .witness(witness.pack())
        .build()
        .data()
}

fn sign_and_submit_funded(
    fixture: &Fixture,
    req_id: Hash256,
    key: &Privkey,
    key_purpose: OnchainKeyPurpose,
    funded_tx: &Transaction,
) -> [u8; 65] {
    let sig = sign_onchain_request(
        key,
        &OnchainSigningContent {
            key_purpose,
            transaction: funded_tx.clone(),
        },
    )
    .expect("sign funded tx");
    assert_eq!(
        fixture
            .submit_with_tx(req_id, sig, funded_tx.clone())
            .expect("submit funded tx"),
        SubmitWatchtowerSignatureResult::Applied
    );
    sig
}

#[test]
fn external_funding_ckb_allows_small_tlc_and_small_partial_balance_via_wallet_topup() {
    let tenant_payout_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0xaa; 20]);
    let min_payout_capacity = CellOutput::new_builder()
        .lock(tenant_payout_lock.clone())
        .build()
        .occupied_capacity(Capacity::shannons(0))
        .expect("occupied capacity")
        .as_u64();
    let small_amount = (min_payout_capacity - 1_000_000_000) as u128; // 51 CKB (< 61 CKB)
    let base_key = Privkey::from([42; 32]);
    let small_tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([6; 32]).pubkey());
    let small_preimage = Hash256::from([11; 32]);
    let small_hash: Hash256 = HashAlgorithm::Sha256.hash(small_preimage.as_ref()).into();

    // Case 0: < 61 CKB inbound preimage TLC; Case 1: < 61 CKB partial local balance.
    for is_balance in [false, true] {
        let (local_amount, tlcs, preimages, at_time, expected_purpose, signing_key, tag) =
            if is_balance {
                (
                    small_amount,
                    vec![],
                    vec![],
                    200_000,
                    OnchainKeyPurpose::Settlement,
                    &base_key,
                    0xc2u8,
                )
            } else {
                (
                    100_000_000_000,
                    vec![SettlementTlc {
                        tlc_id: TLCId::Offered(0),
                        hash_algorithm: HashAlgorithm::Sha256,
                        payment_amount: small_amount,
                        payment_hash: small_hash,
                        expiry: 150_000,
                        local_key: None,
                        local_key_pubkey: Some(small_tlc_key.pubkey()),
                        local_key_commitment_number: Some(6),
                        remote_key: Privkey::from([12; 32]).pubkey(),
                    }],
                    vec![(small_hash, small_preimage)],
                    100_000,
                    OnchainKeyPurpose::Tlc {
                        commitment_number: 6,
                    },
                    &small_tlc_key,
                    0xc1u8,
                )
            };
        let fixture = build_custom_ckb_fixture(
            false,
            false,
            local_amount,
            100_000_000_000,
            tlcs,
            preimages,
            false,
        );
        assert!(fixture
            .build_with_collector(at_time, &mut PanicCollector)
            .expect("small CKB claim must emit ExternalFundingRequired")
            .is_none());
        let (req_id, pending) = fixture.pending_request();
        assert_eq!(pending.content.key_purpose, expected_purpose);
        let tmpl = &pending.content.transaction;
        let outs: Vec<_> = tmpl.raw().outputs().into_iter().collect();
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[1].lock(), Script::default());
        assert_eq!(
            Unpack::<u64>::unpack(&outs[1].capacity()),
            small_amount as u64
        );
        let funded_tx = tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .input(CellInput::new(OutPoint::new(Byte32::from([tag; 32]), 0), 0))
            .set_outputs(vec![
                outs[0].clone(),
                outs[1]
                    .clone()
                    .as_builder()
                    .lock(tenant_payout_lock.clone())
                    .capacity(small_amount as u64 + 10_000_000_000 - 1_000)
                    .build(),
            ])
            .witness([tag].pack())
            .build()
            .data();
        sign_and_submit_funded(&fixture, req_id, signing_key, expected_purpose, &funded_tx);
        let signed = fixture.build_at(at_time).unwrap().expect("signed tx");
        assert_eq!(signed.data().raw().as_slice(), funded_tx.raw().as_slice());
    }
}

#[test]
fn external_udt_external_funding_template_tx_tlc_and_low_excess_balance_flow() {
    use ckb_types::{core::DepType, packed::CellDep};

    let custom_payout_lock = Script::new_builder()
        .code_hash(Byte32::from([0x99; 32]))
        .args([0xabu8; 32].as_slice().pack())
        .build();
    let custom_wallet_funding_lock = Script::new_builder()
        .code_hash(Byte32::from([0x88; 32]))
        .args([0xcdu8; 28].as_slice().pack())
        .build();
    let wallet_lock_dep = CellDep::new_builder()
        .out_point(OutPoint::new(Byte32::from([0xee; 32]), 0))
        .dep_type(DepType::Code)
        .build();
    let wallet_witness_1 = vec![0x11u8, 0x22, 0x33, 0x44, 0x55];
    let wallet_witness_2 = vec![0xaau8, 0xbb, 0xcc];

    // Cover all 3 external-funding UDT scenarios:
    //   0: UDT TLC timeout (`required_expiry_since = Some(...)`)
    //   1: UDT TLC preimage (`required_expiry_since = None`)
    //   2: low-excess partial UDT balance unlock (`required_expiry_since = None`)
    for scenario in 0..3 {
        let with_preimage = scenario == 1;
        let is_balance = scenario == 2;
        let fixture = udt_external_funding_fixture(with_preimage, is_balance);

        // 1. Watchtower emits unfunded ExternalFundingRequired template with ZERO cell_collector calls.
        assert!(fixture
            .build_with_collector(200_000, &mut PanicCollector)
            .expect("template build must succeed without cell_collector")
            .is_none());
        let (request_id, pending) = fixture.pending_request();
        let expected_expiry =
            (scenario == 0).then(|| fiber_types::watchtower::settlement_timestamp_since(100_000));
        assert_eq!(pending.required_expiry_since, expected_expiry);
        assert_eq!(pending.content.transaction.raw().inputs().len(), 1);
        assert_eq!(pending.content.transaction.raw().outputs().len(), 2);
        assert_eq!(
            pending
                .content
                .transaction
                .raw()
                .outputs()
                .get(1)
                .unwrap()
                .lock(),
            Script::default()
        );

        // RPC status exposes ExternalFundingRequired with required_expiry_since.
        let rpc = WatchtowerRpcServerImpl::new(fixture.store.clone())
            .with_signing_events(fixture.events.clone());
        let status = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rpc.get_watchtower_signing_status(
                RpcContext {
                    node_id: fixture.node_id.to_string(),
                    tenant_scoped: false,
                },
                crate::rpc::watchtower::GetWatchtowerSigningStatusParams {
                    channel_id: fixture.channel.channel_id.into(),
                },
            ))
            .expect("status");
        assert!(matches!(
            status.status,
            fiber_json_types::WatchtowerSigningStatus::ExternalFundingRequired {
                request_id: rpc_id,
                required_expiry_since,
                ..
            } if rpc_id == request_id.into() && required_expiry_since == expected_expiry
        ));

        // Re-scanning while pending reuses the same template without querying cell_collector.
        assert!(fixture
            .build_with_collector(210_000, &mut PanicCollector)
            .unwrap()
            .is_none());
        assert_eq!(fixture.pending_request().0, request_id);

        // 2. Wallet sets payout lock and builds the funded tx (custom lock cell_dep + funding input + change + 2 witnesses).
        let (signing_key, wrong_key) = if is_balance {
            (&fixture.base_key, &fixture.tlc_key)
        } else {
            (&fixture.tlc_key, &fixture.base_key)
        };
        let wallet_funding_outpoint = OutPoint::new(Byte32::from([0x71 + scenario as u8; 32]), 0);
        let tmpl_outs: Vec<_> = pending
            .content
            .transaction
            .raw()
            .outputs()
            .into_iter()
            .collect();
        let template_with_payout_lock = pending
            .content
            .transaction
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                tmpl_outs[0].clone(),
                tmpl_outs[1]
                    .clone()
                    .as_builder()
                    .lock(custom_payout_lock.clone())
                    .capacity(16_000_000_000u64)
                    .build(),
            ])
            .build()
            .data();
        let funded_tx = fund_wallet_tx(
            &template_with_payout_lock,
            wallet_funding_outpoint.clone(),
            expected_expiry.unwrap_or(0),
            custom_wallet_funding_lock.clone(),
            &wallet_witness_1,
        )
        .into_view()
        .as_advanced_builder()
        .cell_dep(wallet_lock_dep.clone())
        .witness(wallet_witness_2.clone().pack())
        .build()
        .data();
        let funded_content = OnchainSigningContent {
            key_purpose: pending.content.key_purpose,
            transaction: funded_tx.clone(),
        };
        let valid_sig = sign_onchain_request(signing_key, &funded_content).expect("sign funded");

        // 3. Negative checks: wrong key, tampered outputs[0], wrong since (for TLC unlocks).
        let wrong_sig = sign_onchain_request(wrong_key, &funded_content).unwrap();
        assert!(fixture
            .submit_with_tx(request_id, wrong_sig, funded_tx.clone())
            .is_err());

        let tampered_out0 = funded_tx
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                CellOutput::new_builder().capacity(1u64).build(),
                funded_tx.raw().outputs().get(1).unwrap(),
                funded_tx.raw().outputs().get(2).unwrap(),
            ])
            .build()
            .data();
        let tampered_out0_sig = sign_onchain_request(
            signing_key,
            &OnchainSigningContent {
                key_purpose: pending.content.key_purpose,
                transaction: tampered_out0.clone(),
            },
        )
        .unwrap();
        assert!(fixture
            .submit_with_tx(request_id, tampered_out0_sig, tampered_out0)
            .is_err());

        if !is_balance {
            let bad_since = if expected_expiry.is_some() {
                0
            } else {
                fiber_types::watchtower::settlement_timestamp_since(100_000)
            };
            let bad_since_tx = funded_tx
                .clone()
                .into_view()
                .as_advanced_builder()
                .set_inputs(vec![
                    funded_tx.raw().inputs().get(0).unwrap(),
                    CellInput::new(wallet_funding_outpoint, bad_since),
                ])
                .build()
                .data();
            let bad_since_sig = sign_onchain_request(
                signing_key,
                &OnchainSigningContent {
                    key_purpose: pending.content.key_purpose,
                    transaction: bad_since_tx.clone(),
                },
            )
            .unwrap();
            assert!(fixture
                .submit_with_tx(request_id, bad_since_sig, bad_since_tx)
                .is_err());
        } else {
            // Balance withdrawal with no pending TLCs allows a mature timestamp-locked funding cell.
            let timestamp_funded_tx = funded_tx
                .clone()
                .into_view()
                .as_advanced_builder()
                .set_inputs(vec![
                    funded_tx.raw().inputs().get(0).unwrap(),
                    CellInput::new(
                        wallet_funding_outpoint,
                        fiber_types::watchtower::settlement_timestamp_since(100_000),
                    ),
                ])
                .build()
                .data();
            assert!(
                fiber_types::validate_and_normalize_external_funding_watchtower_tx(
                    &pending.content.transaction,
                    &timestamp_funded_tx,
                    None,
                    None,
                )
                .is_ok()
            );
        }

        // 4. Valid submit + idempotency (zero placeholder & pre-filled witness[0]) + diff witness rejection.
        assert_eq!(
            fixture
                .submit_with_tx(request_id, valid_sig, funded_tx.clone())
                .unwrap(),
            SubmitWatchtowerSignatureResult::Applied
        );
        let filled_w0_tx = apply_external_settlement_signature(
            funded_tx.clone().into_view(),
            valid_sig,
            with_preimage,
        )
        .unwrap()
        .data();
        for retry_tx in [funded_tx.clone(), filled_w0_tx] {
            assert_eq!(
                fixture
                    .submit_with_tx(request_id, valid_sig, retry_tx)
                    .unwrap(),
                SubmitWatchtowerSignatureResult::AlreadyApplied
            );
        }
        let diff_witness_tx = funded_tx
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                funded_tx.witnesses().get(0).unwrap(),
                [0x99u8].pack(),
                wallet_witness_2.clone().pack(),
            ])
            .build()
            .data();
        let bad_w0_sig_tx = apply_external_settlement_signature(
            funded_tx.clone().into_view(),
            [0x55u8; 65],
            with_preimage,
        )
        .unwrap()
        .data();
        let empty_witnesses_tx = funded_tx
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![])
            .build()
            .data();
        let mut mutated_w0_body = funded_tx.witnesses().get(0).unwrap().raw_data().to_vec();
        // Flip a non-signature byte in the 72-byte balance section (keeps valid V2 witness layout
        // and all-zero 65-byte signature slot so only digest binding of witness[0] catches it).
        let htlc_count = mutated_w0_body[17] as usize;
        mutated_w0_body[18 + htlc_count * 97] ^= 1;
        let mutated_w0_body_tx = funded_tx
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                mutated_w0_body.pack(),
                funded_tx.witnesses().get(1).unwrap(),
                funded_tx.witnesses().get(2).unwrap(),
            ])
            .build()
            .data();
        for bad_retry in [
            diff_witness_tx.clone(),
            bad_w0_sig_tx.clone(),
            empty_witnesses_tx.clone(),
            mutated_w0_body_tx.clone(),
        ] {
            assert!(fixture
                .submit_with_tx(request_id, valid_sig, bad_retry)
                .is_err());
        }

        // 5. Final tx preserves client payout lock, cell_deps & witness[1..] without calling cell_collector.
        let final_tx = fixture
            .build_with_collector(220_000, &mut PanicCollector)
            .unwrap()
            .expect("signed external-funding tx");
        assert_applied_signature(&final_tx, &funded_content, signing_key);
        assert_eq!(
            final_tx.outputs().get(1).unwrap().lock(),
            custom_payout_lock
        );
        assert_eq!(final_tx.witnesses().len(), 3);
        assert_eq!(
            final_tx.witnesses().get(1).unwrap().raw_data().as_ref(),
            wallet_witness_1.as_slice()
        );
        assert_eq!(
            final_tx.witnesses().get(2).unwrap().raw_data().as_ref(),
            wallet_witness_2.as_slice()
        );
        assert_eq!(
            final_tx
                .cell_deps()
                .get(final_tx.cell_deps().len() - 1)
                .unwrap(),
            wallet_lock_dep
        );

        // 6. After confirmed spend pruning, last_applied still serves exact AlreadyApplied & rejects diffs/corrupted witness[0].
        let spent = HashSet::from([funded_tx.raw().inputs().get(0).unwrap().previous_output()]);
        prune_spent_signing_requests(
            &fixture.events,
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id,
            &spent,
        );
        assert_eq!(
            fixture
                .submit_with_tx(request_id, valid_sig, funded_tx)
                .unwrap(),
            SubmitWatchtowerSignatureResult::AlreadyApplied
        );
        for bad_retry in [
            diff_witness_tx,
            bad_w0_sig_tx,
            empty_witnesses_tx,
            mutated_w0_body_tx,
        ] {
            assert!(fixture
                .submit_with_tx(request_id, valid_sig, bad_retry)
                .is_err());
        }

        // 7. Oversized submission is rejected by bounds check even for an unknown request_id.
        let oversized_tx = TransactionBuilder::default()
            .witness(vec![0u8; fiber_types::MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES + 1].pack())
            .build()
            .data();
        let err = fixture
            .submit_with_tx(Hash256::from([0xfe; 32]), valid_sig, oversized_tx)
            .expect_err("oversized tx must fail bounds check before pending lookup");
        assert!(err.message().contains("exceeds maximum size"), "{err}");
    }
}

#[test]
fn local_udt_settlement_keeps_collecting_when_three_cells_raise_fee_above_estimate() {
    #[derive(Clone)]
    struct ThresholdCollector {
        available: Vec<LiveCell>,
        calls: usize,
    }

    #[async_trait::async_trait]
    impl CellCollector for ThresholdCollector {
        async fn collect_live_cells_async(
            &mut self,
            query: &CellQueryOptions,
            apply_changes: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            self.calls += 1;
            let mut selected = Vec::new();
            let mut total = 0u64;
            let mut remaining = Vec::new();
            for cell in std::mem::take(&mut self.available) {
                if total < query.min_total_capacity {
                    let cap: u64 = cell.output.capacity().unpack();
                    total += cap;
                    selected.push(cell);
                } else {
                    remaining.push(cell);
                }
            }
            if apply_changes {
                self.available = remaining;
            } else {
                selected.extend(remaining);
                self.available = selected.clone();
            }
            Ok((selected, total))
        }

        fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    let udt_script = test_udt_script();
    let mut fixture = Fixture::with_features(false, false, false, true, ChannelFeatures::V2);
    let settlement_data = fixture.channel.local_settlement_data.clone();
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    fixture.store.insert_watch_channel(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        Some(udt_script.clone()),
        WatchChannelKeys {
            local_settlement: SettlementKey::local(fixture.base_key.clone()),
            remote_settlement: fixture.channel.remote_settlement_key,
            local_funding: fixture.channel.local_funding_pubkey,
            remote_funding: fixture.channel.remote_funding_pubkey,
        },
        settlement_data,
        ChannelFeatures::V2,
    );
    fixture.channel = fixture
        .store
        .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
        .expect("channel");
    let output: CellOutput = fixture.cell.output.clone().into();
    fixture.cell.output = output
        .as_builder()
        .type_(Some(udt_script).pack())
        .capacity(25_000_000_000u64)
        .build()
        .into();
    fixture.cell.output_data = Some(ckb_jsonrpc_types::JsonBytes::from_vec(
        240_000_000_000u128.to_le_bytes().to_vec(),
    ));

    // Probe the exact initial shortfall requested by local UDT TLC settlement using a 1-round spy.
    #[derive(Clone, Default)]
    struct SpyCollector {
        first_min_total_capacity: u64,
        lock: Option<Script>,
    }
    #[async_trait::async_trait]
    impl CellCollector for SpyCollector {
        async fn collect_live_cells_async(
            &mut self,
            query: &CellQueryOptions,
            _: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            if self.first_min_total_capacity == 0 {
                self.first_min_total_capacity = query.min_total_capacity;
                self.lock = Some(query.primary_script.clone());
            }
            Ok((vec![], 0))
        }
        fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }

    let mut spy = SpyCollector::default();
    assert!(fixture.build_with_collector(200_000, &mut spy).is_err());
    let shortfall = spy.first_min_total_capacity;
    let lock = spy.lock.expect("fee provider lock");
    assert!(shortfall > 0);

    // Construct 3 cells that sum to `shortfall` EXACTLY (the 2-input fee estimate), so adding
    // the 3rd input raises the actual fee by 1 CellInput (~44B) above the estimate; a 4th cell
    // is available to cover the delta.
    let c1 = shortfall / 3;
    let c2 = shortfall / 3;
    let c3 = shortfall - c1 - c2;
    let c4 = 6_100_000_000u64;
    let make_cell = |seed: u8, cap: u64| LiveCell {
        output: CellOutput::new_builder()
            .capacity(cap)
            .lock(lock.clone())
            .build(),
        output_data: Default::default(),
        out_point: OutPoint::new(Byte32::from([seed; 32]), 0),
        block_number: 0,
        tx_index: 0,
    };

    let mut collector = ThresholdCollector {
        available: vec![
            make_cell(0xa1, c1),
            make_cell(0xa2, c2),
            make_cell(0xa3, c3),
            make_cell(0xa4, c4),
        ],
        calls: 0,
    };
    let tx = fixture
        .build_with_collector(200_000, &mut collector)
        .expect("builder must keep collecting the 4th cell after 3 cells raise actual fee")
        .expect("signed local UDT settlement tx");
    assert_eq!(collector.calls, 2);
    assert_eq!(tx.inputs().len(), 5); // 1 commitment input + 4 fee inputs

    // Repeating collector that ignores `apply_changes` must terminate cleanly (no infinite loop).
    #[derive(Clone)]
    struct RepeatingSmallCollector(LiveCell);
    #[async_trait::async_trait]
    impl CellCollector for RepeatingSmallCollector {
        async fn collect_live_cells_async(
            &mut self,
            _: &CellQueryOptions,
            _: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            let cap: u64 = self.0.output.capacity().unpack();
            Ok((vec![self.0.clone()], cap))
        }
        fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            Ok(())
        }
        fn reset(&mut self) {}
    }
    let mut repeating = RepeatingSmallCollector(make_cell(0xb1, c1));
    let err = fixture
        .build_with_collector(200_000, &mut repeating)
        .expect_err("repeating collector must terminate cleanly instead of looping forever");
    assert!(err.to_string().contains("Not enough capacity"), "{err}");
}

#[test]
fn external_funding_watchtower_request_never_touches_operator_held_cells() {
    let _serial = crate::ckb::held_cells::TEST_SERIAL.blocking_lock();
    let payout_lock = test_custom_payout_lock();
    let fixture = udt_external_funding_fixture(true, false);

    assert!(fixture.build_at(200_000).unwrap().is_none());
    let (request_id, pending) = fixture.pending_request();

    let now_ms = crate::now_timestamp_as_millis_u64();
    let operator_utxo = OutPoint::new(Byte32::from([0xd1; 32]), 0);
    let unheld_utxo = OutPoint::new(Byte32::from([0xd2; 32]), 0);
    crate::ckb::held_cells::release_for_watchtower([operator_utxo.clone(), unheld_utxo.clone()]);
    crate::ckb::held_cells::hold_for_watchtower([operator_utxo.clone()], now_ms);

    let funded_tx = fund_wallet_tx(
        &pending.content.transaction,
        operator_utxo.clone(),
        0,
        payout_lock,
        &[0x01],
    )
    .into_view()
    .as_advanced_builder()
    .input(CellInput::new(unheld_utxo.clone(), 0))
    .witness([0x02u8].pack())
    .build()
    .data();
    let sig = sign_and_submit_funded(
        &fixture,
        request_id,
        &fixture.tlc_key,
        pending.content.key_purpose,
        &funded_tx,
    );

    let is_held = |outpoint: &OutPoint, at_ms: u64| {
        let mut tracker = LockTracker::default();
        crate::ckb::held_cells::lock_held(&mut tracker, at_ms).unwrap();
        tracker.locked.contains(outpoint)
    };
    assert!(is_held(&operator_utxo, now_ms));
    assert!(!is_held(&unheld_utxo, now_ms));

    // Syncing holds before and after TTL expiry must neither lock unheld_utxo nor release operator_utxo.
    let channels = fixture.store.get_watch_channels_with_nodes();
    crate::watchtower::actor::tests::expire_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &channels,
        now_ms + 1_000,
    );
    assert!(is_held(&operator_utxo, now_ms + 1_000));
    assert!(!is_held(&unheld_utxo, now_ms + 1_000));

    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for req in ext.signed_signatures.values_mut() {
                    req.created_at_ms = 1;
                }
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    crate::watchtower::actor::tests::expire_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &channels,
        now_ms + 2_000,
    );
    assert!(is_held(&operator_utxo, now_ms + 2_000));

    // Removing the watch channel with a signed entry must not release operator_utxo.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.signed_signatures.insert(
                    request_id,
                    fiber_types::SignedWatchtowerRequest {
                        content: OnchainSigningContent {
                            key_purpose: pending.content.key_purpose,
                            transaction: TransactionBuilder::default()
                                .input(CellInput::new(OutPoint::new(Byte32::from([1; 32]), 0), 0))
                                .input(CellInput::new(operator_utxo.clone(), 0))
                                .build()
                                .data(),
                        },
                        signature: sig,
                        created_at_ms: now_ms,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    assert!(is_held(&operator_utxo, now_ms + 2_000));

    crate::ckb::held_cells::release_for_watchtower([operator_utxo, unheld_utxo]);
}

#[test]
fn external_funding_scan_race_with_submit_preserves_accepted_funded_tx() {
    let payout_lock = test_custom_payout_lock();
    let fixture = udt_external_funding_fixture(false, false);

    assert!(fixture.build_at(200_000).unwrap().is_none());
    let (request_id, pending) = fixture.pending_request();
    let required_expiry = pending.required_expiry_since.expect("timeout expiry");

    let wallet_witness_1 = [0xdeu8, 0xad, 0xbe, 0xef];
    let funded_tx = fund_wallet_tx(
        &pending.content.transaction,
        OutPoint::new(Byte32::from([0xe3; 32]), 0),
        required_expiry,
        payout_lock,
        &wallet_witness_1,
    );
    let funded_content = OnchainSigningContent {
        key_purpose: pending.content.key_purpose,
        transaction: funded_tx.clone(),
    };
    sign_and_submit_funded(
        &fixture,
        request_id,
        &fixture.tlc_key,
        pending.content.key_purpose,
        &funded_tx,
    );

    // Racing scan calls build_signed_settlement_tx with the unfunded template (with Some(id) and None).
    let fee_signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).unwrap());
    for existing_id in [Some(request_id), None] {
        let final_tx = build_signed_settlement_tx(
            &fixture.events,
            &fixture.store,
            &fixture.node_id,
            fixture.channel.channel_id,
            pending.content.transaction.clone().into_view(),
            pending.content.key_purpose,
            false,
            None,
            &fee_signer,
            existing_id,
            Some(required_expiry),
        )
        .unwrap()
        .expect("racing scan must apply accepted funded tx");
        assert_applied_signature(&final_tx, &funded_content, &fixture.tlc_key);
        assert_eq!(
            final_tx.witnesses().get(1).unwrap().raw_data().as_ref(),
            &wallet_witness_1
        );
        let WatchtowerSignerState::External(ext) = fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        else {
            panic!("external signer");
        };
        assert!(ext.pending_requests.is_empty());
        assert!(ext.signed_signatures.contains_key(&request_id));
        assert_eq!(
            ext.last_applied.as_ref().map(|a| a.request_id),
            Some(request_id)
        );
    }

    // Concurrent remove + recreate mid-scan must NEVER insert a 3-output funded tx into pending_requests.
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    fixture.store.insert_watch_channel(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        fixture.channel.funding_udt_type_script.clone(),
        WatchChannelKeys {
            local_settlement: SettlementKey::external(fixture.base_key.pubkey()),
            remote_settlement: fixture.channel.remote_settlement_key,
            local_funding: fixture.channel.local_funding_pubkey,
            remote_funding: fixture.channel.remote_funding_pubkey,
        },
        fixture.channel.local_settlement_data.clone(),
        ChannelFeatures::V2,
    );
    assert!(apply_existing_signed_settlement_tx(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        fixture.channel.channel_id,
        request_id,
        &funded_content,
        false,
    )
    .unwrap()
    .is_none());
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(
        ext.pending_requests.is_empty(),
        "applying a vanished signed_signatures entry must not seed pending_requests with a 3-output tx"
    );
}

#[test]
fn external_funding_eviction_clears_last_applied_while_confirmed_spend_preserves_it() {
    let payout_lock = test_custom_payout_lock();
    // Modes:
    //   0: TTL expiration (`expire_external_signing_requests`) -> clears last_applied
    //   1: unconfirmed dead-input pruning (`evict_dead_fee_signing_requests`) -> clears last_applied
    //   2: broadcast failure (`evict_failed_signed_settlement_tx`) -> clears last_applied
    //   3: confirmed spend (`prune_spent_signing_requests`) then dead-input check -> preserves last_applied
    for mode in 0..4 {
        let fixture = udt_external_funding_fixture(false, false);
        assert!(fixture.build_at(200_000).unwrap().is_none());
        let (request_id, pending) = fixture.pending_request();
        let required_expiry = pending.required_expiry_since.expect("timeout expiry");
        let funded_tx = fund_wallet_tx(
            &pending.content.transaction,
            OutPoint::new(Byte32::from([0xe4; 32]), 0),
            required_expiry,
            payout_lock.clone(),
            &[0x55],
        );
        let sig = sign_and_submit_funded(
            &fixture,
            request_id,
            &fixture.tlc_key,
            pending.content.key_purpose,
            &funded_tx,
        );

        match mode {
            0 => {
                let channels = fixture.store.get_watch_channels_with_nodes();
                let expired_at = crate::now_timestamp_as_millis_u64()
                    + crate::ckb::held_cells::WATCHTOWER_HOLD_MS
                    + 10_000;
                crate::watchtower::actor::tests::expire_external_signing_requests(
                    &fixture.events,
                    &fixture.store,
                    &channels,
                    expired_at,
                );
            }
            1 => evict_dead_fee_signing_requests(
                &fixture.events,
                &fixture.store,
                &fixture.node_id,
                &fixture.channel.channel_id,
                &HashSet::from([request_id]),
            ),
            2 => evict_failed_signed_settlement_tx(
                &fixture.events,
                &fixture.store,
                &fixture.node_id,
                &fixture.channel.channel_id,
                funded_tx.raw().as_slice(),
                crate::now_timestamp_as_millis_u64(),
            ),
            _ => {
                let spent = HashSet::from([OutPoint::from(fixture.cell.out_point.clone())]);
                prune_spent_signing_requests(
                    &fixture.events,
                    &fixture.store,
                    &fixture.node_id,
                    &fixture.channel.channel_id,
                    &spent,
                );
                evict_dead_fee_signing_requests(
                    &fixture.events,
                    &fixture.store,
                    &fixture.node_id,
                    &fixture.channel.channel_id,
                    &HashSet::from([request_id]),
                );
            }
        }

        let WatchtowerSignerState::External(ext) = fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        else {
            panic!("external signer");
        };
        assert!(ext.signed_signatures.is_empty());
        if mode == 3 {
            assert!(ext.last_applied.is_some());
            assert_eq!(
                fixture
                    .submit_with_tx(request_id, sig, funded_tx)
                    .expect("idempotent retry after confirmed spend"),
                SubmitWatchtowerSignatureResult::AlreadyApplied
            );
        } else {
            assert!(ext.last_applied.is_none(), "mode={mode}");
            assert!(fixture
                .submit_with_tx(request_id, sig, funded_tx)
                .expect_err("retry after eviction must fail")
                .message()
                .contains("not found in pending requests"));
        }
    }
}

#[test]
fn external_funding_two_parties_all_settled_single_output_ckb_and_udt() {
    let payout_lock = test_custom_payout_lock();
    let udt_script = test_udt_script();

    for is_udt in [false, true] {
        let mut fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
        fixture
            .store
            .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
        fixture.node_id = NodeId::from_bytes(b"hosted-tenant-two-parties".to_vec());
        let settlement = SettlementData {
            local_amount: 100_000_000_000,
            remote_amount: 100_000_000_000,
            tlcs: vec![],
        };
        fixture.store.insert_watch_channel(
            fixture.node_id.clone(),
            fixture.channel.channel_id,
            is_udt.then(|| udt_script.clone()),
            WatchChannelKeys {
                local_settlement: SettlementKey::external(fixture.base_key.pubkey()),
                remote_settlement: fixture.channel.remote_settlement_key,
                local_funding: fixture.channel.local_funding_pubkey,
                remote_funding: fixture.channel.remote_funding_pubkey,
            },
            settlement.clone(),
            ChannelFeatures::V2,
        );
        fixture.channel = fixture
            .store
            .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
            .expect("channel");

        // Remote already settled (`unlock_type = 0xFF`), leaving `two_parties_all_settled = true`.
        let witness_bytes = settlement_data_to_witness(
            &settlement,
            false,
            fixture.base_key.pubkey(),
            fixture.channel.remote_settlement_key,
        );
        let mut previous =
            SettlementWitness::build_from_witness(&[&[0], witness_bytes.as_slice()].concat())
                .unwrap();
        previous.unlocks.push(Unlock {
            unlock_type: 0xFF,
            with_preimage: false,
            signature: [0; 65],
            preimage: None,
        });
        let mut remaining = previous.clone();
        assert!(remaining.update());
        assert_eq!(remaining.settlement_local_pubkey_hash, [0u8; 20]);
        fixture.witness = Some(previous);
        fixture.tracked.clear();

        let output: CellOutput = fixture.cell.output.clone().into();
        let mut lock_args = output.lock().args().raw_data()[..36].to_vec();
        lock_args.extend_from_slice(blake160(&remaining.to_witness()).as_ref());
        lock_args.extend_from_slice(&[1, ChannelFeatures::V2.bits()]);
        let lock = output.lock().as_builder().args(lock_args.pack()).build();
        let output = output
            .as_builder()
            .lock(lock)
            .type_(is_udt.then(|| udt_script.clone()).pack())
            .capacity(100_000_000_000u64)
            .build();
        fixture.cell.output = output.into();
        fixture.cell.output_data = is_udt.then(|| {
            ckb_jsonrpc_types::JsonBytes::from_vec(100_000_000_000u128.to_le_bytes().to_vec())
        });

        assert!(fixture.build_at(200_000).unwrap().is_none());
        let (request_id, pending) = fixture.pending_request();
        assert_eq!(pending.required_expiry_since, None);
        let template = &pending.content.transaction;
        assert_eq!(template.raw().inputs().len(), 1);
        assert_eq!(template.raw().outputs().len(), 1);
        assert_eq!(
            template.raw().outputs().get(0).unwrap().lock(),
            Script::default()
        );

        // Set payout lock & pay fee directly from the single payout output (1 input, 1 output).
        let payout = template.raw().outputs().get(0).unwrap();
        let payout_cap: u64 = payout.capacity().unpack();
        let funded_tx = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![payout
                .as_builder()
                .lock(payout_lock.clone())
                .capacity(payout_cap - 1_000)
                .build()])
            .build()
            .data();
        sign_and_submit_funded(
            &fixture,
            request_id,
            &fixture.base_key,
            pending.content.key_purpose,
            &funded_tx,
        );

        let built = fixture.build_at(200_000).unwrap().expect("signed tx");
        assert_eq!(built.data().raw().as_slice(), funded_tx.raw().as_slice());
        assert_eq!(built.outputs().len(), 1);
        assert_eq!(built.outputs().get(0).unwrap().lock(), payout_lock);
        assert_eq!(
            Unpack::<u64>::unpack(&built.outputs().get(0).unwrap().capacity()),
            payout_cap - 1_000
        );
    }
}

#[test]
fn external_mixed_value_tlcs_prioritize_self_fundable_claim_over_small_tlc() {
    // Channel has eligible 51-CKB and 200-CKB preimage TLCs in that order. A standard-lock
    // wallet with no spare cell cannot self-fund the 51-CKB claim (< 61 CKB floor), so the
    // watchtower must prioritize the self-fundable 200-CKB claim instead of starving it.
    let base_key = Privkey::from([42; 32]);
    let tlc_key_small =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
    let tlc_key_large =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([8; 32]).pubkey());
    let preimage_small = Hash256::from([0x11; 32]);
    let preimage_large = Hash256::from([0x22; 32]);
    let hash_small: Hash256 = HashAlgorithm::Sha256.hash(preimage_small.as_ref()).into();
    let hash_large: Hash256 = HashAlgorithm::Sha256.hash(preimage_large.as_ref()).into();
    let tlcs = vec![
        SettlementTlc {
            tlc_id: TLCId::Received(1),
            payment_amount: 5_100_000_000, // 51 CKB (< 61 CKB standard secp256k1 cell floor)
            payment_hash: hash_small,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_small.pubkey()),
            local_key_commitment_number: Some(1),
            remote_key: Privkey::from([15; 32]).pubkey(),
        },
        SettlementTlc {
            tlc_id: TLCId::Received(2),
            payment_amount: 20_000_000_000, // 200 CKB (self-fundable with 0 spare cells)
            payment_hash: hash_large,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_large.pubkey()),
            local_key_commitment_number: Some(2),
            remote_key: Privkey::from([16; 32]).pubkey(),
        },
    ];
    let fixture = build_custom_ckb_fixture(
        true,
        false,
        10_000_000_000,
        10_000_000_000,
        tlcs,
        vec![(hash_small, preimage_small), (hash_large, preimage_large)],
        false,
    );
    assert!(fixture.build_at(50_000).expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    assert_eq!(
        pending.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 2
        },
        "self-fundable 200-CKB TLC must be selected ahead of 51-CKB TLC"
    );
    let template = &pending.content.transaction;
    let payout = template.raw().outputs().get(1).unwrap();
    let payout_cap: u64 = payout.capacity().unpack();
    assert_eq!(payout_cap, 20_000_000_000);

    // Wallet with standard lock and no spare cell funds the 200-CKB payout directly.
    let wallet_lock = Script::new_builder()
        .code_hash(Byte32::from([0x71; 32]))
        .args([0x33u8; 20].pack())
        .build();
    let funded_tx = template
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            template.raw().outputs().get(0).unwrap(),
            payout
                .as_builder()
                .lock(wallet_lock)
                .capacity(payout_cap - 1_000)
                .build(),
        ])
        .build()
        .data();
    sign_and_submit_funded(
        &fixture,
        request_id,
        &tlc_key_large,
        pending.content.key_purpose,
        &funded_tx,
    );
    let built = fixture
        .build_at(50_000)
        .expect("apply")
        .expect("signed 200-CKB tx");
    assert_eq!(built.data().raw().as_slice(), funded_tx.raw().as_slice());

    // Also verify a self-fundable 200-CKB preimage TLC beats an eligible 500-CKB timeout TLC
    // (because timeout TLCs require a spare wallet cell carrying an absolute timestamp `since`).
    let tlcs_timeout_first = vec![
        SettlementTlc {
            tlc_id: TLCId::Offered(1),
            payment_amount: 50_000_000_000, // 500 CKB timeout (requires spare wallet cell)
            payment_hash: hash_small,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_small.pubkey()),
            local_key_commitment_number: Some(1),
            remote_key: Privkey::from([15; 32]).pubkey(),
        },
        SettlementTlc {
            tlc_id: TLCId::Received(2),
            payment_amount: 20_000_000_000, // 200 CKB preimage (self-fundable)
            payment_hash: hash_large,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 200_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_large.pubkey()),
            local_key_commitment_number: Some(2),
            remote_key: Privkey::from([16; 32]).pubkey(),
        },
    ];
    let fixture2 = build_custom_ckb_fixture(
        true,
        false,
        10_000_000_000,
        10_000_000_000,
        tlcs_timeout_first,
        vec![(hash_large, preimage_large)],
        false,
    );
    assert!(fixture2.build_at(150_000).expect("queue").is_none());
    let (_, pending2) = fixture2.pending_request();
    assert_eq!(
        pending2.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 2
        },
        "self-fundable preimage TLC must be selected ahead of timeout TLC requiring a spare cell"
    );
    assert_eq!(pending2.required_expiry_since, None);
}

#[test]
fn external_replacement_request_gets_fresh_id_so_stale_retry_cannot_consume_it() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue initial").is_none());
    let (id1, pending1) = fixture.pending_request();
    // Re-running while active preserves the same request ID.
    assert!(fixture.build().expect("reuse active").is_none());
    let (id1_again, _) = fixture.pending_request();
    assert_eq!(id1_again, id1);

    // Wallet funds and submits id1 with wallet cell A.
    let funded1 = fund_pending(&pending1);
    let sig1 = sign_onchain_request(&fixture.tlc_key, &funded1).expect("sig1");
    assert_eq!(
        fixture
            .submit_with_tx(id1, sig1, funded1.transaction.clone())
            .expect("submit id1"),
        SubmitWatchtowerSignatureResult::Applied
    );

    // Wallet cell A is spent elsewhere -> evict id1 and clear last_applied.
    evict_dead_fee_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &HashSet::from([id1]),
    );

    // Next scan for the SAME live commitment cell must issue a replacement with a FRESH request_id.
    assert!(fixture.build().expect("queue replacement").is_none());
    let (id2, pending2) = fixture.pending_request();
    assert_ne!(
        id2, id1,
        "replacement request after eviction must have a fresh request_id"
    );
    assert_eq!(pending2.content, pending1.content);
    // Re-running while replacement is active keeps id2 stable.
    assert!(fixture.build().expect("reuse replacement").is_none());
    assert_eq!(fixture.pending_request().0, id2);

    // A delayed retry for id1 (carrying dead wallet cell A) must be rejected and leave id2 intact.
    assert!(fixture
        .submit_with_tx(id1, sig1, funded1.transaction)
        .is_err());
    assert_eq!(fixture.pending_request().0, id2);

    // Fresh funding submitted against id2 succeeds.
    let funded2 = fund_pending(&pending2);
    let sig2 = sign_onchain_request(&fixture.tlc_key, &funded2).expect("sig2");
    assert_eq!(
        fixture
            .submit_with_tx(id2, sig2, funded2.transaction)
            .expect("submit id2"),
        SubmitWatchtowerSignatureResult::Applied
    );
}

#[test]
fn external_dead_fee_classifier_distinguishes_live_spent_and_unknown_inputs() {
    let commitment_outpoint = OutPoint::new(Byte32::from([0x11; 32]), 0);
    let fee_outpoint = OutPoint::new(Byte32::from([0x22; 32]), 0);
    let req_id = Hash256::from([0x33; 32]);
    let funded_tx = TransactionBuilder::default()
        .input(CellInput::new(commitment_outpoint.clone(), 0))
        .input(CellInput::new(fee_outpoint.clone(), 0))
        .build()
        .data();
    let mut external = fiber_types::WatchtowerExternalSignerState::default();
    external.signed_signatures.insert(
        req_id,
        fiber_types::SignedWatchtowerRequest {
            content: OnchainSigningContent {
                key_purpose: OnchainKeyPurpose::Settlement,
                transaction: funded_tx,
            },
            signature: [0x55; 65],
            created_at_ms: 1_000,
        },
    );

    let fee_tx_hash: ckb_types::H256 = fee_outpoint.tx_hash().unpack();
    let commitment_tx_hash: ckb_types::H256 = commitment_outpoint.tx_hash().unpack();

    // 1. Live fee input -> neither confirmed_spent nor dead_fee_only.
    let (confirmed, dead) = classify_dead_fee_signing_requests(
        &external,
        |_| Ok("live".to_string()),
        |_| Ok(Some(Status::Committed)),
    );
    assert!(confirmed.is_empty() && dead.is_empty());

    // 2. Temporarily unknown fee input (wallet 1 block ahead: get_live_cell="unknown", parent tx not committed)
    //    -> must NOT evict.
    for parent_status in [None, Some(Status::Pending), Some(Status::Proposed)] {
        let (confirmed, dead) = classify_dead_fee_signing_requests(
            &external,
            |op| {
                if *op == fee_outpoint {
                    Ok("unknown".to_string())
                } else {
                    Ok("live".to_string())
                }
            },
            |hash| {
                if *hash == fee_tx_hash {
                    Ok(parent_status.clone())
                } else {
                    Ok(Some(Status::Committed))
                }
            },
        );
        assert!(
            confirmed.is_empty() && dead.is_empty(),
            "temporarily unknown fee cell (parent={parent_status:?}) must not be evicted"
        );
    }

    // 3. Confirmed-spent fee input (get_live_cell="unknown" AND parent tx Committed) while commitment
    //    is live and signed tx is NOT committed -> classified as dead_fee_only (evict + clear last_applied).
    let (confirmed, dead) = classify_dead_fee_signing_requests(
        &external,
        |op| {
            if *op == fee_outpoint {
                Ok("unknown".to_string())
            } else {
                Ok("live".to_string())
            }
        },
        |hash| {
            if *hash == fee_tx_hash || *hash == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    assert!(confirmed.is_empty());
    assert_eq!(dead, HashSet::from([req_id]));

    // 4. Both fee input AND commitment input confirmed-spent -> classified as confirmed_spent_inputs
    //    (preserve last_applied).
    let (confirmed, dead) = classify_dead_fee_signing_requests(
        &external,
        |_| Ok("unknown".to_string()),
        |hash| {
            if *hash == fee_tx_hash || *hash == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    assert_eq!(confirmed, HashSet::from([commitment_outpoint.clone()]));
    assert!(dead.is_empty());

    // 5. TOCTOU race: commitment_outpoint returns "live" on the first lookup, then a block
    //    confirms the settlement before fee_input is checked (fee_input is ConfirmedSpent and
    //    signed_tx_hash is Committed) -> must classify as confirmed_spent_inputs (NOT dead_fee_only)
    //    so last_applied is preserved.
    let signed_tx_hash: ckb_types::H256 = external.signed_signatures[&req_id]
        .content
        .transaction
        .calc_tx_hash()
        .unpack();
    let mut commitment_checks = 0usize;
    let (confirmed, dead) = classify_dead_fee_signing_requests(
        &external,
        |op| {
            if *op == commitment_outpoint {
                commitment_checks += 1;
                if commitment_checks == 1 {
                    Ok("live".to_string())
                } else {
                    Ok("unknown".to_string())
                }
            } else {
                Ok("unknown".to_string())
            }
        },
        |hash| {
            if *hash == fee_tx_hash || *hash == signed_tx_hash || *hash == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    assert_eq!(confirmed, HashSet::from([commitment_outpoint.clone()]));
    assert!(dead.is_empty());

    // 5b. Non-signed TOCTOU race: commitment_outpoint transitions from "live" on first lookup to
    //     ConfirmedSpent on recheck after fee_input is ConfirmedSpent -> still confirmed_spent_inputs.
    let mut pending_only = fiber_types::WatchtowerExternalSignerState::default();
    pending_only.pending_requests.insert(
        req_id,
        fiber_types::PendingWatchtowerRequest {
            content: external.signed_signatures[&req_id].content.clone(),
            created_at_ms: 1_000,
            required_expiry_since: None,
        },
    );
    let mut pending_commitment_checks = 0usize;
    let (confirmed, dead) = classify_dead_fee_signing_requests(
        &pending_only,
        |op| {
            if *op == commitment_outpoint {
                pending_commitment_checks += 1;
                if pending_commitment_checks == 1 {
                    Ok("live".to_string())
                } else {
                    Ok("unknown".to_string())
                }
            } else {
                Ok("unknown".to_string())
            }
        },
        |hash| {
            if *hash == fee_tx_hash || *hash == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    assert_eq!(pending_commitment_checks, 2);
    assert_eq!(confirmed, HashSet::from([commitment_outpoint]));
    assert!(dead.is_empty());
}

#[test]
fn external_offline_restart_past_ttl_reconciles_confirmation_before_expiring() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let _built = fixture.build().expect("apply").expect("signed tx");

    let commitment_outpoint = pending
        .content
        .transaction
        .raw()
        .inputs()
        .get(0)
        .unwrap()
        .previous_output();
    let created_at_ms = match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        WatchtowerSignerState::External(ext) => ext.signed_signatures[&request_id].created_at_ms,
        other => panic!("expected external signer, got {other:?}"),
    };
    let after_ttl_ms = created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 60_000;

    // Simulate restart past TTL after the partial settlement confirmed on-chain:
    // confirmation reconciliation runs before TTL expiration.
    prune_spent_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &HashSet::from([commitment_outpoint]),
    );
    expire_channel_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        after_ttl_ms,
    );

    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("expected external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id),
        "confirmed settlement receipt must survive restart past TTL"
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
    // Identical client retry still gets AlreadyApplied instead of request_id not found.
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction)
            .expect("idempotent retry after restart"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );
}

#[test]
fn external_broadcast_transport_error_retains_receipt_while_rpc_rejection_evicts() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let built = fixture.build().expect("apply").expect("signed tx");
    let now_ms = crate::now_timestamp_as_millis_u64();

    // 1. Transport/timeout error where node actually accepted tx -> retain signed_signatures + last_applied.
    let timeout_err = ckb_sdk::RpcError::Other(anyhow!("request timed out"));
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &timeout_err,
        now_ms,
        |_| Ok("live".to_string()),
        |_| Ok(Some(Status::Pending)),
    );
    // 2. Transport/timeout error where tx lookup also returns None/unknown -> still retain for next scan.
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &timeout_err,
        now_ms,
        |_| Ok("live".to_string()),
        |_| Ok(None),
    );
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        WatchtowerSignerState::External(ext) => {
            assert!(ext.signed_signatures.contains_key(&request_id));
            assert_eq!(
                ext.last_applied.as_ref().map(|a| a.request_id),
                Some(request_id)
            );
            assert_eq!(ext.consecutive_expired_requests, 0);
        }
        other => panic!("expected external signer, got {other:?}"),
    }

    // 2b. Retryable JSON-RPC errors (PoolRejectedTransactionByMaxAncestorsCountLimit -1105,
    //     PoolIsFull -1106, InternalError -32603, and -302 temporary tip-lag Immature in either
    //     message or data) must ALSO retain signed_signatures + last_applied without arming
    //     backoff so an offline wallet does not need to re-sign.
    let retryable_errors = vec![
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-1105),
            message: "PoolRejectedTransactionByMaxAncestorsCountLimit".to_string(),
            data: None,
        },
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-1106),
            message: "PoolIsFull".to_string(),
            data: None,
        },
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-32603),
            message: "InternalError".to_string(),
            data: None,
        },
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-302),
            message: "TransactionFailedToVerify: Verification failed Transaction(Immature(0))"
                .to_string(),
            data: None,
        },
        jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-302),
            message: "TransactionFailedToVerify".to_string(),
            data: Some(jsonrpc_core::Value::String(
                "Transaction(Immature(1))".to_string(),
            )),
        },
    ];
    for rpc_err in retryable_errors {
        let retryable_err = ckb_sdk::RpcError::Rpc(rpc_err);
        handle_external_settlement_send_error(
            &fixture.events,
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id,
            &built,
            &retryable_err,
            now_ms,
            |_| Ok("live".to_string()),
            |_| Ok(None),
        );
    }
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        WatchtowerSignerState::External(ext) => {
            assert!(ext.signed_signatures.contains_key(&request_id));
            assert_eq!(
                ext.last_applied.as_ref().map(|a| a.request_id),
                Some(request_id)
            );
            assert_eq!(ext.consecutive_expired_requests, 0);
            assert_eq!(ext.retry_after_ms, 0);
        }
        other => panic!("expected external signer, got {other:?}"),
    }

    // 3. Definitive CKB RPC rejection (non-immature -302 TransactionFailedToVerify) -> evict signed_signatures, clear last_applied, arm backoff.
    let rpc_reject_err = ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-302),
        message: "TransactionFailedToVerify: ValidationFailure(-1)".to_string(),
        data: None,
    });
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &rpc_reject_err,
        now_ms,
        |_| Ok("live".to_string()),
        |_| Ok(None),
    );
    match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        WatchtowerSignerState::External(ext) => {
            assert!(!ext.signed_signatures.contains_key(&request_id));
            assert!(ext.last_applied.is_none());
            assert_eq!(ext.consecutive_expired_requests, 1);
            assert!(ext.retry_after_ms > now_ms);
        }
        other => panic!("expected external signer, got {other:?}"),
    }
}

#[test]
fn external_broadcasts_already_signed_settlement_even_if_higher_priority_claim_arrives() {
    let base_key = Privkey::from([42; 32]);
    let tlc_key_small =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
    let tlc_key_large =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([8; 32]).pubkey());
    let preimage_small = Hash256::from([0x11; 32]);
    let preimage_large = Hash256::from([0x22; 32]);
    let hash_small: Hash256 = HashAlgorithm::Sha256.hash(preimage_small.as_ref()).into();
    let hash_large: Hash256 = HashAlgorithm::Sha256.hash(preimage_large.as_ref()).into();
    let tlcs = vec![
        SettlementTlc {
            tlc_id: TLCId::Received(1),
            payment_amount: 10_000_000_000, // 100 CKB (eligible first)
            payment_hash: hash_small,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_small.pubkey()),
            local_key_commitment_number: Some(1),
            remote_key: Privkey::from([15; 32]).pubkey(),
        },
        SettlementTlc {
            tlc_id: TLCId::Received(2),
            payment_amount: 20_000_000_000, // 200 CKB (becomes eligible after wallet signs #1)
            payment_hash: hash_large,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key_large.pubkey()),
            local_key_commitment_number: Some(2),
            remote_key: Privkey::from([16; 32]).pubkey(),
        },
    ];
    let fixture = build_custom_ckb_fixture(
        true,
        false,
        10_000_000_000,
        10_000_000_000,
        tlcs,
        vec![(hash_small, preimage_small)],
        false,
    );
    assert!(fixture.build_at(50_000).expect("queue small").is_none());
    let (request_id, pending) = fixture.pending_request();
    assert_eq!(
        pending.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 1
        }
    );
    let template = &pending.content.transaction;
    let payout = template.raw().outputs().get(1).unwrap();
    let payout_cap: u64 = payout.capacity().unpack();
    let funded_tx = template
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            template.raw().outputs().get(0).unwrap(),
            payout
                .as_builder()
                .lock(test_custom_payout_lock())
                .capacity(payout_cap - 1_000)
                .build(),
        ])
        .build()
        .data();
    sign_and_submit_funded(
        &fixture,
        request_id,
        &tlc_key_small,
        pending.content.key_purpose,
        &funded_tx,
    );

    // Higher-priority 200-CKB preimage arrives while wallet is offline before broadcast.
    fixture
        .store
        .insert_watch_preimage(fixture.node_id.clone(), hash_large, preimage_large);

    // Watchtower must broadcast the already-signed 100-CKB settlement rather than stalling
    // waiting for a brand-new signature on the 200-CKB claim.
    let built = fixture
        .build_at(50_000)
        .expect("broadcast existing signed tx")
        .expect("already-signed settlement tx must be returned");
    assert_eq!(built.data().raw().as_slice(), funded_tx.raw().as_slice());
}

#[test]
fn external_single_input_confirmed_settlement_survives_restart_past_ttl_without_scan_hit() {
    let fixture = Fixture::new(true, true, true, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    assert_eq!(pending.required_expiry_since, None);

    // Self-fund from the payout output with only 1 input (no extra wallet fee input).
    let template = &pending.content.transaction;
    let payout = template.raw().outputs().get(1).unwrap();
    let payout_cap: u64 = payout.capacity().unpack();
    let single_input_tx = template
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_outputs(vec![
            template.raw().outputs().get(0).unwrap(),
            payout
                .as_builder()
                .lock(test_custom_payout_lock())
                .capacity(payout_cap - 1_000)
                .build(),
        ])
        .build()
        .data();
    assert_eq!(single_input_tx.raw().inputs().len(), 1);
    let funded_content = OnchainSigningContent {
        key_purpose: pending.content.key_purpose,
        transaction: single_input_tx.clone(),
    };
    let sig = sign_onchain_request(&fixture.tlc_key, &funded_content).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, single_input_tx.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let signed_tx_hash: ckb_types::H256 = single_input_tx.calc_tx_hash().unpack();
    let commitment_outpoint = OutPoint::from(fixture.cell.out_point.clone());

    // Simulate tower restart past the 30-minute TTL where the indexer scan returned empty
    // `spent_inputs`, but the CKB node reports the 1-input signed tx as `Committed`.
    let WatchtowerSignerState::External(ext_before) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("expected external signer");
    };
    let (confirmed_spent_inputs, dead_fee_only) = classify_dead_fee_signing_requests(
        &ext_before,
        |_| Ok("unknown".to_string()),
        |tx_hash| {
            if *tx_hash == signed_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    assert!(confirmed_spent_inputs.contains(&commitment_outpoint));
    assert!(dead_fee_only.is_empty());

    prune_spent_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &confirmed_spent_inputs,
    );
    let after_ttl_ms =
        crate::now_timestamp_as_millis_u64() + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 5_000;
    expire_channel_external_signing_requests(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        after_ttl_ms,
    );

    let WatchtowerSignerState::External(ext_after) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("expected external signer");
    };
    assert!(ext_after.signed_signatures.is_empty());
    assert_eq!(
        ext_after.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext_after.consecutive_expired_requests, 0);
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, single_input_tx)
            .expect("retry after restart"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );
}

#[test]
fn external_removed_request_id_is_never_resurrected_after_channel_reregistration() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (retired_id, pending) = fixture.pending_request();

    // Simulate concurrent remove_watch_channel + create_watch_channel while a scan is holding
    // `retired_id` from an earlier unlocked read.
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    fixture.store.insert_watch_channel(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        fixture.channel.funding_udt_type_script.clone(),
        WatchChannelKeys {
            local_settlement: SettlementKey::external(fixture.base_key.pubkey()),
            remote_settlement: fixture.channel.remote_settlement_key,
            local_funding: fixture.channel.local_funding_pubkey,
            remote_funding: fixture.channel.remote_funding_pubkey,
        },
        fixture.channel.local_settlement_data.clone(),
        fixture.channel.channel_features,
    );

    let signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
    let res = build_signed_settlement_tx(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        fixture.channel.channel_id,
        pending.content.transaction.clone().into_view(),
        pending.content.key_purpose,
        false,
        None,
        &signer,
        Some(retired_id),
        pending.required_expiry_since,
    )
    .expect("build_signed_settlement_tx");
    assert!(res.is_none());

    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("expected external signer");
    };
    assert!(
        !ext.pending_requests.contains_key(&retired_id),
        "retired request_id must never be resurrected into a re-registered channel"
    );
    assert!(ext.pending_requests.is_empty());
}

#[test]
fn external_revocation_path_expires_obsolete_unsigned_requests_without_backoff() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, _) = fixture.pending_request();

    let unrelated_outpoint = OutPoint::new(Byte32::from([0xee; 32]), 0);
    let after_ttl_ms =
        crate::now_timestamp_as_millis_u64() + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 5_000;
    retire_external_signing_requests_on_revocation_with_lookup(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &unrelated_outpoint,
        after_ttl_ms,
        |_| Ok(None),
    );

    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("expected external signer");
    };
    assert!(!ext.pending_requests.contains_key(&request_id));
    assert!(ext.pending_requests.is_empty());
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
}

#[test]
fn external_mature_ckb_balance_reachable_despite_expired_timeout_tlc() {
    for for_remote in [true, false] {
        let base_key = Privkey::from([42; 32]);
        let tlc_key =
            fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
        let payment_hash: Hash256 = HashAlgorithm::Sha256.hash([0x33u8; 32]).into();
        let tlcs = vec![SettlementTlc {
            // Timeout TLC belongs to local party: Offered when for_remote, Received when !for_remote.
            tlc_id: if for_remote {
                TLCId::Offered(1)
            } else {
                TLCId::Received(1)
            },
            payment_amount: 5_000_000_000, // 50 CKB timeout TLC (requires spare timestamped wallet input)
            payment_hash,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 100_000,
            local_key: None,
            local_key_pubkey: Some(tlc_key.pubkey()),
            local_key_commitment_number: Some(1),
            remote_key: Privkey::from([15; 32]).pubkey(),
        }];
        let fixture = build_custom_ckb_fixture(
            for_remote,
            false,
            20_000_000_000, // 200 CKB mature balance (self-fundable with 0 spare cells)
            10_000_000_000,
            tlcs,
            vec![],
            false,
        );
        let settlement_data = if for_remote {
            &fixture.channel.remote_settlement_data
        } else {
            &fixture.channel.local_settlement_data
        };
        let signer =
            LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));

        // At epoch 12 (2/3 of delay_epoch=3 after cell_header_epoch=10), only the expired timeout
        // TLC is eligible; balance delay (epoch 13) has not matured yet.
        assert!(build_settlement_tx(
            &fixture.events,
            fixture.cell.clone(),
            EpochNumberWithFraction::new(10, 0, 1),
            EpochNumberWithFraction::new(12, 0, 1),
            150_000,
            &fixture.node_id,
            fixture.for_remote,
            fixture.channel.clone(),
            settlement_data,
            fixture.witness.clone(),
            &fixture.tracked,
            &signer,
            &mut FeeCells,
            &fixture.store,
        )
        .expect("queue timeout")
        .is_none());
        let (timeout_id, timeout_req) = fixture.pending_request();
        assert!(timeout_req.required_expiry_since.is_some());
        assert_eq!(
            timeout_req.content.key_purpose,
            OnchainKeyPurpose::Tlc {
                commitment_number: 1
            }
        );

        // At epoch 14 (full balance delay matured, all TLCs expired), the watchtower must offer the
        // self-fundable 200-CKB balance as a separate pending request, AND `first_pending()` must
        // expose it ahead of the stuck timeout TLC regardless of random request_id ordering.
        assert!(fixture.build_at(150_000).expect("queue balance").is_none());
        let (exposed_id, exposed_req) = fixture.pending_request();
        assert_ne!(exposed_id, timeout_id);
        assert_eq!(
            exposed_req.content.key_purpose,
            OnchainKeyPurpose::Settlement
        );
        assert_eq!(exposed_req.required_expiry_since, None);

        // Self-fund and sign the balance withdrawal with 0 extra wallet cells (even though the
        // expired timeout TLC remains in `pending_htlcs` of witness[0]).
        let template = &exposed_req.content.transaction;
        let payout = template.raw().outputs().get(1).unwrap();
        let payout_cap: u64 = payout.capacity().unpack();
        assert_eq!(payout_cap, 20_000_000_000);
        let funded_tx = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                template.raw().outputs().get(0).unwrap(),
                payout
                    .as_builder()
                    .lock(test_custom_payout_lock())
                    .capacity(payout_cap - 1_000)
                    .build(),
            ])
            .build()
            .data();
        sign_and_submit_funded(
            &fixture,
            exposed_id,
            &base_key,
            OnchainKeyPurpose::Settlement,
            &funded_tx,
        );
        let built = fixture
            .build_at(150_000)
            .expect("broadcast balance")
            .expect("signed balance tx");
        assert_eq!(built.data().raw().as_slice(), funded_tx.raw().as_slice());

        // Simulate confirming the 200-CKB balance tx on-chain: prune spent requests for the
        // initial commitment cell and construct the successor commitment cell + SettlementWitness
        // carrying unlock_type 0xFF (for_remote) or 0xFE (!for_remote), which zeroes our local
        // settlement pubkey hash while leaving the 50-CKB expired timeout TLC in `pending_htlcs`.
        prune_spent_signing_requests(
            &fixture.events,
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id,
            &HashSet::from([OutPoint::from(fixture.cell.out_point.clone())]),
        );
        let successor_witness_raw = built.witnesses().get(0).unwrap().raw_data();
        let successor_witness = SettlementWitness::build_from_witness(&successor_witness_raw[16..])
            .expect("decode balance unlock witness");
        let mut check_sw = successor_witness.clone();
        assert!(check_sw.update());
        if for_remote {
            assert_eq!(check_sw.settlement_local_pubkey_hash, [0u8; 20]);
        } else {
            assert_eq!(check_sw.settlement_remote_pubkey_hash, [0u8; 20]);
        }
        assert_eq!(check_sw.pending_htlcs.len(), 1);

        let successor_cell = Cell {
            output: built.outputs().get(0).unwrap().into(),
            output_data: Some(ckb_jsonrpc_types::JsonBytes::default()),
            out_point: OutPoint::new(built.hash(), 0).into(),
            block_number: 1u64.into(),
            tx_index: 0u32.into(),
        };

        // The successor scan must NOT return Ok(None) just because our balance slot is zeroed:
        // it must offer and settle the remaining 50-CKB timeout TLC!
        assert!(build_settlement_tx(
            &fixture.events,
            successor_cell.clone(),
            EpochNumberWithFraction::new(14, 0, 1),
            EpochNumberWithFraction::new(14, 0, 1),
            150_000,
            &fixture.node_id,
            fixture.for_remote,
            fixture.channel.clone(),
            settlement_data,
            Some(successor_witness.clone()),
            &fixture.tracked,
            &signer,
            &mut FeeCells,
            &fixture.store,
        )
        .expect("queue remaining timeout TLC on successor")
        .is_none());
        let (tlc_req_id, tlc_req) = fixture.pending_request();
        assert_eq!(
            tlc_req.content.key_purpose,
            OnchainKeyPurpose::Tlc {
                commitment_number: 1
            }
        );
        assert!(tlc_req.required_expiry_since.is_some());
        let tlc_payout_cap: u64 = tlc_req
            .content
            .transaction
            .raw()
            .outputs()
            .get(1)
            .unwrap()
            .capacity()
            .unpack();
        assert_eq!(tlc_payout_cap, 5_000_000_000);

        // Wallet funds the remaining 50-CKB timeout TLC with a spare cell (e.g. from its freshly
        // withdrawn 200-CKB balance), signs, and watchtower broadcasts it.
        let funded_tlc = fund_pending(&tlc_req);
        sign_and_submit_funded(
            &fixture,
            tlc_req_id,
            &tlc_key,
            tlc_req.content.key_purpose,
            &funded_tlc.transaction,
        );
        let settled_tlc_tx = build_settlement_tx(
            &fixture.events,
            successor_cell,
            EpochNumberWithFraction::new(14, 0, 1),
            EpochNumberWithFraction::new(14, 0, 1),
            150_000,
            &fixture.node_id,
            fixture.for_remote,
            fixture.channel.clone(),
            settlement_data,
            Some(successor_witness),
            &fixture.tracked,
            &signer,
            &mut FeeCells,
            &fixture.store,
        )
        .expect("broadcast remaining timeout TLC")
        .expect("signed timeout TLC tx");
        assert_eq!(
            settled_tlc_tx.data().raw().as_slice(),
            funded_tlc.transaction.raw().as_slice()
        );
    }
}

#[test]
fn external_broadcasts_already_signed_superseded_snapshot_tlc_even_if_higher_priority_claim_arrives(
) {
    let base_key = Privkey::from([42; 32]);
    let tlc_key_small =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([11; 32]).pubkey());
    let tlc_key_large =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([12; 32]).pubkey());
    let preimage_small = Hash256::from([0x11; 32]);
    let preimage_large = Hash256::from([0x22; 32]);
    let hash_small: Hash256 = HashAlgorithm::Sha256.hash(preimage_small.as_ref()).into();
    let hash_large: Hash256 = HashAlgorithm::Sha256.hash(preimage_large.as_ref()).into();
    let superseded_snapshot = SettlementData {
        local_amount: 10_000_000_000,
        remote_amount: 10_000_000_000,
        tlcs: vec![
            SettlementTlc {
                tlc_id: TLCId::Received(1),
                payment_amount: 10_000_000_000, // 100 CKB
                payment_hash: hash_small,
                hash_algorithm: HashAlgorithm::Sha256,
                expiry: 100_000,
                local_key: None,
                local_key_pubkey: Some(tlc_key_small.pubkey()),
                local_key_commitment_number: Some(11),
                remote_key: Privkey::from([15; 32]).pubkey(),
            },
            SettlementTlc {
                tlc_id: TLCId::Received(2),
                payment_amount: 20_000_000_000, // 200 CKB
                payment_hash: hash_large,
                hash_algorithm: HashAlgorithm::Sha256,
                expiry: 100_000,
                local_key: None,
                local_key_pubkey: Some(tlc_key_large.pubkey()),
                local_key_commitment_number: Some(12),
                remote_key: Privkey::from([16; 32]).pubkey(),
            },
        ],
    };

    // Start channel with superseded_snapshot, then supersede it with an empty newer snapshot so
    // TLC commitment numbers 11 & 12 exist ONLY in the retained superseded remote settlement.
    let mut fixture = build_custom_ckb_fixture(
        true,
        false,
        superseded_snapshot.local_amount,
        superseded_snapshot.remote_amount,
        superseded_snapshot.tlcs.clone(),
        vec![(hash_small, preimage_small)],
        false,
    );
    fixture
        .store
        .update_pending_remote_settlement(
            fixture.node_id.clone(),
            fixture.channel.channel_id,
            11,
            superseded_snapshot.clone(),
        )
        .expect("retain superseded snapshot");
    let newer_empty = SettlementData {
        local_amount: 10_000_000_000,
        remote_amount: 10_000_000_000,
        tlcs: vec![],
    };
    fixture
        .store
        .update_pending_remote_settlement(
            fixture.node_id.clone(),
            fixture.channel.channel_id,
            12,
            newer_empty.clone(),
        )
        .expect("advance pending remote settlement");
    fixture.store.update_local_settlement(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        newer_empty.clone(),
    );
    fixture.store.update_revocation(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        fiber_types::RevocationData {
            commitment_number: 10,
            aggregated_signature: musig2::CompactSignature::from_bytes(&[1u8; 64]).unwrap(),
            output: CellOutput::default(),
            output_data: Bytes::default(),
        },
        newer_empty,
    );
    fixture.channel = fixture
        .store
        .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
        .expect("reloaded channel");
    // Confirm the active ChannelData no longer knows TLC key 11 without superseded settlements.
    assert!(fixture
        .channel
        .expected_onchain_pubkey(&OnchainKeyPurpose::Tlc {
            commitment_number: 11
        })
        .is_err());

    let signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
    // Queue 100-CKB TLC from the superseded snapshot.
    assert!(build_settlement_tx(
        &fixture.events,
        fixture.cell.clone(),
        EpochNumberWithFraction::new(10, 0, 1),
        EpochNumberWithFraction::new(14, 0, 1),
        50_000,
        &fixture.node_id,
        true,
        fixture.channel.clone(),
        &superseded_snapshot,
        None,
        &fixture.tracked,
        &signer,
        &mut FeeCells,
        &fixture.store,
    )
    .expect("queue superseded TLC")
    .is_none());
    let (req_id, pending) = fixture.pending_request();
    assert_eq!(
        pending.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 11
        }
    );
    let funded = fund_pending(&pending);
    sign_and_submit_funded(
        &fixture,
        req_id,
        &tlc_key_small,
        pending.content.key_purpose,
        &funded.transaction,
    );

    // Higher-priority 200-CKB preimage arrives before broadcast.
    fixture
        .store
        .insert_watch_preimage(fixture.node_id.clone(), hash_large, preimage_large);

    // Watchtower must still broadcast the already-accepted superseded-snapshot TLC signature!
    let built = build_settlement_tx(
        &fixture.events,
        fixture.cell.clone(),
        EpochNumberWithFraction::new(10, 0, 1),
        EpochNumberWithFraction::new(14, 0, 1),
        50_000,
        &fixture.node_id,
        true,
        fixture.channel.clone(),
        &superseded_snapshot,
        None,
        &fixture.tracked,
        &signer,
        &mut FeeCells,
        &fixture.store,
    )
    .expect("broadcast superseded signed TLC")
    .expect("already-signed superseded TLC tx must be broadcast");
    assert_eq!(
        built.data().raw().as_slice(),
        funded.transaction.raw().as_slice()
    );
}

#[test]
fn external_cap_reached_displaces_lowest_priority_unsigned_pending_preserving_signed() {
    let base_key = Privkey::from([42; 32]);
    let amounts: [u128; 6] = [
        1_000_000_000,  // 10 CKB (below self-fundable min)
        2_000_000_000,  // 20 CKB
        3_000_000_000,  // 30 CKB
        4_000_000_000,  // 40 CKB
        20_000_000_000, // 200 CKB (self-fundable)
        30_000_000_000, // 300 CKB (higher self-fundable)
    ];
    let mut keys = Vec::new();
    let mut preimages = Vec::new();
    let mut hashes = Vec::new();
    let mut tlcs = Vec::new();
    for (idx, &amount) in amounts.iter().enumerate() {
        let seed = (idx as u8) + 1;
        let key = fiber_types::channel::derive_private_key(
            &base_key,
            &Privkey::from([seed + 30; 32]).pubkey(),
        );
        let preimage = Hash256::from([seed + 0x50; 32]);
        let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();
        tlcs.push(SettlementTlc {
            tlc_id: TLCId::Received(seed as u64),
            payment_amount: amount,
            payment_hash,
            hash_algorithm: HashAlgorithm::Sha256,
            expiry: 500_000,
            local_key: None,
            local_key_pubkey: Some(key.pubkey()),
            local_key_commitment_number: Some(seed as u64),
            remote_key: Privkey::from([seed + 60; 32]).pubkey(),
        });
        keys.push(key);
        preimages.push(preimage);
        hashes.push(payment_hash);
    }

    let fixture = build_custom_ckb_fixture(
        true,
        false,
        10_000_000_000,
        10_000_000_000,
        tlcs,
        vec![],
        false,
    );

    // Fill all 4 per-channel slots on the same commitment outpoint with 4 progressively larger
    // below-minimum preimage claims (10, 20, 30, 40 CKB).
    let mut initial_ids = Vec::new();
    for i in 0..4 {
        fixture
            .store
            .insert_watch_preimage(fixture.node_id.clone(), hashes[i], preimages[i]);
        assert!(fixture.build_at(50_000).expect("queue small tlc").is_none());
        let (id, req) = fixture.pending_request();
        assert_eq!(
            req.content.key_purpose,
            OnchainKeyPurpose::Tlc {
                commitment_number: (i + 1) as u64
            }
        );
        initial_ids.push(id);
    }
    let WatchtowerSignerState::External(ext_full) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(
        ext_full.pending_requests.len(),
        MAX_PENDING_EXTERNAL_SIGNING_REQUESTS_PER_CHANNEL
    );

    // Now make the 200-CKB self-fundable claim eligible -> must displace the lowest-priority
    // unsigned entry (10 CKB = initial_ids[0]), keep total <= 4, and be exposed via first_pending().
    fixture
        .store
        .insert_watch_preimage(fixture.node_id.clone(), hashes[4], preimages[4]);
    assert!(fixture
        .build_at(50_000)
        .expect("displace lowest pending")
        .is_none());
    let WatchtowerSignerState::External(ext_displaced) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(
        ext_displaced.pending_requests.len() + ext_displaced.signed_signatures.len(),
        MAX_PENDING_EXTERNAL_SIGNING_REQUESTS_PER_CHANNEL
    );
    assert!(
        !ext_displaced.pending_requests.contains_key(&initial_ids[0]),
        "lowest-priority 10-CKB unsigned entry must be displaced"
    );
    assert!(ext_displaced.pending_requests.contains_key(&initial_ids[1]));
    assert!(ext_displaced.pending_requests.contains_key(&initial_ids[2]));
    assert!(ext_displaced.pending_requests.contains_key(&initial_ids[3]));
    let (id_200, req_200) = fixture.pending_request();
    assert_eq!(
        req_200.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 5
        }
    );

    // Backoff must remain strictly enforced even when a strictly higher-priority claim arrives.
    let now_ms = crate::now_timestamp_as_millis_u64();
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = now_ms + 60_000;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    fixture
        .store
        .insert_watch_preimage(fixture.node_id.clone(), hashes[5], preimages[5]);
    assert!(fixture
        .build_at(50_000)
        .expect("blocked by backoff")
        .is_none());
    assert_eq!(
        fixture.pending_request().0,
        id_200,
        "backoff must block even higher-priority displacement"
    );

    // Clear backoff and move 3 entries to `signed_signatures`, leaving only 1 unsigned entry
    // (20 CKB = initial_ids[1]). Build a 300-CKB template via build_signed_settlement_tx and
    // verify ONLY the unsigned entry is displaced while ALL 3 signed_signatures are preserved.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = 0;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    for (id, key_idx) in [
        (initial_ids[2], 2usize),
        (initial_ids[3], 3usize),
        (id_200, 4usize),
    ] {
        let req = match fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        {
            WatchtowerSignerState::External(ext) => ext.pending_requests[&id].clone(),
            _ => panic!("external signer"),
        };
        let funded = fund_pending(&req);
        sign_and_submit_funded(
            &fixture,
            id,
            &keys[key_idx],
            req.content.key_purpose,
            &funded.transaction,
        );
    }

    // Build a 300-CKB template on the same commitment_outpoint directly through
    // `build_signed_settlement_tx` (bypassing the early signed broadcast check so we exercise
    // displacement with 3 signed + 1 unsigned).
    let signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
    let commitment_outpoint = OutPoint::from(fixture.cell.out_point.clone());
    let template_300 = TransactionBuilder::default()
        .input(CellInput::new(commitment_outpoint.clone(), 0))
        .output(
            CellOutput::new_builder()
                .capacity(30_000_000_000u64)
                .build(),
        )
        .output_data(Bytes::default())
        .witness([0x99u8; 32].as_slice().pack())
        .build();
    assert!(build_signed_settlement_tx(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        fixture.channel.channel_id,
        template_300,
        OnchainKeyPurpose::Tlc {
            commitment_number: 6
        },
        true,
        None,
        &signer,
        None,
        None,
    )
    .expect("displace last unsigned entry")
    .is_none());
    let WatchtowerSignerState::External(ext_after_300) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(ext_after_300.signed_signatures.len(), 3);
    assert!(ext_after_300
        .signed_signatures
        .contains_key(&initial_ids[2]));
    assert!(ext_after_300
        .signed_signatures
        .contains_key(&initial_ids[3]));
    assert!(ext_after_300.signed_signatures.contains_key(&id_200));
    assert_eq!(ext_after_300.pending_requests.len(), 1);
    assert!(
        !ext_after_300.pending_requests.contains_key(&initial_ids[1]),
        "only the unsigned 20-CKB entry may be displaced"
    );
    let (id_300, req_300) = fixture.pending_request();
    assert_eq!(
        req_300.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 6
        }
    );

    // Move the 4th entry to `signed_signatures` (now 4 signed, 0 unsigned) -> a new 500-CKB
    // request MUST NOT evict any `signed_signatures`!
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                let pending_300 = ext.pending_requests.remove(&id_300).unwrap();
                ext.signed_signatures.insert(
                    id_300,
                    fiber_types::SignedWatchtowerRequest {
                        content: pending_300.content,
                        signature: [0x77; 65],
                        created_at_ms: now_ms,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    let template_500 = TransactionBuilder::default()
        .input(CellInput::new(commitment_outpoint, 0))
        .output(
            CellOutput::new_builder()
                .capacity(50_000_000_000u64)
                .build(),
        )
        .output_data(Bytes::default())
        .witness([0x88u8; 32].as_slice().pack())
        .build();
    assert!(build_signed_settlement_tx(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        fixture.channel.channel_id,
        template_500,
        OnchainKeyPurpose::Settlement,
        false,
        None,
        &signer,
        None,
        None,
    )
    .expect("must not evict signed_signatures")
    .is_none());
    let WatchtowerSignerState::External(ext_all_signed) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(ext_all_signed.signed_signatures.len(), 4);
    assert!(ext_all_signed.pending_requests.is_empty());
}

#[test]
fn external_barely_above_61_ckb_preimage_does_not_hide_mature_balance_and_larger_lock_fallback() {
    let base_key = Privkey::from([42; 32]);
    let tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
    let preimage = Hash256::from([0x44; 32]);
    let payment_hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();

    // Case A: 61 CKB + 1 shannon preimage TLC lacks the fee buffer (< EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY),
    // so it must NOT hide a mature 200-CKB balance on the very first scan.
    let barely_above_61_tlcs = vec![SettlementTlc {
        tlc_id: TLCId::Received(1),
        payment_amount: 6_100_000_001, // 61 CKB + 1 shannon (< 6_100_010_000)
        payment_hash,
        hash_algorithm: HashAlgorithm::Sha256,
        expiry: 100_000,
        local_key: None,
        local_key_pubkey: Some(tlc_key.pubkey()),
        local_key_commitment_number: Some(1),
        remote_key: Privkey::from([15; 32]).pubkey(),
    }];
    let fixture_a = build_custom_ckb_fixture(
        true,
        false,
        20_000_000_000, // 200 CKB mature balance
        10_000_000_000,
        barely_above_61_tlcs,
        vec![(payment_hash, preimage)],
        false,
    );
    assert!(fixture_a.build_at(150_000).expect("queue").is_none());
    let (_, exposed_a) = fixture_a.pending_request();
    assert_eq!(
        exposed_a.content.key_purpose,
        OnchainKeyPurpose::Settlement,
        "61 CKB + 1 shannon preimage TLC must not hide a mature 200-CKB balance"
    );
    let payout_cap_a: u64 = exposed_a
        .content
        .transaction
        .raw()
        .outputs()
        .get(1)
        .unwrap()
        .capacity()
        .unpack();
    assert_eq!(payout_cap_a, 20_000_000_000);

    // Case B: 70-CKB preimage TLC (>= EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY for secp256k1,
    // but potentially too small for a larger lock like Omnilock/JoyID) + mature 200-CKB balance.
    // Scan 1 offers the 70-CKB TLC; if the wallet leaves it unsigned, Scan 2 offers the larger
    // mature 200-CKB balance as an alternative claim and `first_pending()` exposes the 200-CKB balance.
    let larger_lock_tlcs = vec![SettlementTlc {
        tlc_id: TLCId::Received(1),
        payment_amount: 7_000_000_000, // 70 CKB
        payment_hash,
        hash_algorithm: HashAlgorithm::Sha256,
        expiry: 100_000,
        local_key: None,
        local_key_pubkey: Some(tlc_key.pubkey()),
        local_key_commitment_number: Some(1),
        remote_key: Privkey::from([15; 32]).pubkey(),
    }];
    let fixture_b = build_custom_ckb_fixture(
        true,
        false,
        20_000_000_000, // 200 CKB mature balance
        10_000_000_000,
        larger_lock_tlcs,
        vec![(payment_hash, preimage)],
        false,
    );
    assert!(fixture_b.build_at(150_000).expect("scan 1").is_none());
    let (id_70, req_70) = fixture_b.pending_request();
    assert_eq!(
        req_70.content.key_purpose,
        OnchainKeyPurpose::Tlc {
            commitment_number: 1
        }
    );
    assert!(fixture_b.build_at(150_000).expect("scan 2").is_none());
    let (id_200, req_200) = fixture_b.pending_request();
    assert_ne!(id_200, id_70);
    assert_eq!(
        req_200.content.key_purpose,
        OnchainKeyPurpose::Settlement,
        "larger mature CKB balance must be offered when smaller TLC sits unsigned"
    );
    // Scan 3 reuses the existing 200-CKB balance request without creating duplicates.
    assert!(fixture_b.build_at(150_000).expect("scan 3").is_none());
    let WatchtowerSignerState::External(ext_b) = fixture_b
        .store
        .get_watchtower_signer(&fixture_b.node_id, &fixture_b.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert_eq!(ext_b.pending_requests.len(), 2);
    assert_eq!(fixture_b.pending_request().0, id_200);
}

#[test]
fn external_send_error_301_reconciles_committed_unknown_and_dead_fee_inputs() {
    let err_301 = ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-301),
        message: "TransactionFailedToResolve: Resolve failed Dead(...)".to_string(),
        data: None,
    });
    let setup = || {
        let fixture = Fixture::new(true, true, false, false);
        assert!(fixture.build().expect("queue").is_none());
        let (request_id, pending) = fixture.pending_request();
        let funded = fund_pending(&pending);
        let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
        assert_eq!(
            fixture
                .submit_with_tx(request_id, sig, funded.transaction.clone())
                .expect("submit"),
            SubmitWatchtowerSignatureResult::Applied
        );
        let built = fixture.build().expect("apply").expect("signed tx");
        (fixture, request_id, built)
    };
    let now_ms = crate::now_timestamp_as_millis_u64();

    // Case 1a: -301 when signed tx is already Committed -> prune signed_signatures, preserve last_applied, NO backoff.
    let (fixture, request_id, built) = setup();
    let signed_tx_hash: ckb_types::H256 = built.hash().unpack();
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |_| Ok("unknown".to_string()),
        |h| {
            if *h == signed_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);

    // Case 1b: -301 when signed tx lookup returns None, but inputs[0] is ConfirmedSpent -> prune, preserve last_applied, NO backoff.
    let (fixture, request_id, built) = setup();
    let commitment_outpoint = built.inputs().get(0).unwrap().previous_output();
    let commitment_tx_hash: ckb_types::H256 = commitment_outpoint.tx_hash().unpack();
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |_| Ok("unknown".to_string()),
        |h| {
            if *h == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);

    // Case 2: -301 when inputs[0] is Live and funding input (inputs[1]) is Unknown (unsynced parent)
    // -> retain signed_signatures + last_applied, NO backoff.
    let (fixture, request_id, built) = setup();
    let commitment_outpoint = built.inputs().get(0).unwrap().previous_output();
    let fee_outpoint = built.inputs().get(1).unwrap().previous_output();
    let fee_tx_hash: ckb_types::H256 = fee_outpoint.tx_hash().unpack();
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == commitment_outpoint {
                Ok("live".to_string())
            } else {
                Ok("unknown".to_string())
            }
        },
        // Parent block of fee input is not synced on this node yet -> Unknown.
        |_| Ok(None),
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.contains_key(&request_id));
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);

    // Case 3: -301 when inputs[0] is Live and funding input (inputs[1]) is ConfirmedSpent
    // -> evict signed_signatures, clear last_applied, arm backoff.
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == commitment_outpoint {
                Ok("live".to_string())
            } else {
                Ok("unknown".to_string())
            }
        },
        |h| {
            if *h == fee_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert!(ext.last_applied.is_none());
    assert_eq!(ext.consecutive_expired_requests, 1);
    assert!(ext.retry_after_ms > now_ms);
}

#[test]
fn external_revocation_retires_signed_requests_and_releases_quota_without_backoff() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let commitment_outpoint = OutPoint::from(fixture.cell.out_point.clone());
    let (node_active_before, global_active_before, _) = fixture
        .store
        .watchtower_external_signing_stats(&fixture.node_id);
    assert_eq!(node_active_before, 1);
    assert_eq!(global_active_before, 1);

    // Revocation branch encounters the revoked commitment outpoint BEFORE TTL has elapsed:
    // signed_signatures referencing it must be retired immediately, clearing uncommitted
    // last_applied, releasing channel/node/global quota, and NEVER arming backoff.
    let now_ms = crate::now_timestamp_as_millis_u64();
    retire_external_signing_requests_on_revocation_with_lookup(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &commitment_outpoint,
        now_ms,
        |_| Ok(None),
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert!(ext.pending_requests.is_empty());
    assert!(ext.last_applied.is_none());
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
    let (node_active_after, global_active_after, _) = fixture
        .store
        .watchtower_external_signing_stats(&fixture.node_id);
    assert_eq!(node_active_after, 0);
    assert_eq!(global_active_after, 0);

    // Also verify a past-TTL signed request whose tx IS Committed preserves `last_applied`
    // while still retiring `signed_signatures` and releasing quota without arming backoff.
    assert!(fixture.build().expect("queue 2").is_none());
    let (req_id_2, pending_2) = fixture.pending_request();
    let funded_2 = fund_pending(&pending_2);
    let sig_2 = sign_onchain_request(&fixture.tlc_key, &funded_2).expect("sig 2");
    assert_eq!(
        fixture
            .submit_with_tx(req_id_2, sig_2, funded_2.transaction.clone())
            .expect("submit 2"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let signed_tx_hash_2: ckb_types::H256 = funded_2.transaction.calc_tx_hash().unpack();
    let other_outpoint = OutPoint::new(Byte32::from([0xdd; 32]), 0);
    let after_ttl_ms = now_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 10_000;
    retire_external_signing_requests_on_revocation_with_lookup(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &other_outpoint,
        after_ttl_ms,
        |h| {
            if *h == signed_tx_hash_2 {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext_2) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext_2.signed_signatures.is_empty());
    assert_eq!(
        ext_2.last_applied.as_ref().map(|a| a.request_id),
        Some(req_id_2)
    );
    assert_eq!(ext_2.consecutive_expired_requests, 0);
    assert_eq!(ext_2.retry_after_ms, 0);
}

#[test]
fn external_empty_spent_inputs_reconciles_committed_signed_tx_past_ttl() {
    let fixture = Fixture::new(true, true, true, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let signed_tx_hash: ckb_types::H256 = funded.transaction.calc_tx_hash().unpack();
    let after_ttl_ms =
        crate::now_timestamp_as_millis_u64() + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 10_000;

    // Simulate `check_watch_channel` hitting `Ok(None)` (rebuilding funding indexer) past TTL:
    // `spent_inputs` is empty, but reconciliation queries liveness/tx status before expiration
    // and preserves `last_applied` without arming backoff.
    reconcile_and_expire_external_signing_requests_with_lookups(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &HashSet::new(),
        after_ttl_ms,
        |_| Ok("unknown".to_string()),
        |h| {
            if *h == signed_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );

    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction)
            .expect("idempotent retry after empty spent_inputs reconcile"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );

    // Once active requests are empty, subsequent reconciliation performs zero RPC lookups.
    let rpc_calls = std::cell::Cell::new(0usize);
    reconcile_and_expire_external_signing_requests_with_lookups(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &HashSet::new(),
        after_ttl_ms + 10_000,
        |_| {
            rpc_calls.set(rpc_calls.get() + 1);
            Ok("unknown".to_string())
        },
        |_| {
            rpc_calls.set(rpc_calls.get() + 1);
            Ok(None)
        },
    );
    assert_eq!(rpc_calls.get(), 0);
}

#[test]
fn external_websocket_events_publish_effective_tenant_backoff_across_channels() {
    use crate::rpc::watchtower::{GetWatchtowerSigningStatusParams, WatchtowerSigningStatus};
    use crate::signing::notifications::{SigningChange, SigningScope};
    use futures::FutureExt;

    let fixture_b = Fixture::new(true, true, false, false);
    let node_id = fixture_b.node_id.clone();
    let channel_b = fixture_b.channel.channel_id;
    let channel_a = Hash256::from([0xa1; 32]);
    fixture_b.store.insert_watch_channel(
        node_id.clone(),
        channel_a,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(fixture_b.base_key.pubkey()),
            remote_settlement: fixture_b.channel.remote_settlement_key,
            local_funding: fixture_b.channel.local_funding_pubkey,
            remote_funding: fixture_b.channel.remote_funding_pubkey,
        },
        SettlementData {
            local_amount: 10_000_000_000,
            remote_amount: 10_000_000_000,
            tlcs: vec![],
        },
        ChannelFeatures::V2,
    );

    let now_ms = crate::now_timestamp_as_millis_u64();
    let tenant_retry_after_ms = now_ms + 300_000;
    // Channel A is in tenant-wide backoff; channel B has a shorter local backoff.
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_a, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.consecutive_expired_requests = 2;
                ext.retry_after_ms = tenant_retry_after_ms;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_b, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.consecutive_expired_requests = 1;
                ext.retry_after_ms = now_ms + 60_000;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    let mut changes = fixture_b
        .events
        .hub
        .subscribe(SigningScope::Watchtower(node_id.clone()));

    // Channel B clears its own backoff on a successful send -> live WebSocket event AND RPC
    // get_watchtower_signing_status(B) must both report Backoff with channel A's tenant backoff!
    clear_channel_external_retry_backoff(&fixture_b.events, &fixture_b.store, &node_id, &channel_b);

    let event = changes
        .recv()
        .now_or_never()
        .expect("event ready")
        .expect("event ok");
    let SigningChange::WatchtowerUpdated { channel_id, status } = event.as_ref() else {
        panic!("expected WatchtowerUpdated event, got {event:?}");
    };
    assert_eq!(*channel_id, channel_b);
    assert_eq!(
        *status,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms: tenant_retry_after_ms,
            consecutive_expired_requests: 0,
        }
    );

    let rpc = WatchtowerRpcServerImpl::new(fixture_b.store.clone())
        .with_signing_events(fixture_b.events.clone());
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let rpc_status = rt
        .block_on(rpc.get_watchtower_signing_status(
            RpcContext {
                node_id: node_id.to_string(),
                tenant_scoped: false,
            },
            GetWatchtowerSigningStatusParams {
                channel_id: channel_b.into(),
            },
        ))
        .expect("get_watchtower_signing_status");
    assert_eq!(rpc_status.status, *status);

    // Also verify `submit_watchtower_signature` publishes effective tenant backoff when the last
    // pending request on channel B is consumed while channel A is still in tenant backoff.
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_a, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = 0;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    assert!(fixture_b.build().expect("queue on B").is_none());
    let (req_id_b, pending_b) = fixture_b.pending_request();
    // Drain the ExternalFundingRequired event from queueing.
    let _ = changes.recv().now_or_never();
    // Re-arm tenant backoff on channel A before submitting channel B's signature.
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_a, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = tenant_retry_after_ms;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    let funded_b = fund_pending(&pending_b);
    let sig_b = sign_onchain_request(&fixture_b.tlc_key, &funded_b).expect("sig B");
    assert_eq!(
        fixture_b
            .submit_with_tx(req_id_b, sig_b, funded_b.transaction)
            .expect("submit B"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let submit_event = changes
        .recv()
        .now_or_never()
        .expect("submit event ready")
        .expect("submit event ok");
    let SigningChange::WatchtowerUpdated {
        channel_id: submit_ch,
        status: submit_status,
    } = submit_event.as_ref()
    else {
        panic!("expected WatchtowerUpdated after submit, got {submit_event:?}");
    };
    assert_eq!(*submit_ch, channel_b);
    assert_eq!(
        *submit_status,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms: tenant_retry_after_ms,
            consecutive_expired_requests: 0,
        }
    );
    let rpc_status_after_submit = rt
        .block_on(rpc.get_watchtower_signing_status(
            RpcContext {
                node_id: node_id.to_string(),
                tenant_scoped: false,
            },
            GetWatchtowerSigningStatusParams {
                channel_id: channel_b.into(),
            },
        ))
        .expect("get_watchtower_signing_status after submit");
    assert_eq!(rpc_status_after_submit.status, *submit_status);
}

#[test]
fn external_unchanged_rebroadcast_and_unobserved_updates_skip_stats_scan_and_events() {
    use crate::signing::notifications::{SigningChange, SigningScope};
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct CountingWatchtowerStore {
        inner: Store,
        stats_calls: Arc<AtomicUsize>,
    }

    impl WatchtowerStore for CountingWatchtowerStore {
        fn get_watch_channels_with_nodes(&self) -> Vec<(NodeId, ChannelData)> {
            self.inner.get_watch_channels_with_nodes()
        }
        fn get_watch_channels_for_node(&self, node_id: &NodeId) -> Vec<ChannelData> {
            self.inner.get_watch_channels_for_node(node_id)
        }
        fn insert_watch_channel(
            &self,
            node_id: NodeId,
            channel_id: Hash256,
            funding_udt_type_script: Option<Script>,
            keys: WatchChannelKeys,
            settlement_data: SettlementData,
            channel_features: ChannelFeatures,
        ) -> bool {
            self.inner.insert_watch_channel(
                node_id,
                channel_id,
                funding_udt_type_script,
                keys,
                settlement_data,
                channel_features,
            )
        }
        fn remove_watch_channel(&self, node_id: NodeId, channel_id: Hash256) -> bool {
            self.inner.remove_watch_channel(node_id, channel_id)
        }
        fn get_watch_channel(&self, node_id: &NodeId, channel_id: &Hash256) -> Option<ChannelData> {
            self.inner.get_watch_channel(node_id, channel_id)
        }
        fn update_revocation(
            &self,
            node_id: NodeId,
            channel_id: Hash256,
            revocation_data: RevocationData,
            remote_settlement_data: SettlementData,
        ) {
            self.inner.update_revocation(
                node_id,
                channel_id,
                revocation_data,
                remote_settlement_data,
            )
        }
        fn update_pending_remote_settlement(
            &self,
            node_id: NodeId,
            channel_id: Hash256,
            commitment_number: u64,
            pending_remote_settlement_data: SettlementData,
        ) -> Result<(), String> {
            self.inner.update_pending_remote_settlement(
                node_id,
                channel_id,
                commitment_number,
                pending_remote_settlement_data,
            )
        }
        fn get_superseded_remote_settlements(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
        ) -> Vec<SettlementData> {
            self.inner
                .get_superseded_remote_settlements(node_id, channel_id)
        }
        fn update_local_settlement(
            &self,
            node_id: NodeId,
            channel_id: Hash256,
            local_settlement_data: SettlementData,
        ) {
            self.inner
                .update_local_settlement(node_id, channel_id, local_settlement_data)
        }
        fn insert_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256, preimage: Hash256) {
            self.inner
                .insert_watch_preimage(node_id, payment_hash, preimage)
        }
        fn remove_watch_preimage(&self, node_id: NodeId, payment_hash: Hash256) {
            self.inner.remove_watch_preimage(node_id, payment_hash)
        }
        fn get_watchtower_signer(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
        ) -> WatchtowerSignerState {
            self.inner.get_watchtower_signer(node_id, channel_id)
        }
        fn put_watchtower_signer(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
            state: WatchtowerSignerState,
        ) {
            self.inner.put_watchtower_signer(node_id, channel_id, state)
        }
        fn update_watchtower_signer<R, E>(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
            update: impl FnOnce(&mut WatchtowerSignerState) -> Result<R, E>,
        ) -> Result<R, E> {
            self.inner
                .update_watchtower_signer(node_id, channel_id, update)
        }
        fn watchtower_external_signing_stats(&self, node_id: &NodeId) -> (usize, usize, u64) {
            self.stats_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.watchtower_external_signing_stats(node_id)
        }
        fn get_watch_preimage(&self, node_id: &NodeId, payment_hash: &Hash256) -> Option<Hash256> {
            self.inner.get_watch_preimage(node_id, payment_hash)
        }
        fn insert_onchain_tlc_settlement(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
            tlc_id: TLCId,
            settlement: crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement,
        ) {
            self.inner
                .insert_onchain_tlc_settlement(node_id, channel_id, tlc_id, settlement)
        }
        fn get_onchain_tlc_settlement(
            &self,
            node_id: &NodeId,
            channel_id: &Hash256,
            tlc_id: TLCId,
            payment_hash: &Hash256,
        ) -> Option<crate::fiber::onchain_tlc_reconcile::OnChainTlcSettlement> {
            self.inner
                .get_onchain_tlc_settlement(node_id, channel_id, tlc_id, payment_hash)
        }
    }

    let fixture = Fixture::new(true, true, false, false);
    let stats_calls = Arc::new(AtomicUsize::new(0));
    let counting_store = CountingWatchtowerStore {
        inner: fixture.store.clone(),
        stats_calls: stats_calls.clone(),
    };
    let signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));

    // Queue a pending request.
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");

    // 1. Unobserved path (no WS subscribers): submit_watchtower_signature,
    //    apply_existing_signed_settlement_tx, clear_channel_external_retry_backoff, and
    //    prune_spent_signing_requests must perform ZERO stats scans.
    stats_calls.store(0, Ordering::SeqCst);
    let rpc = WatchtowerRpcServerImpl::new(counting_store.clone())
        .with_signing_events(fixture.events.clone());
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    assert_eq!(
        rt.block_on(rpc.submit_watchtower_signature(
            RpcContext {
                node_id: fixture.node_id.to_string(),
                tenant_scoped: false,
            },
            SubmitWatchtowerSignatureParams {
                channel_id: fixture.channel.channel_id.into(),
                request_id: request_id.into(),
                signature: sig.to_vec(),
                signed_transaction: funded.transaction.clone().into(),
            },
        ))
        .expect("submit unobserved"),
        SubmitWatchtowerSignatureResult::Applied
    );
    assert_eq!(
        stats_calls.load(Ordering::SeqCst),
        0,
        "unobserved submit_watchtower_signature must skip watchtower_external_signing_stats"
    );

    let signed_content = match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        WatchtowerSignerState::External(ext) => ext.signed_signatures[&request_id].content.clone(),
        other => panic!("expected external signer, got {other:?}"),
    };
    assert!(apply_existing_signed_settlement_tx(
        &fixture.events,
        &counting_store,
        &fixture.node_id,
        fixture.channel.channel_id,
        request_id,
        &signed_content,
        false,
    )
    .expect("apply unobserved")
    .is_some());
    assert_eq!(
        stats_calls.load(Ordering::SeqCst),
        0,
        "unobserved apply_existing_signed_settlement_tx must skip stats scan"
    );

    // Arm backoff directly, then clear while unobserved -> 0 stats scans.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.retry_after_ms = crate::now_timestamp_as_millis_u64() + 60_000;
                ext.consecutive_expired_requests = 1;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    clear_channel_external_retry_backoff(
        &fixture.events,
        &counting_store,
        &fixture.node_id,
        &fixture.channel.channel_id,
    );
    assert_eq!(
        stats_calls.load(Ordering::SeqCst),
        0,
        "unobserved clear_channel_external_retry_backoff must skip stats scan"
    );

    // 2. Observed path: attach WS subscriber. Repeatedly applying the already-applied signed tx
    //    (both via apply_existing_signed_settlement_tx and build_signed_settlement_tx) must emit
    //    ZERO WatchtowerUpdated events and perform ZERO stats scans.
    let mut changes = fixture
        .events
        .hub
        .subscribe(SigningScope::Watchtower(fixture.node_id.clone()));
    stats_calls.store(0, Ordering::SeqCst);

    for _ in 0..3 {
        let tx = apply_existing_signed_settlement_tx(
            &fixture.events,
            &counting_store,
            &fixture.node_id,
            fixture.channel.channel_id,
            request_id,
            &signed_content,
            false,
        )
        .expect("rebroadcast apply")
        .expect("signed tx");
        assert_applied_signature(&tx, &funded, &fixture.tlc_key);
    }
    // Also exercise build_signed_settlement_tx with existing_request_id = Some(request_id)
    // and existing_request_id = None hitting the signed_signatures branch.
    let tx_existing = build_signed_settlement_tx(
        &fixture.events,
        &counting_store,
        &fixture.node_id,
        fixture.channel.channel_id,
        pending.content.transaction.clone().into_view(),
        pending.content.key_purpose,
        false,
        None,
        &signer,
        Some(request_id),
        pending.required_expiry_since,
    )
    .expect("build_signed_settlement_tx existing")
    .expect("signed tx");
    assert_applied_signature(&tx_existing, &funded, &fixture.tlc_key);

    assert_eq!(
        stats_calls.load(Ordering::SeqCst),
        0,
        "unchanged rebroadcast must not trigger watchtower_external_signing_stats scan"
    );
    assert!(
        changes.recv().now_or_never().is_none(),
        "unchanged rebroadcast must not emit redundant WatchtowerUpdated events"
    );

    // Exercise build_signed_settlement_tx with existing_request_id = None (does 1 pre-lock stats
    // read for cap checks, but must emit 0 WatchtowerUpdated events and 0 finish stats scans).
    stats_calls.store(0, Ordering::SeqCst);
    let tx_concurrent = build_signed_settlement_tx(
        &fixture.events,
        &counting_store,
        &fixture.node_id,
        fixture.channel.channel_id,
        pending.content.transaction.clone().into_view(),
        pending.content.key_purpose,
        false,
        None,
        &signer,
        None,
        pending.required_expiry_since,
    )
    .expect("build_signed_settlement_tx concurrent signed hit")
    .expect("signed tx");
    assert_applied_signature(&tx_concurrent, &funded, &fixture.tlc_key);
    assert_eq!(
        stats_calls.load(Ordering::SeqCst),
        1,
        "only the pre-lock cap stats read should run; finish_watchtower_signing_update must skip"
    );
    assert!(
        changes.recv().now_or_never().is_none(),
        "unchanged concurrent signed hit must not emit redundant WatchtowerUpdated events"
    );

    // Contrast: if last_applied differs (e.g. cleared), applying DOES update state and emits 1 event.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.last_applied = None;
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    stats_calls.store(0, Ordering::SeqCst);
    assert!(apply_existing_signed_settlement_tx(
        &fixture.events,
        &counting_store,
        &fixture.node_id,
        fixture.channel.channel_id,
        request_id,
        &signed_content,
        false,
    )
    .expect("apply when last_applied changed")
    .is_some());
    assert_eq!(stats_calls.load(Ordering::SeqCst), 1);
    let event = changes
        .recv()
        .now_or_never()
        .expect("event ready")
        .expect("event ok");
    assert!(matches!(
        event.as_ref(),
        SigningChange::WatchtowerUpdated { channel_id, .. } if *channel_id == fixture.channel.channel_id
    ));
}

#[test]
fn external_send_error_302_cellbase_immaturity_retains_signed_tx_without_backoff() {
    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let funded = fund_pending(&pending);
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let built = fixture.build().expect("apply").expect("signed tx");
    let now_ms = crate::now_timestamp_as_millis_u64();

    let retryable_errors = [
        ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-302),
            message:
                "TransactionFailedToVerify: Verification failed Transaction(CellbaseImmaturity)"
                    .to_string(),
            data: None,
        }),
        ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-302),
            message: "TransactionFailedToVerify".to_string(),
            data: Some(serde_json::Value::String(
                "Verification failed Transaction(CellbaseImmaturity)".to_string(),
            )),
        }),
        ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
            code: jsonrpc_core::ErrorCode::ServerError(-302),
            message: "TransactionFailedToVerify: Verification failed Transaction(Immature)"
                .to_string(),
            data: None,
        }),
    ];

    for err in &retryable_errors {
        handle_external_settlement_send_error(
            &fixture.events,
            &fixture.store,
            &fixture.node_id,
            &fixture.channel.channel_id,
            &built,
            err,
            now_ms,
            |_| Ok("live".to_string()),
            |_| Ok(None),
        );
        let WatchtowerSignerState::External(ext) = fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        else {
            panic!("external signer");
        };
        assert!(
            ext.signed_signatures.contains_key(&request_id),
            "retryable -302 error {err:?} must retain signed_signatures"
        );
        assert_eq!(
            ext.last_applied.as_ref().map(|a| a.request_id),
            Some(request_id)
        );
        assert_eq!(ext.consecutive_expired_requests, 0);
        assert_eq!(ext.retry_after_ms, 0);
    }

    // Non-immaturity -302 error must still evict and arm backoff.
    let script_failure = ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-302),
        message: "TransactionFailedToVerify: Verification failed Script(...)".to_string(),
        data: None,
    });
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &script_failure,
        now_ms,
        |_| Ok("live".to_string()),
        |_| Ok(None),
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert!(ext.last_applied.is_none());
    assert_eq!(ext.consecutive_expired_requests, 1);
    assert!(ext.retry_after_ms > now_ms);
}

#[test]
fn external_send_error_301_retains_on_unsynced_cell_dep_when_all_inputs_live() {
    use ckb_types::{core::DepType, packed::CellDep};

    let err_301 = ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-301),
        message: "TransactionFailedToResolve: Resolve failed Unknown(...)".to_string(),
        data: None,
    });
    let wallet_dep_outpoint = OutPoint::new(Byte32::from([0xee; 32]), 0);
    let wallet_dep_tx_hash: ckb_types::H256 = wallet_dep_outpoint.tx_hash().unpack();
    let wallet_dep = CellDep::new_builder()
        .out_point(wallet_dep_outpoint.clone())
        .dep_type(DepType::Code)
        .build();

    let fixture = Fixture::new(true, true, false, false);
    assert!(fixture.build().expect("queue").is_none());
    let (request_id, pending) = fixture.pending_request();
    let mut funded = fund_pending(&pending);
    funded.transaction = funded
        .transaction
        .into_view()
        .as_advanced_builder()
        .cell_dep(wallet_dep)
        .build()
        .data();
    let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction.clone())
            .expect("submit"),
        SubmitWatchtowerSignatureResult::Applied
    );
    let built = fixture.build().expect("apply").expect("signed tx");
    let now_ms = crate::now_timestamp_as_millis_u64();

    // All inputs are "live", but wallet cell_dep returns "unknown" with non-committed parent tx
    // -> must retain signed_signatures + last_applied without backoff.
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == wallet_dep_outpoint {
                Ok("unknown".to_string())
            } else {
                Ok("live".to_string())
            }
        },
        |_| Ok(None),
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(
        ext.signed_signatures.contains_key(&request_id),
        "unsynced cell_dep on -301 must retain signed tx"
    );
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);

    // If the wallet cell_dep is ConfirmedSpent (parent tx Committed) while all inputs remain live,
    // -301 must evict signed_signatures, clear last_applied, and arm backoff.
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == wallet_dep_outpoint {
                Ok("unknown".to_string())
            } else {
                Ok("live".to_string())
            }
        },
        |h| {
            if *h == wallet_dep_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert!(ext.last_applied.is_none());
    assert_eq!(ext.consecutive_expired_requests, 1);
    assert!(ext.retry_after_ms > now_ms);
}

#[test]
fn external_send_error_301_rechecks_confirmation_before_evicting_spent_fee_input() {
    let err_301 = ckb_sdk::RpcError::Rpc(jsonrpc_core::Error {
        code: jsonrpc_core::ErrorCode::ServerError(-301),
        message: "TransactionFailedToResolve: Resolve failed Dead(...)".to_string(),
        data: None,
    });
    let setup = || {
        let fixture = Fixture::new(true, true, false, false);
        assert!(fixture.build().expect("queue").is_none());
        let (request_id, pending) = fixture.pending_request();
        let funded = fund_pending(&pending);
        let sig = sign_onchain_request(&fixture.tlc_key, &funded).expect("sig");
        assert_eq!(
            fixture
                .submit_with_tx(request_id, sig, funded.transaction.clone())
                .expect("submit"),
            SubmitWatchtowerSignatureResult::Applied
        );
        let built = fixture.build().expect("apply").expect("signed tx");
        (fixture, request_id, sig, funded, built)
    };
    let now_ms = crate::now_timestamp_as_millis_u64();

    // Case A: Settlement confirms between reading inputs[0] ("live") and inputs[1] ("unknown" /
    // ConfirmedSpent); rechecking get_tx_status(&signed_tx_hash) observes Status::Committed ->
    // prune signed_signatures preserving last_applied, zero backoff, AlreadyApplied on retry.
    let (fixture, request_id, sig, funded, built) = setup();
    let signed_tx_hash: ckb_types::H256 = built.hash().unpack();
    let commitment_outpoint = built.inputs().get(0).unwrap().previous_output();
    let fee_outpoint = built.inputs().get(1).unwrap().previous_output();
    let fee_tx_hash: ckb_types::H256 = fee_outpoint.tx_hash().unpack();
    let mut tx_status_reads = 0usize;
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == commitment_outpoint {
                Ok("live".to_string())
            } else {
                Ok("unknown".to_string())
            }
        },
        |h| {
            if *h == signed_tx_hash {
                tx_status_reads += 1;
                if tx_status_reads == 1 {
                    Ok(None)
                } else {
                    Ok(Some(Status::Committed))
                }
            } else if *h == fee_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction)
            .expect("idempotent retry after TOCTOU commit"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );

    // Case B: get_tx_status(&signed_tx_hash) stays None, but inputs[0] transitions from "live"
    // on the first read to ConfirmedSpent on the second read -> prune preserving last_applied,
    // zero backoff, AlreadyApplied on retry.
    let (fixture, request_id, sig, funded, built) = setup();
    let commitment_outpoint = built.inputs().get(0).unwrap().previous_output();
    let commitment_tx_hash: ckb_types::H256 = commitment_outpoint.tx_hash().unpack();
    let fee_outpoint = built.inputs().get(1).unwrap().previous_output();
    let fee_tx_hash: ckb_types::H256 = fee_outpoint.tx_hash().unpack();
    let mut commitment_reads = 0usize;
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == commitment_outpoint {
                commitment_reads += 1;
                if commitment_reads == 1 {
                    Ok("live".to_string())
                } else {
                    Ok("unknown".to_string())
                }
            } else {
                Ok("unknown".to_string())
            }
        },
        |h| {
            if *h == fee_tx_hash || *h == commitment_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.is_empty());
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
    assert_eq!(
        fixture
            .submit_with_tx(request_id, sig, funded.transaction)
            .expect("idempotent retry after TOCTOU commitment spend"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );

    // Case C: On TOCTOU recheck, signed tx appears Pending in mempool -> retain without backoff.
    let (fixture, request_id, _sig, _funded, built) = setup();
    let signed_tx_hash: ckb_types::H256 = built.hash().unpack();
    let commitment_outpoint = built.inputs().get(0).unwrap().previous_output();
    let fee_outpoint = built.inputs().get(1).unwrap().previous_output();
    let fee_tx_hash: ckb_types::H256 = fee_outpoint.tx_hash().unpack();
    let mut tx_status_reads = 0usize;
    handle_external_settlement_send_error(
        &fixture.events,
        &fixture.store,
        &fixture.node_id,
        &fixture.channel.channel_id,
        &built,
        &err_301,
        now_ms,
        |op| {
            if *op == commitment_outpoint {
                Ok("live".to_string())
            } else {
                Ok("unknown".to_string())
            }
        },
        |h| {
            if *h == signed_tx_hash {
                tx_status_reads += 1;
                if tx_status_reads == 1 {
                    Ok(None)
                } else {
                    Ok(Some(Status::Pending))
                }
            } else if *h == fee_tx_hash {
                Ok(Some(Status::Committed))
            } else {
                Ok(None)
            }
        },
    );
    let WatchtowerSignerState::External(ext) = fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    else {
        panic!("external signer");
    };
    assert!(ext.signed_signatures.contains_key(&request_id));
    assert_eq!(
        ext.last_applied.as_ref().map(|a| a.request_id),
        Some(request_id)
    );
    assert_eq!(ext.consecutive_expired_requests, 0);
    assert_eq!(ext.retry_after_ms, 0);
}

#[test]
fn external_unsigned_requests_expire_on_chain_lookup_errors_without_backoff() {
    use crate::signing::notifications::{SigningChange, SigningScope};
    use futures::FutureExt;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn spawn_mock_ckb_rpc(responses: Vec<String>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock ckb rpc");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (format!("http://{addr}"), handle)
    }

    let signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
    let tx_hash_hex = format!("{:#x}", Byte32::from([0x44; 32]));
    let get_txs_hit = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"last_cursor":"0x","objects":[{{"tx_hash":"{tx_hash_hex}","block_number":"0x1","tx_index":"0x0","io_index":"0x0","io_type":"input"}}]}}}}"#
    );
    let rpc_err = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal error"}}"#
        .to_string();
    let get_tx_none = r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string();

    let scenarios = [
        // 1. find_first_input_tx_hash fails with RPC error.
        vec![rpc_err.clone()],
        // 2. get_transaction returns Ok(None).
        vec![get_txs_hit.clone(), get_tx_none],
        // 3. get_transaction fails with RPC error.
        vec![get_txs_hit, rpc_err],
    ];

    for responses in scenarios {
        let fixture = Fixture::new(true, true, false, false);
        let now_ms = crate::now_timestamp_as_millis_u64();
        let pending_id = Hash256::from([0x11; 32]);
        let signed_id = Hash256::from([0x22; 32]);
        let dummy_tx = TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0x33; 32]), 0),
                0,
            ))
            .build()
            .data();
        let dummy_content = OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: dummy_tx.clone(),
        };
        let signed_tx_hash = fiber_types::external_funding_watchtower_tx_hash(&dummy_tx);

        fixture
            .store
            .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
                if let WatchtowerSignerState::External(ext) = state {
                    ext.pending_requests.insert(
                        pending_id,
                        fiber_types::PendingWatchtowerRequest {
                            content: dummy_content.clone(),
                            created_at_ms: now_ms,
                            required_expiry_since: None,
                        },
                    );
                    ext.signed_signatures.insert(
                        signed_id,
                        fiber_types::SignedWatchtowerRequest {
                            content: dummy_content.clone(),
                            signature: [0x55; 65],
                            created_at_ms: now_ms,
                        },
                    );
                    ext.last_applied = Some(fiber_types::LastAppliedWatchtowerSignature {
                        request_id: signed_id,
                        signature: [0x55; 65],
                        signed_transaction_hash: signed_tx_hash,
                    });
                }
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(
            fixture
                .store
                .watchtower_external_signing_stats(&fixture.node_id),
            (2, 2, 0)
        );

        let mut changes = fixture
            .events
            .hub
            .subscribe(SigningScope::Watchtower(fixture.node_id.clone()));
        let (url, server) = spawn_mock_ckb_rpc(responses);
        let ckb_client = CkbRpcClient::with_builder(&url, |b| b.no_proxy()).expect("ckb client");
        let after_ttl_ms = now_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 1;

        check_watch_channel(
            &fixture.events,
            &fixture.store,
            &signer,
            &ckb_client,
            &mut FeeCells,
            fixture.node_id.clone(),
            fixture.channel.clone(),
            after_ttl_ms,
        );
        server.join().expect("mock rpc server finished");

        let WatchtowerSignerState::External(ext) = fixture
            .store
            .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
        else {
            panic!("external signer");
        };
        assert!(
            ext.pending_requests.is_empty(),
            "expired unsigned pending request must be removed even on chain lookup error"
        );
        assert!(
            ext.signed_signatures.contains_key(&signed_id),
            "signed_signatures must NOT be dropped on chain lookup error"
        );
        assert_eq!(
            ext.last_applied.as_ref().map(|a| a.request_id),
            Some(signed_id)
        );
        assert_eq!(ext.consecutive_expired_requests, 0);
        assert_eq!(ext.retry_after_ms, 0);
        assert_eq!(
            fixture
                .store
                .watchtower_external_signing_stats(&fixture.node_id),
            (1, 1, 0),
            "per-node and global quota must reflect expired unsigned request removal"
        );
        let event = changes
            .recv()
            .now_or_never()
            .expect("event ready")
            .expect("event ok");
        assert!(matches!(
            event.as_ref(),
            SigningChange::WatchtowerUpdated {
                channel_id,
                status: fiber_json_types::WatchtowerSigningStatus::NoSignatureRequired,
            } if *channel_id == fixture.channel.channel_id
        ));
    }
}

#[test]
fn external_websocket_sibling_channel_receives_tenant_backoff_arm_and_early_clear() {
    use crate::rpc::watchtower::{GetWatchtowerSigningStatusParams, WatchtowerSigningStatus};
    use crate::signing::notifications::{SigningChange, SigningScope};
    use futures::FutureExt;

    let fixture_b = Fixture::new(true, true, false, false);
    let node_id = fixture_b.node_id.clone();
    let channel_b = fixture_b.channel.channel_id;
    let channel_a = Hash256::from([0xa1; 32]);
    let channel_c = Hash256::from([0xc3; 32]);
    for ch in [channel_a, channel_c] {
        fixture_b.store.insert_watch_channel(
            node_id.clone(),
            ch,
            None,
            WatchChannelKeys {
                local_settlement: SettlementKey::external(fixture_b.base_key.pubkey()),
                remote_settlement: fixture_b.channel.remote_settlement_key,
                local_funding: fixture_b.channel.local_funding_pubkey,
                remote_funding: fixture_b.channel.remote_funding_pubkey,
            },
            SettlementData {
                local_amount: 10_000_000_000,
                remote_amount: 10_000_000_000,
                tlcs: vec![],
            },
            ChannelFeatures::V2,
        );
    }

    let now_ms = crate::now_timestamp_as_millis_u64();
    let dummy_content = |seed: u8| OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([seed; 32]), 0),
                0,
            ))
            .build()
            .data(),
    };

    // Put an active pending request on channel C so we verify it is unaffected by sibling backoff.
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_c, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.insert(
                    Hash256::from([0xcc; 32]),
                    fiber_types::PendingWatchtowerRequest {
                        content: dummy_content(0xcc),
                        created_at_ms: now_ms,
                        required_expiry_since: None,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // Put an already-expired pending request on channel A so expiring it arms tenant backoff.
    let expired_created_at =
        now_ms.saturating_sub(crate::ckb::held_cells::WATCHTOWER_HOLD_MS + 1_000);
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_a, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.insert(
                    Hash256::from([0xaa; 32]),
                    fiber_types::PendingWatchtowerRequest {
                        content: dummy_content(0xaa),
                        created_at_ms: expired_created_at,
                        required_expiry_since: None,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    let mut changes = fixture_b
        .events
        .hub
        .subscribe(SigningScope::Watchtower(node_id.clone()));
    let drain_events = |changes: &mut crate::signing::notifications::SigningChanges| {
        let mut collected = Vec::new();
        while let Some(Ok(ev)) = changes.recv().now_or_never() {
            collected.push(ev);
        }
        collected
    };
    let rpc = WatchtowerRpcServerImpl::new(fixture_b.store.clone())
        .with_signing_events(fixture_b.events.clone());
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let rpc_status_b = || {
        rt.block_on(rpc.get_watchtower_signing_status(
            RpcContext {
                node_id: node_id.to_string(),
                tenant_scoped: false,
            },
            GetWatchtowerSigningStatusParams {
                channel_id: channel_b.into(),
            },
        ))
        .expect("get_watchtower_signing_status(B)")
        .status
    };

    // 1. Channel A expires its request -> imposes tenant backoff. A subscriber watching ONLY
    //    channel B must receive WatchtowerUpdated(B, Backoff) matching get_watchtower_signing_status(B).
    expire_channel_external_signing_requests(
        &fixture_b.events,
        &fixture_b.store,
        &node_id,
        &channel_a,
        now_ms,
    );
    let events_after_arm = drain_events(&mut changes);
    let b_event_arm = events_after_arm
        .iter()
        .find_map(|ev| match ev.as_ref() {
            SigningChange::WatchtowerUpdated { channel_id, status } if *channel_id == channel_b => {
                Some(status.clone())
            }
            _ => None,
        })
        .expect("channel B must receive Backoff update when channel A arms tenant backoff");
    assert!(matches!(
        b_event_arm,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms,
            consecutive_expired_requests: 0,
        } if retry_after_ms > now_ms
    ));
    assert_eq!(b_event_arm, rpc_status_b());
    assert!(
        !events_after_arm
            .iter()
            .any(|ev| ev.channel_id() == channel_c),
        "channel C with active pending request must not receive spurious backoff update"
    );

    // 2. Channel A clears its backoff early -> channel B must receive NoSignatureRequired,
    //    matching get_watchtower_signing_status(B).
    clear_channel_external_retry_backoff(&fixture_b.events, &fixture_b.store, &node_id, &channel_a);
    let events_after_clear = drain_events(&mut changes);
    let b_event_clear = events_after_clear
        .iter()
        .find_map(|ev| match ev.as_ref() {
            SigningChange::WatchtowerUpdated { channel_id, status } if *channel_id == channel_b => {
                Some(status.clone())
            }
            _ => None,
        })
        .expect("channel B must receive NoSignatureRequired when channel A clears tenant backoff");
    assert_eq!(b_event_clear, WatchtowerSigningStatus::NoSignatureRequired);
    assert_eq!(b_event_clear, rpc_status_b());

    // 3. Channel A evicts a failed signed tx -> imposes tenant backoff again, and B receives Backoff.
    let failed_content = dummy_content(0xab);
    let failed_raw = failed_content.transaction.raw();
    fixture_b
        .store
        .update_watchtower_signer(&node_id, &channel_a, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.signed_signatures.insert(
                    Hash256::from([0xab; 32]),
                    fiber_types::SignedWatchtowerRequest {
                        content: failed_content.clone(),
                        signature: [0x11; 65],
                        created_at_ms: now_ms,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    evict_failed_signed_settlement_tx(
        &fixture_b.events,
        &fixture_b.store,
        &node_id,
        &channel_a,
        failed_raw.as_slice(),
        now_ms,
    );
    let events_after_evict = drain_events(&mut changes);
    let b_event_evict = events_after_evict
        .iter()
        .find_map(|ev| match ev.as_ref() {
            SigningChange::WatchtowerUpdated { channel_id, status } if *channel_id == channel_b => {
                Some(status.clone())
            }
            _ => None,
        })
        .expect("channel B must receive Backoff update when channel A evicts failed signed tx");
    assert!(matches!(
        b_event_evict,
        WatchtowerSigningStatus::Backoff {
            retry_after_ms,
            consecutive_expired_requests: 0,
        } if retry_after_ms > now_ms
    ));
    assert_eq!(b_event_evict, rpc_status_b());
}
