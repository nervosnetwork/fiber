//! Settlement and watchtower persistence types.
//!
//! Contains the data structures used by the watchtower service to monitor channels
//! and handle force-close scenarios.

use std::collections::BTreeMap;

use crate::channel::{ChannelFeatures, TLCId};
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

    /// Interpret the persisted secret/public pair, rejecting a contradiction.
    ///
    /// Meant for request boundaries that receive both halves from a caller (the
    /// `create_watch_channel` RPC). Readers of already-stored rows use
    /// [`ChannelData::settlement_key`], which treats the secret as authoritative
    /// instead of failing.
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

/// Encode a TLC using the channel's exact on-chain witness layout.
pub fn settlement_tlc_witness(tlc: &SettlementTlc, for_remote: bool) -> Vec<u8> {
    let mut bytes = vec![((tlc.hash_algorithm as u8) << 1) + u8::from(tlc.tlc_id.is_received())];
    bytes.extend_from_slice(&tlc.payment_amount.to_le_bytes());
    bytes.extend_from_slice(&tlc.payment_hash.as_ref()[..ChannelFeatures::PAYMENT_HASH_LEN]);
    let local = tlc.local_pubkey();
    let keys = if for_remote {
        [tlc.remote_key, local]
    } else {
        [local, tlc.remote_key]
    };
    for key in keys {
        bytes.extend_from_slice(&ckb_hash::blake2b_256(key.serialize())[..20]);
    }
    // CKB absolute timestamp since, in seconds.
    bytes.extend_from_slice(&(0x4000000000000000u64 | (tlc.expiry / 1000)).to_le_bytes());
    bytes
}

/// Maximum number of TLC entries encodable in a commitment witness.
///
/// Indices `0..254` (`0x00..=0xFD`) address TLCs, while `0xFE` and `0xFF` are
/// reserved as final-settlement unlock sentinels.
pub const MAX_WITNESS_TLC_COUNT: usize = 0xfe;

/// Encode the settlement snapshot committed by the commitment-lock args.
pub fn settlement_data_witness(
    data: &SettlementData,
    for_remote: bool,
    local: Pubkey,
    remote: Pubkey,
) -> Result<Vec<u8>, String> {
    data.validate_signing_keys(false)?;
    if data.tlcs.len() > MAX_WITNESS_TLC_COUNT {
        return Err("TLC count exceeds witness encoding limit (max 254)".to_owned());
    }
    let len = data.tlcs.len() as u8;
    let mut bytes = vec![len];
    for tlc in &data.tlcs {
        bytes.extend_from_slice(&settlement_tlc_witness(tlc, for_remote));
    }
    let sides = if for_remote {
        [(remote, data.remote_amount), (local, data.local_amount)]
    } else {
        [(local, data.local_amount), (remote, data.remote_amount)]
    };
    for (key, amount) in sides {
        bytes.extend_from_slice(&ckb_hash::blake2b_256(key.serialize())[..20]);
        bytes.extend_from_slice(&amount.to_le_bytes());
    }
    Ok(bytes)
}

impl SettlementData {
    /// Validate witness bounds and TLC signing metadata before persisting a snapshot.
    /// External channels must contain public-only keys and derivation indices.
    pub fn validate_signing_keys(&self, external: bool) -> Result<(), String> {
        if self.tlcs.len() > MAX_WITNESS_TLC_COUNT {
            return Err("TLC count exceeds witness encoding limit (max 254)".to_owned());
        }
        for tlc in &self.tlcs {
            if external && tlc.local_key.is_some() {
                return Err(
                    "external watch channels cannot contain local TLC private keys".to_owned(),
                );
            }
            if let (Some(secret), Some(public)) = (&tlc.local_key, tlc.local_key_pubkey) {
                if secret.pubkey() != public {
                    return Err("local TLC public key does not match private key".to_owned());
                }
            }
            if tlc.local_key.is_none() && tlc.local_key_pubkey.is_none() {
                return Err("local TLC signing public key is missing".to_owned());
            }
            if external && tlc.local_key_commitment_number.is_none() {
                return Err("external TLC signing derivation index is missing".to_owned());
            }
        }
        Ok(())
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
    pub channel_features: ChannelFeatures,
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

/// Maximum serialized byte length allowed for an externally funded watchtower transaction.
pub const MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES: usize = 64 * 1024;
/// Maximum number of inputs, outputs, cell_deps, or witnesses allowed in an externally funded
/// watchtower transaction.
pub const MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS: usize = 64;

/// Outstanding external watchtower signing request awaiting a client-funded transaction and signature.
///
/// The watchtower issues an unfunded template transaction containing only the commitment input
/// (`inputs[0]`), settlement outputs (`[residual_commitment_output, payout_output]` or
/// `[payout_output]` when both parties are settled), and the `CommitmentLock` witness placeholder
/// (`witness[0]`). The client wallet funds fees (either from the payout output when permitted or
/// by attaching its own `cell_deps`, `inputs[1..]`, change output, and `witness[1..]`) and submits
/// both `signature` and `signed_transaction`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PendingWatchtowerRequest {
    /// Unfunded template on-chain plaintext.
    pub content: OnchainSigningContent,
    /// Creation timestamp (Unix ms), used to bound request TTL.
    #[serde(default)]
    pub created_at_ms: u64,
    /// Optional absolute timestamp `since` required on a funding input for a timeout TLC unlock
    /// (matching `CommitmentLock`'s `load_expiry_raw_since_value`).
    #[serde(default)]
    pub required_expiry_since: Option<u64>,
}

/// Verified external watchtower signature awaiting transaction assembly and broadcast.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SignedWatchtowerRequest {
    /// Client-funded transaction carrying client `cell_deps`, optional `inputs[1..]`,
    /// outputs, and `witness[1..]`, with `witness[0]` normalized to the template placeholder.
    pub content: OnchainSigningContent,
    /// Recoverable ECDSA signature for `inputs[0]`.
    #[serde_as(as = "SliceHex")]
    pub signature: [u8; 65],
    /// Acceptance timestamp (Unix ms), used to bound TTL if broadcast fails.
    #[serde(default)]
    pub created_at_ms: u64,
}

/// External watchtower signer state machine.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct WatchtowerExternalSignerState {
    /// Outstanding requests awaiting external signature: request_id -> PendingWatchtowerRequest.
    #[serde(default)]
    pub pending_requests: BTreeMap<Hash256, PendingWatchtowerRequest>,
    /// Verified signatures received from external signer, awaiting application:
    /// request_id -> SignedWatchtowerRequest.
    #[serde(default)]
    pub signed_signatures: BTreeMap<Hash256, SignedWatchtowerRequest>,
    /// Last signature that successfully resumed this watch channel (kept simple for persistence).
    #[serde(default)]
    pub last_applied: Option<LastAppliedWatchtowerSignature>,
    /// Consecutive expired or failed external requests for retry backoff.
    #[serde(default)]
    pub consecutive_expired_requests: u32,
    /// Earliest Unix ms timestamp before which this channel will not create new signing requests.
    #[serde(default)]
    pub retry_after_ms: u64,
}

impl WatchtowerExternalSignerState {
    pub fn is_empty(&self) -> bool {
        self.pending_requests.is_empty() && self.signed_signatures.is_empty()
    }

    /// Return the highest-priority pending request for RPC status and subscription replay:
    /// prefer self-fundable plain-CKB payouts (`>= EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY`,
    /// no expiry input required), then any non-timeout claim (`required_expiry_since.is_none()`),
    /// then larger CKB payout capacity, breaking ties deterministically by lowest `request_id`.
    pub fn first_pending(&self) -> Option<(Hash256, &PendingWatchtowerRequest)> {
        self.pending_requests
            .iter()
            .max_by(|(id_a, req_a), (id_b, req_b)| {
                pending_request_priority(req_a)
                    .cmp(&pending_request_priority(req_b))
                    .then_with(|| id_b.cmp(id_a))
            })
            .map(|(id, req)| (*id, req))
    }
}

/// Minimum plain-CKB payout capacity (61 CKB standard secp256k1 occupied floor + 10,000 shannons
/// fee buffer) for an external watchtower request to be treated as self-fundable without attaching
/// a separate wallet funding input.
pub const EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY: u64 = 6_100_010_000;

