#!/usr/bin/env bash

export RUST_BACKTRACE=full RUST_LOG=debug

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." &>/dev/null && pwd)"
nodes_dir="$(dirname "$script_dir")/nodes"

"${nodes_dir}/start.sh" unit
