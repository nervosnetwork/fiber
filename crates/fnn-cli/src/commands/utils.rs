/// Format a u128 value as hex string with 0x prefix (matching U128Hex serde format).
pub fn to_hex_u128(val: u128) -> String {
    format!("{:#x}", val)
}

/// Format a u64 value as hex string with 0x prefix (matching U64Hex serde format).
pub fn to_hex_u64(val: u64) -> String {
    format!("{:#x}", val)
}
