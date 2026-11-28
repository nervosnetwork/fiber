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
        v2: content
            .v2
            .map(|v| {
                Ok::<_, String>(fiber_types::V2NonceBinding {
                    channel_id: v.channel_id.into(),
                    owner: v.owner.try_into()?,
                    public_nonce: musig2::PubNonce::from_bytes(&v.public_nonce)
                        .map_err(|e| e.to_string())?,
                })
            })
            .transpose()?,
        slot: NonceSlot {
            purpose: nonce_purpose_from_rpc(content.slot.purpose),
            commitment_number: content.slot.commitment_number,
        },
        commitment_counter: content.commitment_counter.map(commitment_counter_from_rpc),
        key_agg_ctx: parse_key_agg_ctx_checked(&content.key_agg_ctx)?,
        agg_nonce: AggNonce::from_bytes(&content.agg_nonce).map_err(|error| error.to_string())?,
        content: musig2_signable_from_rpc(content.content)?,
    })
}

/// Encode signer-owned MuSig2 plaintext for tests and RPC clients.
pub fn musig2_to_rpc(content: &Musig2SigningContent) -> JsonMusig2SigningContent {
    JsonMusig2SigningContent {
        v2: content
            .v2
            .as_ref()
            .map(|v| fiber_json_types::V2NonceBinding {
                channel_id: v.channel_id.into(),
                owner: v.owner.into(),
                public_nonce: v.public_nonce.serialize().to_vec(),
            }),
        slot: fiber_json_types::NonceSlot {
            purpose: match content.slot.purpose {
                NoncePurpose::Commitment => fiber_json_types::NoncePurpose::Commitment,
                NoncePurpose::Revocation => fiber_json_types::NoncePurpose::Revocation,
                NoncePurpose::Closing => fiber_json_types::NoncePurpose::Closing,
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
        v2_nonces: material
            .v2_nonces
            .iter()
            .map(|n| n.serialize().to_vec())
            .collect(),
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
        v2_nonces: material
            .v2_nonces
            .iter()
            .map(|n| n.serialize().to_vec())
            .collect(),
        commitment_points: material
            .commitment_points
            .iter()
            .map(|(n, p)| (*n, (*p).into()))
            .collect(),
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
                        // RPC direction is wallet-relative; witness direction depends on the lane.
                        tlc_id: if tlc.inbound == settlement.for_remote {
                            TLCId::Received(tlc.tlc_id)
                        } else {
                            TLCId::Offered(tlc.tlc_id)
                        },
                        hash_algorithm: tlc.hash_algorithm.into(),
                        payment_amount: tlc.payment_amount,
                        payment_hash: tlc.payment_hash.into(),
                        expiry: tlc.expiry,
                        local_key: None,
                        local_key_pubkey: Some(tlc.local_key_pubkey.try_into()?),
                        local_key_commitment_number: Some(tlc.local_key_commitment_number),
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

fn parse_key_agg_ctx_checked(bytes: &[u8]) -> Result<KeyAggContext, String> {
    // Pre-validate `musig2::KeyAggContext::from_bytes` wire layout so malformed
    // input cannot panic on `n_pubkey == 0` or `n_pubkey * 33` overflow on wasm32.
    if bytes.len() < 38 {
        return Err("invalid key_agg_ctx: too short".to_string());
    }
    let has_tweak = (bytes[0] & 0b10) != 0;
    let cursor = if has_tweak { 1 + 32 } else { 1 };
    let count_end = cursor + 4;
    if bytes.len() < count_end {
        return Err("invalid key_agg_ctx: truncated pubkey count".to_string());
    }
    let n_pubkey = u32::from_be_bytes([
        bytes[cursor],
        bytes[cursor + 1],
        bytes[cursor + 2],
        bytes[cursor + 3],
    ]);
    if n_pubkey == 0 {
        return Err("invalid key_agg_ctx: pubkey count must be non-zero".to_string());
    }
    let expected_remaining = u64::from(n_pubkey)
        .checked_mul(33)
        .ok_or_else(|| "invalid key_agg_ctx: pubkey length overflow".to_string())?;
    let actual_remaining = (bytes.len() - count_end) as u64;
    if expected_remaining != actual_remaining {
        return Err("invalid key_agg_ctx: pubkey byte length mismatch".to_string());
    }
    KeyAggContext::from_bytes(bytes).map_err(|error| error.to_string())
}

fn nonce_purpose_from_rpc(purpose: fiber_json_types::NoncePurpose) -> NoncePurpose {
    match purpose {
        fiber_json_types::NoncePurpose::Commitment => NoncePurpose::Commitment,
        fiber_json_types::NoncePurpose::Revocation => NoncePurpose::Revocation,
        fiber_json_types::NoncePurpose::Closing => NoncePurpose::Closing,
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

/// Decode participant evidence, rejecting malformed nonce or partial signature bytes.
pub fn session_evidence_from_rpc(
    value: fiber_json_types::SigningSessionEvidence,
) -> Result<crate::SigningSessionEvidence, String> {
    Ok(crate::SigningSessionEvidence {
        peer_public_nonce: musig2::PubNonce::from_bytes(&value.peer_public_nonce)
            .map_err(|e| e.to_string())?,
        peer_partial_signature: value
            .peer_partial_signature
            .map(|s| musig2::PartialSignature::from_slice(&s).map_err(|e| e.to_string()))
            .transpose()?,
    })
}

/// Encode public participant evidence for the JSON boundary.
pub fn session_evidence_to_rpc(
    value: &crate::SigningSessionEvidence,
) -> fiber_json_types::SigningSessionEvidence {
    fiber_json_types::SigningSessionEvidence {
        peer_public_nonce: value.peer_public_nonce.serialize().to_vec(),
        peer_partial_signature: value.peer_partial_signature.map(|s| s.serialize().to_vec()),
    }
}

#[cfg(test)]
mod tests;
