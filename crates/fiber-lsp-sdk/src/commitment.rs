//! Mandatory validation of commitments against signer-owned channel and payment state.

use ckb_types::{
    packed::{CellOutput, Script, Transaction},
    prelude::*,
};
use fiber_types::{
    blake2b_hash_with_salt, ChannelSigningTransition, Hash256, HashAlgorithm, InMemorySigner,
    Pubkey, SettlementData, SettlementTlc,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    ChannelBinding, ChannelSigningContent, Musig2SignableContent, OwnedSettlementBinding,
    SignerError,
};

/// Immutable channel terms approved locally, never learned from a signing request.
/// Amounts include reserved CKB for CKB channels, and are token units for UDT channels.
#[serde_as]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitmentParameters {
    /// Counterparty funding key approved when opening the channel.
    pub remote_funding_key: Pubkey,
    /// Counterparty settlement base key approved when opening the channel.
    pub remote_settlement_key: Pubkey,
    /// Counterparty punishment/close destination approved at opening.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub remote_shutdown_script: Script,
    /// Trusted network CommitmentLock script, with empty args.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub commitment_lock: Script,
    /// Exact eight-byte relative epoch delay negotiated at opening.
    pub delay_epoch: [u8; 8],
    /// Trusted network epoch duration in milliseconds (not supplied by the LSP).
    pub epoch_duration_ms: u64,
    /// Approved commitment transaction fee in shannons.
    pub commitment_fee: u64,
    /// Initial signer balance, including its reserve in CKB channels.
    pub local_amount: u128,
    /// Initial counterparty balance, including its reserve in CKB channels.
    pub remote_amount: u128,
    /// Signer's negotiated CKB reserve; not spendable by an off-chain payment.
    pub local_reserved_ckb_amount: u64,
    /// Counterparty's negotiated CKB reserve.
    pub remote_reserved_ckb_amount: u64,
}

impl CommitmentParameters {
    pub(crate) fn safety_margin_ms(&self) -> Result<u64, SignerError> {
        let since = u64::from_le_bytes(self.delay_epoch);
        if since >> 56 != 0xa0 || self.epoch_duration_ms == 0 {
            return Err(invalid(
                "invalid relative epoch delay or network epoch duration",
            ));
        }
        let epoch = ckb_types::core::EpochNumberWithFraction::from_full_value(
            since & 0x00ff_ffff_ffff_ffff,
        );
        if epoch.length() == 0 || epoch.index() >= epoch.length() {
            return Err(invalid("invalid commitment epoch fraction"));
        }
        // The node reserves 2/3 of the commitment delay for on-chain recovery. Use
        // checked integer arithmetic and round up rather than weakening that deadline.
        let numerator = (u128::from(epoch.number()) * u128::from(epoch.length())
            + u128::from(epoch.index()))
        .checked_mul(u128::from(self.epoch_duration_ms))
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| invalid("commitment delay overflow"))?;
        let denominator = u128::from(epoch.length()) * 3;
        let margin = u64::try_from(numerator.div_ceil(denominator))
            .map_err(|_| invalid("commitment delay overflow"))?;
        if margin == 0 {
            return Err(invalid("zero commitment safety margin"));
        }
        Ok(margin)
    }
}

/// Locally approved fixed-amount, single-part payment. Scoped to one channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAuthorization {
    /// Payment preimage hash.
    pub payment_hash: Hash256,
    /// Hash algorithm chosen by the wallet.
    pub hash_algorithm: HashAlgorithm,
    /// True for an invoice issued by this wallet, false for an approved payment.
    pub inbound: bool,
    /// Exact amount in this channel's asset. No implicit fee deduction is allowed.
    pub amount: u128,
    /// Last local time (milliseconds) at which a new TLC may be accepted.
    pub expires_at_ms: u64,
    /// Minimum remaining TLC lifetime at first acceptance.
    pub min_tlc_expiry_delta_ms: u64,
    /// Maximum acceptable absolute TLC expiry.
    pub max_tlc_expiry_ms: u64,
}

