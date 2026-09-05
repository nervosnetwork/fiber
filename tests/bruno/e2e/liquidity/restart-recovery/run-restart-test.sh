#!/usr/bin/env bash
set -Eeuo pipefail

# Dedicated supervisor for the liquidity restart-recovery Bruno E2E suite.
#
# Unlike the shared tests/nodes/start.sh monitor, this script owns the whole
# process lifecycle: it starts the CKB dev chain and both FNN nodes itself and
# records every spawned PID. It only ever signals PIDs it started - there is
# no `pkill -f`, no pattern-matched process discovery, and no reuse of the
# generic node monitor.
#
# Flow:
#   1. Preflight dependency and port checks (refuses to touch foreign processes).
#   2. Build the fnn binary and the udt-init provisioner.
#   3. Initialize the CKB dev chain (nilpotent; the one-time init provisions
#      the liquidity-enabled node configs itself) and start CKB as an owned
#      PID. On existing chains only the Bruno environment files are refreshed
#      chain-free; node configs are verified, never rewritten, here.
#   4. Start node1 (client) and node2 (provider) as owned PIDs.
#   5. Run the phase1 Bruno suite: setup, channel, quote, accept, execute,
#      and both swaps parked in PayoutPending with the payout broadcast but
#      deliberately not mined.
#   6. Discover the swap identifiers over RPC, gate on the persisted payout
#      broadcast record, and write the phase-handoff artifact.
#   7. Stop node2 (SIGTERM to its own PID, bounded await), restart it with the
#      same data directory and password, and wait for its RPC.
#   8. Run the phase2 Bruno suite with the handoff injected via --env-var:
#      reconnect, mine the payout, complete the payment and claim, and require
#      both swaps to reach Success with the same payout hash and the exact
#      pre-restart channel balance delta.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd -- "$script_dir/../../../../.." &>/dev/null && pwd)"
nodes_dir="$repo_root/tests/nodes"
deploy_dir="$repo_root/tests/deploy"
bruno_dir="$repo_root/tests/bruno"
run_dir="$repo_root/tests/artifacts/liquidity/restart-recovery"
log_dir="$run_dir/logs"
handoff_file="$run_dir/phase-handoff.json"

TEST_ENV="${TEST_ENV:-debug}"
CKB_RPC_URL="${CKB_RPC_URL:-http://127.0.0.1:8114}"
NODE1_RPC_URL="${NODE1_RPC_URL:-http://127.0.0.1:21714}"
NODE2_RPC_URL="${NODE2_RPC_URL:-http://127.0.0.1:21715}"
CKB_PORT="${CKB_PORT:-8114}"
NODE1_P2P_PORT="${NODE1_P2P_PORT:-8344}"
NODE2_P2P_PORT="${NODE2_P2P_PORT:-8345}"
RPC_READY_TIMEOUT="${RPC_READY_TIMEOUT:-120}"
STOP_GRACE_SECONDS="${STOP_GRACE_SECONDS:-30}"

FAILURE_CONTEXT="startup"
OWNED_PIDS=()
CKB_PID=""
NODE1_PID=""
NODE2_PID=""
HANDOFF_SWAP_ID=""
HANDOFF_PAYMENT_HASH=""
RESTART_PAYOUT_TX_HASH=""
HANDOFF_CHANNEL_ID=""
CLIENT_LOCAL_BEFORE=""
CLIENT_REMOTE_BEFORE=""

log() { echo "[restart-recovery] $*"; }

register_pid() {
  OWNED_PIDS+=("$1")
}

# ---------------------------------------------------------------------------
# Cleanup and failure handling: only ever signals registered owned PIDs.
# ---------------------------------------------------------------------------

