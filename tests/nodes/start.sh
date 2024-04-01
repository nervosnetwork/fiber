#!/usr/bin/env bash

export RUST_BACKTRACE=full RUST_LOG=debug

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"

cd "$nodes_dir" || exit 1

chmod 600 */ckb/sk

start() {
    cargo run -- "$@"
}

start -d 1 &
start -d 2 &
start -d 3 &

wait
