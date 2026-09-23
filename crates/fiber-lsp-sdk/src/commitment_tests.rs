//! Adversarial LSP fixtures. Transactions are rehashed after malicious changes, so these
//! tests exercise local authorization rather than merely comparing two node-supplied hashes.
use ckb_types::{
    core::TransactionBuilder,
    packed::{CellInput, CellOutput, OutPoint, Script, Transaction},
    prelude::*,
};
use fiber_types::{
    blake2b_hash_with_salt, settlement_witness_hash, try_derive_tlc_pubkey,
    ChannelSigningTransition, HashAlgorithm, InMemorySigner, SettlementData, SettlementTlc, TLCId,
};
use musig2::{AggNonce, KeyAggContext};

use crate::*;

pub(crate) struct Fixture {
    pub root: RootSigner<MemoryStore>,
    pub signer: ChannelSigner<MemoryStore>,
    pub store: MemoryStore,
    pub remote: InMemorySigner,
    pub funding: Transaction,
    pub parameters: CommitmentParameters,
}

impl Fixture {
    pub async fn new(udt: bool) -> Self {
        let store = MemoryStore::default();
        let root = RootSigner::create(RootKey::import([42; 32]).unwrap(), store.clone())
            .await
            .unwrap();
        let signer = root.create_channel().await.unwrap();
        let remote = InMemorySigner::generate_from_seed(b"commitment counterparty");
        let mut funding_keys = [
            signer.public_material().base_public_keys.funding_pubkey,
            remote.funding_key.pubkey(),
        ];
        funding_keys.sort();
        let ctx = KeyAggContext::new(funding_keys).unwrap();
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let hash = blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        let lock = Script::new_builder()
            .args(hash[..20].to_vec().pack())
            .build();
        let type_script = udt.then(|| Script::new_builder().code_hash([8; 32].pack()).build());
        let funding = TransactionBuilder::default()
            .input(CellInput::new(OutPoint::new([7; 32].pack(), 0), 0))
            .output(
                CellOutput::new_builder()
                    .capacity(1000u64)
                    .lock(lock)
                    .type_(type_script.pack())
                    .build(),
            )
            .output_data(if udt {
                1000u128.to_le_bytes().to_vec().pack()
            } else {
                Default::default()
            })
            .build()
            .data();
        signer
            .bind_from_approved_funding(
                &funding,
                0,
                Script::default(),
                &[OutPoint::new([7; 32].pack(), 0)],
            )
            .await
            .unwrap();
        let parameters = CommitmentParameters {
            remote_shutdown_script: Script::default(),
            remote_funding_key: remote.funding_key.pubkey(),
            remote_settlement_key: remote.tlc_base_key.pubkey(),
            commitment_lock: Script::new_builder()
                .code_hash([3; 32].pack())
                .hash_type(1u8)
                .build(),
            delay_epoch: (0xa000_0000_0000_0000u64
                | ckb_types::core::EpochNumberWithFraction::new(1, 0, 1).full_value())
            .to_le_bytes(),
            epoch_duration_ms: 2000,
            commitment_fee: 10,
            local_amount: 400,
            remote_amount: 600,
            local_reserved_ckb_amount: if udt { 500 } else { 100 },
            remote_reserved_ckb_amount: if udt { 500 } else { 100 },
        };
        signer
            .approve_commitment_parameters(&funding, parameters.clone())
            .await
            .unwrap();
        Self {
            root,
            signer,
            store,
            remote,
            funding,
            parameters,
        }
    }

    pub fn opening(&self) -> SettlementData {
        SettlementData {
            local_amount: 400,
            remote_amount: 600,
            tlcs: vec![],
        }
    }

    pub fn terms(&self, inbound: bool) -> PaymentAuthorization {
        PaymentAuthorization {
            payment_hash: HashAlgorithm::CkbHash.hash([9; 32]).into(),
            hash_algorithm: HashAlgorithm::CkbHash,
            inbound,
            amount: 50,
            expires_at_ms: 10_000,
            min_tlc_expiry_delta_ms: 1000,
            max_tlc_expiry_ms: 100_000,
        }
    }

    pub fn tlc(&self, inbound: bool, for_remote: bool) -> SettlementTlc {
        let base = self.signer.public_material().base_public_keys.tlc_base_key;
        SettlementTlc {
            tlc_id: if inbound == for_remote {
                TLCId::Received(4)
            } else {
                TLCId::Offered(4)
            },
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 50,
            payment_hash: self.terms(inbound).payment_hash,
            expiry: 20_000,
            local_key: None,
            local_key_pubkey: Some(
                try_derive_tlc_pubkey(&base, &self.signer.get_commitment_point(1)).unwrap(),
            ),
            local_key_commitment_number: Some(1),
            remote_key: self.remote.derive_tlc_key(1).pubkey(),
        }
    }

    pub fn incoming(&self, for_remote: bool) -> SettlementData {
        SettlementData {
            local_amount: 400,
            remote_amount: 550,
            tlcs: vec![self.tlc(true, for_remote)],
        }
    }

    pub async fn request(
        &self,
        for_remote: bool,
        version: u64,
        data: SettlementData,
    ) -> (ChannelSigningContent, CommitmentContext) {
        // The first real commitment follows funding collaboration and has version one.
        let version = version + 1;
        let keys = self.signer.public_material().base_public_keys;
        let mut funding_keys = [keys.funding_pubkey, self.remote.funding_key.pubkey()];
        funding_keys.sort();
        let ctx = KeyAggContext::new(funding_keys).unwrap();
        let lock_ctx = KeyAggContext::new(if for_remote {
            [keys.funding_pubkey, self.remote.funding_key.pubkey()]
        } else {
            [self.remote.funding_key.pubkey(), keys.funding_pubkey]
        })
        .unwrap();
        let point: musig2::secp::Point = lock_ctx.aggregated_pubkey();
        let hash = blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        let mut args = hash[..20].to_vec();
        args.extend_from_slice(&self.parameters.delay_epoch);
        args.extend_from_slice(&version.to_be_bytes());
        args.extend_from_slice(&settlement_witness_hash(
            &data,
            for_remote,
            keys.tlc_base_key,
            self.remote.tlc_base_key.pubkey(),
        ));
        args.push(0);
        let lock = self
            .parameters
            .commitment_lock
            .clone()
            .as_builder()
            .args(args.pack())
            .build();
        let tx = TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(self.funding.calc_tx_hash(), 0),
                0,
            ))
            .output(
                self.funding
                    .raw()
                    .outputs()
                    .get(0)
                    .unwrap()
                    .as_builder()
                    .capacity(990u64)
                    .lock(lock)
                    .build(),
            )
            .output_data(self.funding.raw().outputs_data().get(0).unwrap())
            .build()
            .data();
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: version,
        };
        let nonce = self
            .signer
            .get_musig2_nonce(slot)
            .await
            .unwrap()
            .public_nonce;
        let remote_nonce = musig2::SecNonce::build([7; 32]).build().public_nonce();
        let agg_nonce = AggNonce::sum([nonce, remote_nonce.clone()]);
        let signable = Musig2SignableContent::CommitmentTransaction(tx);
        let partial = musig2::sign_partial(
            &ctx,
            self.remote.funding_key.clone(),
            musig2::SecNonce::build([7; 32]).build(),
            &agg_nonce,
            signable.signing_message(),
        )
        .unwrap();
        (
            ChannelSigningContent::Musig2(Musig2SigningContent {
                slot,
                commitment_counter: Some(CommitmentCounter::Local),
                key_agg_ctx: ctx,
                agg_nonce,
                content: signable,
            }),
            CommitmentContext {
                session: SigningSessionEvidence {
                    peer_public_nonce: remote_nonce,
                    peer_partial_signature: (!for_remote).then_some(partial),
                },
                transition: if for_remote {
                    ChannelSigningTransition::SendCommitmentSigned
                } else {
                    ChannelSigningTransition::CompleteReceivedCommitment
                },
                settlement: OwnedSettlementBinding {
                    data,
                    local_settlement_key: keys.tlc_base_key,
                    remote_settlement_key: self.remote.tlc_base_key.pubkey(),
                    for_remote: Some(for_remote),
                },
            },
        )
    }

    pub async fn sign(&self, for_remote: bool, version: u64, data: SettlementData) {
        let (content, context) = self.request(for_remote, version, data).await;
        let prepared = self
            .signer
            .prepare_commitment(content, context, 100)
            .await
            .unwrap();
        self.signer.sign_commitment(prepared, 100).await.unwrap();
    }

    pub async fn assert_rejected(
        &self,
        content: ChannelSigningContent,
        context: CommitmentContext,
        reason: &str,
    ) {
        let before = self.store.snapshot().unwrap();
        let error = self
            .signer
            .prepare_commitment(content, context, 100)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(reason),
            "expected {reason}, got {error}"
        );
        assert_eq!(self.store.snapshot().unwrap(), before);
    }
}

