#!/usr/bin/env bash
set -euo pipefail

cargo_toml="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/Cargo.toml"
cd -- "$(dirname -- "$cargo_toml")/../../nodes" || exit 1
NODES_DIR="$PWD" cargo run --manifest-path="$cargo_toml"
