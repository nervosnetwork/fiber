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
            local_amount: 100_000_000_000,
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
            None,
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
        let mut tracked = crate::fiber::onchain_tlc_reconcile::verified_tracked_settlement_tlcs(
            &lock, &channel, for_remote,
        )
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
        (request_id, req.content().clone())
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

    fn submit(
        &self,
        request_id: Hash256,
        signature: [u8; 65],
    ) -> Result<SubmitWatchtowerSignatureResult, jsonrpsee::types::ErrorObjectOwned> {
        self.submit_with_tx(request_id, signature, None)
    }

    fn submit_with_tx(
        &self,
        request_id: Hash256,
        signature: [u8; 65],
        signed_transaction: Option<Transaction>,
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
                    signed_transaction: signed_transaction.map(Into::into),
                },
            ))
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
fn external_request_reuses_reserved_fee_inputs() {
    #[derive(Clone)]
    struct NoMoreCells;
    #[async_trait::async_trait]
    impl CellCollector for NoMoreCells {
        async fn collect_live_cells_async(
            &mut self,
            _: &CellQueryOptions,
            _: bool,
        ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
            panic!("an outstanding external request must not select replacement fee inputs");
        }
        fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
            unreachable!()
        }
        fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
            unreachable!()
        }
        fn reset(&mut self) {}
    }
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let fixture = Fixture::new(for_remote, subsequent, false, false);
            assert!(fixture.build().unwrap().is_none());
            let (id, content) = fixture.pending();
            assert!(fixture
                .build_with_collector(200_001, &mut NoMoreCells)
                .unwrap()
                .is_none());
            assert_eq!(fixture.pending(), (id, content.clone()));
            let signature = sign_onchain_request(&fixture.tlc_key, &content).unwrap();
            fixture.submit(id, signature).unwrap();
            let tx = fixture
                .build_with_collector(200_002, &mut NoMoreCells)
                .unwrap()
                .unwrap();
            assert_applied_signature(&tx, &content, &fixture.tlc_key);
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
            let (_, content) = fixture.pending();
            let fee_since: u64 = content
                .transaction
                .raw()
                .inputs()
                .get(1)
                .expect("fee input")
                .since()
                .unpack();
            assert_eq!(
                fee_since,
                fiber_types::watchtower::settlement_timestamp_since(100_000)
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
                    assert!(fixture.build_at(200_000).expect("queue request").is_none());
                    let (request_id, content) = fixture.pending();
                    let signature =
                        sign_onchain_request(&fixture.tlc_key, &content).expect("TLC signature");
                    assert_eq!(
                        fixture.submit(request_id, signature).expect("submit"),
                        SubmitWatchtowerSignatureResult::Applied
                    );

                    // The next scan observes a later chain median time, but the
                    // same commitment, TLC and fee cell must reuse this signature.
                    let tx = fixture
                        .build_at(260_000)
                        .expect("next scan")
                        .expect("accepted signature must produce a signed transaction");
                    assert_applied_signature(&tx, &content, &fixture.tlc_key);
                    let expected_since = if with_preimage {
                        0
                    } else {
                        fiber_types::watchtower::settlement_timestamp_since(100_000)
                    };
                    let fee_since: u64 = tx.inputs().get(1).expect("fee input").since().unpack();
                    assert_eq!(fee_since, expected_since);
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
                let (request_id, content) = fixture.pending();
                assert_eq!(
                    content.key_purpose,
                    OnchainKeyPurpose::Tlc {
                        commitment_number: 7
                    }
                );
                assert!(fixture.build().expect("still pending").is_none());
                assert_eq!(fixture.pending(), (request_id, content.clone()));
                let wrong =
                    sign_onchain_request(&fixture.base_key, &content).expect("wrong-key signature");
                assert!(fixture.submit(request_id, wrong).is_err());
                assert_eq!(fixture.pending(), (request_id, content.clone()));
                let signature =
                    sign_onchain_request(&fixture.tlc_key, &content).expect("TLC signature");
                assert_eq!(
                    fixture.submit(request_id, signature).expect("submit"),
                    SubmitWatchtowerSignatureResult::Applied
                );
                assert_eq!(
                    fixture.submit(request_id, signature).expect("retry"),
                    SubmitWatchtowerSignatureResult::AlreadyApplied
                );
                let tx = fixture
                    .build()
                    .expect("apply signature")
                    .expect("signed tx");
                assert_applied_signature(&tx, &content, &fixture.tlc_key);
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
    let (request_id, mut content) = fixture.pending();
    content.key_purpose = OnchainKeyPurpose::Tlc {
        commitment_number: 99,
    };
    let signature = sign_onchain_request(&fixture.base_key, &content).expect("base-key signature");
    let mut state = fiber_types::WatchtowerExternalSignerState::default();
    state.pending_requests.insert(
        request_id,
        fiber_types::PendingWatchtowerRequest::OperatorSponsored {
            content,
            created_at_ms: crate::now_timestamp_as_millis_u64(),
        },
    );
    fixture.store.put_watchtower_signer(
        &fixture.node_id,
        &fixture.channel.channel_id,
        WatchtowerSignerState::External(state),
    );
    let err = fixture
        .submit(request_id, signature)
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
                            fiber_types::SignedWatchtowerRequest::OperatorSponsored {
                                content: content.clone(),
                                signature: sign_onchain_request(base_key, content).unwrap(),
                                created_at_ms: now_ms,
                            },
                        )
                    })
                    .collect();
            } else {
                external.pending_requests = requests
                    .iter()
                    .map(|(id, content)| {
                        (
                            *id,
                            fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                                content: content.clone(),
                                created_at_ms: now_ms,
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
                assert_eq!(state.signed_signatures[live_id].content(), live);
            } else {
                assert_eq!(state.pending_requests.len(), 1);
                assert_eq!(state.pending_requests[live_id].content(), live);
            }
            let expected = crate::rpc::watchtower::to_rpc_watchtower_signing_status(
                WatchtowerSignerState::External(state),
            );
            match &expected {
                WatchtowerSigningStatus::SignatureRequired { request_id, .. } => {
                    assert_eq!(*request_id, (*live_id).into())
                }
                WatchtowerSigningStatus::ExternalFundingRequired { .. } => {
                    panic!("unexpected ExternalFundingRequired for operator-sponsored test")
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
    let (request_id, content) = fixture.pending();
    let signature = sign_onchain_request(&fixture.tlc_key, &content).unwrap();
    fixture.submit(request_id, signature).unwrap();
    let spent = HashSet::from([content
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
    // must clear the historical receipt so the same request can be signed again.
    assert!(fixture.build().unwrap().is_none());
    assert_eq!(fixture.pending(), (request_id, content));
    assert_eq!(
        fixture.submit(request_id, signature).unwrap(),
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
                crate::fiber::onchain_tlc_reconcile::verified_tracked_settlement_tlcs(
                    &lock,
                    &fixture.channel,
                    for_remote,
                )
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
                    status: fiber_json_types::WatchtowerSigningStatus::SignatureRequired { .. },
                    ..
                }
            ));
            let (request_id, content) = fixture.pending();
            assert_eq!(content.key_purpose, OnchainKeyPurpose::Settlement);
            let wrong =
                sign_onchain_request(&fixture.tlc_key, &content).expect("wrong-key signature");
            assert!(fixture.submit(request_id, wrong).is_err());
            let signature =
                sign_onchain_request(&fixture.base_key, &content).expect("settlement signature");
            fixture
                .submit(request_id, signature)
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
            assert_applied_signature(&tx, &content, &fixture.base_key);
        }
    }
}

#[test]
fn external_tlc_signing_replaces_stale_request_purpose_for_same_transaction() {
    for already_signed in [false, true] {
        let fixture = Fixture::new(true, true, false, false);
        assert!(fixture.build().expect("queue").is_none());
        let (request_id, expected) = fixture.pending();
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
                fiber_types::SignedWatchtowerRequest::OperatorSponsored {
                    content: stale,
                    signature: wrong_signature,
                    created_at_ms: now_ms,
                },
            );
            state.last_applied = Some(fiber_types::LastAppliedWatchtowerSignature {
                request_id,
                signature: wrong_signature,
                signed_transaction_hash: None,
            });
        } else {
            state.pending_requests.insert(
                request_id,
                fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                    content: stale,
                    created_at_ms: now_ms,
                },
            );
        }
        fixture.store.put_watchtower_signer(
            &fixture.node_id,
            &fixture.channel.channel_id,
            WatchtowerSignerState::External(state),
        );
        assert!(fixture.build().expect("replace stale request").is_none());
        assert_eq!(fixture.pending(), (request_id, expected.clone()));
        let signature =
            sign_onchain_request(&fixture.tlc_key, &expected).expect("corrected signature");
        assert_eq!(
            fixture
                .submit(request_id, signature)
                .expect("submit corrected signature"),
            SubmitWatchtowerSignatureResult::Applied
        );
        let tx = fixture
            .build()
            .expect("apply corrected signature")
            .expect("signed tx");
        assert_applied_signature(&tx, &expected, &fixture.tlc_key);
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
    let (first_id, first_content) = fixture.pending();
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
    let (second_id, second_content) = state
        .pending_requests
        .iter()
        .find(|(id, _)| **id != first_id)
        .map(|(id, req)| (*id, req.content().clone()))
        .expect("second request");
    let first_sig =
        sign_onchain_request(&fixture.tlc_key, &first_content).expect("first signature");
    let second_sig =
        sign_onchain_request(&fixture.tlc_key, &second_content).expect("second signature");
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let first_submit = scope.spawn(|| {
            start.wait();
            fixture.submit(first_id, first_sig)
        });
        let second_submit = scope.spawn(|| {
            start.wait();
            second.submit(second_id, second_sig)
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
    assert_applied_signature(&first_tx, &first_content, &fixture.tlc_key);
    assert_applied_signature(&second_tx, &second_content, &fixture.tlc_key);
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
    let (req_1, content_1) = fixture.pending();

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
    let (req_2, content_2) = signer_state
        .pending_requests
        .iter()
        .find(|(k, _)| **k != req_1)
        .map(|(k, v)| (*k, v.content().clone()))
        .expect("second pending request");
    assert_ne!(
        req_1, req_2,
        "two distinct transactions must produce distinct request_ids"
    );

    // 3. Client signs req_1 and submits via RPC
    let sig_1 = sign_onchain_request(&fixture.tlc_key, &content_1).expect("TLC signature 1");
    let submit_1_res = fixture.submit(req_1, sig_1);

    // In current buggy code: submit_1_res fails with:
    // "signature request id does not match the current request"
    // With our fix: submit_1_res MUST succeed with SubmitWatchtowerSignatureResult::Applied!
    assert_eq!(
        submit_1_res.expect("submit req_1 must succeed without being overwritten"),
        SubmitWatchtowerSignatureResult::Applied
    );

    // 4. Client also signs req_2 and submits via RPC
    let sig_2 = sign_onchain_request(&fixture.tlc_key, &content_2).expect("TLC signature 2");
    let submit_2_res = fixture.submit(req_2, sig_2);
    assert_eq!(
        submit_2_res.expect("submit req_2 must succeed"),
        SubmitWatchtowerSignatureResult::Applied
    );

    // 5. Watchtower loop can now consume signatures for both transactions successfully
    let tx_1 = fixture
        .build()
        .expect("build signed tx 1")
        .expect("tx 1 signed");
    assert_applied_signature(&tx_1, &content_1, &fixture.tlc_key);

    let tx_2 = modified_fixture
        .build()
        .expect("build signed tx 2")
        .expect("tx 2 signed");
    assert_applied_signature(&tx_2, &content_2, &fixture.tlc_key);

    // 6. Test idempotency & persistence across scan cycles before on-chain confirmation:
    // Retrying submission for req_1 must return AlreadyApplied
    assert_eq!(
        fixture
            .submit(req_1, sig_1)
            .expect("retry submit req_1 after build"),
        SubmitWatchtowerSignatureResult::AlreadyApplied
    );

    // Re-building tx_1 before onchain confirmation must still return the signed tx and not re-ask
    let tx_1_second = fixture
        .build()
        .expect("build signed tx 1 again")
        .expect("tx 1 still signed");
    assert_applied_signature(&tx_1_second, &content_1, &fixture.tlc_key);
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
                let (id, content) = fixture.pending();
                let signature = sign_onchain_request(&fixture.tlc_key, &content).unwrap();
                assert_eq!(
                    fixture.submit(id, signature).unwrap(),
                    SubmitWatchtowerSignatureResult::Applied
                );
                let tx = fixture
                    .build_at(260_000)
                    .unwrap()
                    .expect("signed transaction");
                assert_applied_signature(&tx, &content, &fixture.tlc_key);
            }
        }
    }
}

