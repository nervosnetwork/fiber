//! Settlement and watchtower persistence types.
//!
//! Contains the data structures used by the watchtower service to monitor channels
//! and handle force-close scenarios.

use std::collections::BTreeMap;

use crate::channel::{CommitmentContractFeatures, TLCId};
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettlementData {
    /// The total amount of CKB/UDT being settled for the local party
    pub local_amount: u128,
    /// The total amount of CKB/UDT being settled for the remote party
    pub remote_amount: u128,
    /// The list of pending Time-Locked Contracts (TLCs) included in this settlement
    pub tlcs: Vec<SettlementTlc>,
}

/// Data needed to authorize and execute a Time-Locked Contract (TLC) settlement transaction.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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

/// Settlement key material for one side of a watched channel.
///
/// The watchtower persists the local settlement key as a secret/public pair
/// (`local_settlement_key` / `local_settlement_key_pubkey`), where either half may
/// be absent. Re-deriving "does this node hold the secret, or does an external
/// signer?" at every call site is what produced three spellings of the same
/// state, so producers and consumers of that pair state the intent once, here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettlementKey {
    /// This process holds the secret and signs locally.
    Local(Privkey),
    /// Only the public key is known; the client's signer produces signatures.
    External(Pubkey),
}

impl SettlementKey {
    /// Key material for a channel this node signs for.
    pub fn local(secret: Privkey) -> Self {
        Self::Local(secret)
    }

    /// Key material for a channel signed by the client's signer.
    pub fn external(pubkey: Pubkey) -> Self {
        Self::External(pubkey)
    }

    /// Public key committed to by this key material.
    pub fn pubkey(&self) -> Pubkey {
        match self {
            Self::Local(key) => key.pubkey(),
            Self::External(pubkey) => *pubkey,
        }
    }

    /// The secret, when this node signs locally.
    pub fn secret(&self) -> Option<&Privkey> {
        match self {
            Self::Local(key) => Some(key),
            Self::External(_) => None,
        }
    }

    /// Whether signatures must come from an external signer.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Interpret the persisted secret/public pair.
    ///
    /// Returns `Ok(None)` when neither half is present, which is the only state
    /// that carries no key material. A secret and a public key that disagree are
    /// rejected instead of silently picking one.
    pub fn from_parts(
        secret: Option<Privkey>,
        pubkey: Option<Pubkey>,
    ) -> Result<Option<Self>, String> {
        match (secret, pubkey) {
            (Some(secret), Some(pubkey)) if secret.pubkey() != pubkey => Err(
                "local settlement public key does not match the local settlement secret"
                    .to_string(),
            ),
            (Some(secret), _) => Ok(Some(Self::Local(secret))),
            (None, Some(pubkey)) => Ok(Some(Self::External(pubkey))),
            (None, None) => Ok(None),
        }
    }

    /// The persisted `(secret, public)` pair.
    pub fn to_parts(&self) -> (Option<Privkey>, Option<Pubkey>) {
        match self {
            Self::Local(key) => (Some(key.clone()), Some(key.pubkey())),
            Self::External(pubkey) => (None, Some(*pubkey)),
        }
    }
}

/// Key material of one watched channel, named so that local and remote cannot be
/// swapped by argument position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchChannelKeys {
    /// The local party's settlement key material (secret or public only).
    pub local_settlement: SettlementKey,
    /// The remote party's settlement public key.
    pub remote_settlement: Pubkey,
    /// The local party's funding public key.
    pub local_funding: Pubkey,
    /// The remote party's funding public key.
    pub remote_funding: Pubkey,
}

impl WatchChannelKeys {
    /// The same channel seen from the other party.
    ///
    /// Settlement and funding keys swap sides and the settlement secret stays
    /// with its owner, so the mirrored row can only verify signatures.
    pub fn inverted(&self) -> Self {
        Self {
            local_settlement: SettlementKey::External(self.remote_settlement),
            remote_settlement: self.local_settlement.pubkey(),
            local_funding: self.remote_funding,
            remote_funding: self.local_funding,
        }
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
    /// The commitment-lock features used by this channel.
    #[serde(default)]
    pub commitment_contract_features: CommitmentContractFeatures,
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
#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WatchtowerExternalSignerState {
    /// Outstanding requests awaiting external signature: request_id -> OnchainSigningContent.
    #[serde(default)]
    pub pending_requests: BTreeMap<Hash256, OnchainSigningContent>,
    /// Verified signatures received from external signer, awaiting application:
    /// request_id -> (OnchainSigningContent, [u8; 65]).
    #[serde_as(as = "BTreeMap<_, (_, SliceHex)>")]
    #[serde(default)]
    pub signed_signatures: BTreeMap<Hash256, (OnchainSigningContent, [u8; 65])>,
    /// Last signature that successfully resumed this watch channel (kept simple for persistence).
    #[serde(default)]
    pub last_applied: Option<LastAppliedWatchtowerSignature>,
}

impl WatchtowerExternalSignerState {
    pub fn is_empty(&self) -> bool {
        self.pending_requests.is_empty() && self.signed_signatures.is_empty()
    }

