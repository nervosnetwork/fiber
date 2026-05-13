#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd -- "$script_dir/../../../.." &>/dev/null && pwd)"

TEST_ENV="${TEST_ENV:-debug}"
EXTERNAL_FUNDING_PRIVKEY="${EXTERNAL_FUNDING_PRIVKEY:-0x85af6ff21ea891dbb384b771e02317427e7b66e84b4516c03d74ca4fd5ad0500}"
FUNDING_AMOUNT_HEX="${FUNDING_AMOUNT_HEX:-0xba43b7400}"

CKB_RPC_URL="${CKB_RPC_URL:-http://127.0.0.1:8114}"
NODE1_RPC_URL="${NODE1_RPC_URL:-http://127.0.0.1:21714}"
NODE2_RPC_URL="${NODE2_RPC_URL:-http://127.0.0.1:21715}"
NODE3_RPC_URL="${NODE3_RPC_URL:-http://127.0.0.1:21716}"
NODE3_ADDR="${NODE3_ADDR:-/ip4/127.0.0.1/tcp/8346/p2p/QmaFDJb9CkMrXy7nhTWBY5y9mvuykre3EzzRsCJUAVXprZ}"
NODE1_PUBKEY="${NODE1_PUBKEY:-02a64b8993f33b2ebd37a4de1c9441f491291a4e779da8e519bcfb7c1f3f56c9c0}"
NODE3_PUBKEY="${NODE3_PUBKEY:-03032b99943822e721a651c5a5b9621043017daa9dc3ec81d83215fd2e25121187}"

log_dir="$script_dir/logs"
mkdir -p "$log_dir"
node1_log="$log_dir/external-funding-open-node1-restart.log"

restarted_node1_pid=""
channel_id=""
shutdown_tx_hash=""

log() {
  printf '[run-restart-test] %s\n' "$*"
}

cleanup() {
  local exit_code=$?
  trap - EXIT
  set +e

  if [[ -n "${restarted_node1_pid:-}" ]] && kill -0 "$restarted_node1_pid" 2>/dev/null; then
    kill "$restarted_node1_pid" 2>/dev/null || true
    wait "$restarted_node1_pid" 2>/dev/null || true
  fi

  if [[ "$exit_code" -eq 0 ]]; then
    log "success"
  else
    log "failed"
    log "node1 restart log: $node1_log"
  fi

  exit "$exit_code"
}

trap cleanup EXIT

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

rpc_call() {
  local url="$1"
  local method="$2"
  local params_json="$3"
  local payload
  payload="$(jq -cn --arg method "$method" --argjson params "$params_json" \
    '{id:"42", jsonrpc:"2.0", method:$method, params:$params}')"
  curl -fsS "$url" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json' \
    --data "$payload"
}

rpc_result() {
  local url="$1"
  local method="$2"
  local params_json="$3"
  local response
  response="$(rpc_call "$url" "$method" "$params_json")"
  if jq -e '.error != null' >/dev/null <<<"$response"; then
    echo "$response" | jq . >&2
    echo "rpc failed: $method" >&2
    exit 1
  fi
  echo "$response"
}

wait_for_rpc() {
  local url="$1"
  local label="${2:-$1}"
  local attempt
  for ((attempt = 1; attempt <= 180; attempt++)); do
    if rpc_call "$url" "node_info" '[]' >/dev/null 2>&1; then
      return 0
    fi
    if (( attempt % 10 == 0 )); then
      log "waiting for $label rpc... (${attempt}s)"
    fi
    sleep 1
  done
  echo "rpc not ready: $url" >&2
  exit 1
}