#[test]
fn external_ckb_and_udt_settlement_uses_separate_payout_and_fee_sponsor_locks() {
    let tenant_payout_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0xaa; 20]);
    let fee_signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("key"));
    let sponsor_lock = get_script_by_contract(Contract::Secp256k1Lock, fee_signer.pubkey_hash());
    assert_ne!(tenant_payout_lock, sponsor_lock);

    for is_udt in [false, true] {
        let mut fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
        let udt_script = is_udt.then(|| {
            Script::new_builder()
                .code_hash(Byte32::from([0x77; 32]))
                .args([0x01u8; 32].as_slice().pack())
                .build()
        });
        let mut settlement_data = fixture.channel.local_settlement_data.clone();
        if is_udt {
            // For UDT channels with a separate fee sponsor, test local balance settlement
            // (funded from excess commitment cell CKB without any fee sponsor subsidy).
            settlement_data.tlcs.clear();
        }
        fixture
            .store
            .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
        fixture.store.insert_watch_channel(
            fixture.node_id.clone(),
            fixture.channel.channel_id,
            udt_script.clone(),
            WatchChannelKeys {
                local_settlement: SettlementKey::external(fixture.base_key.pubkey()),
                remote_settlement: fixture.channel.remote_settlement_key,
                local_funding: fixture.channel.local_funding_pubkey,
                remote_funding: fixture.channel.remote_funding_pubkey,
            },
            settlement_data,
            ChannelFeatures::V2,
            Some(tenant_payout_lock.clone()),
        );
        fixture.channel = fixture
            .store
            .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
            .expect("updated watch channel");
        assert_eq!(
            fixture.channel.payout_lock_script.as_ref(),
            Some(&tenant_payout_lock)
        );
        if let Some(udt) = udt_script.clone() {
            let witness_bytes = settlement_data_to_witness(
                &fixture.channel.local_settlement_data,
                fixture.for_remote,
                fixture.base_key.pubkey(),
                fixture.channel.remote_settlement_key,
            );
            let mut output: CellOutput = fixture.cell.output.clone().into();
            let mut args = output.lock().args().raw_data().to_vec();
            args[36..56].copy_from_slice(blake160(&witness_bytes).as_ref());
            let lock = output.lock().as_builder().args(args.pack()).build();
            // Provide enough excess CKB in the UDT commitment cell so the tenant's UDT
            // settlement output is funded solely from the commitment cell without any
            // subsidy from the operator's fee sponsor.
            output = output
                .as_builder()
                .lock(lock)
                .type_(Some(udt).pack())
                .capacity(40_000_000_000u64)
                .build();
            fixture.cell.output = output.into();
            fixture.cell.output_data = Some(ckb_jsonrpc_types::JsonBytes::from_vec(
                200_000_000_000u128.to_le_bytes().to_vec(),
            ));
            fixture.tracked.clear();
        }

        // Watchtower emits an unfunded ExternalFunding template tx for both CKB and UDT
        // settlements under a separate payout lock (zero operator fee inputs or cell_collector calls).
        #[derive(Clone)]
        struct PanicCollector;
        #[async_trait::async_trait]
        impl CellCollector for PanicCollector {
            async fn collect_live_cells_async(
                &mut self,
                _: &CellQueryOptions,
                _: bool,
            ) -> Result<(Vec<LiveCell>, u64), CellCollectorError> {
                panic!("external-funding template must never query operator cell_collector");
            }
            fn lock_cell(&mut self, _: OutPoint, _: u64) -> Result<(), CellCollectorError> {
                unreachable!()
            }
            fn apply_tx(&mut self, _: Transaction, _: u64) -> Result<(), CellCollectorError> {
                unreachable!()
            }
            fn reset(&mut self) {}
        }

        assert!(fixture
            .build_with_collector(200_000, &mut PanicCollector)
            .unwrap()
            .is_none());
        let (req_id, pending) = fixture.pending_request();
        assert!(pending.is_external_funding(), "is_udt={is_udt}");
        let content = pending.content();
        assert_eq!(content.transaction.raw().inputs().len(), 1);
        let outputs: Vec<_> = content.transaction.raw().outputs().into_iter().collect();
        let outputs_data: Vec<_> = content
            .transaction
            .raw()
            .outputs_data()
            .into_iter()
            .collect();
        assert_eq!(outputs.len(), 2, "is_udt={is_udt}");
        // Output 0: remaining commitment output.
        // Output 1: tenant settlement payout locked to payout_lock_script.
        assert_eq!(outputs[1].lock(), tenant_payout_lock);
        if is_udt {
            assert_eq!(outputs[1].type_().to_opt(), udt_script);
            assert_eq!(
                outputs_data[1].raw_data().as_ref(),
                100_000_000_000u128.to_le_bytes()
            );
            // High-excess UDT partial balance: wallet can deduct fee directly from outputs[1]
            // (1 input, 0 external wallet CKB needed!).
            let payout_cap: u64 = outputs[1].capacity().unpack();
            let funded_tx = content
                .transaction
                .clone()
                .into_view()
                .as_advanced_builder()
                .set_outputs(vec![
                    outputs[0].clone(),
                    outputs[1]
                        .clone()
                        .as_builder()
                        .capacity(payout_cap - 1_000)
                        .build(),
                ])
                .build()
                .data();
            let funded_content = OnchainSigningContent {
                key_purpose: content.key_purpose,
                transaction: funded_tx.clone(),
            };
            let sig = sign_onchain_request(&fixture.base_key, &funded_content).unwrap();
            assert_eq!(
                fixture
                    .submit_with_tx(req_id, sig, Some(funded_tx))
                    .unwrap(),
                SubmitWatchtowerSignatureResult::Applied
            );
            let final_tx = fixture
                .build_with_collector(200_000, &mut PanicCollector)
                .unwrap()
                .expect("signed 1-input UDT settlement tx");
            assert_eq!(final_tx.inputs().len(), 1);
            assert_eq!(final_tx.witnesses().len(), 1);
            assert_applied_signature(&final_tx, &funded_content, &fixture.base_key);
        } else {
            let cap: u64 = outputs[1].capacity().unpack();
            assert_eq!(cap, 20_000_000_000u64);
            assert!(outputs[1].type_().to_opt().is_none());
            // CKB timeout TLC: wallet attaches its own input with `since >= required_expiry_since`.
            let expiry_since = pending
                .required_expiry_since()
                .expect("CKB timeout TLC requires expiry since");
            let funded_tx = content
                .transaction
                .clone()
                .into_view()
                .as_advanced_builder()
                .input(CellInput::new(
                    OutPoint::new(Byte32::from([0xaa; 32]), 0),
                    expiry_since,
                ))
                .output(
                    CellOutput::new_builder()
                        .capacity(9_999_999_000u64)
                        .lock(tenant_payout_lock.clone())
                        .build(),
                )
                .output_data(ckb_types::packed::Bytes::default())
                .witness([0xdeu8, 0xad].pack())
                .build()
                .data();
            let funded_content = OnchainSigningContent {
                key_purpose: content.key_purpose,
                transaction: funded_tx.clone(),
            };
            let sig = sign_onchain_request(&fixture.tlc_key, &funded_content).unwrap();
            assert_eq!(
                fixture
                    .submit_with_tx(req_id, sig, Some(funded_tx))
                    .unwrap(),
                SubmitWatchtowerSignatureResult::Applied
            );
            let final_tx = fixture
                .build_with_collector(200_000, &mut PanicCollector)
                .unwrap()
                .expect("signed CKB timeout TLC tx");
            assert_eq!(final_tx.inputs().len(), 2);
            assert_eq!(
                final_tx.outputs().get(2).unwrap().lock(),
                tenant_payout_lock
            );
            assert_ne!(final_tx.outputs().get(2).unwrap().lock(), sponsor_lock);
            assert_applied_signature(&final_tx, &funded_content, &fixture.tlc_key);
        }
    }
}

