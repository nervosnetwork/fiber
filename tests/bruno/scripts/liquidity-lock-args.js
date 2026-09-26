// Shared decoder for the liquidity-lock contract script args and UDT cell
// data used by the liquidity Bruno suites.
//
// The `build_liquidity_lock_args` layout (crates/fiber-lib/src/liquidity/mod.rs)
// is exactly 152 bytes:
//   payment_hash            32 bytes
//   blake2b(claimant_lock)  32 bytes
//   blake2b(refund_lock)    32 bytes
//   refund_after_lock_time   8 bytes (u64 little-endian absolute CKB `since`)
//   amount                  16 bytes (u128 little-endian CKB/UDT amount)
//   asset_type_hash         32 bytes (blake2b of the UDT type script molecule,
//                                    all zeros for CKB swaps)
//
// UDT cell data holds the token amount as a 16-byte little-endian u128.

const ARGS_LENGTH = 152;
const UDT_DATA_LENGTH = 16;

function normalizeHexBytes(value, expectedLength, field) {
  if (typeof value !== "string") {
    throw new Error(`${field} must be a hex string`);
  }
  const hex = value.startsWith("0x") || value.startsWith("0X") ? value.slice(2) : value;
  if (!/^[0-9a-fA-F]*$/.test(hex)) {
    throw new Error(`${field} must contain only hex characters: ${value}`);
  }
  if (hex.length % 2 !== 0) {
    throw new Error(`${field} must have an even number of hex characters: ${value}`);
  }
  if (expectedLength !== undefined && hex.length !== expectedLength * 2) {
    throw new Error(`${field} must be ${expectedLength} bytes, got ${hex.length / 2}`);
  }
  const bytes = [];
  for (let i = 0; i < hex.length; i += 2) {
    bytes.push(parseInt(hex.slice(i, i + 2), 16));
  }
  return bytes;
}

function toHex(bytes) {
  return (
    "0x" + bytes.map((b) => b.toString(16).padStart(2, "0")).join("")
  );
}

function readLeUint(bytes, offset, length, field) {
  let value = 0n;
  for (let i = length - 1; i >= 0; i--) {
    value = (value << 8n) | BigInt(bytes[offset + i]);
  }
  if (value < 0n) {
    throw new Error(`${field} must not be negative`);
  }
  return value;
}

function decodeLiquidityLockArgs(argsHex) {
  const bytes = normalizeHexBytes(argsHex, ARGS_LENGTH, "liquidity lock args");
  return {
    paymentHash: toHex(bytes.slice(0, 32)),
    claimantLockHash: toHex(bytes.slice(32, 64)),
    refundLockHash: toHex(bytes.slice(64, 96)),
    refundAfterLockTime: readLeUint(bytes, 96, 8, "refund_after_lock_time"),
    amount: readLeUint(bytes, 104, 16, "amount"),
    assetTypeHash: toHex(bytes.slice(120, 152)),
  };
}

function decodeUdtAmount(dataHex) {
  const bytes = normalizeHexBytes(dataHex, UDT_DATA_LENGTH, "UDT cell data");
  return readLeUint(bytes, 0, UDT_DATA_LENGTH, "UDT amount");
}

module.exports = {
  ARGS_LENGTH,
  UDT_DATA_LENGTH,
  decodeLiquidityLockArgs,
  decodeUdtAmount,
};
