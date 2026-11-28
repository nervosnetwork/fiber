use ckb_types::prelude::*;
use fiber_json_types::{
    ChannelSigningTransition, LspTenantRuntimeStatus, LspTenantStatus, RegisterLspTenantResult,
};
use fiber_types::Privkey;

use super::*;
use crate::{json::musig2_to_rpc, MemoryStore, RootKey};

fn root_key() -> RootKey {
    RootKey::import([42; 32]).expect("root key")
}

fn lsp_pubkey() -> fiber_types::Pubkey {
    Privkey::from(&[7; 32]).pubkey()
}

async fn session() -> HostedSession<MemoryStore> {
    let created = RootSigner::create(root_key(), MemoryStore::default())
        .await
        .expect("create root");
    HostedSession::new(created)
}

fn registration_request() -> (fiber_types::Pubkey, Hash256, u64) {
    (lsp_pubkey(), Hash256::from([3; 32]), 1_700_000_000_123)
}

#[tokio::test]
async fn new_session_defaults_to_auto() {
    let session = session().await;
    assert_eq!(session.policy, SigningPolicy::Auto);
}

#[tokio::test]
async fn registration_binds_trusted_lsp_and_client_request() {
    let session = session().await;
    let (lsp_node_id, request_id, signed_at) = registration_request();
    let params = session
        .begin_registration(lsp_node_id, request_id, signed_at)
        .expect("begin");
    assert_eq!(params.request_id, request_id.into());
    assert_eq!(params.signed_at, signed_at);
    let root_signer_pubkey: fiber_types::Pubkey = session.identity_public_key().into();
    assert_eq!(params.root_signer_pubkey, root_signer_pubkey.into());

    let payload = TenantRegistryPayload::new(
        lsp_node_id,
        session.identity_public_key().into(),
        request_id.into(),
        signed_at,
    );
    let signature = fiber_types::TenantRegistrySignature::from_slice(
        &hex::decode(&params.signature).expect("signature hex"),
    )
    .expect("signature");
    payload
        .verify_signature(&signature)
        .expect("registration signature");
}

#[tokio::test]
async fn registration_round_trip_stores_the_token() {
    let mut session = session().await;
    let (lsp_node_id, request_id, signed_at) = registration_request();
    let params = session
        .begin_registration(lsp_node_id, request_id, signed_at)
        .expect("begin");
    session
        .finish_registration(RegisterLspTenantResult {
            tenant: LspTenantStatus {
                tenant_id: session.tenant_id().as_str().to_string(),
                root_signer_pubkey: Some(params.root_signer_pubkey),
                invoice_pubkey: lsp_pubkey().into(),
                channels: Vec::new(),
                created_at: 1,
                runtime_status: LspTenantRuntimeStatus::Cold,
            },
            access_token: "token".to_string(),
            expires_at_ms: 3_601_000,
        })
        .expect("finish");
    assert_eq!(session.tenant_token(), Some("token"));
}

#[tokio::test]
async fn opening_consumes_the_pending_channel() {
    let mut session = session().await;
    session.allocate_pending_channel().await.unwrap();
    let pending = session.pending_channel_key_id().unwrap();
    let signer = session.open_channel(pending).await.unwrap();
    let (request, result, network, inputs) = crate::opening::tests::proposal(&signer).await;
    session.begin_channel_opening(request).await.unwrap();
    session
        .finish_channel_opening(&result, &network, &inputs)
        .await
        .unwrap();
    assert!(session.pending_channel_key_id().is_none());
    assert_eq!(session.binding(result.channel_id.into()), Some(pending));
}

