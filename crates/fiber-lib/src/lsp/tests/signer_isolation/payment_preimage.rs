//! Regression for obtaining a preimage through RPC before external TLC removal signing.

use std::time::Duration;

use fiber_json_types::{
    ChannelSigningStatus, ChannelSigningTransition, GetPaymentCommandParams,
    GetPaymentCommandResult, PaymentStatus, SubmitChannelSignatureParams,
    SubmitChannelSignatureResult,
};
use fiber_lsp_sdk::{
    ChannelSignature, ChannelSigningContent, CommitmentContext, OwnedSettlementBinding,
    PaymentAuthorization, SignerError,
};
use jsonrpsee::{core::client::ClientT, rpc_params};

use crate::{fiber_types::Hash256, invoice::CkbInvoiceStatus};

use super::{open_hosted_external_channel, ExternalSignerClient, TestLspFixture};

#[tokio::test]
async fn test_hosted_payment_preimage_available_before_removal_signature() {
    let fixture = TestLspFixture::new("payment-preimage").await;
    let (_, _, _, client) = fixture.register_tenant(1).await;
    let (channel_id, payer) = open_hosted_external_channel(&fixture, &client).await;
    payer.finish_opening(&fixture.public_t, channel_id).await;
    let balance_before = fixture
        .public_t
        .get_channel_actor_state(channel_id)
        .to_remote_amount;

    let amount = 100_000_000;
    let (invoice, receiver_preimage) = fixture.public_t.gen_basic_invoice(amount);
    let payment_hash = *invoice.payment_hash();
    payer
        .signer
        .authorize_payment(PaymentAuthorization {
            payment_hash,
            hash_algorithm: invoice.hash_algorithm().copied().unwrap_or_default(),
            inbound: false,
            amount,
            expires_at_ms: crate::now_timestamp_as_millis_u64() + 120_000,
            min_tlc_expiry_delta_ms: 1,
            max_tlc_expiry_ms: u64::MAX,
        })
        .await
        .unwrap();

    let sent: GetPaymentCommandResult = client
        .request(
            "send_payment",
            rpc_params![serde_json::json!({
                "invoice": invoice.to_string(),
                "max_fee_amount": "0x0",
                "max_parts": "0x1",
            })],
        )
        .await
        .unwrap();
    assert_eq!(sent.payment_hash, payment_hash.into());
    assert!(sent.payment_preimage.is_none());

    let mut observed_inflight_preimage = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // Query after the signing request exists, so receipt of RemoveTlc
            // has already persisted its preimage before removal is signed.
            let status = payer.get_signing_status(channel_id).await.unwrap().status;
            let payment: GetPaymentCommandResult = client
                .request(
                    "get_payment",
                    rpc_params![GetPaymentCommandParams {
                        payment_hash: payment_hash.into()
                    }],
                )
                .await
                .unwrap();
            if let Some(preimage) = payment.payment_preimage {
                observed_inflight_preimage |= payment.status == PaymentStatus::Inflight;
                // The payer only authorizes evidence obtained through its tenant RPC.
                payer
                    .signer
                    .authorize_fulfillment(payment_hash, preimage.into())
                    .await
                    .unwrap();
            }
            if matches!(status, ChannelSigningStatus::SignatureRequired { .. }) {
                let submission = prepare_without_preimage_shortcut(&payer, channel_id, status)
                    .await
                    .expect("RPC must supply the preimage before TLC removal signing");
                assert_eq!(
                    payer.submit(submission).await.unwrap(),
                    SubmitChannelSignatureResult::Applied
                );
            } else {
                let state = fixture.public_t.get_channel_actor_state(channel_id);
                if payment.status == PaymentStatus::Success
                    && state.tlc_state.all_tlcs().count() == 0
                    && !state.tlc_state.waiting_ack
                {
                    assert_eq!(payment.payment_preimage, Some(receiver_preimage.into()));
                    assert_eq!(state.to_remote_amount, balance_before - amount);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("payment must settle using RPC evidence alone");
    assert!(
        observed_inflight_preimage,
        "preimage must be available before settlement"
    );
    assert_eq!(
        fixture.public_t.get_invoice_status(&payment_hash),
        Some(CkbInvoiceStatus::Paid),
    );
}

// The shared test driver injects fixture preimages at removal. This driver
// requires explicit invoice/RPC evidence to authorize each resolution.
async fn prepare_without_preimage_shortcut(
    sdk: &ExternalSignerClient,
    channel_id: Hash256,
    status: ChannelSigningStatus,
) -> Result<SubmitChannelSignatureParams, SignerError> {
    let ChannelSigningStatus::SignatureRequired {
        request_id,
        transition,
        content,
        settlement,
        session_evidence,
    } = status.clone()
    else {
        panic!("expected a pending signing request")
    };
    let transition = match transition {
        ChannelSigningTransition::SendCommitmentSigned => {
            crate::fiber_types::ChannelSigningTransition::SendCommitmentSigned
        }
        ChannelSigningTransition::CompleteReceivedCommitment => {
            crate::fiber_types::ChannelSigningTransition::CompleteReceivedCommitment
        }
        _ => return Ok(sdk.prepare_submission(channel_id, status).await),
    };
    let (data, local_settlement_key, remote_settlement_key, for_remote) =
        fiber_lsp_sdk::json::settlement_from_rpc(&settlement.unwrap()).unwrap();
    let content = fiber_lsp_sdk::json::musig2_from_rpc(content).unwrap();
    let slot = content.slot;
    let now = crate::now_timestamp_as_millis_u64();
    let prepared = sdk
        .signer
        .prepare_commitment(
            ChannelSigningContent::Musig2(content),
            CommitmentContext {
                transition,
                session: fiber_lsp_sdk::json::session_evidence_from_rpc(session_evidence).unwrap(),
                settlement: OwnedSettlementBinding {
                    data,
                    local_settlement_key,
                    remote_settlement_key,
                    for_remote: Some(for_remote),
                },
            },
            now,
        )
        .await?;
    let ChannelSignature::Musig2(signature) = sdk.signer.sign_commitment(prepared, now).await?
    else {
        panic!("expected MuSig2 signature")
    };
    let next = sdk.signer.next_material(slot).await?;
    Ok(SubmitChannelSignatureParams {
        channel_id: channel_id.into(),
        request_id,
        partial_signature: signature.partial_signature.serialize(),
        next_material: Some(fiber_lsp_sdk::json::next_material_to_rpc(&next)),
    })
}