    pub fn first_pending(&self) -> Option<(Hash256, &OnchainSigningContent)> {
        self.pending_requests.iter().next().map(|(id, c)| (*id, c))
    }
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
    /// Local settlement key material as persisted, or `None` when the row holds
    /// no settlement key at all.
    pub fn settlement_key(&self) -> Option<SettlementKey> {
        let secret = self.local_settlement_key.clone();
        let pubkey = self.local_settlement_key_pubkey;
        SettlementKey::from_parts(secret, pubkey).ok().flatten()
    }

    /// Column-form of [`Self::settlement_key`] for callers that only need the
    /// public half.
    fn stored_settlement_pubkey(&self) -> Option<Pubkey> {
        self.local_settlement_key_pubkey
            .or_else(|| self.local_settlement_key.as_ref().map(Privkey::pubkey))
    }

    /// Return the public key committed to the local settlement path.
    pub fn local_settlement_pubkey(&self) -> Pubkey {
        self.stored_settlement_pubkey()
            .expect("watch channel must contain a local settlement public or private key")
    }

    /// Key material of this row, named as in [`WatchChannelKeys`].
    pub fn watch_channel_keys(&self) -> Option<WatchChannelKeys> {
        Some(WatchChannelKeys {
            local_settlement: self.settlement_key()?,
            remote_settlement: self.remote_settlement_key,
            local_funding: self.local_funding_pubkey,
            remote_funding: self.remote_funding_pubkey,
        })
    }

    /// Return the expected public key for an on-chain signing request.
    ///
    /// TLC requests must resolve to a known derivation index. Missing or conflicting
    /// key material cannot be replaced by the settlement base key.
    pub fn expected_onchain_pubkey(
        &self,
        purpose: &crate::channel_signer::OnchainKeyPurpose,
    ) -> Result<Pubkey, String> {
        match purpose {
            crate::channel_signer::OnchainKeyPurpose::Settlement => self
                .stored_settlement_pubkey()
                .ok_or_else(|| "settlement signing public key is missing".to_string()),
            crate::channel_signer::OnchainKeyPurpose::Tlc { commitment_number } => {
                let mut expected = None;
                for tlc in self
                    .local_settlement_data
                    .tlcs
                    .iter()
                    .chain(self.remote_settlement_data.tlcs.iter())
                    .chain(self.pending_remote_settlement_data.tlcs.iter())
                    .filter(|tlc| tlc.local_key_commitment_number == Some(*commitment_number))
                {
                    let pubkey = tlc
                        .local_key_pubkey
                        .or_else(|| tlc.local_key.as_ref().map(Privkey::pubkey))
                        .ok_or_else(|| "TLC signing public key is missing".to_string())?;
                    if expected.is_some_and(|key| key != pubkey) {
                        return Err(
                            "conflicting TLC signing public keys for derivation index".to_string()
                        );
                    }
                    expected = Some(pubkey);
                }
                expected.ok_or_else(|| {
                    format!("TLC signing key not found for derivation index {commitment_number}")
                })
            }
        }
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
pub fn settlement_tlc_to_witness(
    tlc: &SettlementTlc,
    for_remote: bool,
    commitment_contract_features: CommitmentContractFeatures,
) -> Vec<u8> {
    let mut vec = Vec::new();
    let offered_flag = if tlc.tlc_id.is_offered() { 0u8 } else { 1u8 };
    vec.push(((tlc.hash_algorithm as u8) << 1) + offered_flag);
    vec.extend_from_slice(&tlc.payment_amount.to_le_bytes());
    vec.extend_from_slice(
        &tlc.payment_hash.as_ref()[..commitment_contract_features.payment_hash_len()],
    );
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
    commitment_contract_features: CommitmentContractFeatures,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
) -> Vec<u8> {
    let mut vec = Vec::new();
    let len =
        u8::try_from(data.tlcs.len()).expect("TLC count exceeds witness encoding limit (max 255)");
    vec.push(len);
    for tlc in &data.tlcs {
        vec.extend_from_slice(&settlement_tlc_to_witness(
            tlc,
            for_remote,
            commitment_contract_features,
        ));
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
    commitment_contract_features: CommitmentContractFeatures,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
) -> [u8; 20] {
    blake160(&settlement_data_to_witness(
        data,
        for_remote,
        commitment_contract_features,
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
    let commitment_contract_features = match lock_args.len() {
        57 => CommitmentContractFeatures::LEGACY,
        58 => match CommitmentContractFeatures::from_bits(lock_args[57]) {
            Ok(features) => features,
            Err(_) => return false,
        },
        _ => return false,
    };
    let matches_orientation = |flag| {
        settlement_witness_hash(
            settlement,
            flag,
            commitment_contract_features,
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
        let hash = settlement_witness_hash(
            data,
            for_remote,
            CommitmentContractFeatures::LEGACY,
            local,
            remote,
        );
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
