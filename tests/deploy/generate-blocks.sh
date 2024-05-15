#!/usr/bin/env bash

CKB_RPC_URL="${CKB_RPC_URL:-http://127.0.0.1:8114}"

function generate_one() {
    curl --request POST \
        -H "Content-Type: application/json" \
        --url "$CKB_RPC_URL" \
        --data '{
      "id": 42,
      "jsonrpc": "2.0",
      "method": "generate_block",
      "params": []
    }'
    echo
}

function generate_n() {
    if [ $# -eq 0 ]; then
        generate_one
    else
        for i in $(seq "$@"); do
            generate_one
        done
    fi
}

case "${1:-}" in
--url)
    CKB_RPC_URL="$2"
    shift
    shift
    generate_n "$@"
    ;;
--url=*)
    CKB_RPC_URL="${1#*=}"
    shift
    generate_n "$@"
    ;;
--help)
    echo 'usage: generate-blocks.sh [--help|--url CKB_RPC_URL] [count]'
    ;;
*)
    generate_n "$@"
    ;;
esac
