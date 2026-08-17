//! Settlement and watchtower persistence types.
//!
//! Contains the data structures used by the watchtower service to monitor channels
//! and handle force-close scenarios.

use crate::channel::TLCId;
use crate::channel_signer::OnchainSigningContent;
use crate::invoice::HashAlgorithm;
use crate::serde_utils::{CompactSignatureAsBytes, EntityHex, SliceHex};
use crate::{Hash256, Privkey, Pubkey};
use ckb_types::packed::{Bytes, CellOutput, Script, Transaction};
use musig2::CompactSignature;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Offset of `blake160(settlement_data_to_witness(...))` inside CommitmentLock args.
pub const COMMITMENT_LOCK_SETTLEMENT_HASH_OFFSET: usize = 36;
/// Length of the settlement-witness hash committed in CommitmentLock args.
pub const COMMITMENT_LOCK_SETTLEMENT_HASH_LEN: usize = 20;

/// Data needed to revoke an outdated commitment transaction.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RevocationData {
    /// The commitment transaction version number that was revoked
    pub commitment_number: u64,
    /// The aggregated signature from both parties that authorizes the revocation
    #[serde_as(as = "CompactSignatureAsBytes")]
    pub aggregated_signature: CompactSignature,
    /// The output cell from the revoked commitment transaction
    #[serde_as(as = "EntityHex")]
    pub output: CellOutput,
    /// The associated data for the output cell (e.g., UDT amount for token transfers)
    #[serde_as(as = "EntityHex")]
    pub output_data: Bytes,
}

/// Data needed to authorize and execute a settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SettlementTlc {
    /// The ID of the TLC (either offered or received)
    pub tlc_id: TLCId,
    /// The hash algorithm used for the TLC
    pub hash_algorithm: HashAlgorithm,
    /// The amount of CKB/UDT involved in the TLC
    pub payment_amount: u128,
    /// The hash of the payment preimage
    pub payment_hash: Hash256,
    /// The expiry time for the TLC in milliseconds
    pub expiry: u64,
    /// The local party's private key used to sign the TLC.
    ///
    /// External signer channels leave this empty so channel private keys never
    /// enter the node or watchtower store.
    #[serde(default)]
    pub local_key: Option<Privkey>,
    /// Public key corresponding to the signer-owned TLC key.
    ///
    /// `None` preserves the legacy local-signer representation and derives the
    /// public key from `local_key`. External signer channels always set it.
    #[serde(default)]
    pub local_key_pubkey: Option<Pubkey>,
    /// Commitment point index used to derive an external signer TLC key.
    #[serde(default)]
    pub local_key_commitment_number: Option<u64>,
    /// The remote party's public key used to verify the TLC
    pub remote_key: Pubkey,
}

impl SettlementTlc {
    /// Return the actual local TLC public key for witness construction.
    pub fn local_pubkey(&self) -> Pubkey {
        self.local_key_pubkey
            .or_else(|| self.local_key.as_ref().map(Privkey::pubkey))
            .expect("settlement TLC must contain a local public or private key")
    }
}

/// The data of a channel that the watchtower is monitoring.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChannelData {
    /// The unique identifier of the channel
    pub channel_id: Hash256,
    /// The UDT type script if this is a UDT channel, None for CKB channels
    #[serde_as(as = "Option<EntityHex>")]
    pub funding_udt_type_script: Option<Script>,
    /// The local party's private key used to settle the commitment transaction.
    /// External signer channels leave this empty.
    #[serde(default)]
    pub local_settlement_key: Option<Privkey>,
    /// Public key used by the local settlement path.
    #[serde(default)]
    pub local_settlement_key_pubkey: Option<Pubkey>,
    /// The remote party's public key used to settle the commitment transaction
    pub remote_settlement_key: Pubkey,
    /// The local party's funding public key
    pub local_funding_pubkey: Pubkey,
    /// The remote party's funding public key
    pub remote_funding_pubkey: Pubkey,
    /// Settlement data for the remote commitment transaction
    pub remote_settlement_data: SettlementData,
    /// Pending settlement data for the remote commitment transaction
    /// (in case revocation hasn't been received yet)
    pub pending_remote_settlement_data: SettlementData,
    /// Settlement data for the local commitment transaction
    pub local_settlement_data: SettlementData,
    /// Data needed to revoke an outdated commitment transaction
    pub revocation_data: Option<RevocationData>,
}

/// Persistable watchtower signer sub-state for one watched channel.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum WatchtowerSignerState {
    /// The watchtower holds the settlement secret and signs locally.
    #[default]
    Internal,
    /// The client owns settlement keys; the tower pauses for an external signature.
    External(WatchtowerExternalSignerState),
}

/// External watchtower signer state machine.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WatchtowerExternalSignerState {
    /// Current pause point.
    pub state: WatchtowerExternalState,
    /// Last signature that successfully resumed this watch channel.
    #[serde(default)]
    pub last_applied: Option<LastAppliedWatchtowerSignature>,
}

