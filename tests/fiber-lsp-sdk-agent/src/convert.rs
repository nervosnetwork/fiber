//! Valid protocol fixtures for the agent's checked signing path.

#[cfg(test)]
pub(crate) mod tests {
    use ckb_types::{
        core::TransactionBuilder,
        packed::{CellInput, CellOutput, OutPoint, Script, Transaction},
        prelude::*,
    };
    use fiber_lsp_sdk::{
        ChannelSigner, CommitmentCounter, CommitmentParameters, Musig2SignableContent,
        Musig2SigningContent, NoncePurpose, NonceSlot, SignerStore,
    };
    use fiber_types::{InMemorySigner, SettlementData};
    use musig2::{AggNonce, KeyAggContext, SecNonce};

    fn remote() -> InMemorySigner {
        InMemorySigner::generate_from_seed(b"agent fixture remote")
    }

    pub(crate) fn parameters() -> CommitmentParameters {
        let remote = remote();
        CommitmentParameters {
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
            commitment_fee: 0,
            local_amount: 40_000_000_000,
            remote_amount: 60_000_000_000,
            local_reserved_ckb_amount: 9_900_000_000,
            remote_reserved_ckb_amount: 9_900_000_000,
        }
    }
    pub(crate) fn remote_binding_pubkey() -> fiber_types::Pubkey {
        remote().funding_key.pubkey()
    }
    pub(crate) fn bound_funding_outpoint() -> OutPoint {
        OutPoint::new([7; 32].pack(), 0)
    }
    pub(crate) fn bound_shutdown_script() -> Script {
        Script::default()
    }

    pub(crate) async fn commitment_status<S: SignerStore>(
        signer: &ChannelSigner<S>,
        funding: &Transaction,
        for_remote: bool,
    ) -> fiber_json_types::ChannelSigningStatus {
        let remote = remote();
        let keys = signer.public_material().base_public_keys;
        let mut funding_keys = [keys.funding_pubkey, remote.funding_key.pubkey()];
        funding_keys.sort();
        let ctx = KeyAggContext::new(funding_keys).unwrap();
        let lock_ctx = KeyAggContext::new(if for_remote {
            [keys.funding_pubkey, remote.funding_key.pubkey()]
        } else {
            [remote.funding_key.pubkey(), keys.funding_pubkey]
        })
        .unwrap();
        let point: musig2::secp::Point = lock_ctx.aggregated_pubkey();
        let data = SettlementData {
            local_amount: 40_000_000_000,
            remote_amount: 60_000_000_000,
            tlcs: vec![],
        };
        let mut args =
            fiber_types::blake2b_hash_with_salt(&point.serialize_xonly(), &[])[..20].to_vec();
        args.extend_from_slice(&parameters().delay_epoch);
        args.extend_from_slice(&1u64.to_be_bytes());
        args.extend_from_slice(&fiber_types::settlement_witness_hash(
            &data,
            for_remote,
            keys.tlc_base_key,
            remote.tlc_base_key.pubkey(),
        ));
        args.push(0);
        let tx = TransactionBuilder::default()
            .input(CellInput::new(OutPoint::new(funding.calc_tx_hash(), 0), 0))
            .output(
                CellOutput::new_builder()
                    .capacity(100_000_000_000u64)
                    .lock(
                        parameters()
                            .commitment_lock
                            .as_builder()
                            .args(args.pack())
                            .build(),
                    )
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 1,
        };
        let local_nonce = signer.get_musig2_nonce(slot).await.unwrap().public_nonce;
        let peer = SecNonce::build([7; 32]).build();
        let peer_nonce = peer.public_nonce();
        let agg_nonce = AggNonce::sum([local_nonce, peer_nonce.clone()]);
        let content = Musig2SignableContent::CommitmentTransaction(tx);
        let partial: musig2::PartialSignature = musig2::sign_partial(
            &ctx,
            remote.funding_key.clone(),
            peer,
            &agg_nonce,
            content.signing_message(),
        )
        .unwrap();
        fiber_json_types::ChannelSigningStatus::SignatureRequired {
            request_id: fiber_json_types::Hash256([0x22; 32]),
            transition: if for_remote {
                fiber_json_types::ChannelSigningTransition::SendCommitmentSigned
            } else {
                fiber_json_types::ChannelSigningTransition::CompleteReceivedCommitment
            },
            session_evidence: fiber_json_types::SigningSessionEvidence {
                peer_public_nonce: peer_nonce.serialize().to_vec(),
                peer_partial_signature: (!for_remote).then(|| partial.serialize().to_vec()),
            },
            content: fiber_lsp_sdk::json::musig2_to_rpc(&Musig2SigningContent {
                slot,
                commitment_counter: Some(CommitmentCounter::Local),
                key_agg_ctx: ctx,
                agg_nonce,
                content,
            }),
            settlement: Some(fiber_json_types::SigningSettlement {
                local_amount: 40_000_000_000,
                remote_amount: 60_000_000_000,
                local_settlement_pubkey: keys.tlc_base_key.into(),
                remote_settlement_pubkey: remote.tlc_base_key.pubkey().into(),
                for_remote,
                tlcs: vec![],
            }),
        }
    }

