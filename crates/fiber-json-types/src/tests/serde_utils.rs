use crate::serde_utils::Privkey;

#[test]
fn privkey_debug_is_redacted() {
    let raw = [7u8; 32];
    let raw_hex = hex::encode(raw);
    let debug = format!("{:?}", Privkey(raw));

    assert_eq!(debug, "Privkey(<redacted>)");
    assert!(!debug.contains(&raw_hex));
}
