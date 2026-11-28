//! Local signer implementation for CKB transactions.
//!
//! This module provides a minimal encapsulation of the secret key used for signing
//! CKB transactions. It caches derived values (public key, pubkey hash) to avoid
//! repeated computation.

use ckb_sdk::{
    constants::SIGHASH_TYPE_HASH,
    traits::{SecpCkbRawKeySigner, TransactionDependencyProvider},
    tx_builder::unlock_tx_async,
    unlock::{ScriptUnlocker, SecpSighashUnlocker, UnlockError},
    util::blake160,
    ScriptId,
};
use ckb_types::packed;
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};
use std::collections::{HashMap, HashSet};

use super::{config::new_default_transaction_dependency_provider, FundingError, FundingTx};

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
        let pubkey = secret_key.public_key(SECP256K1);
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
        let msg = Message::from_digest(*message);
        let signature = SECP256K1.sign_ecdsa_recoverable(&msg, &self.secret_key);
        let (recov_id, data) = signature.serialize_compact();

        let mut signature_bytes = [0u8; 65];
        signature_bytes[0..64].copy_from_slice(&data[0..64]);
        signature_bytes[64] = i32::from(recov_id) as u8;
        signature_bytes
    }

    /// Sign a funding transaction using the signer's secret key.
    ///
    /// This method encapsulates the signing logic to avoid exposing the secret key
    /// directly to callers. It uses CKB SDK's `unlock_tx_async` internally.
    /// When `allowed_local_inputs` is provided, every input unlocked by
    /// `local_lock_script` must belong to that locally-funded set so that peer
    /// inputs sharing our lock script cannot be unlocked via the same script group.
    pub async fn sign_funding_tx(
        &self,
        tx: FundingTx,
        rpc_url: String,
        local_lock_script: &packed::Script,
        allowed_local_inputs: Option<&HashSet<packed::OutPoint>>,
    ) -> Result<FundingTx, FundingError> {
        let tx_dep_provider = new_default_transaction_dependency_provider(&rpc_url, 10).await;
        self.sign_funding_tx_with_provider(
            tx,
            &tx_dep_provider,
            local_lock_script,
            allowed_local_inputs,
        )
        .await
    }

    /// Sign a funding transaction against an explicit dependency provider.
    pub async fn sign_funding_tx_with_provider<P: TransactionDependencyProvider>(
        &self,
        mut tx: FundingTx,
        tx_dep_provider: &P,
        local_lock_script: &packed::Script,
        allowed_local_inputs: Option<&HashSet<packed::OutPoint>>,
    ) -> Result<FundingTx, FundingError> {
        let inner_tx = tx.take().ok_or(FundingError::AbsentTx)?;
        if let Some(allowed) = allowed_local_inputs {
            for (input_index, input) in inner_tx.inputs().into_iter().enumerate() {
                let out_point = input.previous_output();
                let cell_output = tx_dep_provider
                    .get_cell_async(&out_point)
                    .await
                    .map_err(UnlockError::TxDep)?;
                if &cell_output.lock() == local_lock_script && !allowed.contains(&out_point) {
                    return Err(FundingError::PeerInputUsesOurFundingLock { input_index });
                }
            }
        }
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![self.secret_key]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );

        let (signed_tx, _) = unlock_tx_async(inner_tx, tx_dep_provider, &unlockers).await?;
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
        let keypair = secp256k1::Keypair::from_secret_key(SECP256K1, &self.secret_key);
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
mod tests;
