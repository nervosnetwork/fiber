#!/usr/bin/env bash

set -euo pipefail

MOLC="${MOLC:-moleculec}"

schema_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
gen_dir="$(dirname "$schema_dir")/gen"

files=("fiber.mol" "invoice.mol" "gossip.mol")
for file in "${files[@]}"; do
    f="$schema_dir/$file"
    output_file="$gen_dir/$(basename "${f%%.mol}.rs")"
    "$MOLC" --language rust --schema-file "$f" | rustfmt >"$output_file"
done
