#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

runs=${DST_RUNS:-0}
if [[ ! $runs =~ ^(0|[1-9][0-9]*)$ ]]; then
    echo "DST_RUNS must be a non-negative integer (0 runs indefinitely)" >&2
    exit 2
fi

target_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked --target-dir "$target_dir" -p fiber-dst --bin fiber-dst

target_dir=$(cd "$target_dir" && pwd)
binary="$target_dir/debug/fiber-dst"
log_root="$target_dir/dst"
mkdir -p "$log_root"
log_dir=$(mktemp -d "$log_root/run-XXXXXXXX")
printf 'DST logs: %s\n' "$log_dir"

child_pid=
stop() {
    if [[ -n $child_pid ]]; then
        kill "$child_pid" 2>/dev/null || true
        wait "$child_pid" 2>/dev/null || true
    fi
    printf 'DST stopped. Logs: %s\n' "$log_dir" >&2
    exit "$1"
}
trap 'stop 130' INT
trap 'stop 143' TERM

run=0
failures=0
while [[ $runs == 0 || $run != "$runs" ]]; do
    ((run += 1))
    log="$log_dir/$run.log"
    printf 'Run %d started (%s)\n' "$run" "$log"
    "$binary" random --payments "${DST_PAYMENTS:-128}" --phases "${DST_PHASES:-8}" --chaos >"$log" 2>&1 &
    child_pid=$!
    status=0
    wait "$child_pid" || status=$?
    child_pid=

    header=
    IFS= read -r header <"$log" || true
    if [[ $header != 'fiber-dst random '* ]]; then
        printf 'Run %d stopped before selecting a seed. See %s\n' "$run" "$log" >&2
        exit 1
    fi
    if [[ $status == 0 ]]; then
        printf 'Run %d passed (%s)\n' "$run" "$header"
    else
        ((failures += 1))
        printf 'Run %d failed (exit %d). Log: %s\n' "$run" "$status" "$log" >&2
        printf 'Replay: %q %s\n' "$binary" "${header#fiber-dst }" >&2
    fi
done

printf 'DST finished. %d passed, %d failed. Logs: %s\n' "$((run - failures))" "$failures" "$log_dir"
[[ $failures == 0 ]]
