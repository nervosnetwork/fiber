// Shared Molecule encoding helpers for CKB script/outpoint values used by the
// liquidity Bruno suites.
//
// The CKB `Script` molecule table layout is:
//   u32 full byte size, u32 offset per field (3 fields), then
//   code_hash (32 bytes), hash_type (1 byte), args (Bytes vector).
// The `OutPoint` molecule fixvec entry is tx_hash (32 bytes) followed by a
// little-endian u32 index (36 bytes total).

const HASH_TYPE_TO_BYTE = {
  data: 0,
  type: 1,
  data1: 2,
  data2: 3,
};

const BYTE_TO_HASH_TYPE = Object.fromEntries(
  Object.entries(HASH_TYPE_TO_BYTE).map(([name, byte]) => [byte, name]),
);

function pushU32(bytes, value) {
  if (!Number.isInteger(value) || value < 0 || value > 0xffffffff) {
    throw new Error(`value is not a u32: ${value}`);
  }
  bytes.push(value & 0xff, (value >> 8) & 0xff, (value >>> 16) & 0xff, (value >>> 24) & 0xff);
}

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

function hashTypeByte(hashType) {
  if (typeof hashType === "number") {
    if (!Object.prototype.hasOwnProperty.call(BYTE_TO_HASH_TYPE, hashType)) {
      throw new Error(`unknown hash_type byte: ${hashType}`);
    }
    return hashType;
  }
  if (typeof hashType !== "string" || !Object.prototype.hasOwnProperty.call(HASH_TYPE_TO_BYTE, hashType)) {
    throw new Error(`unknown hash_type: ${JSON.stringify(hashType)}`);
  }
  return HASH_TYPE_TO_BYTE[hashType];
}

function encodeMoleculeScript({ codeHash, hashType, args }) {
  const codeHashBytes = normalizeHexBytes(codeHash, 32, "code_hash");
  const byte = hashTypeByte(hashType);
  const argsBytes = normalizeHexBytes(args || "0x", undefined, "args");

  const bytes = [];
  pushU32(bytes, 16 + 32 + 1 + 4 + argsBytes.length);
  pushU32(bytes, 16);
  pushU32(bytes, 48);
  pushU32(bytes, 49);
  bytes.push(...codeHashBytes);
  bytes.push(byte);
  pushU32(bytes, argsBytes.length);
  bytes.push(...argsBytes);

  return "0x" + bytes.map((b) => b.toString(16).padStart(2, "0")).join("");
}

function readU32(bytes, offset, field) {
  if (offset + 4 > bytes.length) {
    throw new Error(`${field} offset ${offset} is outside the ${bytes.length}-byte buffer`);
  }
  return (
    bytes[offset] |
    (bytes[offset + 1] << 8) |
    (bytes[offset + 2] << 16) |
    (bytes[offset + 3] * 0x1000000)
  );
}

function decodeMoleculeScript(hex) {
  const bytes = normalizeHexBytes(hex, undefined, "script");
  const totalSize = readU32(bytes, 0, "total_size");
  if (totalSize !== bytes.length) {
    throw new Error(`script total_size ${totalSize} does not match buffer length ${bytes.length}`);
  }
  const codeHashOffset = readU32(bytes, 4, "code_hash offset");
  const hashTypeOffset = readU32(bytes, 8, "hash_type offset");
  const argsOffset = readU32(bytes, 12, "args offset");
  if (codeHashOffset !== 16 || hashTypeOffset !== 48 || argsOffset !== 49) {
    throw new Error(
      `unexpected script field offsets: code_hash=${codeHashOffset}, hash_type=${hashTypeOffset}, args=${argsOffset}`,
    );
  }

  const codeHashBytes = bytes.slice(16, 48);
  const hashType = bytes[48];
  const argsLength = readU32(bytes, 49, "args length");
  if (49 + 4 + argsLength !== bytes.length) {
    throw new Error(`args length ${argsLength} does not match the trailing bytes`);
  }

  return {
    codeHash:
      "0x" + codeHashBytes.map((b) => b.toString(16).padStart(2, "0")).join(""),
    hashType: BYTE_TO_HASH_TYPE[hashType],
    hashTypeByte: hashType,
    args:
      "0x" +
      bytes
        .slice(53)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(""),
  };
}

function decodeOutpoint(hex) {
  if (typeof hex !== "string" || !hex.startsWith("0x") || hex.length !== 74) {
    throw new Error(`outpoint must be 36 molecule bytes: ${hex}`);
  }
  const bytes = normalizeHexBytes(hex, 36, "outpoint");
  return {
    txHash: "0x" + bytes.slice(0, 32).map((b) => b.toString(16).padStart(2, "0")).join(""),
    index: bytes[32] | (bytes[33] << 8) | (bytes[34] << 16) | (bytes[35] * 0x1000000),
  };
}

module.exports = {
  BYTE_TO_HASH_TYPE,
  HASH_TYPE_TO_BYTE,
  decodeMoleculeScript,
  decodeOutpoint,
  encodeMoleculeScript,
  hashTypeByte,
};
