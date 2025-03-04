use crate::fiber::serde_utils::{EntityHex, SliceHex, U128Hex, U16Hex, U32Hex, U64Hex};
use ckb_types::packed::Script;
use ckb_types::prelude::*;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[serde_as]
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Foo {
    #[serde_as(as = "SliceHex")]
    slice: [u8; 4],
    #[serde_as(as = "Option<EntityHex>")]
    entity: Option<Script>,
    #[serde_as(as = "U128Hex")]
    bar_128: u128,
    #[serde_as(as = "U64Hex")]
    bar_64: u64,
    #[serde_as(as = "U32Hex")]
    bar_32: u32,
    #[serde_as(as = "U16Hex")]
    bar_16: u16,
}

#[test]
fn test_serde_utils() {
    let foo = Foo {
        slice: [1, 2, 3, 4],
        entity: Some(Script::new_builder().build()),
        bar_128: 0xdeadbeef,
        bar_64: 0x123,
        bar_32: 0x10,
        bar_16: 0xa,
    };

    let json = r#"{"slice":"0x01020304","entity":"0x3500000010000000300000003100000000000000000000000000000000000000000000000000000000000000000000000000000000","bar_128":"0xdeadbeef","bar_64":"0x123","bar_32":"0x10","bar_16":"0xa"}"#;
    assert_eq!(serde_json::to_string(&foo).unwrap(), json);
    assert_eq!(serde_json::from_str::<Foo>(json).unwrap(), foo);
}
