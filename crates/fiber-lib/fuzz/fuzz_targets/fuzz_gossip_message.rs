#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::GossipMessage;

fuzz_target!(|data: &[u8]| {
    // Fuzz the gossip protocol message deserialization from untrusted molecule bytes.
    // Any panic here indicates a bug in the parser.
    let _ = GossipMessage::from_molecule_slice(data);
});
