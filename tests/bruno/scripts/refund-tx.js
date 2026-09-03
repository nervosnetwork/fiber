// Shared helpers for the real liquidity refund E2E suite.
//
// The suite drives the `tests/liquidity-lock-mutator` sidecar in its
// `"kind": "refund"` mode: the sidecar builds the exact runtime-shaped Loop
// In client refund transaction (`build_loop_in_client_refund_transaction`)
// for a confirmed liquidity-lock cell without needing any private key (the
// liquidity-lock refund path requires no secp256k1 signature). The suite
// then exercises the CKB `since` semantics: the one-second-before-maturity
// variant must be rejected by the tx pool as immature, and the exact-since
// variant must commit after the median block time passes the maturity.

const axios = require("axios");
const {
  rpc,
  waitLiquidityState,
} = require(process.cwd() + "/scripts/wait-liquidity-state");

const MUTATOR_HTTP_TIMEOUT_MS = 30000;
const MINING_SETTLE_MS = 5000;
const COMMIT_MAX_ATTEMPTS = 240;
const COMMIT_INTERVAL_MS = 1000;
const COMMIT_DEADLINE_MS = 240000;

// CKB since absolute-timestamp layout (RFC 0017 + ckb-sdk constants):
// bit 63 relative flag, bits 61-62 metric flag (0x40 = timestamp), bits
// 56-60 remain zero, bits 0-55 the unix-seconds value.
const SINCE_METRIC_TIMESTAMP = BigInt("0x4000000000000000");
const SINCE_REMAIN_FLAGS_MASK = BigInt("0x1f00000000000000");
const SINCE_RELATIVE_FLAG = BigInt("0x8000000000000000");
const SINCE_VALUE_MASK = BigInt("0x00ffffffffffffff");

// The refund maturity is the quote expiry itself for Loop In quotes
// (`build_loop_in_quote_terms` stores `absolute_timestamp_since(expires_at)`),
// requested with a 60 second `expires_after_seconds` so the pre-maturity
// rejection stays deterministic even on slow runners.
const QUOTE_EXPIRES_AFTER_SECONDS = "0x3c";

// Median block time is the median of the past 37 block timestamps
// (`median_time_block_count`): generating 4 dev-chain epochs (40 blocks)
// after wall clock passed the maturity always fills the window.
const MATURITY_MEDIAN_BLOCK_COUNT = 37;
const MATURITY_GENESIS_EPOCH_LENGTH = 10;
const MATURITY_EPOCHS_PER_GENERATION = "0x1";
const MATURITY_MAX_GENERATED_EPOCHS = 12;
const MATURITY_POLL_ATTEMPTS = 60;
const MATURITY_POLL_INTERVAL_MS = 2000;

function assertValidRefundBuild(built) {
  if (!built || typeof built !== "object") {
    throw new Error("refund sidecar returned no JSON object");
  }
  if (built.error) {
    throw new Error(`refund sidecar failed: ${built.error}`);
  }
  if (!built.tx || typeof built.tx !== "object") {
    throw new Error(
      `refund sidecar response is missing the built tx: ${JSON.stringify(built)}`,
    );
  }
  if (
    typeof built.tx_hash !== "string"
    || !built.tx_hash.startsWith("0x")
    || built.tx_hash.length !== 66
  ) {
    throw new Error(
      `refund sidecar tx_hash must be a 32-byte hex: ${JSON.stringify(built)}`,
    );
  }
  if (!built.outpoint || typeof built.outpoint.tx_hash !== "string") {
    throw new Error(
      `refund sidecar must report the spent lock outpoint: ${JSON.stringify(built)}`,
    );
  }
  return built;
}

// POST one refund request to the mutator sidecar and return the validated
// build {tx, tx_hash, outpoint} where outpoint is the spent lock cell.
async function buildRefundTx({ mutatorUrl, request }) {
  if (!mutatorUrl || !request || typeof request !== "object") {
    throw new Error("buildRefundTx requires mutatorUrl and a request object");
  }
  const response = await axios.post(mutatorUrl, request, {
    timeout: MUTATOR_HTTP_TIMEOUT_MS,
  });
  return assertValidRefundBuild(response && response.data);
}

