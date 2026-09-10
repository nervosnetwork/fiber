//! Node-side watchtower signer.
//!
//! Parallel to [`crate::fiber::channel_signer::ChannelSigner`]: both are
//! `Local(...) | External` enums with a request / apply_submitted API so local
//! and external signing share one call path without a shared signer actor.

use fiber_types::{
    compute_tx_message, ChannelData, Hash256, OnchainSigningContent, Privkey, Pubkey,
};
use molecule::prelude::Entity;
use secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, SECP256K1,
};

/// Outcome of requesting an on-chain watchtower signature.
#[derive(Clone, Debug)]
pub enum WatchtowerSignOutcome {
    /// Local key material produced a recoverable ECDSA signature.
    Ready([u8; 65]),
    /// No local settlement key; persist `AwaitingSignature` and wait for submit.
    AwaitingExternal {
        request_id: Hash256,
        content: OnchainSigningContent,
    },
}

/// Host-side signing entry for one watched channel's on-chain spends.
///
/// Morphologically aligned with [`crate::fiber::channel_signer::ChannelSigner`]:
/// `Local` binds settlement key material; `External` awaits RPC submit.
#[derive(Clone, Debug)]
pub enum WatchtowerSigner {
    /// Watchtower holds the settlement secret and signs locally.
    Local(Privkey),
    /// Client owns settlement keys; tower only awaits external submits.
    External,
}

impl WatchtowerSigner {
    /// Watchtower holds the settlement secret and signs locally.
    pub fn local(settlement_key: Privkey) -> Self {
        Self::Local(settlement_key)
    }

    /// Client owns settlement keys; tower only awaits external submits.
    pub fn external() -> Self {
        Self::External
    }

    /// Bind from persisted watch-channel data.
    pub fn from_channel_data(data: &ChannelData) -> Self {
        match data.local_settlement_key.clone() {
            Some(key) => Self::Local(key),
            None => Self::External,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Bound settlement key when local; `None` when external.
    pub fn settlement_key(&self) -> Option<&Privkey> {
        match self {
            Self::Local(key) => Some(key),
            Self::External => None,
        }
    }

    /// Request an on-chain signature for `content` using the bound settlement key.
    pub fn request_onchain(
        &self,
        content: OnchainSigningContent,
    ) -> Result<WatchtowerSignOutcome, String> {
        self.request_onchain_with_key(content, self.settlement_key().cloned())
    }

    /// Request an on-chain signature, optionally overriding the unlock key
    /// (e.g. a derived TLC key for the current commitment).
    ///
    /// When `unlock_key` is `Some`, signs immediately regardless of whether
    /// this entry is [`Self::Local`] or [`Self::External`] (the settle path
    /// may already have derived a TLC key). When `unlock_key` is `None`,
    /// returns [`WatchtowerSignOutcome::AwaitingExternal`].
    pub fn request_onchain_with_key(
        &self,
        content: OnchainSigningContent,
        unlock_key: Option<Privkey>,
    ) -> Result<WatchtowerSignOutcome, String> {
        if let Some(key) = unlock_key {
            let signature = sign_onchain_request(&key, &content)?;
            return Ok(WatchtowerSignOutcome::Ready(signature));
        }
        let request_id = Hash256::from(ckb_hash::blake2b_256(content.transaction.as_slice()));
        Ok(WatchtowerSignOutcome::AwaitingExternal {
            request_id,
            content,
        })
    }

    /// Verify an externally submitted signature against the expected pubkey and content.
    pub fn apply_submitted(
        expected_pubkey: &Pubkey,
        content: &OnchainSigningContent,
        signature: [u8; 65],
    ) -> Result<[u8; 65], String> {
        verify_onchain_signature(expected_pubkey, content, &signature)?;
        Ok(signature)
    }
}

/// Sign an on-chain CKB transaction using recoverable ECDSA.
pub fn sign_onchain_request(
    privkey: &Privkey,
    content: &OnchainSigningContent,
) -> Result<[u8; 65], String> {
    let message = compute_tx_message(&content.transaction);
    let secp_msg = Message::from_digest(message);
    let signature = SECP256K1.sign_ecdsa_recoverable(&secp_msg, &privkey.0);
    let (recov_id, data) = signature.serialize_compact();
    let mut signature_bytes = [0u8; 65];
    signature_bytes[0..64].copy_from_slice(&data[0..64]);
    signature_bytes[64] = i32::from(recov_id) as u8;
    Ok(signature_bytes)
}

/// Verify a 65-byte recoverable ECDSA signature against the expected public key and transaction.
pub fn verify_onchain_signature(
    expected_pubkey: &Pubkey,
    content: &OnchainSigningContent,
    signature_bytes: &[u8; 65],
) -> Result<(), String> {
    let message = compute_tx_message(&content.transaction);
    let secp_msg = Message::from_digest(message);
    let recov_id = RecoveryId::try_from(signature_bytes[64] as i32)
        .map_err(|e| format!("invalid recovery id: {e}"))?;
    let sig = RecoverableSignature::from_compact(&signature_bytes[0..64], recov_id)
        .map_err(|e| format!("invalid compact signature: {e}"))?;
    let recovered_pubkey = SECP256K1
        .recover_ecdsa(&secp_msg, &sig)
        .map_err(|e| format!("failed to recover pubkey: {e}"))?;
    if recovered_pubkey.serialize() != expected_pubkey.0 {
        return Err("recovered public key does not match expected public key".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ckb_types::core::TransactionBuilder;
    use fiber_types::OnchainKeyPurpose;

    use super::*;

    #[test]
    fn local_request_signs_and_verifies() {
        let privkey = Privkey::from(&[99u8; 32]);
        let pubkey = privkey.pubkey();
        let unsigned_tx = TransactionBuilder::default().build().data();
        let content = OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: unsigned_tx,
        };
        let signer = WatchtowerSigner::local(privkey);
        let outcome = signer.request_onchain(content.clone()).expect("sign ok");
        let WatchtowerSignOutcome::Ready(signature) = outcome else {
            panic!("expected Ready");
        };
        verify_onchain_signature(&pubkey, &content, &signature).expect("verify ok");
        let wrong = Privkey::from(&[100u8; 32]).pubkey();
        assert!(verify_onchain_signature(&wrong, &content, &signature).is_err());
    }

    #[test]
    fn external_request_awaits_submission() {
        let unsigned_tx = TransactionBuilder::default().build().data();
        let content = OnchainSigningContent {
            key_purpose: OnchainKeyPurpose::Settlement,
            transaction: unsigned_tx,
        };
        let outcome = WatchtowerSigner::external()
            .request_onchain(content)
            .expect("outcome ok");
        assert!(matches!(
            outcome,
            WatchtowerSignOutcome::AwaitingExternal { .. }
        ));
    }
}