async fn bound_session_with_inbound() -> (
    HostedSession<MemoryStore>,
    Hash256,
    ChannelSigningStatus,
    Hash256,
) {
    let f = crate::commitment_tests::Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    let (content, context) = f.request(true, 1, f.incoming(true)).await;
    let ChannelSigningContent::Musig2(content) = content else {
        panic!()
    };
    let snapshot = context.settlement;
    let settlement = fiber_json_types::SigningSettlement {
        local_amount: snapshot.data.local_amount,
        remote_amount: snapshot.data.remote_amount,
        local_settlement_pubkey: snapshot.local_settlement_key.into(),
        remote_settlement_pubkey: snapshot.remote_settlement_key.into(),
        for_remote: true,
        tlcs: snapshot
            .data
            .tlcs
            .iter()
            .map(|tlc| fiber_json_types::SigningSettlementTlc {
                tlc_id: u64::from(tlc.tlc_id),
                local_key_commitment_number: tlc.local_key_commitment_number.unwrap(),
                inbound: true,
                payment_hash: tlc.payment_hash.into(),
                payment_amount: tlc.payment_amount,
                hash_algorithm: tlc.hash_algorithm.into(),
                expiry: tlc.expiry,
                local_key_pubkey: tlc.local_pubkey().into(),
                remote_key: tlc.remote_key.into(),
            })
            .collect(),
    };
    let channel_id = Hash256::from([0x22; 32]);
    let hash = f.terms(true).payment_hash;
    let mut session = HostedSession::new(f.root);
    session
        .state
        .bindings
        .insert(channel_id, f.signer.channel_key_id());
    let status = ChannelSigningStatus::SignatureRequired {
        session_evidence: crate::json::session_evidence_to_rpc(&context.session),
        request_id: Hash256::from([0x33; 32]).into(),
        transition: ChannelSigningTransition::SendCommitmentSigned,
        content: musig2_to_rpc(&content),
        settlement: Some(settlement),
    };
    (session, channel_id, status, hash)
}