/// Compute the priority key `(self_fundable, no_expiry_required, payout_capacity)` for a pending
/// external watchtower request.
///
/// Higher tuples take precedence: self-fundable plain-CKB payouts
/// (`required_expiry_since.is_none()`, no type script, and
/// `payout_capacity >= EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY`) rank first, then non-timeout
/// claims, then larger CKB payout capacity.
pub fn pending_request_priority(req: &PendingWatchtowerRequest) -> (bool, bool, u64) {
    use ckb_types::prelude::Unpack;

    let no_expiry_required = req.required_expiry_since.is_none();
    let outputs = req.content.transaction.raw().outputs();
    let payout = outputs
        .len()
        .checked_sub(1)
        .and_then(|idx| outputs.get(idx));
    let payout_capacity = payout
        .as_ref()
        .map(|o| Unpack::<u64>::unpack(&o.capacity()))
        .unwrap_or(0);
    let is_plain_ckb = payout.is_some_and(|o| o.type_().is_none());
    let self_fundable = no_expiry_required
        && is_plain_ckb
        && payout_capacity >= EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY;
    (self_fundable, no_expiry_required, payout_capacity)
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
    /// Blake2b-256 digest of `tx.raw()` and client funding witnesses (`witness[1..]`)
    /// for exact idempotency checking.
    pub signed_transaction_hash: Hash256,
}

/// Parse a V2 single-unlock `CommitmentLock` settlement witness (`witness[0]`), returning
/// `(htlc_count, unlock_type, with_preimage, sig_start, sig_end)`.
fn parse_settlement_witness_unlock(witness: &[u8]) -> Result<(u8, u8, bool, usize, usize), String> {
    // Layout: 16B xudt placeholder + 1B unlock_count(0x01) + 1B htlc_count(N) +
    // N * 97B V2 HTLC scripts + 72B party balances + 1B unlock_type + 1B with_preimage +
    // 65B signature + optional 32B preimage.
    if witness.len() < 18 {
        return Err("commitment settlement witness is too short".to_string());
    }
    if witness[16] != 0x01 {
        return Err(format!(
            "commitment settlement witness must have unlock_count=1, got {}",
            witness[16]
        ));
    }
    let htlc_count_u8 = witness[17];
    let htlc_count = htlc_count_u8 as usize;
    let unlock_offset = 18usize
        .checked_add(
            htlc_count
                .checked_mul(97)
                .ok_or_else(|| "settlement witness htlc offset overflow".to_string())?,
        )
        .and_then(|v| v.checked_add(72))
        .ok_or_else(|| "settlement witness unlock offset overflow".to_string())?;
    let with_preimage_idx = unlock_offset
        .checked_add(1)
        .ok_or_else(|| "settlement witness flag offset overflow".to_string())?;
    if witness.len() <= with_preimage_idx {
        return Err("commitment settlement witness is truncated before unlock".to_string());
    }
    let unlock_type = witness[unlock_offset];
    let with_preimage = match witness[with_preimage_idx] {
        0 => false,
        1 => true,
        flag => {
            return Err(format!(
                "invalid with_preimage flag in commitment witness: {flag}"
            ));
        }
    };
    let sig_start = with_preimage_idx + 1;
    let sig_end = sig_start + 65;
    let expected_len = sig_end + if with_preimage { 32 } else { 0 };
    if witness.len() != expected_len {
        return Err(format!(
            "unexpected commitment witness length: expected {expected_len}, got {}",
            witness.len()
        ));
    }
    Ok((
        htlc_count_u8,
        unlock_type,
        with_preimage,
        sig_start,
        sig_end,
    ))
}

/// Return whether a V2 single-unlock `CommitmentLock` settlement witness (`witness[0]`)
/// carries a 32-byte preimage (`with_preimage == true`).
pub fn settlement_witness_with_preimage(witness: &[u8]) -> Result<bool, String> {
    parse_settlement_witness_unlock(witness).map(|(_, _, with_preimage, _, _)| with_preimage)
}

/// Locate the 65-byte signature slice `(start, end)` inside a V2 single-unlock
/// `CommitmentLock` settlement witness (`witness[0]`).
fn settlement_witness_signature_range(witness: &[u8]) -> Result<(usize, usize), String> {
    let (_, _, _, sig_start, sig_end) = parse_settlement_witness_unlock(witness)?;
    Ok((sig_start, sig_end))
}

fn is_absolute_timestamp_since(raw_since: u64) -> bool {
    (raw_since >> 56) == 0x40
}

/// Enforce byte-size and item-count bounds on an externally funded watchtower transaction
/// before acquiring signer locks or hashing its contents.
pub fn validate_external_funding_watchtower_tx_bounds(tx: &Transaction) -> Result<(), String> {
    use ckb_types::prelude::Entity;

    if tx.as_slice().len() > MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES {
        return Err(format!(
            "externally funded watchtower transaction exceeds maximum size ({MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES} bytes)"
        ));
    }
    let raw = tx.raw();
    let inputs_len = raw.inputs().len();
    if inputs_len > MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
        return Err(format!(
            "externally funded watchtower transaction exceeds maximum inputs ({MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS}), got {inputs_len}"
        ));
    }
    let outputs_len = raw.outputs().len();
    if outputs_len > MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
        return Err(format!(
            "externally funded watchtower transaction exceeds maximum outputs ({MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS}), got {outputs_len}"
        ));
    }
    if outputs_len != raw.outputs_data().len() {
        return Err(
            "externally funded watchtower transaction outputs and outputs_data length mismatch"
                .to_string(),
        );
    }
    if raw.cell_deps().len() > MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
        return Err(format!(
            "externally funded watchtower transaction exceeds maximum cell_deps ({MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS})"
        ));
    }
    let witnesses_len = tx.witnesses().len();
    if witnesses_len > MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
        return Err(format!(
            "externally funded watchtower transaction exceeds maximum witnesses ({MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS}), got {witnesses_len}"
        ));
    }
    Ok(())
}

/// Validate `witness[0]` of a retried externally funded watchtower transaction submission.
///
/// Requires `witness[0]` to be present, parse as a single-unlock V2 settlement witness, and carry
/// either the all-zero placeholder or `signature` in its 65-byte signature slot.
pub fn validate_external_funding_retry_witness_zero(
    tx: &Transaction,
    signature: &[u8; 65],
) -> Result<(), String> {
    let w0 = tx
        .witnesses()
        .get(0)
        .ok_or_else(|| "externally funded transaction is missing witness[0]".to_string())?;
    let w0_bytes = w0.raw_data();
    let (sig_start, sig_end) = settlement_witness_signature_range(&w0_bytes)?;
    let sig_slot = &w0_bytes[sig_start..sig_end];
    if sig_slot != [0u8; 65].as_slice() && sig_slot != signature.as_slice() {
        return Err(
            "externally funded watchtower transaction witness[0] signature slot must be zero placeholder or match submitted signature"
                .to_string(),
        );
    }
    Ok(())
}

/// Deterministic digest binding an externally funded watchtower transaction's raw body,
/// normalized `witness[0]` (with its 65-byte signature slot zeroed), and all client funding
/// witnesses (`witness[1..]`), independent of whether `witness[0]` carries the placeholder or
/// applied signature.
pub fn external_funding_watchtower_tx_hash(tx: &Transaction) -> Hash256 {
    use ckb_types::prelude::*;

    let mut blake2b = ckb_hash::new_blake2b();
    blake2b.update(tx.raw().as_slice());
    if let Some(w0) = tx.witnesses().get(0) {
        blake2b.update(&[1u8]);
        let mut w0_bytes = w0.raw_data().to_vec();
        if let Ok((sig_start, sig_end)) = settlement_witness_signature_range(&w0_bytes) {
            w0_bytes[sig_start..sig_end].fill(0);
        }
        blake2b.update(&(w0_bytes.len() as u64).to_le_bytes());
        blake2b.update(&w0_bytes);
    } else {
        blake2b.update(&[0u8]);
    }
    let funding_witnesses: Vec<_> = tx.witnesses().into_iter().skip(1).collect();
    blake2b.update(&(funding_witnesses.len() as u64).to_le_bytes());
    for witness in funding_witnesses {
        let raw = witness.raw_data();
        blake2b.update(&(raw.len() as u64).to_le_bytes());
        blake2b.update(&raw);
    }
    let mut out = [0u8; 32];
    blake2b.finalize(&mut out);
    Hash256::from(out)
}

