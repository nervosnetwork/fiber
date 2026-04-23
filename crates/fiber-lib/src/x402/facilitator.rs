use crate::fiber::types::pubkey_from_tentacle;
use crate::invoice::InvoiceStore;
use crate::x402::types::{
    invoice_payee, verify_invoice_preimage, x402_asset_for_invoice, x402_network, FiberExactProof,
    SettleRequest, SettleResponse, VerifyRequest, VerifyResponse, X402_SCHEME_EXACT, X402_VERSION,
};
use crate::FiberConfig;
use std::str::FromStr;

fn invalid(reason: &str, message: &str) -> VerifyResponse {
    VerifyResponse {
        is_valid: false,
        invalid_reason: Some(reason.to_string()),
        invalid_message: Some(message.to_string()),
        payer: None,
        extensions: None,
    }
}

pub fn verify_exact_payment<S>(
    store: &S,
    config: &FiberConfig,
    request: VerifyRequest,
) -> VerifyResponse
where
    S: InvoiceStore,
{
    if request.x402_version != X402_VERSION || request.payment_payload.x402_version != X402_VERSION
    {
        return invalid("unsupported_x402_version", "unsupported x402 version");
    }

    if request.payment_requirements.scheme != X402_SCHEME_EXACT
        || request.payment_payload.accepted.scheme != X402_SCHEME_EXACT
    {
        return invalid("unsupported_scheme", "unsupported payment scheme");
    }

    let expected_network = x402_network(config);
    if request.payment_requirements.network != expected_network
        || request.payment_payload.accepted.network != expected_network
    {
        return invalid("unsupported_network", "unsupported payment network");
    }

    let proof = match FiberExactProof::from_payload(&request.payment_payload.payload) {
        Ok(proof) => proof,
        Err(reason) => return invalid(&reason, "invalid payment proof payload"),
    };

    let invoice = match fiber_types::CkbInvoice::from_str(&proof.invoice) {
        Ok(invoice) => invoice,
        Err(_) => return invalid("invalid_invoice", "invalid invoice encoding"),
    };

    let expected_pay_to = hex::encode(pubkey_from_tentacle(config.public_key()).serialize());
    let invoice_payee = match invoice_payee(&invoice) {
        Ok(payee) => hex::encode(payee.serialize()),
        Err(reason) => return invalid(&reason, "invoice payee is unavailable"),
    };
    if invoice_payee != expected_pay_to {
        return invalid(
            "invoice_payee_mismatch",
            "invoice does not belong to this merchant",
        );
    }

    if request.payment_requirements.pay_to != expected_pay_to
        || request.payment_payload.accepted.pay_to != expected_pay_to
    {
        return invalid(
            "pay_to_mismatch",
            "payment recipient does not match invoice",
        );
    }

    let amount = match invoice.amount() {
        Some(amount) => amount,
        None => return invalid("invalid_invoice_amount", "invoice amount is required"),
    };
    let expected_amount = amount.to_string();
    if request.payment_requirements.amount != expected_amount
        || request.payment_payload.accepted.amount != expected_amount
    {
        return invalid("amount_mismatch", "payment amount does not match invoice");
    }

    let expected_asset = x402_asset_for_invoice(&invoice);
    if request.payment_requirements.asset != expected_asset
        || request.payment_payload.accepted.asset != expected_asset
    {
        return invalid("asset_mismatch", "payment asset does not match invoice");
    }

    if !verify_invoice_preimage(&invoice, proof.payment_preimage) {
        return invalid(
            "invalid_payment_preimage",
            "payment preimage does not match invoice payment hash",
        );
    }

    let payment_hash = invoice.payment_hash();
    if store.get_invoice(payment_hash).is_none() {
        return invalid("invoice_not_found", "invoice not found in merchant store");
    }

    match store.get_invoice_status(payment_hash) {
        Some(crate::invoice::CkbInvoiceStatus::Paid) => VerifyResponse {
            is_valid: true,
            invalid_reason: None,
            invalid_message: None,
            payer: None,
            extensions: None,
        },
        Some(_) => invalid("invoice_not_paid", "invoice has not been paid"),
        None => invalid(
            "invoice_not_found",
            "invoice status not found in merchant store",
        ),
    }
}

