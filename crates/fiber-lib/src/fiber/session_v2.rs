//! Independent, durable signing sessions for commitment-owner V2 exchanges.

use fiber_types::{Hash256, NoncePurposeV2, Privkey, Pubkey, SigningNonceV2};
use musig2::{
    AggNonce, BinaryEncoding, KeyAggContext, PartialSignature, SecNonce, SecNonceBuilder,
};

pub(super) const SESSION_MARKER: u32 = 0x56320001;

// Opening parameter adapters reuse the existing parameter validation, never V1
// signing state. The placeholder revocation field is discarded at construction.
pub(super) fn open_parameters(m: super::types::OpenChannelV2) -> super::types::OpenChannel {
    super::types::OpenChannel {
        chain_hash: m.chain_hash,
        channel_id: m.channel_id,
        funding_udt_type_script: m.funding_udt_type_script,
        funding_amount: m.funding_amount,
        shutdown_script: m.shutdown_script,
        reserved_ckb_amount: m.reserved_ckb_amount,
        funding_fee_rate: m.funding_fee_rate,
        commitment_fee_rate: m.commitment_fee_rate,
        commitment_delay_epoch: m.commitment_delay_epoch,
        max_tlc_value_in_flight: m.max_tlc_value_in_flight,
        max_tlc_number_in_flight: m.max_tlc_number_in_flight,
        channel_flags: m.channel_flags,
        first_per_commitment_point: m.first_per_commitment_point,
        second_per_commitment_point: m.second_per_commitment_point,
        funding_pubkey: m.funding_pubkey,
        tlc_basepoint: m.tlc_basepoint,
        next_revocation_nonce: m.initial_commitment_nonce.clone(),
        next_commitment_nonce: m.initial_commitment_nonce,
        channel_announcement_nonce: m.channel_announcement_nonce,
    }
}

pub(super) fn opening_message(
    m: super::types::OpenChannel,
    nonce: musig2::PubNonce,
) -> super::types::FiberMessage {
    super::types::FiberMessage::open_channel_v2(super::types::OpenChannelV2 {
        chain_hash: m.chain_hash,
        channel_id: m.channel_id,
        funding_udt_type_script: m.funding_udt_type_script,
        funding_amount: m.funding_amount,
        shutdown_script: m.shutdown_script,
        reserved_ckb_amount: m.reserved_ckb_amount,
        funding_fee_rate: m.funding_fee_rate,
        commitment_fee_rate: m.commitment_fee_rate,
        commitment_delay_epoch: m.commitment_delay_epoch,
        max_tlc_value_in_flight: m.max_tlc_value_in_flight,
        max_tlc_number_in_flight: m.max_tlc_number_in_flight,
        channel_flags: m.channel_flags,
        first_per_commitment_point: m.first_per_commitment_point,
        second_per_commitment_point: m.second_per_commitment_point,
        funding_pubkey: m.funding_pubkey,
        tlc_basepoint: m.tlc_basepoint,
        initial_commitment_nonce: nonce,
        channel_features: 1,
        channel_announcement_nonce: m.channel_announcement_nonce,
    })
}

pub(super) fn accept_parameters(m: super::types::AcceptChannelV2) -> super::types::AcceptChannel {
    super::types::AcceptChannel {
        channel_id: m.channel_id,
        funding_amount: m.funding_amount,
        shutdown_script: m.shutdown_script,
        reserved_ckb_amount: m.reserved_ckb_amount,
        max_tlc_value_in_flight: m.max_tlc_value_in_flight,
        max_tlc_number_in_flight: m.max_tlc_number_in_flight,
        first_per_commitment_point: m.first_per_commitment_point,
        second_per_commitment_point: m.second_per_commitment_point,
        funding_pubkey: m.funding_pubkey,
        tlc_basepoint: m.tlc_basepoint,
        next_revocation_nonce: m.initial_commitment_nonce.clone(),
        next_commitment_nonce: m.initial_commitment_nonce,
        channel_announcement_nonce: m.channel_announcement_nonce,
    }
}

pub(super) fn accepting_message(
    m: super::types::AcceptChannel,
    nonce: musig2::PubNonce,
) -> super::types::FiberMessage {
    super::types::FiberMessage::accept_channel_v2(super::types::AcceptChannelV2 {
        channel_id: m.channel_id,
        funding_amount: m.funding_amount,
        shutdown_script: m.shutdown_script,
        reserved_ckb_amount: m.reserved_ckb_amount,
        max_tlc_value_in_flight: m.max_tlc_value_in_flight,
        max_tlc_number_in_flight: m.max_tlc_number_in_flight,
        first_per_commitment_point: m.first_per_commitment_point,
        second_per_commitment_point: m.second_per_commitment_point,
        funding_pubkey: m.funding_pubkey,
        tlc_basepoint: m.tlc_basepoint,
        initial_commitment_nonce: nonce,
        channel_features: 1,
        channel_announcement_nonce: m.channel_announcement_nonce,
    })
}

fn secret_nonce(record: &SigningNonceV2, key: &Privkey) -> SecNonce {
    fiber_types::channel_v2_validation::secret_nonce_v2(record, key)
}