cleanup() {
  trap - EXIT INT TERM ERR
  local pid deadline=$(( $(date +%s) + 15 ))
  # `${OWNED_PIDS[@]+...}` keeps this safe on macOS bash 3.2, where
  # expanding an empty array under `set -u` would abort the trap.
  for pid in ${OWNED_PIDS[@]+"${OWNED_PIDS[@]}"}; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in ${OWNED_PIDS[@]+"${OWNED_PIDS[@]}"}; do
    while kill -0 "$pid" 2>/dev/null && (( $(date +%s) < deadline )); do
      sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
  done
  for pid in ${OWNED_PIDS[@]+"${OWNED_PIDS[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

collect_failure_diagnostics() {
  # Best-effort: never raises, never signals anything beyond owned PIDs.
  local stamp tmpdir
  stamp="$(date +%Y%m%d-%H%M%S)"
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/restart-recovery-diag.XXXXXX")"
  collect_node_diagnostics client "$NODE1_RPC_URL" "$tmpdir/node1.json"
  collect_node_diagnostics provider "$NODE2_RPC_URL" "$tmpdir/node2.json"
  local ckb_tip="unavailable"
  ckb_tip="$(rpc_call "$CKB_RPC_URL" get_tip_block_number '[]' 2>/dev/null \
    | jq -r '.result // "unavailable"' 2>/dev/null)" || ckb_tip="unavailable"
  [[ -n "$ckb_tip" ]] || ckb_tip="unavailable"
  if jq -n \
      --arg collected_at "$(date -u +%FT%TZ)" \
      --arg ckb_rpc_url "$CKB_RPC_URL" \
      --arg ckb_tip_block_number "$ckb_tip" \
      --slurpfile client "$tmpdir/node1.json" \
      --slurpfile provider "$tmpdir/node2.json" \
      '{
        collected_at: $collected_at,
        ckb_rpc_url: $ckb_rpc_url,
        ckb_tip_block_number: $ckb_tip_block_number,
        nodes: {client: $client[0], provider: $provider[0]}
      }' >"$tmpdir/diagnostics.json" 2>/dev/null; then
    bash "$bruno_dir/scripts/write-liquidity-diagnostics.sh" restart-recovery \
      "failure-$stamp.json" <"$tmpdir/diagnostics.json" 2>/dev/null \
      && log "failure diagnostics written to tests/artifacts/liquidity/restart-recovery/failure-$stamp.json"
  fi
  rm -rf "$tmpdir"
  local log_file
  for log_file in "$log_dir"/*.log; do
    [[ -e "$log_file" ]] || continue
    echo "----- tail of $log_file -----" >&2
    tail -n 40 "$log_file" >&2 || true
  done
  return 0
}

failure() {
  echo "[restart-recovery] FAILURE: $1" >&2
  set +e +E
  trap - ERR
  collect_failure_diagnostics
  exit 1
}

on_error() {
  local exit_code=$?
  set +e +E
  trap - ERR
  echo "[restart-recovery] unexpected command failure (exit $exit_code) while: $FAILURE_CONTEXT" >&2
  collect_failure_diagnostics
  exit "$exit_code"
}
trap on_error ERR

# ---------------------------------------------------------------------------
# Bounded wait and RPC helpers.
# ---------------------------------------------------------------------------

wait_until() {
  local description="$1" timeout_seconds="$2"
  shift 2
  local deadline=$(( $(date +%s) + timeout_seconds ))
  while ! "$@" >/dev/null 2>&1; do
    if (( $(date +%s) >= deadline )); then
      echo "timed out after ${timeout_seconds}s waiting for: $description" >&2
      return 1
    fi
    sleep 1
  done
}

rpc_call() {
  local url="$1" method="$2" params="$3"
  curl -sf --max-time 10 -X POST "$url" -H 'Content-Type: application/json' \
    -d "$(jq -cn --arg method "$method" --argjson params "$params" \
      '{id: "restart-recovery-supervisor", jsonrpc: "2.0", method: $method, params: $params}')"
}

rpc_ready() {
  rpc_call "$1" node_info '[]' >/dev/null
}

port_busy() {
  nc -z 127.0.0.1 "$1" >/dev/null 2>&1
}

port_free() {
  ! port_busy "$1"
}

process_gone() {
  ! kill -0 "$1" 2>/dev/null
}

collect_node_diagnostics() {
  local label="$1" url="$2" out="$3"
  local swaps="null" swap="null" transactions="null" payment="null" invoice="null"
  swaps="$(rpc_call "$url" list_swaps '[{"limit":"0x64"}]' 2>/dev/null)" || swaps="null"
  [[ -n "$swaps" ]] || swaps="null"
  if [[ -n "$HANDOFF_SWAP_ID" ]]; then
    swap="$(rpc_call "$url" get_swap "{\"swap_id\":\"$HANDOFF_SWAP_ID\"}" 2>/dev/null)" || swap="null"
    [[ -n "$swap" ]] || swap="null"
    transactions="$(rpc_call "$url" list_liquidity_chain_transactions "{\"swap_id\":\"$HANDOFF_SWAP_ID\"}" 2>/dev/null)" || transactions="null"
    [[ -n "$transactions" ]] || transactions="null"
  fi
  if [[ -n "$HANDOFF_PAYMENT_HASH" ]]; then
    payment="$(rpc_call "$url" get_payment "{\"payment_hash\":\"$HANDOFF_PAYMENT_HASH\"}" 2>/dev/null)" || payment="null"
    [[ -n "$payment" ]] || payment="null"
    invoice="$(rpc_call "$url" get_invoice "{\"payment_hash\":\"$HANDOFF_PAYMENT_HASH\"}" 2>/dev/null)" || invoice="null"
    [[ -n "$invoice" ]] || invoice="null"
  fi
  jq -n \
    --arg node "$label" \
    --arg url "$url" \
    --argjson swaps "$swaps" \
    --argjson get_swap "$swap" \
    --argjson transactions "$transactions" \
    --argjson get_payment "$payment" \
    --argjson get_invoice "$invoice" \
    '{
      node: $node,
      rpc_url: $url,
      calls: {
        list_swaps: $swaps,
        get_swap: $get_swap,
        list_liquidity_chain_transactions: $transactions,
        get_payment: $get_payment,
        get_invoice: $get_invoice
      }
    }' >"$out" 2>/dev/null || printf '{}' >"$out"
}

# ---------------------------------------------------------------------------
# Process ownership: every PID below is recorded and only ever signaled via
# the registry. There is deliberately no pkill anywhere in this script.
# ---------------------------------------------------------------------------

start_ckb() {
  FAILURE_CONTEXT="starting the CKB dev chain"
  log "starting CKB dev chain (pid recorded, log: $log_dir/ckb.log)"
  ckb run -C "$deploy_dir/node-data" --indexer >>"$log_dir/ckb.log" 2>&1 &
  CKB_PID=$!
  register_pid "$CKB_PID"
}

STARTED_PID=""

start_fnn() {
  local node_id="$1" password="$2" log_file="$3" log_prefix="$4"
  FAILURE_CONTEXT="starting fnn node $node_id"
  (
    cd "$nodes_dir"
    exec env FIBER_SECRET_KEY_PASSWORD="$password" LOG_PREFIX="$log_prefix" \
      "$repo_root/target/$TEST_ENV/fnn" -d "$node_id"
  ) >>"$log_file" 2>&1 &
  # Must run in the caller's shell (no command substitution upstream) so the
  # fnn process stays a direct child and can be reaped with `wait`.
  STARTED_PID=$!
  register_pid "$STARTED_PID"
}

stop_fnn() {
  local pid="$1" description="$2" exit_status=0 watchdog
  if process_gone "$pid"; then
    log "$description (pid $pid) already exited"
    return 0
  fi
  log "stopping $description (pid $pid) with SIGTERM"
  kill -TERM "$pid"
  (
    sleep "$STOP_GRACE_SECONDS"
    kill -KILL "$pid" 2>/dev/null || true
  ) &
  watchdog=$!
  register_pid "$watchdog"
  wait "$pid" || exit_status=$?
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  log "$description (pid $pid) exited with status $exit_status"
}

# ---------------------------------------------------------------------------
# Preflight and provisioning.
# ---------------------------------------------------------------------------

check_dependencies() {
  local command_name
  for command_name in ckb ckb-cli cargo jq curl nc npx node; do
    command -v "$command_name" >/dev/null || {
      echo "missing required command: $command_name" >&2
      exit 1
    }
  done
  local node_major
  node_major="$(node -p 'parseInt(process.versions.node, 10)')"
  if (( node_major < 18 )); then
    echo "node >= 18 is required to run the Bruno CLI 1.20.0 (found: $(node --version))" >&2
    exit 1
  fi
}

check_ports_free() {
  local port description
  for port in "$CKB_PORT" "$NODE1_P2P_PORT" "$NODE2_P2P_PORT" 21714 21715; do
    case "$port" in
      "$CKB_PORT") description="CKB RPC" ;;
      "$NODE1_P2P_PORT") description="node1 P2P" ;;
      "$NODE2_P2P_PORT") description="node2 P2P" ;;
      21714) description="node1 RPC" ;;
      21715) description="node2 RPC" ;;
    esac
    if port_busy "$port"; then
      echo "port $port ($description) is already in use; refusing to start on top of" >&2
      echo "another process. Stop it manually - this supervisor never kills" >&2
      echo "processes it did not start." >&2
      exit 1
    fi
  done
}

build_binaries() {
  FAILURE_CONTEXT="building the fnn binary"
  local build_args=(--locked)
  case "$TEST_ENV" in
    release) build_args+=(--release) ;;
    debug) ;;
    *) build_args+=(--profile "$TEST_ENV") ;;
  esac
  if [[ "$TEST_ENV" != "release" ]]; then
    build_args+=(--features debug-add-tlc)
  fi
  log "building fnn (${build_args[*]})"
  (cd "$repo_root" && cargo build "${build_args[@]}")
  FAILURE_CONTEXT="building the udt-init provisioner"
  log "building udt-init"
  (cd "$deploy_dir/udt-init" && cargo build --locked)
}

ensure_liquidity_module_enabled() {
  local node_dir
  for node_dir in "$nodes_dir/1" "$nodes_dir/2"; do
    if ! grep -q '^[[:space:]]*-[[:space:]]*liquidity[[:space:]]*$' "$node_dir/config.yml" 2>/dev/null; then
      cat >&2 <<EOF
$node_dir/config.yml does not enable the 'liquidity' RPC module.
This supervisor deliberately does not run chain-bound provisioning
(tests/deploy/deploy.sh) against an existing dev chain. To regenerate the
liquidity-enabled node configs, start the dev chain once and run the full
provisioner, then retry:

  ckb run -C $deploy_dir/node-data --indexer &
  $deploy_dir/deploy.sh
  kill %1

or regenerate everything from scratch with:
  REMOVE_OLD_STATE=y ./tests/nodes/start.sh e2e/liquidity/ckb-loop-out
EOF
      exit 1
    fi
  done
}

provision_dev_chain() {
  local fresh_chain="0"
  if [[ ! -d "$deploy_dir/node-data" ]]; then
    fresh_chain="1"
    FAILURE_CONTEXT="initializing the CKB dev chain (one-time)"
    log "dev chain data missing; running tests/deploy/init-dev-chain.sh (one-time)"
    log "(the one-time init provisions contracts, wallets and the liquidity-enabled node configs itself)"
    "$deploy_dir/init-dev-chain.sh"
  elif [[ -z "${SKIP_PROVISIONING:-}" ]]; then
    # Existing chain: the full provisioner (udt-init main mode) funds UDT
    # accounts over live CKB RPC, which is not available before this
    # supervisor starts its own CKB. Refresh only the genesis-derived Bruno
    # environment files, which need no chain connection.
    FAILURE_CONTEXT="refreshing the Bruno environment files (chain-free)"
    log "refreshing Bruno environment files via udt-init GENERATE_BRUNO_ENVIRONMENTS_ONLY (no chain RPC)"
    GENERATE_BRUNO_ENVIRONMENTS_ONLY=1 NODES_DIR="$nodes_dir" \
      "$deploy_dir/udt-init/target/debug/udt-init"
  fi
  FAILURE_CONTEXT="verifying the liquidity-enabled node configs"
  ensure_liquidity_module_enabled
}

resolve_node_endpoints() {
  # CI provisioning may randomize node ports; use the record generated with
  # the node configs before any readiness or RPC checks run.
  source "$bruno_dir/scripts/resolve-node-urls.sh"
  log "resolved node endpoints: node1 rpc=$NODE1_RPC_URL p2p=$NODE1_P2P_PORT, node2 rpc=$NODE2_RPC_URL p2p=$NODE2_P2P_PORT"
}

wait_provisioning_ports_released() {
  local port
  for port in "$CKB_PORT" "$NODE1_P2P_PORT" "$NODE2_P2P_PORT" 21714 21715; do
    FAILURE_CONTEXT="waiting for port $port to be released"
    if ! wait_until "port $port to be released after provisioning" 15 port_free "$port"; then
      failure "port $port is still occupied after provisioning"
    fi
  done
}

# ---------------------------------------------------------------------------
# Phase orchestration.
# ---------------------------------------------------------------------------

run_bruno_phase() {
  local phase_dir="$1"
  shift
  FAILURE_CONTEXT="running the $phase_dir Bruno suite"
  (
    cd "$bruno_dir"
    npx @usebruno/cli@1.20.0 run "e2e/liquidity/restart-recovery/$phase_dir" -r --env test "$@"
  )
}

discover_handoff() {
  FAILURE_CONTEXT="discovering the phase handoff state over RPC"
  local client_swaps candidates count
  client_swaps="$(rpc_call "$NODE1_RPC_URL" list_swaps '[{"limit":"0x64"}]')" || failure "client list_swaps failed"
  candidates="$(jq -ec '[.result.swaps[]? | select(.swap_kind == "loop_out" and .state == "payout_pending")]' <<<"$client_swaps")" \
    || failure "client list_swaps response was not parseable"
  count="$(jq 'length' <<<"$candidates")"
  [[ "$count" -eq 1 ]] || failure "expected exactly one client loop_out swap in payout_pending, found $count"
  HANDOFF_SWAP_ID="$(jq -r '.[0].swap_id' <<<"$candidates")"
  HANDOFF_PAYMENT_HASH="$(jq -r '.[0].payment_hash' <<<"$candidates")"
  log "handoff swap id: $HANDOFF_SWAP_ID payment hash: $HANDOFF_PAYMENT_HASH"

  # The provider must still hold the swap in PayoutPending with the payout
  # broadcast record persisted - that record is exactly what the restarted
  # process resumes from.
  local provider_swap
  provider_swap="$(rpc_call "$NODE2_RPC_URL" get_swap "{\"swap_id\":\"$HANDOFF_SWAP_ID\"}")" \
    || failure "provider get_swap failed"
  jq -e --arg swap_id "$HANDOFF_SWAP_ID" --arg payment_hash "$HANDOFF_PAYMENT_HASH" \
    '.result != null and .result.state == "payout_pending" and .result.swap_id == $swap_id and .result.payment_hash == $payment_hash' \
    <<<"$provider_swap" >/dev/null \
    || failure "provider swap is not in the expected payout_pending state: $provider_swap"

  local provider_transactions payout_record
  provider_transactions="$(rpc_call "$NODE2_RPC_URL" list_liquidity_chain_transactions "{\"swap_id\":\"$HANDOFF_SWAP_ID\"}")" \
    || failure "provider list_liquidity_chain_transactions failed"
  payout_record="$(jq -ec '[.result.transactions[]? | select(.role == "payout")]' <<<"$provider_transactions")" \
    || failure "provider chain transaction response was not parseable"
  [[ "$(jq 'length' <<<"$payout_record")" -eq 1 ]] || failure "provider must hold exactly one payout record"
  [[ "$(jq -r '.[0].status' <<<"$payout_record")" == "broadcast" ]] \
    || failure "provider payout record must be broadcast before the stop"
  RESTART_PAYOUT_TX_HASH="$(jq -r '.[0].tx_hash' <<<"$payout_record")"

  # Exactly one ready channel must exist between the nodes; the supervisor
  # wipes the fiber stores before the first start, so ambiguity means state
  # leaked in from outside this run.
  local channels ready_channels
  channels="$(rpc_call "$NODE1_RPC_URL" list_channels '[{}]')" || failure "client list_channels failed"
  ready_channels="$(jq -ec '[.result.channels[]? | select(.state.state_name == "ChannelReady")]' <<<"$channels")" \
    || failure "client list_channels response was not parseable"
  [[ "$(jq 'length' <<<"$ready_channels")" -eq 1 ]] || failure "expected exactly one ChannelReady channel on the client"
  HANDOFF_CHANNEL_ID="$(jq -r '.[0].channel_id' <<<"$ready_channels")"
  CLIENT_LOCAL_BEFORE="$(jq -r '.[0].local_balance' <<<"$ready_channels")"
  CLIENT_REMOTE_BEFORE="$(jq -r '.[0].remote_balance' <<<"$ready_channels")"

  mkdir -p "$run_dir"
  jq -n \
    --arg swap_id "$HANDOFF_SWAP_ID" \
    --arg payment_hash "$HANDOFF_PAYMENT_HASH" \
    --arg payout_tx_hash "$RESTART_PAYOUT_TX_HASH" \
    --arg channel_id "$HANDOFF_CHANNEL_ID" \
    --arg client_local_balance_before_payment "$CLIENT_LOCAL_BEFORE" \
    --arg client_remote_balance_before_payment "$CLIENT_REMOTE_BEFORE" \
    --arg captured_at "$(date -u +%FT%TZ)" \
    '{
      swap_id: $swap_id,
      payment_hash: $payment_hash,
      payout_tx_hash: $payout_tx_hash,
      channel_id: $channel_id,
      client_local_balance_before_payment: $client_local_balance_before_payment,
      client_remote_balance_before_payment: $client_remote_balance_before_payment,
      provider_swap_state_at_stop: "payout_pending",
      captured_at: $captured_at,
    }' >"$handoff_file"
  log "phase handoff artifact written to $handoff_file"
}

node2_rpc_down() {
  ! rpc_call "$NODE2_RPC_URL" node_info '[]' >/dev/null 2>&1
}

restart_provider() {
  FAILURE_CONTEXT="stopping the provider node2"
  stop_fnn "$NODE2_PID" "provider node2"
  if ! wait_until "node2 RPC at $NODE2_RPC_URL to stop accepting" 15 node2_rpc_down; then
    failure "node2 RPC still accepting connections after the process exited"
  fi
  log "node2 RPC is down; restarting with the same data directory and password"
  FAILURE_CONTEXT="restarting the provider node2"
  start_fnn 2 password2 "$log_dir/node2-restart.log" "[node2-restart]"
  NODE2_PID="$STARTED_PID"
  if ! wait_until "node2 RPC at $NODE2_RPC_URL to become ready" "$RPC_READY_TIMEOUT" rpc_ready "$NODE2_RPC_URL"; then
    failure "node2 did not become ready after the restart"
  fi
  if ! rpc_ready "$NODE1_RPC_URL"; then
    failure "client node1 lost its RPC during the provider restart window"
  fi
  log "node2 restarted (pid $NODE2_PID) and its RPC is ready"
}

main() {
  mkdir -p "$log_dir"
  check_dependencies
  check_ports_free
  build_binaries
  provision_dev_chain
  resolve_node_endpoints
  # The fresh-chain path runs a temporary CKB during initialization; wait for
  # it to release its ports instead of sleeping a fixed interval.
  wait_provisioning_ports_released

  if [[ -z "${KEEP_FIBER_STATE:-}" ]]; then
    FAILURE_CONTEXT="resetting the fiber stores"
    log "wiping tests/nodes/{1,2}/fiber/store for a deterministic start (KEEP_FIBER_STATE=1 keeps them)"
    rm -rf "$nodes_dir/1/fiber/store" "$nodes_dir/2/fiber/store"
  fi

  start_ckb
  if ! wait_until "the CKB RPC at $CKB_RPC_URL to become ready" "$RPC_READY_TIMEOUT" \
      rpc_call "$CKB_RPC_URL" get_tip_block_number '[]'; then
    failure "CKB RPC did not become ready"
  fi
  log "CKB dev chain is ready (pid $CKB_PID)"

  export RUST_BACKTRACE=full
  export RUST_LOG="${RUST_LOG:-info,fnn=debug,fnn::cch::trackers::lnd_trackers=off,fnn::fiber::gossip=off,fnn::fiber::graph=off,fnn::utils::actor=off}"

  FAILURE_CONTEXT="starting the client node1"
  start_fnn 1 password1 "$log_dir/node1.log" "[node1]"
  NODE1_PID="$STARTED_PID"
  if ! wait_until "node1 RPC at $NODE1_RPC_URL to become ready" "$RPC_READY_TIMEOUT" rpc_ready "$NODE1_RPC_URL"; then
    failure "node1 did not become ready"
  fi
  log "node1 (client) is ready (pid $NODE1_PID)"

  start_fnn 2 password2 "$log_dir/node2-phase1.log" "[node2]"
  NODE2_PID="$STARTED_PID"
  if ! wait_until "node2 RPC at $NODE2_RPC_URL to become ready" "$RPC_READY_TIMEOUT" rpc_ready "$NODE2_RPC_URL"; then
    failure "node2 did not become ready"
  fi
  log "node2 (provider) is ready (pid $NODE2_PID)"

  if ! run_bruno_phase phase1; then
    failure "phase1 suite failed (swap parked in PayoutPending was not reached)"
  fi
  log "phase1 passed: loop out swap is in PayoutPending with the payout broadcast"

  discover_handoff

  restart_provider

  log "running phase2 against the restarted provider"
  if ! run_bruno_phase phase2 \
      --env-var "RESTART_SWAP_ID=$HANDOFF_SWAP_ID" \
      --env-var "RESTART_PAYMENT_HASH=$HANDOFF_PAYMENT_HASH" \
      --env-var "RESTART_PAYOUT_TX_HASH=$RESTART_PAYOUT_TX_HASH" \
      --env-var "RESTART_CHANNEL_ID=$HANDOFF_CHANNEL_ID" \
      --env-var "RESTART_CLIENT_LOCAL_BEFORE=$CLIENT_LOCAL_BEFORE" \
      --env-var "RESTART_CLIENT_REMOTE_BEFORE=$CLIENT_REMOTE_BEFORE"; then
    failure "phase2 suite failed (the restarted provider did not recover the swap to Success)"
  fi

  log "PASS: the restarted provider recovered the loop out swap to Success with the same payout hash"
  log "artifacts: $run_dir"
}

main "$@"
