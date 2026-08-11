use bincode::{deserialize, serialize};
use ckb_hash::blake2b_256;
use fiber_store::backend::StorageBackend;
use serde::{Deserialize, Serialize};

use crate::fiber_types::{EcdsaSignature, Hash256, Privkey, Pubkey};
use crate::invoice::CkbInvoice;
use crate::store::Store;

use super::{HostedTenantRecord, TenantId};

const INVOICE_REGISTRATION_PREFIX: &[u8] = b"\xf1lsp/invoice/";
const LSP_INVOICE_HINT_DOMAIN: &[u8] = b"fiber-lsp-invoice-hint/v1";

/// Default time an LSP may wait for an offline tenant before dispatching.
pub const DEFAULT_LSP_BUFFER_DURATION_MS: u64 = 24 * 60 * 60 * 1_000;
/// Protocol cap for invoice-requested offline buffering.
pub const MAX_LSP_BUFFER_DURATION_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Fields protected by Public T's signature and distributed with an invoice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LspInvoiceHintPayload {
    pub version: u8,
    pub lsp_node_id: Pubkey,
    pub payment_hash: Hash256,
    /// Digest of the complete signed invoice, binding amount, asset and terms.
    pub invoice_digest: Hash256,
    pub buffer_duration_ms: u64,
    pub expires_at: u64,
}

/// Authenticated routing and buffering hint returned alongside a Fiber invoice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LspInvoiceHint {
    pub payload: LspInvoiceHintPayload,
    pub signature: EcdsaSignature,
}

impl LspInvoiceHint {
    fn digest(payload: &LspInvoiceHintPayload) -> Result<[u8; 32], String> {
        let encoded = serialize(payload).map_err(|error| error.to_string())?;
        Ok(blake2b_256(
            [LSP_INVOICE_HINT_DOMAIN, encoded.as_slice()].concat(),
        ))
    }

    pub fn sign(payload: LspInvoiceHintPayload, signer: &Privkey) -> Result<Self, String> {
        let signature = signer.sign(Self::digest(&payload)?);
        Ok(Self { payload, signature })
    }

    /// Verify signature, lifetime and protocol bounds at `now`.
    pub fn verify(&self, now: u64) -> Result<(), String> {
        if self.payload.version != 1 {
            return Err(format!(
                "unsupported LSP invoice hint version {}",
                self.payload.version
            ));
        }
        if self.payload.buffer_duration_ms > MAX_LSP_BUFFER_DURATION_MS {
            return Err("LSP invoice hint exceeds maximum buffer duration".to_string());
        }
        if now >= self.payload.expires_at {
            return Err("LSP invoice hint has expired".to_string());
        }
        let digest = Self::digest(&self.payload)?;
        if !self.signature.verify(&self.payload.lsp_node_id, &digest) {
            return Err("invalid LSP invoice hint signature".to_string());
        }
        Ok(())
    }

    /// The single public trampoline hop a wallet should use for this invoice.
    pub fn trampoline_hops(&self) -> [Pubkey; 1] {
        [self.payload.lsp_node_id]
    }

    /// Verify this hint and bind it to the exact signed invoice presented to a wallet.
    pub fn verify_for_invoice(&self, invoice: &CkbInvoice, now: u64) -> Result<(), String> {
        self.verify(now)?;
        if invoice.payment_hash() != &self.payload.payment_hash {
            return Err("LSP invoice hint payment hash does not match invoice".to_string());
        }
        if invoice_digest(invoice) != self.payload.invoice_digest {
            return Err("LSP invoice hint does not match signed invoice".to_string());
        }
        Ok(())
    }
}

fn invoice_digest(invoice: &CkbInvoice) -> Hash256 {
    blake2b_256(invoice.to_string().as_bytes()).into()
}

/// Persistent routing ownership for one invoice payment hash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LspInvoiceRegistration {
    pub tenant_id: TenantId,
    pub invoice: CkbInvoice,
    pub hint: LspInvoiceHint,
}

/// Persistence interface for registered hosted invoices.
pub trait LspInvoiceStore: Clone + Send + Sync + 'static {
    fn get_lsp_invoice(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspInvoiceRegistration>, String>;
    fn put_lsp_invoice(&self, registration: &LspInvoiceRegistration) -> Result<(), String>;
}