/// Validate an externally funded watchtower transaction against its watchtower-issued template,
/// returning a normalized copy whose `witness[0]` is reset to the exact template placeholder
/// while preserving all client-provided `cell_deps`, `inputs[1..]`, payout/change outputs, and
/// `witness[1..]`.
///
/// Templates have either 2 outputs (`[residual_commitment_output, payout_output]`) when the
/// commitment cell continues on-chain, or 1 output (`[payout_output]`) when both parties are
/// settled. All template `cell_deps` must be retained, every submitted output must satisfy CKB
/// occupied capacity for its actual lock/type/data, every payout/change output must replace the
/// template placeholder lock (`Script::default()`), and any template UDT payout type/amount must
/// be preserved across the submitted payout outputs.
///
/// When `required_expiry_since` is `None`, the client may pay fees directly out of the payout
/// output (`inputs.len() == 1`) or attach extra wallet inputs (`inputs.len() >= 2`); absolute
/// timestamp `since` values (`0x40` prefix) are permitted on balance withdrawals
/// (`unlock_type >= 0xFE`, with or without already-expired TLCs in the witness) where the contract
/// never inspects input timestamps, and rejected on unlocks where a timestamp could affect the
/// contract path.
/// When `required_expiry_since` is `Some(required)`, the client must attach at least one funding
/// input (`inputs.len() >= 2`), and the **first** input carrying an absolute timestamp `since`
/// (`0x40` prefix) must satisfy `since >= required` (matching
/// `CommitmentLock::load_expiry_raw_since_value`).
///
/// When `submitted_signature` is `Some(sig)`, `witness[0]` in `submitted` may contain either
/// the all-zero placeholder or `sig` in its 65-byte signature slot. When `None` (pre-signing
/// validation in the SDK), `witness[0]` must equal the template witness verbatim.
pub fn validate_and_normalize_external_funding_watchtower_tx(
    template: &Transaction,
    submitted: &Transaction,
    required_expiry_since: Option<u64>,
    submitted_signature: Option<&[u8; 65]>,
) -> Result<Transaction, String> {
    use ckb_types::prelude::*;

    validate_external_funding_watchtower_tx_bounds(submitted)?;
    let template_raw = template.raw();
    let submitted_raw = submitted.raw();
    if submitted_raw.version() != template_raw.version() {
        return Err("externally funded watchtower transaction version mismatch".to_string());
    }
    if !submitted_raw.header_deps().is_empty() {
        return Err(
            "externally funded watchtower transaction must not include header_deps".to_string(),
        );
    }
    let mut submitted_deps =
        std::collections::HashSet::with_capacity(submitted_raw.cell_deps().len());
    for dep in submitted_raw.cell_deps() {
        if !submitted_deps.insert(dep) {
            return Err(
                "externally funded watchtower transaction contains duplicate cell_deps".to_string(),
            );
        }
    }
    for template_dep in template_raw.cell_deps() {
        if !submitted_deps.contains(&template_dep) {
            return Err(
                "externally funded watchtower transaction must retain all template cell_deps"
                    .to_string(),
            );
        }
    }
    let template_outputs_len = template_raw.outputs().len();
    if template_outputs_len != 1 && template_outputs_len != 2 {
        return Err(format!(
            "invalid watchtower template outputs count: expected 1 or 2, got {template_outputs_len}"
        ));
    }
    let min_inputs = if required_expiry_since.is_some() {
        2
    } else {
        1
    };
    let inputs_len = submitted_raw.inputs().len();
    if !(min_inputs..=MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS).contains(&inputs_len) {
        return Err(format!(
            "externally funded watchtower transaction must have between {min_inputs} and {MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS} inputs, got {inputs_len}"
        ));
    }
    let outputs_len = submitted_raw.outputs().len();
    if !(template_outputs_len..=MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS).contains(&outputs_len) {
        return Err(format!(
            "externally funded watchtower transaction must have between {template_outputs_len} and {MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS} outputs, got {outputs_len}"
        ));
    }
    let witnesses_len = submitted.witnesses().len();
    if !(1..=MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS).contains(&witnesses_len) {
        return Err(format!(
            "externally funded watchtower transaction must have between 1 and {MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS} witnesses, got {witnesses_len}"
        ));
    }

    let template_input0 = template_raw
        .inputs()
        .get(0)
        .ok_or_else(|| "template transaction is missing input[0]".to_string())?;
    let submitted_input0 = submitted_raw
        .inputs()
        .get(0)
        .ok_or_else(|| "externally funded transaction is missing input[0]".to_string())?;
    if submitted_input0 != template_input0 {
        return Err(
            "externally funded watchtower transaction input[0] must match template commitment input"
                .to_string(),
        );
    }
    let mut seen_outpoints = std::collections::HashSet::with_capacity(inputs_len);
    for input in submitted_raw.inputs() {
        if !seen_outpoints.insert(input.previous_output()) {
            return Err(
                "externally funded watchtower transaction contains duplicate input outpoints"
                    .to_string(),
            );
        }
    }

    // When the template carries a residual commitment output (`outputs.len() == 2`),
    // `outputs[0]` and `outputs_data[0]` must match the template verbatim. When `outputs.len() == 1`
    // (`two_parties_all_settled`), `outputs[0]` is the client's final payout and may be adjusted
    // for transaction fees.
    let payout_start = if template_outputs_len == 2 {
        let template_output0 = template_raw
            .outputs()
            .get(0)
            .ok_or_else(|| "template transaction is missing output[0]".to_string())?;
        let template_data0 = template_raw
            .outputs_data()
            .get(0)
            .ok_or_else(|| "template transaction is missing outputs_data[0]".to_string())?;
        if submitted_raw.outputs().get(0) != Some(template_output0)
            || submitted_raw.outputs_data().get(0) != Some(template_data0)
        {
            return Err(
                "externally funded watchtower transaction output[0] must match template commitment output"
                    .to_string(),
            );
        }
        1usize
    } else {
        0usize
    };

    let template_payout_idx = template_outputs_len - 1;
    let template_payout_output = template_raw
        .outputs()
        .get(template_payout_idx)
        .ok_or_else(|| "template transaction is missing payout output".to_string())?;
    let template_payout_udt = match template_payout_output.type_().to_opt() {
        Some(udt_type) => {
            let payout_data = template_raw
                .outputs_data()
                .get(template_payout_idx)
                .ok_or_else(|| "template transaction is missing payout outputs_data".to_string())?
                .raw_data();
            let amount_bytes: [u8; 16] = payout_data
                .get(..16)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| {
                    "template UDT payout outputs_data is shorter than 16 bytes".to_string()
                })?;
            Some((udt_type, u128::from_le_bytes(amount_bytes)))
        }
        None => None,
    };

    let default_lock = Script::default();
    let mut funded_udt_total = 0u128;
    for (i, output) in submitted_raw.outputs().into_iter().enumerate() {
        let data = submitted_raw
            .outputs_data()
            .get(i)
            .ok_or_else(|| {
                format!("externally funded watchtower transaction is missing outputs_data[{i}]")
            })?
            .raw_data();
        let capacity: u64 = output.capacity().unpack();
        let data_cap = ckb_types::core::Capacity::bytes(data.len())
            .map_err(|e| format!("output[{i}] data capacity overflow: {e}"))?;
        let occupied = output
            .occupied_capacity(data_cap)
            .map_err(|e| format!("output[{i}] occupied capacity overflow: {e}"))?
            .as_u64();
        if capacity < occupied {
            return Err(format!(
                "externally funded watchtower transaction output[{i}] capacity ({capacity}) is below occupied capacity ({occupied})"
            ));
        }
        if i >= payout_start {
            if output.lock() == default_lock {
                return Err(format!(
                    "externally funded watchtower transaction output[{i}] must replace placeholder lock script"
                ));
            }
            if let Some((ref udt_type, _)) = template_payout_udt {
                if output.type_().to_opt().as_ref() == Some(udt_type) {
                    let amount_bytes: [u8; 16] = data
                        .get(..16)
                        .and_then(|s| s.try_into().ok())
                        .ok_or_else(|| {
                            format!(
                                "externally funded watchtower transaction UDT output[{i}] data is shorter than 16 bytes"
                            )
                        })?;
                    funded_udt_total = funded_udt_total
                        .checked_add(u128::from_le_bytes(amount_bytes))
                        .ok_or_else(|| {
                            "externally funded watchtower transaction UDT payout overflow"
                                .to_string()
                        })?;
                }
            }
        }
    }
    if let Some((_, expected_udt_amount)) = template_payout_udt {
        if funded_udt_total < expected_udt_amount {
            return Err(format!(
                "externally funded watchtower transaction UDT payout ({funded_udt_total}) is below template payout ({expected_udt_amount})"
            ));
        }
    }

    let template_w0 = template
        .witnesses()
        .get(0)
        .ok_or_else(|| "template transaction is missing witness[0]".to_string())?;
    let submitted_w0 = submitted
        .witnesses()
        .get(0)
        .ok_or_else(|| "externally funded transaction is missing witness[0]".to_string())?;
    let template_w0_bytes = template_w0.raw_data();
    let submitted_w0_bytes = submitted_w0.raw_data();
    let (_htlc_count, unlock_type, _with_preimage, sig_start, sig_end) =
        parse_settlement_witness_unlock(&template_w0_bytes)?;
    if template_w0_bytes[sig_start..sig_end] != [0u8; 65] {
        return Err("template witness[0] signature slot must be all-zero placeholder".to_string());
    }
    if submitted_w0_bytes.len() != template_w0_bytes.len()
        || submitted_w0_bytes[..sig_start] != template_w0_bytes[..sig_start]
        || submitted_w0_bytes[sig_end..] != template_w0_bytes[sig_end..]
    {
        return Err(
            "externally funded watchtower transaction witness[0] does not match template unlock"
                .to_string(),
        );
    }
    let sig_slot = &submitted_w0_bytes[sig_start..sig_end];
    let is_zero_placeholder = sig_slot == [0u8; 65].as_slice();
    let matches_submitted_sig = submitted_signature.is_some_and(|sig| sig_slot == sig.as_slice());
    if !is_zero_placeholder && !matches_submitted_sig {
        return Err(
            "externally funded watchtower transaction witness[0] signature slot must be zero placeholder or match submitted signature"
                .to_string(),
        );
    }

    // Mirror CommitmentLock::load_expiry_raw_since_value: first absolute-timestamp input since.
    let first_abs_timestamp_since = submitted_raw
        .inputs()
        .into_iter()
        .map(|input| Unpack::<u64>::unpack(&input.since()))
        .find(|raw_since| is_absolute_timestamp_since(*raw_since));
    match required_expiry_since {
        Some(required) => {
            if !is_absolute_timestamp_since(required) {
                return Err(
                    "template required_expiry_since is not an absolute timestamp".to_string(),
                );
            }
            let Some(actual) = first_abs_timestamp_since else {
                return Err(
                    "externally funded timeout TLC transaction requires an input with absolute timestamp since >= expiry"
                        .to_string(),
                );
            };
            if actual < required {
                return Err(format!(
                    "externally funded timeout TLC transaction expiry since ({actual}) is earlier than required ({required})"
                ));
            }
        }
        None => {
            // On balance withdrawals (`unlock_type >= 0xFE`, even when already-expired TLCs
            // remain in the witness), CommitmentLock never inspects input absolute timestamps,
            // so wallets may spend mature timestamp-locked funding cells.
            let allow_timestamp_funding = unlock_type >= 0xFE;
            if !allow_timestamp_funding && first_abs_timestamp_since.is_some() {
                return Err(
                    "externally funded non-timeout watchtower transaction must not carry an absolute timestamp since"
                        .to_string(),
                );
            }
        }
    }

    let mut normalized_witnesses = Vec::with_capacity(witnesses_len);
    normalized_witnesses.push(template_w0);
    normalized_witnesses.extend(submitted.witnesses().into_iter().skip(1));
    Ok(submitted
        .clone()
        .into_view()
        .as_advanced_builder()
        .set_witnesses(normalized_witnesses)
        .build()
        .data())
}

