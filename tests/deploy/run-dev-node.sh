#!/usr/bin/env bash

set -euo pipefail

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

if ! [[ -d "$data_dir" ]]; then
    ckb init -C "$data_dir" -c dev --force --ba-arg 0xc8328aabcd9b9e8e64fbc566c4385c3bdeb219d7
fi
ckb run -C "$data_dir" --indexer