fn mutate_tx(content: &mut ChannelSigningContent, mutate: impl FnOnce(Transaction) -> Transaction) {
    let ChannelSigningContent::Musig2(musig) = content else {
        panic!()
    };
    let Musig2SignableContent::CommitmentTransaction(tx) = &mut musig.content else {
        panic!()
    };
    *tx = mutate(tx.clone());
}

#[tokio::test]
async fn rejects_forged_opening_balances_even_when_snapshot_hash_matches() {
    let f = Fixture::new(false).await;
    let mut data = f.opening();
    data.local_amount = 399;
    data.remote_amount = 601;
    let (content, context) = f.request(true, 0, data).await;
    f.assert_rejected(content, context, "opening balances")
        .await;
}

#[tokio::test]
async fn rejects_wrong_contract_asset_capacity_fee_inputs_and_lock_arguments() {
    for udt in [false, true] {
        let f = Fixture::new(udt).await;
        for attack in 0..10 {
            let (mut content, context) = f.request(true, 0, f.opening()).await;
            mutate_tx(&mut content, |tx| {
                let raw = tx.raw();
                let output = raw.outputs().get(0).unwrap();
                let mut builder = raw.clone().as_builder();
                match attack {
                    0 => {
                        builder = builder.outputs(
                            vec![output
                                .clone()
                                .as_builder()
                                .lock(
                                    output
                                        .lock()
                                        .as_builder()
                                        .code_hash([99; 32].pack())
                                        .build(),
                                )
                                .build()]
                            .pack(),
                        )
                    }
                    1 => {
                        builder = builder
                            .outputs(vec![output.as_builder().capacity(989u64).build()].pack())
                    }
                    2 => {
                        builder = builder.outputs(
                            vec![output
                                .as_builder()
                                .type_(Some(Script::default()).pack())
                                .build()]
                            .pack(),
                        )
                    }
                    3 => builder = builder.outputs_data(vec![[0u8; 16].to_vec().pack()].pack()),
                    4 => {
                        builder = builder.inputs(
                            vec![CellInput::new(OutPoint::new([88; 32].pack(), 0), 0)].pack(),
                        )
                    }
                    5 => builder = builder.outputs(vec![output.clone(), output].pack()),
                    6 => {
                        builder = builder.inputs(
                            vec![raw
                                .inputs()
                                .get(0)
                                .unwrap()
                                .as_builder()
                                .since(1u64)
                                .build()]
                            .pack(),
                        )
                    }
                    _ => {
                        let mut args = output.lock().args().raw_data().to_vec();
                        args[match attack {
                            7 => 0,
                            8 => 20,
                            _ => 56,
                        }] ^= 1;
                        builder = builder.outputs(
                            vec![output
                                .clone()
                                .as_builder()
                                .lock(output.lock().as_builder().args(args.pack()).build())
                                .build()]
                            .pack(),
                        );
                    }
                }
                tx.as_builder().raw(builder.build()).build()
            });
            {
                let before = f.store.snapshot().unwrap();
                assert!(
                    f.signer
                        .prepare_commitment(content, context, 100)
                        .await
                        .is_err(),
                    "accepted attack {attack} (udt={udt})"
                );
                assert_eq!(
                    f.store.snapshot().unwrap(),
                    before,
                    "rejection changed persistent state"
                );
            }
        }
    }
}

