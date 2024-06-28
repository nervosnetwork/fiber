#!/usr/bin/env bash

set -euo pipefail

MOLC="${MOLC:-moleculec}"

schema_dir="$( cd -- "$( dirname -- "${BASH_SOURCE[0]}" )" &> /dev/null && pwd )"
gen_dir="$(dirname "$schema_dir")/gen"


files=("cfn.mol" "invoice.mol")
for file in "${files[@]}"; do
    f="$schema_dir/$file"
    output_file="$gen_dir/$(basename "${f%%.mol}.rs")"
    "$MOLC" --language rust --schema-file "$f" | rustfmt > "$output_file"

    ## ignore them in clippy
    ALLOW_CLIPPY="#![allow(clippy::all)]"
    temp_file=$(mktemp)
    echo -e "$ALLOW_CLIPPY\n$(cat "$output_file")" > "$temp_file"
    mv "$temp_file" "$output_file"

done
