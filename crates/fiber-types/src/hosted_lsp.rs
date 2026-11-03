//! Shared identity and registration types for hosted Fiber LSP clients.

use std::{fmt, str::FromStr};

use secp256k1::{ecdsa::Signature, Message, SECP256K1};
use serde::{Deserialize, Serialize};

use crate::{blake2b_hash_with_salt, Pubkey};

/// Domain identifying the canonical tenant registration payload.
pub const TENANT_REGISTRY_PROTOCOL: &str = "fiber-hosted-lsp-tenant-registry/v2";

/// Domain separating hosted-LSP tenant identifiers from other hashes.
pub const TENANT_ID_DOMAIN: &[u8] = b"fiber-hosted-lsp-tenant-id/v1";

/// Maximum accepted age of a client-generated registration timestamp.
pub const TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS: u64 = 5 * 60 * 1000;

/// Maximum accepted future skew of a client-generated registration timestamp.
pub const TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS: u64 = 30 * 1000;

/// Stable hosted tenant identifier derived from a RootSigner public key.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TenantId(String);

impl TenantId {
    /// Construct and validate a tenant identifier.
    ///
    /// This constructor accepts the hosted LSP's legacy operator identifiers so
    /// existing records remain readable. New registrations must use
    /// [`Self::from_root_signer_pubkey`].
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 {
            return Err("tenant id must contain between 1 and 64 characters".to_string());
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "tenant id may only contain ASCII letters, digits, '-' and '_'".to_string(),
            );
        }
        Ok(Self(value))
    }

    /// Derive the canonical tenant identifier for a RootSigner identity.
    pub fn from_root_signer_pubkey(root_signer_pubkey: &Pubkey) -> Self {
        let digest = blake2b_hash_with_salt(&root_signer_pubkey.serialize(), TENANT_ID_DOMAIN);
        Self(hex::encode(digest))
    }

    /// Borrow the identifier as its canonical string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for TenantId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// Client-generated RootSigner proof used to register or refresh a hosted LSP tenant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TenantRegistryPayload {
    /// Public Fiber node identity of the hosted LSP.
    pub lsp_node_id: Pubkey,
    /// RootSigner identity that deterministically defines the tenant.
    pub root_signer_pubkey: Pubkey,
    /// Client-generated, single-use registration request identifier.
    pub request_id: [u8; 32],
    /// Client-generated Unix timestamp in milliseconds when the proof was signed.
    pub signed_at: u64,
}

impl TenantRegistryPayload {
    /// Construct a registration payload for one client-generated request.
    pub fn new(
        lsp_node_id: Pubkey,
        root_signer_pubkey: Pubkey,
        request_id: [u8; 32],
        signed_at: u64,
    ) -> Self {
        Self {
            lsp_node_id,
            root_signer_pubkey,
            request_id,
            signed_at,
        }
    }

    /// Return the fixed canonical binary encoding signed by the RootSigner.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(TENANT_REGISTRY_PROTOCOL.len() + 33 + 33 + 32 + 8);
        bytes.extend_from_slice(TENANT_REGISTRY_PROTOCOL.as_bytes());
        bytes.extend_from_slice(&self.lsp_node_id.serialize());
        bytes.extend_from_slice(&self.root_signer_pubkey.serialize());
        bytes.extend_from_slice(&self.request_id);
        bytes.extend_from_slice(&self.signed_at.to_be_bytes());
        bytes
    }

    /// Validate the signed timestamp against the verifier's current time.
    pub fn validate_signed_at(&self, now_ms: u64) -> Result<(), String> {
        if self.signed_at > now_ms {
            if self.signed_at - now_ms > TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS {
                return Err("tenant registration signed_at is too far in the future".to_string());
            }
        } else if now_ms - self.signed_at > TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS {
            return Err("tenant registration signed_at is too old".to_string());
        }
        Ok(())
    }

    /// Compute the canonical digest signed during tenant registration.
    pub fn digest(&self) -> [u8; 32] {
        blake2b_hash_with_salt(&self.canonical_bytes(), &[])
    }

    /// Verify a compact ECDSA signature against the RootSigner in this payload.
    pub fn verify_signature(&self, signature: &TenantRegistrySignature) -> Result<(), String> {
        let signature = Signature::from_compact(&signature.0).map_err(|error| error.to_string())?;
        let public_key = self.root_signer_pubkey.into();
        SECP256K1
            .verify_ecdsa(
                &Message::from_digest(self.digest()),
                &signature,
                &public_key,
            )
            .map_err(|error| error.to_string())
    }
}

/// Compact ECDSA signature over a [`TenantRegistryPayload`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantRegistrySignature(pub [u8; 64]);

impl TenantRegistrySignature {
    /// Parse and validate a compact secp256k1 ECDSA signature.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, secp256k1::Error> {
        let signature = Signature::from_compact(bytes)?;
        Ok(Self(signature.serialize_compact()))
    }

    /// Return the compact 64-byte signature.
    pub fn serialize(&self) -> [u8; 64] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_pubkey() -> Pubkey {
        secp256k1::SecretKey::from_byte_array(&[42; 32])
            .unwrap()
            .public_key(SECP256K1)
            .into()
    }

    fn lsp_pubkey() -> Pubkey {
        secp256k1::SecretKey::from_byte_array(&[7; 32])
            .unwrap()
            .public_key(SECP256K1)
            .into()
    }

    #[test]
    fn tenant_id_derivation_has_a_fixed_vector() {
        assert_eq!(
            TenantId::from_root_signer_pubkey(&root_pubkey()).as_str(),
            "5feabe8ac4480f8979ecbf3fb43618dd0d3472eb140235a35ee643d4346b862f"
        );
    }

    #[test]
    fn registry_payload_encoding_and_digest_have_fixed_vectors() {
        let payload =
            TenantRegistryPayload::new(lsp_pubkey(), root_pubkey(), [3; 32], 1_700_000_000_123);
        assert_eq!(
            hex::encode(payload.canonical_bytes()),
            "66696265722d686f737465642d6c73702d74656e616e742d72656769737472792f763202989c0b76cb563971fdc9bef31ec06c3560f3249d6ee9e5d83c57625596e05f6f035be5e9478209674a96e60f1f037f6176540fd001fa1d64694770c56a7709c42c03030303030303030303030303030303030303030303030303030303030303030000018bcfe5687b"
        );
        assert_eq!(
            hex::encode(payload.digest()),
            "0119b795a736bcbb4d6d9f255500d0584f8702133e6d61c04bfc0a42081e3375"
        );
    }

    #[test]
    fn signed_at_validation_handles_boundaries_and_u64_extremes() {
        let mut payload =
            TenantRegistryPayload::new(lsp_pubkey(), root_pubkey(), [3; 32], 1_000_000);
        payload
            .validate_signed_at(1_000_000 + TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS)
            .unwrap();
        payload
            .validate_signed_at(1_000_000 - TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS)
            .unwrap();
        assert!(payload
            .validate_signed_at(1_000_000 + TENANT_REGISTRY_MAX_SIGNED_AT_AGE_MS + 1)
            .is_err());
        assert!(payload
            .validate_signed_at(1_000_000 - TENANT_REGISTRY_MAX_SIGNED_AT_FUTURE_SKEW_MS - 1)
            .is_err());

        payload.signed_at = u64::MAX;
        assert!(payload.validate_signed_at(0).is_err());
        payload.signed_at = 0;
        assert!(payload.validate_signed_at(u64::MAX).is_err());
    }
}