#[test]
fn internal_udt_settlement_refuses_to_subsidize_from_separate_fee_sponsor() {
    let tenant_payout_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0xaa; 20]);
    let mut fixture = Fixture::with_features(false, false, false, true, ChannelFeatures::V2);
    let udt_script = Script::new_builder()
        .code_hash(Byte32::from([0x77; 32]))
        .args([0x01u8; 32].as_slice().pack())
        .build();
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
        fixture.channel.local_settlement_data.clone(),
        ChannelFeatures::V2,
        Some(tenant_payout_lock),
    );
    fixture.channel = fixture
        .store
        .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
        .expect("updated watch channel");
    // Only 200 CKB in the commitment cell: not enough excess CKB to create a ~142 CKB UDT output
    // without draining the separate fee sponsor.
    let mut output: CellOutput = fixture.cell.output.clone().into();
    output = output
        .as_builder()
        .type_(Some(udt_script).pack())
        .capacity(20_000_000_000u64)
        .build();
    fixture.cell.output = output.into();
    fixture.cell.output_data = Some(ckb_jsonrpc_types::JsonBytes::from_vec(
        240_000_000_000u128.to_le_bytes().to_vec(),
    ));
    // 1. UDT TLC unlock: CommitmentLock preserves full commitment cell capacity, so creating a
    // new UDT output would always drain the separate fee sponsor.
    let err = fixture.build_at(200_000).unwrap_err().to_string();
    assert!(
        err.contains("separate fee sponsor"),
        "expected separate fee sponsor refusal for TLC unlock, got: {err}"
    );

    // 2. UDT balance unlock with only 200 CKB in the commitment cell: not enough excess CKB to
    // create a ~142 CKB UDT output without draining the separate fee sponsor.
    let mut balance_only = fixture.channel.local_settlement_data.clone();
    balance_only.tlcs.clear();
    fixture
        .store
        .remove_watch_channel(fixture.node_id.clone(), fixture.channel.channel_id);
    fixture.store.insert_watch_channel(
        fixture.node_id.clone(),
        fixture.channel.channel_id,
        fixture.channel.funding_udt_type_script.clone(),
        WatchChannelKeys {
            local_settlement: SettlementKey::local(fixture.base_key.clone()),
            remote_settlement: fixture.channel.remote_settlement_key,
            local_funding: fixture.channel.local_funding_pubkey,
            remote_funding: fixture.channel.remote_funding_pubkey,
        },
        balance_only,
        ChannelFeatures::V2,
        fixture.channel.payout_lock_script.clone(),
    );
    fixture.channel = fixture
        .store
        .get_watch_channel(&fixture.node_id, &fixture.channel.channel_id)
        .expect("updated watch channel");
    let witness_bytes = settlement_data_to_witness(
        &fixture.channel.local_settlement_data,
        fixture.for_remote,
        fixture.base_key.pubkey(),
        fixture.channel.remote_settlement_key,
    );
    let mut output: CellOutput = fixture.cell.output.clone().into();
    let mut args = output.lock().args().raw_data().to_vec();
    args[36..56].copy_from_slice(blake160(&witness_bytes).as_ref());
    let lock = output.lock().as_builder().args(args.pack()).build();
    output = output.as_builder().lock(lock).build();
    fixture.cell.output = output.into();
    fixture.tracked.clear();
    let err = fixture.build_at(200_000).unwrap_err().to_string();
    assert!(
        err.contains("separate fee sponsor"),
        "expected separate fee sponsor refusal for balance unlock, got: {err}"
    );
}

