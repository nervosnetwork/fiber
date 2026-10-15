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

### Stopping and exporting IndexedDB

`await fiber.stop()` waits for previously accepted API calls, actor shutdown writes,
background database work, and IndexedDB transactions to finish, then closes the
database connection and terminates both workers. After it resolves, exporting the
database needs no additional flush or synchronization for this instance. Other
instances using the same database must also be stopped before exporting.

Calls made after stopping begins are rejected. Repeated calls to `stop()` share
the same result. If startup, shutdown, a worker, or a database operation fails,
`stop()` rejects and releases the workers; that failure must not be treated as a
successful shutdown for backup purposes. Restarting requires a new `Fiber` instance.

The entire stop operation has a 30-second timeout, including waiting for startup
and previously accepted calls. On timeout, both workers are forcibly terminated
and pending calls are rejected. A panic detected by Rust's state check also
rejects `stop()` and triggers the same worker cleanup; it does not leave the
workers running. Neither case guarantees preservation of uncommitted writes.

The shutdown unit tests run with `npm test --workspace @nervosnetwork/fiber-js`.
For the browser regression, install the root npm dependencies and those in
`.github/wasm-test-runner`, then run `make wasm-stop-test`. This also requires
`wasm-bindgen-cli` matching the workspace's locked `wasm-bindgen` version and
Chrome (set `CHROME_BIN` to use an existing installation). The browser test covers
stopping with a pending invoice write, restarting the actual WASM node to read
that invoice, and releasing database connections, without peers or external CKB services.

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

Install `yaml` before running the example: `npm install yaml`.

```js
import { Fiber, randomSecretKey } from "@nervosnetwork/fiber-js";
import { parseDocument } from "yaml";

const fiber = new Fiber();
const config = parseDocument(
  fiber.getDefaultConfig("testnet", "https://testnet.ckbapp.dev/"),
  { schema: "failsafe" },
);

// Browser nodes cannot connect to the TCP bootnodes from the default config.
config.setIn(["fiber", "listening_addr"], "/ip4/127.0.0.1/tcp/8228");
config.setIn(["fiber", "bootnode_addrs"], [
  "/dns4/thrall.fiber.channel/tcp/443/wss/p2p/Qmes1EBD4yNo9Ywkfe6eRw9tG1nVNGLDmMud1xJMsoYFKy",
  "/dns4/onyxia.fiber.channel/tcp/443/wss/p2p/QmdyQWjPtbK4NWWsvy8s69NGJaQULwgeQDT5ZpNDrTNaeV",
]);
config.setIn(["fiber", "announce_listening_addr"], false);

await fiber.start(
  config.toString({ lineWidth: 0 }),
  randomSecretKey(),
  randomSecretKey(),
  undefined,
  "info",
  "/wasm",
);

console.log(await fiber.invokeCommand("list_peers", []));
```
