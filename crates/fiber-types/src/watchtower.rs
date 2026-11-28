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
mod tests;

#[cfg(test)]
mod key_material_tests;