#[tokio::test]
async fn rejects_forged_tlc_keys_derivation_amount_expiry_hash_and_direction() {
    let f = Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    for attack in 0..8 {
        let mut data = f.incoming(true);
        match attack {
            0 => data.tlcs[0].local_key_pubkey = Some(f.remote.tlc_base_key.pubkey()),
            1 => data.tlcs[0].local_key_commitment_number = None,
            2 => data.tlcs[0].local_key_commitment_number = Some(3),
            3 => {
                data.tlcs[0].payment_amount = 51;
                data.remote_amount = 549;
            }
            4 => data.tlcs[0].expiry = 101,
            5 => data.tlcs[0].hash_algorithm = HashAlgorithm::Sha256,
            6 => data.tlcs[0].tlc_id = TLCId::Offered(4),
            _ => data.tlcs[0].payment_hash = [8; 32].into(),
        }
        let (content, context) = f.request(true, 1, data).await;
        {
            let before = f.store.snapshot().unwrap();
            assert!(
                f.signer
                    .prepare_commitment(content, context, 100)
                    .await
                    .is_err(),
                "accepted attack {attack}"
            );
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
}

#[tokio::test]
async fn rejects_settlement_identity_and_orientation_spoofing() {
    let f = Fixture::new(false).await;
    for attack in 0..4 {
        let (content, mut context) = f.request(true, 0, f.opening()).await;
        match attack {
            0 => context.settlement.local_settlement_key = f.remote.tlc_base_key.pubkey(),
            1 => {
                context.settlement.remote_settlement_key =
                    f.signer.public_material().base_public_keys.tlc_base_key
            }
            2 => context.settlement.for_remote = Some(false),
            _ => context.transition = ChannelSigningTransition::SendClosingSigned,
        }
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .prepare_commitment(content, context, 100)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
}

#[tokio::test]
async fn receive_fulfillment_is_verified_in_both_lanes_and_survives_restart() {
    for for_remote in [false, true] {
        let mut f = Fixture::new(false).await;
        f.sign(for_remote, 0, f.opening()).await;
        f.signer
            .authorize_invoice(f.terms(true), [9; 32].into())
            .await
            .unwrap();
        f.sign(for_remote, 1, f.incoming(for_remote)).await;
        let restored = RootSigner::open(
            RootKey::import([42; 32]).unwrap(),
            MemoryStore::from_snapshot(&f.store.snapshot().unwrap()).unwrap(),
        )
        .await
        .unwrap();
        f.signer = restored
            .open_channel(f.signer.channel_key_id())
            .await
            .unwrap();
        let paid = SettlementData {
            local_amount: 450,
            remote_amount: 550,
            tlcs: vec![],
        };
        let (content, context) = f.request(for_remote, 2, paid.clone()).await;
        f.assert_rejected(content, context, "locally authorized resolution")
            .await;
        f.signer
            .authorize_fulfillment(f.terms(true).payment_hash, [9; 32].into())
            .await
            .unwrap();
        let (content, context) = f.request(for_remote, 2, f.opening()).await;
        f.assert_rejected(content, context, "balances do not match")
            .await;
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .authorize_cancellation(f.terms(true).payment_hash)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
        f.sign(for_remote, 2, paid.clone()).await;
        // Exact retries are allowed; they cannot advance the balance twice.
        f.sign(for_remote, 2, paid).await;
        let (content, context) = f.request(for_remote, 3, f.incoming(for_remote)).await;
        f.assert_rejected(content, context, "replay").await;
    }
}

#[tokio::test]
async fn rejects_existing_tlc_mutation_duplicate_and_unaccounted_balance_changes() {
    let f = Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(true, 1, f.incoming(true)).await;
    for attack in 0..4 {
        let mut data = f.incoming(true);
        match attack {
            0 => data.tlcs[0].expiry += 1000,
            1 => {
                data.tlcs.push(data.tlcs[0].clone());
                data.remote_amount -= 50;
            }
            2 => {
                data.local_amount -= 1;
                data.remote_amount += 1;
            }
            _ => data.tlcs[0].remote_key = f.remote.funding_key.pubkey(),
        }
        let (content, context) = f.request(true, 2, data).await;
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .prepare_commitment(content, context, 100)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
    let (content, context) = f.request(true, 3, f.incoming(true)).await;
    f.assert_rejected(content, context, "gap").await;
}

#[tokio::test]
async fn default_prepare_cannot_bypass_commitment_context_validation() {
    let f = Fixture::new(false).await;
    let (content, _) = f.request(true, 0, f.opening()).await;
    assert!(f
        .signer
        .prepare(content)
        .await
        .unwrap_err()
        .to_string()
        .contains("intent-specific verifier"));
}

#[tokio::test]
async fn cancellation_and_outgoing_payment_require_explicit_local_authorization() {
    for inbound in [true, false] {
        let f = Fixture::new(false).await;
        f.sign(true, 0, f.opening()).await;
        if inbound {
            f.signer
                .authorize_invoice(f.terms(true), [9; 32].into())
                .await
                .unwrap();
        } else {
            f.signer.authorize_payment(f.terms(false)).await.unwrap();
        }
        let pending = if inbound {
            f.incoming(true)
        } else {
            SettlementData {
                local_amount: 350,
                remote_amount: 600,
                tlcs: vec![f.tlc(false, true)],
            }
        };
        f.sign(true, 1, pending).await;
        f.signer
            .authorize_cancellation(f.terms(inbound).payment_hash)
            .await
            .unwrap();
        f.sign(true, 2, f.opening()).await;
    }
}

#[tokio::test]
async fn stale_approval_cannot_overwrite_new_payment_authorization() {
    let f = Fixture::new(false).await;
    let (content, context) = f.request(true, 0, f.opening()).await;
    let prepared = f
        .signer
        .prepare_commitment(content, context, 100)
        .await
        .unwrap();
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    assert_eq!(
        f.signer.sign_commitment(prepared, 100).await.unwrap_err(),
        SignerError::SigningStateChanged
    );
}

#[tokio::test]
async fn rejects_expired_confirmation_and_invoice_without_preimage_possession() {
    let f = Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f.signer.authorize_payment(f.terms(true)).await.is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .authorize_invoice(f.terms(true), [8; 32].into())
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    let (content, context) = f.request(true, 1, f.incoming(true)).await;
    let prepared = f
        .signer
        .prepare_commitment(content, context, 100)
        .await
        .unwrap();
    assert!(f
        .signer
        .sign_commitment(prepared, 10_000)
        .await
        .unwrap_err()
        .to_string()
        .contains("expired"));
    // A rejected approval did not consume the commitment version or mutate balances.
    f.sign(true, 1, f.incoming(true)).await;
}

#[tokio::test]
async fn validates_cross_lane_tlc_identity_and_does_not_reapply_invoice_expiry() {
    let f = Fixture::new(false).await;
    f.sign(false, 0, f.opening()).await;
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(true, 1, f.incoming(true)).await;
    let mut forged = f.incoming(false);
    forged.tlcs[0].remote_key = f.remote.funding_key.pubkey();
    let (content, context) = f.request(false, 1, forged).await;
    f.assert_rejected(content, context, "differs between commitment lanes")
        .await;
    let (content, context) = f.request(false, 1, f.incoming(false)).await;
    let prepared = f
        .signer
        .prepare_commitment(content, context, 11_000)
        .await
        .unwrap();
    assert!(!prepared.commitment_review().unwrap().has_outbound_changes);
    f.signer.sign_commitment(prepared, 11_000).await.unwrap();
}

#[tokio::test]
async fn udt_receipt_and_settlement_conserve_tokens_and_ckb_separately() {
    let f = Fixture::new(true).await;
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(true, 1, f.incoming(true)).await;
    f.signer
        .authorize_fulfillment(f.terms(true).payment_hash, [9; 32].into())
        .await
        .unwrap();
    f.sign(
        true,
        2,
        SettlementData {
            local_amount: 450,
            remote_amount: 550,
            tlcs: vec![],
        },
    )
    .await;
}

#[cfg(feature = "json")]
fn rpc_status(
    content: ChannelSigningContent,
    context: CommitmentContext,
) -> fiber_json_types::ChannelSigningStatus {
    let ChannelSigningContent::Musig2(content) = content else {
        panic!()
    };
    let snapshot = context.settlement;
    let for_remote = snapshot.for_remote.unwrap();
    fiber_json_types::ChannelSigningStatus::SignatureRequired {
        session_evidence: crate::json::session_evidence_to_rpc(&context.session),
        request_id: fiber_types::Hash256::from([6; 32]).into(),
        transition: if for_remote {
            fiber_json_types::ChannelSigningTransition::SendCommitmentSigned
        } else {
            fiber_json_types::ChannelSigningTransition::CompleteReceivedCommitment
        },
        content: crate::json::musig2_to_rpc(&content),
        settlement: Some(fiber_json_types::SigningSettlement {
            local_amount: snapshot.data.local_amount,
            remote_amount: snapshot.data.remote_amount,
            local_settlement_pubkey: snapshot.local_settlement_key.into(),
            remote_settlement_pubkey: snapshot.remote_settlement_key.into(),
            for_remote,
            tlcs: snapshot
                .data
                .tlcs
                .iter()
                .map(|tlc| fiber_json_types::SigningSettlementTlc {
                    tlc_id: u64::from(tlc.tlc_id),
                    local_key_commitment_number: tlc.local_key_commitment_number.unwrap(),
                    inbound: tlc.tlc_id.is_received() == for_remote,
                    payment_hash: tlc.payment_hash.into(),
                    hash_algorithm: tlc.hash_algorithm.into(),
                    payment_amount: tlc.payment_amount,
                    expiry: tlc.expiry,
                    local_key_pubkey: tlc.local_pubkey().into(),
                    remote_key: tlc.remote_key.into(),
                })
                .collect(),
        }),
    }
}

#[cfg(feature = "json")]
#[tokio::test]
async fn malicious_lsp_rpc_is_rejected_before_auto_or_manual_confirmation() {
    for policy in [SigningPolicy::Auto, SigningPolicy::Manual] {
        for for_remote in [false, true] {
            for attack in 0..8 {
                let f = Fixture::new(false).await;
                f.sign(for_remote, 0, f.opening()).await;
                f.signer
                    .authorize_invoice(f.terms(true), [9; 32].into())
                    .await
                    .unwrap();
                let mut data = f.incoming(for_remote);
                match attack {
                    0 => {
                        data.local_amount -= 1;
                        data.remote_amount += 1;
                    }
                    1 => data.tlcs[0].local_key_pubkey = Some(f.remote.tlc_base_key.pubkey()),
                    _ => (),
                }
                let (content, context) = f.request(for_remote, 1, data).await;
                let mut status = rpc_status(content, context);
                let fiber_json_types::ChannelSigningStatus::SignatureRequired {
                    settlement, ..
                } = &mut status
                else {
                    panic!()
                };
                match attack {
                    2 => *settlement = None,
                    5 => settlement.as_mut().unwrap().tlcs[0].inbound ^= true,
                    _ => (),
                }
                let channel_id = fiber_types::Hash256::from([1; 32]);
                let mut directory = HostedSessionState::default();
                directory
                    .bindings
                    .insert(channel_id, f.signer.channel_key_id());
                let mut session = HostedSession::new(f.root)
                    .with_state(directory)
                    .with_policy(policy);
                // Exercise the serialized RPC boundary, not just an internal verifier helper.
                let mut value = serde_json::to_value(&status).unwrap();
                if matches!(attack, 3 | 4 | 6 | 7) {
                    let field = if matches!(attack, 3 | 6) {
                        "tlc_id"
                    } else {
                        "local_key_commitment_number"
                    };
                    let tlc = value
                        .get_mut("settlement")
                        .unwrap()
                        .get_mut("tlcs")
                        .unwrap()[0]
                        .as_object_mut()
                        .unwrap();
                    if attack < 6 {
                        tlc.remove(field);
                    } else {
                        tlc.insert(field.into(), serde_json::Value::Null);
                    }
                    let before = f.store.snapshot().unwrap();
                    assert!(
                        serde_json::from_value::<fiber_json_types::ChannelSigningStatus>(value)
                            .is_err()
                    );
                    assert_eq!(f.store.snapshot().unwrap(), before);
                    continue;
                }
                let wire = serde_json::to_vec(&value).unwrap();
                let status = serde_json::from_slice(&wire).unwrap();
                {
                    let before = f.store.snapshot().unwrap();
                    assert!(
                        session
                            .handle_channel_status(channel_id, status, 100)
                            .await
                            .is_err(),
                        "accepted attack {attack}, policy {policy:?}, remote {for_remote}"
                    );
                    assert_eq!(
                        f.store.snapshot().unwrap(),
                        before,
                        "rejection changed persistent state"
                    );
                }
            }
        }
    }
}

#[cfg(feature = "json")]
#[tokio::test]
async fn rpc_receipt_direction_is_local_in_both_commitment_views() {
    for for_remote in [false, true] {
        let f = Fixture::new(false).await;
        f.sign(for_remote, 0, f.opening()).await;
        f.signer
            .authorize_invoice(f.terms(true), [9; 32].into())
            .await
            .unwrap();
        let (content, context) = f.request(for_remote, 1, f.incoming(for_remote)).await;
        let channel_id = fiber_types::Hash256::from([1; 32]);
        let mut directory = HostedSessionState::default();
        directory
            .bindings
            .insert(channel_id, f.signer.channel_key_id());
        let mut session = HostedSession::new(f.root).with_state(directory);
        assert!(matches!(
            session
                .handle_channel_status(channel_id, rpc_status(content, context), 100)
                .await
                .unwrap(),
            ProcessOutcome::ReadyToSubmit(_)
        ));
    }
}

#[tokio::test]
async fn rejects_wrong_opening_parameters_and_rollback_after_settlement() {
    let f = Fixture::new(false).await;
    let mut other = f.parameters.clone();
    other.local_amount -= 1;
    other.remote_amount += 1;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .approve_commitment_parameters(&f.funding, other)
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(true, 1, f.incoming(true)).await;
    let (content, context) = f.request(true, 0, f.opening()).await;
    f.assert_rejected(content, context, "rollback").await;
}

#[tokio::test]
async fn commitment_approval_is_atomic_under_concurrent_signers() {
    let f = Fixture::new(false).await;
    let (content, context) = f.request(true, 0, f.opening()).await;
    let first = f
        .signer
        .prepare_commitment(content.clone(), context.clone(), 100)
        .await
        .unwrap();
    let second_signer = f
        .root
        .open_channel(f.signer.channel_key_id())
        .await
        .unwrap();
    let second = second_signer
        .prepare_commitment(content, context, 100)
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        f.signer.sign_commitment(first, 100),
        second_signer.sign_commitment(second, 100)
    );
    assert!(a.is_ok() || b.is_ok());
    // A duplicate may return the same signature; it can never create a second state transition.
    f.sign(true, 0, f.opening()).await;
}

#[tokio::test]
async fn rejects_expired_existing_tlc_and_uncommitted_millisecond_grace() {
    let f = Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    let mut data = f.incoming(true);
    data.tlcs[0].expiry = 1999;
    let (content, context) = f.request(true, 1, data).await;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .prepare_commitment(content, context, 100)
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.sign(true, 1, f.incoming(true)).await;
    let (content, context) = f.request(true, 2, f.incoming(true)).await;
    assert!(f
        .signer
        .prepare_commitment(content, context, 20_000)
        .await
        .unwrap_err()
        .to_string()
        .contains("expired TLC"));
}

#[tokio::test]
async fn rejects_retry_that_replaces_aggregate_nonce_even_with_identical_transaction() {
    let f = Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    let (mut content, context) = f.request(true, 0, f.opening()).await;
    let ChannelSigningContent::Musig2(musig) = &mut content else {
        panic!()
    };
    musig.agg_nonce = AggNonce::sum([musig2::SecNonce::build([99; 32]).build().public_nonce()]);
    f.assert_rejected(content, context, "nonce").await;
}

#[tokio::test]
async fn rejects_tlc_that_spends_either_partys_reserved_ckb() {
    for inbound in [false, true] {
        let f = Fixture::new(false).await;
        f.sign(true, 0, f.opening()).await;
        let mut terms = f.terms(inbound);
        terms.amount = if inbound { 550 } else { 350 };
        if inbound {
            f.signer
                .authorize_invoice(terms.clone(), [9; 32].into())
                .await
                .unwrap();
        } else {
            f.signer.authorize_payment(terms.clone()).await.unwrap();
        }
        let mut tlc = f.tlc(inbound, true);
        tlc.payment_amount = terms.amount;
        let data = SettlementData {
            local_amount: if inbound { 400 } else { 50 },
            remote_amount: if inbound { 50 } else { 600 },
            tlcs: vec![tlc],
        };
        let (content, context) = f.request(true, 1, data).await;
        f.assert_rejected(content, context, "reserved CKB").await;
    }
}

impl Fixture {
    pub(crate) async fn revocation(
        &self,
        receiving: bool,
        version: u64,
    ) -> (ChannelSigningContent, RevocationContext) {
        let records = self.signer.recovery_records().await.unwrap();
        let old = records
            .iter()
            .find(|r| r.reference.for_remote == receiving && r.reference.version == version)
            .unwrap();
        let next = records
            .iter()
            .find(|r| r.reference.for_remote == receiving && r.reference.version == version + 1)
            .unwrap();
        let local = self
            .signer
            .public_material()
            .base_public_keys
            .funding_pubkey;
        let remote = self.remote.funding_key.pubkey();
        let key_agg_ctx = KeyAggContext::new(if receiving {
            [local, remote]
        } else {
            [remote, local]
        })
        .unwrap();
        let slot = NonceSlot {
            purpose: NoncePurpose::Revocation,
            commitment_number: version + 1,
        };
        let nonce = self
            .signer
            .get_musig2_nonce(slot)
            .await
            .unwrap()
            .public_nonce;
        let peer_nonce = musig2::SecNonce::build([55; 32]).build();
        let peer_public_nonce = peer_nonce.public_nonce();
        let agg_nonce = AggNonce::sum([nonce, peer_public_nonce.clone()]);
        let content = Musig2SignableContent::Revocation {
            output: self
                .funding
                .raw()
                .outputs()
                .get(0)
                .unwrap()
                .as_builder()
                .capacity(990u64)
                .lock(Script::default())
                .build(),
            output_data: self
                .funding
                .raw()
                .outputs_data()
                .get(0)
                .unwrap()
                .as_slice()
                .to_vec(),
            commitment_lock_script_args: old
                .transaction
                .raw()
                .outputs()
                .get(0)
                .unwrap()
                .lock()
                .args()
                .raw_data()[..36]
                .to_vec(),
        };
        let peer = musig2::sign_partial(
            &key_agg_ctx,
            self.remote.funding_key.clone(),
            peer_nonce,
            &agg_nonce,
            content.signing_message(),
        )
        .unwrap();
        (
            ChannelSigningContent::Musig2(Musig2SigningContent {
                slot,
                commitment_counter: Some(if receiving {
                    CommitmentCounter::Local
                } else {
                    CommitmentCounter::Remote
                }),
                key_agg_ctx,
                agg_nonce,
                content,
            }),
            RevocationContext {
                transition: if receiving {
                    fiber_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck
                } else {
                    fiber_types::ChannelSigningTransition::SendRevokeAndAck
                },
                session: SigningSessionEvidence {
                    peer_public_nonce,
                    peer_partial_signature: receiving.then_some(peer),
                },
                revoked: old.reference.clone(),
                replacement: next.reference.clone(),
            },
        )
    }
}

#[tokio::test]
async fn participant_nonce_and_peer_signature_are_mandatory_and_bound() {
    let f = Fixture::new(false).await;
    let (content, context) = f.request(false, 0, f.opening()).await;
    let mut bad = context.clone();
    bad.session.peer_partial_signature = None;
    f.assert_rejected(content.clone(), bad, "missing peer")
        .await;
    let mut bad = context.clone();
    bad.session.peer_public_nonce = musig2::SecNonce::build([98; 32]).build().public_nonce();
    f.assert_rejected(content.clone(), bad, "aggregate nonce")
        .await;
    let mut bad = context.clone();
    bad.session.peer_partial_signature =
        Some(musig2::PartialSignature::from_slice(&[2; 32]).unwrap());
    f.assert_rejected(content.clone(), bad, "peer partial")
        .await;
    f.sign(false, 0, f.opening()).await;
    let records = f.signer.recovery_records().await.unwrap();
    assert!(records[0].complete_signature.is_some());
    let reopened = f
        .root
        .open_channel(f.signer.channel_key_id())
        .await
        .unwrap();
    assert!(reopened.recovery_records().await.unwrap()[0]
        .complete_signature
        .is_some());
}

#[tokio::test]
async fn revocation_validates_old_state_destination_amount_and_safe_successor() {
    for receiving in [false, true] {
        let f = Fixture::new(false).await;
        f.sign(receiving, 0, f.opening()).await;
        f.sign(receiving, 1, f.opening()).await;
        let (content, context) = f.revocation(receiving, 1).await;
        // Changing the lane/reference cannot be authorized even with valid signature bytes.
        let mut bad_context = context.clone();
        bad_context.replacement.for_remote = !receiving;
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .prepare_revocation(content.clone(), bad_context, 100)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
        for bad in 0..3 {
            let mut changed = content.clone();
            let ChannelSigningContent::Musig2(ref mut m) = changed else {
                panic!()
            };
            let Musig2SignableContent::Revocation {
                output,
                commitment_lock_script_args,
                ..
            } = &mut m.content
            else {
                panic!()
            };
            match bad {
                0 => *output = output.clone().as_builder().capacity(989u64).build(),
                1 => {
                    *output = output
                        .clone()
                        .as_builder()
                        .lock(Script::new_builder().args([1u8].pack()).build())
                        .build()
                }
                _ => commitment_lock_script_args[35] ^= 1,
            }
            {
                let before = f.store.snapshot().unwrap();
                #[cfg(feature = "json")]
                assert_lifecycle_rpc_rejected(
                    &f,
                    changed.clone(),
                    context.session.clone(),
                    fiber_json_types::ChannelSigningTransition::SendRevokeAndAck,
                    receiving,
                )
                .await;
                assert!(f
                    .signer
                    .prepare_revocation(changed, context.clone(), 100)
                    .await
                    .is_err());
                assert_eq!(
                    f.store.snapshot().unwrap(),
                    before,
                    "rejection changed persistent state"
                );
            }
        }
        {
            let before = f.store.snapshot().unwrap();
            assert!(f.signer.prepare(content.clone()).await.is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
        let prepared = f
            .signer
            .prepare_revocation(content.clone(), context.clone(), 100)
            .await
            .unwrap();
        {
            let before = f.store.snapshot().unwrap();
            assert!(f.signer.sign(prepared.clone()).await.is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
        f.signer.sign_revocation(prepared, 100).await.unwrap();
        let retry = f
            .signer
            .prepare_revocation(content, context, 100)
            .await
            .unwrap();
        f.signer.sign_revocation(retry, 100).await.unwrap();
    }
}

#[tokio::test]
async fn revocation_requires_complete_successor_not_just_a_signed_proposal() {
    let f = Fixture::new(false).await;
    f.sign(false, 0, f.opening()).await;
    let (content, mut context) = f.request(false, 1, f.opening()).await;
    context.session.peer_partial_signature = None;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .prepare_commitment(content, context, 100)
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    assert_eq!(f.signer.recovery_records().await.unwrap().len(), 1);
}

#[tokio::test]
async fn preimage_release_requires_recoverable_inbound_tlc_and_fresh_time() {
    let f = Fixture::new(false).await;
    let hash = f.terms(true).payment_hash;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(false, 0, f.opening()).await;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .authorize_preimage_release(hash, [9; 32].into(), 100, &TestChain::funding(&f))
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.sign(true, 0, f.opening()).await;
    f.sign(true, 1, f.incoming(true)).await;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .authorize_preimage_release(hash, [9; 32].into(), 100, &TestChain::funding(&f))
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.sign(false, 1, f.incoming(false)).await;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .authorize_preimage_release(hash, [8; 32].into(), 100, &TestChain::funding(&f))
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .authorize_preimage_release(hash, [9; 32].into(), 19_000, &TestChain::funding(&f))
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    let reference = f
        .signer
        .authorize_preimage_release(hash, [9; 32].into(), 100, &TestChain::funding(&f))
        .await
        .unwrap();
    assert!(!reference.for_remote);
    assert_eq!(reference.version, 2);
}

struct TestChain {
    outpoint: OutPoint,
    cell: VerifiedCell,
    now: u64,
    mature: bool,
    extras: Vec<(OutPoint, VerifiedCell)>,
}
impl TestChain {
    fn funding(f: &Fixture) -> Self {
        Self {
            outpoint: OutPoint::new(f.funding.calc_tx_hash(), 0),
            cell: VerifiedCell {
                output: f.funding.raw().outputs().get(0).unwrap(),
                data: f
                    .funding
                    .raw()
                    .outputs_data()
                    .get(0)
                    .unwrap()
                    .raw_data()
                    .to_vec(),
            },
            now: 100,
            mature: true,
            extras: vec![],
        }
    }
}
#[async_trait::async_trait]
impl ChainVerifier for TestChain {
    async fn live_cell(&self, outpoint: &OutPoint) -> Result<VerifiedCell, SignerError> {
        if let Some((_, cell)) = self.extras.iter().find(|(point, _)| point == outpoint) {
            return Ok(cell.clone());
        }
        if *outpoint != self.outpoint {
            return Err(crate::commitment::invalid("not live"));
        }
        Ok(self.cell.clone())
    }
    async fn verify_commitment_lineage(
        &self,
        source: fiber_types::Hash256,
        outpoint: &OutPoint,
    ) -> Result<(), SignerError> {
        if source != self.outpoint.tx_hash().into() || *outpoint != self.outpoint {
            return Err(crate::commitment::invalid("incorrect lineage"));
        }
        Ok(())
    }
    async fn verify_maturity(&self, _: &[CellInput]) -> Result<(), SignerError> {
        if !self.mature {
            return Err(crate::commitment::invalid("immature input"));
        }
        Ok(())
    }
    async fn median_time_ms(&self) -> Result<u64, SignerError> {
        Ok(self.now)
    }
}

#[tokio::test]
async fn onchain_claim_checks_live_input_witness_destination_fee_and_maturity() {
    let f = Fixture::new(false).await;
    f.sign(false, 0, f.opening()).await;
    let record = f.signer.recovery_records().await.unwrap().remove(0);
    let source = record.transaction.raw().outputs().get(0).unwrap();
    let outpoint = OutPoint::new(record.transaction.calc_tx_hash(), 0);
    let mut chain = TestChain {
        outpoint: outpoint.clone(),
        cell: VerifiedCell {
            output: source.clone(),
            data: vec![],
        },
        now: 100,
        mature: true,
        extras: vec![],
    };
    let body = fiber_types::settlement_data_to_witness(
        &record.settlement.data,
        false,
        record.settlement.local_settlement_key,
        record.settlement.remote_settlement_key,
    );
    let mut next = body.clone();
    next[1..37].fill(0);
    let mut args = source.lock().args().raw_data()[..36].to_vec();
    args.extend_from_slice(&fiber_types::blake2b_hash_with_salt(&next, &[])[..20]);
    args.push(1);
    let remainder = source
        .clone()
        .as_builder()
        .capacity(590u64)
        .lock(source.lock().as_builder().args(args.pack()).build())
        .build();
    let destination = Script::new_builder().args([42u8].pack()).build();
    let mut witness = vec![16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 1];
    witness.extend_from_slice(&body);
    witness.extend_from_slice(&[0xfe, 0]);
    witness.extend_from_slice(&[0; 65]);
    let tx = TransactionBuilder::default()
        .input(CellInput::new(
            outpoint,
            u64::from_le_bytes(f.parameters.delay_epoch),
        ))
        .output(remainder)
        .output_data(ckb_types::packed::Bytes::default())
        .output(
            CellOutput::new_builder()
                .capacity(395u64)
                .lock(destination.clone())
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .witness(witness.pack())
        .build()
        .data();
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: tx.clone(),
    };
    let terms = OnchainSpendAuthorization {
        source: record.reference,
        destination,
        fee: 5,
        additional_inputs: vec![],
        cell_deps: vec![],
    };
    #[cfg(feature = "json")]
    for policy in [SigningPolicy::Auto, SigningPolicy::Manual] {
        for attack in 0..4 {
            let channel_id = fiber_types::Hash256::from([1; 32]);
            let mut directory = HostedSessionState::default();
            directory
                .bindings
                .insert(channel_id, f.signer.channel_key_id());
            let root = RootSigner::open(RootKey::import([42; 32]).unwrap(), f.store.clone())
                .await
                .unwrap();
            let session = HostedSession::new(root)
                .with_state(directory)
                .with_policy(policy);
            let status = fiber_json_types::WatchtowerSigningStatus::SignatureRequired {
                request_id: fiber_types::Hash256::from([6; 32]).into(),
                content: fiber_json_types::OnchainSigningContent {
                    key_purpose: fiber_json_types::OnchainKeyPurpose::Settlement,
                    transaction: content.transaction.clone().into(),
                },
            };
            let wire = serde_json::to_vec(&status).unwrap();
            let ProcessOutcome::NeedConfirmation(pending) = session
                .handle_watchtower_status_verified(
                    channel_id,
                    serde_json::from_slice(&wire).unwrap(),
                    terms.clone(),
                    &chain,
                )
                .await
                .unwrap()
            else {
                panic!("onchain must require confirmation")
            };
            let original = chain.cell.clone();
            let original_point = chain.outpoint.clone();
            match attack {
                0 => chain.mature = false,
                1 => chain.outpoint = OutPoint::new([99u8; 32].pack(), 0),
                2 => {
                    chain.cell.output = chain
                        .cell
                        .output
                        .clone()
                        .as_builder()
                        .capacity(989u64)
                        .build()
                }
                _ => chain.cell.data = vec![1],
            }
            let before = f.store.snapshot().unwrap();
            assert!(session.confirm_onchain(pending, &chain).await.is_err());
            assert!(session
                .handle_watchtower_status_verified(
                    channel_id,
                    serde_json::from_slice(&wire).unwrap(),
                    terms.clone(),
                    &chain
                )
                .await
                .is_err());
            assert_eq!(f.store.snapshot().unwrap(), before);
            chain.cell = original;
            chain.outpoint = original_point;
            chain.mature = true;
        }
    }
    let prepared = f
        .signer
        .prepare_onchain(content.clone(), terms.clone(), &chain)
        .await
        .unwrap();
    {
        let before = f.store.snapshot().unwrap();
        assert!(f.signer.sign(prepared.clone()).await.is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    chain.mature = false;
    {
        let before = f.store.snapshot().unwrap();
        assert!(f.signer.sign_onchain(prepared, &chain).await.is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    chain.mature = true;
    for bad in 0..4 {
        let mut altered = content.clone();
        let mut outputs: Vec<_> = tx.raw().outputs().into_iter().collect();
        match bad {
            0 => {
                outputs[1] = outputs[1]
                    .clone()
                    .as_builder()
                    .lock(Script::default())
                    .build()
            }
            1 => outputs[1] = outputs[1].clone().as_builder().capacity(394u64).build(),
            2 => outputs[0] = outputs[0].clone().as_builder().capacity(589u64).build(),
            _ => {}
        }
        altered.transaction = tx
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(outputs)
            .build()
            .data();
        if bad == 3 {
            let mut changed = witness.clone();
            changed[18] ^= 1;
            altered.transaction = altered
                .transaction
                .into_view()
                .as_advanced_builder()
                .set_witnesses(vec![changed.pack()])
                .build()
                .data();
        }
        {
            let before = f.store.snapshot().unwrap();
            #[cfg(feature = "json")]
            assert_onchain_rpc_rejected(&f, &altered, &terms, &chain).await;
            assert!(f
                .signer
                .prepare_onchain(altered, terms.clone(), &chain)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
    let prepared = f
        .signer
        .prepare_onchain(content, terms, &chain)
        .await
        .unwrap();
    assert!(matches!(
        f.signer.sign_onchain(prepared, &chain).await.unwrap(),
        ChannelSignature::Onchain(_)
    ));
}

#[tokio::test]
async fn cooperative_close_checks_exact_allocations_and_local_fee_approval() {
    for udt in [false, true] {
        let f = Fixture::new(udt).await;
        f.sign(false, 0, f.opening()).await;
        f.sign(true, 0, f.opening()).await;
        let (mut content, context) = f.request(true, 1, f.opening()).await;
        let tx = TransactionBuilder::default()
            .input(CellInput::new(
                OutPoint::new(f.funding.calc_tx_hash(), 0),
                0,
            ))
            .output(
                CellOutput::new_builder()
                    .capacity(if udt { 495u64 } else { 395u64 })
                    .lock(Script::default())
                    .type_(f.funding.raw().outputs().get(0).unwrap().type_())
                    .build(),
            )
            .output_data(if udt {
                400u128.to_le_bytes().to_vec().pack()
            } else {
                ckb_types::packed::Bytes::default()
            })
            .output(
                CellOutput::new_builder()
                    .capacity(if udt { 495u64 } else { 595u64 })
                    .lock(Script::default())
                    .type_(f.funding.raw().outputs().get(0).unwrap().type_())
                    .build(),
            )
            .output_data(if udt {
                600u128.to_le_bytes().to_vec().pack()
            } else {
                ckb_types::packed::Bytes::default()
            })
            .build()
            .data();
        let ChannelSigningContent::Musig2(ref mut m) = content else {
            panic!()
        };
        m.content = Musig2SignableContent::CooperativeCloseTransaction(tx.clone());
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .prepare_close(content.clone(), context.session.clone())
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
        f.signer
            .authorize_close(CloseAuthorization {
                fee: 10,
                local_fee_share: 5,
                remote_fee_share: 5,
            })
            .await
            .unwrap();
        let prepared = f
            .signer
            .prepare_close(content.clone(), context.session.clone())
            .await
            .unwrap();
        // Equal destination scripts do not imply which participant owns output zero.
        let mut reordered = content.clone();
        let ChannelSigningContent::Musig2(m) = &mut reordered else {
            panic!()
        };
        m.content = Musig2SignableContent::CooperativeCloseTransaction(
            tx.clone()
                .into_view()
                .as_advanced_builder()
                .set_outputs(
                    tx.raw()
                        .outputs()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect(),
                )
                .set_outputs_data(
                    tx.raw()
                        .outputs_data()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect(),
                )
                .build()
                .data(),
        );
        f.signer
            .prepare_close(reordered, context.session.clone())
            .await
            .unwrap();
        for wrong in [394u64, 396] {
            let mut changed = content.clone();
            let ChannelSigningContent::Musig2(ref mut m) = changed else {
                panic!()
            };
            let outputs = vec![
                tx.raw()
                    .outputs()
                    .get(0)
                    .unwrap()
                    .as_builder()
                    .capacity(wrong)
                    .build(),
                tx.raw()
                    .outputs()
                    .get(1)
                    .unwrap()
                    .as_builder()
                    .capacity(990 - wrong)
                    .build(),
            ];
            m.content = Musig2SignableContent::CooperativeCloseTransaction(
                tx.clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_outputs(outputs)
                    .build()
                    .data(),
            );
            {
                let before = f.store.snapshot().unwrap();
                #[cfg(feature = "json")]
                assert_lifecycle_rpc_rejected(
                    &f,
                    changed.clone(),
                    context.session.clone(),
                    fiber_json_types::ChannelSigningTransition::SendClosingSigned,
                    false,
                )
                .await;
                assert!(f
                    .signer
                    .prepare_close(changed, context.session.clone())
                    .await
                    .is_err());
                assert_eq!(
                    f.store.snapshot().unwrap(),
                    before,
                    "rejection changed persistent state"
                );
            }
        }
        f.signer
            .authorize_close(CloseAuthorization {
                fee: 11,
                local_fee_share: 6,
                remote_fee_share: 5,
            })
            .await
            .unwrap();
        {
            let before = f.store.snapshot().unwrap();
            assert!(f.signer.sign(prepared).await.is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
}

#[tokio::test]
async fn onchain_inbound_tlc_requires_exact_preimage_key_and_residual_state() {
    for udt in [false, true] {
        let f = Fixture::new(udt).await;
        f.signer
            .authorize_invoice(f.terms(true), [9; 32].into())
            .await
            .unwrap();
        f.sign(false, 0, f.opening()).await;
        f.sign(false, 1, f.incoming(false)).await;
        let record = f.signer.recovery_records().await.unwrap().pop().unwrap();
        let source = record.transaction.raw().outputs().get(0).unwrap();
        let outpoint = OutPoint::new(record.transaction.calc_tx_hash(), 0);
        let mut chain = TestChain {
            outpoint: outpoint.clone(),
            cell: VerifiedCell {
                output: source.clone(),
                data: record
                    .transaction
                    .raw()
                    .outputs_data()
                    .get(0)
                    .unwrap()
                    .raw_data()
                    .to_vec(),
            },
            now: 100,
            mature: true,
            extras: vec![],
        };
        let body = fiber_types::settlement_data_to_witness(
            &record.settlement.data,
            false,
            record.settlement.local_settlement_key,
            record.settlement.remote_settlement_key,
        );
        let mut next = body.clone();
        next.drain(1..86);
        next[0] = 0;
        let mut args = source.lock().args().raw_data()[..36].to_vec();
        args.extend_from_slice(&fiber_types::blake2b_hash_with_salt(&next, &[])[..20]);
        args.push(1);
        let remainder = source
            .clone()
            .as_builder()
            .capacity(if udt { 990u64 } else { 940u64 })
            .lock(source.lock().as_builder().args(args.pack()).build())
            .build();
        let destination = Script::new_builder().args([42u8].pack()).build();
        let auxiliary = OutPoint::new([88u8; 32].pack(), 0);
        if udt {
            chain.extras.push((
                auxiliary.clone(),
                VerifiedCell {
                    output: CellOutput::new_builder()
                        .capacity(100u64)
                        .lock(destination.clone())
                        .build(),
                    data: vec![],
                },
            ));
        }

        let mut witness = vec![16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 1];
        witness.extend_from_slice(&body);
        witness.extend_from_slice(&[0, 1]);
        witness.extend_from_slice(&[0; 65]);
        witness.extend_from_slice(&[9; 32]);
        let tx = TransactionBuilder::default()
            .input(CellInput::new(
                outpoint,
                u64::from_le_bytes(f.parameters.delay_epoch),
            ))
            .inputs(if udt {
                vec![CellInput::new(auxiliary.clone(), 0)]
            } else {
                vec![]
            })
            .output(remainder)
            .output_data(if udt {
                950u128.to_le_bytes().to_vec().pack()
            } else {
                ckb_types::packed::Bytes::default()
            })
            .output(
                CellOutput::new_builder()
                    .capacity(if udt { 95u64 } else { 45u64 })
                    .type_(source.type_())
                    .lock(destination.clone())
                    .build(),
            )
            .output_data(if udt {
                50u128.to_le_bytes().to_vec().pack()
            } else {
                ckb_types::packed::Bytes::default()
            })
            .witness(witness.pack())
            .build()
            .data();
        let content = OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Tlc {
                commitment_number: 1,
            },
            transaction: tx.clone(),
        };
        let terms = OnchainSpendAuthorization {
            source: record.reference,
            destination,
            fee: 5,
            additional_inputs: if udt { vec![auxiliary] } else { vec![] },
            cell_deps: vec![],
        };
        for bad in 0..3 {
            let mut changed = content.clone();
            if bad == 0 {
                changed.key_purpose = OnchainKeyPurpose::Tlc {
                    commitment_number: 2,
                };
            } else {
                let mut altered = witness.clone();
                if bad == 1 {
                    *altered.last_mut().unwrap() ^= 1;
                } else {
                    altered[17 + body.len()] = 0xfe;
                }
                changed.transaction = tx
                    .clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_witnesses(vec![altered.pack()])
                    .build()
                    .data();
            }
            {
                let before = f.store.snapshot().unwrap();
                assert!(f
                    .signer
                    .prepare_onchain(changed, terms.clone(), &chain)
                    .await
                    .is_err());
                assert_eq!(
                    f.store.snapshot().unwrap(),
                    before,
                    "rejection changed persistent state"
                );
            }
        }
        let prepared = f
            .signer
            .prepare_onchain(content, terms, &chain)
            .await
            .unwrap();
        assert!(matches!(
            f.signer.sign_onchain(prepared, &chain).await.unwrap(),
            ChannelSignature::Onchain(_)
        ));
        // Releasing a secret records fulfillment, so later refunds are forbidden.
        f.signer
            .authorize_preimage_release(
                f.terms(true).payment_hash,
                [9; 32].into(),
                100,
                &TestChain::funding(&f),
            )
            .await
            .unwrap();
        {
            let before = f.store.snapshot().unwrap();
            assert!(f
                .signer
                .authorize_cancellation(f.terms(true).payment_hash)
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
}

#[tokio::test]
async fn announcement_requires_approved_network_fields_and_correct_nonce() {
    let f = Fixture::new(false).await;
    let local = f.signer.public_material().base_public_keys.funding_pubkey;
    let remote = f.remote.funding_key.pubkey();
    let mut keys = [local, remote];
    keys.sort();
    let ctx = KeyAggContext::new(keys).unwrap();
    let point: musig2::secp::Point = ctx.aggregated_pubkey();
    let announcement = fiber_types::ChannelAnnouncement::new_unsigned(
        &local,
        &remote,
        OutPoint::new(f.funding.calc_tx_hash(), 0),
        [4; 32].into(),
        &secp256k1::XOnlyPublicKey::from_slice(&point.serialize_xonly()).unwrap(),
        800,
        None,
    );
    let slot = NonceSlot {
        purpose: NoncePurpose::ChannelAnnouncement,
        commitment_number: 0,
    };
    let local_nonce = f.signer.get_musig2_nonce(slot).await.unwrap().public_nonce;
    let peer_public_nonce = musig2::SecNonce::build([5; 32]).build().public_nonce();
    let session = SigningSessionEvidence {
        peer_public_nonce: peer_public_nonce.clone(),
        peer_partial_signature: None,
    };
    let content = ChannelSigningContent::Musig2(Musig2SigningContent {
        slot,
        commitment_counter: None,
        key_agg_ctx: ctx,
        agg_nonce: AggNonce::sum([local_nonce, peer_public_nonce]),
        content: Musig2SignableContent::ChannelAnnouncement(announcement.clone()),
    });
    {
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .prepare_announcement(content.clone(), session.clone())
            .await
            .is_err());
        assert_eq!(
            f.store.snapshot().unwrap(),
            before,
            "rejection changed persistent state"
        );
    }
    f.signer.approve_announcement(announcement).await.unwrap();
    for bad in 0..3 {
        let mut changed = content.clone();
        let ChannelSigningContent::Musig2(m) = &mut changed else {
            panic!()
        };
        let Musig2SignableContent::ChannelAnnouncement(a) = &mut m.content else {
            panic!()
        };
        match bad {
            0 => a.chain_hash = [5; 32].into(),
            1 => a.capacity += 1,
            _ => m.slot.purpose = NoncePurpose::Commitment,
        }
        {
            let before = f.store.snapshot().unwrap();
            #[cfg(feature = "json")]
            assert_lifecycle_rpc_rejected(
                &f,
                changed.clone(),
                session.clone(),
                fiber_json_types::ChannelSigningTransition::SignChannelAnnouncement,
                false,
            )
            .await;
            assert!(f
                .signer
                .prepare_announcement(changed, session.clone())
                .await
                .is_err());
            assert_eq!(
                f.store.snapshot().unwrap(),
                before,
                "rejection changed persistent state"
            );
        }
    }
    let prepared = f
        .signer
        .prepare_announcement(content, session)
        .await
        .unwrap();
    f.signer.sign(prepared).await.unwrap();
}

#[cfg(feature = "json")]
async fn assert_lifecycle_rpc_rejected(
    f: &Fixture,
    content: ChannelSigningContent,
    evidence: SigningSessionEvidence,
    transition: fiber_json_types::ChannelSigningTransition,
    receiving: bool,
) {
    let ChannelSigningContent::Musig2(content) = content else {
        panic!()
    };
    let status = fiber_json_types::ChannelSigningStatus::SignatureRequired {
        request_id: fiber_types::Hash256::from([6; 32]).into(),
        transition: if receiving {
            fiber_json_types::ChannelSigningTransition::CompleteReceivedRevokeAndAck
        } else {
            transition
        },
        content: crate::json::musig2_to_rpc(&content),
        session_evidence: crate::json::session_evidence_to_rpc(&evidence),
        settlement: None,
    };
    let wire = serde_json::to_vec(&status).unwrap();
    for policy in [SigningPolicy::Auto, SigningPolicy::Manual] {
        let channel_id = fiber_types::Hash256::from([1; 32]);
        let mut directory = HostedSessionState::default();
        directory
            .bindings
            .insert(channel_id, f.signer.channel_key_id());
        let root = RootSigner::open(RootKey::import([42; 32]).unwrap(), f.store.clone())
            .await
            .unwrap();
        let mut session = HostedSession::new(root)
            .with_state(directory)
            .with_policy(policy);
        let before = f.store.snapshot().unwrap();
        assert!(session
            .handle_channel_status(channel_id, serde_json::from_slice(&wire).unwrap(), 100)
            .await
            .is_err());
        assert_eq!(f.store.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn revocation_rechecks_successor_order_time_and_confirmation_revision() {
    let f = Fixture::new(false).await;
    f.signer
        .authorize_invoice(f.terms(true), [9; 32].into())
        .await
        .unwrap();
    f.sign(false, 0, f.opening()).await;
    f.sign(false, 1, f.incoming(false)).await;
    let (content, context) = f.revocation(false, 1).await;
    for attack in 0..4 {
        let mut changed = context.clone();
        match attack {
            0 => changed.replacement.tx_hash = [99; 32].into(),
            1 => changed.revoked.version = 2,
            2 => changed.replacement.version = 3,
            _ => (),
        }
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .prepare_revocation(
                content.clone(),
                changed,
                if attack == 3 { 19_000 } else { 100 }
            )
            .await
            .is_err());
        assert_eq!(f.store.snapshot().unwrap(), before);
    }
    let prepared = f
        .signer
        .prepare_revocation(content.clone(), context.clone(), 100)
        .await
        .unwrap();
    let before = f.store.snapshot().unwrap();
    assert!(f
        .signer
        .sign_revocation(prepared.clone(), 19_000)
        .await
        .is_err());
    assert_eq!(f.store.snapshot().unwrap(), before);
    f.sign(false, 2, f.incoming(false)).await;
    let before = f.store.snapshot().unwrap();
    assert!(f.signer.sign_revocation(prepared, 100).await.is_err());
    assert!(f
        .signer
        .prepare_revocation(content, context, 100)
        .await
        .is_err());
    assert_eq!(f.store.snapshot().unwrap(), before);
}

struct DescendantChain {
    chain: TestChain,
    ancestor: fiber_types::Hash256,
    valid_lineage: bool,
}
#[async_trait::async_trait]
impl ChainVerifier for DescendantChain {
    async fn live_cell(&self, point: &OutPoint) -> Result<VerifiedCell, SignerError> {
        self.chain.live_cell(point).await
    }
    async fn verify_commitment_lineage(
        &self,
        source: fiber_types::Hash256,
        point: &OutPoint,
    ) -> Result<(), SignerError> {
        if !self.valid_lineage || source != self.ancestor || point != &self.chain.outpoint {
            return Err(crate::commitment::invalid("incorrect lineage"));
        }
        Ok(())
    }
    async fn verify_maturity(&self, inputs: &[CellInput]) -> Result<(), SignerError> {
        self.chain.verify_maturity(inputs).await
    }
    async fn median_time_ms(&self) -> Result<u64, SignerError> {
        self.chain.median_time_ms().await
    }
}

#[tokio::test]
async fn timeout_then_descendant_settlement_rechecks_time_lineage_and_residual() {
    let f = Fixture::new(false).await;
    f.signer.authorize_payment(f.terms(false)).await.unwrap();
    f.sign(false, 0, f.opening()).await;
    f.sign(
        false,
        1,
        SettlementData {
            local_amount: 350,
            remote_amount: 600,
            tlcs: vec![f.tlc(false, false)],
        },
    )
    .await;
    let record = f.signer.recovery_records().await.unwrap().pop().unwrap();
    let source = record.transaction.raw().outputs().get(0).unwrap();
    let point = OutPoint::new(record.transaction.calc_tx_hash(), 0);
    let destination = Script::new_builder().args([42u8].pack()).build();
    let auxiliary = OutPoint::new([88u8; 32].pack(), 0);
    let mut chain = DescendantChain {
        ancestor: record.reference.tx_hash,
        valid_lineage: true,
        chain: TestChain {
            outpoint: point.clone(),
            cell: VerifiedCell {
                output: source.clone(),
                data: vec![],
            },
            now: 21_000,
            mature: true,
            extras: vec![(
                auxiliary.clone(),
                VerifiedCell {
                    output: CellOutput::new_builder()
                        .capacity(100u64)
                        .lock(destination.clone())
                        .build(),
                    data: vec![],
                },
            )],
        },
    };
    let body = fiber_types::settlement_data_to_witness(
        &record.settlement.data,
        false,
        record.settlement.local_settlement_key,
        record.settlement.remote_settlement_key,
    );
    let mut next = body.clone();
    next.drain(1..86);
    next[0] = 0;
    let residual = |body: &[u8], capacity: u64| {
        let mut args = source.lock().args().raw_data()[..36].to_vec();
        args.extend_from_slice(&blake2b_hash_with_salt(body, &[])[..20]);
        args.push(1);
        source
            .clone()
            .as_builder()
            .capacity(capacity)
            .lock(source.lock().as_builder().args(args.pack()).build())
            .build()
    };
    let witness_for = |body: &[u8], marker: u8| {
        let mut witness = vec![16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 1];
        witness.extend_from_slice(body);
        witness.extend_from_slice(&[marker, 0]);
        witness.extend_from_slice(&[0; 65]);
        witness
    };
    let tx = TransactionBuilder::default()
        .input(CellInput::new(
            point,
            u64::from_le_bytes(f.parameters.delay_epoch),
        ))
        .input(CellInput::new(auxiliary.clone(), (0x40u64 << 56) | 21))
        .output(residual(&next, 940))
        .output_data(ckb_types::packed::Bytes::default())
        .output(
            CellOutput::new_builder()
                .capacity(145u64)
                .lock(destination.clone())
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .witness(witness_for(&body, 0).pack())
        .build()
        .data();
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Tlc {
            commitment_number: 1,
        },
        transaction: tx.clone(),
    };
    let mut terms = OnchainSpendAuthorization {
        source: record.reference,
        destination: destination.clone(),
        fee: 5,
        additional_inputs: vec![auxiliary],
        cell_deps: vec![],
    };
    for attack in 0..5 {
        let mut changed = content.clone();
        match attack {
            0 => chain.chain.now = 20_999,
            1 | 2 => {
                let mut inputs: Vec<_> = tx.raw().inputs().into_iter().collect();
                inputs[1] = inputs[1]
                    .clone()
                    .as_builder()
                    .since(if attack == 1 { 0 } else { (0x40u64 << 56) | 20 })
                    .build();
                changed.transaction = tx
                    .clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_inputs(inputs)
                    .build()
                    .data();
            }
            3 => chain.valid_lineage = false,
            _ => chain.chain.mature = false,
        }
        let before = f.store.snapshot().unwrap();
        assert!(
            f.signer
                .prepare_onchain(changed, terms.clone(), &chain)
                .await
                .is_err(),
            "attack {attack}"
        );
        assert_eq!(f.store.snapshot().unwrap(), before);
        chain.chain.now = 21_000;
        chain.chain.mature = true;
        chain.valid_lineage = true;
    }
    let prepared = f
        .signer
        .prepare_onchain(content, terms.clone(), &chain)
        .await
        .unwrap();
    f.signer.sign_onchain(prepared, &chain).await.unwrap();
    // The timeout transaction is mined; settle the local balance from its residual cell.
    chain.chain.outpoint = OutPoint::new(tx.calc_tx_hash(), 0);
    chain.chain.cell.output = tx.raw().outputs().get(0).unwrap();
    chain.chain.extras.clear();
    terms.additional_inputs.clear();
    let mut final_body = next.clone();
    final_body[1..37].fill(0);
    let descendant = TransactionBuilder::default()
        .input(CellInput::new(chain.chain.outpoint.clone(), 0))
        .output(residual(&final_body, 590))
        .output_data(ckb_types::packed::Bytes::default())
        .output(
            CellOutput::new_builder()
                .capacity(345u64)
                .lock(destination)
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .witness(witness_for(&next, 0xfe).pack())
        .build()
        .data();
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: descendant.clone(),
    };
    for attack in 0..3 {
        let mut changed = content.clone();
        match attack {
            0 => chain.valid_lineage = false,
            1 => {
                changed.transaction = descendant
                    .clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_inputs(vec![CellInput::new(chain.chain.outpoint.clone(), 1)])
                    .build()
                    .data()
            }
            _ => {
                changed.transaction = descendant
                    .clone()
                    .into_view()
                    .as_advanced_builder()
                    .set_witnesses(vec![witness_for(&body, 0xfe).pack()])
                    .build()
                    .data()
            }
        }
        let before = f.store.snapshot().unwrap();
        assert!(f
            .signer
            .prepare_onchain(changed, terms.clone(), &chain)
            .await
            .is_err());
        assert_eq!(f.store.snapshot().unwrap(), before);
        chain.valid_lineage = true;
    }
    let prepared = f
        .signer
        .prepare_onchain(content, terms, &chain)
        .await
        .unwrap();
    chain.valid_lineage = false;
    let before = f.store.snapshot().unwrap();
    assert!(f
        .signer
        .sign_onchain(prepared.clone(), &chain)
        .await
        .is_err());
    assert_eq!(f.store.snapshot().unwrap(), before);
    chain.valid_lineage = true;
    f.signer.sign_onchain(prepared, &chain).await.unwrap();
}

#[cfg(feature = "json")]
async fn assert_onchain_rpc_rejected(
    f: &Fixture,
    content: &OnchainSigningContent,
    terms: &OnchainSpendAuthorization,
    chain: &impl ChainVerifier,
) {
    for policy in [SigningPolicy::Auto, SigningPolicy::Manual] {
        let channel_id = fiber_types::Hash256::from([1; 32]);
        let mut directory = HostedSessionState::default();
        directory
            .bindings
            .insert(channel_id, f.signer.channel_key_id());
        let root = RootSigner::open(RootKey::import([42; 32]).unwrap(), f.store.clone())
            .await
            .unwrap();
        let session = HostedSession::new(root)
            .with_state(directory)
            .with_policy(policy);
        let status = fiber_json_types::WatchtowerSigningStatus::SignatureRequired {
            request_id: fiber_types::Hash256::from([6; 32]).into(),
            content: fiber_json_types::OnchainSigningContent {
                key_purpose: fiber_json_types::OnchainKeyPurpose::Settlement,
                transaction: content.transaction.clone().into(),
            },
        };
        let wire = serde_json::to_vec(&status).unwrap();
        let before = f.store.snapshot().unwrap();
        assert!(session
            .handle_watchtower_status_verified(
                channel_id,
                serde_json::from_slice(&wire).unwrap(),
                terms.clone(),
                chain
            )
            .await
            .is_err());
        assert_eq!(f.store.snapshot().unwrap(), before);
    }
}

#[tokio::test]
async fn prepared_validation_rejects_wrong_signing_routes_without_mutation() {
    let f = Fixture::new(false).await;
    let (content, context) = f.request(false, 0, f.opening()).await;
    let prepared = f
        .signer
        .prepare_commitment(content, context, 100)
        .await
        .unwrap();
    assert!(prepared.commitment_context().is_some());
    assert!(prepared.commitment_review().is_some());
    assert!(prepared.revocation_context().is_none());
    assert!(prepared.onchain_authorization().is_none());
    let before = f.store.snapshot().unwrap();
    assert!(f.signer.sign(prepared.clone()).await.is_err());
    let error = f
        .signer
        .sign_revocation(prepared.clone(), 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected verified revocation"));
    assert!(f
        .signer
        .sign_onchain(prepared.clone(), &TestChain::funding(&f))
        .await
        .is_err());
    assert_eq!(f.store.snapshot().unwrap(), before);
    f.signer.sign_commitment(prepared, 100).await.unwrap();

    f.sign(false, 1, f.opening()).await;
    let (content, context) = f.revocation(false, 1).await;
    let prepared = f
        .signer
        .prepare_revocation(content, context, 100)
        .await
        .unwrap();
    assert!(prepared.revocation_context().is_some());
    assert!(prepared.commitment_context().is_none());
    assert!(prepared.commitment_review().is_none());
    assert!(prepared.onchain_authorization().is_none());
    let before = f.store.snapshot().unwrap();
    let error = f
        .signer
        .sign_commitment(prepared.clone(), 100)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expected verified commitment"));
    assert!(f.signer.sign(prepared.clone()).await.is_err());
    assert_eq!(f.store.snapshot().unwrap(), before);
    f.signer.sign_revocation(prepared, 100).await.unwrap();
}