async fn authorize_receive(
    session: &HostedSession<MemoryStore>,
    channel_id: Hash256,
    payment_hash: Hash256,
) {
    session
        .channel_signer(channel_id)
        .await
        .unwrap()
        .authorize_invoice(
            crate::PaymentAuthorization {
                payment_hash,
                hash_algorithm: fiber_types::HashAlgorithm::CkbHash,
                inbound: true,
                amount: 50,
                expires_at_ms: 10_000,
                min_tlc_expiry_delta_ms: 1000,
                max_tlc_expiry_ms: 100_000,
            },
            [9; 32].into(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn auto_allows_an_issued_inbound_commitment() {
    let (mut session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
    authorize_receive(&session, channel_id, payment_hash).await;
    session.registry_mut().note_signed_balance(10);
    let outcome = session
        .handle_channel_status(channel_id, status, 0)
        .await
        .expect("handle");
    assert!(matches!(outcome, ProcessOutcome::ReadyToSubmit(_)));
}

#[tokio::test]
async fn auto_denies_an_unissued_inbound_commitment() {
    let (mut session, channel_id, status, _) = bound_session_with_inbound().await;
    session.registry_mut().note_signed_balance(10);
    let error = session
        .handle_channel_status(channel_id, status, 0)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no local payment authorization"));
}

#[tokio::test]
async fn hash_only_registry_cannot_authorize_an_unverified_receipt() {
    let (mut session, channel_id, status, hash) = bound_session_with_inbound().await;
    session.registry_mut().record_issued_invoice(hash);
    session.registry_mut().note_signed_balance(0);
    assert!(session
        .handle_channel_status(channel_id, status, 0)
        .await
        .unwrap_err()
        .to_string()
        .contains("no local payment authorization"));
}

#[tokio::test]
async fn manual_asks_before_signing() {
    let (session, channel_id, status, payment_hash) = bound_session_with_inbound().await;
    let mut session = session.with_policy(SigningPolicy::Manual);
    authorize_receive(&session, channel_id, payment_hash).await;
    let outcome = session
        .handle_channel_status(channel_id, status, 0)
        .await
        .expect("handle");
    assert!(matches!(outcome, ProcessOutcome::NeedConfirmation(_)));
}

fn watchtower_required_status() -> WatchtowerSigningStatus {
    let tx = ckb_types::core::TransactionBuilder::default()
        .output(
            ckb_types::packed::CellOutput::new_builder()
                .capacity(1000u64)
                .build(),
        )
        .output_data(ckb_types::packed::Bytes::default())
        .build()
        .data();
    WatchtowerSigningStatus::ExternalFundingRequired {
        request_id: Hash256::from([0x44; 32]).into(),
        content: fiber_json_types::OnchainSigningContent {
            key_purpose: fiber_json_types::OnchainKeyPurpose::Settlement,
            transaction: tx.into(),
        },
        expires_at_ms: 0,
        required_expiry_since: None,
    }
}

#[tokio::test]
async fn watchtower_status_result_rejects_missing_onchain_authorization() {
    struct UnreachableChain;

    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    impl crate::ChainVerifier for UnreachableChain {
        async fn live_cell(
            &self,
            _out_point: &ckb_types::packed::OutPoint,
        ) -> Result<crate::VerifiedCell, crate::SignerError> {
            unreachable!("authorization check runs before chain queries")
        }

        async fn verify_commitment_lineage(
            &self,
            _source: Hash256,
            _outpoint: &ckb_types::packed::OutPoint,
        ) -> Result<(), crate::SignerError> {
            unreachable!("authorization check runs before chain queries")
        }

        async fn verify_maturity(
            &self,
            _inputs: &[ckb_types::packed::CellInput],
        ) -> Result<(), crate::SignerError> {
            unreachable!("authorization check runs before chain queries")
        }

        async fn median_time_ms(&self) -> Result<u64, crate::SignerError> {
            unreachable!("authorization check runs before chain queries")
        }
    }

    let (session, channel_id, _, _) = bound_session_with_inbound().await;
    let result = GetWatchtowerSigningStatusResult {
        channel_id: channel_id.into(),
        status: watchtower_required_status(),
    };
    let mut handler = crate::NoopWatchtowerEventHandler;
    assert!(session
        .handle_watchtower_status_result(result.clone(), 0, None, &UnreachableChain, &mut handler,)
        .await
        .is_err());

    let manual = session.with_policy(SigningPolicy::Manual);
    assert!(manual
        .handle_watchtower_status_result(result, 0, None, &UnreachableChain, &mut handler)
        .await
        .is_err());
}

#[tokio::test]
async fn watch_channel_params_omit_the_settlement_secret() {
    let (session, channel_id, _, _) = bound_session_with_inbound().await;
    let params = session
        .watch_channel_params(
            channel_id,
            Privkey::from(&[8; 32]).pubkey(),
            Privkey::from(&[9; 32]).pubkey(),
            None,
            fiber_json_types::SettlementData {
                local_amount: 1,
                remote_amount: 1,
                tlcs: Vec::new(),
            },
            fiber_types::ChannelFeatures::V2,
        )
        .await
        .expect("watch params");
    assert!(params.local_settlement_key.is_none());
    assert!(params.local_settlement_key_pubkey.is_some());
    assert_eq!(params.channel_id, channel_id.into());
}

#[tokio::test]
async fn outbound_refund_auto_authorizes_cancel_while_fulfill_and_inbound_cancel_still_require_authorization(
) {
    let make_status =
        |content: ChannelSigningContent, context: crate::CommitmentContext, for_remote: bool| {
            let ChannelSigningContent::Musig2(content) = content else {
                panic!("expected musig2")
            };
            let snapshot = context.settlement;
            let settlement = fiber_json_types::SigningSettlement {
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
                        payment_amount: tlc.payment_amount,
                        hash_algorithm: tlc.hash_algorithm.into(),
                        expiry: tlc.expiry,
                        local_key_pubkey: tlc.local_pubkey().into(),
                        remote_key: tlc.remote_key.into(),
                    })
                    .collect(),
            };
            ChannelSigningStatus::SignatureRequired {
                session_evidence: crate::json::session_evidence_to_rpc(&context.session),
                request_id: Hash256::from([0x33; 32]).into(),
                transition: if for_remote {
                    ChannelSigningTransition::SendCommitmentSigned
                } else {
                    ChannelSigningTransition::CompleteReceivedCommitment
                },
                content: musig2_to_rpc(&content),
                settlement: Some(settlement),
            }
        };

    // 1. Outbound TLC refunded back to `local_amount` must NOT deadlock waiting for RPC `Failed`.
    let f = crate::commitment_tests::Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.sign(false, 0, f.opening()).await;
    f.signer.authorize_payment(f.terms(false)).await.unwrap();
    let outbound_local = fiber_types::SettlementData {
        local_amount: 350,
        remote_amount: 600,
        tlcs: vec![f.tlc(false, false)],
    };
    let outbound_remote = fiber_types::SettlementData {
        local_amount: 350,
        remote_amount: 600,
        tlcs: vec![f.tlc(false, true)],
    };
    f.sign(true, 1, outbound_remote).await;
    f.sign(false, 1, outbound_local).await;

    let channel_id = Hash256::from([0x44; 32]);
    let key_id = f.signer.channel_key_id();
    let (pay_remote_content, pay_remote_ctx) = f
        .request(
            false,
            2,
            fiber_types::SettlementData {
                local_amount: 350,
                remote_amount: 650,
                tlcs: vec![],
            },
        )
        .await;
    let (refund_content, refund_ctx) = f.request(false, 2, f.opening()).await;
    let mut session = HostedSession::new(f.root);
    session.state.bindings.insert(channel_id, key_id);

    // Removing an outbound TLC that pays `remote_amount` without `authorize_fulfillment`
    // must still be rejected.
    let err = session
        .handle_channel_status(
            channel_id,
            make_status(pay_remote_content, pay_remote_ctx, false),
            100,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("locally authorized resolution"),
        "unexpected error: {err}"
    );

    // Removing the same outbound TLC back into `local_amount` (refund) auto-authorizes
    // cancellation so HostedTenant can finish removing the failed TLC.
    let outcome = session
        .handle_channel_status(
            channel_id,
            make_status(refund_content, refund_ctx, false),
            100,
        )
        .await
        .expect("outbound refund should auto-authorize cancel");
    let ProcessOutcome::NeedConfirmation(pending) = outcome else {
        panic!("expected confirmation for outbound balance transition, got {outcome:?}");
    };
    let _ = session
        .confirm(pending, 100)
        .await
        .expect("confirming refunded outbound commitment should succeed");

    // 2. Inbound TLC cancelled back to `remote_amount` must STILL require explicit authorization.
    let f_in = crate::commitment_tests::Fixture::new(false).await;
    f_in.sign(false, 0, f_in.opening()).await;
    f_in.signer
        .authorize_invoice(f_in.terms(true), [9; 32].into())
        .await
        .unwrap();
    f_in.sign(false, 1, f_in.incoming(false)).await;
    let (in_cancel_content, in_cancel_ctx) = f_in.request(false, 2, f_in.opening()).await;
    let in_key_id = f_in.signer.channel_key_id();
    let mut in_session = HostedSession::new(f_in.root);
    in_session.state.bindings.insert(channel_id, in_key_id);
    let in_err = in_session
        .handle_channel_status(
            channel_id,
            make_status(in_cancel_content, in_cancel_ctx, false),
            100,
        )
        .await
        .unwrap_err();
    assert!(
        in_err.to_string().contains("locally authorized resolution"),
        "unexpected inbound cancel error: {in_err}"
    );
}

fn make_commitment_status(
    content: ChannelSigningContent,
    context: crate::CommitmentContext,
    for_remote: bool,
) -> ChannelSigningStatus {
    let ChannelSigningContent::Musig2(content) = content else {
        panic!("expected musig2")
    };
    let snapshot = context.settlement;
    let settlement = fiber_json_types::SigningSettlement {
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
                payment_amount: tlc.payment_amount,
                hash_algorithm: tlc.hash_algorithm.into(),
                expiry: tlc.expiry,
                local_key_pubkey: tlc.local_pubkey().into(),
                remote_key: tlc.remote_key.into(),
            })
            .collect(),
    };
    ChannelSigningStatus::SignatureRequired {
        session_evidence: crate::json::session_evidence_to_rpc(&context.session),
        request_id: Hash256::from([0x33; 32]).into(),
        transition: if for_remote {
            ChannelSigningTransition::SendCommitmentSigned
        } else {
            ChannelSigningTransition::CompleteReceivedCommitment
        },
        content: musig2_to_rpc(&content),
        settlement: Some(settlement),
    }
}

