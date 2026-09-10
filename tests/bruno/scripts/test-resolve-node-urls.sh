#!/usr/bin/env bash
set -euo pipefail

# Regression harness for tests/bruno/scripts/resolve-node-urls.sh and its
# callers (the refund and udt-loop-in-rejection suite runners and the e2e.yml
# diagnostics steps).
#
# Part 1 sources the helper inside a sandbox repo layout with different
# tests/nodes/.ports fixtures and asserts the exported endpoint variables:
#   - a valid record (one port per line: node1 P2P, node1 RPC, node2 P2P,
#     node2 RPC, node3 P2P, node3 RPC) yields the node RPC URLs derived from
#     lines 2 and 4 plus the P2P/RPC ports from lines 1-4, and wins over
#     pre-set environment values because the record is regenerated together
#     with the node configs it describes;
#   - a missing or malformed record (too few lines, non-numeric or
#     out-of-range ports) leaves every variable untouched, so callers keep
#     their environment overrides or their defaults.
#
# Part 2 runs the unchanged refund and udt-loop-in-rejection run.sh scripts
# inside a sandbox with stubbed mutator/npx/collector binaries and proves via
# the stub collector's log that the collector receives the resolved node RPC
# URLs (randomized record -> randomized URLs; missing record -> the caller
# environment or, when nothing is set, the collector's own defaults). It also
# pins the mutator sidecar argv: the refund runner (whose mutator mode never
# signs and needs no wallet key) must not pass --privkey-path, while the
# rejection runner still must.
#
# The stub CKB/mutator servers bind loopback ports 28014/28117; run this
# harness when those ports are free.

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd -- "$script_dir/../../.." &>/dev/null && pwd)"
helper="$script_dir/resolve-node-urls.sh"
CKB_PORT_TEST=28014
MUTATOR_PORT_TEST=28117

fail() { printf 'test-resolve-node-urls: %s\n' "$1" >&2; exit 1; }

VARS_DUMP_CMD='source tests/bruno/scripts/resolve-node-urls.sh; printf "%s|%s|%s|%s|%s|%s\n" "${NODE1_RPC_URL-}" "${NODE2_RPC_URL-}" "${NODE1_P2P_PORT-}" "${NODE1_RPC_PORT-}" "${NODE2_P2P_PORT-}" "${NODE2_RPC_PORT-}"'

# ---------------------------------------------------------------------------
# Part 1: resolve-node-urls.sh unit tests.
# ---------------------------------------------------------------------------

unit_sandbox="$(mktemp -d "${TMPDIR:-/tmp}/resolve-node-urls-unit.XXXXXX")"

dump_resolved() {
  local sandbox="$1"; shift
  (
    cd "$sandbox"
    if (( "$#" > 0 )); then
      env "$@" bash -c "$VARS_DUMP_CMD"
    else
      bash -c "$VARS_DUMP_CMD"
    fi
  )
}

write_ports_file() {
  local sandbox="$1" port
  : >"$sandbox/tests/nodes/.ports"
  for port in "${@:2}"; do
    printf '%s\n' "$port" >>"$sandbox/tests/nodes/.ports"
  done
}

if [[ ! -f "$helper" ]]; then
  fail "missing helper script: $helper"
fi
mkdir -p "$unit_sandbox/tests/nodes" "$unit_sandbox/tests/bruno/scripts"
cp "$helper" "$unit_sandbox/tests/bruno/scripts/resolve-node-urls.sh"

# A valid six-line record (the real generate_nodes_config layout) resolves
# every endpoint variable from its first four lines.
write_ports_file "$unit_sandbox" 26001 26002 26003 26004 8346 21716
actual="$(dump_resolved "$unit_sandbox")"
expected="http://127.0.0.1:26002|http://127.0.0.1:26004|26001|26002|26003|26004"
[[ "$actual" == "$expected" ]] || fail "six-line record: expected '$expected', got '$actual'"

# A four-line record (nodes 1 and 2 only) resolves just as well.
write_ports_file "$unit_sandbox" 26001 26002 26003 26004
actual="$(dump_resolved "$unit_sandbox")"
[[ "$actual" == "$expected" ]] || fail "four-line record: expected '$expected', got '$actual'"

# Without the record nothing is set: callers keep their defaults.
rm -f "$unit_sandbox/tests/nodes/.ports"
actual="$(dump_resolved "$unit_sandbox")"
expected="|||||"
[[ "$actual" == "$expected" ]] || fail "missing record: expected '$expected', got '$actual'"

# Without the record pre-set environment values survive untouched.
actual="$(dump_resolved "$unit_sandbox" \
  "NODE1_RPC_URL=http://127.0.0.1:29999" "NODE2_RPC_URL=http://127.0.0.1:29998")"