pub fn settle_exact_payment<S>(
    store: &S,
    config: &FiberConfig,
    request: SettleRequest,
) -> SettleResponse
where
    S: InvoiceStore,
{
    let verify_request = VerifyRequest {
        x402_version: request.x402_version,
        payment_payload: request.payment_payload.clone(),
        payment_requirements: request.payment_requirements.clone(),
    };

    let verify = verify_exact_payment(store, config, verify_request);
    if !verify.is_valid {
        return SettleResponse {
            success: false,
            error_reason: verify.invalid_reason,
            error_message: verify.invalid_message,
            payer: verify.payer,
            transaction: String::new(),
            network: x402_network(config),
            amount: None,
            extensions: None,
        };
    }

    let proof = FiberExactProof::from_payload(&request.payment_payload.payload)
        .expect("verified settle request must contain valid proof");
    let invoice = fiber_types::CkbInvoice::from_str(&proof.invoice)
        .expect("verified settle request must contain valid invoice");

    SettleResponse {
        success: true,
        error_reason: None,
        error_message: None,
        payer: None,
        transaction: format!("fiber-receipt:{:x}", invoice.payment_hash()),
        network: x402_network(config),
        amount: invoice.amount().map(|amount| amount.to_string()),
        extensions: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{settle_exact_payment, verify_exact_payment};
    use crate::gen_rand_sha256_hash;
    use crate::invoice::{
        CkbInvoice, CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceError, InvoiceStore,
    };
    use crate::tests::get_fiber_config;
    use crate::x402::types::{SettleRequest, VerifyRequest};
    use crate::FiberConfig;
    use fiber_types::Hash256;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct MockStore {
        invoices: RefCell<HashMap<Hash256, CkbInvoice>>,
        invoice_statuses: RefCell<HashMap<Hash256, CkbInvoiceStatus>>,
    }

    impl MockStore {
        fn with_invoice(self, invoice: CkbInvoice, status: CkbInvoiceStatus) -> Self {
            let payment_hash = *invoice.payment_hash();
            self.invoices.borrow_mut().insert(payment_hash, invoice);
            self.invoice_statuses
                .borrow_mut()
                .insert(payment_hash, status);
            self
        }
    }

    impl Default for MockStore {
        fn default() -> Self {
            Self {
                invoices: RefCell::new(HashMap::new()),
                invoice_statuses: RefCell::new(HashMap::new()),
            }
        }
    }

    impl InvoiceStore for MockStore {
        fn get_invoice(&self, id: &Hash256) -> Option<CkbInvoice> {
            self.invoices.borrow().get(id).cloned()
        }

        fn insert_invoice(
            &self,
            invoice: CkbInvoice,
            _preimage: Option<Hash256>,
        ) -> Result<(), InvoiceError> {
            let payment_hash = *invoice.payment_hash();
            self.invoices.borrow_mut().insert(payment_hash, invoice);
            self.invoice_statuses
                .borrow_mut()
                .insert(payment_hash, CkbInvoiceStatus::Open);
            Ok(())
        }

        fn update_invoice_status(
            &self,
            id: &Hash256,
            status: CkbInvoiceStatus,
        ) -> Result<(), InvoiceError> {
            self.invoice_statuses.borrow_mut().insert(*id, status);
            Ok(())
        }

        fn get_invoice_status(&self, id: &Hash256) -> Option<CkbInvoiceStatus> {
            self.invoice_statuses.borrow().get(id).copied()
        }
    }

    fn test_config(chain: &str) -> FiberConfig {
        let temp_dir = TempDir::new().expect("temp dir");
        let mut config = get_fiber_config(&temp_dir, Some("x402-merchant"));
        config.chain = chain.to_string();
        config
    }

    fn build_invoice(
        config: &FiberConfig,
        amount: u128,
        preimage: Hash256,
    ) -> (CkbInvoice, String, String) {
        let secret_key = SecretKey::from_slice(
            config
                .read_or_generate_secret_key()
                .expect("merchant secret key")
                .as_ref(),
        )
        .expect("valid secret key");
        let public_key = PublicKey::from_secret_key(&Secp256k1::new(), &secret_key);
        let invoice = InvoiceBuilder::new(Currency::Fibt)
            .amount(Some(amount))
            .payment_preimage(preimage)
            .payee_pub_key(public_key)
            .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret_key))
            .expect("build signed invoice");
        let invoice_string = invoice.to_string();
        let pay_to = hex::encode(public_key.serialize());
        (invoice, invoice_string, pay_to)
    }

    fn verify_request(pay_to: &str, invoice: &str, payment_preimage: Hash256) -> VerifyRequest {
        let requirements = json!({
            "scheme": "exact",
            "network": "fiber:testnet",
            "asset": "ckb",
            "amount": "1000",
            "payTo": pay_to,
            "maxTimeoutSeconds": 60,
            "extra": {},
        });
        let payload = json!({
            "x402Version": 2,
            "accepted": requirements.clone(),
            "payload": {
                "invoice": invoice,
                "paymentPreimage": payment_preimage,
            }
        });

        serde_json::from_value(json!({
            "x402Version": 2,
            "paymentPayload": payload,
            "paymentRequirements": requirements,
        }))
        .expect("deserialize verify request")
    }

    #[test]
    fn test_x402_verify_rejects_pay_to_mismatch() {
        let config = test_config("testnet");
        let preimage = gen_rand_sha256_hash();
        let (invoice, invoice_string, _pay_to) = build_invoice(&config, 1000, preimage);
        let store = MockStore::default().with_invoice(invoice, CkbInvoiceStatus::Paid);

        let response = verify_exact_payment(
            &store,
            &config,
            verify_request(&hex::encode([7u8; 33]), &invoice_string, preimage),
        );

        assert!(!response.is_valid);
        assert_eq!(response.invalid_reason.as_deref(), Some("pay_to_mismatch"));
    }

    #[test]
    fn test_x402_verify_accepts_paid_invoice_proof_unit() {
        let config = test_config("testnet");
        let preimage = gen_rand_sha256_hash();
        let (invoice, invoice_string, pay_to) = build_invoice(&config, 1000, preimage);
        let store = MockStore::default().with_invoice(invoice, CkbInvoiceStatus::Paid);

        let response = verify_exact_payment(
            &store,
            &config,
            verify_request(&pay_to, &invoice_string, preimage),
        );

        assert!(response.is_valid);
        assert!(response.invalid_reason.is_none());
    }

    #[test]
    fn test_x402_settle_returns_deterministic_receipt_unit() {
        let config = test_config("testnet");
        let preimage = gen_rand_sha256_hash();
        let (invoice, invoice_string, pay_to) = build_invoice(&config, 1000, preimage);
        let payment_hash = *invoice.payment_hash();
        let store = MockStore::default().with_invoice(invoice, CkbInvoiceStatus::Paid);

        let verify = verify_request(&pay_to, &invoice_string, preimage);
        let response = settle_exact_payment(
            &store,
            &config,
            SettleRequest {
                x402_version: verify.x402_version,
                payment_payload: verify.payment_payload,
                payment_requirements: verify.payment_requirements,
            },
        );

        assert!(response.success);
        assert_eq!(response.network, "fiber:testnet");
        assert_eq!(response.amount.as_deref(), Some("1000"));
        assert_eq!(
            response.transaction,
            format!("fiber-receipt:{:x}", payment_hash)
        );
    }
}
