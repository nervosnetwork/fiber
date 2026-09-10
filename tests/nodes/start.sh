#!/usr/bin/env bash
set -euo pipefail
export SHELLOPTS

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"
deploy_dir="$(dirname "$script_dir")/deploy"
bruno_dir="$(dirname "$script_dir")/bruno/environments"
test_env="${TEST_ENV:-debug}"
testcase_name="${1:-}"
testcase_dir="$(dirname "$script_dir")/bruno/${testcase_name}"
start_node_ids=()
enable_fiber_metrics="${ENABLE_FIBER_METRICS:-}"
fiber_build_features="${FIBER_BUILD_FEATURES:-}"
lsp_sdk_agent_control_addr="${LSP_SDK_AGENT_CONTROL_ADDR:-127.0.0.1:21917}"
export RUST_BACKTRACE=full RUST_LOG=info,fnn=debug,fnn::cch::trackers::lnd_trackers=off,fnn::fiber::gossip=off,fnn::fiber::graph=off,fnn::utils::actor=off

is_lsp_e2e() {
    case "$testcase_name" in
        e2e/lsp|e2e/lsp-remote-signer|e2e/lsp-remote-signer-watchtower*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

# Read a one-line `section.key` value from a generated node YAML file.
yaml_map_value() {
    local file="$1"
    local section="$2"
    local key="$3"
    awk -v section="$section" -v key="$key" '
        $0 ~ "^" section ":" { in_section = 1; next }
        in_section && $0 ~ /^[^[:space:]#]/ { in_section = 0 }
        in_section && $1 == key ":" { print $2; exit }
    ' "$file"
}

# CI remaps fiber/rpc ports after init. Do not reuse leftover extra wait ports
# from a previous LSP run when starting a non-LSP workflow. LSP cases write the
# SDK agent control port immediately so wait.sh does not race node RPC readiness.
rm -f "$nodes_dir/.extra-ports"
if is_lsp_e2e; then
    printf '%s\n' "${lsp_sdk_agent_control_addr##*:}" >"$nodes_dir/.extra-ports"
fi

append_fiber_build_feature() {
    local feature="$1"
    if [[ ",$fiber_build_features," != *",$feature,"* ]]; then
        fiber_build_features="${fiber_build_features:+$fiber_build_features,}$feature"
    fi
}

if ! [ -d "$testcase_dir" ]; then
  echo "usage: ${BASH_SOURCE[0]} TESTCASE" >&2
  echo "$testcase_dir is not a testcase directory"
  exit 1
fi

case "$testcase_name" in
  "e2e/cross-chain-hub")
    ./tests/deploy/lnd-init/setup-lnd.sh
    ;;
  "e2e/cross-chain-hub-separate")
    ./tests/deploy/lnd-init/setup-lnd.sh
    export CCH_SEPARATE=y
    ;;
  "e2e/router-pay")
    export START_BOOTNODE=y
    ;;
  e2e/lsp|e2e/lsp-remote-signer|e2e/lsp-remote-signer-watchtower*)
    export RPC_ENABLED_MODULES=cch,channel,payment,graph,info,invoice,lsp,peer,pubsub,watchtower,dev,prof
    ;;
  "e2e/funding-tx-verification")
    cd ./tests/funding-tx-builder/ && cargo build --locked && cd -
    export FIBER_FUNDING_TX_SHELL_BUILDER="$(dirname "$script_dir")/funding-tx-builder/target/debug/funding-tx-builder ${EXTRA_BRU_ARGS:-}"
    echo "FIBER_FUNDING_TX_SHELL_BUILDER=\"$FIBER_FUNDING_TX_SHELL_BUILDER\""
    ;;
  "e2e/watchtower/force-close-preimage-multiple")
    export FIBER_ENABLE_PEER_RECONNECT_BACKOFF=false
    ;;
  "e2e/watchtower/force-close-mpp")
    export FIBER_WATCHTOWER_CHECK_INTERVAL_SECONDS=3
    ;;
  "unit")
    start_node_ids=(3)
    ;;
esac

should_remove_old_state="${REMOVE_OLD_STATE:-}"
should_clean_fiber_state="${REMOVE_OLD_FIBER:-}"
should_start_bootnode="${START_BOOTNODE:-}"

