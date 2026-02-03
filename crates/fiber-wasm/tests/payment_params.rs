#![cfg(target_arch = "wasm32")]

use fnn::{fiber::types::Privkey, rpc::payment::SendPaymentCommandParams};
use wasm_bindgen_test::wasm_bindgen_test;

#[wasm_bindgen_test]
fn send_payment_params_trampoline_roundtrip() {
    let pubkey = Privkey::from_slice(&[1u8; 32]).pubkey();

    let params = SendPaymentCommandParams {
        target_pubkey: Some(pubkey),
        amount: None,
        payment_hash: None,
        final_tlc_expiry_delta: None,
        tlc_expiry_limit: None,
        invoice: None,
        timeout: None,
        max_fee_amount: None,
        max_fee_rate: Some(5),
        max_parts: None,
        trampoline_hops: Some(vec![pubkey]),
        keysend: None,
        udt_type_script: None,
        allow_self_payment: None,
        custom_records: None,
        hop_hints: None,
        dry_run: None,
    };

    let value = serde_wasm_bindgen::to_value(&params).expect("serialize params");
    let decoded: SendPaymentCommandParams =
        serde_wasm_bindgen::from_value(value).expect("deserialize params");

    assert_eq!(decoded.trampoline_hops, params.trampoline_hops);
    assert_eq!(decoded.max_fee_rate, params.max_fee_rate);
}
