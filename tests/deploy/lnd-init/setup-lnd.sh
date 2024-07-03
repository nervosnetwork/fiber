#!/usr/bin/env bash

set -e

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
bob_port=11009
ingrid_port=10009

echo "=> bootstrap"
echo "script_dir=$script_dir"

kill-via-pid-file () {
  local pid="$(cat "$1" 2>/dev/null || true)"
  if [ -n "$pid" ]; then
    kill "$pid" || true
    sleep 3
    if kill -0 "$pid" 2>/dev/null; then
      echo "Failed to kill $pid, force killing it."
      kill -9 "$pid"
    fi
    rm -f "$1"
  fi
}

cleanup() {
  echo "=> cleanup"
  kill-via-pid-file "$script_dir/bitcoind/bitcoind.pid"
  kill-via-pid-file "$script_dir/lnd-bob/lnd.pid"
  kill-via-pid-file "$script_dir/lnd-ingrid/lnd.pid"
  rm -rf "$script_dir/bitcoind/regtest"

  local lnd_dir

  for lnd_dir in lnd-bob lnd-ingrid; do
    rm -rf "$script_dir/$lnd_dir/data"
    rm -rf "$script_dir/$lnd_dir/letsencrypt"
    rm -rf "$script_dir/$lnd_dir/logs"
  done
}

setup-bitcoind() {
  echo "=> setting up bitcoind"
  local bitcoind_dir="$script_dir/bitcoind"
  local bitcoind_conf="$bitcoind_dir/bitcoin.conf"
  local bitcoind_pid="$bitcoind_dir/bitcoind.pid"

  bitcoind -conf="$bitcoind_conf" -datadir="$bitcoind_dir" -daemonwait -pid="$bitcoind_pid"
  bitcoin-cli -conf="$bitcoind_conf" -datadir="$bitcoind_dir" -rpcwait createwallet dev >/dev/null
  echo "bitcoind wallet created"
  bitcoin-cli -conf="$bitcoind_conf" -generate 101 >/dev/null
  echo "bitcoind blocks generated"
}

setup-lnd() {
  local lnd_name="$1"
  local lnd_port="$2"
  local lnd_dir="$script_dir/$lnd_name"
  echo "=> setting up lnd $lnd_name"
  nohup lnd --lnddir="$lnd_dir" &>/dev/null &
  echo "$!" > "$lnd_dir/lnd.pid"
  local retries=30
  echo "waiting for ready"
  while [[ $retries -gt 0 ]] && ! lncli -n regtest --lnddir="$lnd_dir" --no-macaroons --rpcserver "localhost:$lnd_port" getinfo &>/dev/null; do
    sleep 1
    retries=$((retries - 1))
  done
  echo "remaining retries=$retries"
}

setup-channels() {
  echo "=> open channel from ingrid to bob"
  local bob_dir="$script_dir/lnd-bob"
  local ingrid_dir="$script_dir/lnd-ingrid"
  local ingrid_p2tr_address="$(lncli -n regtest --lnddir="$ingrid_dir" --no-macaroons --rpcserver "localhost:$ingrid_port" newaddress p2tr | jq -r .address)"
  local bob_node_key="$(lncli -n regtest --lnddir="$bob_dir" --no-macaroons --rpcserver "localhost:$bob_port" getinfo | jq -r .identity_pubkey)"
  echo "ingrid_p2tr_address=$ingrid_p2tr_address"
  echo "bob_node_key=$bob_node_key"

  echo "deposit btc"
  local bitcoind_dir="$script_dir/bitcoind"
  local bitcoind_conf="$bitcoind_dir/bitcoin.conf"
  bitcoin-cli -conf="$bitcoind_conf" -rpcwait -named sendtoaddress address="$ingrid_p2tr_address" amount=5 fee_rate=25
  bitcoin-cli -conf="$bitcoind_conf" -generate 1 >/dev/null

  echo "openchannel"
  local retries=5
  while [[ $retries -gt 0 ]] && ! lncli -n regtest --lnddir="$ingrid_dir" --no-macaroons --rpcserver "localhost:$ingrid_port" \
      openchannel \
      --node_key "$bob_node_key" \
      --connect localhost:9835 \
      --local_amt 1000000 \
      --sat_per_vbyte 1 \
      --min_confs 0; do
    sleep 3
    retries=$((retries - 1))
  done

  echo "generate blocks"
  bitcoin-cli -conf="$bitcoind_conf" -generate 3 >/dev/null
}

cleanup
setup-bitcoind
setup-lnd lnd-bob $bob_port
setup-lnd lnd-ingrid $ingrid_port
setup-channels
