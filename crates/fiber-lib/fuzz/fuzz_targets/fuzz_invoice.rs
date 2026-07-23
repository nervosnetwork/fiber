#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::invoice::CkbInvoice;

fuzz_target!(|data: &[u8]| {
    // Fuzz the CKB invoice parser (bech32m decoding + molecule deserialization).
    // This is user-provided input from payment strings.
    // Use the unsigned-allowing parser so the fuzzer also exercises the
    // unsigned-invoice code path.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = CkbInvoice::from_str_allowing_unsigned(s);
    }
});
