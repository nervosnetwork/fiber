#!/usr/bin/env bash

export RUST_BACKTRACE=full RUST_LOG=info,ckb_pcn_node=debug

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"

cd "$nodes_dir" || exit 1

chmod 600 */ckb/sk

start() {
    cargo run -- "$@"
}

if [ "$#" -ne 1 ]; then
    start -d 1 &
    start -d 2 &
    start -d 3 &
else
    for id in "$@"; do
        start -d "$id" &
    done
fi

wait
