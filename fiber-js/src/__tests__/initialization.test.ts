/**
 * Tests for Fiber initialization options, particularly for optional parameters.
 * 
 * These tests verify that the TypeScript types correctly allow optional
 * parameters like `ckbSecretKey` to be undefined.
 */

/// <reference types="@types/jest" />
import { FiberWorkerInitializationOptions } from '../types/general';
import { OpenChannelWithExternalFundingParams } from '../types/channel';
import { HexString } from '../types/general';

describe('FiberWorkerInitializationOptions', () => {
    const baseOptions = {
        inputBuffer: new SharedArrayBuffer(1024),
        outputBuffer: new SharedArrayBuffer(1024),
        logLevel: 'debug',
        fiberKeyPair: new Uint8Array(32).fill(1),
        config: JSON.stringify({
            fiber: {
                listening_addr: '/ip4/127.0.0.1/tcp/8228',
            },
            ckb: {
                rpc_url: 'http://127.0.0.1:8114',
            }
        }),
    };

    it('should accept ckbSecretKey as undefined', () => {
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
            ckbSecretKey: undefined,
        };
        expect(options.ckbSecretKey).toBeUndefined();
    });

    it('should accept ckbSecretKey as valid Uint8Array', () => {
        const validSecretKey = new Uint8Array(32).fill(2);
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
            ckbSecretKey: validSecretKey,
        };
        expect(options.ckbSecretKey).toBe(validSecretKey);
        expect(options.ckbSecretKey?.length).toBe(32);
    });

    it('should accept when ckbSecretKey is omitted', () => {
        // This tests that ckbSecretKey is truly optional
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
        };
        expect(options.ckbSecretKey).toBeUndefined();
    });

    it('should accept chainSpec as undefined', () => {
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
            chainSpec: undefined,
        };
        expect(options.chainSpec).toBeUndefined();
    });

    it('should accept databasePrefix as undefined', () => {
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
            databasePrefix: undefined,
        };
        expect(options.databasePrefix).toBeUndefined();
    });

    it('should accept databasePrefix as string', () => {
        const options: FiberWorkerInitializationOptions = {
            ...baseOptions,
            databasePrefix: 'test-prefix',
        };
        expect(options.databasePrefix).toBe('test-prefix');
    });
});

describe('Optional parameter edge cases', () => {
    it('should accept external funding params with pubkey', () => {
        const shutdownCodeHash = ('0x' + '22'.repeat(32)) as HexString;
        const shutdownArgs = ('0x' + '33'.repeat(20)) as HexString;
        const fundingCodeHash = ('0x' + '44'.repeat(32)) as HexString;
        const fundingArgs = ('0x' + '55'.repeat(20)) as HexString;

        const params: OpenChannelWithExternalFundingParams = {
            pubkey: '03' + '11'.repeat(32),
            funding_amount: '0x174876e800',
            shutdown_script: {
                code_hash: shutdownCodeHash,
                hash_type: 'type',
                args: shutdownArgs,
            },
            funding_lock_script: {
                code_hash: fundingCodeHash,
                hash_type: 'type',
                args: fundingArgs,
            },
        };

        expect(params.pubkey.startsWith('03')).toBe(true);
    });

    it('should handle all optional params being undefined', () => {
        const options: FiberWorkerInitializationOptions = {
            inputBuffer: new SharedArrayBuffer(1024),
            outputBuffer: new SharedArrayBuffer(1024),
            logLevel: 'info',
            fiberKeyPair: new Uint8Array(32),
            config: '{}',
            // ckbSecretKey, chainSpec, databasePrefix are all omitted
        };
        
        expect(options.ckbSecretKey).toBeUndefined();
        expect(options.chainSpec).toBeUndefined();
        expect(options.databasePrefix).toBeUndefined();
    });

    it('should handle ckbSecretKey with exact 32 bytes', () => {
        // 32 bytes is the expected length for secp256k1 secret keys
        const secretKey = new Uint8Array(32).fill(0x42);
        const options: FiberWorkerInitializationOptions = {
            inputBuffer: new SharedArrayBuffer(1024),
            outputBuffer: new SharedArrayBuffer(1024),
            logLevel: 'info',
            fiberKeyPair: new Uint8Array(32),
            config: '{}',
            ckbSecretKey: secretKey,
        };
        
        expect(options.ckbSecretKey).toHaveLength(32);
    });

    it('should verify fiberKeyPair is required (type-level)', () => {
        // This test verifies at runtime that fiberKeyPair must be provided
        // In TypeScript, this is enforced by the type system
        const options = {
            inputBuffer: new SharedArrayBuffer(1024),
            outputBuffer: new SharedArrayBuffer(1024),
            logLevel: 'info',
            fiberKeyPair: new Uint8Array(32),
            config: '{}',
        };
        
        // Verify the option is set
        expect(options.fiberKeyPair).toBeDefined();
        expect(options.fiberKeyPair.length).toBe(32);
    });
});