#[test]
fn external_pending_request_expires_after_ttl_releases_fee_cell_and_enforces_backoff() {
    let _serial = crate::ckb::held_cells::TEST_SERIAL.blocking_lock();
    #[derive(Clone, Default)]
    struct LockTracker {
        locked: std::collections::HashSet<OutPoint>,
    }

    #[async_trait::async_trait]
    impl CellCollector for LockTracker {
        async fn collect_live_cells_async(
            &mut self,
            _query: &CellQueryOptions,
            _apply_changes: bool,
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

    let fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);

    let first = fixture.build().expect("build settlement");
    assert!(first.is_none(), "external signer pauses for signature");

    let channels = fixture.store.get_watch_channels_with_nodes();
    let (request_id, created_at_ms, fee_outpoint) = match fixture
        .store
        .get_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id)
    {
        fiber_types::WatchtowerSignerState::External(ext) => {
            assert_eq!(ext.pending_requests.len(), 1);
            let (&id, req) = ext.pending_requests.iter().next().expect("pending request");
            let created_at = req.created_at_ms();
            let fee_input = req
                .content()
                .transaction
                .raw()
                .inputs()
                .get(1)
                .expect("fee input")
                .previous_output();
            (id, created_at, fee_input)
        }
        other => panic!("expected external signer state, got {other:?}"),
    };

    // Mid-TTL sync must NOT renew created_at_ms or drop the request.
    let mid_ttl = created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS / 2;
    crate::watchtower::actor::sync_external_fee_holds(
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
                    .map(|r| r.created_at_ms()),
                Some(created_at_ms)
            );
            assert_eq!(ext.consecutive_expired_requests, 0);
        }
        other => panic!("expected external signer state, got {other:?}"),
    }

    // Past TTL: unsigned request must expire, release its fee cell, and arm backoff.
    let expired_at = created_at_ms + crate::ckb::held_cells::WATCHTOWER_HOLD_MS;
    crate::watchtower::actor::sync_external_fee_holds(
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

    // Fee cell is no longer held after expiry.
    let mut tracker = LockTracker::default();
    crate::ckb::held_cells::lock_held(&mut tracker, expired_at).expect("lock held");
    assert!(
        !tracker.locked.contains(&fee_outpoint),
        "expired request must release its fee cell"
    );

    // While in retry backoff (set into the future relative to real clock), build_settlement_tx
    // must refuse to create a new fee-holding request.
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
                "channel in backoff must not create a new pending fee-holding request"
            );
        }
        other => panic!("expected external signer state, got {other:?}"),
    }

    // Signed-but-unbroadcast signatures must also expire after WATCHTOWER_HOLD_MS and release their fee cell.
    let signed_id = Hash256::from([0xab; 32]);
    let signed_created_at = expired_at + 10_000;
    let signed_fee_outpoint = OutPoint::new(Byte32::from([0xcd; 32]), 0);
    let signed_tx = TransactionBuilder::default()
        .input(CellInput::new(OutPoint::new(Byte32::from([1; 32]), 0), 0))
        .input(CellInput::new(signed_fee_outpoint.clone(), 0))
        .build()
        .data();
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let fiber_types::WatchtowerSignerState::External(ext) = state {
                ext.signed_signatures.insert(
                    signed_id,
                    fiber_types::SignedWatchtowerRequest::OperatorSponsored {
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
    crate::watchtower::actor::sync_external_fee_holds(
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
    let mut tracker2 = LockTracker::default();
    crate::ckb::held_cells::lock_held(
        &mut tracker2,
        signed_created_at + crate::ckb::held_cells::WATCHTOWER_HOLD_MS,
    )
    .expect("lock held");
    assert!(
        !tracker2.locked.contains(&signed_fee_outpoint),
        "expired signed_signature must release its fee cell"
    );
}

#[test]
fn external_fee_hold_caps_and_cross_channel_per_node_backoff() {
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
            None,
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
                        fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                            content: dummy_content(10 + i),
                            created_at_ms: now_ms,
                        },
                    );
                }
                for i in 0..2u8 {
                    let id = Hash256::from([20 + i; 32]);
                    ext.signed_signatures.insert(
                        id,
                        fiber_types::SignedWatchtowerRequest::OperatorSponsored {
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
        "per-channel cap (pending + signed == 4) must block new fee-holding requests"
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

    // 3. Per-node cap (16): 16 active holds on sibling channel blocks fixture channel.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &sibling_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for i in 0..8u8 {
                    let id = Hash256::from([40 + i; 32]);
                    ext.pending_requests.insert(
                        id,
                        fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                            content: dummy_content(40 + i),
                            created_at_ms: now_ms,
                        },
                    );
                }
                for i in 0..8u8 {
                    let id = Hash256::from([50 + i; 32]);
                    ext.signed_signatures.insert(
                        id,
                        fiber_types::SignedWatchtowerRequest::OperatorSponsored {
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

    // 4. Global cap (64): 64 holds on another node blocks fixture channel; 63 allows it.
    fixture
        .store
        .update_watchtower_signer(&other_node, &other_channel, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                for i in 0..64u8 {
                    let id = Hash256::from([100 + i; 32]);
                    ext.pending_requests.insert(
                        id,
                        fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                            content: dummy_content(100 + i),
                            created_at_ms: now_ms,
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
        "once below global cap and outside own-node backoff, new fee-holding request succeeds"
    );
}

#[allow(clippy::too_many_arguments)]
fn build_separate_payout_ckb_fixture(
    for_remote: bool,
    subsequent: bool,
    local_amount: u128,
    remote_amount: u128,
    tlcs: Vec<SettlementTlc>,
    preimages: Vec<(Hash256, Hash256)>,
    payout_lock: Script,
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
        Some(payout_lock),
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
    let mut tracked = crate::fiber::onchain_tlc_reconcile::verified_tracked_settlement_tlcs(
        &lock, &channel, for_remote,
    )
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

#[test]
fn separate_sponsor_ckb_settlement_skips_small_tlc_and_claims_larger_tlc() {
    let tenant_payout_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0xaa; 20]);
    let fee_signer = LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("key"));
    let sponsor_lock = get_script_by_contract(Contract::Secp256k1Lock, fee_signer.pubkey_hash());
    assert_ne!(tenant_payout_lock, sponsor_lock);

    let min_payout_capacity = CellOutput::new_builder()
        .lock(tenant_payout_lock.clone())
        .build()
        .occupied_capacity(Capacity::shannons(0))
        .expect("occupied capacity")
        .as_u64();
    let small_amount = (min_payout_capacity - 1) as u128;
    let large_amount = 20_000_000_000u128;

    let base_key = Privkey::from([42; 32]);
    let small_tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([6; 32]).pubkey());
    let large_tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([7; 32]).pubkey());
    let small_preimage = Hash256::from([11; 32]);
    let large_preimage = Hash256::from([13; 32]);
    let small_hash: Hash256 = HashAlgorithm::Sha256.hash(small_preimage.as_ref()).into();
    let large_hash: Hash256 = HashAlgorithm::Sha256.hash(large_preimage.as_ref()).into();

    // Operator-sponsored (`local_keys = true`, `separate_fee_sponsor = true`) must skip under-capacity
    // CKB TLCs instead of subsidizing them from the operator fee wallet.
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            for with_preimage in [false, true] {
                let offered = for_remote != with_preimage;
                let small_tlc = SettlementTlc {
                    tlc_id: if offered {
                        TLCId::Offered(0)
                    } else {
                        TLCId::Received(0)
                    },
                    hash_algorithm: HashAlgorithm::Sha256,
                    payment_amount: small_amount,
                    payment_hash: small_hash,
                    expiry: 100_000,
                    local_key: Some(small_tlc_key.clone()),
                    local_key_pubkey: Some(small_tlc_key.pubkey()),
                    local_key_commitment_number: None,
                    remote_key: Privkey::from([12; 32]).pubkey(),
                };
                let large_tlc = SettlementTlc {
                    tlc_id: if offered {
                        TLCId::Offered(1)
                    } else {
                        TLCId::Received(1)
                    },
                    hash_algorithm: HashAlgorithm::Sha256,
                    payment_amount: large_amount,
                    payment_hash: large_hash,
                    expiry: 100_000,
                    local_key: Some(large_tlc_key.clone()),
                    local_key_pubkey: Some(large_tlc_key.pubkey()),
                    local_key_commitment_number: None,
                    remote_key: Privkey::from([12; 32]).pubkey(),
                };
                let preimages = if with_preimage {
                    vec![(small_hash, small_preimage), (large_hash, large_preimage)]
                } else {
                    vec![]
                };
                let fixture = build_separate_payout_ckb_fixture(
                    for_remote,
                    subsequent,
                    100_000_000_000,
                    100_000_000_000,
                    vec![small_tlc, large_tlc],
                    preimages,
                    tenant_payout_lock.clone(),
                    true,
                );

                // Must skip the first under-capacity TLC and claim the second TLC directly.
                let tx = fixture
                    .build_at(200_000)
                    .expect("small TLC must not block larger TLC")
                    .expect("signed tx");

                let witness_bytes = tx
                    .witnesses()
                    .get(0)
                    .expect("settlement witness")
                    .raw_data();
                let parsed = SettlementWitness::build_from_witness(
                    &witness_bytes[XUDT_COMPATIBLE_WITNESS.len()..],
                )
                .expect("parsed witness");
                let unlock = parsed.unlocks.last().expect("unlock");
                assert_eq!(unlock.unlock_type, 1);
                assert_eq!(unlock.with_preimage, with_preimage);

                let outputs: Vec<_> = tx.outputs().into_iter().collect();
                assert_eq!(outputs.len(), 3);
                assert_eq!(outputs[1].lock(), tenant_payout_lock);
                assert_eq!(
                    Unpack::<u64>::unpack(&outputs[1].capacity()),
                    large_amount as u64
                );
                assert_eq!(outputs[2].lock(), sponsor_lock);
            }
        }
    }

    // Mixed initial settlement: small timeout TLC at index 0 followed by large preimage TLC at
    // index 1. Skipping index 0 must leave `delay_epoch` intact so index 1 uses 1/3 delay_epoch.
    for for_remote in [false, true] {
        let small_timeout_tlc = SettlementTlc {
            tlc_id: if for_remote {
                TLCId::Offered(0)
            } else {
                TLCId::Received(0)
            },
            hash_algorithm: HashAlgorithm::Sha256,
            payment_amount: small_amount,
            payment_hash: small_hash,
            expiry: 100_000,
            local_key: Some(small_tlc_key.clone()),
            local_key_pubkey: Some(small_tlc_key.pubkey()),
            local_key_commitment_number: None,
            remote_key: Privkey::from([12; 32]).pubkey(),
        };
        let large_preimage_tlc = SettlementTlc {
            tlc_id: if for_remote {
                TLCId::Received(1)
            } else {
                TLCId::Offered(1)
            },
            hash_algorithm: HashAlgorithm::Sha256,
            payment_amount: large_amount,
            payment_hash: large_hash,
            expiry: 500_000,
            local_key: Some(large_tlc_key.clone()),
            local_key_pubkey: Some(large_tlc_key.pubkey()),
            local_key_commitment_number: None,
            remote_key: Privkey::from([12; 32]).pubkey(),
        };
        let fixture = build_separate_payout_ckb_fixture(
            for_remote,
            false,
            100_000_000_000,
            100_000_000_000,
            vec![small_timeout_tlc, large_preimage_tlc],
            vec![(large_hash, large_preimage)],
            tenant_payout_lock.clone(),
            true,
        );
        let tx = fixture
            .build_at(200_000)
            .expect("mixed claim build")
            .expect("signed tx");
        // Commitment delay_epoch is 3 epochs; preimage unlock must use 1/3 * 3 = 1 epoch.
        let commitment_since: u64 = tx
            .inputs()
            .get(0)
            .expect("commitment input")
            .since()
            .unpack();
        let expected_delay = mul(EpochNumberWithFraction::new(3, 0, 1), 1, 3).expect("1/3 delay");
        let expected_since = Since::new(
            SinceType::EpochNumberWithFraction,
            expected_delay.full_value(),
            true,
        )
        .value();
        assert_eq!(commitment_since, expected_since);
    }
}

