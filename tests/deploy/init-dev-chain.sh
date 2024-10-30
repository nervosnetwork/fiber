#!/usr/bin/env bash

set -euo pipefail
export SHELLOPTS

check_deps() {
    for command in "$@"; do
        if ! command -v "$command" >/dev/null; then
            echo "$* are required to run this script"
            exit 1
        fi
    done
}

check_deps ckb ckb-cli perl

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
data_dir="$script_dir/node-data"
nodes_dir="$script_dir/../nodes"

# If -f is used, we will remove old state data. Otherwise we will skip the initialization.
while getopts "f" opt; do
    case $opt in
    f)
        rm -rf "$data_dir"
        ;;
    \?)
        echo "Invalid option: $OPTARG" 1>&2
        ;;
    esac
done

# Initialize the data directory if it does not exist.
if ! [[ -d "$data_dir" ]]; then
    ckb init -C "$data_dir" -c dev --force --ba-arg 0xc8328aabcd9b9e8e64fbc566c4385c3bdeb219d7
    cp "$nodes_dir/deployer/dev.toml" "$data_dir/specs/dev.toml"
    sed -i.bak 's|\.\./\.\./deploy/contracts|\.\./\.\./\.\./deploy/contracts|g' "$data_dir/specs/dev.toml"

    # Enable the IntegrationTest module (required to generate blocks).
    if ! grep -E '^modules.*IntegrationTest' "$data_dir/ckb.toml"; then
        # -i.bak is required to sed work on both Linux and macOS.
        sed -i.bak 's/\("Debug"\)/\1, "IntegrationTest"/' "$data_dir/ckb.toml"
    fi

    ckb run -C "$data_dir" --indexer &

    # Make some accounts with default balances, and deploy the contracts to the network.
    # Don't continue until the default account has some money.
    # Transfer some money from the default account (node 3) to node 1 for later use.
    echo "begin to setup wallet states for nodes"
    for i in {1..20}; do
        if ! nc -z 127.0.0.1 8114; then
            echo "waiting CKB ready $i ..."
            sleep 2
        else
            echo "CKB is ready now ..."
            break
        fi
    done

    # Transfer some money to the node 1.
    # The address of node 1 can be seen with the following command:
    # echo | HOME=/tmp ckb-cli account import --local-only --privkey-path "$$nodes_dir/1/ckb/key"
    ckb-cli wallet transfer --to-address $(cat "$nodes_dir/1/ckb/wallet") --capacity 5000000000 --fee-rate 2000 --privkey-path "$nodes_dir/deployer/ckb/key"

    sleep 1
    "$script_dir/generate-blocks.sh" 6
    sleep 1

    # Transfer some money to the node 2.
    ckb-cli wallet transfer --to-address $(cat "$nodes_dir/2/ckb/wallet") --capacity 5000000000 --fee-rate 2000 --privkey-path "$nodes_dir/deployer/ckb/key"
    sleep 1
    "$script_dir/generate-blocks.sh" 6
    sleep 1

    # Transfer some money to the node 3.
    ckb-cli wallet transfer --to-address $(cat "$nodes_dir/3/ckb/wallet") --capacity 5000000000 --fee-rate 2000 --privkey-path "$nodes_dir/deployer/ckb/key"
    sleep 1
    # Generate a few blocks so that above transaction is confirmed.
    echo "begin to generate blocks for wallet updating..."
    "$script_dir/generate-blocks.sh" 6

    # Also deploy the contracts.
    echo "deploy.sh..."
    "$script_dir/deploy.sh"

    pkill -P $$
fi
