#!/usr/bin/env bash
set -euo pipefail
umask 077

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
artifacts_dir="$repo_root/tests/artifacts"
liquidity_dir="$artifacts_dir/liquidity"
output_dir="$liquidity_dir/$suite"
destination="$output_dir/$name"

ensure_real_directory() {
  local path="$1"
  local label="$2"
  if [[ -L "$path" ]]; then
    printf 'refusing symlink %s directory: %s\n' "$label" "$path" >&2
    exit 1
  fi
  if [[ -e "$path" ]]; then
    if [[ ! -d "$path" ]]; then
      printf '%s path is not a directory: %s\n' "$label" "$path" >&2
      exit 1
    fi
  else
    mkdir "$path"
  fi
  if [[ -L "$path" || ! -d "$path" ]]; then
    printf 'unsafe %s directory: %s\n' "$label" "$path" >&2
    exit 1
  fi
}

ensure_real_directory "$artifacts_dir" artifacts
ensure_real_directory "$liquidity_dir" liquidity
ensure_real_directory "$output_dir" suite

if [[ -L "$destination" ]]; then
  printf 'refusing symlink diagnostics destination: %s\n' "$destination" >&2
  exit 1
fi
if [[ -e "$destination" && ! -f "$destination" ]]; then
  printf 'diagnostics destination is not a regular file: %s\n' "$destination" >&2
  exit 1
fi

temporary_file=""
cleanup() {
  if [[ -n "$temporary_file" ]]; then
    rm -f "$temporary_file"
  fi
}
trap cleanup EXIT
trap 'cleanup; exit 1' HUP INT TERM
temporary_file="$(mktemp "$output_dir/.$name.tmp.XXXXXX")"
jq --sort-keys '
  def sensitive_key:
    ascii_downcase as $key |
    ($key | gsub("[-_]"; "")) as $compact |
    ($key | contains("private")) or
    ($key | contains("privkey")) or
    ($key | contains("preimage")) or
    ($key | contains("password")) or
    ($key | contains("passphrase")) or
    ($key | contains("token")) or
    ($key | contains("bearer")) or
    ($key | contains("authorization")) or
    ($compact | startswith("auth")) or
    ($compact == "apikey") or
    ($compact == "xapikey") or
    (($compact | startswith("api")) and ($compact | test("key|token|auth|secret|bearer"))) or
    ($key | contains("seed")) or
    ($key | contains("mnemonic")) or
    ($key == "settlement_key") or
    ($key == "local_key") or
    ($key == "client_invoice") or
    ($key == "invoice") or
    ($key == "invoice_address") or
    ($key == "secret") or
    ($key | endswith("_secret"));
  def redact_url:
    gsub(
      "(?<prefix>Authorization[[:space:]]*:[[:space:]]*)(?:[a-z][a-z0-9_-]*[[:space:]]+)?[^[:space:],;\"'\''}]+";
      "\(.prefix)[REDACTED]";
      "i"
    ) |
    gsub(
      "(?<prefix>\\b(?:Bearer|Basic|Token|(?:X-)?Api[-_]?Key)(?:[[:space:]]+|[[:space:]]*:[[:space:]]*))[^[:space:],;\"'\''}]+";
      "\(.prefix)[REDACTED]";
      "i"
    ) |
    gsub("://[^@/[:space:]]+@"; "://[REDACTED]@") |
    gsub(
      "(?<prefix>[?&](?:[a-z0-9_-]*(?:private|privkey|preimage)[a-z0-9_-]*|[a-z0-9_-]*token[a-z0-9_-]*|[a-z0-9_-]*bearer[a-z0-9_-]*|[a-z0-9_-]*api[a-z0-9_-]*(?:key|token|auth|secret|bearer)[a-z0-9_-]*|key|secret|password|passphrase|authorization|auth[a-z0-9_-]*|seed|mnemonic|settlement_key|local_key|client_invoice|invoice|invoice_address|[a-z0-9_-]+_secret)=)[^&#[:space:]\"'\''},\\]]*";
      "\(.prefix)[REDACTED]";
      "i"
    );
  def redact:
    if type == "object" then
      with_entries(
        if (.key | sensitive_key) then
          .value = "[REDACTED]"
        else
          .value |= redact
        end
      )
    elif type == "array" then map(redact)
    elif type == "string" then redact_url
    else .
    end;
  redact
' >"$temporary_file"
if [[ -L "$destination" ]]; then
  printf 'refusing symlink diagnostics destination: %s\n' "$destination" >&2
  exit 1
fi
if [[ -e "$destination" && ! -f "$destination" ]]; then
  printf 'diagnostics destination is not a regular file: %s\n' "$destination" >&2
  exit 1
fi
mv "$temporary_file" "$destination"
temporary_file=""
trap - EXIT HUP INT TERM
printf '%s\n' "$destination"