impl ChannelData {
    /// Local settlement key material as persisted, or `None` when the row holds
    /// no settlement key at all.
    ///
    /// The secret is authoritative: signatures are produced with it, so a stored
    /// public key that disagrees with it is ignored rather than used to verify
    /// signatures the secret cannot produce. Writers go through
    /// [`SettlementKey::to_parts`], so a disagreeing row should not exist.
    pub fn settlement_key(&self) -> Option<SettlementKey> {
        match (&self.local_settlement_key, self.local_settlement_key_pubkey) {
            (Some(secret), _) => Some(SettlementKey::Local(secret.clone())),
            (None, Some(pubkey)) => Some(SettlementKey::External(pubkey)),
            (None, None) => None,
        }
    }

    /// Return the public key committed to the local settlement path.
    pub fn local_settlement_pubkey(&self) -> Pubkey {
        self.settlement_key()
            .map(|key| key.pubkey())
            .expect("watch channel must contain a local settlement public or private key")
    }

    /// Return the expected public key for an on-chain signing request.
    ///
    /// TLC requests must resolve to a known derivation index. Missing or conflicting
    /// key material cannot be replaced by the settlement base key.
    pub fn expected_onchain_pubkey(
        &self,
        purpose: &crate::channel_signer::OnchainKeyPurpose,
    ) -> Result<Pubkey, String> {
        self.expected_onchain_pubkey_with_settlements(purpose, &[])
    }

