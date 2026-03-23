#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const rawResponse = process.argv[2];
const privateKey = process.argv[3];

if (!rawResponse) {
    console.error("Usage: node sign-openchannel-response.mjs '<open_channel_response_json>' '<ckb_private_key>'");
    process.exit(1);
}

if (!privateKey || !/^0x[0-9a-fA-F]{64}$/.test(privateKey)) {
    console.error("The second argument must be a 0x-prefixed 32-byte hex private key");
    process.exit(1);
}

let parsed;
try {
    parsed = JSON.parse(rawResponse);
} catch (error) {
    console.error(`Invalid JSON response: ${error instanceof Error ? error.message : String(error)}`);
    process.exit(1);
}

if (parsed.error) {
    console.error(`open_channel_with_external_funding RPC error: ${JSON.stringify(parsed.error)}`);
    process.exit(1);
}

const channelId = parsed.result?.channel_id ?? parsed.result?.temporary_channel_id;
const unsignedFundingTx = parsed.result?.unsigned_funding_tx;

if (!channelId) {
    console.error("Missing result.channel_id (or legacy result.temporary_channel_id)");
    process.exit(1);
}

if (!unsignedFundingTx) {
    console.error("Missing result.unsigned_funding_tx");
    process.exit(1);
}

const tempDir = mkdtempSync(join(tmpdir(), "fiber-ext-funding-sign-"));
const txPath = join(tempDir, "unsigned-funding-tx.json");
const keyPath = join(tempDir, "privkey");

function getLockArgFromPrivateKey(privkeyPath) {
    const result = spawnSync(
        "ckb-cli",
        [
            "util",
            "key-info",
            "--privkey-path",
            privkeyPath,
            "--output-format",
            "json",
            "--local-only",
            "--no-color",
        ],
        { encoding: "utf8" },
    );

    if (result.status !== 0) {
        process.stderr.write(result.stderr || "ckb-cli util key-info failed\n");
        process.exit(result.status ?? 1);
    }

    const output = result.stdout ?? "";
    const jsonStart = output.indexOf("{");
    if (jsonStart < 0) {
        console.error("Failed to parse ckb-cli key-info output");
        process.exit(1);
    }

    const info = JSON.parse(output.slice(jsonStart));
    const lockArg = info.lock_arg;
    if (typeof lockArg !== "string" || !/^0x[0-9a-fA-F]{40}$/.test(lockArg)) {
        console.error("ckb-cli key-info output missing lock_arg");
        process.exit(1);
    }

    return lockArg.toLowerCase();
}