# The following environment variables are used in the contract tests.
# We may load all contracts within the following folder to the test environment.
export TESTING_CONTRACTS_DIR="$deploy_dir/contracts"


if [ -n "$should_clean_fiber_state" ]; then
    echo "starting to clean fiber store ...."
    rm -rf "$nodes_dir"/*/fiber/store "$nodes_dir"/*/lsp
elif [ -n "$should_remove_old_state" ]; then
    echo "starting to reset ...."
    rm -rf "$nodes_dir"/*/fiber/store "$nodes_dir"/*/lsp
    "$deploy_dir/init-dev-chain.sh" -f
    if is_lsp_e2e; then
        rm -rf "$nodes_dir/lsp-sdk-agent"
        rm -f "$nodes_dir/lsp-sdk-agent-status.json"
        rm -f "$nodes_dir/lsp-sdk-agent.log"
    fi
fi

# Initialize the dev-chain if it does not exist.
# This script is nilpotent, so it is safe to run multiple times.
"$deploy_dir/init-dev-chain.sh"

echo "Initializing finished, begin to start services .... ${test_env}"
sleep 1

ckb run -C "$deploy_dir/node-data" --indexer &
build_args=(--locked)
case "$test_env" in
    debug)
        ;;
    release)
        build_args+=(--release)
        ;;
    *)
        build_args+=(--profile "$test_env")
        ;;
esac
if [[ "$test_env" != "release" ]]; then
    append_fiber_build_feature debug-add-tlc
fi
if [[ -n "$enable_fiber_metrics" ]]; then
    append_fiber_build_feature metrics
fi
if [[ -n "$fiber_build_features" ]]; then
    echo "building fnn with features: $fiber_build_features"
    build_args+=(--features "$fiber_build_features")
fi
cargo build "${build_args[@]}"
if is_lsp_e2e; then
    agent_build_args=(--locked)
    case "$test_env" in
        debug)
            ;;
        release)
            agent_build_args+=(--release)
            ;;
        *)
            agent_build_args+=(--profile "$test_env")
            ;;
    esac
    cargo build "${agent_build_args[@]}" -p fiber-lsp-sdk-agent
fi

# Start the dev node in the background.
cd "$nodes_dir" || exit 1

start_fnn() {
    log_file="${2}.log"
    echo "logging to ${log_file}"
    ../../target/"${test_env}"/fnn "$@" 2>&1 | tee "$log_file"
}

if [[ -n "$enable_fiber_metrics" ]]; then
    bootnode_metrics_addr="${BOOTNODE_METRICS_ADDR:-127.0.0.1:29113}"
    node1_metrics_addr="${NODE1_METRICS_ADDR:-127.0.0.1:29114}"
    node2_metrics_addr="${NODE2_METRICS_ADDR:-127.0.0.1:29115}"
    node3_metrics_addr="${NODE3_METRICS_ADDR:-127.0.0.1:29116}"
    echo "fiber metrics enabled:"
    echo "  bootnode: ${bootnode_metrics_addr}"
    echo "  node1: ${node1_metrics_addr}"
    echo "  node2: ${node2_metrics_addr}"
    echo "  node3: ${node3_metrics_addr}"
fi

