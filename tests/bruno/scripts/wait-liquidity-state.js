const axios = require("axios");

const DIAGNOSTIC_LIST_LIMIT = 100;
const DIAGNOSTIC_LIST_LIMIT_HEX = "0x64";
const REDACTED = "[REDACTED]";
let rpcRequestSequence = 0;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function redactText(value) {
  return value
    .replace(
      /(Authorization\s*:\s*)(?:[a-z][a-z0-9_-]*\s+)?(?:"[^"]*"|'[^']*'|[^\s,;"'}]+)/gi,
      `$1${REDACTED}`,
    )
    .replace(
      /\b(Bearer|Basic|Token|(?:X-)?Api[-_]?Key)(\s+|\s*:\s*)(?:"[^"]*"|'[^']*'|[^\s,;"'}]+)/gi,
      `$1$2${REDACTED}`,
    )
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^@/\s]+@/gi, `$1${REDACTED}@`)
    .replace(/([?&])([^=&#\s]+)=([^&#\s"'},\]]*)/g, (match, separator, key) => {
      let decodedKey = key;
      try {
        decodedKey = decodeURIComponent(key.replace(/\+/g, " "));
      } catch (_) {
        // Keep malformed query keys visible unless their encoded spelling is recognizably sensitive.
      }
      return isSensitiveQueryKey(decodedKey)
        ? `${separator}${key}=${REDACTED}`
        : match;
    })
    .replace(
      /((?:private.?key|privkey|preimage|payment.?secret|password|passphrase|token|bearer|authorization|api.?key|x-api-key|seed|mnemonic|[a-z0-9]+_secret)["']?\s*[:=]\s*["']?)[^\s,"'}]+/gi,
      `$1${REDACTED}`,
    );
}

function isSensitiveKey(key) {
  const lower = key.toLowerCase();
  const compact = lower.replace(/[-_]/g, "");
  return lower.includes("private")
    || lower.includes("privkey")
    || lower.includes("preimage")
    || lower.includes("password")
    || lower.includes("passphrase")
    || lower.includes("token")
    || lower.includes("bearer")
    || lower.includes("authorization")
    || compact.startsWith("auth")
    || compact === "apikey"
    || compact === "xapikey"
    || (compact.startsWith("api") && /(key|token|auth|secret|bearer)/.test(compact))
    || lower.includes("seed")
    || lower.includes("mnemonic")
    || lower === "settlement_key"
    || lower === "local_key"
    || lower === "client_invoice"
    || lower === "invoice"
    || lower === "invoice_address"
    || lower === "secret"
    || lower.endsWith("_secret");
}

function isSensitiveQueryKey(key) {
  return isSensitiveKey(key) || key.toLowerCase() === "key";
}

function redact(value) {
  if (Array.isArray(value)) {
    return value.map(redact);
  }
  if (value && typeof value === "object") {
    const redacted = {};
    for (const [key, child] of Object.entries(value)) {
      redacted[key] = isSensitiveKey(key) ? REDACTED : redact(child);
    }
    return redacted;
  }
  return typeof value === "string" ? redactText(value) : value;
}

function serializeError(error) {
  return redact({
    name: error && error.name,
    message: error && error.message ? String(error.message) : String(error),
    code: error && error.code,
    status: error && error.response && error.response.status,
    response: error && error.response && error.response.data,
  });
}

async function rpc(url, method, params = [], timeoutMs) {
  if (!url || !method) {
    throw new Error("rpc requires an explicit url and method");
  }

  rpcRequestSequence += 1;
  const requestId = `bruno-liquidity-${rpcRequestSequence}`;
  const response = await axios.post(
    url,
    { id: requestId, jsonrpc: "2.0", method, params },
    timeoutMs === undefined ? undefined : { timeout: timeoutMs },
  );
  const data = response && response.data;

  if (!data || typeof data !== "object" || Array.isArray(data)) {
    throw new Error(`invalid JSON-RPC response for ${method}: expected an object`);
  }
  if (data.jsonrpc !== "2.0") {
    throw new Error(`invalid JSON-RPC response for ${method}: jsonrpc must be 2.0`);
  }
  if (
    !Object.prototype.hasOwnProperty.call(data, "id")
    || !Object.is(data.id, requestId)
  ) {
    const redactedResponse = redact(data);
    const actualId = String(redact(data.id));
    const error = new Error(
      `JSON-RPC response id mismatch for ${method}: expected=${requestId}, actual=${actualId}, response=${JSON.stringify(redactedResponse)}`,
    );
    error.rpcResponse = redactedResponse;
    throw error;
  }
  if (data.error != null) {
    const error = new Error(`JSON-RPC ${method} failed: ${JSON.stringify(redact(data.error))}`);
    error.rpcResponse = redact(data);
    throw error;
  }
  if (!Object.prototype.hasOwnProperty.call(data, "result")) {
    throw new Error(`invalid JSON-RPC response for ${method}: result is missing`);
  }

  return { data: redact(data), result: data.result };
}

