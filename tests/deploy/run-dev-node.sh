#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
data_dir="$script_dir/node-data"

# If -f is used, we will remove old state data. Otherwise we will skip the initialization.

while getopts "f" opt; do
    case $opt in
    f)
        # TODO: this is a linux only command.
        for pid in $(pgrep "^ckb$"); do
            # grep with exact match
            if grep -Fq "$data_dir" "/proc/$pid/cmdline"; then
                kill "$pid"
            fi
        done
        rm -rf "$data_dir"
        ;;
    \?)
        echo "Invalid option: $OPTARG" 1>&2
        ;;
    esac
done

if ! [[ -d "$data_dir" ]]; then
    ckb init -C "$data_dir" -c dev --force --ba-arg 0xc8328aabcd9b9e8e64fbc566c4385c3bdeb219d7
    # Enable the IntegrationTest module (required to generate blocks).
    if ! grep -E '^modules.*IntegrationTest' "$data_dir/ckb.toml"; then
        sed -E -i 's/^(modules.*)\[(.*)\]/\1[\2,"IntegrationTest"]/' "$data_dir/ckb.toml"
    fi
    # Transfer some money from the default account (node 3) to node 1 for later use.
    (
        sleep 3
        ckb-cli wallet transfer --to-address ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqgx5lf4pczpamsfam48evs0c8nvwqqa59qapt46f --capacity 5000000000 --fee-rate 2000 --privkey-path "$script_dir/../nodes/3/ckb-chain/key"
        # Generate a few blocks so that above transaction is confirmed.
        "$script_dir/generate-blocks.sh" 4
    ) &
fi
ckb run -C "$data_dir" --indexer
