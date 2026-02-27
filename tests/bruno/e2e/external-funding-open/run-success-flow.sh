#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/../../../.." && pwd)"
ENV_FILE="${1:-$ROOT_DIR/tests/bruno/environments/test.bru}"

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command: $1" >&2
    exit 1
  }
}

require_cmd curl
require_cmd node
require_cmd npm
require_cmd ckb-cli

if [[ ! -f "$ENV_FILE" ]]; then
  echo "Environment file not found: $ENV_FILE" >&2
  exit 1
fi

get_env_var() {
  local key="$1"
  awk -F': ' -v k="$key" '$1 ~ "^[[:space:]]*"k"$" {gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2); print $2; exit}' "$ENV_FILE"
}

CKB_RPC_URL="$(get_env_var CKB_RPC_URL)"
NODE1_RPC_URL="$(get_env_var NODE1_RPC_URL)"
NODE2_RPC_URL="$(get_env_var NODE2_RPC_URL)"
NODE3_RPC_URL="$(get_env_var NODE3_RPC_URL)"
NODE3_ADDR="$(get_env_var NODE3_ADDR)"
NODE3_PEERID="$(get_env_var NODE3_PEERID)"

if [[ -z "${CKB_RPC_URL}" || -z "${NODE1_RPC_URL}" || -z "${NODE2_RPC_URL}" || -z "${NODE3_RPC_URL}" || -z "${NODE3_ADDR}" || -z "${NODE3_PEERID}" ]]; then
  echo "Failed to parse required variables from $ENV_FILE" >&2
  exit 1
fi

NODE2_KEY_FILE="$ROOT_DIR/tests/nodes/2/ckb/plain_key"
SIGNER_SCRIPT="$ROOT_DIR/.vscode/dbg-tools/utils/sign-openchannel-response.mjs"

if [[ ! -f "$NODE2_KEY_FILE" ]]; then
  echo "Missing node2 key file: $NODE2_KEY_FILE" >&2
  exit 1
fi
if [[ ! -f "$SIGNER_SCRIPT" ]]; then
  echo "Missing signer script: $SIGNER_SCRIPT" >&2
  exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

rpc_post() {
  local url="$1"
  local payload_file="$2"
  curl -sS -X POST -H "Content-Type: application/json" --data @"$payload_file" "$url"
}

echo "[external-funding-open] connect node1 -> node3"
cat >"$TMP_DIR/connect.json" <<EOF
{"id":"42","jsonrpc":"2.0","method":"connect_peer","params":[{"address":"$NODE3_ADDR"}]}
EOF
rpc_post "$NODE1_RPC_URL" "$TMP_DIR/connect.json" >"$TMP_DIR/connect.resp.json"

echo "[external-funding-open] get node2 funding script"
cat >"$TMP_DIR/node2info.json" <<'EOF'
{"id":"42","jsonrpc":"2.0","method":"node_info","params":[]}
EOF
rpc_post "$NODE2_RPC_URL" "$TMP_DIR/node2info.json" >"$TMP_DIR/node2info.resp.json"
NODE2_SCRIPT_JSON="$(node -e 'const x=require("fs").readFileSync(process.argv[1],"utf8"); const j=JSON.parse(x); if(j.error){throw new Error(j.error.message)}; process.stdout.write(JSON.stringify(j.result.default_funding_lock_script));' "$TMP_DIR/node2info.resp.json")"

build_open_payload() {
  local amount_hex="$1"
  cat >"$TMP_DIR/open.json" <<EOF
{
  "id":"42",
  "jsonrpc":"2.0",
  "method":"open_channel_with_external_funding",
  "params":[{
    "peer_id":"$NODE3_PEERID",
    "funding_amount":"$amount_hex",
    "public":true,
    "shutdown_script":$NODE2_SCRIPT_JSON,
    "funding_lock_script":$NODE2_SCRIPT_JSON
  }]
}
EOF
}