// Decode an encoded absolute-timestamp since into its unix-seconds value,
// rejecting any non-timestamp metric or stray flag bits the runtime would
// never produce (`absolute_timestamp_since`).
function decodeSinceSeconds(sinceHex) {
  const since = BigInt(sinceHex);
  if (since & SINCE_RELATIVE_FLAG) {
    throw new Error(`since must be absolute: ${sinceHex}`);
  }
  if ((since & BigInt("0x6000000000000000")) !== SINCE_METRIC_TIMESTAMP) {
    throw new Error(`since must use the timestamp metric: ${sinceHex}`);
  }
  if (since & SINCE_REMAIN_FLAGS_MASK) {
    throw new Error(`since must not set reserved flag bits: ${sinceHex}`);
  }
  return since & SINCE_VALUE_MASK;
}

// Wait (bounded) until CKB reports a tip block whose median time covers the
// refund maturity. Advancing is driven by `generate_epochs` generation and
// every check goes through CKB RPC (`get_tip_header` timestamps and
// `get_block_median_time`) rather than a fixed sleep.
async function waitRefundMaturity({ ckbRpcUrl, maturitySeconds }) {
  if (!ckbRpcUrl || maturitySeconds === undefined) {
    throw new Error("waitRefundMaturity requires ckbRpcUrl and maturitySeconds");
  }
  const maturity = BigInt(maturitySeconds);
  let generatedEpochs = 0;
  let lastTipSeconds = 0n;
  let lastMedianSeconds;

  for (let attempt = 0; attempt < MATURITY_POLL_ATTEMPTS; attempt++) {
    const tip = await rpc(ckbRpcUrl, "get_tip_header", []);
    if (!tip || typeof tip.hash !== "string" || tip.timestamp === undefined) {
      throw new Error(`get_tip_header returned no usable header: ${JSON.stringify(tip)}`);
    }
    lastTipSeconds = BigInt(tip.timestamp) / BigInt(1000);
    if (lastTipSeconds >= maturity) {
      lastMedianSeconds = await rpc(ckbRpcUrl, "get_block_median_time", [tip.hash]);
      if (lastMedianSeconds === undefined) {
        throw new Error(
          `get_block_median_time returned no median: ${JSON.stringify(lastMedianSeconds)}`,
        );
      }
      if (BigInt(lastMedianSeconds) >= maturity) {
        return { tip, medianSeconds: lastMedianSeconds, generatedEpochs, attempts: attempt + 1 };
      }
    }
    if (generatedEpochs >= MATURITY_MAX_GENERATED_EPOCHS) {
      throw new Error(
        `refund maturity ${maturity} not reached after generating ${generatedEpochs} epochs: `
          + `tip timestamp ${tip.timestamp}, median ${lastMedianSeconds}`,
      );
    }
    await rpc(ckbRpcUrl, "generate_epochs", [MATURITY_EPOCHS_PER_GENERATION]);
    generatedEpochs += MATURITY_GENESIS_EPOCH_LENGTH;
    await new Promise((resolve) => setTimeout(resolve, MATURITY_POLL_INTERVAL_MS));
  }
  throw new Error(
    `refund maturity ${maturity} not reached after ${MATURITY_POLL_ATTEMPTS} attempts`,
  );
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

// Mine the configured epochs and wait for the refund transaction to commit.
async function mineAndConfirmRefundTx({ ckbRpcUrl, txHash, generateEpochs = "0x2" }) {
  await rpc(ckbRpcUrl, "generate_epochs", [generateEpochs]);
  await new Promise((resolve) => setTimeout(resolve, MINING_SETTLE_MS));
  return waitCkbTransactionCommitted({ ckbRpcUrl, txHash });
}

module.exports = {
  MINING_SETTLE_MS,
  QUOTE_EXPIRES_AFTER_SECONDS,
  SINCE_METRIC_TIMESTAMP,
  SINCE_VALUE_MASK,
  buildRefundTx,
  decodeSinceSeconds,
  mineAndConfirmRefundTx,
  waitCkbTransactionCommitted,
  waitRefundMaturity,
};
