#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
source_writer="$script_dir/../write-liquidity-diagnostics.sh"
fixture="$script_dir/fixtures/sensitive-diagnostics.json"
sandbox="$(mktemp -d "${TMPDIR:-/tmp}/liquidity-diagnostics-test.XXXXXX")"
external="$(mktemp -d "${TMPDIR:-/tmp}/liquidity-diagnostics-external.XXXXXX")"

cleanup() {
  rm -rf "$sandbox" "$external"
}
trap cleanup EXIT

mkdir -p "$sandbox/tests/bruno/scripts"
cp "$source_writer" "$sandbox/tests/bruno/scripts/write-liquidity-diagnostics.sh"
writer="$sandbox/tests/bruno/scripts/write-liquidity-diagnostics.sh"
artifacts="$sandbox/tests/artifacts"
printf 'outside-unchanged' >"$external/outside.json"

assert_external_unchanged() {
  if [[ "$(<"$external/outside.json")" != "outside-unchanged" ]]; then
    printf 'external file was modified through symlink\n' >&2
    exit 1
  fi
}

assert_rejected() {
  if printf '{}' | bash "$writer" evil-suite outside.json >/dev/null 2>&1; then
    printf 'symlink attack was accepted\n' >&2
    exit 1
  fi
  assert_external_unchanged
}

ln -s "$external" "$artifacts"
assert_rejected
rm "$artifacts"

mkdir "$artifacts"
ln -s "$external" "$artifacts/liquidity"
assert_rejected
rm "$artifacts/liquidity"

mkdir "$artifacts/liquidity"
ln -s "$external" "$artifacts/liquidity/evil-suite"
assert_rejected
rm "$artifacts/liquidity/evil-suite"

mkdir "$artifacts/liquidity/evil-suite"
ln -s "$external/outside.json" "$artifacts/liquidity/evil-suite/outside.json"
assert_rejected
rm -rf "$artifacts"

output="$(bash "$writer" safe-suite diagnostics.json <"$fixture")"
if mode="$(stat -f '%Lp' "$output" 2>/dev/null)"; then
  :
else
  mode="$(stat -c '%a' "$output")"
fi
if [[ "$mode" != "600" ]]; then
  printf 'expected artifact mode 600, got %s\n' "$mode" >&2
  exit 1
fi

serialized="$(jq -c . "$output")"
secrets=(
  encoded%40user p%40ss query-token-value query-api-key-value query-key-value
  query-secret-value query-password-value query-payment-secret query-preimage query-invoice
  invoice-address-secret invoice-object-secret payment-secret-value payment-preimage-secret
  private-key-value passphrase-value authorization-value auth-header-value api-key-value
  seed-value mnemonic-value suffix-secret-value
)
for secret in "${secrets[@]}"; do
  if [[ "$serialized" == *"$secret"* ]]; then
    printf 'leaked secret: %s\n' "$secret" >&2
    exit 1
  fi
done
if [[ "$serialized" != *"safe=visible"* ]]; then
  printf 'safe URL query diagnostic was removed\n' >&2
  exit 1
fi

printf 'write-liquidity-diagnostics checks passed\n'
