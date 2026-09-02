#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
collection_dir="$(cd -- "$script_dir/../../.." &>/dev/null && pwd)"
mock_server="$(mktemp "${TMPDIR:-/tmp}/liquidity-helper-smoke.XXXXXX.js")"
mock_pid=""

cleanup() {
  if [[ -n "$mock_pid" ]] && kill -0 "$mock_pid" 2>/dev/null; then
    kill "$mock_pid" 2>/dev/null || true
    wait "$mock_pid" 2>/dev/null || true
  fi
  rm -f "$mock_server"
}
trap cleanup EXIT

cat >"$mock_server" <<'NODE'
const http = require("http");

const sensitive = require(process.cwd() + "/scripts/fixtures/sensitive-diagnostics.json");

function resultFor(payload) {
  if (payload.method === "list_channels") return { channels: [] };
  if (payload.method === "local_node_info") return { version: "smoke" };
  if (payload.method === "list_swaps") {
    return {
      swaps: Array.from({ length: 105 }, (_, index) => ({ swap_id: `0x${index}` })),
      next_cursor: "more-results",
      received_params: payload.params,
    };
  }
  if (payload.method === "list_liquidity_chain_transactions") {
    return {
      transactions: Array.from({ length: 105 }, (_, index) => ({ tx_hash: `0x${index}` })),
    };
  }
  if (payload.method === "get_invoice") return sensitive.get_invoice;
  if (payload.method === "get_payment") return sensitive.get_payment;
  return { state: "Pending" };
}

function handler(request, response) {
  let body = "";
  request.on("data", (chunk) => { body += chunk; });
  request.on("end", () => {
    const payload = JSON.parse(body);
    response.writeHead(200, { "Content-Type": "application/json" });
    response.end(JSON.stringify({
      id: payload.id,
      jsonrpc: "2.0",
      result: resultFor(payload),
    }));
  });
}

for (const port of [8114, 21714, 21715]) {
  http.createServer(handler).listen(port, "127.0.0.1");
}
NODE

cd "$collection_dir"
node "$mock_server" &
mock_pid=$!
sleep 1
if ! kill -0 "$mock_pid" 2>/dev/null; then
  printf 'liquidity helper mock responder failed to start\n' >&2
  exit 1
fi
npx @usebruno/cli@1.20.0 run e2e/liquidity/helpers-smoke -r --env test
