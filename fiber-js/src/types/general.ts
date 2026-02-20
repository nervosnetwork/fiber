interface DbWorkerInitializationOptions {
    inputBuffer: SharedArrayBuffer;
    outputBuffer: SharedArrayBuffer;
    logLevel: string;
}
interface FiberWorkerInitializationOptions {
    inputBuffer: SharedArrayBuffer;
    outputBuffer: SharedArrayBuffer;
    logLevel: string;
    fiberKeyPair: Uint8Array;
    ckbSecretKey: Uint8Array;
    config: string;
    chainSpec?: string;
    databasePrefix?: string;
}

interface FiberInvokeRequest {
    name: string;
    args: any[];
};
type FiberInvokeResponse = { ok: true; data: any; } | { ok: false; error: string };

type HexString = `0x${string}`;

type HashAlgorithm = "ckb_hash" | "sha256";

export type {
    DbWorkerInitializationOptions,
    FiberWorkerInitializationOptions,
    FiberInvokeRequest,
    FiberInvokeResponse,
    HexString,
    HashAlgorithm
}