#[tokio::test]
async fn dismissed_outbound_refund_does_not_block_subsequent_fulfillment() {
    let f = crate::commitment_tests::Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.sign(false, 0, f.opening()).await;
    let terms = f.terms(false);
    let payment_hash = terms.payment_hash;
    f.signer.authorize_payment(terms).await.unwrap();
    let outbound_local = fiber_types::SettlementData {
        local_amount: 350,
        remote_amount: 600,
        tlcs: vec![f.tlc(false, false)],
    };
    let outbound_remote = fiber_types::SettlementData {
        local_amount: 350,
        remote_amount: 600,
        tlcs: vec![f.tlc(false, true)],
    };
    f.sign(true, 1, outbound_remote).await;
    f.sign(false, 1, outbound_local).await;

    let channel_id = Hash256::from([0x45; 32]);
    let key_id = f.signer.channel_key_id();
    let (refund_content, refund_ctx) = f.request(false, 2, f.opening()).await;
    let (fulfill_content, fulfill_ctx) = f
        .request(
            false,
            2,
            fiber_types::SettlementData {
                local_amount: 350,
                remote_amount: 650,
                tlcs: vec![],
            },
        )
        .await;

    let mut session = HostedSession::new(f.root);
    session.state.bindings.insert(channel_id, key_id);

    // Inspecting an outbound refund produces a confirmation prompt, but must NOT persist
    // `Resolution::Cancel` before the wallet actually confirms and signs.
    let outcome = session
        .handle_channel_status(
            channel_id,
            make_commitment_status(refund_content, refund_ctx, false),
            100,
        )
        .await
        .expect("outbound refund should prepare for confirmation");
    let ProcessOutcome::NeedConfirmation(pending_refund) = outcome else {
        panic!("expected confirmation for refund, got {outcome:?}");
    };
    // Dismiss without confirming.
    drop(pending_refund);

    // Because nothing was persisted on dismissal, authorizing the real preimage and signing
    // the fulfillment commitment paying `remote_amount` must still succeed.
    f.signer
        .authorize_fulfillment(payment_hash, [9; 32].into())
        .await
        .expect("dismissed refund must not reject valid preimage authorization");
    let fulfill_outcome = session
        .handle_channel_status(
            channel_id,
            make_commitment_status(fulfill_content, fulfill_ctx, false),
            100,
        )
        .await
        .expect("fulfillment commitment should validate after refund dismissal");
    let ProcessOutcome::NeedConfirmation(pending_fulfill) = fulfill_outcome else {
        panic!("expected confirmation for fulfillment, got {fulfill_outcome:?}");
    };
    let _ = session
        .confirm(pending_fulfill, 100)
        .await
        .expect("confirming fulfillment after dismissed refund should succeed");
}