attempt_open() {
  local amount_hex="$1"
  echo "[external-funding-open] open_channel_with_external_funding amount=$amount_hex"
  build_open_payload "$amount_hex"
  rpc_post "$NODE1_RPC_URL" "$TMP_DIR/open.json" >"$TMP_DIR/open.resp.json"
  local result
  result="$(node -e '
const fs = require("fs");
const p = process.argv[1];
const j = JSON.parse(fs.readFileSync(p, "utf8"));
if (j.error) {
  process.stdout.write(`ERR:${j.error.message || JSON.stringify(j.error)}`);
  process.exit(0);
}
if (!j.result?.channel_id || !j.result?.unsigned_funding_tx) {
  process.stdout.write("ERR:missing channel_id or unsigned_funding_tx");
  process.exit(0);
}
process.stdout.write("OK");
' "$TMP_DIR/open.resp.json")"
  if [[ "$result" == "OK" ]]; then
    return 0
  fi
  echo "[external-funding-open] $result"
  return 1
}

if [[ -n "${EXTERNAL_FUNDING_AMOUNT:-}" ]]; then
  FUNDING_AMOUNT="$EXTERNAL_FUNDING_AMOUNT"
  echo "[external-funding-open] using fixed funding_amount=$FUNDING_AMOUNT"
  attempt_open "$FUNDING_AMOUNT" || {
    echo "[external-funding-open] fixed funding amount failed; try a lower EXTERNAL_FUNDING_AMOUNT" >&2
    exit 1
  }
else
  # Auto-decrease funding amount until open succeeds.
  CANDIDATE_AMOUNTS=(
    0x8bb2c9700  # 375 CKB
    0x6fc23ac00  # 300 CKB
    0x5d21dba00  # 250 CKB
    0x4a817c800  # 200 CKB
    0x37e11d600  # 150 CKB
    0x2540be400  # 100 CKB
    0x12a05f200  # 50 CKB
  )
  OPEN_OK=0
  for amount in "${CANDIDATE_AMOUNTS[@]}"; do
    if attempt_open "$amount"; then
      FUNDING_AMOUNT="$amount"
      OPEN_OK=1
      break
    fi
  done
  if [[ "$OPEN_OK" -ne 1 ]]; then
    echo "[external-funding-open] failed to open external funding channel for all candidate amounts" >&2
    exit 1
  fi
  echo "[external-funding-open] selected funding_amount=$FUNDING_AMOUNT"
fi

NODE2_PRIVKEY_RAW="$(tr -d '\n\r' < "$NODE2_KEY_FILE")"
NODE2_PRIVKEY="0x${NODE2_PRIVKEY_RAW#0x}"
echo "[external-funding-open] sign unsigned funding tx with node2 key"
CKB_RPC_URL="$CKB_RPC_URL" node "$SIGNER_SCRIPT" "$(cat "$TMP_DIR/open.resp.json")" "$NODE2_PRIVKEY" >"$TMP_DIR/signed_submit_params.json"

CHANNEL_ID="$(node -e 'const x=require("fs").readFileSync(process.argv[1],"utf8"); const j=JSON.parse(x); process.stdout.write(j.channel_id);' "$TMP_DIR/signed_submit_params.json")"
SIGNED_TX_JSON="$(node -e 'const x=require("fs").readFileSync(process.argv[1],"utf8"); const j=JSON.parse(x); process.stdout.write(JSON.stringify(j.signed_funding_tx));' "$TMP_DIR/signed_submit_params.json")"
UNSIGNED_TX_JSON="$(node -e 'const x=require("fs").readFileSync(process.argv[1],"utf8"); const j=JSON.parse(x); process.stdout.write(JSON.stringify(j.result.unsigned_funding_tx));' "$TMP_DIR/open.resp.json")"

echo "[external-funding-open] run bruno collection with preopen signed tx"
(
  cd "$ROOT_DIR/tests/bruno"
  npm exec -- @usebruno/cli@1.20.0 run e2e/external-funding-open -r --env test \
    --env-var EXTERNAL_FUNDING_USE_PREOPEN=1 \
    --env-var EXTERNAL_FUNDING_PREOPEN_CHANNEL_ID="$CHANNEL_ID" \
    --env-var EXTERNAL_FUNDING_PREOPEN_UNSIGNED_TX="$UNSIGNED_TX_JSON" \
    --env-var EXTERNAL_FUNDING_SIGNED_TX="$SIGNED_TX_JSON"
)
