#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::invoice::CkbInvoice;
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    // Fuzz the CKB invoice parser (bech32m decoding + molecule deserialization).
    // This is user-provided input from payment strings.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = CkbInvoice::from_str(s);
    }
});
