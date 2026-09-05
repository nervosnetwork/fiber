#!/usr/bin/env bash
set -euo pipefail

# Sandbox dry-run regression for run-restart-test.sh node port resolution.
#
# The real supervisor needs a CKB dev chain, the fnn binaries and the Bruno
# CLI; this harness replaces every external process with deterministic stubs
# and runs the unchanged supervisor script inside a throwaway repo layout:
#
#   1. randomized: the provisioning stub randomizes the node ports (exactly
#      what udt-init's generate_nodes_config does under ON_GITHUB_ACTION) and
#      records them in tests/nodes/.ports. The supervisor must re-resolve the
#      node endpoints from that record after provisioning; its readiness
#      gates must target the randomized RPC URLs.
#   2. defaults: the provisioning stub writes the conventional default ports
#      (what a local no-ON_GITHUB_ACTION provisioning produces), proving the
#      resolved endpoints - and therefore the local behavior - are unchanged.
#   3. no-port-file: without tests/nodes/.ports the supervisor must keep the
#      configured defaults.
#
# Every stub fnn serves the minimal JSON-RPC surface the supervisor gates on
# (node_info, list_swaps, get_swap, list_liquidity_chain_transactions,
# list_channels) on the RPC port parsed from its generated config.yml, so
# "the supervisor passed its gates" proves it targeted the ports the nodes
# actually listen on. Both stub servers log every request with the bound
# port, and the harness additionally greps the supervisor log for the
# resolved endpoint line.
#
# Ports used: CKB 28014 plus the node RPC ports 26002/26004 (randomized
# scenario) or 21714/21715 (default scenarios). P2P ports are never bound.
# Run this harness when those loopback ports are free - the same
# precondition the real suite documents.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd -- "$script_dir/../../../../.." &>/dev/null && pwd)"
supervisor="$script_dir/run-restart-test.sh"
CKB_PORT_TEST=28014

fail() { printf 'test-run-restart-test: %s\n' "$1" >&2; exit 1; }

[[ -f "$supervisor" ]] || fail "missing supervisor script: $supervisor"
[[ -f "$repo_root/tests/bruno/scripts/resolve-node-urls.sh" ]] \
  || fail "missing tests/bruno/scripts/resolve-node-urls.sh"

harness_dir="$(mktemp -d "${TMPDIR:-/tmp}/restart-supervisor-test.XXXXXX")"
cleanup() {
  rm -rf "$harness_dir"
}
trap cleanup EXIT

cat >"$harness_dir/stub-server.py" <<'PY'
import json, os, re, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

mode = os.environ.get("STUB_MODE", "ckb")
port = int(os.environ.get("STUB_PORT", "0"))
node_id = os.environ.get("STUB_NODE_ID", "")
if mode == "fnn":
    with open(os.path.join(os.environ["STUB_NODES_DIR"], node_id, "config.yml")) as handle:
        config = handle.read()
    match = re.search(r"^\s*listening_addr:\s*127\.0\.0\.1:(\d+)\s*$", config, re.MULTILINE)
    if not match:
        sys.exit("fnn stub could not find an rpc listening_addr in the node config")
    port = int(match.group(1))

SWAP = {
    "swap_id": "0xstub-swap",
    "swap_kind": "loop_out",
    "state": "payout_pending",
    "payment_hash": "0xstub-hash",
}

def result_for(method):
    if method == "node_info":
        return {"node_id": node_id or "ckb-stub"}
    if method == "list_swaps":
        return {"swaps": [dict(SWAP)]}
    if method == "get_swap":
        return dict(SWAP)
    if method == "list_liquidity_chain_transactions":
        return {"transactions": [
            {"role": "payout", "status": "broadcast", "tx_hash": "0xstub-payout"}
        ]}
    if method == "list_channels":
        return {"channels": [{
            "channel_id": "0xstub-channel",
            "state": {"state_name": "ChannelReady"},
            "local_balance": "0x64",
            "remote_balance": "0x64",
        }]}
    return {"stub": mode}

