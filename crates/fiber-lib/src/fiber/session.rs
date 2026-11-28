//! Independent, durable signing sessions for commitment-owner V2 exchanges.

use fiber_types::channel_v2_validation::secret_nonce_v2_parts;
use fiber_types::{
    ChannelSessionV2, Hash256, NoncePurposeV2, Privkey, Pubkey, SigningNonceV2,
    INITIAL_COMMITMENT_NUMBER,
};
use musig2::{AggNonce, BinaryEncoding, KeyAggContext, PartialSignature, PubNonce, SecNonce};

/// Allocate the initial owner-bound signing session before constructing a channel.
pub(crate) fn new_session(
    channel_id: Hash256,
    key: &Privkey,
    remote_nonce: Option<PubNonce>,
) -> ChannelSessionV2 {
    new_public_session(
        allocate_nonce(
            channel_id,
            key.pubkey(),
            INITIAL_COMMITMENT_NUMBER + 1,
            NoncePurposeV2::Commitment,
            key,
        ),
        remote_nonce,
    )
}

/// Construct a public-only session without any host-side channel secret.
pub(crate) fn new_public_session(
    own: SigningNonceV2,
    remote_nonce: Option<PubNonce>,
) -> ChannelSessionV2 {
    ChannelSessionV2 {
        own,
        own_retired: Vec::new(),
        bootstrap_remote_nonce: remote_nonce.clone(),
        remote_ready: None,
        remote_nonce,
        remote_number: INITIAL_COMMITMENT_NUMBER + 1,
        outgoing: None,
        incoming: None,
        pending_incoming: None,
        pending_ack: None,
        last_ack: None,
        revocation_effect: None,
        bootstrap_settlement: None,
        remove_effects: Vec::new(),
        closing: None,
        superseded_settlements: vec![],
    }
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
    let public_nonce =
        secret_nonce_v2_parts(seed, channel_id, owner, number, purpose, key).public_nonce();
    SigningNonceV2 {
        seed,
        channel_id,
        owner,
        number,
        purpose,
        public_nonce,
        context: None,
        signature: None,
    }
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
    let secnonce = secret_nonce(record, key);
    if record.public_nonce != secnonce.public_nonce() {
        return Err("Invalid persisted V2 nonce seed/key".to_owned());
    }
    let signature = musig2::sign_partial(aggregate, key, secnonce, nonce, message)
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
mod tests;
