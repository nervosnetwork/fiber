#!/usr/bin/env bash

export RUST_BACKTRACE=full RUST_LOG=info,ckb_pcn_node=debug

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"

export BINARY_PATH="$(realpath "$script_dir/../../build/release")"

cd "$nodes_dir" || exit 1
start() {
    cargo run -- "$@"
}

if [ "$#" -ne 1 ]; then
    LOG_SURFFIX=$' [node 1]\n' start -d 1 &
    LOG_SURFFIX=$' [node 2]\n' start -d 2 &
    LOG_SURFFIX=$' [node 3]\n' start -d 3 &
else
    for id in "$@"; do
        LOG_SURFFIX=" [$id]"$'\n' start -d "$id" &
    done
fi

wait