class Handler(BaseHTTPRequestHandler):
    def _log(self, what):
        log_file = os.environ.get("STUB_LOG")
        if log_file:
            with open(log_file, "a") as handle:
                handle.write(f"{mode} {port} {what}\n")

    def do_GET(self):
        self._log("GET /")
        payload = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0) or 0)
        body = self.rfile.read(length).decode("utf-8", "replace")
        try:
            method = json.loads(body).get("method", "")
        except ValueError:
            method = ""
        self._log(method)
        payload = json.dumps({
            "id": 1,
            "jsonrpc": "2.0",
            "result": result_for(method),
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY

build_sandbox() {
  local sandbox="$harness_dir/$1"
  mkdir -p "$sandbox/bin" \
    "$sandbox/tests/deploy/udt-init" \
    "$sandbox/tests/nodes" \
    "$sandbox/tests/bruno/scripts" \
    "$sandbox/tests/bruno/e2e/liquidity/restart-recovery" \
    "$sandbox/target/debug"
  cp "$supervisor" "$sandbox/tests/bruno/e2e/liquidity/restart-recovery/"
  cp "$repo_root/tests/bruno/scripts/resolve-node-urls.sh" "$sandbox/tests/bruno/scripts/"
  cp "$repo_root/tests/bruno/scripts/write-liquidity-diagnostics.sh" "$sandbox/tests/bruno/scripts/"

  # Provisioning stand-in: writes the liquidity-enabled node configs plus the
  # tests/nodes/.ports record exactly like udt-init's generate_nodes_config
  # (randomized ports under ON_GITHUB_ACTION; here they come from
  # FAKE_NODE_PORTS, and FAKE_NO_PORTS=1 skips the record).
  cat >"$sandbox/tests/deploy/init-dev-chain.sh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
nodes_dir="$(cd -- "$(dirname -- "$0")/../nodes" &>/dev/null && pwd)"
read -r n1_p2p n1_rpc n2_p2p n2_rpc _ <<<"${FAKE_NODE_PORTS:-8344 21714 8345 21715}"
write_config() {
  local id="$1" p2p="$2" rpc="$3"
  mkdir -p "$nodes_dir/$id"
  cat >"$nodes_dir/$id/config.yml" <<EOF
# stub-generated node config
fiber:
  listening_addr: /ip4/0.0.0.0/tcp/$p2p
rpc:
  listening_addr: 127.0.0.1:$rpc
  enabled_modules:
  - channel
  - liquidity
EOF
}
write_config 1 "$n1_p2p" "$n1_rpc"
write_config 2 "$n2_p2p" "$n2_rpc"
if [[ -z "${FAKE_NO_PORTS:-}" ]]; then
  printf '%s\n%s\n%s\n%s\n8346\n21716\n' "$n1_p2p" "$n1_rpc" "$n2_p2p" "$n2_rpc" \
    >"$nodes_dir/.ports"
fi
STUB

  # fnn stand-in: serves the supervisor's RPC gates on the RPC port of the
  # generated config.yml for the requested node directory.
  cat >"$sandbox/target/debug/fnn" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "${1:-}" == "-d" && -n "${2:-}" ]] || { echo "usage: fnn -d <node-dir>" >&2; exit 2; }
: "${STUB_SERVER_PY:?}" "${STUB_NODES_DIR:?}"
STUB_MODE=fnn STUB_NODE_ID="$2" exec python3 "${STUB_SERVER_PY}"
STUB

  # CKB stand-in: `ckb run` serves the CKB RPC on $CKB_PORT forever.
  cat >"$sandbox/bin/ckb" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "run" ]]; then
  : "${STUB_SERVER_PY:?}" "${CKB_PORT:?}"
  STUB_MODE=ckb STUB_PORT="$CKB_PORT" exec python3 "${STUB_SERVER_PY}"
fi
exit 0
STUB

  for stub in cargo ckb-cli npx; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$sandbox/bin/$stub"
  done
  printf '#!/usr/bin/env bash\nprintf %%s "20"\n' >"$sandbox/bin/node"

  chmod +x "$sandbox/bin/"* "$sandbox/target/debug/fnn" \
    "$sandbox/tests/deploy/init-dev-chain.sh"
}