expected="http://127.0.0.1:29999|http://127.0.0.1:29998||||"
[[ "$actual" == "$expected" ]] || fail "missing record with env overrides: expected '$expected', got '$actual'"

# Malformed records are ignored in favor of the environment or defaults.
write_ports_file "$unit_sandbox" 26001 not-a-port 26003 26004
actual="$(dump_resolved "$unit_sandbox")"
expected="|||||"
[[ "$actual" == "$expected" ]] || fail "non-numeric record: expected '$expected', got '$actual'"

write_ports_file "$unit_sandbox" 26001 26002
actual="$(dump_resolved "$unit_sandbox")"
[[ "$actual" == "|||||" ]] || fail "short record: expected untouched variables, got '$actual'"

write_ports_file "$unit_sandbox" 26001 70000 26003 26004
actual="$(dump_resolved "$unit_sandbox")"
[[ "$actual" == "|||||" ]] || fail "out-of-range record: expected untouched variables, got '$actual'"

# A valid record wins over pre-set environment values.
write_ports_file "$unit_sandbox" 26001 26002 26003 26004
actual="$(dump_resolved "$unit_sandbox" \
  "NODE1_RPC_URL=http://127.0.0.1:29999" "NODE2_RPC_URL=http://127.0.0.1:29998")"
expected="http://127.0.0.1:26002|http://127.0.0.1:26004|26001|26002|26003|26004"
[[ "$actual" == "$expected" ]] || fail "record must win over env overrides: expected '$expected', got '$actual'"

rm -rf "$unit_sandbox"

# ---------------------------------------------------------------------------
# Part 2: the suite runners hand the resolved URLs to the collector.
# ---------------------------------------------------------------------------

caller_sandbox="$(mktemp -d "${TMPDIR:-/tmp}/resolve-node-urls-callers.XXXXXX")"
stub_server_py="$caller_sandbox/stub-server.py"
ckb_server_pid=""

cleanup() {
  if [[ -n "$ckb_server_pid" ]]; then
    kill "$ckb_server_pid" 2>/dev/null || true
    wait "$ckb_server_pid" 2>/dev/null || true
  fi
  rm -rf "$caller_sandbox"
}
trap cleanup EXIT

cat >"$stub_server_py" <<'PY'
import json, os
from http.server import BaseHTTPRequestHandler, HTTPServer