function defaultState(result) {
  if (!result || typeof result !== "object" || typeof result.state !== "string") {
    throw new Error("JSON-RPC result does not contain a string state");
  }
  return result.state;
}

async function waitLiquidityState({
  rpcUrl,
  method = "get_swap",
  params = [],
  expectedState,
  getState = defaultState,
  maxAttempts = 90,
  intervalMs = 1000,
  deadlineMs = 90000,
}) {
  if (!Number.isInteger(maxAttempts) || maxAttempts < 1) {
    throw new Error("maxAttempts must be a positive integer");
  }
  if (!Number.isFinite(intervalMs) || intervalMs < 0) {
    throw new Error("intervalMs must be a non-negative number");
  }
  if (!Number.isFinite(deadlineMs) || deadlineMs <= 0) {
    throw new Error("deadlineMs must be a positive number");
  }
  if (typeof getState !== "function") {
    throw new Error("getState must be a function");
  }

  const startedAt = Date.now();
  const deadlineAt = startedAt + deadlineMs;
  let attempts = 0;
  let lastResponse;
  let lastError;

  while (attempts < maxAttempts && Date.now() < deadlineAt) {
    attempts += 1;
    try {
      const remainingMs = Math.max(1, deadlineAt - Date.now());
      const response = await rpc(rpcUrl, method, params, remainingMs);
      lastResponse = response.data;
      lastError = undefined;
      const state = getState(response.result);
      if (Object.is(state, expectedState)) {
        return {
          attempts,
          elapsedMs: Date.now() - startedAt,
          state: redact(state),
          result: redact(response.result),
        };
      }
    } catch (error) {
      lastError = error && error.rpcResponse
        ? { error: serializeError(error), response: error.rpcResponse }
        : serializeError(error);
    }

    const remainingMs = deadlineAt - Date.now();
    if (attempts < maxAttempts && remainingMs > 0) {
      await sleep(Math.min(intervalMs, remainingMs));
    }
  }

  const details = [
    `endpoint=${redact(rpcUrl)}`,
    `method=${method}`,
    `expected_state=${JSON.stringify(redact(expectedState))}`,
    `attempts=${attempts}`,
    `deadline_ms=${deadlineMs}`,
  ];
  if (lastResponse !== undefined) {
    details.push(`last_response=${JSON.stringify(redact(lastResponse))}`);
  }
  if (lastError !== undefined) {
    details.push(`last_error=${JSON.stringify(redact(lastError))}`);
  }
  throw new Error(`liquidity state polling timed out: ${details.join(", ")}`);
}

function waitSwapState({ rpcUrl, swapId, expectedState, ...options }) {
  return waitLiquidityState({
    ...options,
    rpcUrl,
    method: "get_swap",
    params: [{ swap_id: swapId }],
    expectedState,
  });
}

function waitCkbTransactionStatus({ ckbRpcUrl, txHash, expectedStatus, ...options }) {
  return waitLiquidityState({
    ...options,
    rpcUrl: ckbRpcUrl,
    method: "get_transaction",
    params: [txHash],
    expectedState: expectedStatus,
    getState: (result) => result && result.tx_status && result.tx_status.status,
  });
}

function waitCkbLiveCell({
  ckbRpcUrl,
  outPoint,
  expectedStatus = "live",
  withData = true,
  ...options
}) {
  return waitLiquidityState({
    ...options,
    rpcUrl: ckbRpcUrl,
    method: "get_live_cell",
    params: [outPoint, withData],
    expectedState: expectedStatus,
    getState: (result) => result && result.status,
  });
}

async function diagnosticRpc(url, method, params) {
  try {
    const response = await rpc(url, method, params, 10000);
    return { result: redact(response.result) };
  } catch (error) {
    return { error: serializeError(error) };
  }
}

function capDiagnosticList(response, field) {
  const result = response && response.result;
  if (result && Array.isArray(result[field]) && result[field].length > DIAGNOSTIC_LIST_LIMIT) {
    result[field] = result[field].slice(0, DIAGNOSTIC_LIST_LIMIT);
    result.truncated = true;
  }
  return response;
}