/// Current location of an external watchtower signer.
#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum WatchtowerExternalState {
    /// No signature is currently required.
    #[default]
    Ready,
    /// Settlement or TLC spend is paused until this signature is submitted.
    AwaitingSignature {
        /// Identifier of the outstanding request.
        request_id: Hash256,
        /// Unsigned spend the client must sign.
        content: OnchainSigningContent,
    },
    /// A matching signature is stored and can be applied on the next settle attempt.
    Signed {
        /// Identifier of the signed request.
        request_id: Hash256,
        /// Unsigned spend that was signed.
        content: OnchainSigningContent,
        /// Recoverable ECDSA signature.
        #[serde_as(as = "SliceHex")]
        signature: [u8; 65],
    },
}

/// Receipt for one applied watchtower signature.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastAppliedWatchtowerSignature {
    /// Identifier of the applied request.
    pub request_id: Hash256,
    /// Signature that resumed the spend.
    #[serde_as(as = "SliceHex")]
    pub signature: [u8; 65],
}

impl ChannelData {
    /// Return the public key committed to the local settlement path.
    pub fn local_settlement_pubkey(&self) -> Pubkey {
        self.local_settlement_key_pubkey
            .or_else(|| self.local_settlement_key.as_ref().map(Privkey::pubkey))
            .expect("watch channel must contain a local settlement public or private key")
    }
}

/// CKB blake160: the first 20 bytes of blake2b-256.
pub fn blake160(message: &[u8]) -> [u8; 20] {
    let hash = ckb_hash::blake2b_256(message);
    let mut out = [0u8; 20];
    out.copy_from_slice(&hash[..20]);
    out
}

/// Absolute timestamp `since` value used in Fiber settlement TLC witnesses.
pub fn settlement_timestamp_since(expiry_ms: u64) -> u64 {
    0x4000_0000_0000_0000 | (expiry_ms / 1000)
}

/// Build the witness bytes for a single TLC in a settlement transaction.
pub fn settlement_tlc_to_witness(tlc: &SettlementTlc, for_remote: bool) -> Vec<u8> {
    let mut vec = Vec::new();
    let offered_flag = if tlc.tlc_id.is_offered() { 0u8 } else { 1u8 };
    vec.push(((tlc.hash_algorithm as u8) << 1) + offered_flag);
    vec.extend_from_slice(&tlc.payment_amount.to_le_bytes());
    vec.extend_from_slice(&tlc.payment_hash.as_ref()[0..20]);
    if for_remote {
        vec.extend_from_slice(&blake160(&tlc.remote_key.serialize()));
        vec.extend_from_slice(&blake160(&tlc.local_pubkey().serialize()));
    } else {
        vec.extend_from_slice(&blake160(&tlc.local_pubkey().serialize()));
        vec.extend_from_slice(&blake160(&tlc.remote_key.serialize()));
    }
    vec.extend_from_slice(&settlement_timestamp_since(tlc.expiry).to_le_bytes());
    vec
}

/// Build the witness bytes hashed into a commitment transaction's lock args.
pub fn settlement_data_to_witness(
    data: &SettlementData,
    for_remote: bool,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
) -> Vec<u8> {
    let mut vec = Vec::new();
    let len =
        u8::try_from(data.tlcs.len()).expect("TLC count exceeds witness encoding limit (max 255)");
    vec.push(len);
    for tlc in &data.tlcs {
        vec.extend_from_slice(&settlement_tlc_to_witness(tlc, for_remote));
    }
    if for_remote {
        vec.extend_from_slice(&blake160(&remote_settlement_key.serialize()));
        vec.extend_from_slice(data.remote_amount.to_le_bytes().as_ref());
        vec.extend_from_slice(&blake160(&local_settlement_key.serialize()));
        vec.extend_from_slice(data.local_amount.to_le_bytes().as_ref());
    } else {
        vec.extend_from_slice(&blake160(&local_settlement_key.serialize()));
        vec.extend_from_slice(data.local_amount.to_le_bytes().as_ref());
        vec.extend_from_slice(&blake160(&remote_settlement_key.serialize()));
        vec.extend_from_slice(data.remote_amount.to_le_bytes().as_ref());
    }
    vec
}

/// Hash of a settlement witness, as committed in CommitmentLock args `[36..56]`.
pub fn settlement_witness_hash(
    data: &SettlementData,
    for_remote: bool,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
) -> [u8; 20] {
    blake160(&settlement_data_to_witness(
        data,
        for_remote,
        local_settlement_key,
        remote_settlement_key,
    ))
}

/// Local pubkey hash for a settlement TLC.
pub fn settlement_tlc_local_pubkey_hash(tlc: &SettlementTlc) -> [u8; 20] {
    blake160(&tlc.local_pubkey().serialize())
}