#[tokio::test]
async fn outbound_mixed_fulfill_and_refund_batch_signs_together() {
    let f = crate::commitment_tests::Fixture::new(false).await;
    f.sign(true, 0, f.opening()).await;
    f.sign(false, 0, f.opening()).await;

    let preimage_1 = Hash256::from([0x11; 32]);
    let preimage_2 = Hash256::from([0x22; 32]);
    let mut terms_1 = f.terms(false);
    terms_1.payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage_1).into();
    let mut terms_2 = f.terms(false);
    terms_2.payment_hash = fiber_types::HashAlgorithm::CkbHash.hash(preimage_2).into();
    f.signer.authorize_payment(terms_1.clone()).await.unwrap();
    f.signer.authorize_payment(terms_2.clone()).await.unwrap();

    let make_tlc = |id: u64, hash: Hash256, for_remote: bool| {
        let mut tlc = f.tlc(false, for_remote);
        tlc.tlc_id = if for_remote {
            fiber_types::TLCId::Offered(id)
        } else {
            fiber_types::TLCId::Received(id)
        };
        tlc.payment_hash = hash;
        tlc
    };

    // Start from balances 300/600 with two distinct 50-unit outbound TLCs locked.
    let both_remote = fiber_types::SettlementData {
        local_amount: 300,
        remote_amount: 600,
        tlcs: vec![
            make_tlc(1, terms_1.payment_hash, true),
            make_tlc(2, terms_2.payment_hash, true),
        ],
    };
    let both_local = fiber_types::SettlementData {
        local_amount: 300,
        remote_amount: 600,
        tlcs: vec![
            make_tlc(1, terms_1.payment_hash, false),
            make_tlc(2, terms_2.payment_hash, false),
        ],
    };
    f.sign(true, 1, both_remote).await;
    f.sign(false, 1, both_local).await;

    // Authorize fulfillment for TLC 1 (+50 remote) while leaving TLC 2 pending (refunded +50 local).
    f.signer
        .authorize_fulfillment(terms_1.payment_hash, preimage_1)
        .await
        .unwrap();

    let channel_id = Hash256::from([0x46; 32]);
    let key_id = f.signer.channel_key_id();
    let (mixed_content, mixed_ctx) = f
        .request(
            false,
            2,
            fiber_types::SettlementData {
                local_amount: 350,
                remote_amount: 650,
                tlcs: vec![],
            },
        )
        .await;

    let mut session = HostedSession::new(f.root);
    session.state.bindings.insert(channel_id, key_id);
    let outcome = session
        .handle_channel_status(
            channel_id,
            make_commitment_status(mixed_content, mixed_ctx, false),
            100,
        )
        .await
        .expect("mixed fulfill + refund batch should validate into 350/650");
    let ProcessOutcome::NeedConfirmation(pending) = outcome else {
        panic!("expected confirmation for mixed batch, got {outcome:?}");
    };
    let _ = session
        .confirm(pending, 100)
        .await
        .expect("confirming mixed fulfill + refund batch should succeed");
}