function hexToBytes(hex) {
    const clean = hex.startsWith("0x") ? hex.slice(2) : hex;
    if (clean.length % 2 !== 0) {
        throw new Error("hex string has odd length");
    }
    const out = new Uint8Array(clean.length / 2);
    for (let i = 0; i < out.length; i += 1) {
        out[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
    }
    return out;
}

function bytesToHex(bytes) {
    return `0x${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

function readU32Le(bytes, offset) {
    return (
        bytes[offset] |
        (bytes[offset + 1] << 8) |
        (bytes[offset + 2] << 16) |
        (bytes[offset + 3] << 24)
    ) >>> 0;
}

function writeU32Le(bytes, offset, value) {
    bytes[offset] = value & 0xff;
    bytes[offset + 1] = (value >>> 8) & 0xff;
    bytes[offset + 2] = (value >>> 16) & 0xff;
    bytes[offset + 3] = (value >>> 24) & 0xff;
}

function decodeBytesOpt(fieldBytes) {
    if (fieldBytes.length === 0) {
        return null;
    }
    if (fieldBytes.length < 4) {
        throw new Error("invalid BytesOpt field length");
    }
    const len = readU32Le(fieldBytes, 0);
    if (len !== fieldBytes.length - 4) {
        throw new Error("invalid BytesOpt payload length");
    }
    return fieldBytes.slice(4);
}

function encodeBytesOpt(maybeBytes) {
    if (maybeBytes == null) {
        return new Uint8Array(0);
    }
    const out = new Uint8Array(4 + maybeBytes.length);
    writeU32Le(out, 0, maybeBytes.length);
    out.set(maybeBytes, 4);
    return out;
}

function decodeWitnessArgs(hex) {
    const bytes = hexToBytes(hex);
    if (bytes.length === 0) {
        return { lock: null, inputType: null, outputType: null };
    }
    if (bytes.length < 16) {
        throw new Error("invalid WitnessArgs length");
    }
    const total = readU32Le(bytes, 0);
    if (total !== bytes.length) {
        throw new Error("WitnessArgs total size mismatch");
    }
    const off0 = readU32Le(bytes, 4);
    const off1 = readU32Le(bytes, 8);
    const off2 = readU32Le(bytes, 12);
    if (!(off0 >= 16 && off0 <= off1 && off1 <= off2 && off2 <= total)) {
        throw new Error("invalid WitnessArgs offsets");
    }

    return {
        lock: decodeBytesOpt(bytes.slice(off0, off1)),
        inputType: decodeBytesOpt(bytes.slice(off1, off2)),
        outputType: decodeBytesOpt(bytes.slice(off2, total)),
    };
}

function encodeWitnessArgs({ lock, inputType, outputType }) {
    const field0 = encodeBytesOpt(lock);
    const field1 = encodeBytesOpt(inputType);
    const field2 = encodeBytesOpt(outputType);
    const headerSize = 16;
    const off0 = headerSize;
    const off1 = off0 + field0.length;
    const off2 = off1 + field1.length;
    const total = off2 + field2.length;

    const out = new Uint8Array(total);
    writeU32Le(out, 0, total);
    writeU32Le(out, 4, off0);
    writeU32Le(out, 8, off1);
    writeU32Le(out, 12, off2);
    out.set(field0, off0);
    out.set(field1, off1);
    out.set(field2, off2);
    return bytesToHex(out);
}

function isAllZero(bytes) {
    for (const value of bytes) {
        if (value !== 0) {
            return false;
        }
    }
    return true;
}

function pickSignatureHex(value) {
    if (typeof value === "string") {
        if (/^0x[0-9a-fA-F]{130}$/.test(value)) {
            return value;
        }
        return null;
    }
    if (Array.isArray(value)) {
        for (const item of value) {
            const found = pickSignatureHex(item);
            if (found) {
                return found;
            }
        }
        return null;
    }
    if (value && typeof value === "object") {
        for (const item of Object.values(value)) {
            const found = pickSignatureHex(item);
            if (found) {
                return found;
            }
        }
    }
    return null;
}

function getSignatureHexForLockArg(signatures, lockArg) {
    if (!signatures || typeof signatures !== "object") {
        return null;
    }

    const directCandidates = [
        signatures[lockArg],
        signatures[lockArg.toLowerCase()],
        signatures[lockArg.toUpperCase()],
    ];
    for (const candidate of directCandidates) {
        const found = pickSignatureHex(candidate);
        if (found) {
            return found;
        }
    }

    for (const [key, value] of Object.entries(signatures)) {
        if (typeof key === "string" && key.toLowerCase() === lockArg) {
            const found = pickSignatureHex(value);
            if (found) {
                return found;
            }
        }
    }

    return pickSignatureHex(signatures);
}

function maybePatchWitnessWithSignature(transaction, signatures, lockArg) {
    if (!transaction || !Array.isArray(transaction.witnesses) || transaction.witnesses.length === 0) {
        return;
    }
    const firstWitness = transaction.witnesses[0];
    if (typeof firstWitness !== "string") {
        return;
    }
    let parsedWitness;
    try {
        parsedWitness = decodeWitnessArgs(firstWitness);
    } catch (_) {
        return;
    }
    if (!parsedWitness.lock || !isAllZero(parsedWitness.lock)) {
        return;
    }
    const signatureHex = getSignatureHexForLockArg(signatures, lockArg);
    if (!signatureHex) {
        return;
    }
    parsedWitness.lock = hexToBytes(signatureHex);
    transaction.witnesses[0] = encodeWitnessArgs(parsedWitness);
}

try {
    // Align with fiber-wasm-demo-c2c: keep WitnessArgs structure, replace only lock with 65 zero bytes.
    const txForSigning = {
        ...unsignedFundingTx,
        witnesses: Array.isArray(unsignedFundingTx.witnesses)
            ? [...unsignedFundingTx.witnesses]
            : [],
    };
    const firstWitness = txForSigning.witnesses[0] ?? "0x";
    const parsedWitness = decodeWitnessArgs(firstWitness);
    parsedWitness.lock = new Uint8Array(65);
    const rewrittenWitness = encodeWitnessArgs(parsedWitness);
    if (txForSigning.witnesses.length === 0) {
        txForSigning.witnesses.push(rewrittenWitness);
    } else {
        txForSigning.witnesses[0] = rewrittenWitness;
    }

    const txFilePayload = {
        transaction: txForSigning,
        multisig_configs: {},
        signatures: {},
    };
    writeFileSync(txPath, `${JSON.stringify(txFilePayload)}\n`);
    writeFileSync(keyPath, `${privateKey}\n`, { mode: 0o600 });
    chmodSync(keyPath, 0o600);
    const lockArg = getLockArgFromPrivateKey(keyPath);

    const signed = spawnSync(
        "ckb-cli",
        [
            "--local-only",
            "--no-color",
            "--output-format",
            "json",
            "tx",
            "sign-inputs",
            "--tx-file",
            txPath,
            "--privkey-path",
            keyPath,
            "--add-signatures",
        ],
        { encoding: "utf8" },
    );

    if (signed.status !== 0) {
        process.stderr.write(signed.stderr || "ckb-cli tx sign-inputs failed\n");
        process.exit(signed.status ?? 1);
    }

    let signedTxFile;
    try {
        signedTxFile = JSON.parse(readFileSync(txPath, "utf8"));
    } catch (error) {
        console.error(
            `Failed to parse signed tx file: ${error instanceof Error ? error.message : String(error)}`,
        );
        process.exit(1);
    }

    const signedFundingTx = signedTxFile?.transaction;
    if (!signedFundingTx) {
        console.error("Missing transaction in signed tx file");
        process.exit(1);
    }
    maybePatchWitnessWithSignature(signedFundingTx, signedTxFile?.signatures, lockArg);
    process.stdout.write(
        JSON.stringify({
            channel_id: channelId,
            signed_funding_tx: signedFundingTx,
        }),
    );
} finally {
    rmSync(tempDir, { recursive: true, force: true });
}
