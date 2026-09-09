use ckb_sdk::traits::{CellCollectorError, LiveCell};
use ckb_types::{core::ScriptHashType, packed::Byte32};
use fiber_types::{
    OnchainKeyPurpose, OnchainSigningContent, SettlementTlc, WatchtowerExternalState,
    WatchtowerSignerState,
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
            local_keys.then(|| base_key.clone()),
            base_key.pubkey(),
            Privkey::from([10; 32]).pubkey(),
            Privkey::from([11; 32]).pubkey(),
            Privkey::from([12; 32]).pubkey(),
            settlement,
        );
        store.insert_watch_preimage(
            node_id.clone(),
            HashAlgorithm::Sha256.hash(preimage.as_ref()).into(),
            preimage,
        );
        let channel = store
            .get_watch_channel(&node_id, &channel_id)
            .expect("channel");
        let snapshot = settlement_data_for_commitment(&channel, for_remote, 10);
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
        let mut args = vec![0; 20];
        args.extend_from_slice(&since.to_le_bytes());
        args.extend_from_slice(&10u64.to_be_bytes());
        args.extend_from_slice(blake160(&witness_bytes).as_ref());
        let mut lock = Script::new_builder()
            .code_hash(Byte32::from([1; 32]))
            .hash_type(ScriptHashType::Type)
            .args(args.pack())
            .build();
        let mut tracked =
            tracked_settlement_tlcs(&lock, &channel, for_remote).expect("verified TLC identities");
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
        let signer =
            LocalSigner::new(secp256k1::SecretKey::from_slice(&[21; 32]).expect("fee key"));
        build_settlement_tx(
            self.cell.clone(),
            EpochNumberWithFraction::new(10, 0, 1),
            EpochNumberWithFraction::new(14, 0, 1),
            200_000,
            &self.node_id,
            self.for_remote,
            self.channel.clone(),
            self.witness.clone(),
            &self.tracked,
            &signer,
            &mut FeeCells,
            &self.store,
        )
    }

    fn pending(&self) -> (Hash256, OnchainSigningContent) {
        let WatchtowerSignerState::External(state) = self
            .store
            .get_watchtower_signer(&self.node_id, &self.channel.channel_id)
        else {
            panic!("external signer")
        };
        let WatchtowerExternalState::AwaitingSignature {
            request_id,
            content,
        } = state.state
        else {
            panic!("pending request")
        };
        (request_id, content)
    }

    fn submit(
        &self,
        request_id: Hash256,
        signature: [u8; 65],
    ) -> Result<SubmitWatchtowerSignatureResult, jsonrpsee::types::ErrorObjectOwned> {
        let rpc = WatchtowerRpcServerImpl::new(self.store.clone());
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
            panic!("external signer")
        };
        assert!(!matches!(
            state.state,
            WatchtowerExternalState::AwaitingSignature { .. }
        ));
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
    let state = fiber_types::WatchtowerExternalSignerState {
        state: WatchtowerExternalState::AwaitingSignature {
            request_id,
            content,
        },
        last_applied: None,
    };
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
            let snapshot = settlement_data_for_commitment(&fixture.channel, for_remote, 10);
            let witness_bytes = settlement_data_to_witness(
                snapshot,
                for_remote,
                fixture.base_key.pubkey(),
                fixture.channel.remote_settlement_key,
            );
            let output: CellOutput = fixture.cell.output.clone().into();
            let mut args = output.lock().args().raw_data()[..36].to_vec();
            args.extend_from_slice(blake160(&witness_bytes).as_ref());
            let lock = output.lock().as_builder().args(args.pack()).build();
            fixture.tracked = tracked_settlement_tlcs(&lock, &fixture.channel, for_remote)
                .expect("empty snapshot is verified");
            fixture.cell.output = output.as_builder().lock(lock).build().into();
            fixture.witness = subsequent.then(|| {
                SettlementWitness::build_from_witness(&[&[0], witness_bytes.as_slice()].concat())
                    .expect("settlement witness")
            });
            assert!(fixture.build().expect("queue settlement").is_none());
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
        let state = if already_signed {
            WatchtowerExternalState::Signed {
                request_id,
                content: stale,
                signature: wrong_signature,
            }
        } else {
            WatchtowerExternalState::AwaitingSignature {
                request_id,
                content: stale,
            }
        };
        fixture.store.put_watchtower_signer(
            &fixture.node_id,
            &fixture.channel.channel_id,
            WatchtowerSignerState::External(fiber_types::WatchtowerExternalSignerState {
                state,
                last_applied: already_signed.then_some(
                    fiber_types::LastAppliedWatchtowerSignature {
                        request_id,
                        signature: wrong_signature,
                    },
                ),
            }),
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
