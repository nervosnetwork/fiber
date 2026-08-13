//! JSON-RPC conversion kept outside the portable SDK transport boundary.

use ckb_types::prelude::Unpack;
use fiber_json_types::{
    ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial,
    Musig2SignableContent as JsonMusig2SignableContent,
    Musig2SigningContent as JsonMusig2SigningContent,
    NextChannelSignerMaterial as JsonNextChannelSignerMaterial,
};
use fiber_lsp_sdk::{
    ChannelBinding, ChannelOpenSignerMaterial, CommitmentCounter, Musig2SignableContent,
    Musig2SigningContent, NextChannelSignerMaterial, NoncePurpose, NonceSlot,
};
use fiber_types::{ChannelAnnouncement, ChannelBasePublicKeys, Pubkey};
use molecule::prelude::Entity;
use musig2::{AggNonce, KeyAggContext};

pub(crate) fn channel_binding_from_rpc(
    binding: fiber_json_types::ChannelBinding,
) -> Result<ChannelBinding, String> {
    Ok(ChannelBinding {
        channel_id: binding.channel_id.into(),
        funding_outpoint: binding.funding_outpoint,
        remote_public_keys: ChannelBasePublicKeys {
            funding_pubkey: Pubkey::try_from(binding.remote_public_keys.funding_pubkey)?,
            tlc_base_key: Pubkey::try_from(binding.remote_public_keys.tlc_base_key)?,
        },
        funding_lock_script: binding.funding_lock_script.into(),
        local_shutdown_script: binding.local_shutdown_script.into(),
        commitment_delay_epoch: binding.commitment_delay_epoch,
    })
}

pub(crate) fn musig2_from_rpc(
    content: JsonMusig2SigningContent,
) -> Result<Musig2SigningContent, String> {
    Ok(Musig2SigningContent {
        slot: NonceSlot {
            purpose: nonce_purpose_from_rpc(content.slot.purpose),
            commitment_number: content.slot.commitment_number,
        },
        commitment_counter: content.commitment_counter.map(commitment_counter_from_rpc),
        key_agg_ctx: KeyAggContext::from_bytes(&content.key_agg_ctx)
            .map_err(|error| error.to_string())?,
        agg_nonce: AggNonce::from_bytes(&content.agg_nonce).map_err(|error| error.to_string())?,
        content: musig2_signable_from_rpc(content.content)?,
    })
}

pub(crate) fn open_material_to_rpc(
    material: &ChannelOpenSignerMaterial,
) -> JsonChannelOpenSignerMaterial {
    JsonChannelOpenSignerMaterial {
        base_public_keys: fiber_json_types::ChannelBasePublicKeys {
            funding_pubkey: material.base_public_keys.funding_pubkey.into(),
            tlc_base_key: material.base_public_keys.tlc_base_key.into(),
        },
        first_commitment_point: material.first_commitment_point.into(),
        second_commitment_point: material.second_commitment_point.into(),
        commitment_nonce: material.commitment_nonce.serialize().to_vec(),
        next_commitment_nonce: material.next_commitment_nonce.serialize().to_vec(),
        revocation_nonce: material.revocation_nonce.serialize().to_vec(),
        channel_announcement_nonce: material
            .channel_announcement_nonce
            .as_ref()
            .map(|nonce| nonce.serialize().to_vec()),
    }
}

pub(crate) fn next_material_to_rpc(
    material: &NextChannelSignerMaterial,
) -> JsonNextChannelSignerMaterial {
    JsonNextChannelSignerMaterial {
        next_commitment_point: material.next_commitment_point.map(Into::into),
        next_commitment_nonce: material
            .next_commitment_nonce
            .as_ref()
            .map(|nonce| nonce.serialize().to_vec()),
        next_revocation_nonce: material
            .next_revocation_nonce
            .as_ref()
            .map(|nonce| nonce.serialize().to_vec()),
    }
}

fn nonce_purpose_from_rpc(purpose: fiber_json_types::NoncePurpose) -> NoncePurpose {
    match purpose {
        fiber_json_types::NoncePurpose::Commitment => NoncePurpose::Commitment,
        fiber_json_types::NoncePurpose::Revocation => NoncePurpose::Revocation,
        fiber_json_types::NoncePurpose::ChannelAnnouncement => NoncePurpose::ChannelAnnouncement,
    }
}

fn commitment_counter_from_rpc(counter: fiber_json_types::CommitmentCounter) -> CommitmentCounter {
    match counter {
        fiber_json_types::CommitmentCounter::Local => CommitmentCounter::Local,
        fiber_json_types::CommitmentCounter::Remote => CommitmentCounter::Remote,
    }
}

