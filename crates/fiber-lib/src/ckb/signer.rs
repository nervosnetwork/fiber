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
mod tests {
    use super::*;

    #[test]
    fn test_local_signer_creation() {
        // Use a fixed test secret key
        let secret_key_bytes = [1u8; 32];
        let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();

        let signer = LocalSigner::new(secret_key);

        // Verify pubkey is derived correctly
        let expected_pubkey = secret_key.public_key(SECP256K1);
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
        let msg = Message::from_digest(message);
        let direct_sig = SECP256K1.sign_ecdsa_recoverable(&msg, &secret_key);
        let (recov_id, data) = direct_sig.serialize_compact();
        let mut direct_sig_bytes = [0u8; 65];
        direct_sig_bytes[0..64].copy_from_slice(&data[0..64]);
        direct_sig_bytes[64] = i32::from(recov_id) as u8;

        // They should be identical
        assert_eq!(signer_sig, direct_sig_bytes);
    }

    #[tokio::test]
    async fn test_sign_funding_tx_rejects_peer_input_using_local_lock() {
        use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
        use ckb_sdk::traits::OffchainTransactionDependencyProvider;
        use ckb_types::core::{Capacity, TransactionView};
        use ckb_types::packed::{Byte32, CellInput, CellOutput, OutPoint};
        use ckb_types::prelude::{Builder, Entity, Pack, Unpack};

        let secret_key = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let signer = LocalSigner::new(secret_key);
        let local_lock = get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
        let peer_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0x99u8; 20][..]);
        let cell_deps = get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
            .await
            .expect("secp256k1 cell deps");

        let local_outpoint = OutPoint::new(Byte32::from_slice(&[1u8; 32]).unwrap(), 0);
        let peer_outpoint = OutPoint::new(Byte32::from_slice(&[2u8; 32]).unwrap(), 0);

        let local_cell = CellOutput::new_builder()
            .capacity(Capacity::shannons(100_000_000_000).pack())
            .lock(local_lock.clone())
            .build();
        let peer_cell = CellOutput::new_builder()
            .capacity(Capacity::shannons(100_000_000_000).pack())
            .lock(peer_lock)
            .build();

        // Case 1: local input + peer input with peer's own lock -> signing succeeds.
        let mut provider = OffchainTransactionDependencyProvider::default();
        provider.cells.insert(
            (local_outpoint.tx_hash().unpack(), 0),
            (local_cell.clone(), ckb_types::bytes::Bytes::new()),
        );
        provider.cells.insert(
            (peer_outpoint.tx_hash().unpack(), 0),
            (peer_cell, ckb_types::bytes::Bytes::new()),
        );
        let tx = TransactionView::new_advanced_builder()
            .cell_deps(cell_deps.clone())
            .input(CellInput::new(local_outpoint.clone(), 0))
            .input(CellInput::new(peer_outpoint.clone(), 0))
            .output(local_cell.clone())
            .output_data(ckb_types::packed::Bytes::default())
            .witnesses(vec![
                ckb_types::packed::Bytes::default(),
                ckb_types::packed::Bytes::default(),
            ])
            .build();
        let allowed = HashSet::from([local_outpoint.clone()]);
        let signed = signer
            .sign_funding_tx_with_provider(tx.into(), &provider, &local_lock, Some(&allowed))
            .await
            .expect("local input + distinct peer lock should sign");
        assert!(!signed.into_inner().unwrap().witnesses().is_empty());

        // Case 2: peer injects an input with local_lock alongside local input (same script group).
        provider.cells.insert(
            (peer_outpoint.tx_hash().unpack(), 0),
            (local_cell.clone(), ckb_types::bytes::Bytes::new()),
        );
        let malicious_tx = TransactionView::new_advanced_builder()
            .cell_deps(cell_deps.clone())
            .input(CellInput::new(local_outpoint.clone(), 0))
            .input(CellInput::new(peer_outpoint.clone(), 0))
            .output(local_cell.clone())
            .output_data(ckb_types::packed::Bytes::default())
            .witnesses(vec![
                ckb_types::packed::Bytes::default(),
                ckb_types::packed::Bytes::default(),
            ])
            .build();
        let err = signer
            .sign_funding_tx_with_provider(
                malicious_tx.clone().into(),
                &provider,
                &local_lock,
                Some(&allowed),
            )
            .await
            .expect_err("peer input using local lock must be rejected before signing");
        assert!(
            matches!(
                err,
                FundingError::PeerInputUsesOurFundingLock { input_index: 1 }
            ),
            "expected PeerInputUsesOurFundingLock at index 1, got {err:?}"
        );

        // Case 3: node contributed 0 inputs (allowed set is empty), peer injected local_lock input.
        let zero_local_tx = TransactionView::new_advanced_builder()
            .cell_deps(cell_deps)
            .input(CellInput::new(peer_outpoint, 0))
            .output(local_cell)
            .output_data(ckb_types::packed::Bytes::default())
            .witnesses(vec![ckb_types::packed::Bytes::default()])
            .build();
        let empty_allowed = HashSet::new();
        let err = signer
            .sign_funding_tx_with_provider(
                zero_local_tx.into(),
                &provider,
                &local_lock,
                Some(&empty_allowed),
            )
            .await
            .expect_err("peer input using local lock must be rejected when local funded 0 inputs");
        assert!(
            matches!(
                err,
                FundingError::PeerInputUsesOurFundingLock { input_index: 0 }
            ),
            "expected PeerInputUsesOurFundingLock at index 0, got {err:?}"
        );
    }
}
