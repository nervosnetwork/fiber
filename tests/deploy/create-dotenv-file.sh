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

ALWAYS_SUCCESS_INFO_FILE="$(ls migrations/always_success/*.json | grep -v deployment | head -n 1)"

sed -n \
    -e 's/,$//' \
    -e 's/^ *"type_id": /NEXT_PUBLIC_ALWAYS_SUCCESS_CODE_HASH=/p' \
    "$ALWAYS_SUCCESS_INFO_FILE" | head -1

sed -n \
    -e 's/,$//' \
    -e 's/^ *"tx_hash": /NEXT_PUBLIC_ALWAYS_SUCCESS_TX_HASH=/p' \
    "$ALWAYS_SUCCESS_INFO_FILE" | tail -1