#[test]
fn separate_sponsor_ckb_settlement_prevents_under_capacity_output_when_only_small_claim_eligible() {
    let tenant_payout_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0xaa; 20]);
    let min_payout_capacity = CellOutput::new_builder()
        .lock(tenant_payout_lock.clone())
        .build()
        .occupied_capacity(Capacity::shannons(0))
        .expect("occupied capacity")
        .as_u64();
    let small_amount = (min_payout_capacity - 1) as u128;

    let base_key = Privkey::from([42; 32]);
    let small_tlc_key =
        fiber_types::channel::derive_private_key(&base_key, &Privkey::from([6; 32]).pubkey());
    let small_preimage = Hash256::from([11; 32]);
    let small_hash: Hash256 = HashAlgorithm::Sha256.hash(small_preimage.as_ref()).into();

    // 1. Only a small timeout TLC exists under operator fee sponsorship (`local_keys = true`):
    //    - Before expiry (current_time <= expiry): returns Ok(None).
    //    - After expiry (current_time > expiry): skipping the under-capacity timeout TLC
    //      decrements pending_tlcs_count so watchtower falls through to 0xFF / 0xFE balance sweep.
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let small_tlc = SettlementTlc {
                tlc_id: if for_remote {
                    TLCId::Offered(0)
                } else {
                    TLCId::Received(0)
                },
                hash_algorithm: HashAlgorithm::Sha256,
                payment_amount: small_amount,
                payment_hash: small_hash,
                expiry: 100_000,
                local_key: Some(small_tlc_key.clone()),
                local_key_pubkey: Some(small_tlc_key.pubkey()),
                local_key_commitment_number: None,
                remote_key: Privkey::from([12; 32]).pubkey(),
            };
            let fixture = build_separate_payout_ckb_fixture(
                for_remote,
                subsequent,
                100_000_000_000,
                100_000_000_000,
                vec![small_tlc],
                vec![],
                tenant_payout_lock.clone(),
                true,
            );
            assert!(fixture
                .build_at(50_000)
                .expect("unexpired small-only timeout TLC must not error")
                .is_none());

            // After expiry, skipping the small timeout TLC falls through to 0xFF / 0xFE balance sweep.
            let tx = fixture
                .build_at(200_000)
                .expect("expired small timeout TLC falls through to balance settlement")
                .expect("signed balance sweep tx");
            let outputs: Vec<_> = tx.outputs().into_iter().collect();
            assert_eq!(outputs[1].lock(), tenant_payout_lock);
            assert_eq!(
                Unpack::<u64>::unpack(&outputs[1].capacity()),
                100_000_000_000
            );
            let witness_bytes = tx
                .witnesses()
                .get(0)
                .expect("settlement witness")
                .raw_data();
            let settlement_witness = SettlementWitness::build_from_witness(
                &witness_bytes[XUDT_COMPATIBLE_WITNESS.len()..],
            )
            .expect("parse settlement witness");
            let expected_unlock_type = if for_remote { 0xFF } else { 0xFE };
            assert_eq!(
                settlement_witness
                    .unlocks
                    .last()
                    .expect("unlock")
                    .unlock_type,
                expected_unlock_type
            );
        }
    }

    // 2. Only a small preimage TLC is eligible BEFORE expiry -> returns Ok(None).
    //    Once it expires (current_time > expiry), it falls through to local balance settlement.
    for for_remote in [false, true] {
        for subsequent in [false, true] {
            let small_tlc = SettlementTlc {
                tlc_id: if for_remote {
                    TLCId::Received(0)
                } else {
                    TLCId::Offered(0)
                },
                hash_algorithm: HashAlgorithm::Sha256,
                payment_amount: small_amount,
                payment_hash: small_hash,
                expiry: 150_000,
                local_key: Some(small_tlc_key.clone()),
                local_key_pubkey: Some(small_tlc_key.pubkey()),
                local_key_commitment_number: None,
                remote_key: Privkey::from([12; 32]).pubkey(),
            };
            let fixture = build_separate_payout_ckb_fixture(
                for_remote,
                subsequent,
                100_000_000_000,
                100_000_000_000,
                vec![small_tlc],
                vec![(small_hash, small_preimage)],
                tenant_payout_lock.clone(),
                true,
            );
            assert!(fixture
                .build_at(100_000)
                .expect("unexpired small preimage TLC must not error")
                .is_none());

            let tx = fixture
                .build_at(200_000)
                .expect("expired small preimage TLC falls through to balance settlement")
                .expect("signed balance sweep tx");
            let outputs: Vec<_> = tx.outputs().into_iter().collect();
            assert_eq!(outputs[1].lock(), tenant_payout_lock);
            assert_eq!(
                Unpack::<u64>::unpack(&outputs[1].capacity()),
                100_000_000_000
            );
        }
    }

    // 3. Safety capacity check remains intact when local balance itself is below occupied capacity
    // under operator fee sponsorship (`local_keys = true`).
    let fixture = build_separate_payout_ckb_fixture(
        false,
        false,
        small_amount,
        100_000_000_000,
        vec![],
        vec![],
        tenant_payout_lock,
        true,
    );
    let err = fixture
        .build_at(200_000)
        .expect_err("under-capacity balance payout must still be rejected")
        .to_string();
    assert!(
        err.contains("smaller than occupied capacity"),
        "expected occupied capacity check error, got: {err}"
    );
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

