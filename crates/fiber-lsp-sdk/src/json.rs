//! Conversions between Fiber JSON-RPC types and signer-owned SDK types.
//!
//! This module is the reusable client boundary for
//! `get_channel_signing_status` / `submit_channel_signature`. It does not
//! implement transport or auto-approve signing.

use ckb_types::prelude::Unpack;
use fiber_json_types::{
    ChannelOpenSignerMaterial as JsonChannelOpenSignerMaterial,
    Musig2SignableContent as JsonMusig2SignableContent,
    Musig2SigningContent as JsonMusig2SigningContent,
    NextChannelSignerMaterial as JsonNextChannelSignerMaterial,
    SigningSettlement as JsonSigningSettlement,
};
use fiber_types::{ChannelAnnouncement, SettlementData, SettlementTlc, TLCId};
use molecule::prelude::Entity;
use musig2::{AggNonce, KeyAggContext};

use crate::{
    ChannelOpenSignerMaterial, CommitmentCounter, Musig2SignableContent, Musig2SigningContent,
    NextChannelSignerMaterial, NoncePurpose, NonceSlot, SettlementBinding,
};

/// Decode node-supplied MuSig2 signing plaintext.
pub fn musig2_from_rpc(content: JsonMusig2SigningContent) -> Result<Musig2SigningContent, String> {
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

/// Encode signer-owned MuSig2 plaintext for tests and RPC clients.
pub fn musig2_to_rpc(content: &Musig2SigningContent) -> JsonMusig2SigningContent {
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
        content: musig2_signable_to_rpc(&content.content),
    }
}

/// Encode channel-open public material for `open_channel_with_external_funding`.
pub fn open_material_to_rpc(material: &ChannelOpenSignerMaterial) -> JsonChannelOpenSignerMaterial {
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

/// Encode the next-round public material submitted with a signature.
pub fn next_material_to_rpc(material: &NextChannelSignerMaterial) -> JsonNextChannelSignerMaterial {
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

/// Decode the public settlement snapshot attached to a commitment request.
pub fn settlement_from_rpc(
    settlement: &JsonSigningSettlement,
) -> Result<
    (
        SettlementData,
        fiber_types::Pubkey,
        fiber_types::Pubkey,
        bool,
    ),
    String,
> {
    Ok((
        SettlementData {
            local_amount: settlement.local_amount,
            remote_amount: settlement.remote_amount,
            tlcs: settlement
                .tlcs
                .iter()
                .map(|tlc| {
                    Ok(SettlementTlc {
                        tlc_id: if tlc.inbound {
                            TLCId::Received(0)
                        } else {
                            TLCId::Offered(0)
                        },
                        hash_algorithm: tlc.hash_algorithm.into(),
                        payment_amount: tlc.payment_amount,
                        payment_hash: tlc.payment_hash.into(),
                        expiry: tlc.expiry,
                        local_key: None,
                        local_key_pubkey: Some(tlc.local_key_pubkey.try_into()?),
                        local_key_commitment_number: None,
                        remote_key: tlc.remote_key.try_into()?,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?,
        },
        settlement.local_settlement_pubkey.try_into()?,
        settlement.remote_settlement_pubkey.try_into()?,
        settlement.for_remote,
    ))
}

/// Decode node-supplied on-chain spend plaintext.
pub fn onchain_from_rpc(
    content: fiber_json_types::OnchainSigningContent,
) -> crate::OnchainSigningContent {
    crate::OnchainSigningContent {
        key_purpose: match content.key_purpose {
            fiber_json_types::OnchainKeyPurpose::Settlement => crate::OnchainKeyPurpose::Settlement,
            fiber_json_types::OnchainKeyPurpose::Tlc { commitment_number } => {
                crate::OnchainKeyPurpose::Tlc { commitment_number }
            }
        },
        transaction: content.transaction.into(),
    }
}

pub fn settlement_binding<'a>(
    data: &'a SettlementData,
    local_settlement_key: fiber_types::Pubkey,
    remote_settlement_key: fiber_types::Pubkey,
    for_remote: bool,
) -> SettlementBinding<'a> {
    SettlementBinding {
        data,
        local_settlement_key,
        remote_settlement_key,
        for_remote: Some(for_remote),
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

fn musig2_signable_to_rpc(content: &Musig2SignableContent) -> JsonMusig2SignableContent {
    match content {
        Musig2SignableContent::CommitmentTransaction(transaction) => {
            JsonMusig2SignableContent::CommitmentTransaction {
                transaction: transaction.clone().into(),
            }
        }
        Musig2SignableContent::CooperativeCloseTransaction(transaction) => {
            JsonMusig2SignableContent::CooperativeCloseTransaction {
                transaction: transaction.clone().into(),
            }
        }
        Musig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        } => JsonMusig2SignableContent::Revocation {
            output: output.clone(),
            output_data: output_data.clone(),
            commitment_lock_script_args: commitment_lock_script_args.clone(),
        },
        Musig2SignableContent::ChannelAnnouncement(_) => {
            JsonMusig2SignableContent::ChannelAnnouncement {
                unsigned_announcement: content.canonical_bytes(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use fiber_types::Pubkey;
    use musig2::SecNonce;
    use secp256k1::{PublicKey, SecretKey, SECP256K1};

    use super::*;
    use crate::{MemoryStore, RootKey, RootSigner};

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
        let content = Musig2SigningContent {
            slot,
            commitment_counter: Some(CommitmentCounter::Local),
            key_agg_ctx: KeyAggContext::new([
                channel.public_material().base_public_keys.funding_pubkey,
                Pubkey::from(remote_pubkey),
            ])
            .expect("aggregate keys"),
            agg_nonce: AggNonce::sum([local_nonce, remote_nonce]),
            content: Musig2SignableContent::CommitmentTransaction(Default::default()),
        };
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

    #[test]
    fn settlement_rpc_round_trips_fields_needed_to_bind_a_commitment() {
        let local = Pubkey::from(PublicKey::from_secret_key(
            SECP256K1,
            &SecretKey::from_byte_array(&[3u8; 32]).expect("local secret"),
        ));
        let remote = Pubkey::from(PublicKey::from_secret_key(
            SECP256K1,
            &SecretKey::from_byte_array(&[4u8; 32]).expect("remote secret"),
        ));
        let payment_hash = fiber_types::Hash256::from([9; 32]);
        let json = JsonSigningSettlement {
            local_amount: 15,
            remote_amount: 1,
            local_settlement_pubkey: local.into(),
            remote_settlement_pubkey: remote.into(),
            for_remote: true,
            tlcs: vec![fiber_json_types::SigningSettlementTlc {
                inbound: true,
                payment_hash: payment_hash.into(),
                payment_amount: 5,
                hash_algorithm: fiber_json_types::HashAlgorithm::CkbHash,
                expiry: 1_000,
                local_key_pubkey: local.into(),
                remote_key: remote.into(),
            }],
        };
        let (data, local_key, remote_key, for_remote) =
            settlement_from_rpc(&json).expect("convert settlement");
        assert_eq!(data.local_amount, 15);
        assert_eq!(local_key, local);
        assert_eq!(remote_key, remote);
        assert!(for_remote);
        assert_eq!(data.tlcs[0].payment_hash, payment_hash);
        assert!(matches!(data.tlcs[0].tlc_id, TLCId::Received(0)));
    }
}