/// Exact context accompanying one commitment. The snapshot uses Fiber witness orientation;
/// use [`Self::inbound`] to obtain the wallet-relative direction.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitmentContext {
    /// Protocol transition returned by the node.
    pub transition: ChannelSigningTransition,
    /// Participant nonce and peer signature for this exact session.
    pub session: crate::SigningSessionEvidence,
    /// Snapshot to independently bind to the transaction.
    pub settlement: OwnedSettlementBinding,
}

impl CommitmentContext {
    /// Whether a TLC is inbound to the signer, regardless of commitment orientation.
    pub fn inbound(&self, tlc: &SettlementTlc) -> bool {
        tlc.tlc_id.is_received() == (self.settlement.for_remote == Some(true))
    }
}

/// Validated state changes suitable for a signing UI. Amounts are settlement amounts,
/// not spendable wallet balances or final payment outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitmentReview {
    /// Whether this is the counterparty's commitment transaction.
    pub for_remote: bool,
    /// Actual version encoded in CommitmentLock, not the MuSig2 nonce counter.
    pub version: u64,
    /// Previous balance in this commitment lane.
    pub previous_local_amount: u128,
    /// Signer's balance in the proposed state.
    pub local_amount: u128,
    /// Counterparty's balance in the proposed state.
    pub remote_amount: u128,
    /// Whether this state contains or resolves a locally authorized outbound payment.
    pub has_outbound_changes: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Resolution {
    Pending,
    Fulfill,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct AuthorizedPayment {
    pub terms: PaymentAuthorization,
    pub resolution: Resolution,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TrackedTlc {
    id: u64,
    inbound: bool,
    hash: Hash256,
    algorithm: HashAlgorithm,
    amount: u128,
    expiry: u64,
    local_key: Pubkey,
    derivation: u64,
    remote_key: Pubkey,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lane {
    version: u64,
    content_hash: [u8; 32],
    nonce_counter: u64,
    local_amount: u128,
    remote_amount: u128,
    tlcs: Vec<TrackedTlc>,
    retired: Vec<TrackedTlc>,
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CommitmentState {
    pub parameters: CommitmentParameters,
    #[serde_as(as = "fiber_types::EntityHex")]
    pub funding_output: CellOutput,
    pub funding_data: Vec<u8>,
    pub payments: Vec<AuthorizedPayment>,
    lanes: [Option<Lane>; 2],
    pub lifecycle: crate::lifecycle::LifecycleState,
}

pub(crate) fn invalid(message: &str) -> SignerError {
    SignerError::InvalidContent(format!("commitment validation: {message}"))
}

fn sum(a: u128, b: u128) -> Result<u128, SignerError> {
    a.checked_add(b).ok_or_else(|| invalid("amount overflow"))
}
fn sub(a: u128, b: u128) -> Result<u128, SignerError> {
    a.checked_sub(b)
        .ok_or_else(|| invalid("unfunded state transition"))
}

impl CommitmentState {
    pub fn new(
        parameters: CommitmentParameters,
        funding_output: CellOutput,
        funding_data: Vec<u8>,
    ) -> Result<Self, SignerError> {
        parameters.safety_margin_ms()?;
        if !parameters.commitment_lock.args().is_empty() {
            return Err(invalid("approved commitment lock must have empty args"));
        }
        let total = sum(parameters.local_amount, parameters.remote_amount)?;
        let capacity: u64 = funding_output.capacity().unpack();
        if capacity <= parameters.commitment_fee {
            return Err(invalid("commitment fee consumes funding capacity"));
        }
        let expected_total = if funding_output.type_().to_opt().is_some() {
            if parameters
                .local_reserved_ckb_amount
                .checked_add(parameters.remote_reserved_ckb_amount)
                != Some(capacity)
            {
                return Err(invalid(
                    "UDT funding capacity does not match approved CKB reserves",
                ));
            }
            u128::from_le_bytes(
                funding_data
                    .as_slice()
                    .try_into()
                    .map_err(|_| invalid("invalid funding token amount"))?,
            )
        } else {
            if parameters.local_amount < u128::from(parameters.local_reserved_ckb_amount)
                || parameters.remote_amount < u128::from(parameters.remote_reserved_ckb_amount)
            {
                return Err(invalid("opening balances do not cover reserved CKB"));
            }
            if !funding_data.is_empty() {
                return Err(invalid("unexpected CKB funding data"));
            }
            u128::from(capacity)
        };
        if total != expected_total {
            return Err(invalid(
                "initial balances do not equal approved funding amount",
            ));
        }
        Ok(Self {
            parameters,
            funding_output,
            funding_data,
            payments: Vec::new(),
            lanes: [None, None],
            lifecycle: Default::default(),
        })
    }

    pub fn authorize(&mut self, terms: PaymentAuthorization) -> Result<(), SignerError> {
        if terms.amount == 0
            || terms.min_tlc_expiry_delta_ms == 0
            || terms.max_tlc_expiry_ms <= terms.expires_at_ms
        {
            return Err(invalid("invalid payment amount or expiry bounds"));
        }
        if let Some(previous) = self
            .payments
            .iter()
            .find(|p| p.terms.payment_hash == terms.payment_hash)
        {
            return if previous.terms == terms {
                Ok(())
            } else {
                Err(invalid("payment authorization cannot be replaced"))
            };
        }
        self.payments.push(AuthorizedPayment {
            terms,
            resolution: Resolution::Pending,
        });
        Ok(())
    }

    pub fn resolve(&mut self, hash: Hash256, preimage: Option<Hash256>) -> Result<(), SignerError> {
        let payment = self
            .payments
            .iter_mut()
            .find(|p| p.terms.payment_hash == hash)
            .ok_or_else(|| invalid("unknown payment"))?;
        let resolution = match preimage {
            Some(preimage) => {
                if payment.terms.hash_algorithm.hash(preimage.as_ref()) != *hash.as_ref() {
                    return Err(invalid("preimage does not match payment hash"));
                }
                Resolution::Fulfill
            }
            None => Resolution::Cancel,
        };
        if payment.resolution != Resolution::Pending && payment.resolution != resolution {
            return Err(invalid("payment resolution cannot change after approval"));
        }
        payment.resolution = resolution;
        Ok(())
    }

    pub fn validate(
        &mut self,
        content: &ChannelSigningContent,
        context: &CommitmentContext,
        binding: &ChannelBinding,
        signer: &InMemorySigner,
        now_ms: u64,
    ) -> Result<CommitmentReview, SignerError> {
        let for_remote = match context.transition {
            ChannelSigningTransition::SendCommitmentSigned => true,
            ChannelSigningTransition::CompleteReceivedCommitment => false,
            _ => return Err(invalid("incorrect commitment transition")),
        };
        let snapshot = &context.settlement;
        if snapshot.for_remote != Some(for_remote) {
            return Err(invalid("commitment orientation disagrees with transition"));
        }
        if snapshot.local_settlement_key != signer.tlc_base_key.pubkey()
            || snapshot.remote_settlement_key != self.parameters.remote_settlement_key
        {
            return Err(invalid(
                "settlement keys do not belong to the approved channel",
            ));
        }
        let ChannelSigningContent::Musig2(musig) = content else {
            return Err(invalid("expected MuSig2 commitment"));
        };
        let Musig2SignableContent::CommitmentTransaction(tx) = &musig.content else {
            return Err(invalid("expected commitment transaction"));
        };
        // Both channel commitment signing transitions currently use the local nonce counter.
        if musig.commitment_counter != Some(crate::CommitmentCounter::Local) {
            return Err(invalid("incorrect commitment nonce counter"));
        }
        let version = self.validate_transaction(tx, binding, signer, for_remote)?;
        if for_remote && version != musig.slot.commitment_number {
            return Err(invalid(
                "commitment version disagrees with local nonce counter",
            ));
        }
        if snapshot.data.tlcs.len() > 255 {
            return Err(invalid("too many TLCs"));
        }
        // Reject missing keys before witness construction (which expects a local public key).
        let safety_deadline = now_ms
            .checked_add(self.parameters.safety_margin_ms()?)
            .ok_or_else(|| invalid("safety deadline overflow"))?;
        let mut tlcs = Vec::new();
        for tlc in &snapshot.data.tlcs {
            // Commitment witnesses encode seconds; do not overstate the remaining lifetime
            // by trusting the uncommitted millisecond remainder supplied by the node.
            if (tlc.expiry / 1000) * 1000 <= safety_deadline {
                return Err(invalid(
                    "commitment contains an expired TLC or insufficient on-chain safety window",
                ));
            }
            let derivation = tlc
                .local_key_commitment_number
                .ok_or_else(|| invalid("missing TLC key derivation index"))?;
            if derivation >= (1u64 << 48) {
                return Err(invalid("TLC derivation index exceeds the protocol range"));
            }
            let local_key = tlc
                .local_key_pubkey
                .ok_or_else(|| invalid("missing TLC public key"))?;
            if tlc.local_key.is_some() || signer.derive_tlc_key(derivation).pubkey() != local_key {
                return Err(invalid("TLC public key is not owned by this signer"));
            }
            let inbound = context.inbound(tlc);
            let id = u64::from(tlc.tlc_id);
            if tlcs.iter().any(|p: &TrackedTlc| {
                (p.id == id && p.inbound == inbound) || p.hash == tlc.payment_hash
            }) {
                return Err(invalid("duplicate TLC id or single-part payment hash"));
            }
            tlcs.push(TrackedTlc {
                id,
                inbound,
                hash: tlc.payment_hash,
                algorithm: tlc.hash_algorithm,
                amount: tlc.payment_amount,
                expiry: tlc.expiry,
                local_key,
                derivation,
                remote_key: tlc.remote_key,
            });
        }
        if !fiber_types::settlement_matches_commitment_tx(
            tx,
            &snapshot.data,
            snapshot.local_settlement_key,
            snapshot.remote_settlement_key,
            Some(for_remote),
        ) {
            return Err(invalid("settlement snapshot does not match transaction"));
        }
        self.validate_total(&snapshot.data)?;
        let content_hash = content
            .content_hash(&content.canonical_bytes())
            .map_err(|error| invalid(&error))?;
        let index = usize::from(for_remote);
        let previous = self.lanes[index].as_ref();
        let previous_local = previous.map_or(self.parameters.local_amount, |p| p.local_amount);
        if let Some(previous) = previous {
            if version == previous.version && content_hash == previous.content_hash {
                if previous.tlcs != tlcs
                    || previous.local_amount != snapshot.data.local_amount
                    || previous.remote_amount != snapshot.data.remote_amount
                {
                    return Err(invalid("retry changed unsigned context"));
                }
                return Ok(CommitmentReview {
                    for_remote,
                    version,
                    previous_local_amount: previous.local_amount,
                    local_amount: previous.local_amount,
                    remote_amount: previous.remote_amount,
                    has_outbound_changes: tlcs.iter().any(|t| !t.inbound),
                });
            }
            if previous.version.checked_add(1) != Some(version) {
                return Err(invalid("commitment version rollback, conflict or gap"));
            }
            if musig.slot.commitment_number < previous.nonce_counter {
                return Err(invalid("commitment nonce counter rolled back"));
            }
        } else if version != 1
            || !tlcs.is_empty()
            || snapshot.data.local_amount != self.parameters.local_amount
            || snapshot.data.remote_amount != self.parameters.remote_amount
        {
            return Err(invalid(
                "first commitment must match approved opening balances at version one",
            ));
        }
        let old_tlcs = previous.map_or(&[][..], |p| p.tlcs.as_slice());
        let mut retired = previous.map_or_else(Vec::new, |p| p.retired.clone());
        let mut local = previous_local;
        let mut remote = previous.map_or(self.parameters.remote_amount, |p| p.remote_amount);
        let mut outbound = tlcs.iter().any(|t| !t.inbound);
        for tlc in &tlcs {
            let existing = old_tlcs
                .iter()
                .find(|t| t.id == tlc.id && t.inbound == tlc.inbound);
            if let Some(existing) = existing {
                if existing != tlc {
                    return Err(invalid("existing TLC was modified"));
                }
                continue;
            }
            if retired
                .iter()
                .any(|t| t.hash == tlc.hash || (t.id == tlc.id && t.inbound == tlc.inbound))
            {
                return Err(invalid("retired TLC or invoice replay"));
            }
            // The other lane can already contain this TLC; do not reapply wall-clock invoice
            // acceptance limits while completing the second half of the protocol exchange.
            let other = self.lanes[1 - index].as_ref().and_then(|lane| {
                lane.tlcs
                    .iter()
                    .chain(&lane.retired)
                    .find(|t| t.hash == tlc.hash)
            });
            if let Some(other) = other {
                if other != tlc {
                    return Err(invalid("TLC differs between commitment lanes"));
                }
            }
            let payment = self
                .payments
                .iter()
                .find(|p| p.terms.payment_hash == tlc.hash)
                .ok_or_else(|| invalid("TLC has no local payment authorization"))?;
            let terms = &payment.terms;
            if terms.inbound != tlc.inbound
                || terms.amount != tlc.amount
                || terms.hash_algorithm != tlc.algorithm
            {
                return Err(invalid(
                    "TLC does not match authorized direction, amount or hash algorithm",
                ));
            }
            if other.is_none()
                && (payment.resolution != Resolution::Pending
                    || now_ms >= terms.expires_at_ms
                    || (tlc.expiry / 1000) * 1000
                        < now_ms
                            .checked_add(terms.min_tlc_expiry_delta_ms)
                            .ok_or_else(|| invalid("expiry overflow"))?
                    || tlc.expiry > terms.max_tlc_expiry_ms)
            {
                return Err(invalid(
                    "new TLC is resolved, expired or outside authorized expiry bounds",
                ));
            }
            if tlc.inbound {
                remote = sub(remote, tlc.amount)?;
            } else {
                local = sub(local, tlc.amount)?;
            }
        }
        for old in old_tlcs {
            if tlcs
                .iter()
                .any(|t| t.id == old.id && t.inbound == old.inbound)
            {
                continue;
            }
            let payment = self
                .payments
                .iter()
                .find(|p| p.terms.payment_hash == old.hash)
                .ok_or_else(|| invalid("removed TLC has no authorization"))?;
            let pay_local = match payment.resolution {
                Resolution::Pending => {
                    return Err(invalid(
                        "TLC removed without a locally authorized resolution",
                    ))
                }
                Resolution::Fulfill => old.inbound,
                Resolution::Cancel => !old.inbound,
            };
            if pay_local {
                local = sum(local, old.amount)?;
            } else {
                remote = sum(remote, old.amount)?;
            }
            outbound |= !old.inbound;
            retired.push(old.clone());
        }
        if local != snapshot.data.local_amount || remote != snapshot.data.remote_amount {
            return Err(invalid("balances do not match authorized TLC transitions"));
        }
        self.lanes[index] = Some(Lane {
            version,
            content_hash,
            nonce_counter: musig.slot.commitment_number,
            local_amount: local,
            remote_amount: remote,
            tlcs,
            retired,
        });
        Ok(CommitmentReview {
            for_remote,
            version,
            previous_local_amount: previous_local,
            local_amount: local,
            remote_amount: remote,
            has_outbound_changes: outbound,
        })
    }

    fn validate_total(&self, snapshot: &SettlementData) -> Result<(), SignerError> {
        if self.funding_output.type_().to_opt().is_none()
            && (snapshot.local_amount < u128::from(self.parameters.local_reserved_ckb_amount)
                || snapshot.remote_amount < u128::from(self.parameters.remote_reserved_ckb_amount))
        {
            return Err(invalid("commitment spends reserved CKB"));
        }
        let mut total = sum(snapshot.local_amount, snapshot.remote_amount)?;
        for tlc in &snapshot.tlcs {
            total = sum(total, tlc.payment_amount)?;
        }
        if total != sum(self.parameters.local_amount, self.parameters.remote_amount)? {
            return Err(invalid("settlement does not conserve channel funds"));
        }
        Ok(())
    }

    fn validate_transaction(
        &self,
        tx: &Transaction,
        binding: &ChannelBinding,
        signer: &InMemorySigner,
        for_remote: bool,
    ) -> Result<u64, SignerError> {
        let raw = tx.raw();
        if raw.version() != 0u32.pack()
            || raw.inputs().len() != 1
            || raw.outputs().len() != 1
            || raw.outputs_data().len() != 1
            || !raw.cell_deps().is_empty()
            || !raw.header_deps().is_empty()
            || !tx.witnesses().is_empty()
        {
            return Err(invalid("unexpected commitment transaction structure"));
        }
        let input = raw
            .inputs()
            .get(0)
            .ok_or_else(|| invalid("missing funding input"))?;
        if input.previous_output() != binding.funding_outpoint || input.since() != 0u64.pack() {
            return Err(invalid("incorrect funding input or since"));
        }
        let output = raw
            .outputs()
            .get(0)
            .ok_or_else(|| invalid("missing commitment output"))?;
        let lock = output.lock();
        let args = lock.args().raw_data();
        let expected = &self.parameters.commitment_lock;
        if lock.code_hash() != expected.code_hash()
            || lock.hash_type() != expected.hash_type()
            || args.len() != 57
            || args[56] != 0
        {
            return Err(invalid(
                "incorrect commitment lock contract or argument layout",
            ));
        }
        let local = signer.funding_key.pubkey();
        let remote = self.parameters.remote_funding_key;
        let keys = if for_remote {
            [local, remote]
        } else {
            [remote, local]
        };
        let ctx = musig2::KeyAggContext::new(keys).map_err(|e| invalid(&e.to_string()))?;
        let point: musig2::secp::Point = ctx.aggregated_pubkey();
        let hash = blake2b_hash_with_salt(&point.serialize_xonly(), &[]);
        if args[..20] != hash[..20] || args[20..28] != self.parameters.delay_epoch {
            return Err(invalid("incorrect commitment revocation key or delay"));
        }
        let capacity: u64 = self.funding_output.capacity().unpack();
        if output.capacity() != (capacity - self.parameters.commitment_fee).pack()
            || output.type_() != self.funding_output.type_()
            || raw
                .outputs_data()
                .get(0)
                .ok_or_else(|| invalid("missing output data"))?
                .raw_data()
                .as_ref()
                != self.funding_data
        {
            return Err(invalid(
                "commitment asset, amount or fee differs from approved funding",
            ));
        }
        Ok(u64::from_be_bytes(
            args[28..36]
                .try_into()
                .map_err(|_| invalid("invalid commitment version"))?,
        ))
    }
}

impl CommitmentState {
    pub(crate) fn validate_revocation(
        &mut self,
        content: &ChannelSigningContent,
        context: &crate::RevocationContext,
        binding: &ChannelBinding,
        signer: &InMemorySigner,
        now_ms: u64,
    ) -> Result<(), SignerError> {
        use crate::lifecycle::{validate_session, RevocationRecord};
        validate_session(
            content,
            &context.session,
            context.transition,
            &self.parameters,
            signer,
        )?;
        let receiving = match context.transition {
            ChannelSigningTransition::SendRevokeAndAck => false,
            ChannelSigningTransition::CompleteReceivedRevokeAndAck => true,
            _ => return Err(invalid("invalid revocation transition")),
        };
        if context.revoked.for_remote != receiving
            || context.replacement.for_remote != receiving
            || context.revoked.version.checked_add(1) != Some(context.replacement.version)
        {
            return Err(invalid("invalid revoked/replacement lane or version"));
        }
        let old = self.lifecycle.find(&context.revoked)?;
        let replacement = self.lifecycle.find(&context.replacement)?;
        let ChannelSigningContent::Musig2(m) = content else {
            return Err(invalid("expected revocation"));
        };
        if m.slot.commitment_number != context.replacement.version {
            return Err(invalid(
                "revocation nonce counter differs from replacement version",
            ));
        }
        if !receiving && replacement.complete_signature.is_none() {
            return Err(invalid("replacement commitment is not recoverable"));
        }
        // Recheck recovery time even if the signature request has waited in a UI.
        let deadline = now_ms
            .checked_add(self.parameters.safety_margin_ms()?)
            .ok_or_else(|| invalid("deadline overflow"))?;
        if replacement
            .settlement
            .data
            .tlcs
            .iter()
            .any(|t| (t.expiry / 1000) * 1000 <= deadline)
        {
            return Err(invalid("replacement TLC recovery window expired"));
        }
        let Musig2SignableContent::Revocation {
            output,
            output_data,
            commitment_lock_script_args,
        } = &m.content
        else {
            return Err(invalid("expected revocation payload"));
        };
        let old_args = old
            .transaction
            .raw()
            .outputs()
            .get(0)
            .ok_or_else(|| invalid("missing old output"))?
            .lock()
            .args()
            .raw_data();
        if commitment_lock_script_args.as_slice() != &old_args[..36] {
            return Err(invalid("revocation does not bind the old commitment"));
        }
        let expected_lock = if receiving {
            &binding.local_shutdown_script
        } else {
            &self.parameters.remote_shutdown_script
        };
        let capacity: u64 = self.funding_output.capacity().unpack();
        if output.lock() != *expected_lock
            || output.type_() != self.funding_output.type_()
            || output.capacity() != (capacity - self.parameters.commitment_fee).pack()
            || output_data.as_slice() != self.funding_data.pack().as_slice()
        {
            return Err(invalid(
                "incorrect revocation destination, asset, amount or fee",
            ));
        }
        if let Some(previous) = self
            .lifecycle
            .revocations
            .iter()
            .find(|r| r.context.revoked == context.revoked)
        {
            if previous.context != *context
                || previous
                    .content
                    .content_hash(&previous.content.canonical_bytes())
                    .map_err(|e| invalid(&e))?
                    != content
                        .content_hash(&content.canonical_bytes())
                        .map_err(|e| invalid(&e))?
            {
                return Err(invalid("conflicting revocation retry"));
            }
            return Ok(());
        }
        if self.lanes[usize::from(receiving)]
            .as_ref()
            .is_none_or(|lane| lane.version != context.replacement.version)
        {
            return Err(invalid(
                "revocation replacement is not the latest signed state",
            ));
        }
        let expected_old = self
            .lifecycle
            .revocations
            .iter()
            .filter(|r| r.context.revoked.for_remote == receiving)
            .map(|r| r.context.revoked.version)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("revocation version overflow"))?;
        if context.revoked.version != expected_old {
            return Err(invalid("revocation rollback or gap"));
        }
        self.lifecycle.revocations.push(RevocationRecord {
            context: context.clone(),
            content: content.clone(),
            signature: None,
        });
        Ok(())
    }

    pub(crate) fn release_preimage(
        &mut self,
        hash: Hash256,
        preimage: Hash256,
        now_ms: u64,
    ) -> Result<crate::CommitmentReference, SignerError> {
        let payment = self
            .payments
            .iter()
            .find(|p| p.terms.payment_hash == hash)
            .ok_or_else(|| invalid("unknown invoice"))?;
        if !payment.terms.inbound
            || payment.resolution == Resolution::Cancel
            || payment.terms.hash_algorithm.hash(preimage.as_ref()) != *hash.as_ref()
        {
            return Err(invalid("incorrect preimage release authorization"));
        }
        let record = self
            .lifecycle
            .commitments
            .iter()
            .rev()
            .find(|r| !r.reference.for_remote && r.complete_signature.is_some())
            .ok_or_else(|| invalid("no recoverable local commitment"))?;
        if self
            .lifecycle
            .revocations
            .iter()
            .any(|r| r.context.revoked == record.reference)
        {
            return Err(invalid("protected commitment was revoked"));
        }
        let tlc = record
            .settlement
            .data
            .tlcs
            .iter()
            .find(|t| t.payment_hash == hash)
            .ok_or_else(|| invalid("invoice is not protected by latest recoverable commitment"))?;
        if tlc.tlc_id.is_received()
            || tlc.payment_amount != payment.terms.amount
            || tlc.hash_algorithm != payment.terms.hash_algorithm
        {
            return Err(invalid("protected TLC differs from invoice"));
        }
        let deadline = now_ms
            .checked_add(self.parameters.safety_margin_ms()?)
            .ok_or_else(|| invalid("deadline overflow"))?;
        if (tlc.expiry / 1000) * 1000 <= deadline {
            return Err(invalid("preimage release recovery window expired"));
        }
        // A prior remote state without this incoming TLC cannot confiscate our own funds:
        // receipt is protected by our complete local commitment. Do not require an ack
        // whose protocol delivery may itself depend on fulfillment.
        let reference = record.reference.clone();
        if !self.lifecycle.released.contains(&hash) {
            self.lifecycle.released.push(hash);
        }
        // Once permission is returned, the secret may be public. Never authorize a refund.
        self.resolve(hash, Some(preimage))?;
        Ok(reference)
    }

    pub(crate) fn validate_close(
        &self,
        content: &ChannelSigningContent,
        binding: &ChannelBinding,
    ) -> Result<(), SignerError> {
        let terms = self
            .lifecycle
            .close
            .as_ref()
            .ok_or_else(|| invalid("close fees not approved"))?;
        if terms.local_fee_share.checked_add(terms.remote_fee_share) != Some(terms.fee) {
            return Err(invalid("invalid close fee allocation"));
        }
        let local = self.lanes[0]
            .as_ref()
            .ok_or_else(|| invalid("missing local commitment"))?;
        let remote = self.lanes[1]
            .as_ref()
            .ok_or_else(|| invalid("missing remote commitment"))?;
        if !local.tlcs.is_empty()
            || !remote.tlcs.is_empty()
            || local.local_amount != remote.local_amount
            || local.remote_amount != remote.remote_amount
        {
            return Err(invalid(
                "close requires matching balances and no unresolved TLCs",
            ));
        }
        let ChannelSigningContent::Musig2(m) = content else {
            return Err(invalid("expected close"));
        };
        let Musig2SignableContent::CooperativeCloseTransaction(tx) = &m.content else {
            return Err(invalid("expected close transaction"));
        };
        let raw = tx.raw();
        if raw.version() != 0u32.pack()
            || raw.inputs().len() != 1
            || raw.outputs().len() != 2
            || raw.outputs_data().len() != 2
            || !raw.header_deps().is_empty()
            || !tx.witnesses().is_empty()
        {
            return Err(invalid("unexpected close transaction structure"));
        }
        let input = raw
            .inputs()
            .get(0)
            .ok_or_else(|| invalid("missing close input"))?;
        if input.previous_output() != binding.funding_outpoint || input.since() != 0u64.pack() {
            return Err(invalid("wrong close input"));
        }
        let udt = self.funding_output.type_().to_opt().is_some();
        // Both parties may use the same shutdown script. Match the complete output,
        // not just its destination, so either deterministic key ordering is valid.
        let mut expected = Vec::with_capacity(2);
        for (amount, fee, reserve, script) in [
            (
                local.local_amount,
                terms.local_fee_share,
                self.parameters.local_reserved_ckb_amount,
                binding.local_shutdown_script.clone(),
            ),
            (
                local.remote_amount,
                terms.remote_fee_share,
                self.parameters.remote_reserved_ckb_amount,
                self.parameters.remote_shutdown_script.clone(),
            ),
        ] {
            let capacity = if udt {
                reserve
            } else {
                u64::try_from(amount).map_err(|_| invalid("close capacity overflow"))?
            };
            let capacity = capacity
                .checked_sub(fee)
                .ok_or_else(|| invalid("close fee exceeds balance"))?;
            let output = CellOutput::new_builder()
                .capacity(capacity)
                .lock(script)
                .type_(self.funding_output.type_())
                .build();
            let data = if udt {
                amount.to_le_bytes().to_vec()
            } else {
                vec![]
            };
            expected.push((output, data));
        }
        for (i, output) in raw.outputs().into_iter().enumerate() {
            let data = raw
                .outputs_data()
                .get(i)
                .ok_or_else(|| invalid("missing close data"))?
                .raw_data();
            let index = expected
                .iter()
                .position(|(approved, bytes)| {
                    *approved == output && bytes.as_slice() == data.as_ref()
                })
                .ok_or_else(|| invalid("incorrect close destination, amount, asset or fee"))?;
            expected.remove(index);
        }
        Ok(())
    }
}