if [ "${#start_node_ids[@]}" = 0 ]; then
    if [[ -n "$should_start_bootnode" ]]; then
        if [[ -n "$enable_fiber_metrics" ]]; then
            FIBER_SECRET_KEY_PASSWORD='password0' LOG_PREFIX=$'[boot node]' FIBER_METRICS_ADDR="$bootnode_metrics_addr" start_fnn -d bootnode &
        else
            FIBER_SECRET_KEY_PASSWORD='password0' LOG_PREFIX=$'[boot node]' start_fnn -d bootnode &
        fi
        # sleep some time to ensure bootnode started
        # while other nodes try to connect to it.
        sleep 5
        # export the environment variable so that other nodes can connect to the bootnode.
        export FIBER_BOOTNODE_ADDRS=/ip4/127.0.0.1/tcp/8343/p2p/Qmbyc4rhwEwxxSQXd5B4Ej4XkKZL6XLipa3iJrnPL9cjGR
    fi
    node2_args=(-d 2)
    if is_lsp_e2e; then
        node2_args+=(
            -s fiber,rpc,ckb,lsp
            --fiber-auto-accept-channel-ckb-funding-amount 50000000000
            --rpc-biscuit-public-key ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79
            --rpc-biscuit-private-key-path "${script_dir}/biscuit-test-private-key"
        )
    fi
    if [[ -n "$enable_fiber_metrics" ]]; then
        FIBER_SECRET_KEY_PASSWORD='password1' LOG_PREFIX=$'[node 1]' FIBER_METRICS_ADDR="$node1_metrics_addr" start_fnn -d 1 &
        FIBER_SECRET_KEY_PASSWORD='password2' LOG_PREFIX=$'[node 2]' FIBER_METRICS_ADDR="$node2_metrics_addr" start_fnn "${node2_args[@]}" &
        FIBER_SECRET_KEY_PASSWORD='password3' LOG_PREFIX=$'[node 3]' FIBER_METRICS_ADDR="$node3_metrics_addr" start_fnn -d 3 &
    else
        FIBER_SECRET_KEY_PASSWORD='password1' LOG_PREFIX=$'[node 1]' start_fnn -d 1 &
        FIBER_SECRET_KEY_PASSWORD='password2' LOG_PREFIX=$'[node 2]' start_fnn "${node2_args[@]}" &
        FIBER_SECRET_KEY_PASSWORD='password3' LOG_PREFIX=$'[node 3]' start_fnn -d 3 &
    fi
    if is_lsp_e2e; then
        agent_store="$nodes_dir/lsp-sdk-agent"
        agent_status="$nodes_dir/lsp-sdk-agent-status.json"
        operator_token="$(sed -n 's/^  LSP_OPERATOR_TOKEN: //p' "$bruno_dir/test.bru")"
        node2_rpc_addr="$(yaml_map_value "$nodes_dir/2/config.yml" rpc listening_addr)"
        if [[ -z "$node2_rpc_addr" ]]; then
            echo "failed to read rpc.listening_addr from $nodes_dir/2/config.yml" >&2
            exit 1
        fi
        node2_rpc_url="http://${node2_rpc_addr}"
        echo "starting fiber-lsp-sdk-agent --rpc ${node2_rpc_url} --control-addr ${lsp_sdk_agent_control_addr}"
        run_lsp_sdk_agent() {
            while true; do
                if ! ../../target/"${test_env}"/fiber-lsp-sdk-agent \
                        --rpc "$node2_rpc_url" \
                        --store "$agent_store" \
                        --status-file "$agent_status" \
                        --control-addr "$lsp_sdk_agent_control_addr" \
                        --operator-token "$operator_token" \
                        2>&1 | tee -a lsp-sdk-agent.log; then
                    echo "fiber-lsp-sdk-agent failed; retrying persisted signer"
                fi
                echo "fiber-lsp-sdk-agent exited; restarting persisted signer"
                sleep 1
            done
        }
        run_lsp_sdk_agent &
    fi
    if [[ -n "${CCH_SEPARATE:-}" ]]; then
        # Wait for node 3 to start so CCH can connect to it
        sleep 3
        FIBER_SECRET_KEY_PASSWORD='password4' LOG_PREFIX=$'[node cch]' start_fnn -d cch &
    fi
else
    for id in "${start_node_ids[@]}"; do
        if [[ -n "$enable_fiber_metrics" ]]; then
            metrics_var_name="NODE${id}_METRICS_ADDR"
            default_metrics_addr="127.0.0.1:$((29113 + id))"
            metrics_addr="${!metrics_var_name:-$default_metrics_addr}"
            FIBER_SECRET_KEY_PASSWORD="password$id" LOG_PREFIX="[$id]"$'' FIBER_METRICS_ADDR="$metrics_addr" start_fnn -d "$id" &
        else
            FIBER_SECRET_KEY_PASSWORD="password$id" LOG_PREFIX="[$id]"$'' start_fnn -d "$id" &
        fi
    done
fi

# we will exit when any of the background processes exits.
# we don't use `wait -n` because of compatibility issues between bash and zsh
initial_jobs=$(jobs -p | wc -l)
while true; do
    current_jobs=$(jobs -p | wc -l)
    if [ "$current_jobs" -lt "$initial_jobs" ]; then
        echo "A background job has exited, exiting ..."
        exit 1
    fi
    sleep 1
done
