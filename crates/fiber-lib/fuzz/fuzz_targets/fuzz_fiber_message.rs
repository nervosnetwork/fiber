#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::FiberMessage;

fuzz_target!(|data: &[u8]| {
    // Fuzz the P2P message deserialization from untrusted molecule bytes.
    // Any panic here indicates a bug in the parser.
    let _ = FiberMessage::from_molecule_slice(data);
});
