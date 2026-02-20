#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::{PaymentHopData, TrampolineHopPayload};

fuzz_target!(|data: &[u8]| {
    // Fuzz the onion packet hop data deserialization.
    // These are parsed from untrusted forwarded data in the P2P network.

    // Fuzz PaymentHopData molecule deserialization
    let _ = PaymentHopData::deserialize(data);

    // Fuzz TrampolineHopPayload molecule deserialization
    let _ = TrampolineHopPayload::deserialize(data);

    // Also test roundtrip: if deserialization succeeds, re-serialization
    // followed by deserialization should produce the same result.
    if let Some(hop) = PaymentHopData::deserialize(data) {
        let reserialized = hop.serialize();
        let hop2 = PaymentHopData::deserialize(&reserialized)
            .expect("roundtrip deserialization must succeed");
        assert_eq!(hop, hop2, "roundtrip must preserve PaymentHopData");
    }

    if let Some(payload) = TrampolineHopPayload::deserialize(data) {
        let reserialized = payload.serialize();
        let payload2 = TrampolineHopPayload::deserialize(&reserialized)
            .expect("roundtrip deserialization must succeed");
        assert_eq!(
            payload, payload2,
            "roundtrip must preserve TrampolineHopPayload"
        );
    }
});
