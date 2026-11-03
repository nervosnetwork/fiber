use std::fmt;

use secp256k1::{PublicKey, SecretKey, SECP256K1};
use zeroize::Zeroizing;

use crate::SignerError;

/// Long-lived secp256k1 identity key owned by the signer.
///
/// Channel keys are derived from this secret through a separate domain. The
/// root key itself is never used as a Fiber channel key.
pub struct RootKey {
    secret: Zeroizing<[u8; 32]>,
}

impl RootKey {
    /// Generate a valid secp256k1 root key using the platform CSPRNG.
    pub fn generate() -> Result<Self, SignerError> {
        loop {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|error| SignerError::Random(error.to_string()))?;
            if let Ok(mut key) = SecretKey::from_byte_array(&bytes) {
                key.non_secure_erase();
                return Ok(Self {
                    secret: Zeroizing::new(bytes),
                });
            }
        }
    }

    /// Import an existing 32-byte secp256k1 root secret.
    pub fn import(secret: [u8; 32]) -> Result<Self, SignerError> {
        let mut key =
            SecretKey::from_byte_array(&secret).map_err(|_| SignerError::InvalidRootKey)?;
        key.non_secure_erase();
        Ok(Self {
            secret: Zeroizing::new(secret),
        })
    }

    /// Return the public identity key corresponding to this root key.
    pub fn public_key(&self) -> PublicKey {
        let mut secret_key = self.secret_key();
        let public_key = PublicKey::from_secret_key(SECP256K1, &secret_key);
        secret_key.non_secure_erase();
        public_key
    }

    pub(crate) fn secret_bytes(&self) -> &[u8; 32] {
        &self.secret
    }

    pub(crate) fn sign_tenant_registry_payload(
        &self,
        payload: &fiber_types::TenantRegistryPayload,
    ) -> fiber_types::TenantRegistrySignature {
        let mut secret_key = self.secret_key();
        let signature = SECP256K1.sign_ecdsa(
            &secp256k1::Message::from_digest(payload.digest()),
            &secret_key,
        );
        secret_key.non_secure_erase();
        fiber_types::TenantRegistrySignature(signature.serialize_compact())
    }

    fn secret_key(&self) -> SecretKey {
        SecretKey::from_byte_array(&self.secret).expect("RootKey validates at construction")
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootKey")
            .field("secret", &"[REDACTED]")
            .field("public_key", &self.public_key())
            .finish()
    }
}

/// Explicitly handled backup returned when the SDK generates a new root key.
pub struct RootKeyBackup {
    secret: Zeroizing<[u8; 32]>,
}

impl RootKeyBackup {
    pub(crate) fn new(secret: [u8; 32]) -> Self {
        Self {
            secret: Zeroizing::new(secret),
        }
    }

    /// Expose a copy of the secret for explicit backup or import.
    pub fn expose_secret(&self) -> [u8; 32] {
        *self.secret
    }
}

impl fmt::Debug for RootKeyBackup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RootKeyBackup").field(&"[REDACTED]").finish()
    }
}
