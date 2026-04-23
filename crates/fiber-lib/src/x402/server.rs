use crate::fiber::types::pubkey_from_tentacle;
use crate::invoice::InvoiceStore;
use crate::x402::facilitator::{settle_exact_payment, verify_exact_payment};
use crate::x402::types::{
    x402_network, SettleRequest, SettleResponse, SupportedKind, SupportedResponse, VerifyRequest,
    VerifyResponse, X402_SCHEME_EXACT, X402_VERSION,
};
use crate::FiberConfig;
use std::collections::HashMap;

pub fn supported_response(config: &FiberConfig) -> SupportedResponse {
    let signer = pubkey_from_tentacle(config.public_key());
    let mut signers = HashMap::new();
    signers.insert("fiber:*".to_string(), vec![hex::encode(signer.serialize())]);

    SupportedResponse {
        kinds: vec![SupportedKind {
            x402_version: X402_VERSION,
            scheme: X402_SCHEME_EXACT.to_string(),
            network: x402_network(config),
            extra: None,
        }],
        extensions: Vec::new(),
        signers,
    }
}

pub fn verify_response<S>(store: &S, config: &FiberConfig, request: VerifyRequest) -> VerifyResponse
where
    S: InvoiceStore,
{
    verify_exact_payment(store, config, request)
}

pub fn settle_response<S>(store: &S, config: &FiberConfig, request: SettleRequest) -> SettleResponse
where
    S: InvoiceStore,
{
    settle_exact_payment(store, config, request)
}
