#!/usr/bin/env bash

set -xeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
data_dir="$script_dir/node-data"
udt_init_dir="$script_dir/udt-init"
nodes_dir="$script_dir/../nodes"
cd "$script_dir" || exit 1

miner_key_file="$data_dir/specs/miner.key"

if ! [ -f "$miner_key_file" ]; then
    echo "$miner_key_file not found, use the default test account" >&2
    mkdir -p "$(dirname "$miner_key_file")"
    echo "d00c06bfd800d27397002dca6fb0993d5ba6399b4238b2f29ee9deb97593d2bc" >"$miner_key_file"
fi

ckb-cli() {
    # Don't pollute the home directory.
    env HOME="$data_dir" ckb-cli "$@"
}

if ! (echo | ckb-cli account import --local-only --privkey-path "$miner_key_file"); then
    :
fi

run_udt_init() {
    export $(xargs <".env")
    export NODES_DIR="$nodes_dir"
    (
        cd "$udt_init_dir" || exit 1
        cargo run -- "$@"
    )
}

./create-dotenv-file.sh >.env
run_udt_init