fn udt_external_funding_fixture(
    with_preimage: bool,
    clear_tlcs: bool,
    payout_lock: Script,
) -> Fixture {
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
        Some(payout_lock),
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
    template
        .clone()
        .into_view()
        .as_advanced_builder()
        .input(CellInput::new(outpoint, since))
        .output(
            CellOutput::new_builder()
                .capacity(6_000_000_000u64)
                .lock(change_lock)
                .build(),
        )
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
            .submit_with_tx(req_id, sig, Some(funded_tx.clone()))
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
        let fixture = build_separate_payout_ckb_fixture(
            false,
            false,
            local_amount,
            100_000_000_000,
            tlcs,
            preimages,
            tenant_payout_lock.clone(),
            false,
        );
        assert!(fixture
            .build_with_collector(at_time, &mut PanicCollector)
            .expect("small CKB claim must emit ExternalFundingRequired")
            .is_none());
        let (req_id, pending) = fixture.pending_request();
        assert!(pending.is_external_funding());
        assert_eq!(pending.content().key_purpose, expected_purpose);
        let tmpl = &pending.content().transaction;
        let outs: Vec<_> = tmpl.raw().outputs().into_iter().collect();
        assert_eq!(outs.len(), 2);
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

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test]
async fn watchtower_actor_pre_start_restores_external_fee_holds_before_periodic_check() {
    let _serial = crate::ckb::held_cells::TEST_SERIAL.lock().await;
    let fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
    let now_ms = crate::now_timestamp_as_millis_u64();

    let pending_id = Hash256::from([0xf1; 32]);
    let pending_fee_outpoint = OutPoint::new(Byte32::from([0xf1; 32]), 1);
    let pending_content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0xc1; 32]), 0),
                0,
            ))
            .input(CellInput::new(pending_fee_outpoint.clone(), 0))
            .build()
            .data(),
    };

    let signed_id = Hash256::from([0xf2; 32]);
    let signed_fee_outpoint = OutPoint::new(Byte32::from([0xf2; 32]), 1);
    let signed_content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0xc2; 32]), 0),
                0,
            ))
            .input(CellInput::new(signed_fee_outpoint.clone(), 0))
            .build()
            .data(),
    };

    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.pending_requests.insert(
                    pending_id,
                    fiber_types::PendingWatchtowerRequest::OperatorSponsored {
                        content: pending_content,
                        created_at_ms: now_ms,
                    },
                );
                ext.signed_signatures.insert(
                    signed_id,
                    fiber_types::SignedWatchtowerRequest::OperatorSponsored {
                        content: signed_content,
                        signature: [0u8; 65],
                        created_at_ms: now_ms,
                    },
                );
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();

    // Simulate process restart: in-memory holds are cleared before WatchtowerActor starts.
    crate::ckb::held_cells::release_for_watchtower([
        pending_fee_outpoint.clone(),
        signed_fee_outpoint.clone(),
    ]);

    let mut before_start = LockTracker::default();
    crate::ckb::held_cells::lock_held(&mut before_start, now_ms).unwrap();
    assert!(!before_start.locked.contains(&pending_fee_outpoint));
    assert!(!before_start.locked.contains(&signed_fee_outpoint));

    let ckb_dir = tempfile::tempdir().expect("ckb temp dir");
    std::fs::write(ckb_dir.path().join("key"), "01".repeat(32)).expect("write plain key");
    static INIT_KEY_PASSWORD: std::sync::Once = std::sync::Once::new();
    INIT_KEY_PASSWORD.call_once(|| {
        if std::env::var_os("FIBER_SECRET_KEY_PASSWORD").is_none() {
            std::env::set_var("FIBER_SECRET_KEY_PASSWORD", "watchtower-test-password");
        }
    });
    let ckb_config = CkbConfig {
        base_dir: Some(ckb_dir.path().to_path_buf()),
        rpc_url: "http://127.0.0.1:8114".to_string(),
        tx_tracing_polling_interval_ms: 4_000,
        udt_whitelist: None,
        funding_tx_shell_builder: None,
    };

    // Spawn WatchtowerActor without sending PeriodicCheck; pre_start alone must restore holds.
    let (actor, handle) = Actor::spawn(
        None,
        WatchtowerActor::new(fixture.store.clone()).with_signing_events(fixture.events.clone()),
        ckb_config,
    )
    .await
    .expect("spawn WatchtowerActor");

    let mut after_start = LockTracker::default();
    crate::ckb::held_cells::lock_held(&mut after_start, now_ms).unwrap();
    assert!(
        after_start.locked.contains(&pending_fee_outpoint),
        "WatchtowerActor::pre_start must hold persisted pending request fee cells before PeriodicCheck"
    );
    assert!(
        after_start.locked.contains(&signed_fee_outpoint),
        "WatchtowerActor::pre_start must hold persisted signed request fee cells before PeriodicCheck"
    );

    actor.stop(None);
    let _ = handle.await;
    crate::ckb::held_cells::release_for_watchtower([pending_fee_outpoint, signed_fee_outpoint]);
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
        let fixture =
            udt_external_funding_fixture(with_preimage, is_balance, custom_payout_lock.clone());

        // 1. Watchtower emits unfunded ExternalFundingRequired template with ZERO cell_collector calls.
        assert!(fixture
            .build_with_collector(200_000, &mut PanicCollector)
            .expect("template build must succeed without cell_collector")
            .is_none());
        let (request_id, pending) = fixture.pending_request();
        assert!(pending.is_external_funding());
        let expected_expiry =
            (scenario == 0).then(|| fiber_types::watchtower::settlement_timestamp_since(100_000));
        assert_eq!(pending.required_expiry_since(), expected_expiry);
        assert_eq!(pending.content().transaction.raw().inputs().len(), 1);
        assert_eq!(pending.content().transaction.raw().outputs().len(), 2);
        assert_eq!(
            pending
                .content()
                .transaction
                .raw()
                .outputs()
                .get(1)
                .unwrap()
                .lock(),
            custom_payout_lock
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

        // 2. Wallet builds the funded tx (custom lock cell_dep + funding input + change + 2 witnesses).
        let (signing_key, wrong_key) = if is_balance {
            (&fixture.base_key, &fixture.tlc_key)
        } else {
            (&fixture.tlc_key, &fixture.base_key)
        };
        let wallet_funding_outpoint = OutPoint::new(Byte32::from([0x71 + scenario as u8; 32]), 0);
        let funded_tx = fund_wallet_tx(
            &pending.content().transaction,
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
            key_purpose: pending.content().key_purpose,
            transaction: funded_tx.clone(),
        };
        let valid_sig = sign_onchain_request(signing_key, &funded_content).expect("sign funded");

        // 3. Negative checks: missing tx, wrong key, tampered outputs[0], wrong since.
        assert!(fixture
            .submit_with_tx(request_id, valid_sig, None)
            .expect_err("missing signed_transaction")
            .message()
            .contains("signed_transaction"));
        let wrong_sig = sign_onchain_request(wrong_key, &funded_content).unwrap();
        assert!(fixture
            .submit_with_tx(request_id, wrong_sig, Some(funded_tx.clone()))
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
        for invalid_tx in [tampered_out0, bad_since_tx] {
            let sig = sign_onchain_request(
                signing_key,
                &OnchainSigningContent {
                    key_purpose: pending.content().key_purpose,
                    transaction: invalid_tx.clone(),
                },
            )
            .unwrap();
            assert!(fixture
                .submit_with_tx(request_id, sig, Some(invalid_tx))
                .is_err());
        }

        // 4. Valid submit + idempotency (zero placeholder & pre-filled witness[0]) + diff witness rejection.
        assert_eq!(
            fixture
                .submit_with_tx(request_id, valid_sig, Some(funded_tx.clone()))
                .unwrap(),
            SubmitWatchtowerSignatureResult::Applied
        );
        let filled_w0_tx = apply_settlement_signature(
            BuiltSettlementTx::ExternalFunding(funded_tx.clone().into_view()),
            &LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).unwrap()),
            valid_sig,
            with_preimage,
        )
        .unwrap()
        .into_transaction()
        .data();
        for retry_tx in [funded_tx.clone(), filled_w0_tx] {
            assert_eq!(
                fixture
                    .submit_with_tx(request_id, valid_sig, Some(retry_tx))
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
        assert!(fixture
            .submit_with_tx(request_id, valid_sig, Some(diff_witness_tx.clone()))
            .is_err());

        // 5. Final tx preserves all client cell_deps & witness[1..] without calling cell_collector.
        let final_tx = fixture
            .build_with_collector(220_000, &mut PanicCollector)
            .unwrap()
            .expect("signed external-funding tx");
        assert_applied_signature(&final_tx, &funded_content, signing_key);
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

        // 6. After confirmed spend pruning, last_applied still serves exact AlreadyApplied & rejects diffs.
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
                .submit_with_tx(request_id, valid_sig, Some(funded_tx))
                .unwrap(),
            SubmitWatchtowerSignatureResult::AlreadyApplied
        );
        assert!(fixture
            .submit_with_tx(request_id, valid_sig, Some(diff_witness_tx))
            .is_err());
    }

    // 7. Operator-sponsored request rejects a spurious signed_transaction.
    let operator_fixture = Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
    assert!(operator_fixture.build().unwrap().is_none());
    let (op_req_id, op_content) = operator_fixture.pending();
    let op_sig = sign_onchain_request(&operator_fixture.tlc_key, &op_content).unwrap();
    assert!(operator_fixture
        .submit_with_tx(op_req_id, op_sig, Some(op_content.transaction.clone()))
        .expect_err("operator-sponsored request must reject signed_transaction")
        .message()
        .contains("only accepted for external-funding"));

    // 8. Hosted tenant (`node_id != NodeId::local()`) even with `payout_lock_script: None`
    //    must emit ExternalFundingRequired for plain CKB settlements.
    let mut tenant_fixture =
        Fixture::with_features(false, false, false, false, ChannelFeatures::V2);
    let hosted_tenant = NodeId::from_bytes(Privkey::from([0x7b; 32]).pubkey().serialize().to_vec());
    tenant_fixture.node_id = hosted_tenant.clone();
    tenant_fixture.store.insert_watch_channel(
        hosted_tenant,
        tenant_fixture.channel.channel_id,
        None,
        WatchChannelKeys {
            local_settlement: SettlementKey::external(tenant_fixture.base_key.pubkey()),
            remote_settlement: tenant_fixture.channel.remote_settlement_key,
            local_funding: tenant_fixture.channel.local_funding_pubkey,
            remote_funding: tenant_fixture.channel.remote_funding_pubkey,
        },
        tenant_fixture.channel.local_settlement_data.clone(),
        ChannelFeatures::V2,
        None,
    );
    tenant_fixture.channel = tenant_fixture
        .store
        .get_watch_channel(&tenant_fixture.node_id, &tenant_fixture.channel.channel_id)
        .expect("hosted tenant watch channel");
    assert!(tenant_fixture
        .build_with_collector(200_000, &mut PanicCollector)
        .unwrap()
        .is_none());
    let (_, tenant_pending) = tenant_fixture.pending_request();
    assert!(tenant_pending.is_external_funding());
    assert_eq!(tenant_pending.content().transaction.raw().inputs().len(), 1);
}