run_supervisor() {
  local sandbox="$harness_dir/$1" ports="$2" no_ports="${3:-}"
  (
    cd "$sandbox"
    export PATH="$sandbox/bin:$PATH"
    export STUB_SERVER_PY="$harness_dir/stub-server.py"
    export STUB_LOG="$sandbox/stub-requests.log"
    export STUB_NODES_DIR="$sandbox/tests/nodes"
    export CKB_PORT="$CKB_PORT_TEST"
    export CKB_RPC_URL="http://127.0.0.1:$CKB_PORT_TEST"
    export FAKE_NODE_PORTS="$ports"
    if [[ -n "$no_ports" ]]; then
      export FAKE_NO_PORTS="$no_ports"
    fi
    export TEST_ENV=debug RPC_READY_TIMEOUT=5 STOP_GRACE_SECONDS=2
    : >"$STUB_LOG"
    bash tests/bruno/e2e/liquidity/restart-recovery/run-restart-test.sh
  )
}

assert_scenario() {
  local sandbox="$harness_dir/$1" name="$2" expected_n1_rpc="$3" expected_n2_rpc="$4" \
    expected_n1_p2p="$5" expected_n2_p2p="$6"
  local log
  log="$(cat "$sandbox/supervisor.out")"
  local expected_endpoints
  expected_endpoints="resolved node endpoints: node1 rpc=$expected_n1_rpc p2p=$expected_n1_p2p, node2 rpc=$expected_n2_rpc p2p=$expected_n2_p2p"
  [[ "$log" == *"$expected_endpoints"* ]] \
    || fail "$name: supervisor log lacks the resolved endpoint line '$expected_endpoints'"
  [[ "$log" == *"PASS: the restarted provider recovered the loop out swap to Success"* ]] \
    || fail "$name: supervisor did not reach PASS (see $(pwd)/$1/supervisor.out)"
  grep -q "^fnn ${expected_n1_rpc##*:} node_info$" "$sandbox/stub-requests.log" \
    || fail "$name: node1 RPC gate never hit port ${expected_n1_rpc##*:} (stub log: $sandbox/stub-requests.log)"
  grep -q "^fnn ${expected_n2_rpc##*:} node_info$" "$sandbox/stub-requests.log" \
    || fail "$name: node2 RPC gate never hit port ${expected_n2_rpc##*:} (stub log: $sandbox/stub-requests.log)"
}

# Scenario 1: randomized provisioning record - the supervisor must gate on
# the randomized RPC URLs.
build_sandbox randomized
set +e
run_supervisor randomized "26001 26002 26003 26004" >"$harness_dir/randomized/supervisor.out" 2>&1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
  tail -n 30 "$harness_dir/randomized/supervisor.out" >&2 || true
  fail "randomized scenario: supervisor exited with status $status"
fi
assert_scenario randomized randomized \
  "http://127.0.0.1:26002" "http://127.0.0.1:26004" 26001 26003

# Scenario 2: default provisioning record - the resolved endpoints must stay
# the conventional local defaults.
build_sandbox defaults
set +e
run_supervisor defaults "8344 21714 8345 21715" >"$harness_dir/defaults/supervisor.out" 2>&1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
  tail -n 30 "$harness_dir/defaults/supervisor.out" >&2 || true
  fail "defaults scenario: supervisor exited with status $status"
fi
assert_scenario defaults defaults \
  "http://127.0.0.1:21714" "http://127.0.0.1:21715" 8344 8345

# Scenario 3: no provisioning record at all - the configured defaults must
# be kept untouched.
build_sandbox noports
set +e
run_supervisor noports "8344 21714 8345 21715" y >"$harness_dir/noports/supervisor.out" 2>&1
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
  tail -n 30 "$harness_dir/noports/supervisor.out" >&2 || true
  fail "no-port-file scenario: supervisor exited with status $status"
fi
assert_scenario noports no-port-file \
  "http://127.0.0.1:21714" "http://127.0.0.1:21715" 8344 8345

printf 'restart supervisor port resolution checks passed\n'
