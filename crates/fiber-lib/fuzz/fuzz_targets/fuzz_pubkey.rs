#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::Pubkey;

fuzz_target!(|data: &[u8]| {
    // Fuzz secp256k1 public key parsing from arbitrary bytes.
    // Pubkey::from_slice is called during P2P message deserialization
    // (TryFrom<Byte33> for Pubkey) for node_id, next_hop, etc.
    // Must never panic — should return Err for invalid keys.

    let _ = Pubkey::from_slice(data);

    // Also test with exactly 33 bytes (compressed pubkey size)
    if data.len() >= 33 {
        let _ = Pubkey::from_slice(&data[..33]);
    }

    // Test roundtrip: if parsing succeeds, serialization should roundtrip
    if let Ok(pk) = Pubkey::from_slice(data) {
        let serialized = pk.serialize();
        let pk2 = Pubkey::from_slice(&serialized).expect("roundtrip must succeed");
        assert_eq!(pk, pk2, "roundtrip must preserve Pubkey");
    }
});
