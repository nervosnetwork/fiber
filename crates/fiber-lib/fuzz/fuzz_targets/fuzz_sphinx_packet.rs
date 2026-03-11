#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::PaymentOnionPacket;

fuzz_target!(|data: &[u8]| {
    // Fuzz the sphinx onion packet parsing.
    // PaymentOnionPacket wraps raw bytes received in AddTlc P2P messages.
    // The into_sphinx_onion_packet method calls fiber_sphinx::OnionPacket::from_bytes,
    // which parses version, public key, encrypted data, and HMAC from untrusted input.

    let packet = PaymentOnionPacket::new(data.to_vec());
    let _ = packet.into_sphinx_onion_packet();
});
