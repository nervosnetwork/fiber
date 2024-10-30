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

function deploy() {
    local DEPLOY_NAME="$1"
    local MIGRATION_DIR="migrations/$DEPLOY_NAME"
    local CONFIG_FILE="$MIGRATION_DIR/deployment.toml"
    local INFO_FILE="$MIGRATION_DIR/deployment.json"
    local TEMPLATE_FILE="migrations/templates/$DEPLOY_NAME.toml"

    rm -rf "$MIGRATION_DIR" && mkdir -p "$MIGRATION_DIR"
    GENESIS_TX0="$(ckb -C "$data_dir" list-hashes | sed -n 's/tx_hash = "\(.*\)"/\1/p' | head -1)"
    sed "s/0x8f8c79eb6671709633fe6a46de93c0fedc9c1b8a6527a18d3983879542635c9f/$GENESIS_TX0/" "$TEMPLATE_FILE" >"$CONFIG_FILE"

    ckb-cli deploy gen-txs --from-address $(cat "$nodes_dir/deployer/ckb/wallet") \
        --fee-rate 100000 --deployment-config "$CONFIG_FILE" --info-file "$INFO_FILE" --migration-dir "$MIGRATION_DIR"

    ckb-cli deploy sign-txs --add-signatures --info-file "$INFO_FILE" --privkey-path "$miner_key_file" --output-format json | sed -n 's/: \("[^"]*"\)/: [\1]/p'

    if ckb-cli deploy apply-txs --info-file "$INFO_FILE" --migration-dir "$MIGRATION_DIR" | grep -q "already exists in transaction_pool"; then
        :
    fi
}

generate_blocks() {
    ./generate-blocks.sh 8
    sleep 1
}

# try twice in case the indexer has not updated yet
deploy_and_generate_blocks() {
    deploy "$1" || deploy "$1"
    generate_blocks
}

run_udt_init() {
    export $(xargs <".env")
    export NODES_DIR="$nodes_dir"
    (
        cd "$udt_init_dir" || exit 1
        cargo run -- "$@"
    )
}

deploy_and_generate_blocks xudt

./create-dotenv-file.sh >.env
run_udt_init