    /// Resolve a key including still-valid superseded remote snapshots.
    pub fn expected_onchain_pubkey_with_settlements(
        &self,
        purpose: &crate::channel_signer::OnchainKeyPurpose,
        additional: &[SettlementData],
    ) -> Result<Pubkey, String> {
        match purpose {
            crate::channel_signer::OnchainKeyPurpose::Settlement => self
                .settlement_key()
                .map(|key| key.pubkey())
                .ok_or_else(|| "settlement signing public key is missing".to_string()),
            crate::channel_signer::OnchainKeyPurpose::Tlc { commitment_number } => {
                let mut expected = None;
                for tlc in self
                    .local_settlement_data
                    .tlcs
                    .iter()
                    .chain(self.remote_settlement_data.tlcs.iter())
                    .chain(self.pending_remote_settlement_data.tlcs.iter())
                    .chain(additional.iter().flat_map(|data| data.tlcs.iter()))
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
pub fn settlement_tlc_to_witness(tlc: &SettlementTlc, for_remote: bool) -> Vec<u8> {
    settlement_tlc_witness(tlc, for_remote)
}

/// Build the witness bytes hashed into a commitment transaction's lock args.
pub fn settlement_data_to_witness(
    data: &SettlementData,
    for_remote: bool,
    local_settlement_key: Pubkey,
    remote_settlement_key: Pubkey,
) -> Vec<u8> {
    settlement_data_witness(
        data,
        for_remote,
        local_settlement_key,
        remote_settlement_key,
    )
    .expect("TLC count exceeds witness encoding limit (max 254)")
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
    if lock_args.len() != 58 || ChannelFeatures::from_bits(lock_args[57]).is_err() {
        return false;
    }
    let matches_orientation = |flag| {
        settlement_data_witness(
            settlement,
            flag,
            local_settlement_key,
            remote_settlement_key,
        )
        .is_ok_and(|witness| blake160(&witness) == committed)
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
        args.push(ChannelFeatures::V2.bits());
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
    fn only_v2_lock_layout_is_accepted() {
        let (local, remote) = keys();
        let data = settlement(15, [9; 32]);
        let tx = commitment_tx(&data, true);
        let output = tx.raw().outputs().get(0).unwrap();
        let args = output.lock().args().raw_data();
        assert_eq!(args.len(), ChannelFeatures::LOCK_ARGS_LEN);
        assert!(!settlement_matches_commitment_lock_args(
            &args[..57],
            &data,
            local,
            remote,
            None
        ));
        let mut unsupported = args.to_vec();
        unsupported[57] = 0;
        assert!(!settlement_matches_commitment_lock_args(
            &unsupported,
            &data,
            local,
            remote,
            None
        ));
        unsupported[57] = 2;
        assert!(!settlement_matches_commitment_lock_args(
            &unsupported,
            &data,
            local,
            remote,
            None
        ));
    }

    #[test]
    fn full_payment_hash_is_committed() {
        let (local, remote) = keys();
        let committed = settlement(15, [9; 32]);
        let mut changed = [9; 32];
        changed[31] ^= 1;
        let tx = commitment_tx(&committed, true);
        assert!(!settlement_matches_commitment_tx(
            &tx,
            &settlement(15, changed),
            local,
            remote,
            None
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

#[cfg(test)]
mod key_material_tests {
    use super::*;

    fn key(byte: u8) -> Privkey {
        Privkey::from(&[byte; 32])
    }

    fn settlement_data() -> SettlementData {
        SettlementData {
            local_amount: 0,
            remote_amount: 0,
            tlcs: Vec::new(),
        }
    }

    fn channel_data(local_key: Option<Privkey>, local_pubkey: Option<Pubkey>) -> ChannelData {
        ChannelData {
            channel_id: Hash256::from([7; 32]),
            funding_udt_type_script: None,
            local_settlement_key: local_key,
            local_settlement_key_pubkey: local_pubkey,
            remote_settlement_key: key(2).pubkey(),
            local_funding_pubkey: key(3).pubkey(),
            remote_funding_pubkey: key(4).pubkey(),
            remote_settlement_data: settlement_data(),
            pending_remote_settlement_data: settlement_data(),
            local_settlement_data: settlement_data(),
            revocation_data: None,
            channel_features: ChannelFeatures::V2,
        }
    }

    #[test]
    fn from_parts_rejects_a_contradicting_pair() {
        let error = SettlementKey::from_parts(Some(key(1)), Some(key(9).pubkey()))
            .expect_err("a public key that does not match the secret must be rejected");
        assert!(error.contains("does not match"), "{error}");
    }

    #[test]
    fn from_parts_round_trips_through_the_persisted_pair() {
        let local = SettlementKey::from_parts(Some(key(1)), Some(key(1).pubkey()))
            .unwrap()
            .expect("local key material");
        assert_eq!(local.secret(), Some(&key(1)));
        assert!(!local.is_external());
        assert_eq!(
            local.to_parts(),
            (Some(key(1)), Some(key(1).pubkey())),
            "writers persist a pair the readers can trust"
        );

        let external = SettlementKey::from_parts(None, Some(key(2).pubkey()))
            .unwrap()
            .expect("public-only key material");
        assert_eq!(external.secret(), None);
        assert!(external.is_external());
        assert_eq!(external.to_parts(), (None, Some(key(2).pubkey())));

        assert_eq!(SettlementKey::from_parts(None, None), Ok(None));
    }

    #[test]
    fn stored_rows_trust_the_secret_over_a_contradicting_public_key() {
        let data = channel_data(Some(key(1)), Some(key(9).pubkey()));
        assert_eq!(data.settlement_key(), Some(SettlementKey::Local(key(1))));
        assert_eq!(
            data.local_settlement_pubkey(),
            key(1).pubkey(),
            "signatures come from the secret, so a contradicting stored key is ignored"
        );
        assert_eq!(
            data.expected_onchain_pubkey(&crate::channel_signer::OnchainKeyPurpose::Settlement),
            Ok(key(1).pubkey())
        );

        let external = channel_data(None, Some(key(2).pubkey()));
        assert_eq!(
            external.settlement_key(),
            Some(SettlementKey::External(key(2).pubkey()))
        );
        assert_eq!(
            channel_data(None, None).settlement_key(),
            None,
            "a row with no key material has no settlement key"
        );
    }

    #[test]
    fn external_settlement_rejects_secrets_and_missing_derivation_metadata() {
        let mut data = settlement_data();
        data.tlcs.push(SettlementTlc {
            tlc_id: TLCId::Offered(1),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1,
            payment_hash: Hash256::from([8; 32]),
            expiry: 1000,
            local_key: None,
            local_key_pubkey: Some(key(1).pubkey()),
            local_key_commitment_number: Some(7),
            remote_key: key(2).pubkey(),
        });
        assert!(data.validate_signing_keys(true).is_ok());
        data.tlcs[0].local_key = Some(key(1));
        assert!(data.validate_signing_keys(true).is_err());
        data.tlcs[0].local_key = None;
        data.tlcs[0].local_key_commitment_number = None;
        assert!(data.validate_signing_keys(true).is_err());
    }

    #[test]
    fn settlement_snapshot_rejects_unencodable_tlc_count() {
        let mut data = settlement_data();
        let entry = SettlementTlc {
            tlc_id: TLCId::Offered(1),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1,
            payment_hash: Hash256::from([8; 32]),
            expiry: 1000,
            local_key: None,
            local_key_pubkey: Some(key(1).pubkey()),
            local_key_commitment_number: Some(7),
            remote_key: key(2).pubkey(),
        };
        data.tlcs = vec![entry; MAX_WITNESS_TLC_COUNT];
        let (local, remote) = (key(1).pubkey(), key(2).pubkey());
        assert!(data.validate_signing_keys(true).is_ok());
        assert!(settlement_data_witness(&data, false, local, remote).is_ok());
        data.tlcs.push(data.tlcs[0].clone());
        assert!(data
            .validate_signing_keys(true)
            .unwrap_err()
            .contains("max 254"));
        assert!(settlement_data_witness(&data, false, local, remote).is_err());
        // A malformed persisted snapshot must be a mismatch, not a panic.
        let mut args = [0; 58];
        args[57] = ChannelFeatures::V2.bits();
        assert!(!settlement_matches_commitment_lock_args(
            &args, &data, local, remote, None,
        ));
    }

    #[test]
    fn superseded_tlc_keys_remain_verifiable_without_settlement_key_fallback() {
        let data = channel_data(None, Some(key(1).pubkey()));
        let purpose = crate::channel_signer::OnchainKeyPurpose::Tlc {
            commitment_number: 7,
        };
        assert!(data.expected_onchain_pubkey(&purpose).is_err());
        let mut superseded = settlement_data();
        superseded.tlcs.push(SettlementTlc {
            tlc_id: TLCId::Offered(1),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1,
            payment_hash: Hash256::from([8; 32]),
            expiry: 1000,
            local_key: None,
            local_key_pubkey: Some(key(5).pubkey()),
            local_key_commitment_number: Some(7),
            remote_key: key(2).pubkey(),
        });
        assert_eq!(
            data.expected_onchain_pubkey_with_settlements(&purpose, &[superseded.clone()]),
            Ok(key(5).pubkey())
        );
        let mut conflicting = superseded.clone();
        conflicting.tlcs[0].local_key_pubkey = Some(key(6).pubkey());
        assert!(data
            .expected_onchain_pubkey_with_settlements(&purpose, &[superseded, conflicting])
            .is_err());
    }

    #[test]
    fn inverted_keys_swap_sides_and_drop_the_secret() {
        let keys = WatchChannelKeys {
            local_settlement: SettlementKey::Local(key(1)),
            remote_settlement: key(2).pubkey(),
            local_funding: key(3).pubkey(),
            remote_funding: key(4).pubkey(),
        };
        let mirrored = keys.inverted();
        assert_eq!(
            mirrored.local_settlement,
            SettlementKey::External(key(2).pubkey()),
            "the mirrored row can only verify the original owner's signatures"
        );
        assert_eq!(mirrored.remote_settlement, key(1).pubkey());
        assert_eq!(mirrored.local_funding, key(4).pubkey());
        assert_eq!(mirrored.remote_funding, key(3).pubkey());
    }

    fn build_template_witness(htlc_count: u8, with_preimage: bool, sig: [u8; 65]) -> Vec<u8> {
        let mut w = vec![
            16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0,    // xudt placeholder
            0x01, // unlock_count = 1
            htlc_count,
        ];
        w.extend(std::iter::repeat_n(0x11u8, htlc_count as usize * 97));
        w.extend(std::iter::repeat_n(0x22u8, 72)); // party balances
        w.push(if htlc_count > 0 { 0x00 } else { 0xfe }); // unlock_type
        w.push(u8::from(with_preimage));
        w.extend_from_slice(&sig);
        if with_preimage {
            w.extend_from_slice(&[0xaa; 32]);
        }
        w
    }

    fn test_wallet_lock() -> Script {
        use ckb_types::packed::Byte32;
        use ckb_types::prelude::*;
        Script::new_builder()
            .code_hash(Byte32::from([0x33; 32]))
            .args([0x11u8; 20].pack())
            .build()
    }

    fn test_udt_type() -> Script {
        use ckb_types::packed::Byte32;
        use ckb_types::prelude::*;
        Script::new_builder()
            .code_hash(Byte32::from([0x44; 32]))
            .args([0x22u8; 32])
            .build()
    }

    fn build_template_and_funded(
        htlc_count: u8,
        with_preimage: bool,
        funding_since: u64,
    ) -> (Transaction, Transaction) {
        use ckb_types::core::{DepType, TransactionBuilder};
        use ckb_types::packed::{Byte32, CellDep, CellInput, OutPoint};
        use ckb_types::prelude::*;

        let commitment_outpoint = OutPoint::new(Byte32::from([1u8; 32]), 0);
        let wallet_outpoint = OutPoint::new(Byte32::from([2u8; 32]), 1);
        let wallet_lock = test_wallet_lock();
        let udt_type = test_udt_type();
        let residual_lock = Script::new_builder()
            .code_hash(Byte32::from([0x55; 32]))
            .args([0xaau8; 56].pack())
            .build();
        let residual_out = CellOutput::new_builder()
            .lock(residual_lock)
            .capacity(28_400_000_000u64)
            .type_(Some(udt_type.clone()).pack())
            .build();
        let residual_data = 100u128.to_le_bytes().to_vec().pack();
        let template_udt_out = CellOutput::new_builder()
            .capacity(14_200_000_000u64)
            .type_(Some(udt_type.clone()).pack())
            .build();
        let funded_udt_out = CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(14_200_000_000u64)
            .type_(Some(udt_type).pack())
            .build();
        let udt_data = 50u128.to_le_bytes().to_vec().pack();
        let change_out = CellOutput::new_builder()
            .lock(wallet_lock)
            .capacity(6_100_000_000u64)
            .build();
        let w0 = build_template_witness(htlc_count, with_preimage, [0u8; 65]);

        let template = TransactionBuilder::default()
            .cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                    .dep_type(DepType::DepGroup)
                    .build(),
            )
            .input(CellInput::new(commitment_outpoint.clone(), 0))
            .output(residual_out.clone())
            .output_data(residual_data.clone())
            .output(template_udt_out)
            .output_data(udt_data.clone())
            .witness(w0.clone().pack())
            .build()
            .data();

        let funded = TransactionBuilder::default()
            .cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                    .dep_type(DepType::DepGroup)
                    .build(),
            )
            .cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([0x20; 32]), 0))
                    .dep_type(DepType::Code)
                    .build(),
            )
            .input(CellInput::new(commitment_outpoint, 0))
            .input(CellInput::new(wallet_outpoint, funding_since))
            .output(residual_out)
            .output_data(residual_data)
            .output(funded_udt_out)
            .output_data(udt_data)
            .output(change_out)
            .output_data(Bytes::default())
            .witness(w0.pack())
            .witness([0xdeu8, 0xad, 0xbe, 0xef].pack())
            .witness([0xcau8, 0xfe].pack())
            .build()
            .data();

        (template, funded)
    }

    #[test]
    fn external_funding_watchtower_tx_validation_and_normalization() {
        use ckb_types::core::DepType;
        use ckb_types::packed::{Byte32, CellDep, CellInput, OutPoint};
        use ckb_types::prelude::*;

        let expiry_since = settlement_timestamp_since(100_000);
        let (template, funded) = build_template_and_funded(1, false, expiry_since);
        let sig = [0x55u8; 65];

        // 1. Valid timeout TLC with zero placeholder in witness[0] normalizes cleanly and preserves witness[1..].
        let norm_zero = validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &funded,
            Some(expiry_since),
            Some(&sig),
        )
        .expect("valid zero-placeholder funded tx");
        assert_eq!(norm_zero.witnesses().len(), 3);
        assert_eq!(
            norm_zero.witnesses().get(1).unwrap().raw_data().as_ref(),
            &[0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            norm_zero.witnesses().get(2).unwrap().raw_data().as_ref(),
            &[0xca, 0xfe]
        );

        // 2. Filled witness[0] matching `submitted_signature` normalizes to identical tx & digest.
        let filled_w0 = build_template_witness(1, false, sig);
        let filled = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                filled_w0.pack(),
                [0xdeu8, 0xad, 0xbe, 0xef].pack(),
                [0xcau8, 0xfe].pack(),
            ])
            .build()
            .data();
        let norm_filled = validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &filled,
            Some(expiry_since),
            Some(&sig),
        )
        .expect("valid filled-signature funded tx");
        assert_eq!(norm_filled.as_slice(), norm_zero.as_slice());
        assert_eq!(
            external_funding_watchtower_tx_hash(&filled),
            external_funding_watchtower_tx_hash(&funded)
        );

        // Pre-signing mode (`submitted_signature: None`) rejects non-zero signature slot in witness[0].
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &filled,
            Some(expiry_since),
            None,
        )
        .is_err());

