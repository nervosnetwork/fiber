#!/usr/bin/env bash

set -xeuo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
cd "$script_dir" || exit 1

case "${1:-}" in
--testnet)
    shift
    cat env.example
    ;;
--mainnet)
    shift
    echo 'NEXT_PUBLIC_CKB_CHAIN="LINA"'
    echo 'NEXT_PUBLIC_CKB_RPC_URL="	https://mainnet.ckb.dev/"'
    ;;
*)
    echo 'NEXT_PUBLIC_CKB_CHAIN="DEV"'
    echo 'NEXT_PUBLIC_CKB_RPC_URL="http://127.0.0.1:8114/"'
    GENESIS_TXS="$(ckb-cli rpc get_block_by_number --number 0 | sed -n 's/^    hash: //p')"
    echo 'NEXT_PUBLIC_CKB_GENESIS_TX_0="'"$(echo "$GENESIS_TXS" | head -n 1)"'"'
    echo 'NEXT_PUBLIC_CKB_GENESIS_TX_1="'"$(echo "$GENESIS_TXS" | tail -n 1)"'"'
    ;;
esac

create_dotenv_for_contract() {
    local CONTRACT_NAME="$1"
    local CONTRACT_INFO_FILE="$(ls "migrations/$CONTRACT_NAME"/*.json | grep -v deployment | head -n 1)"
    # Replace dash with underscore because envirornment variable names cannot contain dashes.
    # Also use uppercase because environment variable names are usually uppercase.
    CONTRACT_NAME="$(echo "$CONTRACT_NAME" | tr '-' '_' | tr '[:lower:]' '[:upper:]')"

    sed -n \
        -e 's/,$//' \
        -e 's/^ *"data_hash": "/NEXT_PUBLIC_'"$CONTRACT_NAME"'_CODE_HASH="/p' \
        "$CONTRACT_INFO_FILE" | head -1

    sed -n \
        -e 's/,$//' \
        -e 's/^ *"type_id": "/NEXT_PUBLIC_'"$CONTRACT_NAME"'_TYPE_HASH="/p' \
        "$CONTRACT_INFO_FILE" | head -1

    sed -n \
        -e 's/,$//' \
        -e 's/^ *"tx_hash": /NEXT_PUBLIC_'"$CONTRACT_NAME"'_TX_HASH=/p' \
        "$CONTRACT_INFO_FILE" | head -1

    sed -n \
        -e 's/,$//' \
        -e 's/^ *"index": /NEXT_PUBLIC_'"$CONTRACT_NAME"'_TX_INDEX=/p' \
        "$CONTRACT_INFO_FILE" | head -1

    if ! grep -Eq '"dep_group_recipes": *\[ *\]$' "$CONTRACT_INFO_FILE"; then
        sed -n \
            -e 's/,$//' \
            -e 's/^ *"data_hash": "/NEXT_PUBLIC_'"$CONTRACT_NAME"'_DEP_GROUP_CODE_HASH="/p' \
            "$CONTRACT_INFO_FILE" | tail -1

        sed -n \
            -e 's/,$//' \
            -e 's/^ *"tx_hash": "/NEXT_PUBLIC_'"$CONTRACT_NAME"'_DEP_GROUP_TX_HASH="/p' \
            "$CONTRACT_INFO_FILE" | tail -1

        sed -n \
            -e 's/,$//' \
            -e 's/^ *"index": /NEXT_PUBLIC_'"$CONTRACT_NAME"'_DEP_GROUP_TX_INDEX=/p' \
            "$CONTRACT_INFO_FILE" | tail -1

    fi
}

create_dotenv_for_contract always_success
create_dotenv_for_contract funding-lock
create_dotenv_for_contract commitment-lock
create_dotenv_for_contract simple-udt
create_dotenv_for_contract xudt
