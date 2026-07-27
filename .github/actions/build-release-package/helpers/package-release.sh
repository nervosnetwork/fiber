#!/usr/bin/env bash
set -euo pipefail

pkgname=${1:?package name is required}
bin_suffix=${2:-}

cp "target/release/fnn${bin_suffix}" "fnn${bin_suffix}"
cp "target/release/fnn-cli${bin_suffix}" "fnn-cli${bin_suffix}"
tar czvf "${pkgname}" "fnn${bin_suffix}" "fnn-cli${bin_suffix}" config

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "package_name=${pkgname}" >> "${GITHUB_OUTPUT}"
fi
