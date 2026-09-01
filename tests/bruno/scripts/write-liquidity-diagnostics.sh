#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  printf 'usage: %s <suite> <name.json>\n' "$0" >&2
  exit 2
fi

suite="$1"
name="$2"
if [[ ! "$suite" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]]; then
  printf 'invalid suite name: %s\n' "$suite" >&2
  exit 2
fi
if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*\.json$ ]]; then
  printf 'invalid diagnostics filename: %s\n' "$name" >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  printf 'missing required command: jq\n' >&2
  exit 1
fi

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
repo_root="$(cd -- "$script_dir/../../.." &>/dev/null && pwd)"
output_dir="$repo_root/tests/artifacts/liquidity/$suite"
mkdir -p "$output_dir"

temporary_file="$output_dir/.$name.tmp.$$"
trap 'rm -f "$temporary_file"' EXIT
jq --sort-keys '
  def redact:
    if type == "object" then
      with_entries(
        if (.key | test("private.?key|privkey|preimage|password"; "i")) then
          .value = "[REDACTED]"
        else
          .value |= redact
        end
      )
    elif type == "array" then map(redact)
    else .
    end;
  redact
' >"$temporary_file"
mv "$temporary_file" "$output_dir/$name"
trap - EXIT
printf '%s\n' "$output_dir/$name"