fn musig2_signable_from_rpc(
    content: JsonMusig2SignableContent,
) -> Result<Musig2SignableContent, String> {
    Ok(match content {
        JsonMusig2SignableContent::CommitmentTransaction { transaction } => {
            Musig2SignableContent::CommitmentTransaction(transaction.into())
        }
        JsonMusig2SignableContent::CooperativeCloseTransaction { transaction } => {
            Musig2SignableContent::CooperativeCloseTransaction(transaction.into())
        }
        JsonMusig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        } => Musig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        },
        JsonMusig2SignableContent::ChannelAnnouncement {
            unsigned_announcement,
        } => {
            let molecule =
                fiber_types::gen::gossip::ChannelAnnouncement::from_slice(&unsigned_announcement)
                    .map_err(|error| error.to_string())?;
            Musig2SignableContent::ChannelAnnouncement(ChannelAnnouncement {
                node1_signature: None,
                node2_signature: None,
                ckb_signature: None,
                features: molecule.features().unpack(),
                capacity: molecule.capacity().unpack(),
                chain_hash: molecule.chain_hash().into(),
                channel_outpoint: molecule.channel_outpoint(),
                udt_type_script: molecule.udt_type_script().to_opt(),
                node1_id: molecule
                    .node1_id()
                    .try_into()
                    .map_err(|error: secp256k1::Error| error.to_string())?,
                node2_id: molecule
                    .node2_id()
                    .try_into()
                    .map_err(|error: secp256k1::Error| error.to_string())?,
                ckb_key: molecule
                    .ckb_key()
                    .try_into()
                    .map_err(|error: secp256k1::Error| error.to_string())?,
            })
        }
    })
}

#[cfg(test)]
pub(crate) fn musig2_to_rpc(content: &Musig2SigningContent) -> JsonMusig2SigningContent {
    JsonMusig2SigningContent {
        slot: fiber_json_types::NonceSlot {
            purpose: match content.slot.purpose {
                NoncePurpose::Commitment => fiber_json_types::NoncePurpose::Commitment,
                NoncePurpose::Revocation => fiber_json_types::NoncePurpose::Revocation,
                NoncePurpose::ChannelAnnouncement => {
                    fiber_json_types::NoncePurpose::ChannelAnnouncement
                }
            },
            commitment_number: content.slot.commitment_number,
        },
        commitment_counter: content.commitment_counter.map(|counter| match counter {
            CommitmentCounter::Local => fiber_json_types::CommitmentCounter::Local,
            CommitmentCounter::Remote => fiber_json_types::CommitmentCounter::Remote,
        }),
        key_agg_ctx: content.key_agg_ctx.serialize(),
        agg_nonce: content.agg_nonce.serialize().to_vec(),
        content: match &content.content {
            Musig2SignableContent::CommitmentTransaction(transaction) => {
                JsonMusig2SignableContent::CommitmentTransaction {
                    transaction: transaction.clone().into(),
                }
            }
            Musig2SignableContent::CooperativeCloseTransaction { .. } => unreachable!(),
            Musig2SignableContent::Revocation { .. } => unreachable!(),
            Musig2SignableContent::ChannelAnnouncement(_) => unreachable!(),
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use fiber_lsp_sdk::{ChannelSigner, MemoryStore, RootKey, RootSigner};
    use fiber_types::Pubkey;
    use musig2::SecNonce;
    use secp256k1::{PublicKey, SecretKey, SECP256K1};

    use super::*;

    #[tokio::test]
    async fn open_material_json_is_usable_by_an_rpc_client() {
        let root = RootSigner::create(
            RootKey::import([42; 32]).expect("root key"),
            MemoryStore::default(),
        )
        .await
        .expect("create root");
        let channel = root.create_channel().await.expect("create channel");
        let material = channel
            .channel_open_material(false)
            .await
            .expect("open material");
        let json = serde_json::to_value(open_material_to_rpc(&material)).expect("json");

        assert_eq!(
            json["base_public_keys"]["funding_pubkey"]
                .as_str()
                .expect("funding pubkey")
                .len(),
            66
        );
        assert!(json["commitment_nonce"]
            .as_str()
            .expect("nonce")
            .starts_with("0x"));
        assert!(json["channel_announcement_nonce"].is_null());
    }

    #[tokio::test]
    async fn musig2_rpc_content_round_trips() {
        let root = RootSigner::create(
            RootKey::import([42; 32]).expect("root key"),
            MemoryStore::default(),
        )
        .await
        .expect("create root");
        let channel = root.create_channel().await.expect("create channel");
        let content = musig_content_for(&channel).await;
        let restored = musig2_from_rpc(musig2_to_rpc(&content)).expect("restore");

        assert_eq!(restored.slot, content.slot);
        assert_eq!(
            restored.key_agg_ctx.serialize(),
            content.key_agg_ctx.serialize()
        );
        assert_eq!(
            restored.agg_nonce.serialize(),
            content.agg_nonce.serialize()
        );
    }

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
        use ckb_types::{packed::CellInput, prelude::*};
        ckb_types::core::TransactionBuilder::default()
            .input(
                CellInput::new_builder()
                    .previous_output(bound_funding_outpoint())
                    .build(),
            )
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