    pub(crate) struct Chain {
        pub point: OutPoint,
        pub cell: fiber_lsp_sdk::VerifiedCell,
        pub live: bool,
    }
    #[async_trait::async_trait]
    impl fiber_lsp_sdk::ChainVerifier for Chain {
        async fn live_cell(
            &self,
            point: &OutPoint,
        ) -> Result<fiber_lsp_sdk::VerifiedCell, fiber_lsp_sdk::SignerError> {
            if !self.live || point != &self.point {
                return Err(fiber_lsp_sdk::SignerError::InvalidContent(
                    "spent input".into(),
                ));
            }
            Ok(self.cell.clone())
        }
        async fn verify_commitment_lineage(
            &self,
            source: fiber_types::Hash256,
            point: &OutPoint,
        ) -> Result<(), fiber_lsp_sdk::SignerError> {
            if source != self.point.tx_hash().into() || point != &self.point {
                return Err(fiber_lsp_sdk::SignerError::InvalidContent(
                    "wrong lineage".into(),
                ));
            }
            Ok(())
        }
        async fn verify_maturity(
            &self,
            inputs: &[CellInput],
        ) -> Result<(), fiber_lsp_sdk::SignerError> {
            if inputs.len() != 1
                || inputs[0].since() != u64::from_le_bytes(parameters().delay_epoch).pack()
            {
                return Err(fiber_lsp_sdk::SignerError::InvalidContent(
                    "wrong maturity".into(),
                ));
            }
            Ok(())
        }
        async fn median_time_ms(&self) -> Result<u64, fiber_lsp_sdk::SignerError> {
            Ok(100)
        }
    }

    pub(crate) fn settlement(
        record: &fiber_lsp_sdk::RecoveryRecord,
    ) -> (
        fiber_json_types::WatchtowerSigningStatus,
        fiber_lsp_sdk::OnchainSpendAuthorization,
        Chain,
    ) {
        assert!(!record.reference.for_remote);
        assert!(record.complete_signature.is_some());
        let source = record.transaction.raw().outputs().get(0).unwrap();
        let point = OutPoint::new(record.transaction.calc_tx_hash(), 0);
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
        let destination = Script::new_builder().args([42u8].pack()).build();
        let mut witness = vec![16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 1];
        witness.extend_from_slice(&body);
        witness.extend_from_slice(&[0xfe, 0]);
        witness.extend_from_slice(&[0; 65]);
        let tx = TransactionBuilder::default()
            .input(CellInput::new(
                point.clone(),
                u64::from_le_bytes(parameters().delay_epoch),
            ))
            .output(
                source
                    .clone()
                    .as_builder()
                    .capacity(60_000_000_000u64)
                    .lock(source.lock().as_builder().args(args.pack()).build())
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .output(
                CellOutput::new_builder()
                    .capacity(39_999_999_995u64)
                    .lock(destination.clone())
                    .build(),
            )
            .output_data(ckb_types::packed::Bytes::default())
            .witness(witness.pack())
            .build()
            .data();
        (
            fiber_json_types::WatchtowerSigningStatus::SignatureRequired {
                request_id: fiber_json_types::Hash256([0x33; 32]),
                content: fiber_json_types::OnchainSigningContent {
                    key_purpose: fiber_json_types::OnchainKeyPurpose::Settlement,
                    transaction: tx.into(),
                },
            },
            fiber_lsp_sdk::OnchainSpendAuthorization {
                source: record.reference.clone(),
                destination,
                fee: 5,
                additional_inputs: vec![],
                cell_deps: vec![],
            },
            Chain {
                point,
                cell: fiber_lsp_sdk::VerifiedCell {
                    output: source,
                    data: vec![],
                },
                live: true,
            },
        )
    }
}
