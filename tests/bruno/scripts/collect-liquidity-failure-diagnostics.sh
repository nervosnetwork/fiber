#!/usr/bin/env bash
set -uo pipefail

# Best-effort failure diagnostics collector for the liquidity Bruno suites
# that do not own their own failure diagnostics (the plain matrix entries and
# the existing-state rerun job) and for the suite runners on Bruno failure.
#
# Collects, with short timeouts and hard bounds:
#   - the CKB tip header snapshot,
#   - per node (client node1 / provider node2): the list_swaps response
#     (limit 0x64) and list_liquidity_chain_transactions for at most
#     MAX_CHAIN_TX_SWAPS swap ids harvested from that list response.
#
# The assembled JSON is piped through write-liquidity-diagnostics.sh (which
# redacts sensitive fields) into tests/artifacts/liquidity/<suite>/failure-
# <stamp>.json.
#
# Never raises on RPC or parse errors: unreachable endpoints simply produce
# null sections, and the script exits 0. It must never mask the Bruno exit
# code, so callers may additionally guard it with `|| true`.
#
# Usage: collect-liquidity-failure-diagnostics.sh <suite>
# Environment:
#   NODE1_RPC_URL  (default http://127.0.0.1:21714, the node1 RPC convention)
#   NODE2_RPC_URL  (default http://127.0.0.1:21715, the node2 RPC convention)
#   CKB_RPC_URL    (default http://127.0.0.1:8114)
#   MAX_CHAIN_TX_SWAPS   (default 8, chain-tx fetches per node)
#   CURL_TIMEOUT_SECONDS (default 5, per RPC call)

if [[ "$#" -ne 1 ]]; then
  printf 'usage: %s <suite>\n' "$0" >&2
  exit 2
fi

suite="$1"
if [[ ! "$suite" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  printf 'invalid suite name: %s\n' "$suite" >&2
  exit 2
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
writer="$script_dir/write-liquidity-diagnostics.sh"
if [[ ! -f "$writer" ]]; then
  printf 'missing diagnostics writer: %s\n' "$writer" >&2
  exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
  printf 'missing required command: jq\n' >&2
  exit 1
fi

NODE1_RPC_URL="${NODE1_RPC_URL:-http://127.0.0.1:21714}"
NODE2_RPC_URL="${NODE2_RPC_URL:-http://127.0.0.1:21715}"
CKB_RPC_URL="${CKB_RPC_URL:-http://127.0.0.1:8114}"
MAX_CHAIN_TX_SWAPS="${MAX_CHAIN_TX_SWAPS:-8}"
CURL_TIMEOUT_SECONDS="${CURL_TIMEOUT_SECONDS:-5}"

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/liquidity-failure-diagnostics.XXXXXX")"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

rpc_call() {
  local url="$1" method="$2" params="$3"
  curl -sf --max-time "$CURL_TIMEOUT_SECONDS" -X POST "$url" \
    -H 'Content-Type: application/json' \
    -d "$(jq -cn --arg method "$method" --argjson params "$params" \
      '{id: "liquidity-failure-diagnostics", jsonrpc: "2.0", method: $method, params: $params}')" \
    2>/dev/null || true
}

# Print the raw RPC response if it parses as JSON, else `null`.
json_or_null() {
  local raw="$1"
  if [[ -n "$raw" ]] && printf '%s' "$raw" | jq -e . >/dev/null 2>&1; then
    printf '%s' "$raw"
  else
    printf 'null'
  fi
}

collect_node() {
  local label="$1" url="$2" out="$3"
  local swaps swap_ids swap_id chain_txs params response
  swaps="$(json_or_null "$(rpc_call "$url" list_swaps '[{"limit":"0x64"}]')")"
  swap_ids="$(jq -cn --argjson swaps "$swaps" --argjson max "$MAX_CHAIN_TX_SWAPS" \
    '[($swaps.result.swaps[]? | .swap_id // empty)] | .[0:$max]' 2>/dev/null)" || swap_ids="[]"
  chain_txs="{}"
  while IFS= read -r swap_id; do
    [[ -n "$swap_id" ]] || continue
    params="$(jq -cn --arg id "$swap_id" '[{swap_id: $id}]')"
    response="$(json_or_null "$(rpc_call "$url" list_liquidity_chain_transactions "$params")")"
    chain_txs="$(jq -cn --argjson acc "$chain_txs" --arg id "$swap_id" \
      --argjson response "$response" '$acc + {($id): $response}' 2>/dev/null)" || true
  done < <(jq -r '.[]' <<<"$swap_ids" 2>/dev/null)
  jq -n \
    --arg node "$label" \
    --arg url "$url" \
    --argjson list_swaps "$swaps" \
    --argjson chain_transactions "$chain_txs" \
    '{
      node: $node,
      rpc_url: $url,
      calls: {
        list_swaps: $list_swaps,
        list_liquidity_chain_transactions: $chain_transactions
      }
    }' >"$out" 2>/dev/null \
    || printf '{"node":"%s","rpc_url":"%s","calls":null}' "$label" "$url" >"$out"
}

collect_node client "$NODE1_RPC_URL" "$tmpdir/client.json"
collect_node provider "$NODE2_RPC_URL" "$tmpdir/provider.json"

ckb_tip_header="$(json_or_null "$(rpc_call "$CKB_RPC_URL" get_tip_header '[]')")"

jq -n \
  --arg collected_at "$(date -u +%FT%TZ)" \
  --arg ckb_rpc_url "$CKB_RPC_URL" \
  --argjson ckb_tip_header "$ckb_tip_header" \
  --slurpfile client "$tmpdir/client.json" \
  --slurpfile provider "$tmpdir/provider.json" \
  '{
    collected_at: $collected_at,
    ckb_rpc_url: $ckb_rpc_url,
    ckb_tip_header: $ckb_tip_header,
    nodes: {client: $client[0], provider: $provider[0]}
  }' >"$tmpdir/diagnostics.json" \
  || printf '{"collected_at":"%s","ckb_rpc_url":"%s","ckb_tip_header":null,"nodes":null}' \
    "$(date -u +%FT%TZ)" "$CKB_RPC_URL" >"$tmpdir/diagnostics.json"

stamp="$(date +%Y%m%d-%H%M%S)"
if ! destination="$(bash "$writer" "$suite" "failure-$stamp.json" <"$tmpdir/diagnostics.json")"; then
  printf 'failed to write %s failure diagnostics\n' "$suite" >&2
  exit 1
fi
printf '%s\n' "$destination"
