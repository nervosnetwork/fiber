const assert = require("assert");
const path = require("path");
const Module = require("module");

let responder;
const originalLoad = Module._load;
Module._load = function (request, parent, isMain) {
  if (request === "axios") {
    return { post: (...args) => responder(...args) };
  }
  return originalLoad.call(this, request, parent, isMain);
};

const helpers = require(path.join(__dirname, "..", "wait-liquidity-state.js"));
const fixture = require(path.join(__dirname, "fixtures", "sensitive-diagnostics.json"));

const secretValues = [
  "encoded%40user",
  "p%40ss",
  "query-token-value",
  "query-api-key-value",
  "query-key-value",
  "query-secret-value",
  "query-password-value",
  "query-payment-secret",
  "query-preimage",
  "query-invoice",
  "invoice-address-secret",
  "invoice-object-secret",
  "payment-secret-value",
  "payment-preimage-secret",
  "private-key-value",
  "passphrase-value",
  "authorization-value",
  "auth-header-value",
  "api-key-value",
  "seed-value",
  "mnemonic-value",
  "suffix-secret-value",
];

function response(result, id) {
  return Promise.resolve({ data: { id, jsonrpc: "2.0", result } });
}

function assertNoSecrets(value) {
  const serialized = JSON.stringify(value);
  for (const secret of secretValues) {
    assert.ok(!serialized.includes(secret), `leaked secret: ${secret}`);
  }
  return serialized;
}

(async () => {
  const redactedFixture = helpers.redact(fixture);
  const serializedFixture = assertNoSecrets(redactedFixture);
  assert.ok(serializedFixture.includes("safe=visible"));
  assert.strictEqual(redactedFixture.get_invoice.status, "Open");
  assert.strictEqual(redactedFixture.get_payment.payment_hash, "0xpublic-payment-hash");

  const listSwapParams = [];
  responder = (url, payload) => {
    if (payload.method === "list_swaps") {
      listSwapParams.push(payload.params);
      return response({
        swaps: Array.from({ length: 105 }, (_, index) => ({ swap_id: `0x${index}` })),
        next_cursor: "more-results",
      }, payload.id);
    }
    if (payload.method === "list_liquidity_chain_transactions") {
      return response({
        transactions: Array.from({ length: 105 }, (_, index) => ({ tx_hash: `0x${index}` })),
      }, payload.id);
    }
    if (payload.method === "get_invoice") {
      return response(fixture.get_invoice, payload.id);
    }
    if (payload.method === "get_payment") {
      return response(fixture.get_payment, payload.id);
    }
    return response({ state: "Pending" }, payload.id);
  };

  const diagnostics = await helpers.collectLiquidityDiagnostics({
    nodes: [
      { name: "provider", rpcUrl: "http://node-1", swapId: "0x01", paymentHash: "0x02" },
      { name: "client", rpcUrl: "http://node-2", swapId: "0x03", paymentHash: "0x04" },
    ],
    listParams: { state: "Pending", limit: "0xffff" },
  });

  assertNoSecrets(diagnostics);
  assert.deepStrictEqual(listSwapParams, [
    [{ state: "Pending", limit: "0x64" }],
    [{ state: "Pending", limit: "0x64" }],
  ]);
  assert.strictEqual(diagnostics.nodes.provider.calls.list_swaps.result.swaps.length, 100);
  assert.strictEqual(diagnostics.nodes.provider.calls.list_swaps.result.truncated, true);
  assert.strictEqual(
    diagnostics.nodes.provider.calls.list_liquidity_chain_transactions.result.transactions.length,
    100,
  );
  assert.strictEqual(
    diagnostics.nodes.provider.calls.list_liquidity_chain_transactions.result.truncated,
    true,
  );
  console.log("wait-liquidity-state diagnostics checks passed");
})().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