#[test]
fn external_funding_watchtower_request_never_touches_operator_held_cells() {
    let _serial = crate::ckb::held_cells::TEST_SERIAL.blocking_lock();
    let payout_lock = test_custom_payout_lock();
    let fixture = udt_external_funding_fixture(true, false, payout_lock.clone());

    assert!(fixture.build_at(200_000).unwrap().is_none());
    let (request_id, pending) = fixture.pending_request();
    assert!(pending.is_external_funding());

    let now_ms = crate::now_timestamp_as_millis_u64();
    let operator_utxo = OutPoint::new(Byte32::from([0xd1; 32]), 0);
    let unheld_utxo = OutPoint::new(Byte32::from([0xd2; 32]), 0);
    crate::ckb::held_cells::release_for_watchtower([operator_utxo.clone(), unheld_utxo.clone()]);
    crate::ckb::held_cells::hold_for_watchtower([operator_utxo.clone()], now_ms);

    let funded_tx = fund_wallet_tx(
        &pending.content().transaction,
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
        pending.content().key_purpose,
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
    crate::watchtower::actor::sync_external_fee_holds(
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
                    *req.created_at_ms_mut() = 1;
                }
            }
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap();
    crate::watchtower::actor::sync_external_fee_holds(
        &fixture.events,
        &fixture.store,
        &channels,
        now_ms + 2_000,
    );
    assert!(is_held(&operator_utxo, now_ms + 2_000));

    // Removing the watch channel with an ExternalFunding signed entry must not release operator_utxo.
    fixture
        .store
        .update_watchtower_signer(&fixture.node_id, &fixture.channel.channel_id, |state| {
            if let WatchtowerSignerState::External(ext) = state {
                ext.signed_signatures.insert(
                    request_id,
                    fiber_types::SignedWatchtowerRequest::ExternalFunding {
                        content: OnchainSigningContent {
                            key_purpose: pending.content().key_purpose,
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
    let fixture = udt_external_funding_fixture(false, false, payout_lock.clone());

    assert!(fixture.build_at(200_000).unwrap().is_none());
    let (request_id, pending) = fixture.pending_request();
    let required_expiry = pending.required_expiry_since().expect("timeout expiry");

    let wallet_witness_1 = [0xdeu8, 0xad, 0xbe, 0xef];
    let funded_tx = fund_wallet_tx(
        &pending.content().transaction,
        OutPoint::new(Byte32::from([0xe3; 32]), 0),
        required_expiry,
        payout_lock,
        &wallet_witness_1,
    );
    let funded_content = OnchainSigningContent {
        key_purpose: pending.content().key_purpose,
        transaction: funded_tx.clone(),
    };
    sign_and_submit_funded(
        &fixture,
        request_id,
        &fixture.tlc_key,
        pending.content().key_purpose,
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
            pending.content().transaction.clone().into_view(),
            pending.content().key_purpose,
            false,
            None,
            &fee_signer,
            existing_id,
            fiber_types::WatchtowerFunding::ExternalFunding {
                required_expiry_since: Some(required_expiry),
            },
        )
        .unwrap()
        .expect("racing scan must apply accepted funded tx")
        .into_transaction();
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
}

#[test]
fn external_funding_eviction_clears_last_applied_while_confirmed_spend_preserves_it() {
    let payout_lock = test_custom_payout_lock();
    // Modes:
    //   0: TTL expiration (`sync_external_fee_holds`) -> clears last_applied
    //   1: unconfirmed dead-input pruning (`evict_dead_fee_signing_requests`) -> clears last_applied
    //   2: broadcast failure (`evict_failed_signed_settlement_tx`) -> clears last_applied
    //   3: confirmed spend (`prune_spent_signing_requests`) then dead-input check -> preserves last_applied
    for mode in 0..4 {
        let fixture = udt_external_funding_fixture(false, false, payout_lock.clone());
        assert!(fixture.build_at(200_000).unwrap().is_none());
        let (request_id, pending) = fixture.pending_request();
        let required_expiry = pending.required_expiry_since().expect("timeout expiry");
        let funded_tx = fund_wallet_tx(
            &pending.content().transaction,
            OutPoint::new(Byte32::from([0xe4; 32]), 0),
            required_expiry,
            payout_lock.clone(),
            &[0x55],
        );
        let sig = sign_and_submit_funded(
            &fixture,
            request_id,
            &fixture.tlc_key,
            pending.content().key_purpose,
            &funded_tx,
        );

        match mode {
            0 => {
                let channels = fixture.store.get_watch_channels_with_nodes();
                let expired_at = crate::now_timestamp_as_millis_u64()
                    + crate::ckb::held_cells::WATCHTOWER_HOLD_MS
                    + 10_000;
                crate::watchtower::actor::sync_external_fee_holds(
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
                    .submit_with_tx(request_id, sig, Some(funded_tx))
                    .expect("idempotent retry after confirmed spend"),
                SubmitWatchtowerSignatureResult::AlreadyApplied
            );
        } else {
            assert!(ext.last_applied.is_none(), "mode={mode}");
            assert!(fixture
                .submit_with_tx(request_id, sig, Some(funded_tx))
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
            Some(payout_lock.clone()),
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
        assert!(pending.is_external_funding());
        assert_eq!(pending.required_expiry_since(), None);
        let template = &pending.content().transaction;
        assert_eq!(template.raw().inputs().len(), 1);
        assert_eq!(template.raw().outputs().len(), 1);

        // Pay fee directly from the single payout output (1 input, 1 output).
        let payout = template.raw().outputs().get(0).unwrap();
        let payout_cap: u64 = payout.capacity().unpack();
        let funded_tx = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![payout
                .as_builder()
                .capacity(payout_cap - 1_000)
                .build()])
            .build()
            .data();
        sign_and_submit_funded(
            &fixture,
            request_id,
            &fixture.base_key,
            pending.content().key_purpose,
            &funded_tx,
        );

        let built = fixture.build_at(200_000).unwrap().expect("signed tx");
        assert_eq!(built.data().raw().as_slice(), funded_tx.raw().as_slice());
        assert_eq!(built.outputs().len(), 1);
        assert_eq!(
            Unpack::<u64>::unpack(&built.outputs().get(0).unwrap().capacity()),
            payout_cap - 1_000
        );
    }
}
