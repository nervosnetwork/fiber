// Shared publisher for mutated (or valid) liquidity-lock funding transactions
// built by the `tests/liquidity-lock-mutator` helper binary.
//
// The Bruno suites POST mutation requests to the helper's local HTTP sidecar,
// receive a signed transaction plus the lock cell outpoint, submit the
// transaction through the CKB JSON-RPC (`send_transaction`), mine it via
// `generate_epochs`, and wait for the transaction to commit before the
// provider accept call runs. All polling is bounded.

const axios = require("axios");
const { rpc, waitLiquidityState } = require(
  process.cwd() + "/scripts/wait-liquidity-state",
);

const MUTATOR_HTTP_TIMEOUT_MS = 30000;
const MINING_SETTLE_MS = 5000;
const COMMIT_MAX_ATTEMPTS = 240;
const COMMIT_INTERVAL_MS = 1000;
const COMMIT_DEADLINE_MS = 240000;

function assertValidBuild(result) {
  if (!result || typeof result !== "object") {
    throw new Error("liquidity lock mutator returned no JSON object");
  }
  if (result.error) {
    throw new Error(`liquidity lock mutator failed: ${result.error}`);
  }
  if (!result.tx || typeof result.tx !== "object") {
    throw new Error(`mutator response is missing the signed tx: ${JSON.stringify(result)}`);
  }
  if (typeof result.tx_hash !== "string" || !result.tx_hash.startsWith("0x") || result.tx_hash.length !== 66) {
    throw new Error(`mutator response tx_hash must be a 32-byte hex: ${JSON.stringify(result.tx_hash)}`);
  }
  if (!result.outpoint || result.outpoint.tx_hash !== result.tx_hash) {
    throw new Error(`mutator response outpoint must match the tx hash: ${JSON.stringify(result.outpoint)}`);
  }
  return result;
}

// POST one mutation request to the helper sidecar and return the validated
// build {tx, tx_hash, outpoint}.
async function buildMutatedLockTx({ mutatorUrl, request }) {
  if (!mutatorUrl || !request || typeof request !== "object") {
    throw new Error("buildMutatedLockTx requires mutatorUrl and a request object");
  }
  const response = await axios.post(mutatorUrl, request, {
    timeout: MUTATOR_HTTP_TIMEOUT_MS,
  });
  return assertValidBuild(response && response.data);
}

// Wait (bounded) until a submitted CKB transaction is committed.
function waitCkbTransactionCommitted({ ckbRpcUrl, txHash }) {
  return waitLiquidityState({
    rpcUrl: ckbRpcUrl,
    method: "get_transaction",
    params: [txHash],
    expectedState: "committed",
    getState: (result) => (result && result.tx_status && result.tx_status.status) || "unknown",
    maxAttempts: COMMIT_MAX_ATTEMPTS,
    intervalMs: COMMIT_INTERVAL_MS,
    deadlineMs: COMMIT_DEADLINE_MS,
  });
}

// Publish an already built (signed) lock transaction: send it through the
// CKB JSON-RPC, mine the configured number of epochs, and wait for the
// transaction to commit. Returns the outpoint of the lock cell (output 0).
async function publishSignedLockTx({ ckbRpcUrl, built, generateEpochs = "0x2" }) {
  if (!ckbRpcUrl || !built || !built.tx) {
    throw new Error("publishSignedLockTx requires ckbRpcUrl and a built response");
  }
  const sent = await rpc(ckbRpcUrl, "send_transaction", [built.tx, null]);
  if (sent.result !== built.tx_hash) {
    throw new Error(
      `send_transaction must return the mutator tx hash ${built.tx_hash}, got: ${JSON.stringify(sent.result)}`,
    );
  }
  await rpc(ckbRpcUrl, "generate_epochs", [generateEpochs]);
  await new Promise((resolve) => setTimeout(resolve, MINING_SETTLE_MS));
  const committed = await waitCkbTransactionCommitted({
    ckbRpcUrl,
    txHash: built.tx_hash,
  });
  return {
    txHash: built.tx_hash,
    outpoint: built.outpoint,
    attempts: committed.attempts,
  };
}

// Build and publish in one step (used when the request script drives the
// whole flow, e.g. for the multi-variant data length case).
async function publishLockTx({ ckbRpcUrl, mutatorUrl, request, generateEpochs }) {
  const built = await buildMutatedLockTx({ mutatorUrl, request });
  return publishSignedLockTx({ ckbRpcUrl, built, generateEpochs });
}

// Assert that a rejected provider accept left no trace: the quote is not
// persisted as a swap, the client invoice is still Open, and the provider
// holds no payment record for the invoice payment hash.
async function assertQuoteUnconsumed({ quoteId, paymentHash, providerRpcUrl, clientRpcUrl }) {
  if (!quoteId || !paymentHash || !providerRpcUrl || !clientRpcUrl) {
    throw new Error(
      "assertQuoteUnconsumed requires quoteId, paymentHash, providerRpcUrl and clientRpcUrl",
    );
  }
  const swap = await rpc(providerRpcUrl, "get_swap", [{ swap_id: quoteId }]);
  if (swap.result !== null && swap.result !== undefined) {
    throw new Error(
      `rejected accept must not persist a swap for the quote: ${JSON.stringify(swap.result)}`,
    );
  }
  const invoice = await rpc(clientRpcUrl, "get_invoice", [{ payment_hash: paymentHash }]);
  if (!invoice.result || !invoice.result.invoice) {
    throw new Error(
      `client invoice for ${paymentHash} not found after the rejected accept: ${JSON.stringify(invoice.result)}`,
    );
  }
  if (invoice.result.status !== "Open") {
    throw new Error(
      `client invoice must remain Open after the rejected accept, got: ${invoice.result.status}`,
    );
  }
  const payments = await rpc(providerRpcUrl, "list_payments", [{ limit: "0x190" }]);
  const payments_ = (payments.result && payments.result.payments) || [];
  const match = payments_.find((payment) => payment.payment_hash === paymentHash);
  if (match) {
    throw new Error(
      `provider must not hold a payment record for the invoice payment hash: ${JSON.stringify(match)}`,
    );
  }
}

module.exports = {
  assertQuoteUnconsumed,
  buildMutatedLockTx,
  publishLockTx,
  publishSignedLockTx,
  waitCkbTransactionCommitted,
};
