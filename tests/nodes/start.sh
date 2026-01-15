#!/usr/bin/env bash
set -euo pipefail
export SHELLOPTS
export RUST_BACKTRACE=full RUST_LOG=info,fnn=debug,fnn::cch::trackers::lnd_trackers=off,fnn::fiber::gossip=off,fnn::fiber::graph=off fnn::utils::actor=off

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"
deploy_dir="$(dirname "$script_dir")/deploy"
bruno_dir="$(dirname "$script_dir")/bruno/environments"
test_env="${TEST_ENV:-debug}"
testcase_name="${1:-}"
testcase_dir="$(dirname "$script_dir")/bruno/${testcase_name}"
start_node_ids=()

if ! [ -d "$testcase_dir" ]; then
  echo "usage: ${BASH_SOURCE[0]} TESTCASE" >&2
  echo "$testcase_dir is not a testcase directory"
  exit 1
fi

case "$testcase_name" in
  "e2e/cross-chain-hub")
    ./tests/deploy/lnd-init/setup-lnd.sh
    ;;
  "e2e/router-pay")
    export START_BOOTNODE=y
    ;;
  "e2e/funding-tx-verification")
    cd ./tests/funding-tx-builder/ && cargo build --locked && cd -
    export FIBER_FUNDING_TX_SHELL_BUILDER="$(dirname "$script_dir")/funding-tx-builder/target/debug/funding-tx-builder ${EXTRA_BRU_ARGS:-}"
    echo "FIBER_FUNDING_TX_SHELL_BUILDER=\"$FIBER_FUNDING_TX_SHELL_BUILDER\""
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
    rm -rf "$nodes_dir"/*/fiber/store
elif [ -n "$should_remove_old_state" ]; then
    echo "starting to reset ...."
    rm -rf "$nodes_dir"/*/fiber/store
    "$deploy_dir/init-dev-chain.sh" -f
fi

# Initialize the dev-chain if it does not exist.
# This script is nilpotent, so it is safe to run multiple times.
"$deploy_dir/init-dev-chain.sh"

echo "Initializing finished, begin to start services .... ${test_env}"
sleep 1

ckb run -C "$deploy_dir/node-data" --indexer &
cargo build --locked "--${TEST_ENV:-}"

# Start the dev node in the background.
cd "$nodes_dir" || exit 1

start_fnn() {
    log_file="${2}.log"
    echo "logging to ${log_file}"
    ../../target/"${test_env}"/fnn "$@" 2>&1 | tee "$log_file"
}

if [ "${#start_node_ids[@]}" = 0 ]; then
    if [[ -n "$should_start_bootnode" ]]; then
        FIBER_SECRET_KEY_PASSWORD='password0' LOG_PREFIX=$'[boot node]' start_fnn -d bootnode &
        # sleep some time to ensure bootnode started
        # while other nodes try to connect to it.
        sleep 5
        # export the environment variable so that other nodes can connect to the bootnode.
        export FIBER_BOOTNODE_ADDRS=/ip4/127.0.0.1/tcp/8343/p2p/Qmbyc4rhwEwxxSQXd5B4Ej4XkKZL6XLipa3iJrnPL9cjGR
    fi
    FIBER_SECRET_KEY_PASSWORD='password1' LOG_PREFIX=$'[node 1]' start_fnn -d 1 &
    FIBER_SECRET_KEY_PASSWORD='password2' LOG_PREFIX=$'[node 2]' start_fnn -d 2 &
    FIBER_SECRET_KEY_PASSWORD='password3' LOG_PREFIX=$'[node 3]' start_fnn -d 3 &
else
    for id in "${start_node_ids[@]}"; do
        FIBER_SECRET_KEY_PASSWORD="password$id" LOG_PREFIX="[$id]"$'' start_fnn -d "$id" &
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
