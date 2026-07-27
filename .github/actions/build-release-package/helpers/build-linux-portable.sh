#!/usr/bin/env bash
set -euo pipefail

openssl_configure=${1:?openssl configure arguments are required}
read -r -a configure_args <<< "${openssl_configure}"

pwd_dir=$(pwd)
curl -LO https://www.openssl.org/source/openssl-1.1.1s.tar.gz
tar -xzf openssl-1.1.1s.tar.gz
pushd openssl-1.1.1s
./Configure "${configure_args[@]}"
make
popd

export OPENSSL_LIB_DIR="${pwd_dir}/openssl-1.1.1s"
export OPENSSL_INCLUDE_DIR="${pwd_dir}/openssl-1.1.1s/include"
export OPENSSL_STATIC=1
cargo build --release --locked --features portable