async function collectLiquidityDiagnostics({ nodes, listParams = {} } = {}) {
  if (!Array.isArray(nodes) || nodes.length < 2) {
    throw new Error(
      "collectLiquidityDiagnostics requires at least two explicit participating nodes",
    );
  }

  const nodeNames = new Set();
  nodes.forEach((node, index) => {
    const { name, rpcUrl, swapId, paymentHash } = node || {};
    const label = name || `at index ${index}`;
    if (!name || !rpcUrl || !swapId || !paymentHash) {
      const missing = [
        ["name", name],
        ["rpcUrl", rpcUrl],
        ["swapId", swapId],
        ["paymentHash", paymentHash],
      ]
        .filter(([, value]) => !value)
        .map(([field]) => field);
      const missingDescription = missing.length > 2
        ? `${missing.slice(0, -1).join(", ")}, and ${missing[missing.length - 1]}`
        : missing.join(" and ");
      throw new Error(`diagnostics node ${label} requires ${missingDescription}`);
    }
    if (nodeNames.has(name)) {
      throw new Error(`collectLiquidityDiagnostics requires unique node names: ${name}`);
    }
    nodeNames.add(name);
  });

  const boundedListParams = { ...listParams, limit: DIAGNOSTIC_LIST_LIMIT_HEX };
  const diagnostics = await Promise.all(
    nodes.map(async ({ name, rpcUrl, swapId, paymentHash }) => {
      const calls = {
        list_swaps: diagnosticRpc(rpcUrl, "list_swaps", [boundedListParams]),
        get_swap: diagnosticRpc(rpcUrl, "get_swap", [{ swap_id: swapId }]),
        list_liquidity_chain_transactions: diagnosticRpc(
          rpcUrl,
          "list_liquidity_chain_transactions",
          [{ swap_id: swapId }],
        ),
      };
      const paymentParams = [{ payment_hash: paymentHash }];
      calls.get_payment = diagnosticRpc(rpcUrl, "get_payment", paymentParams);
      calls.get_invoice = diagnosticRpc(rpcUrl, "get_invoice", paymentParams);

      const entries = await Promise.all(
        Object.entries(calls).map(async ([method, call]) => [method, await call]),
      );
      const completedCalls = Object.fromEntries(entries);
      capDiagnosticList(completedCalls.list_swaps, "swaps");
      capDiagnosticList(completedCalls.list_liquidity_chain_transactions, "transactions");
      return [name, { rpc_url: rpcUrl, calls: completedCalls }];
    }),
  );

  return redact({ collected_at: new Date().toISOString(), nodes: Object.fromEntries(diagnostics) });
}

async function collectCkbDiagnostics({
  ckbRpcUrl,
  transactionHashes = [],
  outPoints = [],
} = {}) {
  if (!ckbRpcUrl) {
    throw new Error("collectCkbDiagnostics requires ckbRpcUrl");
  }
  if (!Array.isArray(transactionHashes) || !Array.isArray(outPoints)) {
    throw new Error("collectCkbDiagnostics transactionHashes and outPoints must be arrays");
  }
  if (transactionHashes.length === 0 && outPoints.length === 0) {
    throw new Error("collectCkbDiagnostics requires at least one transaction hash or outpoint");
  }
  if (transactionHashes.some((txHash) => !txHash) || outPoints.some((outPoint) => !outPoint)) {
    throw new Error("collectCkbDiagnostics contains an empty transaction hash or outpoint");
  }

  const transactions = await Promise.all(transactionHashes.map(async (txHash) => ({
    tx_hash: txHash,
    response: await diagnosticRpc(ckbRpcUrl, "get_transaction", [txHash]),
  })));
  const liveCells = await Promise.all(outPoints.map(async (outPoint) => ({
    outpoint: outPoint,
    response: await diagnosticRpc(ckbRpcUrl, "get_live_cell", [outPoint, true]),
  })));

  return redact({
    collected_at: new Date().toISOString(),
    ckb_rpc_url: ckbRpcUrl,
    transactions,
    live_cells: liveCells,
  });
}

module.exports = {
  collectCkbDiagnostics,
  collectLiquidityDiagnostics,
  diagnosticRpc,
  redact,
  rpc,
  waitCkbLiveCell,
  waitCkbTransactionStatus,
  waitLiquidityState,
  waitSwapState,
};