fn registration_key(payment_hash: &Hash256) -> Vec<u8> {
    [INVOICE_REGISTRATION_PREFIX, payment_hash.as_ref()].concat()
}

impl LspInvoiceStore for Store {
    fn get_lsp_invoice(
        &self,
        payment_hash: &Hash256,
    ) -> Result<Option<LspInvoiceRegistration>, String> {
        self.get(registration_key(payment_hash))
            .map(|bytes| deserialize(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn put_lsp_invoice(&self, registration: &LspInvoiceRegistration) -> Result<(), String> {
        let bytes = serialize(registration).map_err(|error| error.to_string())?;
        self.put(registration_key(registration.invoice.payment_hash()), bytes);
        Ok(())
    }
}

/// Validates and persists the mapping used by Public T when an incoming TLC
/// references a hosted tenant invoice.
#[derive(Clone)]
pub struct LspInvoiceRegistry<S> {
    store: S,
    max_buffer_duration_ms: u64,
}

impl<S: LspInvoiceStore> LspInvoiceRegistry<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            max_buffer_duration_ms: MAX_LSP_BUFFER_DURATION_MS,
        }
    }

    pub fn with_max_buffer_duration(store: S, max_buffer_duration_ms: u64) -> Self {
        Self {
            store,
            max_buffer_duration_ms: max_buffer_duration_ms.min(MAX_LSP_BUFFER_DURATION_MS),
        }
    }

    pub fn get(&self, payment_hash: &Hash256) -> Result<Option<LspInvoiceRegistration>, String> {
        self.store.get_lsp_invoice(payment_hash)
    }

    pub fn register(
        &self,
        tenant: &HostedTenantRecord,
        invoice: CkbInvoice,
        buffer_duration_ms: Option<u64>,
        lsp_node_id: Pubkey,
        signer: &Privkey,
    ) -> Result<LspInvoiceRegistration, String> {
        invoice
            .check_signature()
            .map_err(|error| format!("invalid hosted invoice signature: {error}"))?;
        if !invoice.is_signed() {
            return Err("hosted invoice must be signed by the tenant".to_string());
        }
        let payee = invoice
            .recover_payee_pub_key()
            .map(Pubkey::from)
            .map_err(|error| format!("failed to recover hosted invoice payee: {error}"))?;
        if payee != tenant.invoice_pubkey {
            return Err(format!(
                "hosted invoice payee does not match tenant {}",
                tenant.tenant_id
            ));
        }
        if invoice
            .trampoline_route_hint()
            .is_some_and(|node_id| Pubkey::from(*node_id) != lsp_node_id)
        {
            return Err("hosted invoice trampoline route hint does not match Public T".to_string());
        }
        if invoice.is_expired() {
            return Err("hosted invoice has already expired".to_string());
        }
        let expiry = invoice
            .expiry_time()
            .ok_or_else(|| "hosted invoice must have a finite expiry".to_string())?;
        let expires_at = invoice
            .data
            .timestamp
            .checked_add(expiry.as_millis())
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| "hosted invoice expiry overflows u64 milliseconds".to_string())?;
        let requested_buffer_duration_ms =
            buffer_duration_ms.unwrap_or(DEFAULT_LSP_BUFFER_DURATION_MS);
        if requested_buffer_duration_ms > MAX_LSP_BUFFER_DURATION_MS {
            return Err(format!(
                "buffer duration exceeds maximum {}ms",
                MAX_LSP_BUFFER_DURATION_MS
            ));
        }
        let buffer_duration_ms = requested_buffer_duration_ms.min(self.max_buffer_duration_ms);
        if signer.pubkey() != lsp_node_id {
            return Err("LSP hint signing key does not match Public T identity".to_string());
        }

        let hint = LspInvoiceHint::sign(
            LspInvoiceHintPayload {
                version: 1,
                lsp_node_id,
                payment_hash: *invoice.payment_hash(),
                invoice_digest: invoice_digest(&invoice),
                buffer_duration_ms,
                expires_at,
            },
            signer,
        )?;
        let registration = LspInvoiceRegistration {
            tenant_id: tenant.tenant_id.clone(),
            invoice,
            hint,
        };

        if let Some(existing) = self.get(registration.invoice.payment_hash())? {
            if existing == registration {
                return Ok(existing);
            }
            return Err("payment hash is already registered to another hosted invoice".to_string());
        }
        self.store.put_lsp_invoice(&registration)?;
        Ok(registration)
    }
}