#[cfg(test)]
pub(super) fn valid_nonce(record: &SigningNonceV2, key: &Privkey) -> bool {
    record.public_nonce == secret_nonce(record, key).public_nonce()
        && (record.signature.is_none() || record.context.is_some())
}

pub(super) fn allocate_nonce(
    channel_id: Hash256,
    owner: Pubkey,
    number: u64,
    purpose: NoncePurposeV2,
    key: &Privkey,
) -> SigningNonceV2 {
    let seed = rand::random::<[u8; 32]>();
    let mut record = SigningNonceV2 {
        seed,
        channel_id,
        owner,
        number,
        purpose,
        public_nonce: SecNonceBuilder::new(seed).build().public_nonce(),
        context: None,
        signature: None,
    };
    record.public_nonce = secret_nonce(&record, key).public_nonce();
    record
}

pub(super) fn guarded_sign(
    record: &mut SigningNonceV2,
    key: &Privkey,
    keys: [Pubkey; 2],
    aggregate: &KeyAggContext,
    nonce: &AggNonce,
    message: &[u8],
) -> Result<PartialSignature, String> {
    bind_context(record, keys, nonce, message)?;
    if let Some(signature) = record.signature {
        return Ok(signature);
    }
    if record.public_nonce != secret_nonce(record, key).public_nonce() {
        return Err("Invalid persisted V2 nonce seed/key".to_owned());
    }
    let signature = musig2::sign_partial(aggregate, key, secret_nonce(record, key), nonce, message)
        .map_err(|error| error.to_string())?;
    record.signature = Some(signature);
    Ok(signature)
}

pub(super) fn bind_context(
    record: &mut SigningNonceV2,
    keys: [Pubkey; 2],
    nonce: &AggNonce,
    message: &[u8],
) -> Result<(), String> {
    let context = [
        keys[0].serialize().as_slice(),
        keys[1].serialize().as_slice(),
        nonce.to_bytes().as_slice(),
        message,
    ]
    .concat();
    if let Some(previous) = &record.context {
        if previous != &context {
            return Err("Conflicting V2 signing context for consumed nonce".to_owned());
        }
        return Ok(());
    }
    record.context = Some(context);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiber_types::{Hash256, Privkey};
    use musig2::{AggNonce, KeyAggContext};

    #[test]
    fn test_v2_bound_unsigned_nonce_is_valid_recovery_intent() {
        let a = Privkey::from_slice(&[41; 32]);
        let b = Privkey::from_slice(&[42; 32]);
        let mut record = allocate_nonce(
            Hash256::from([17; 32]),
            a.pubkey(),
            2,
            NoncePurposeV2::Commitment,
            &a,
        );
        let peer = allocate_nonce(
            record.channel_id,
            a.pubkey(),
            2,
            NoncePurposeV2::Commitment,
            &b,
        );
        let nonce = AggNonce::sum([record.public_nonce.clone(), peer.public_nonce]);
        bind_context(&mut record, [a.pubkey(), b.pubkey()], &nonce, &[1; 32]).unwrap();
        assert!(record.signature.is_none());
        assert!(valid_nonce(&record, &a));
    }

    #[test]
    fn test_v2_session_aggregation_guard_and_independent_directions() {
        let a = Privkey::from_slice(&[41; 32]);
        let b = Privkey::from_slice(&[42; 32]);
        let channel = Hash256::from([17; 32]);
        let mut owner = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Commitment, &b);
        let mut signer = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Commitment, &a);
        let reverse = allocate_nonce(channel, a.pubkey(), 2, NoncePurposeV2::Commitment, &a);
        let revoke = allocate_nonce(channel, b.pubkey(), 2, NoncePurposeV2::Revocation, &a);
        assert_ne!(signer.public_nonce, reverse.public_nonce);
        assert_ne!(signer.public_nonce, revoke.public_nonce);
        let keys = [a.pubkey(), b.pubkey()];
        let agg = KeyAggContext::new(keys).unwrap();
        let nonce = AggNonce::sum([owner.public_nonce.clone(), signer.public_nonce.clone()]);
        let sa = guarded_sign(&mut signer, &a, keys, &agg, &nonce, b"commitment").unwrap();
        let sb = guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"commitment").unwrap();
        let _: musig2::CompactSignature =
            musig2::aggregate_partial_signatures(&agg, &nonce, [sa, sb], b"commitment").unwrap();
        // Local signing during aggregation must be cached just like a sent CS.
        assert_eq!(
            guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"commitment").unwrap(),
            sb
        );
        assert!(
            guarded_sign(&mut owner, &b, keys, &agg, &nonce, b"different transaction").is_err()
        );
        let changed_nonce =
            AggNonce::sum([owner.public_nonce.clone(), reverse.public_nonce.clone()]);
        assert!(guarded_sign(&mut owner, &b, keys, &agg, &changed_nonce, b"commitment").is_err());
        let serialized = bincode::serialize(&owner).unwrap();
        let mut restored = bincode::deserialize(&serialized).unwrap();
        assert_eq!(
            guarded_sign(&mut restored, &b, keys, &agg, &nonce, b"commitment").unwrap(),
            sb
        );
        assert!(guarded_sign(
            &mut restored,
            &b,
            keys,
            &agg,
            &nonce,
            b"different transaction"
        )
        .is_err());
    }
}