        // Wrong signature in witness[0] rejected.
        let other_sig = [0x66u8; 65];
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &filled,
            Some(expiry_since),
            Some(&other_sig),
        )
        .is_err());

        // 3. Changing client funding witness changes external_funding_watchtower_tx_hash.
        let diff_witness = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                funded.witnesses().get(0).unwrap(),
                [0xdeu8, 0xad, 0xbe, 0xee].pack(),
                [0xcau8, 0xfe].pack(),
            ])
            .build()
            .data();
        assert_ne!(
            external_funding_watchtower_tx_hash(&funded),
            external_funding_watchtower_tx_hash(&diff_witness)
        );

        // 4. Missing or too-early absolute timestamp `since` (or 1-input tx) rejected for timeout TLC.
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &template,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());
        let early = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                funded.raw().inputs().get(0).unwrap(),
                CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since - 1),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &early,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());
        let no_since = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                funded.raw().inputs().get(0).unwrap(),
                CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), 0),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &no_since,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        // 4b. First-absolute-timestamp-input rule: [expiry - 1, expiry] fails while
        //     [expiry, expiry - 1] succeeds (matching CommitmentLock::load_expiry_raw_since_value).
        let second_wallet_outpoint = OutPoint::new(Byte32::from([3u8; 32]), 0);
        let early_then_valid = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                funded.raw().inputs().get(0).unwrap(),
                CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since - 1),
                CellInput::new(second_wallet_outpoint.clone(), expiry_since),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &early_then_valid,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());
        let valid_then_early = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                funded.raw().inputs().get(0).unwrap(),
                CellInput::new(OutPoint::new(Byte32::from([2u8; 32]), 1), expiry_since),
                CellInput::new(second_wallet_outpoint, expiry_since - 1),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &valid_then_early,
            Some(expiry_since),
            Some(&sig),
        )
        .is_ok());

        // 5. Non-timeout preimage TLC allows 1-input fee-from-payout AND 2-input funding,
        //    and rejects unexpected absolute timestamp `since`.
        let (preimage_tmpl, preimage_funded_ok) = build_template_and_funded(1, true, 0);
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &preimage_tmpl,
            &preimage_funded_ok,
            None,
            Some(&sig),
        )
        .is_ok());
        // 1-input fee deducted from payout output[1] (residual output[0] preserved, placeholder lock replaced).
        let wallet_lock = test_wallet_lock();
        let udt_type = test_udt_type();
        let fee_from_payout = preimage_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                preimage_tmpl.raw().outputs().get(0).unwrap(),
                CellOutput::new_builder()
                    .lock(wallet_lock.clone())
                    .type_(Some(udt_type.clone()).pack())
                    .capacity(14_299_999_000u64)
                    .build(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &preimage_tmpl,
            &fee_from_payout,
            None,
            Some(&sig),
        )
        .is_ok());
        // 1-output `two_parties_all_settled` template allows deducting fee directly from outputs[0]
        // once placeholder lock is replaced, and rejects leaving Script::default() in place.
        let final_tmpl = ckb_types::core::TransactionBuilder::default()
            .cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                    .dep_type(DepType::DepGroup)
                    .build(),
            )
            .input(preimage_tmpl.raw().inputs().get(0).unwrap())
            .output(
                CellOutput::new_builder()
                    .capacity(20_000_000_000u64)
                    .build(),
            )
            .output_data(Bytes::default())
            .witness(build_template_witness(0, false, [0u8; 65]).pack())
            .build()
            .data();
        let final_placeholder_lock = final_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .capacity(19_999_999_000u64)
                .build()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_tmpl,
            &final_placeholder_lock,
            None,
            Some(&sig),
        )
        .is_err());
        let final_fee_from_payout = final_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .capacity(19_999_999_000u64)
                .build()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_tmpl,
            &final_fee_from_payout,
            None,
            Some(&sig),
        )
        .is_ok());
        // Balance withdrawal with no pending TLCs (htlc_count == 0, unlock_type == 0xFE) allows
        // a mature absolute-timestamp wallet funding input when required_expiry_since is None.
        let (balance_tmpl, balance_funded_with_ts) =
            build_template_and_funded(0, false, expiry_since);
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &balance_tmpl,
            &balance_funded_with_ts,
            None,
            Some(&sig),
        )
        .is_ok());
        // Balance withdrawal with an expired TLC still present in witness (htlc_count == 1,
        // unlock_type == 0xFE) must ALSO allow a mature absolute-timestamp wallet funding input.
        let mut balance_with_expired_tlc_w = vec![
            16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0,    // xudt placeholder
            0x01, // unlock_count = 1
            0x01, // htlc_count = 1 (expired TLC retained in witness)
        ];
        balance_with_expired_tlc_w.extend(std::iter::repeat_n(0x11u8, 97));
        balance_with_expired_tlc_w.extend(std::iter::repeat_n(0x22u8, 72));
        balance_with_expired_tlc_w.push(0xfe); // balance unlock_type
        balance_with_expired_tlc_w.push(0x00); // with_preimage = false
        balance_with_expired_tlc_w.extend_from_slice(&[0u8; 65]);
        let balance_with_expired_tmpl = balance_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![balance_with_expired_tlc_w.clone().pack()])
            .build()
            .data();
        let balance_with_expired_funded = balance_funded_with_ts
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                balance_with_expired_tlc_w.pack(),
                balance_funded_with_ts.witnesses().get(1).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &balance_with_expired_tmpl,
            &balance_with_expired_funded,
            None,
            Some(&sig),
        )
        .is_ok());
        let (_, preimage_funded_bad_since) = build_template_and_funded(1, true, expiry_since);
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &preimage_tmpl,
            &preimage_funded_bad_since,
            None,
            Some(&sig),
        )
        .is_err());

        // 5a. Placeholder lock, missing template cell_deps, occupied capacity 1-shannon boundary,
        //     and UDT payout preservation (burn vs split).
        // Placeholder lock left on outputs[1] of a 2-output tx is rejected.
        let placeholder_out1 = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                funded.raw().outputs().get(0).unwrap(),
                CellOutput::new_builder()
                    .type_(Some(udt_type.clone()).pack())
                    .capacity(14_200_000_000u64)
                    .build(),
                funded.raw().outputs().get(2).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &placeholder_out1,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        // Missing template cell_deps rejected.
        let empty_deps = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_cell_deps(vec![])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &empty_deps,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        // Duplicate cell_deps rejected before consuming the request (CKB DuplicateCellDeps).
        let template_dep = template.raw().cell_deps().get(0).unwrap();
        let dup_deps = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .cell_dep(template_dep)
            .build()
            .data();
        let dup_err = validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &dup_deps,
            Some(expiry_since),
            Some(&sig),
        )
        .expect_err("duplicate cell_deps must be rejected");
        assert!(dup_err.contains("duplicate cell_deps"), "{dup_err}");

        // Occupied capacity one-shannon boundary on a standard 20-byte-args lock (61 CKB = 6_100_000_000).
        let exact_change = CellOutput::new_builder()
            .lock(wallet_lock.clone())
            .capacity(6_100_000_000u64)
            .build();
        let exact_occ = exact_change
            .occupied_capacity(ckb_types::core::Capacity::bytes(0).unwrap())
            .unwrap()
            .as_u64();
        assert_eq!(exact_occ, 6_100_000_000u64);
        let under_occupied_final = final_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .capacity(exact_occ - 1)
                .build()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_tmpl,
            &under_occupied_final,
            None,
            Some(&sig),
        )
        .is_err());
        let exact_occupied_final = final_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .capacity(exact_occ)
                .build()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_tmpl,
            &exact_occupied_final,
            None,
            Some(&sig),
        )
        .is_ok());

        // UDT payout preservation on final 1-output template: dropping type/data (burn) rejected,
        // underpaying rejected, splitting across two UDT outputs totaling template amount accepted.
        let final_udt_tmpl = ckb_types::core::TransactionBuilder::default()
            .cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([0x10; 32]), 0))
                    .dep_type(DepType::DepGroup)
                    .build(),
            )
            .input(preimage_tmpl.raw().inputs().get(0).unwrap())
            .output(
                CellOutput::new_builder()
                    .type_(Some(udt_type.clone()).pack())
                    .capacity(14_200_000_000u64)
                    .build(),
            )
            .output_data(100u128.to_le_bytes().to_vec().pack())
            .witness(build_template_witness(0, false, [0u8; 65]).pack())
            .build()
            .data();
        let burned_udt = final_udt_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .capacity(14_199_999_000u64)
                .build()])
            .set_outputs_data(vec![Bytes::default()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_udt_tmpl,
            &burned_udt,
            None,
            Some(&sig),
        )
        .is_err());
        let underpaid_udt = final_udt_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![CellOutput::new_builder()
                .lock(wallet_lock.clone())
                .type_(Some(udt_type.clone()).pack())
                .capacity(14_200_000_000u64)
                .build()])
            .set_outputs_data(vec![99u128.to_le_bytes().to_vec().pack()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_udt_tmpl,
            &underpaid_udt,
            None,
            Some(&sig),
        )
        .is_err());
        let split_udt = final_udt_tmpl
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                CellOutput::new_builder()
                    .lock(wallet_lock.clone())
                    .type_(Some(udt_type.clone()).pack())
                    .capacity(14_200_000_000u64)
                    .build(),
                CellOutput::new_builder()
                    .lock(wallet_lock)
                    .type_(Some(udt_type).pack())
                    .capacity(14_200_000_000u64)
                    .build(),
            ])
            .set_outputs_data(vec![
                40u128.to_le_bytes().to_vec().pack(),
                60u128.to_le_bytes().to_vec().pack(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &final_udt_tmpl,
            &split_udt,
            None,
            Some(&sig),
        )
        .is_ok());

        // 5b. Multi-input same-lock wallet omitting trailing empty witnesses (e.g. 3 inputs, 2 witnesses) is accepted.
        let multi_same_lock = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .input(CellInput::new(
                OutPoint::new(Byte32::from([0x23; 32]), 1),
                0,
            ))
            .set_witnesses(vec![
                funded.witnesses().get(0).unwrap(),
                funded.witnesses().get(1).unwrap(),
            ])
            .build()
            .data();
        assert_eq!(multi_same_lock.raw().inputs().len(), 3);
        assert_eq!(multi_same_lock.witnesses().len(), 2);
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &multi_same_lock,
            Some(expiry_since),
            Some(&sig),
        )
        .is_ok());

        // 6. Duplicate inputs (both duplicating inputs[0] and duplicates among inputs[1..]) rejected.
        let dup_input = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                funded.raw().inputs().get(0).unwrap(),
                CellInput::new(
                    funded.raw().inputs().get(0).unwrap().previous_output(),
                    expiry_since,
                ),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &dup_input,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let dup_funding_input = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .input(funded.raw().inputs().get(1).unwrap())
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &dup_funding_input,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        // 6b. Template with non-zero signature slot is rejected.
        let nonzero_template = template
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![build_template_witness(1, false, sig).pack()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &nonzero_template,
            &funded,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        // 6c. Retry witness[0] validation and hash binding reject missing or tampered witness[0].
        let funded_with_sig = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                build_template_witness(1, false, sig).pack(),
                funded.witnesses().get(1).unwrap(),
                funded.witnesses().get(2).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_external_funding_retry_witness_zero(&funded, &sig).is_ok());
        assert!(validate_external_funding_retry_witness_zero(&funded_with_sig, &sig).is_ok());
        assert_eq!(
            external_funding_watchtower_tx_hash(&funded),
            external_funding_watchtower_tx_hash(&funded_with_sig)
        );
        let missing_w0 = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![])
            .build()
            .data();
        assert!(validate_external_funding_retry_witness_zero(&missing_w0, &sig).is_err());
        assert_ne!(
            external_funding_watchtower_tx_hash(&funded),
            external_funding_watchtower_tx_hash(&missing_w0)
        );
        let wrong_sig_w0 = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                build_template_witness(1, false, [0x99u8; 65]).pack(),
                funded.witnesses().get(1).unwrap(),
                funded.witnesses().get(2).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_external_funding_retry_witness_zero(&wrong_sig_w0, &sig).is_err());

        // 7. Tampered inputs[0], outputs[0] (when template has 2 outputs), outputs_data[0],
        //    witness[0], non-zero version, or non-empty header_deps rejected.
        let bad_in0_outpoint = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                CellInput::new(OutPoint::new(Byte32::from([0x99; 32]), 0), 0),
                funded.raw().inputs().get(1).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_in0_outpoint,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let bad_in0_since = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_inputs(vec![
                CellInput::new(
                    funded.raw().inputs().get(0).unwrap().previous_output(),
                    expiry_since,
                ),
                funded.raw().inputs().get(1).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_in0_since,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let orig_out0 = funded.raw().outputs().get(0).unwrap();
        let orig_lock = orig_out0.lock();
        let mut flipped_lock_args = orig_lock.args().raw_data().to_vec();
        assert!(!flipped_lock_args.is_empty());
        flipped_lock_args[0] ^= 0x01;
        let tampered_out0 = orig_out0
            .as_builder()
            .lock(
                orig_lock
                    .as_builder()
                    .args(flipped_lock_args.pack())
                    .build(),
            )
            .build();
        let bad_out0 = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs(vec![
                tampered_out0,
                funded.raw().outputs().get(1).unwrap(),
                funded.raw().outputs().get(2).unwrap(),
            ])
            .build()
            .data();
        let bad_out0_err = validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_out0,
            Some(expiry_since),
            Some(&sig),
        )
        .expect_err("tampered output[0] must be rejected");
        assert!(
            bad_out0_err.contains("output[0] must match template commitment output"),
            "unexpected error: {bad_out0_err}"
        );

        let bad_out_data0 = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_outputs_data(vec![
                99u128.to_le_bytes().to_vec().pack(),
                funded.raw().outputs_data().get(1).unwrap(),
                funded.raw().outputs_data().get(2).unwrap(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_out_data0,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let bad_version = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .version(1u32)
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_version,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let bad_header_deps = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .header_dep(Byte32::from([0xab; 32]))
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &bad_header_deps,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let mut bad_preimage_w0 = build_template_witness(1, true, [0u8; 65]);
        *bad_preimage_w0.last_mut().unwrap() ^= 1;
        let bad_w0_tx = preimage_funded_ok
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![bad_preimage_w0.pack(), [0x01u8].pack()])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &preimage_tmpl,
            &bad_w0_tx,
            None,
            Some(&sig),
        )
        .is_err());

        // 8. Oversized tx (bytes) and excessive item counts rejected.
        let oversized_witness = funded
            .clone()
            .into_view()
            .as_advanced_builder()
            .set_witnesses(vec![
                funded.witnesses().get(0).unwrap(),
                vec![0u8; MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_BYTES + 1].pack(),
            ])
            .build()
            .data();
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &oversized_witness,
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());

        let mut too_many_deps = funded.clone().into_view().as_advanced_builder();
        for i in 0..MAX_EXTERNAL_FUNDING_WATCHTOWER_TX_ITEMS {
            too_many_deps = too_many_deps.cell_dep(
                CellDep::new_builder()
                    .out_point(OutPoint::new(Byte32::from([i as u8; 32]), i as u32))
                    .dep_type(DepType::Code)
                    .build(),
            );
        }
        assert!(validate_and_normalize_external_funding_watchtower_tx(
            &template,
            &too_many_deps.build().data(),
            Some(expiry_since),
            Some(&sig),
        )
        .is_err());
    }

    #[test]
    fn first_pending_prefers_self_fundable_claim_over_lower_id_timeout() {
        use ckb_types::core::TransactionBuilder;
        use ckb_types::prelude::*;

        let make_req = |payout_capacity: u64, required_expiry_since: Option<u64>| {
            let tx = TransactionBuilder::default()
                .output(CellOutput::new_builder().capacity(payout_capacity).build())
                .output_data(ckb_types::packed::Bytes::default())
                .build()
                .data();
            PendingWatchtowerRequest {
                content: crate::channel_signer::OnchainSigningContent {
                    key_purpose: crate::channel_signer::OnchainKeyPurpose::Settlement,
                    transaction: tx,
                },
                created_at_ms: 1_000,
                required_expiry_since,
            }
        };

        let timeout_id = Hash256::from([0x01; 32]);
        let small_id = Hash256::from([0x02; 32]);
        let self_fundable_id = Hash256::from([0xff; 32]);
        let mut state = WatchtowerExternalSignerState::default();
        state.pending_requests.insert(
            timeout_id,
            make_req(10_000_000_000, Some(settlement_timestamp_since(100_000))),
        );
        state
            .pending_requests
            .insert(small_id, make_req(5_100_000_000, None));
        state
            .pending_requests
            .insert(self_fundable_id, make_req(20_000_000_000, None));

        // Even though `self_fundable_id` > `timeout_id`, the self-fundable CKB claim must be exposed.
        assert_eq!(
            state.first_pending().map(|(id, _)| id),
            Some(self_fundable_id)
        );
        state.pending_requests.remove(&self_fundable_id);
        // Without a self-fundable claim, non-timeout still precedes a lower-ID timeout claim.
        assert_eq!(state.first_pending().map(|(id, _)| id), Some(small_id));
    }

    #[test]
    fn first_pending_requires_fee_buffer_for_self_fundable_threshold() {
        use ckb_types::core::TransactionBuilder;
        use ckb_types::prelude::*;

        let make_req = |payout_capacity: u64, type_script: Option<Script>| {
            let tx = TransactionBuilder::default()
                .output(
                    CellOutput::new_builder()
                        .capacity(payout_capacity)
                        .type_(type_script.pack())
                        .build(),
                )
                .output_data(ckb_types::packed::Bytes::default())
                .build()
                .data();
            PendingWatchtowerRequest {
                content: crate::channel_signer::OnchainSigningContent {
                    key_purpose: crate::channel_signer::OnchainKeyPurpose::Settlement,
                    transaction: tx,
                },
                created_at_ms: 1_000,
                required_expiry_since: None,
            }
        };

        let barely_above_61_req = make_req(6_100_000_001, None);
        let at_threshold_req = make_req(EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY, None);
        let ckb_200_req = make_req(20_000_000_000, None);
        let udt_200_req = make_req(20_000_000_000, Some(test_udt_type()));

        assert!(!pending_request_priority(&barely_above_61_req).0);
        assert!(pending_request_priority(&at_threshold_req).0);
        assert!(pending_request_priority(&ckb_200_req).0);
        assert!(!pending_request_priority(&udt_200_req).0);

        let low_id = Hash256::from([0x01; 32]);
        let high_id = Hash256::from([0xff; 32]);

        // 61 CKB + 1 shannon with a lower request_id must rank below a 200 CKB claim.
        let mut ckb_state = WatchtowerExternalSignerState::default();
        ckb_state
            .pending_requests
            .insert(low_id, barely_above_61_req.clone());
        ckb_state.pending_requests.insert(high_id, ckb_200_req);
        assert_eq!(ckb_state.first_pending().map(|(id, _)| id), Some(high_id));

        // Against a 200 CKB UDT claim (not self-fundable), 61 CKB + 1 shannon also ranks below it
        // because it lacks the fee buffer, whereas EXTERNAL_SELF_FUNDABLE_CKB_MIN_CAPACITY ranks above it.
        let mut udt_state = WatchtowerExternalSignerState::default();
        udt_state
            .pending_requests
            .insert(low_id, barely_above_61_req);
        udt_state.pending_requests.insert(high_id, udt_200_req);
        assert_eq!(udt_state.first_pending().map(|(id, _)| id), Some(high_id));

        let threshold_id = Hash256::from([0xfe; 32]);
        udt_state
            .pending_requests
            .insert(threshold_id, at_threshold_req);
        assert_eq!(
            udt_state.first_pending().map(|(id, _)| id),
            Some(threshold_id)
        );
    }
}
