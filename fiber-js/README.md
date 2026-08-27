# Fiber JS

This is a JavaScript wrapper over Fiber wasm, to make Fiber wasm usable in JavaScript projects.

## Build

In the root of fiber, run:
```
npm install
npm build -ws
```
After that, `fiber-js` will be ready to use in npm.

## APIs

`fiber-js` provide the same API as Fiber RPC, see `fiber-js/src/index.ts` for details. For documentation, please refer to the docs of Fiber RPC.

### Default configuration

Use `fiber.getDefaultConfig(network, ckbRpcUrl)` to get the bundled configuration for `Fiber.start`:

- `network` must be either `"mainnet"` or `"testnet"`.
- `ckbRpcUrl` is the CKB RPC endpoint written to `ckb.rpc_url`.
- The return value is a YAML configuration string.

The returned configuration is the corresponding bundled network configuration with the requested CKB RPC URL.

For external funding:

- `openChannelWithExternalFunding` now uses the peer identity `pubkey` field, consistent with the Fiber RPC.
- `openChannelWithExternalFunding` returns the final unsigned funding transaction after peer tx collaboration has frozen the transaction structure.
- If `funding_lock_script` uses a custom wallet lock, pass `funding_lock_script_cell_deps` so the node can resolve that lock while building the initial unsigned tx.
- The caller should sign that returned transaction once and submit it with `submitSignedFundingTx`.
- `submitSignedFundingTx` must use the same transaction structure and only add witnesses/signatures.

## Example

```js
import { Fiber, randomSecretKey } from "@nervosnetwork/fiber-js";

const fiber = new Fiber();
const config = fiber.getDefaultConfig(
  "testnet",
  "https://testnet.ckbapp.dev/",
);

await fiber.start(
  config,
  randomSecretKey(),
  randomSecretKey(),
  undefined,
  "info",
  "/wasm",
);

console.log(await fiber.invokeCommand("list_peers", []));
```