mode = os.environ.get("STUB_MODE", "ckb")
port = int(os.environ.get("STUB_PORT", "0"))

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
        payload = json.dumps({"id": 1, "jsonrpc": "2.0", "result": {"stub": mode}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY

build_caller_sandbox() {
  local sandbox="$1"
  mkdir -p "$sandbox/bin" \
    "$sandbox/tests/bruno/scripts" \
    "$sandbox/tests/bruno/e2e/liquidity/refund" \
    "$sandbox/tests/bruno/e2e/liquidity/udt-loop-in-rejection" \
    "$sandbox/tests/nodes/1/ckb" \
    "$sandbox/tests/liquidity-lock-mutator/target/debug"
  cp "$script_dir/resolve-node-urls.sh" "$sandbox/tests/bruno/scripts/"
  cp "$repo_root/tests/bruno/e2e/liquidity/refund/run.sh" \
    "$sandbox/tests/bruno/e2e/liquidity/refund/run.sh"
  cp "$repo_root/tests/bruno/e2e/liquidity/udt-loop-in-rejection/run.sh" \
    "$sandbox/tests/bruno/e2e/liquidity/udt-loop-in-rejection/run.sh"
  : >"$sandbox/tests/nodes/1/ckb/plain_key"

  cat >"$sandbox/bin/npx" <<'STUB'
#!/usr/bin/env bash
# The Bruno CLI stub always fails so the runners take their collector path.
exit 1
STUB

  cat >"$sandbox/tests/bruno/scripts/collect-liquidity-failure-diagnostics.sh" <<'STUB'
#!/usr/bin/env bash
# Collector stand-in: records the environment the caller exported.
set -euo pipefail
[[ -n "${COLLECTOR_ENV_LOG:-}" ]] || { echo "COLLECTOR_ENV_LOG missing" >&2; exit 1; }
printf 'suite=%s NODE1_RPC_URL=%s NODE2_RPC_URL=%s CKB_RPC_URL=%s\n' \
  "$1" "${NODE1_RPC_URL-}" "${NODE2_RPC_URL-}" "${CKB_RPC_URL-}" >>"$COLLECTOR_ENV_LOG"
STUB

  cat >"$sandbox/tests/liquidity-lock-mutator/target/debug/liquidity-lock-mutator" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"${MUTATOR_ARGV_LOG:?}"
: "${STUB_SERVER_PY:?}"
STUB_MODE=mutator STUB_PORT="${MUTATOR_PORT:?}" exec python3 "${STUB_SERVER_PY}"
STUB

  chmod +x "$sandbox/bin/npx" \
    "$sandbox/tests/bruno/scripts/collect-liquidity-failure-diagnostics.sh" \
    "$sandbox/tests/liquidity-lock-mutator/target/debug/liquidity-lock-mutator"
}

run_suite_runner() {
  local sandbox="$1" suite="$2" extra_env="${3:-}"
  (
    cd "$sandbox"
    export PATH="$sandbox/bin:$PATH"
    export STUB_SERVER_PY="$stub_server_py"
    export STUB_LOG="$sandbox/stub-requests.log"
    export CKB_RPC_URL="http://127.0.0.1:$CKB_PORT_TEST"
    export MUTATOR_PORT="$MUTATOR_PORT_TEST"
    export MUTATOR_ARGV_LOG="$sandbox/mutator-argv.log"
    export COLLECTOR_ENV_LOG="$sandbox/collector-env.log"
    : >"$MUTATOR_ARGV_LOG"
    : >"$COLLECTOR_ENV_LOG"
    if [[ -n "$extra_env" ]]; then
      # shellcheck disable=SC2086
      env $extra_env bash "tests/bruno/e2e/liquidity/$suite/run.sh" >/dev/null 2>&1
    else
      bash "tests/bruno/e2e/liquidity/$suite/run.sh" >/dev/null 2>&1
    fi
    echo $?
  )
}

STUB_MODE=ckb STUB_PORT="$CKB_PORT_TEST" STUB_LOG="$caller_sandbox/stub-requests.log" \
  STUB_SERVER_PY="$stub_server_py" python3 "$stub_server_py" &
ckb_server_pid=$!

build_caller_sandbox "$caller_sandbox"

check_collector_env() {
  local suite="$1" expected_n1="$2" expected_n2="$3"
  local line
  line="$(grep "^suite=$suite " "$caller_sandbox/collector-env.log" | tail -n 1)" \
    || fail "$suite: the collector was never invoked"
  [[ "$line" == *"NODE1_RPC_URL=$expected_n1"* ]] \
    || fail "$suite: collector saw '$line', expected NODE1_RPC_URL=$expected_n1"
  [[ "$line" == *"NODE2_RPC_URL=$expected_n2"* ]] \
    || fail "$suite: collector saw '$line', expected NODE2_RPC_URL=$expected_n2"
}

# Scenario 1: a randomized ports record must reach the collector through the
# runner's exported environment.
printf '26001\n26002\n26003\n26004\n8346\n21716\n' >"$caller_sandbox/tests/nodes/.ports"
status="$(run_suite_runner "$caller_sandbox" refund)"
[[ "$status" == "1" ]] || fail "refund runner should propagate the Bruno failure (exit 1), got $status"
check_collector_env refund "http://127.0.0.1:26002" "http://127.0.0.1:26004"
if grep -q -- "--privkey-path" "$caller_sandbox/mutator-argv.log"; then
  fail "refund runner must not pass --privkey-path to the mutator sidecar"
fi
status="$(run_suite_runner "$caller_sandbox" udt-loop-in-rejection)"
[[ "$status" == "1" ]] || fail "rejection runner should propagate the Bruno failure (exit 1), got $status"
check_collector_env udt-loop-in-rejection "http://127.0.0.1:26002" "http://127.0.0.1:26004"
if ! grep -Eq -- "--privkey-path .*/plain_key" "$caller_sandbox/mutator-argv.log"; then
  fail "rejection runner must still pass --privkey-path to the mutator sidecar"
fi

# Scenario 2: without a ports record the runner environment decides - and
# when nothing is set the collector keeps its own local defaults.
rm -f "$caller_sandbox/tests/nodes/.ports"
status="$(run_suite_runner "$caller_sandbox" refund)"
[[ "$status" == "1" ]] || fail "refund runner should propagate the Bruno failure (exit 1), got $status"
check_collector_env refund "" ""
status="$(run_suite_runner "$caller_sandbox" udt-loop-in-rejection)"
[[ "$status" == "1" ]] || fail "rejection runner should propagate the Bruno failure (exit 1), got $status"
check_collector_env udt-loop-in-rejection "" ""

# Scenario 3: without a record, explicit caller environment survives.
status="$(run_suite_runner "$caller_sandbox" refund \
  "NODE1_RPC_URL=http://127.0.0.1:29999 NODE2_RPC_URL=http://127.0.0.1:29998")"
[[ "$status" == "1" ]] || fail "refund runner should propagate the Bruno failure (exit 1), got $status"
check_collector_env refund "http://127.0.0.1:29999" "http://127.0.0.1:29998"

printf 'resolve-node-urls checks passed\n'
