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

check_deps ckb ckb-cli

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
data_dir="$script_dir/node-data"

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
    # Enable the IntegrationTest module (required to generate blocks).
    if ! grep -E '^modules.*IntegrationTest' "$data_dir/ckb.toml"; then
        sed -i 's/\("Debug"\)/\1, "IntegrationTest"/' "$data_dir/ckb.toml"
    fi

    ckb run -C "$data_dir" --indexer &

    # Make some accounts with default balances, and deploy the contracts to the network.
    # Don't continue until the default account has some money.
    # Transfer some money from the default account (node 3) to node 1 for later use.
    sleep 3
    ckb-cli wallet transfer --to-address ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqgx5lf4pczpamsfam48evs0c8nvwqqa59qapt46f --capacity 5000000000 --fee-rate 2000 --privkey-path "$script_dir/../nodes/3/ckb-chain/key"
    # Generate a few blocks so that above transaction is confirmed.
    "$script_dir/generate-blocks.sh" 4

    # Aslo deploy the contracts.
    "$script_dir/deploy.sh"

    pkill -P $$
fi