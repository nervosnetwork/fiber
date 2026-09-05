#!/usr/bin/env bash
set -euo pipefail

# Runner for the UDT loop-in rejection suite. Builds (once) and starts the
# liquidity-lock-mutator sidecar that signs the mutated lock funding
# transactions, then runs the Bruno suite against the local dev chain. On
# Bruno failure it writes best-effort redacted diagnostics (swap and chain
# transaction JSON from both nodes plus the CKB tip header) under
# tests/artifacts/liquidity/udt-loop-in-rejection/, never masking the Bruno
# exit code.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
# tests/bruno/e2e/liquidity/udt-loop-in-rejection is five levels below the repo root.
repo_root="$(cd -- "$script_dir/../../../../.." &>/dev/null && pwd)"
mutator_dir="$repo_root/tests/liquidity-lock-mutator"
binary="$mutator_dir/target/debug/liquidity-lock-mutator"

CKB_RPC_URL="${CKB_RPC_URL:-http://127.0.0.1:8114}"
MUTATOR_PORT="${MUTATOR_PORT:-38117}"
MUTATOR_URL="http://127.0.0.1:${MUTATOR_PORT}"
MUTATOR_PRIVKEY_PATH="${MUTATOR_PRIVKEY_PATH:-$repo_root/tests/nodes/1/ckb/plain_key}"

# Export the actual node RPC URLs for failure diagnostics when CI generated
# randomized ports and recorded them in tests/nodes/.ports.
source "$repo_root/tests/bruno/scripts/resolve-node-urls.sh"

mutator_pid=""

cleanup() {
  if [[ -n "$mutator_pid" ]] && kill -0 "$mutator_pid" 2>/dev/null; then
    kill "$mutator_pid" 2>/dev/null || true
    wait "$mutator_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if ! [[ -x "$binary" ]]; then
  echo "building liquidity-lock-mutator..."
  (cd "$mutator_dir" && cargo build --locked)
fi

if [[ ! -f "$MUTATOR_PRIVKEY_PATH" ]]; then
  echo "node1 dev wallet key not found: $MUTATOR_PRIVKEY_PATH" >&2
  exit 1
fi

echo "waiting for the CKB dev chain at $CKB_RPC_URL ..."
for _ in $(seq 1 30); do
  if curl -sf -X POST "$CKB_RPC_URL" -H 'Content-Type: application/json' \
    -d '{"id":1,"jsonrpc":"2.0","method":"get_tip_block_number","params":[]}' >/dev/null; then
    break
  fi
  sleep 1
done
if ! curl -sf -X POST "$CKB_RPC_URL" -H 'Content-Type: application/json' \
  -d '{"id":1,"jsonrpc":"2.0","method":"get_tip_block_number","params":[]}' >/dev/null; then
  echo "CKB dev chain is not reachable at $CKB_RPC_URL" >&2
  exit 1
fi

echo "starting the liquidity-lock-mutator sidecar on $MUTATOR_URL"
"$binary" --serve "$MUTATOR_PORT" --rpc-url "$CKB_RPC_URL" --privkey-path "$MUTATOR_PRIVKEY_PATH" &
mutator_pid=$!

ready="0"
for _ in $(seq 1 20); do
  if curl -sf "$MUTATOR_URL" >/dev/null; then
    ready="1"
    break
  fi
  if ! kill -0 "$mutator_pid" 2>/dev/null; then
    echo "liquidity-lock-mutator exited during startup" >&2
    exit 1
  fi
  sleep 0.5
done
if [[ "$ready" != "1" ]]; then
  echo "liquidity-lock-mutator did not become ready on $MUTATOR_URL" >&2
  exit 1
fi

cd "$repo_root/tests/bruno"
export CKB_MUTATOR_URL="$MUTATOR_URL"

collector="$script_dir/../../../scripts/collect-liquidity-failure-diagnostics.sh"

set +e
npx @usebruno/cli@1.20.0 run e2e/liquidity/udt-loop-in-rejection -r --env test
bruno_status=$?
set -e

if [[ "$bruno_status" -ne 0 ]]; then
  echo "Bruno suite failed; writing failure diagnostics (best effort)"
  bash "$collector" udt-loop-in-rejection || true
fi
exit "$bruno_status"
