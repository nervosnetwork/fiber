#![cfg(target_arch = "wasm32")]

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

/// Test that JS `undefined` is correctly converted to Rust `None` for Option<Vec<u8>>
/// This is a regression test for the ckbSecretKey optional parameter.
#[wasm_bindgen_test]
fn test_undefined_to_option_vec_u8() {
    // This test verifies that when JS passes `undefined` for an optional parameter,
    // wasm-bindgen correctly converts it to `None` in Rust.

    // Test case 1: undefined -> None
    let undefined = JsValue::UNDEFINED;
    let result: Option<Vec<u8>> =
        serde_wasm_bindgen::from_value(undefined).expect("undefined should deserialize to None");
    assert!(result.is_none(), "undefined should be converted to None");

    // Test case 2: null -> None
    let null = JsValue::NULL;
    let result: Option<Vec<u8>> =
        serde_wasm_bindgen::from_value(null).expect("null should deserialize to None");
    assert!(result.is_none(), "null should be converted to None");

    // Test case 3: valid array -> Some
    let arr = js_sys::Uint8Array::from(&[1u8, 2, 3, 4][..]);
    let result: Option<Vec<u8>> =
        serde_wasm_bindgen::from_value(arr.into()).expect("valid array should deserialize to Some");
    assert_eq!(
        result,
        Some(vec![1, 2, 3, 4]),
        "valid array should be converted to Some"
    );
}

/// Test edge cases for secret key bytes conversion
#[wasm_bindgen_test]
fn test_secret_key_bytes_conversion() {
    use secp256k1::SecretKey;

    // Test valid 32-byte secret key
    let valid_key = [1u8; 32];
    let result = SecretKey::from_slice(&valid_key);
    assert!(result.is_ok(), "32-byte array should be a valid secret key");

    // Test that from_slice properly validates the key
    // Note: all zeros is invalid for secp256k1
    let invalid_key = [0u8; 32];
    let result = SecretKey::from_slice(&invalid_key);
    // This should fail because all zeros is not a valid secret key
    assert!(result.is_err(), "all-zeros key should be invalid");
}
