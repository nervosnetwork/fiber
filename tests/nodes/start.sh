#!/usr/bin/env bash
set -euo pipefail
export SHELLOPTS
export RUST_BACKTRACE=full RUST_LOG=info,fnn=debug,fnn::cch::actor::tracker=off

should_remove_old_state="${REMOVE_OLD_STATE:-}"
should_clean_fiber_state="${REMOVE_OLD_FIBER:-}"
should_start_bootnode="${START_BOOTNODE:-}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"
deploy_dir="$(dirname "$script_dir")/deploy"
bruno_dir="$(dirname "$script_dir")/bruno/environments"

# The following environment variables are used in the contract tests.
# We may load all contracts within the following folder to the test environment.
export TESTING_CONTRACTS_DIR="$deploy_dir/contracts"

# Initialize the dev-chain if it does not exist.
# This script is nilpotent, so it is safe to run multiple times.
"$deploy_dir/init-dev-chain.sh"

if [ -n "$should_clean_fiber_state" ]; then
    echo "starting to clean fiber store ...."
    rm -rf "$nodes_dir"/*/fiber/store
elif [ -n "$should_remove_old_state" ]; then
    echo "starting to reset ...."
    rm -rf "$nodes_dir"/*/fiber/store
    "$deploy_dir/init-dev-chain.sh" -f
fi

echo "Initializing finished, begin to start services ...."
sleep 1

ckb run -C "$deploy_dir/node-data" --indexer &

# Start the dev node in the background.
cd "$nodes_dir" || exit 1

start() {
    cargo run -- "$@"
}

if [ "$#" -ne 1 ]; then
    if [[ -n "$should_start_bootnode" ]]; then
        LOG_PREFIX=$'[boot node]' start -d bootnode &
        # sleep some time to ensure bootnode started
        # while other nodes try to connect to it.
        sleep 5
        # export the environment variable so that other nodes can connect to the bootnode.
        export FIBER_BOOTNODES_ADDRS=/ip4/127.0.0.1/tcp/8343/p2p/Qmbyc4rhwEwxxSQXd5B4Ej4XkKZL6XLipa3iJrnPL9cjGR
    fi
    LOG_PREFIX=$'[node 1]' start -d 1 &
    LOG_PREFIX=$'[node 2]' start -d 2 &
    LOG_PREFIX=$'[node 3]' start -d 3 &
else
    for id in "$@"; do
        LOG_PREFIX="[$id]"$'' start -d "$id" &
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
