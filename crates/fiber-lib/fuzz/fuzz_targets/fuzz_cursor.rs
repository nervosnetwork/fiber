#![no_main]

use libfuzzer_sys::fuzz_target;

use fnn::fiber::types::Cursor;

fuzz_target!(|data: &[u8]| {
    // Fuzz gossip cursor parsing from arbitrary bytes.
    // Cursors are embedded in P2P GetBroadcastMessages requests.
    // Internally parses BroadcastMessageID (with Pubkey and OutPoint).

    // Cursor: 45-byte timestamp + message_id
    let _ = Cursor::from_bytes(data);

    // Roundtrip: if parsing succeeds, serialization should roundtrip
    if let Ok(cursor) = Cursor::from_bytes(data) {
        let bytes = cursor.to_bytes();
        let cursor2 = Cursor::from_bytes(&bytes).expect("roundtrip must succeed");
        assert_eq!(cursor, cursor2, "roundtrip must preserve Cursor");
    }
});