/// Extract the committed settlement-witness hash from CommitmentLock args.
pub fn commitment_lock_settlement_hash(lock_args: &[u8]) -> Option<[u8; 20]> {
    let end = COMMITMENT_LOCK_SETTLEMENT_HASH_OFFSET + COMMITMENT_LOCK_SETTLEMENT_HASH_LEN;
    if lock_args.len() < end {
        return None;
    }
    let mut hash = [0u8; 20];
    hash.copy_from_slice(&lock_args[COMMITMENT_LOCK_SETTLEMENT_HASH_OFFSET..end]);
    Some(hash)
}

/// Whether `lock_args` commit this settlement snapshot.
///
/// `for_remote` selects Fiber's witness key/amount order. `None` accepts either
/// orientation so a client can verify without trusting the node-supplied flag.
pub fn settlement_matches_commitment_lock_args(
    lock_args: &[u8],
    settlement: &SettlementData,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
    for_remote: Option<bool>,
) -> bool {
    let Some(committed) = commitment_lock_settlement_hash(lock_args) else {
        return false;
    };
    let matches_orientation = |flag| {
        settlement_witness_hash(
            settlement,
            flag,
            local_settlement_key,
            remote_settlement_key,
        ) == committed
    };
    match for_remote {
        Some(flag) => matches_orientation(flag),
        None => matches_orientation(false) || matches_orientation(true),
    }
}

/// Whether a commitment transaction's first output commits this settlement.
pub fn settlement_matches_commitment_tx(
    transaction: &Transaction,
    settlement: &SettlementData,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
    for_remote: Option<bool>,
) -> bool {
    let Some(output) = transaction.raw().outputs().get(0) else {
        return false;
    };
    settlement_matches_commitment_lock_args(
        output.lock().args().raw_data().as_ref(),
        settlement,
        local_settlement_key,
        remote_settlement_key,
        for_remote,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::prelude::*;

    fn keys() -> (Pubkey, Pubkey) {
        (
            Privkey::from(&[1; 32]).pubkey(),
            Privkey::from(&[2; 32]).pubkey(),
        )
    }

    fn tlc(payment_hash: [u8; 32]) -> SettlementTlc {
        SettlementTlc {
            tlc_id: TLCId::Received(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 5,
            payment_hash: Hash256::from(payment_hash),
            expiry: 1_000,
            local_key: None,
            local_key_pubkey: Some(Privkey::from(&[3; 32]).pubkey()),
            local_key_commitment_number: None,
            remote_key: Privkey::from(&[4; 32]).pubkey(),
        }
    }

    fn settlement(local_amount: u128, payment_hash: [u8; 32]) -> SettlementData {
        SettlementData {
            local_amount,
            remote_amount: 1,
            tlcs: vec![tlc(payment_hash)],
        }
    }

    fn commitment_tx(data: &SettlementData, for_remote: bool) -> Transaction {
        let (local, remote) = keys();
        let hash = settlement_witness_hash(data, for_remote, local, remote);
        let mut args = vec![0u8; 36];
        args.extend_from_slice(&hash);
        args.push(0x00);
        ckb_types::core::TransactionBuilder::default()
            .output(
                CellOutput::new_builder()
                    .lock(Script::new_builder().args(args.pack()).build())
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(Bytes::default())
            .build()
            .data()
    }

    #[test]
    fn matching_settlement_binds_to_commitment_lock_args() {
        let (local, remote) = keys();
        let data = settlement(15, [9; 32]);
        let tx = commitment_tx(&data, true);
        assert!(settlement_matches_commitment_tx(
            &tx,
            &data,
            local,
            remote,
            Some(true)
        ));
        assert!(settlement_matches_commitment_tx(
            &tx, &data, local, remote, None
        ));
        assert!(!settlement_matches_commitment_tx(
            &tx,
            &data,
            local,
            remote,
            Some(false)
        ));
    }

    #[test]
    fn local_amount_mismatch_is_rejected() {
        let (local, remote) = keys();
        let committed = settlement(15, [9; 32]);
        let lied = settlement(99, [9; 32]);
        let tx = commitment_tx(&committed, true);
        assert!(!settlement_matches_commitment_tx(
            &tx, &lied, local, remote, None
        ));
    }

    #[test]
    fn tlc_set_mismatch_is_rejected() {
        let (local, remote) = keys();
        let committed = settlement(15, [9; 32]);
        let lied = settlement(15, [8; 32]);
        let tx = commitment_tx(&committed, true);
        assert!(!settlement_matches_commitment_tx(
            &tx, &lied, local, remote, None
        ));
    }

    #[test]
    fn empty_or_short_lock_args_do_not_match() {
        let (local, remote) = keys();
        let data = settlement(15, [9; 32]);
        let tx = ckb_types::core::TransactionBuilder::default()
            .output(
                CellOutput::new_builder()
                    .lock(Script::new_builder().args([0u8; 20].pack()).build())
                    .capacity(1000u64)
                    .build(),
            )
            .output_data(Bytes::default())
            .build()
            .data();
        assert!(!settlement_matches_commitment_tx(
            &tx, &data, local, remote, None
        ));
        assert!(!settlement_matches_commitment_tx(
            &Transaction::default(),
            &data,
            local,
            remote,
            None
        ));
    }
}