connect_node1_to_node3() {
  local response
  local error
  local attempt

  for ((attempt = 1; attempt <= 10; attempt++)); do
    response="$(rpc_call "$NODE1_RPC_URL" "connect_peer" "[{\"address\":\"$NODE3_ADDR\"}]")"
    error="$(jq -r '.error.message // empty' <<<"$response")"
    if [[ -z "$error" || "$error" == *"Peer already connected"* ]]; then
      sleep 1
      return 0
    fi
    sleep 1
  done

  echo "$response" | jq . >&2
  exit 1
}

generate_epoch() {
  rpc_result "$CKB_RPC_URL" "generate_epochs" '["0x1"]' >/dev/null
  sleep 5
}

wait_channel_state() {
  local expected="$1"
  local include_closed="$2"
  local response
  local state_name
  local attempt

  for ((attempt = 1; attempt <= 10; attempt++)); do
    response="$(rpc_result "$NODE3_RPC_URL" "list_channels" \
      "[{\"pubkey\":\"$NODE1_PUBKEY\",\"include_closed\":$include_closed}]")"
    state_name="$(jq -r --arg channel_id "$channel_id" \
      '.result.channels[]? | select(.channel_id == $channel_id) | .state.state_name // empty' \
      <<<"$response" | head -n1)"
    if [[ "$state_name" == "$expected" ]]; then
      if [[ "$expected" == "Closed" ]]; then
        shutdown_tx_hash="$(jq -r --arg channel_id "$channel_id" \
          '.result.channels[]? | select(.channel_id == $channel_id) | .shutdown_transaction_hash // empty' \
          <<<"$response" | head -n1)"
      fi
      return 0
    fi
    generate_epoch
  done

  echo "channel $channel_id did not reach $expected" >&2
  exit 1
}

start_node1_after_restart() {
  (
    cd "$repo_root/tests/nodes" || exit 1
    FIBER_SECRET_KEY_PASSWORD='password1' \
      LOG_PREFIX='[node 1]' \
      ../../target/"${TEST_ENV}"/fnn -d 1
  ) >"$node1_log" 2>&1 &
  restarted_node1_pid=$!
}

require_cmd curl
require_cmd jq
require_cmd pkill

log "expecting existing environment from:"
log "  REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/external-funding-open"

wait_for_rpc "$NODE1_RPC_URL" "node1"
wait_for_rpc "$NODE2_RPC_URL" "node2"
wait_for_rpc "$NODE3_RPC_URL" "node3"

log "connecting node1 to node3"
connect_node1_to_node3

log "loading node2 funding script"
node2_script="$(rpc_result "$NODE2_RPC_URL" "node_info" '[]' | jq -c '.result.default_funding_lock_script')"

log "opening external-funded channel"
open_params="$(jq -cn \
  --arg pubkey "$NODE3_PUBKEY" \
  --arg funding_amount "$FUNDING_AMOUNT_HEX" \
  --argjson shutdown_script "$node2_script" \
  --argjson funding_lock_script "$node2_script" \
  '[{
    pubkey: $pubkey,
    funding_amount: $funding_amount,
    public: true,
    shutdown_script: $shutdown_script,
    funding_lock_script: $funding_lock_script
  }]')"
open_response="$(rpc_result "$NODE1_RPC_URL" "open_channel_with_external_funding" "$open_params")"
channel_id="$(jq -r '.result.channel_id' <<<"$open_response")"
unsigned_tx="$(jq -c '.result.unsigned_funding_tx' <<<"$open_response")"

log "signing external funding tx"
sign_params="$(jq -cn \
  --argjson unsigned_funding_tx "$unsigned_tx" \
  --arg private_key "$EXTERNAL_FUNDING_PRIVKEY" \
  '[{
    unsigned_funding_tx: $unsigned_funding_tx,
    private_key: $private_key
  }]')"
signed_tx="$(rpc_result "$NODE1_RPC_URL" "sign_external_funding_tx" "$sign_params" | jq -c '.result.signed_funding_tx')"

log "restarting node1"
pkill -f "/target/${TEST_ENV}/fnn -d 1"
sleep 2
start_node1_after_restart
wait_for_rpc "$NODE1_RPC_URL" "node1 after restart"
connect_node1_to_node3

log "submitting signed funding tx"
submit_params="$(jq -cn \
  --arg channel_id "$channel_id" \
  --argjson signed_funding_tx "$signed_tx" \
  '[{
    channel_id: $channel_id,
    signed_funding_tx: $signed_funding_tx
  }]')"
submit_response="$(rpc_result "$NODE1_RPC_URL" "submit_signed_funding_tx" "$submit_params")"
funding_tx_hash="$(jq -r '.result.funding_tx_hash' <<<"$submit_response")"
[[ -n "$funding_tx_hash" && "$funding_tx_hash" != "null" ]] || {
  echo "submit_signed_funding_tx did not return funding_tx_hash" >&2
  exit 1
}

log "waiting for ChannelReady"
wait_channel_state "ChannelReady" false

log "shutting down channel"
shutdown_params="$(jq -cn \
  --arg channel_id "$channel_id" \
  --argjson close_script "$node2_script" \
  '[{
    channel_id: $channel_id,
    close_script: $close_script,
    fee_rate: "0x3FC"
  }]')"
rpc_result "$NODE1_RPC_URL" "shutdown_channel" "$shutdown_params" >/dev/null

log "waiting for Closed"
wait_channel_state "Closed" true
[[ -n "$shutdown_tx_hash" && "$shutdown_tx_hash" != "null" ]] || {
  echo "closed channel missing shutdown tx hash" >&2
  exit 1
}

log "done: channel_id=$channel_id funding_tx_hash=$funding_tx_hash shutdown_tx_hash=$shutdown_tx_hash"
