use super::*;

#[test]
fn test_broadcast_message_samples_roundtrip() {
    BroadcastMessage::verify_samples_roundtrip(42);
}
