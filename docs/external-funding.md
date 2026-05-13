# External Funding

## Signing `unsigned_funding_tx` Externally

`open_channel_with_external_funding` returns the negotiated `unsigned_funding_tx`.
At this point the transaction structure is frozen: external signers only need to sign
the CKB cells they contributed for funding, then submit the same transaction with only
the corresponding `witnesses` filled via `submit_signed_funding_tx`.

Do not rebuild the transaction or modify `inputs`, `outputs`, `outputs_data`, or
`cell_deps` after it is returned.

For Rust developers using secp256k1-sighash locks, refer to the development-only
`sign_external_funding_tx` implementation for how to sign the transaction:
[`crates/fiber-lib/src/rpc/dev.rs:296`](../crates/fiber-lib/src/rpc/dev.rs).
This helper only covers secp256k1 signing. It resolves previous outputs, groups
matching lock scripts, signs each script group, and writes the signature into
`WitnessArgs.lock`.

For JavaScript developers, refer to the pinned `fiber-wallet` `@ckb-ccc/ccc`
example. The main signing flow is
[`signFundingTx`](https://github.com/joii2020/fiber-wallet/blob/77decbbe17ef39f1e443015bae41e74e6dd3960d/apps/src/wallet/manager.ts#L553-L584),
with
[`toCccTransaction`](https://github.com/joii2020/fiber-wallet/blob/77decbbe17ef39f1e443015bae41e74e6dd3960d/apps/src/wallet/transaction.ts#L25-L116)
for JSON-RPC to CCC conversion and
[`withFundingTxWitnesses`](https://github.com/joii2020/fiber-wallet/blob/77decbbe17ef39f1e443015bae41e74e6dd3960d/apps/src/wallet/manager.ts#L293-L312)
for copying signed witnesses back to the original transaction. The example supports
standard CCC signers, OmniLock witness preparation
([`prepareOmniLockWitness`](https://github.com/joii2020/fiber-wallet/blob/77decbbe17ef39f1e443015bae41e74e6dd3960d/apps/src/wallet/manager.ts#L60-L85)),
and JoyID redirect signing
([`prepareJoyIdSignTx`](https://github.com/joii2020/fiber-wallet/blob/77decbbe17ef39f1e443015bae41e74e6dd3960d/apps/src/wallet/manager.ts#L477-L515)).
