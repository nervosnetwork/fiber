//! Test helpers that sit on top of [`fiber_lsp_sdk::json`].

#[cfg(test)]
pub(crate) use fiber_lsp_sdk::json::musig2_to_rpc;

#[cfg(test)]
pub(crate) mod tests {
    use fiber_lsp_sdk::{
        ChannelSigner, CommitmentCounter, Musig2SignableContent, Musig2SigningContent,
        NoncePurpose, NonceSlot,
    };
    use fiber_types::Pubkey;
    use musig2::{AggNonce, KeyAggContext, SecNonce};
    use secp256k1::{PublicKey, SecretKey, SECP256K1};

    pub(crate) async fn musig_content_for(
        channel: &ChannelSigner<impl fiber_lsp_sdk::SignerStore>,
    ) -> Musig2SigningContent {
        let slot = NonceSlot {
            purpose: NoncePurpose::Commitment,
            commitment_number: 1,
        };
        let local_nonce = channel
            .get_musig2_nonce(slot)
            .await
            .expect("local nonce")
            .public_nonce;
        let remote_secret = SecretKey::from_byte_array(&[3u8; 32]).expect("remote secret");
        let remote_pubkey = PublicKey::from_secret_key(SECP256K1, &remote_secret);
        let remote_nonce = SecNonce::build([7u8; 32]).build().public_nonce();
        Musig2SigningContent {
            slot,
            commitment_counter: Some(CommitmentCounter::Local),
            key_agg_ctx: KeyAggContext::new([
                channel.public_material().base_public_keys.funding_pubkey,
                Pubkey::from(remote_pubkey),
            ])
            .expect("aggregate keys"),
            agg_nonce: AggNonce::sum([local_nonce, remote_nonce]),
            content: Musig2SignableContent::CommitmentTransaction(bound_commitment_tx()),
        }
    }

    pub(crate) fn bound_funding_outpoint() -> ckb_types::packed::OutPoint {
        use ckb_types::prelude::*;
        ckb_types::packed::OutPoint::new_builder()
            .tx_hash([7u8; 32].pack())
            .index(0u32)
            .build()
    }

    pub(crate) fn bound_commitment_tx() -> ckb_types::packed::Transaction {
        commitment_spending(bound_funding_outpoint())
    }

    pub(crate) fn commitment_spending(
        outpoint: ckb_types::packed::OutPoint,
    ) -> ckb_types::packed::Transaction {
        use ckb_types::{packed::CellInput, prelude::*};
        ckb_types::core::TransactionBuilder::default()
            .input(CellInput::new_builder().previous_output(outpoint).build())
            .build()
            .data()
    }

    pub(crate) fn bound_shutdown_script() -> ckb_types::packed::Script {
        use ckb_types::prelude::*;
        ckb_types::packed::Script::new_builder()
            .args([1u8, 2, 3].pack())
            .build()
    }

    pub(crate) fn remote_binding_pubkey() -> Pubkey {
        let remote_secret = SecretKey::from_byte_array(&[3u8; 32]).expect("remote secret");
        Pubkey::from(PublicKey::from_secret_key(SECP256K1, &remote_secret))
    }
}
