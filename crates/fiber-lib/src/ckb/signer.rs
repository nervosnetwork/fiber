//! Local signer implementation for CKB transactions.
//!
//! This module provides a minimal encapsulation of the secret key used for signing
//! CKB transactions. It caches derived values (public key, pubkey hash) to avoid
//! repeated computation.

use ckb_sdk::{
    constants::SIGHASH_TYPE_HASH,
    traits::{DefaultTransactionDependencyProvider, SecpCkbRawKeySigner},
    tx_builder::unlock_tx_async,
    unlock::{ScriptUnlocker, SecpSighashUnlocker},
    util::blake160,
    ScriptId,
};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use std::collections::HashMap;

use super::{FundingError, FundingTx};

/// A local signer that holds a secret key and provides signing capabilities.
///
/// This is a minimal encapsulation that:
/// - Caches derived values (pubkey, pubkey_hash) to avoid repeated computation
/// - Provides a consistent interface for signing operations
/// - Does not introduce traits, async, or RPC dependencies
#[derive(Clone)]
pub struct LocalSigner {
    secret_key: SecretKey,
    pubkey: PublicKey,
    /// blake160(pubkey.serialize()) - used for lock script args
    pubkey_hash: [u8; 20],
}

impl LocalSigner {
    /// Create a new LocalSigner from a secret key.
    ///
    /// This computes and caches the public key and its blake160 hash.
    pub fn new(secret_key: SecretKey) -> Self {
        let secp = Secp256k1::new();
        let pubkey = secret_key.public_key(&secp);
        let pubkey_hash = blake160(pubkey.serialize().as_ref()).0;
        Self {
            secret_key,
            pubkey,
            pubkey_hash,
        }
    }

    /// Get the public key.
    pub fn pubkey(&self) -> &PublicKey {
        &self.pubkey
    }

    /// Get the blake160 hash of the public key (20 bytes).
    ///
    /// This is commonly used as lock script args for secp256k1-sighash lock.
    pub fn pubkey_hash(&self) -> &[u8; 20] {
        &self.pubkey_hash
    }

    /// Sign a 32-byte message and return a 65-byte recoverable signature.
    ///
    /// The signature format is: [r (32 bytes) | s (32 bytes) | recovery_id (1 byte)]
    ///
    /// This is the low-level signing API. Callers are responsible for computing
    /// the message (e.g., CKB sighash or Fiber's compute_tx_message).
    pub fn sign_recoverable(&self, message: &[u8; 32]) -> [u8; 65] {
        let secp = Secp256k1::new();
        let msg = Message::from_digest(*message);
        let signature = secp.sign_ecdsa_recoverable(&msg, &self.secret_key);
        let (recov_id, data) = signature.serialize_compact();

        let mut signature_bytes = [0u8; 65];
        signature_bytes[0..64].copy_from_slice(&data[0..64]);
        signature_bytes[64] = recov_id.to_i32() as u8;
        signature_bytes
    }

    /// Sign a funding transaction using the signer's secret key.
    ///
    /// This method encapsulates the signing logic to avoid exposing the secret key
    /// directly to callers. It uses CKB SDK's `unlock_tx_async` internally.
    pub async fn sign_funding_tx(
        &self,
        mut tx: FundingTx,
        rpc_url: String,
    ) -> Result<FundingTx, FundingError> {
        // Convert between different versions of secp256k1.
        // This app requires 0.28 because of:
        // ```
        // #[derive(Copy, Clone, PartialOrd, Ord, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
        // pub struct Signature(pub Secp256k1Signature);
        // ```
        //
        // However, ckb-sdk-rust still uses 0.30 .
        //
        // It's complex to use map_err and return an error as well because secp256k1 used by ckb sdk is not public.
        // Expect is OK here since the secret key is valid and can be parsed in both versions.
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![std::str::FromStr::from_str(
            hex::encode(self.secret_key.as_ref()).as_ref(),
        )
        .expect("convert secret key between different secp256k1 versions")]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );
        let inner_tx = tx.take().ok_or(FundingError::AbsentTx)?;
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&rpc_url, 10);

        let (signed_tx, _) = unlock_tx_async(inner_tx, &tx_dep_provider, &unlockers).await?;
        tx.update_for_self(signed_tx);
        Ok(tx)
    }

    /// Get the secret key (test only).
    ///
    /// This method is only available in test builds for test setup purposes.
    /// Production code should use higher-level APIs like `sign_recoverable()`.
    #[cfg(test)]
    pub(crate) fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    /// Get the X-only public key.
    ///
    /// This is useful for creating channel announcements and other protocol messages.
    pub fn x_only_pub_key(&self) -> secp256k1::XOnlyPublicKey {
        let secp = Secp256k1::new();
        let keypair = secp256k1::Keypair::from_secret_key(&secp, &self.secret_key);
        secp256k1::XOnlyPublicKey::from_keypair(&keypair).0
    }
}

impl std::fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSigner")
            .field("pubkey", &self.pubkey)
            .field("pubkey_hash", &hex::encode(self.pubkey_hash))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_signer_creation() {
        // Use a fixed test secret key
        let secret_key_bytes = [1u8; 32];
        let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();

        let signer = LocalSigner::new(secret_key);

        // Verify pubkey is derived correctly
        let secp = Secp256k1::new();
        let expected_pubkey = secret_key.public_key(&secp);
        assert_eq!(signer.pubkey(), &expected_pubkey);

        // Verify pubkey_hash is 20 bytes
        assert_eq!(signer.pubkey_hash().len(), 20);
    }

    #[test]
    fn test_sign_recoverable() {
        let secret_key_bytes = [42u8; 32];
        let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();
        let signer = LocalSigner::new(secret_key);

        let message = [0xab; 32];
        let signature = signer.sign_recoverable(&message);

        // Signature should be 65 bytes
        assert_eq!(signature.len(), 65);

        // Verify the signature is deterministic
        let signature2 = signer.sign_recoverable(&message);
        assert_eq!(signature, signature2);
    }

    #[test]
    fn test_signature_matches_direct_signing() {
        // Verify that LocalSigner produces the same signature as direct secp256k1 signing
        let secret_key_bytes = [99u8; 32];
        let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();
        let signer = LocalSigner::new(secret_key);

        let message = [0x12; 32];

        // Sign using LocalSigner
        let signer_sig = signer.sign_recoverable(&message);

        // Sign directly using secp256k1
        let secp = Secp256k1::new();
        let msg = Message::from_digest(message);
        let direct_sig = secp.sign_ecdsa_recoverable(&msg, &secret_key);
        let (recov_id, data) = direct_sig.serialize_compact();
        let mut direct_sig_bytes = [0u8; 65];
        direct_sig_bytes[0..64].copy_from_slice(&data[0..64]);
        direct_sig_bytes[64] = recov_id.to_i32() as u8;

        // They should be identical
        assert_eq!(signer_sig, direct_sig_bytes);
    }
}
